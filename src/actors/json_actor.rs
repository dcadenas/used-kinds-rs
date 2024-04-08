mod get_recommended_app_string;
use super::nostr_actor::NostrActorMessage;
use crate::actors::http_actor::{HttpActor, HttpActorMessage};
use crate::utils::is_kind_free;
use anyhow::Result;
use chrono::Utc;
use get_recommended_app_string::parse_recommended_app;
use lazy_static::lazy_static;
use nostr_sdk::prelude::*;
use ractor::{
    cast, concurrency::Duration, Actor, ActorProcessingErr, ActorRef, RpcReplyPort,
    SupervisionEvent,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{error, info};

lazy_static! {
    static ref STATS_FILE: String =
        env::var("STATS_FILE").unwrap_or_else(|_| "/var/data/stats.json".to_string());
}

pub struct JsonActor;

#[derive(Debug)]
pub enum JsonActorMessage {
    GetStatsVec((), RpcReplyPort<Vec<(Kind, KindEntry)>>),
    RecordEvent(Event, Url),
    RecordRecommendedApp(Option<Event>, Url),
    SaveState,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KindEntry {
    event: Event,
    count: u64,
    last_updated: i64,
    recommended_app: Option<String>,
    recommended_app_event: Option<Event>,
}

pub struct State {
    kind_stats: HashMap<Kind, KindEntry>,
    http_actor: ActorRef<HttpActorMessage>,
    nostr_actor: ActorRef<NostrActorMessage>,
    recommended_apps: HashMap<Kind, (String, Timestamp)>,
}

impl State {
    fn maybe_refresh_recommended_app(&mut self, kind: Kind) -> Result<()> {
        let current_time = Timestamp::now();
        let five_mins_ago = current_time - Duration::from_secs(60 * 5);

        let update_needed = match self.recommended_apps.get(&kind) {
            Some((_, last_updated)) => *last_updated < five_mins_ago,
            None => true,
        };

        if update_needed {
            cast!(self.nostr_actor, NostrActorMessage::GetRecommendedApp(kind))?;
        }

        Ok(())
    }
}

fn is_old(unix_time: i64) -> bool {
    unix_time < (Utc::now() - chrono::Duration::days(30)).timestamp_millis()
}

#[ractor::async_trait]
impl Actor for JsonActor {
    type Msg = JsonActorMessage;
    type State = State;
    type Arguments = ActorRef<NostrActorMessage>;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        nostr_actor: ActorRef<NostrActorMessage>,
    ) -> Result<Self::State, ActorProcessingErr> {
        let json_str = tokio::fs::read_to_string(&*STATS_FILE)
            .await
            .unwrap_or_else(|e| {
                error!("Failed to read stats file, defaulting to empty: {}", e);
                "{}".to_string()
            });

        let mut kind_stats: HashMap<Kind, KindEntry> = serde_json::from_str(&json_str)
            .unwrap_or_else(|e| {
                error!("Failed to read stats file, defaulting to empty: {}", e);
                HashMap::default()
            });

        // Remove any entries that are older than 1 month or for which is_kind_free is false
        kind_stats.retain(|kind, entry| !is_old(entry.last_updated) && is_kind_free(*kind));

        myself.send_interval(Duration::from_secs(60), || JsonActorMessage::SaveState);
        let (http_actor, _) = Actor::spawn_linked(
            Some("HttpActor".to_string()),
            HttpActor,
            myself.clone(),
            myself.into(),
        )
        .await?;
        let recommended_apps = HashMap::new();

        let state = State {
            kind_stats,
            http_actor,
            nostr_actor,
            recommended_apps,
        };

        Ok(state)
    }

    async fn handle_supervisor_evt(
        &self,
        myself: ActorRef<Self::Msg>,
        message: SupervisionEvent,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SupervisionEvent::ActorPanicked(dead_actor, panic_msg)
                if dead_actor.get_id() == state.http_actor.get_id() =>
            {
                info!("JsonActor: {dead_actor:?} panicked with '{panic_msg}'");

                info!("JsonActor: Terminating json actor");
                myself.stop(Some("JsonActor died".to_string()));
            }
            other => {
                info!("JsonActor: received supervisor event '{other}'");
            }
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        _: ActorRef<Self::Msg>,
        _: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        info!("Json actor stopped");
        Ok(())
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            JsonActorMessage::RecordEvent(event, _url) => {
                if should_log() {
                    info!("Update for kind {}", event.kind);
                }

                state
                    .kind_stats
                    .entry(event.kind)
                    .and_modify(|e| {
                        e.event = event.clone();
                        e.count += 1;
                        e.last_updated = Utc::now().timestamp_millis();
                    })
                    .or_insert_with(|| KindEntry {
                        event: event.clone(),
                        count: 1,
                        last_updated: Utc::now().timestamp_millis(),
                        recommended_app: None,
                        recommended_app_event: None,
                    });

                if let Err(e) = state.maybe_refresh_recommended_app(event.kind) {
                    error!("Failed to refresh recommended app: {}", e);
                }
            }
            JsonActorMessage::RecordRecommendedApp(event, _url) => {
                match parse_recommended_app(event.as_ref()) {
                    Ok((kinds, recommended_app)) => {
                        kinds.iter().for_each(|kind| {
                            state.kind_stats.entry(*kind).and_modify(|e| {
                                e.recommended_app = Some(recommended_app.clone());
                                e.recommended_app_event = event.clone();
                            });
                        });
                    }
                    Err(e) => error!("Failed to parse recommended app: {}", e),
                }
            }
            JsonActorMessage::SaveState => {
                save_stats_to_json(&state.kind_stats).await?;
            }
            JsonActorMessage::GetStatsVec(_arg, reply) => {
                let mut data_as_sorted_vec: Vec<(Kind, KindEntry)> = state
                    .kind_stats
                    .clone()
                    .into_iter()
                    .filter(|(_, v)| !is_old(v.last_updated))
                    .collect();

                data_as_sorted_vec.sort_by_key(|(kind, _)| *kind);
                if !reply.is_closed() {
                    info!("Sending sorted vec");
                    if let Err(e) = reply.send(data_as_sorted_vec) {
                        error!("Error when sending sorted vec: {}", e);
                    }
                }
            }
        }

        Ok(())
    }
}

async fn save_stats_to_json(kind_stats: &HashMap<Kind, KindEntry>) -> Result<()> {
    let json_str = serde_json::to_string_pretty(kind_stats)?;
    tokio::fs::write(&*STATS_FILE, json_str).await?;
    info!("Stats saved to json file");
    Ok(())
}

fn should_log() -> bool {
    let interval_seconds = 10;
    let now = SystemTime::now();
    let since_the_epoch = now.duration_since(UNIX_EPOCH).expect("Time went backwards");

    since_the_epoch.as_secs() % interval_seconds == 0
}
