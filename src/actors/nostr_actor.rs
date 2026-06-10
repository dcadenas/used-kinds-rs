use crate::{
    actors::json_actor::{JsonActor, JsonActorMessage},
    utils::{is_kind_free, should_log},
};
use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use nostr_sdk::prelude::*;
use ractor::{cast, concurrency::Duration, Actor, ActorProcessingErr, ActorRef, SupervisionEvent};
use serde_json;
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

// Always-on popular relays (never rotated)
const ALWAYS_ON_RELAYS: [&str; 3] = [
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://nos.lol",
];

const POPULAR_RELAYS_FALLBACK: [&str; 4] = [
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://nos.lol",
    "wss://relay.snort.social",
];

pub struct NostrActor;

#[derive(Clone)]
pub struct State {
    cancellation_token: Option<CancellationToken>,
    json_actor: ActorRef<JsonActorMessage>,
    client: Client,
    subscription_ids: HashMap<SubscriptionId, ()>,
    latest_event_received_at: DateTime<Utc>,
    current_rotating_relays: Vec<String>, // Track currently connected rotating relays
}

impl State {
    async fn get_events(
        &self,
        nostr_actor: &ActorRef<NostrActorMessage>,
        cancellation_token: CancellationToken,
    ) -> Result<()> {
        let filter = Filter::new().limit(1);
        info!("Filter is {:?}", filter);

        let Output {
            val: main_sub_id, ..
        } = self.client.subscribe(filter.clone(), None).await?;
        info!("Main sub id is {}", main_sub_id);
        self.client
            .handle_notifications(|notification| async {
                if cancellation_token.is_cancelled() {
                    // true breaks the loop
                    return Ok(true);
                }

                match notification {
                    RelayPoolNotification::Event {
                        event,
                        subscription_id,
                        relay_url,
                    } => {
                        cast!(
                            nostr_actor,
                            NostrActorMessage::NewEvent(event, subscription_id, relay_url.into())
                        )?;
                        Ok(false)
                    }
                    RelayPoolNotification::Message { message, .. } => {
                        match message {
                            RelayMessage::Notice(message) => {
                                info!("Received notice: {:?}", message);
                            }
                            RelayMessage::Closed {
                                subscription_id,
                                message,
                            } => {
                                info!("Subscription {} closed: {}", subscription_id, message);
                                if subscription_id.as_ref() == &main_sub_id {
                                    return Ok(true);
                                }
                            }
                            RelayMessage::Event {
                                subscription_id, ..
                            } => {
                                debug!("Received message for sub: {}", subscription_id);
                            }
                            _ => {
                                debug!("Received message: {:?}", message);
                            }
                        }
                        Ok(false)
                    }
                    RelayPoolNotification::Shutdown => {
                        info!("Shutdown");
                        Ok(true)
                    }
                }
            })
            .await?;

        Ok(())
    }

    async fn subscribe_recommended_app_query(&mut self, kind: Kind) {
        let filter = Filter::new()
            .limit(1)
            .kind(Kind::from_u16(31990))
            .custom_tag(SingleLetterTag::lowercase(Alphabet::K), kind.to_string());

        let opts = SubscribeAutoCloseOptions::default().timeout(Some(Duration::from_secs(5)));

        let id = SubscriptionId::generate();
        self.subscription_ids.insert(id.clone(), ());
        let _ = self
            .client
            .subscribe_with_id(id.clone(), filter, Some(opts))
            .await;

        if should_log() {
            info!(
                "Subscribed to recommended app event for kind {:?} with subscription id {}. Current subscriptions: {}",
                kind, id, self.subscription_ids.len()
            );
        }
    }

    fn reset_notification_handler(&mut self, myself: ActorRef<NostrActorMessage>) {
        info!("Resetting subscription_ids");
        self.subscription_ids.clear();
        self.latest_event_received_at = Utc::now();
        if let Some(c) = self.cancellation_token.as_ref() {
            c.cancel()
        }
        let cancellation_token = CancellationToken::new();
        self.cancellation_token = Some(cancellation_token.clone());

        let state_clone = self.clone();
        let token_clone = cancellation_token.clone();
        let myself_clone = myself.clone();
        let join_handler = tokio::spawn(async move {
            loop {
                let token_clone = token_clone.clone();
                if let Err(e) = state_clone
                    .get_events(&myself_clone, token_clone.clone())
                    .await
                {
                    error!("Failed to get events: {}", e);
                }

                if token_clone.is_cancelled() {
                    break;
                }

                // TODO: exponential backoff
                warn!("Subscription closed, reconnecting in 60 seconds");
                tokio::time::sleep(Duration::from_secs(60)).await;
            }

            info!("Notification handler stopped");
        });

        let token_clone = cancellation_token.clone();
        let myself_clone = myself.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60 * 5));
            while !token_clone.is_cancelled() {
                interval.tick().await;
                if let Err(e) = cast!(&myself_clone, NostrActorMessage::HealthCheck) {
                    error!("Failed to reset notification handler: {}", e);
                }
            }
            join_handler.abort();
        });
    }
}

#[derive(Debug, Clone)]
pub enum NostrActorMessage {
    GetRecommendedApp(Kind),
    NewEvent(Box<Event>, SubscriptionId, Url),
    HealthCheck,
    RotateRelays,
}

/// Parse the nostr.watch online-relays response.
///
/// Returns `None` for non-2xx statuses, unparseable bodies, or an empty
/// list so the caller falls back to known relays. nostr.watch outages
/// surface as HTTP 502 with an empty body, which reqwest reports as `Ok`.
fn parse_online_relays(status_code: u16, body: &str) -> Option<Vec<String>> {
    if !(200..300).contains(&status_code) {
        return None;
    }

    let relays: Vec<String> = serde_json::from_str(body).ok()?;

    if relays.is_empty() {
        return None;
    }

    Some(relays)
}

/// Get rotating relays, excluding always-on relays and previously used relays
async fn get_rotating_relays(exclude: &[String]) -> Vec<String> {
    let fetched = match reqwest::get("https://api.nostr.watch/v1/online").await {
        Ok(response) => {
            let status_code = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            parse_online_relays(status_code, &body)
        }
        Err(e) => {
            error!("Failed to fetch relays from nostr.watch: {}", e);
            None
        }
    };

    let always_on_set: Vec<String> = ALWAYS_ON_RELAYS.iter().map(|s| s.to_string()).collect();

    match fetched {
        Some(all_relays) => {
            let filtered: Vec<String> = all_relays
                .into_iter()
                .filter(|r| !always_on_set.contains(r) && !exclude.contains(r))
                .take(5)
                .collect();

            info!(
                "Fetched {} rotating relays from nostr.watch",
                filtered.len()
            );
            filtered
        }
        None => {
            warn!("No usable relay list from nostr.watch, using fallback relays");
            POPULAR_RELAYS_FALLBACK
                .iter()
                .filter(|r| !ALWAYS_ON_RELAYS.contains(r) && !exclude.contains(&r.to_string()))
                .map(|s| s.to_string())
                .take(5)
                .collect()
        }
    }
}

/// Swap the rotating relay set: drop old rotating relays and connect new ones.
///
/// Old relays are removed from the pool, not just disconnected: a pool-wide
/// `connect()` reconnects every relay in Terminated status, so disconnected
/// relays would be resurrected by the next health check and the pool would
/// grow without bound. No relay carries the GOSSIP flag, so removal is never
/// blocked. New relays are connected individually for the same reason.
async fn swap_rotating_relays(client: &Client, old: &[String], new: &[String]) {
    for relay_url in old {
        info!("Removing rotating relay from pool: {}", relay_url);
        if let Err(e) = client.remove_relay(relay_url).await {
            warn!("Failed to remove relay {}: {}", relay_url, e);
        }
    }

    for relay_url in new {
        info!("Connecting to new rotating relay: {}", relay_url);
        if let Err(e) = client.add_relay(relay_url).await {
            warn!("Failed to add relay {}: {}", relay_url, e);
            continue;
        }
        if let Err(e) = client.connect_relay(relay_url).await {
            warn!("Failed to connect to relay {}: {}", relay_url, e);
        }
    }
}

/// Get initial relay set (always-on + rotating)
async fn get_initial_relays() -> (Vec<String>, Vec<String>) {
    let always_on: Vec<String> = ALWAYS_ON_RELAYS.iter().map(|s| s.to_string()).collect();
    let rotating = get_rotating_relays(&[]).await;

    let all_relays: Vec<String> = always_on.iter().chain(rotating.iter()).cloned().collect();
    info!(
        "Initial relays: {} always-on + {} rotating = {}",
        always_on.len(),
        rotating.len(),
        all_relays.len()
    );
    info!("Relays: {:?}", all_relays);

    (all_relays, rotating)
}

#[ractor::async_trait]
impl Actor for NostrActor {
    type Msg = NostrActorMessage;
    type State = State;
    type Arguments = ();

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        _arguments: (),
    ) -> Result<Self::State, ActorProcessingErr> {
        let client = Client::default();

        let (all_relays, rotating_relays) = get_initial_relays().await;
        for relay in all_relays {
            client.add_relay(relay).await?;
        }

        client.connect().await;

        let (json_actor, _) = Actor::spawn_linked(
            Some("JsonActor".to_string()),
            JsonActor,
            myself.clone(),
            myself.clone().into(),
        )
        .await?;

        // Schedule relay rotation every 30 minutes
        myself.send_interval(Duration::from_secs(60 * 30), || {
            NostrActorMessage::RotateRelays
        });

        let mut state = State {
            json_actor,
            client,
            cancellation_token: None,
            subscription_ids: HashMap::new(),
            latest_event_received_at: Utc::now(),
            current_rotating_relays: rotating_relays,
        };

        state.reset_notification_handler(myself);

        Ok(state)
    }

    async fn post_stop(
        &self,
        _: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        info!("Nostr actor stopped");
        if let Some(c) = state.cancellation_token.as_ref() {
            c.cancel()
        }
        Ok(())
    }

    async fn handle_supervisor_evt(
        &self,
        myself: ActorRef<Self::Msg>,
        message: SupervisionEvent,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SupervisionEvent::ActorPanicked(dead_actor, panic_msg)
                if dead_actor.get_id() == state.json_actor.get_id() =>
            {
                info!("NostrActor: {dead_actor:?} panicked with '{panic_msg}'");

                info!("NostrActor: Terminating json actor");
                myself.stop(Some("NostrActor died".to_string()));
            }
            other => {
                info!("NostrActor: received supervisor event '{other}'");
            }
        }
        Ok(())
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            NostrActorMessage::GetRecommendedApp(kind) => {
                state.subscribe_recommended_app_query(kind).await;
            }
            NostrActorMessage::HealthCheck => {
                if state.latest_event_received_at + ChronoDuration::seconds(60 * 5) < Utc::now() {
                    warn!(
                        "No events received in the last 5 minutes, reconnecting always-on relays and resetting notification handler"
                    );

                    for relay in ALWAYS_ON_RELAYS {
                        if let Err(e) = state.client.add_relay(relay).await {
                            error!("Failed to add relay {}: {}", relay, e);
                        }
                    }
                    // Pool-wide connect is safe here only because rotation
                    // removes old relays from the pool (swap_rotating_relays):
                    // the pool holds always-on plus current rotating relays,
                    // exactly the set worth reconnecting during a stall.
                    state.client.connect().await;

                    state.reset_notification_handler(myself);
                }
            }
            NostrActorMessage::NewEvent(event, subscription_id, relay_url) => {
                state.latest_event_received_at = Utc::now();

                if state.subscription_ids.contains_key(&subscription_id) {
                    state.subscription_ids.remove(&subscription_id);

                    cast!(
                        state.json_actor,
                        JsonActorMessage::RecordRecommendedApp(event, relay_url)
                    )?;
                } else if is_kind_free(event.kind) {
                    cast!(
                        state.json_actor,
                        JsonActorMessage::RecordEvent(event, relay_url)
                    )?;
                }
            }
            NostrActorMessage::RotateRelays => {
                info!("Rotating relays - disconnecting from old rotating relays and connecting to new ones");

                // Get new rotating relays, excluding the current ones
                let new_rotating_relays = get_rotating_relays(&state.current_rotating_relays).await;

                if new_rotating_relays.is_empty() {
                    warn!("No new rotating relays available, keeping current ones");
                    return Ok(());
                }

                swap_rotating_relays(
                    &state.client,
                    &state.current_rotating_relays,
                    &new_rotating_relays,
                )
                .await;

                state.current_rotating_relays = new_rotating_relays;

                info!(
                    "Relay rotation complete. New rotating relays: {:?}",
                    state.current_rotating_relays
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_online_relays_accepts_success_with_relay_list() {
        let body = r#"["wss://relay.one.example","wss://relay.two.example"]"#;
        assert_eq!(
            parse_online_relays(200, body),
            Some(vec![
                "wss://relay.one.example".to_string(),
                "wss://relay.two.example".to_string()
            ])
        );
    }

    #[test]
    fn test_parse_online_relays_rejects_error_status_even_with_valid_body() {
        let body = r#"["wss://relay.one.example"]"#;
        assert_eq!(parse_online_relays(502, body), None);
        assert_eq!(parse_online_relays(502, ""), None);
    }

    #[test]
    fn test_parse_online_relays_rejects_unparseable_body() {
        assert_eq!(parse_online_relays(200, "<html>error</html>"), None);
        assert_eq!(parse_online_relays(200, ""), None);
    }

    #[test]
    fn test_parse_online_relays_rejects_empty_list() {
        assert_eq!(parse_online_relays(200, "[]"), None);
    }

    #[tokio::test]
    async fn test_swap_rotating_relays_removes_old_relays_from_pool() {
        let client = Client::default();
        client.add_relay("ws://127.0.0.1:9101").await.unwrap(); // stands in for an always-on relay
        client.add_relay("ws://127.0.0.1:9102").await.unwrap(); // old rotating relay

        let old = vec!["ws://127.0.0.1:9102".to_string()];
        let new = vec!["ws://127.0.0.1:9103".to_string()];
        swap_rotating_relays(&client, &old, &new).await;

        let pool: Vec<String> = client
            .relays()
            .await
            .keys()
            .map(|u| u.to_string())
            .collect();
        assert!(
            pool.iter().any(|u| u.starts_with("ws://127.0.0.1:9101")),
            "always-on relay must stay in the pool: {pool:?}"
        );
        assert!(
            !pool.iter().any(|u| u.starts_with("ws://127.0.0.1:9102")),
            "old rotating relay must leave the pool (disconnect alone lets pool-wide \
             connect() resurrect it): {pool:?}"
        );
        assert!(
            pool.iter().any(|u| u.starts_with("ws://127.0.0.1:9103")),
            "new rotating relay must join the pool: {pool:?}"
        );
    }
}
