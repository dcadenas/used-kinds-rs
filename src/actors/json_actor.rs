use crate::actors::http_actor::{HttpActor, HttpActorMessage};
use crate::utils::is_kind_free;
use anyhow::Result;
use chrono::Utc;
use lazy_static::lazy_static;
use nostr_sdk::prelude::*;
use ractor::{
    concurrency::Duration, Actor, ActorProcessingErr, ActorRef, RpcReplyPort, SupervisionEvent,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use tracing::{error, info};

lazy_static! {
    static ref STATS_FILE: String =
        env::var("STATS_FILE").unwrap_or_else(|_| "/var/data/stats.json".to_string());
}

pub struct JsonActor;

#[derive(Debug)]
pub enum JsonActorMessage {
    GetStatsVec((), RpcReplyPort<Vec<(u32, KindEntry)>>),
    RecordEvent(Event, Url),
    SaveState,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KindEntry {
    event: Event,
    count: u64,
    last_updated: i64,
}

pub struct State {
    kind_stats: HashMap<u32, KindEntry>,
    http_actor: ActorRef<HttpActorMessage>,
}

#[ractor::async_trait]
impl Actor for JsonActor {
    type Msg = JsonActorMessage;
    type State = State;
    type Arguments = ();

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        _: (),
    ) -> Result<Self::State, ActorProcessingErr> {
        let json_str = tokio::fs::read_to_string(&*STATS_FILE)
            .await
            .unwrap_or_else(|e| {
                error!("Failed to read stats file, defaulting to empty: {}", e);
                "{}".to_string()
            });

        let mut kind_stats: HashMap<u32, KindEntry> = serde_json::from_str(&json_str)
            .unwrap_or_else(|e| {
                error!("Failed to read stats file, defaulting to empty: {}", e);
                HashMap::default()
            });

        // Remove any entries that are older than 1 month or for which is_kind_free is false
        kind_stats.retain(|kind, entry| {
            let is_old =
                entry.last_updated < (Utc::now() - chrono::Duration::days(30)).timestamp_millis();
            !is_old && is_kind_free(*kind)
        });

        myself.send_interval(Duration::from_secs(10), || JsonActorMessage::SaveState);
        let (http_actor, _) = Actor::spawn_linked(
            Some("HttpActor".to_string()),
            HttpActor,
            myself.clone(),
            myself.into(),
        )
        .await?;

        let state = State {
            kind_stats,
            http_actor,
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
                state
                    .kind_stats
                    .entry(event.kind.as_u32())
                    .and_modify(|e| {
                        e.event = event.clone();
                        e.count += 1;
                        e.last_updated = Utc::now().timestamp_millis();
                    })
                    .or_insert_with(|| KindEntry {
                        event,
                        count: 1,
                        last_updated: Utc::now().timestamp_millis(),
                    });
            }
            JsonActorMessage::SaveState => {
                save_stats_to_json(&state.kind_stats).await?;
            }
            JsonActorMessage::GetStatsVec(_arg, reply) => {
                let mut data_as_sorted_vec: Vec<(u32, KindEntry)> =
                    state.kind_stats.clone().into_iter().collect();
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

async fn save_stats_to_json(kind_stats: &HashMap<u32, KindEntry>) -> Result<()> {
    let json_str = serde_json::to_string_pretty(kind_stats)?;
    tokio::fs::write(&*STATS_FILE, json_str).await?;
    info!("Stats saved to json file");
    Ok(())
}
