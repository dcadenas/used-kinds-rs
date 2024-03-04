use crate::services::json_service::JsonActorMessage;
use crate::utils::is_kind_free;
use anyhow::Result;
use nostr_sdk::prelude::*;
use ractor::{cast, concurrency::Duration, Actor, ActorProcessingErr, ActorRef};
use tracing::{debug, error, info};

const POPULAR_RELAYS: [&str; 4] = [
    "wss://relay.damus.io",
    "wss://relay.plebstr.com",
    "wss://relayable.org",
    "wss://relay.n057r.club",
];

pub struct NostrActor;
pub struct State {
    json_actor: ActorRef<JsonActorMessage>,
    client: Client,
}

impl State {
    async fn get_events(&self) {
        let amount = 1000;

        let duration = Duration::from_secs(60 * 5);

        let since = Timestamp::now() - duration;
        let filters = vec![Filter::new().limit(amount).since(since)];

        match self
            .client
            .get_events_of_with_opts(
                filters,
                Some(Duration::from_secs(60)),
                FilterOptions::WaitDurationAfterEOSE(Duration::from_secs(10)),
            )
            .await
        {
            Ok(events) => {
                for event in events {
                    if is_kind_free(event.kind.as_u32()) {
                        debug!("Received a new kind event: {:?}", event);
                        info!("Received a new event of kind {}", event.kind);
                        // TODO: How can I know where was this event found? FTM it's hardcoded
                        let relay_url = Url::parse("wss://relay.damus.io").unwrap();
                        if let Err(e) = cast!(
                            self.json_actor,
                            JsonActorMessage::RecordEvent(event, relay_url)
                        ) {
                            error!("Failed to send the new kind event: {}", e);
                        }
                    }
                }
            }
            Err(e) => error!("Failed to get events: {}", e),
        }
    }
}

#[derive(Debug, Clone)]
pub enum NostrActorMessage {
    GetEvents,
    Stop,
}

#[ractor::async_trait]
impl Actor for NostrActor {
    type Msg = NostrActorMessage;
    type State = State;
    type Arguments = ActorRef<JsonActorMessage>;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        json_actor: ActorRef<JsonActorMessage>,
    ) -> Result<Self::State, ActorProcessingErr> {
        let opts = Options::new()
            .wait_for_send(false)
            .connection_timeout(Some(Duration::from_secs(5)))
            .shutdown_on_drop(true)
            .wait_for_subscription(true);

        opts.pool.shutdown_on_drop(true);
        let client = ClientBuilder::new().opts(opts).build();

        for relay in POPULAR_RELAYS.into_iter() {
            client.add_relay(relay).await?;
        }

        client.connect().await;

        let duration = Duration::from_secs(60 * 5);
        myself.send_interval(duration, || NostrActorMessage::GetEvents);

        let state = State {
            json_actor: json_actor,
            client,
        };

        Ok(state)
    }

    async fn post_stop(
        &self,
        _: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        if let Err(e) = cast!(state.json_actor, JsonActorMessage::Stop) {
            error!("Failed to send stop message to json actor: {}", e);
        }

        info!("Nostr service exited");
        Ok(())
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            NostrActorMessage::GetEvents => {
                state.get_events().await;
                Ok(())
            }
            NostrActorMessage::Stop => {
                myself.stop(None);
                Ok(())
            }
        }
    }
}
