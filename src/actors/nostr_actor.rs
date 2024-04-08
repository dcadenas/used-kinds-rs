use crate::{
    actors::json_actor::{JsonActor, JsonActorMessage},
    utils::is_kind_free,
};
use anyhow::Result;
use nostr_sdk::prelude::*;
use ractor::{cast, concurrency::Duration, Actor, ActorProcessingErr, ActorRef, SupervisionEvent};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

const POPULAR_RELAYS: [&str; 5] = [
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://relayable.org",
    "wss://relay.n057r.club",
    "wss://relay.snort.social",
];

pub struct NostrActor;

#[derive(Clone)]
pub struct State {
    cancellation_token: CancellationToken,
    json_actor: ActorRef<JsonActorMessage>,
    client: Client,
}

impl State {
    async fn get_events(&self, cancellation_token: CancellationToken) -> Result<()> {
        let filters = vec![Filter::new()];

        self.client.subscribe(filters.clone()).await;
        self.client
            .handle_notifications(|notification| async {
                if cancellation_token.is_cancelled() {
                    // true breaks the loop
                    return Ok(true);
                }

                if let RelayPoolNotification::Event { event, .. } = notification {
                    if is_kind_free(event.kind) {
                        let relay_url = Url::parse("wss://relay.damus.io")?;
                        if let Err(e) = cast!(
                            self.json_actor,
                            JsonActorMessage::RecordEvent(event, relay_url)
                        ) {
                            error!("Failed to process event: {}", e);
                        }
                    }
                }
                Ok(false)
            })
            .await?;

        Ok(())
    }

    async fn get_recommended_app_event(&mut self, kind: Kind) -> Result<Option<Event>> {
        let filters = vec![Filter::new()
            .limit(1)
            .kind(Kind::ParameterizedReplaceable(31990))
            .custom_tag(SingleLetterTag::lowercase(Alphabet::K), [kind.to_string()])];

        let recommended_apps = self
            .client
            .get_events_of(filters, Some(Duration::from_secs(1)))
            .await?;

        Ok(recommended_apps.first().cloned())
    }
}

#[derive(Debug, Clone)]
pub enum NostrActorMessage {
    GetRecommendedApp(Kind),
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

        let (json_actor, _) = Actor::spawn_linked(
            Some("JsonActor".to_string()),
            JsonActor,
            myself.clone(),
            myself.into(),
        )
        .await?;

        let cancellation_token = CancellationToken::new();
        let cancellation_token_clone = cancellation_token.clone();
        let state = State {
            json_actor,
            client,
            cancellation_token,
        };
        let state_clone = state.clone();
        tokio::spawn(async move {
            while !cancellation_token_clone.is_cancelled() {
                let token_clone = cancellation_token_clone.clone();
                if let Err(e) = state_clone.get_events(token_clone).await {
                    error!("Failed to get events: {}", e);
                }

                // TODO: Exponential backoff
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });

        Ok(state)
    }

    async fn post_stop(
        &self,
        _: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        info!("Nostr actor stopped");
        state.cancellation_token.cancel();
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
            NostrActorMessage::GetRecommendedApp(kind) => {
                match state.get_recommended_app_event(kind).await {
                    Ok(maybe_app_event) => {
                        let relay_url = Url::parse("wss://relay.damus.io")?;
                        cast!(
                            state.json_actor,
                            JsonActorMessage::RecordRecommendedApp(maybe_app_event, relay_url)
                        )?;
                    }
                    Err(e) => error!("Failed to get recommended app event: {}", e),
                };
            }
        }

        Ok(())
    }
}
