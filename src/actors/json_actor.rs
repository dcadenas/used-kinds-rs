mod get_recommended_app_string;
use super::nostr_actor::NostrActorMessage;
use crate::actors::http_actor::{HttpActor, HttpActorMessage};
use crate::utils::is_kind_free;
use crate::utils::should_log;
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
use tracing::{debug, error, info};

lazy_static! {
    static ref STATS_FILE: String =
        env::var("STATS_FILE").unwrap_or_else(|_| "/var/data/stats.json".to_string());
}

pub struct JsonActor;

#[derive(Debug)]
pub enum JsonActorMessage {
    GetStatsVec((), RpcReplyPort<Vec<(Kind, KindEntry)>>),
    RecordEvent(Box<Event>, Url),
    RecordRecommendedApp(Box<Event>, Url),
    SaveState,
    CleanupDocumentedKinds,
    ComputeClusters,
    ApplyClusters(HashMap<Kind, (u32, f64)>), // (cluster_id, similarity_score)
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KindEntry {
    pub event: Event,
    pub count: u64,
    pub last_updated: i64,
    pub recommended_app: Option<String>,
    pub recommended_app_event: Option<Event>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cluster_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cluster_similarity: Option<f64>,
}

pub struct State {
    kind_stats: HashMap<Kind, KindEntry>,
    http_actor: ActorRef<HttpActorMessage>,
    nostr_actor: ActorRef<NostrActorMessage>,
    recommended_apps: HashMap<Kind, Timestamp>,
    clustering_in_progress: bool,
    qdrant_client: Option<qdrant_client::Qdrant>,
}

impl State {
    fn maybe_refresh_recommended_app(&mut self, kind: Kind) -> Result<()> {
        let current_time = Timestamp::now();
        let one_hour_ago = current_time - Duration::from_secs(60 * 60);

        let update_needed = match self.recommended_apps.get(&kind) {
            Some(last_updated) => *last_updated < one_hour_ago,
            None => true,
        };

        if update_needed {
            self.recommended_apps.insert(kind, current_time);
            cast!(self.nostr_actor, NostrActorMessage::GetRecommendedApp(kind))?;
        }

        Ok(())
    }
}

fn is_old(unix_time: i64) -> bool {
    unix_time < (Utc::now() - chrono::Duration::days(30)).timestamp_millis()
}

/// Query all events from Qdrant
async fn query_all_from_qdrant(client: &qdrant_client::Qdrant) -> Result<Vec<(Kind, KindEntry)>> {
    use qdrant_client::qdrant::ScrollPointsBuilder;

    let collection = crate::qdrant_client::collection_name();
    let mut results = Vec::new();
    let mut offset: Option<qdrant_client::qdrant::PointId> = None;

    // Scroll through all points (paginated)
    loop {
        let mut scroll_builder = ScrollPointsBuilder::new(collection)
            .with_payload(true)
            .limit(100);

        if let Some(offset_id) = offset {
            scroll_builder = scroll_builder.offset(offset_id);
        }

        let response = client.scroll(scroll_builder.build()).await?;

        for point in &response.result {
            let kind_num = point.id.as_ref().and_then(|id| {
                if let qdrant_client::qdrant::point_id::PointIdOptions::Num(n) = id.point_id_options.as_ref()? {
                    Some(*n as u16)
                } else {
                    None
                }
            });

            if let Some(kind_num) = kind_num {
                match crate::qdrant_client::payload_to_kind_entry(&point.payload) {
                    Ok(entry) => {
                        results.push((Kind::from(kind_num), entry));
                    }
                    Err(e) => {
                        error!("Failed to deserialize kind {}: {}", kind_num, e);
                    }
                }
            }
        }

        // Check if there are more results
        offset = response.result.last().and_then(|p| p.id.clone());
        if response.result.is_empty() || response.next_page_offset.is_none() {
            break;
        }
    }

    // Sort by kind number
    results.sort_by_key(|(kind, _)| *kind);

    Ok(results)
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
        myself.send_interval(Duration::from_secs(60 * 60 * 6), || {
            JsonActorMessage::CleanupDocumentedKinds
        });
        // Clustering every 5 minutes (with debouncing to prevent overlaps)
        myself.send_interval(Duration::from_secs(60 * 5), || {
            JsonActorMessage::ComputeClusters
        });
        let (http_actor, _) = Actor::spawn_linked(
            Some("HttpActor".to_string()),
            HttpActor,
            myself.clone(),
            myself.into(),
        )
        .await?;
        let recommended_apps = HashMap::new();

        // Initialize Qdrant client (optional - graceful degradation if unavailable)
        let qdrant_client = match crate::qdrant_client::initialize_qdrant().await {
            Ok(client) => {
                info!("Qdrant client initialized successfully");

                // Migrate stats.json on first startup
                match crate::qdrant_client::migrate_from_stats_json(&client, &*STATS_FILE).await {
                    Ok(true) => info!("Completed one-time migration from stats.json to Qdrant"),
                    Ok(false) => info!("No migration needed"),
                    Err(e) => error!("Migration failed (continuing anyway): {}", e),
                }

                Some(client)
            }
            Err(e) => {
                error!("Failed to initialize Qdrant client: {}. Continuing without Qdrant.", e);
                None
            }
        };

        let state = State {
            kind_stats,
            http_actor,
            nostr_actor,
            recommended_apps,
            clustering_in_progress: false,
            qdrant_client,
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
                        e.event = *event.clone();
                        e.count += 1;
                        e.last_updated = Utc::now().timestamp_millis();
                    })
                    .or_insert_with(|| KindEntry {
                        event: *event.clone(),
                        count: 1,
                        last_updated: Utc::now().timestamp_millis(),
                        recommended_app: None,
                        recommended_app_event: None,
                        cluster_id: None,
                        cluster_similarity: None,
                    });

                // Also upsert to Qdrant if available
                if let Some(ref client) = state.qdrant_client {
                    let kind_entry = state.kind_stats.get(&event.kind).unwrap();

                    // Generate vector from event features
                    let features = crate::similarity::EventFeatures::from_event(&event);
                    let vector = features.to_vector();

                    // Create payload from KindEntry
                    use nostr_sdk::JsonUtil;

                    let payload_json = serde_json::json!({
                        "kind": u16::from(event.kind),
                        "count": kind_entry.count,
                        "last_updated": kind_entry.last_updated,
                        "recommended_app": kind_entry.recommended_app,
                        "event_id": event.id.to_string(),
                        "event": event.as_json(),
                    });

                    // Convert to Qdrant Payload (from serde_json::Map)
                    let payload = qdrant_client::Payload::from(
                        payload_json.as_object().cloned().unwrap_or_default()
                    );

                    // Upsert to Qdrant (async, fire-and-forget to avoid blocking)
                    let client_clone = client.clone();
                    let kind_id = u16::from(event.kind) as u64;
                    let collection = crate::qdrant_client::collection_name().to_string();

                    tokio::spawn(async move {
                        use qdrant_client::qdrant::{PointStruct, UpsertPointsBuilder};

                        let point = PointStruct::new(kind_id, vector, payload);

                        if let Err(e) = client_clone
                            .upsert_points(UpsertPointsBuilder::new(collection, vec![point]).build())
                            .await
                        {
                            error!("Failed to upsert to Qdrant: {}", e);
                        }
                    });
                }

                if let Err(e) = state.maybe_refresh_recommended_app(event.kind) {
                    error!("Failed to refresh recommended app: {}", e);
                }
            }
            JsonActorMessage::RecordRecommendedApp(event, _url) => {
                match parse_recommended_app(&event) {
                    Ok((kinds, recommended_app)) => {
                        kinds.iter().for_each(|kind| {
                            state.kind_stats.entry(*kind).and_modify(|e| {
                                e.recommended_app = Some(recommended_app.clone());
                                e.recommended_app_event = Some(*event.clone());
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
                if !reply.is_closed() {
                    // Query Qdrant if available, otherwise fall back to HashMap
                    let data_as_sorted_vec = if let Some(ref client) = state.qdrant_client {
                        match query_all_from_qdrant(client).await {
                            Ok(vec) => {
                                info!("Sending {} events from Qdrant", vec.len());
                                vec
                            }
                            Err(e) => {
                                error!("Failed to query Qdrant, falling back to HashMap: {}", e);
                                // Fallback to HashMap
                                let mut vec: Vec<(Kind, KindEntry)> = state
                                    .kind_stats
                                    .clone()
                                    .into_iter()
                                    .filter(|(_, v)| !is_old(v.last_updated))
                                    .collect();
                                vec.sort_by_key(|(kind, _)| *kind);
                                vec
                            }
                        }
                    } else {
                        // No Qdrant, use HashMap
                        let mut vec: Vec<(Kind, KindEntry)> = state
                            .kind_stats
                            .clone()
                            .into_iter()
                            .filter(|(_, v)| !is_old(v.last_updated))
                            .collect();
                        vec.sort_by_key(|(kind, _)| *kind);
                        vec
                    };

                    if let Err(e) = reply.send(data_as_sorted_vec) {
                        error!("Error when sending sorted vec: {}", e);
                    }
                }
            }
            JsonActorMessage::CleanupDocumentedKinds => {
                // Collect kinds to remove (newly-documented + old events)
                let kinds_to_remove: Vec<Kind> = state
                    .kind_stats
                    .iter()
                    .filter(|(kind, entry)| !is_kind_free(**kind) || is_old(entry.last_updated))
                    .map(|(kind, _)| *kind)
                    .collect();

                if !kinds_to_remove.is_empty() {
                    info!("Cleaning up {} kinds (documented or old)", kinds_to_remove.len());

                    // Remove from HashMap
                    for kind in &kinds_to_remove {
                        state.kind_stats.remove(kind);
                    }

                    // Also delete from Qdrant
                    if let Some(ref client) = state.qdrant_client {
                        let client_clone = client.clone();
                        let collection = crate::qdrant_client::collection_name().to_string();
                        let point_ids: Vec<u64> = kinds_to_remove
                            .iter()
                            .map(|k| u16::from(*k) as u64)
                            .collect();

                        tokio::spawn(async move {
                            use qdrant_client::qdrant::DeletePointsBuilder;

                            let delete_request = DeletePointsBuilder::new(collection)
                                .points(point_ids)
                                .build();

                            if let Err(e) = client_clone.delete_points(delete_request).await {
                                error!("Failed to delete points from Qdrant: {}", e);
                            } else {
                                info!("Deleted {} points from Qdrant", kinds_to_remove.len());
                            }
                        });
                    }

                    if let Err(e) = save_stats_to_json(&state.kind_stats).await {
                        error!("Failed to save stats after cleanup: {}", e);
                    }
                } else {
                    info!("No kinds to clean up");
                }
            }
            JsonActorMessage::ComputeClusters => {
                // Debounce: skip if clustering is already running
                if state.clustering_in_progress {
                    debug!("Skipping clustering - already in progress");
                    return Ok(());
                }

                info!("Starting background clustering for {} kinds", state.kind_stats.len());
                state.clustering_in_progress = true;

                // Clone events for background processing (Kind -> Event)
                let events_owned: HashMap<Kind, Event> = state
                    .kind_stats
                    .iter()
                    .map(|(kind, entry)| (*kind, entry.event.clone()))
                    .collect();

                let myself_clone = _myself.clone();

                // Spawn background task for clustering
                tokio::spawn(async move {
                    info!("Computing similarity clusters in background...");

                    // Convert owned events to borrowed references for clustering
                    let events_map: HashMap<Kind, &Event> = events_owned
                        .iter()
                        .map(|(kind, event)| (*kind, event))
                        .collect();

                    // Compute clusters with threshold of 0.9 (very high similarity required)
                    let clusters = crate::similarity::cluster_kinds(&events_map, 0.9);

                    let cluster_count = clusters.len();
                    let unique_clusters = clusters.values()
                        .map(|(cluster_id, _)| cluster_id)
                        .collect::<std::collections::HashSet<_>>()
                        .len();

                    info!("Background clustering completed: {} clustered kinds in {} clusters", cluster_count, unique_clusters);

                    // Send results back to actor
                    if let Err(e) = cast!(myself_clone, JsonActorMessage::ApplyClusters(clusters)) {
                        error!("Failed to send cluster results to actor: {}", e);
                    }
                });
            }
            JsonActorMessage::ApplyClusters(clusters) => {
                info!("Applying cluster results to {} kinds", clusters.len());

                // First, clear all existing cluster_ids and similarities
                for entry in state.kind_stats.values_mut() {
                    entry.cluster_id = None;
                    entry.cluster_similarity = None;
                }

                // Apply new cluster assignments with similarity scores
                for (kind, (cluster_id, similarity)) in clusters {
                    if let Some(entry) = state.kind_stats.get_mut(&kind) {
                        entry.cluster_id = Some(cluster_id);
                        entry.cluster_similarity = Some(similarity);
                    }
                }

                let cluster_count = state.kind_stats.values()
                    .filter_map(|e| e.cluster_id)
                    .collect::<std::collections::HashSet<_>>()
                    .len();

                info!("Applied {} clusters to kind_stats", cluster_count);

                // Mark clustering as complete
                state.clustering_in_progress = false;
            }
        }

        Ok(())
    }
}

async fn save_stats_to_json(kind_stats: &HashMap<Kind, KindEntry>) -> Result<()> {
    let json_str = serde_json::to_string_pretty(kind_stats)?;
    tokio::fs::write(&*STATS_FILE, json_str).await?;
    debug!("Stats saved to json file");
    Ok(())
}
