use crate::actors::json_actor::{JsonActor, JsonActorMessage};
use crate::utils::is_kind_free;
use anyhow::Result;
use nostr_sdk::prelude::*;
use ractor::{cast, concurrency::Duration, Actor, ActorProcessingErr, ActorRef, SupervisionEvent};
use std::collections::HashMap;
use tracing::{error, info};

const POPULAR_RELAYS: [&str; 5] = [
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://relayable.org",
    "wss://relay.n057r.club",
    "wss://relay.snort.social",
];

pub struct NostrActor;
pub struct State {
    json_actor: ActorRef<JsonActorMessage>,
    client: Client,
    recommended_aps: HashMap<Kind, (String, Timestamp)>,
}

impl State {
    async fn get_events(&mut self) -> Result<()> {
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
                    if let Err(e) = self.process_event(event).await {
                        error!("Failed to process event: {}", e);
                    }
                }
            }
            Err(e) => error!("Failed to get events: {}", e),
        }

        Ok(())
    }

    async fn process_event(&mut self, event: Event) -> Result<()> {
        if is_kind_free(event.kind.as_u32()) {
            let current_time = Timestamp::now();
            let five_mins_ago = current_time - Duration::from_secs(60 * 5);

            let update_needed = match self.recommended_aps.get(&event.kind) {
                Some((_, last_updated)) if *last_updated < five_mins_ago => true,
                Some((recommended_app, _)) => {
                    // If the recommended app is up-to-date, use it without updating.
                    self.send_event(event.clone(), recommended_app.clone())
                        .await?;
                    false
                }
                None => true,
            };

            if update_needed {
                // Fetch and update the recommended app if it's outdated or missing.
                let recommended_app = self.get_recommended_app(event.kind).await?;
                self.recommended_aps
                    .insert(event.kind, (recommended_app.clone(), current_time));
                self.send_event(event, recommended_app).await?;
            }
        }
        Ok(())
    }

    async fn send_event(&self, event: Event, recommended_app: String) -> Result<()> {
        let relay_url = Url::parse("wss://relay.damus.io")?;
        cast!(
            self.json_actor,
            JsonActorMessage::RecordEvent(event, recommended_app, relay_url)
        )?;

        Ok(())
    }

    async fn get_recommended_app(&self, kind: Kind) -> Result<String> {
        let filters = vec![Filter::new()
            .limit(1)
            .kind(Kind::ParameterizedReplaceable(31990))
            .custom_tag(SingleLetterTag::lowercase(Alphabet::K), [kind.to_string()])];
        let recommended_apps = self
            .client
            .get_events_of(filters, Some(Duration::from_secs(1)))
            .await?;

        if let Some(app_event) = recommended_apps.first() {
            let alt_tag_data_value = app_event.tags().iter().find_map(|t| match t {
                Tag::Generic(TagKind::Custom(s), data) if s == "alt" && !data.is_empty() => {
                    data.first()
                }
                _ => None,
            });

            let recommended_app = alt_tag_data_value.unwrap_or(&app_event.content).clone();

            info!("Recommended app for kind {}: {}", kind, recommended_app);
            Ok(recommended_app)
        } else {
            Ok("None found".to_string())
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

        let every_5_minutes = Duration::from_secs(60 * 5);
        myself.send_interval(every_5_minutes, || NostrActorMessage::GetEvents);
        let (json_actor, _) =
            Actor::spawn_linked(Some("JsonActor".to_string()), JsonActor, (), myself.into())
                .await?;

        let state = State {
            json_actor: json_actor,
            client,
            recommended_aps: HashMap::new(),
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
                if let Err(e) = state.get_events().await {
                    error!("Failed to get events: {}", e)
                };
            }
        }

        Ok(())
    }
}
