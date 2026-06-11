use crate::{
    actors::json_actor::{JsonActor, JsonActorMessage},
    utils::{is_kind_free, should_log},
};
use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use nostr_sdk::prelude::*;
use ractor::{cast, concurrency::Duration, Actor, ActorProcessingErr, ActorRef, SupervisionEvent};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

// Always-on popular relays (never rotated)
const ALWAYS_ON_RELAYS: [&str; 3] = [
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://nos.lol",
];

/// Curated fallback when NIP-66 discovery yields nothing. The first three
/// are the always-on set, so the rotating pool drawn from here is the tail.
const POPULAR_RELAYS_FALLBACK: [&str; 8] = [
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://nos.lol",
    "wss://relay.snort.social",
    "wss://relay.nostr.band",
    "wss://nostr.mom",
    "wss://offchain.pub",
    "wss://relay.mostr.pub",
];

/// Relays that NIP-66 monitors publish kind 30166 relay-discovery events to.
const MONITOR_RELAYS: [&str; 2] = ["wss://relay.nostr.watch", "wss://monitorlizard.nostr1.com"];

/// Hard ceiling for one relay-discovery attempt. Discovery runs inside
/// pre_start and the actor's message loop, so it must never hang: on
/// timeout the caller falls back to POPULAR_RELAYS_FALLBACK.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Extract clearnet relay URLs from NIP-66 relay-discovery events.
///
/// Takes the `d` tag (the relay's normalized URL), keeps only `wss://`
/// clearnet hosts, trims trailing slashes so the result compares equal to
/// the curated lists, and dedupes across monitors. Candidates are
/// interleaved round-robin across publisher pubkeys (each keeping its own
/// arrival order) per NIP-66's advice against trusting a single source:
/// the caller takes only the head of this list, so without the interleave
/// one misbehaving monitor bursting events would control every pick.
/// Keys are free, so this bounds a misbehaving publisher, not a
/// determined sybil — the monitor relays' write policy is the gate there.
fn relay_urls_from_discovery_events<'a, I>(events: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a Event>,
{
    let mut seen = std::collections::HashSet::new();
    let mut publisher_order = Vec::new();
    let mut per_publisher: std::collections::HashMap<PublicKey, Vec<String>> =
        std::collections::HashMap::new();

    for event in events {
        let Some(d_value) = event.tags.identifier() else {
            continue;
        };
        let Some(rest) = d_value.strip_prefix("wss://") else {
            // ws:// (plaintext), http gateways, or a bare monitor pubkey
            continue;
        };
        let host = rest
            .split('/')
            .next()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("");
        if host.is_empty()
            || [".onion", ".i2p", ".loki"]
                .iter()
                .any(|overlay| host.ends_with(overlay))
        {
            continue;
        }

        let url = d_value.trim_end_matches('/').to_string();
        if seen.insert(url.clone()) {
            let relays = per_publisher.entry(event.pubkey).or_default();
            if relays.is_empty() {
                publisher_order.push(event.pubkey);
            }
            relays.push(url);
        }
    }

    let mut urls = Vec::with_capacity(seen.len());
    for round in 0.. {
        let mut exhausted = true;
        for publisher in &publisher_order {
            if let Some(url) = per_publisher.get(publisher).and_then(|r| r.get(round)) {
                urls.push(url.clone());
                exhausted = false;
            }
        }
        if exhausted {
            break;
        }
    }

    urls
}

/// Fetch currently-online relay candidates via NIP-66 (kind 30166) events.
///
/// Returns `None` when no monitor answers in time or nothing usable arrives,
/// so the caller falls back to the curated list. The inner fetch timeout is
/// shorter than the outer ceiling so partial results win over a hard cut.
async fn fetch_online_relays() -> Option<Vec<String>> {
    let fetch = async {
        let client = Client::default();
        for monitor in MONITOR_RELAYS {
            if let Err(e) = client.add_relay(monitor).await {
                warn!("Failed to add monitor relay {}: {}", monitor, e);
            }
        }
        client.connect().await;

        let since = Timestamp::now() - Duration::from_secs(2 * 60 * 60);
        let filter = Filter::new()
            .kind(Kind::from_u16(30166))
            .since(since)
            .limit(200);
        let events = client
            .fetch_events(filter, DISCOVERY_TIMEOUT - Duration::from_secs(2))
            .await;
        client.shutdown().await;

        match events {
            Ok(events) => {
                let urls = relay_urls_from_discovery_events(events.iter());
                if urls.is_empty() {
                    None
                } else {
                    Some(urls)
                }
            }
            Err(e) => {
                error!("Failed to fetch NIP-66 relay discovery events: {}", e);
                None
            }
        }
    };

    match tokio::time::timeout(DISCOVERY_TIMEOUT, fetch).await {
        Ok(result) => result,
        Err(_) => {
            warn!("NIP-66 relay discovery timed out");
            None
        }
    }
}

/// Get rotating relays, excluding always-on relays and previously used relays
async fn get_rotating_relays(exclude: &[String]) -> Vec<String> {
    let fetched = fetch_online_relays().await;

    let always_on_set: Vec<String> = ALWAYS_ON_RELAYS.iter().map(|s| s.to_string()).collect();

    match fetched {
        Some(all_relays) => {
            let filtered: Vec<String> = all_relays
                .into_iter()
                .filter(|r| !always_on_set.contains(r) && !exclude.contains(r))
                .take(5)
                .collect();

            info!(
                "Fetched {} rotating relay candidates via NIP-66 discovery",
                filtered.len()
            );
            filtered
        }
        None => {
            warn!("No usable relay list from NIP-66 discovery, using fallback relays");
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

/// Add relays to the pool tolerantly, returning how many were accepted.
///
/// One malformed or rejected relay URL must not take the whole set down
/// with it; the caller decides whether zero usable relays is fatal.
async fn add_relays_warn_and_skip(client: &Client, relays: &[String]) -> usize {
    let mut usable = 0;
    for relay in relays {
        match client.add_relay(relay).await {
            Ok(_) => usable += 1,
            Err(e) => warn!("Skipping relay {}: {}", relay, e),
        }
    }
    usable
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

        // One bad relay URL must not abort startup (warn-and-skip), but a
        // pool with zero relays is useless, so that still fails the boot.
        let (all_relays, rotating_relays) = get_initial_relays().await;
        let usable = add_relays_warn_and_skip(&client, &all_relays).await;
        if usable == 0 {
            return Err(ActorProcessingErr::from(
                "no usable relays at startup".to_string(),
            ));
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

    fn discovery_event_signed_by(keys: &Keys, d_value: &str) -> Event {
        let unsigned = EventBuilder::new(Kind::from(30166u16), "")
            .tag(Tag::identifier(d_value))
            .build(keys.public_key());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async { keys.sign_event(unsigned).await.unwrap() })
    }

    fn discovery_event(d_value: &str) -> Event {
        discovery_event_signed_by(&Keys::generate(), d_value)
    }

    #[test]
    fn test_relay_urls_from_discovery_events_extracts_wss_d_tags() {
        let events = [
            discovery_event("wss://relay.one.example/"),
            discovery_event("wss://relay.two.example"),
        ];
        assert_eq!(
            relay_urls_from_discovery_events(events.iter()),
            vec![
                "wss://relay.one.example".to_string(),
                "wss://relay.two.example".to_string()
            ],
            "trailing slashes are trimmed so urls compare equal to the curated lists"
        );
    }

    #[test]
    fn test_relay_urls_from_discovery_events_skips_overlay_and_non_wss() {
        let events = [
            discovery_event("wss://abcdef0123456789.onion"),
            discovery_event("wss://relay.example.i2p/"),
            discovery_event("wss://some.relay.loki"),
            discovery_event("ws://plaintext.example"),
            discovery_event("https://not-a-relay.example"),
            // NIP-66 allows a hex pubkey instead of a URL
            discovery_event("aa11bb22cc33dd44ee55ff66aa77bb88cc99dd00ee11ff22aa33bb44cc55dd66"),
            discovery_event("wss://good.relay.example"),
        ];
        assert_eq!(
            relay_urls_from_discovery_events(events.iter()),
            vec!["wss://good.relay.example".to_string()]
        );
    }

    #[test]
    fn test_relay_urls_interleave_across_monitor_pubkeys() {
        // One flooding publisher must not own the list head: downstream
        // takes only the first 5 candidates, so without interleaving a
        // single pubkey bursting events controls every rotation pick.
        let flood = Keys::generate();
        let honest = Keys::generate();
        let mut events = Vec::new();
        for i in 0..10 {
            events.push(discovery_event_signed_by(
                &flood,
                &format!("wss://flood{i}.example"),
            ));
        }
        events.push(discovery_event_signed_by(&honest, "wss://honest1.example"));
        events.push(discovery_event_signed_by(&honest, "wss://honest2.example"));

        let urls = relay_urls_from_discovery_events(events.iter());
        assert_eq!(urls.len(), 12, "no relay is dropped, only reordered");
        assert_eq!(
            &urls[..5],
            &[
                "wss://flood0.example".to_string(),
                "wss://honest1.example".to_string(),
                "wss://flood1.example".to_string(),
                "wss://honest2.example".to_string(),
                // honest publisher exhausted; flood fills the tail
                "wss://flood2.example".to_string(),
            ]
        );
    }

    #[test]
    fn test_relay_urls_from_discovery_events_dedupes_across_monitors() {
        // Two monitors reporting the same relay must yield one entry
        let events = [
            discovery_event("wss://relay.one.example/"),
            discovery_event("wss://relay.one.example"),
            discovery_event("wss://relay.two.example"),
        ];
        assert_eq!(
            relay_urls_from_discovery_events(events.iter()),
            vec![
                "wss://relay.one.example".to_string(),
                "wss://relay.two.example".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_add_relays_warn_and_skip_counts_only_usable_relays() {
        let client = Client::default();
        let relays = vec![
            "not a url at all".to_string(),
            "wss://relay.ok.example".to_string(),
        ];
        let usable = add_relays_warn_and_skip(&client, &relays).await;
        assert_eq!(usable, 1, "the malformed url is skipped, not fatal");
        assert!(client
            .relays()
            .await
            .keys()
            .any(|u| u.to_string().starts_with("wss://relay.ok.example")));
    }

    #[tokio::test]
    #[ignore] // Hits live NIP-66 monitor relays; run manually:
              // cargo test test_fetch_online_relays -- --ignored
    async fn test_fetch_online_relays_returns_live_relay_list() {
        let fetched = fetch_online_relays().await;
        let relays = fetched.expect("NIP-66 monitors should yield at least one relay");
        assert!(!relays.is_empty());
        assert!(relays.iter().all(|r| r.starts_with("wss://")));
        println!("NIP-66 discovery returned {} relays", relays.len());
        for r in relays.iter().take(10) {
            println!("  {r}");
        }
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
