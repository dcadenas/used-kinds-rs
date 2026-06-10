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

async fn get_relays() -> Vec<String> {
    let fetched = match reqwest::get("https://api.nostr.watch/v1/online").await {
        Ok(response) => {
            let status_code = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            parse_online_relays(status_code, &body)
        }
        Err(e) => {
            error!("Failed to fetch relays from Nostr API: {}", e);
            None
        }
    };

    let relays = match fetched {
        Some(mut relays) => {
            relays.truncate(5);
            info!("Fetched relays from nostr.watch");
            relays
        }
        None => {
            warn!("No usable relay list from nostr.watch, using fallback relays");
            POPULAR_RELAYS_FALLBACK
                .iter()
                .map(|s| s.to_string())
                .collect()
        }
    };

    info!("Relays: {:?}", relays);
    relays
}

async fn refresh_relays(client: &Client) {
    for relay in get_relays().await {
        if let Err(e) = client.add_relay(relay.as_str()).await {
            error!("Failed to add relay {}: {}", relay, e);
        }
    }

    client.connect().await;
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

        refresh_relays(&client).await;

        let (json_actor, _) = Actor::spawn_linked(
            Some("JsonActor".to_string()),
            JsonActor,
            myself.clone(),
            myself.clone().into(),
        )
        .await?;

        let mut state = State {
            json_actor,
            client,
            cancellation_token: None,
            subscription_ids: HashMap::new(),
            latest_event_received_at: Utc::now(),
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
                        "No events received in the last 5 minutes, refreshing relays and resetting notification handler"
                    );

                    refresh_relays(&state.client).await;
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
}
