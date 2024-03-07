use crate::actors::json_actor::{JsonActor, JsonActorMessage};
use crate::utils::is_kind_free;
use anyhow::Result;
use nostr_sdk::prelude::*;
use ractor::{cast, concurrency::Duration, Actor, ActorProcessingErr, ActorRef, SupervisionEvent};
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
        let (json_actor, _) =
            Actor::spawn_linked(Some("JsonActor".to_string()), JsonActor, (), myself.into())
                .await?;

        let state = State {
            json_actor: json_actor,
            client,
        };

        Ok(state)
    }

    async fn post_stop(
        &self,
        _: ActorRef<Self::Msg>,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        info!("Nostr actor stopped");
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
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            NostrActorMessage::GetEvents => {
                state.get_events().await;
            }
        }

        Ok(())
    }
}
