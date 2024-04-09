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

const POPULAR_RELAYS: [&str; 4] = [
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://relay.n057r.club",
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
        let filters = vec![Filter::new().limit(1)];
        info!("Filter is {:?}", filters);

        let main_sub_id = self.client.subscribe(filters.clone(), None).await;
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
                            NostrActorMessage::NewEvent(event, subscription_id, relay_url)
                        )?;
                        Ok(false)
                    }
                    RelayPoolNotification::Message { message, .. } => {
                        match message {
                            RelayMessage::Notice { message, .. } => {
                                info!("Received notice: {:?}", message);
                            }
                            RelayMessage::Closed {
                                subscription_id,
                                message,
                            } => {
                                info!("Subscription {} closed: {}", subscription_id, message);
                                if subscription_id == main_sub_id {
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
                    RelayPoolNotification::RelayStatus { relay_url, status } => {
                        info!("Relay status: {:?} - {:?}", relay_url, status);
                        Ok(false)
                    }
                    RelayPoolNotification::Stop => {
                        info!("Stop");
                        Ok(true)
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
        let filters = vec![Filter::new()
            .limit(1)
            .kind(Kind::ParameterizedReplaceable(31990))
            .custom_tag(SingleLetterTag::lowercase(Alphabet::K), [kind.to_string()])];

        let opts = SubscribeAutoCloseOptions::default()
            .timeout(Some(Duration::from_secs(5)))
            .filter(FilterOptions::ExitOnEOSE);

        let id = SubscriptionId::generate();
        self.subscription_ids.insert(id.clone(), ());
        self.client
            .subscribe_with_id(id.clone(), filters, Some(opts))
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
        let opts = Options::new()
            .wait_for_send(false)
            .connection_timeout(Some(Duration::from_secs(60)))
            .wait_for_subscription(true);

        let client = ClientBuilder::new().opts(opts).build();

        let opts = RelayOptions::default().ping(true);
        for relay in POPULAR_RELAYS.into_iter() {
            client.add_relay_with_opts(relay, opts.clone()).await?;
        }

        client.connect().await;

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
                        "No events received in the last 5 minutes, resetting notification handler"
                    );

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
