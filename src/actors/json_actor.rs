mod get_recommended_app_string;
use super::nostr_actor::NostrActorMessage;
use crate::actors::http_actor::{HttpActor, HttpActorMessage};
use crate::utils::is_kind_free;
use crate::utils::should_log;
use anyhow::Result;
use chrono::Utc;
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
    http_actor: ActorRef<HttpActorMessage>,
    nostr_actor: ActorRef<NostrActorMessage>,
    recommended_apps: HashMap<Kind, Timestamp>,
    clustering_in_progress: bool,
    qdrant_client: qdrant_client::Qdrant, // Now required, not optional
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

/// Cosine score two kinds must reach to be clustered together. Recalibrate
/// against the logged best-neighbor histogram whenever the featurizer changes.
const CLUSTER_SIMILARITY_THRESHOLD: f32 = 0.8;

/// Whether a stored point was vectorized with an older featurizer version.
fn payload_needs_revectorization(payload: &HashMap<String, qdrant_client::qdrant::Value>) -> bool {
    payload.get("feature_version").and_then(|v| v.as_integer())
        != Some(crate::similarity::FEATURE_VERSION)
}

/// Recompute vectors for points stored with an older featurizer version.
///
/// The full event JSON lives in each payload, so this is mechanical. Without
/// it, vectors produced by different featurizer versions would be compared
/// against each other and similarity results would be silently wrong.
async fn revectorize_outdated_points(client: &qdrant_client::Qdrant) -> Result<usize> {
    use nostr_sdk::JsonUtil;
    use qdrant_client::qdrant::{PointStruct, UpsertPointsBuilder};

    let collection = crate::qdrant_client::collection_name();
    let mut updated_points = Vec::new();

    for point in scroll_all_points(client, collection, true, false).await? {
        if !payload_needs_revectorization(&point.payload) {
            continue;
        }

        let Some(kind_num) = point.id.as_ref().and_then(|id| {
            if let Some(qdrant_client::qdrant::point_id::PointIdOptions::Num(n)) =
                id.point_id_options.as_ref()
            {
                Some(*n)
            } else {
                None
            }
        }) else {
            continue;
        };

        let Some(event_json) = point.payload.get("event").and_then(|v| v.as_str()) else {
            error!("Point {} has no event JSON, cannot re-vectorize", kind_num);
            continue;
        };

        let event = match Event::from_json(event_json) {
            Ok(event) => event,
            Err(e) => {
                error!("Point {} event JSON unparseable: {}", kind_num, e);
                continue;
            }
        };

        let vector = crate::similarity::EventFeatures::from_event(&event).to_vector();

        let mut payload = point.payload.clone();
        payload.insert(
            "feature_version".to_string(),
            qdrant_client::qdrant::Value::from(crate::similarity::FEATURE_VERSION),
        );

        updated_points.push(PointStruct::new(
            kind_num,
            vector,
            qdrant_client::Payload::from(payload),
        ));
    }

    let updated = updated_points.len();
    for chunk in updated_points.chunks(100) {
        client
            .upsert_points(UpsertPointsBuilder::new(collection, chunk.to_vec()).build())
            .await?;
    }

    Ok(updated)
}

/// Scroll every point in a collection, following pagination.
///
/// The plain `.limit(N)` scroll silently truncates past N points, which is
/// how clustering and cleanup previously missed kinds beyond the first 1000.
async fn scroll_all_points(
    client: &qdrant_client::Qdrant,
    collection: &str,
    with_payload: bool,
    with_vectors: bool,
) -> Result<Vec<qdrant_client::qdrant::RetrievedPoint>> {
    use qdrant_client::qdrant::ScrollPointsBuilder;

    let mut points = Vec::new();
    let mut offset: Option<qdrant_client::qdrant::PointId> = None;

    loop {
        let mut scroll_builder = ScrollPointsBuilder::new(collection)
            .with_payload(with_payload)
            .with_vectors(with_vectors)
            .limit(100);

        if let Some(offset_id) = offset {
            scroll_builder = scroll_builder.offset(offset_id);
        }

        let response = client.scroll(scroll_builder.build()).await?;
        let is_empty = response.result.is_empty();
        let next = response.next_page_offset.clone();

        points.extend(response.result);

        match next {
            Some(next_offset) if !is_empty => offset = Some(next_offset),
            _ => break,
        }
    }

    Ok(points)
}

async fn query_all_from_qdrant(client: &qdrant_client::Qdrant) -> Result<Vec<(Kind, KindEntry)>> {
    let collection = crate::qdrant_client::collection_name();
    let mut results = Vec::new();

    for point in scroll_all_points(client, collection, true, false).await? {
        let kind_num = point.id.as_ref().and_then(|id| {
            if let qdrant_client::qdrant::point_id::PointIdOptions::Num(n) =
                id.point_id_options.as_ref()?
            {
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
        myself.send_interval(Duration::from_secs(60 * 60 * 6), || {
            JsonActorMessage::CleanupDocumentedKinds
        });
        let myself_clone = myself.clone();
        let (http_actor, _) = Actor::spawn_linked(
            Some("HttpActor".to_string()),
            HttpActor,
            myself_clone.clone(),
            myself_clone.into(),
        )
        .await?;
        let recommended_apps = HashMap::new();

        // Initialize Qdrant client (required)
        let qdrant_client = crate::qdrant_client::initialize_qdrant()
            .await
            .map_err(|e| ActorProcessingErr::from(e.to_string()))?;

        info!("Qdrant client initialized successfully");

        // Migrate stats.json on first startup
        match crate::qdrant_client::migrate_from_stats_json(&qdrant_client, &STATS_FILE).await {
            Ok(true) => info!("Completed one-time migration from stats.json to Qdrant"),
            Ok(false) => info!("No migration needed"),
            Err(e) => error!("Migration failed (continuing anyway): {}", e),
        }

        // Bring stored vectors up to the current featurizer version
        match revectorize_outdated_points(&qdrant_client).await {
            Ok(0) => info!(
                "All points are at feature version {}",
                crate::similarity::FEATURE_VERSION
            ),
            Ok(n) => info!(
                "Re-vectorized {} points to feature version {}",
                n,
                crate::similarity::FEATURE_VERSION
            ),
            Err(e) => error!("Re-vectorization failed (continuing anyway): {}", e),
        }

        let state = State {
            http_actor,
            nostr_actor,
            recommended_apps,
            clustering_in_progress: false,
            qdrant_client,
        };

        // Trigger initial clustering on startup
        info!("Triggering initial clustering on startup");
        myself.send_message(JsonActorMessage::ComputeClusters)?;

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

                // Upsert to Qdrant
                let event_clone = event.clone();
                let client_clone = state.qdrant_client.clone();
                let collection = crate::qdrant_client::collection_name().to_string();
                let kind_id = u16::from(event.kind) as u64;
                let kind_u16 = u16::from(event.kind);

                // Upsert to Qdrant with incremental clustering (async to avoid blocking actor)
                tokio::spawn(async move {
                    use nostr_sdk::JsonUtil;
                    use qdrant_client::qdrant::{
                        PointId, PointStruct, SearchPointsBuilder, UpsertPointsBuilder,
                    };

                    // Fetch current point to get count
                    let point_id: PointId = kind_id.into();
                    let get_request =
                        qdrant_client::qdrant::GetPointsBuilder::new(&collection, vec![point_id])
                            .with_payload(true)
                            .with_vectors(true)
                            .build();

                    let current_point = client_clone
                        .get_points(get_request)
                        .await
                        .ok()
                        .and_then(|resp| resp.result.first().cloned());

                    let is_new_event = current_point.is_none();

                    // Increment count or start at 1
                    let count = if let Some(ref point) = current_point {
                        point
                            .payload
                            .get("count")
                            .and_then(|v| v.as_integer())
                            .unwrap_or(0) as u64
                            + 1
                    } else {
                        1
                    };

                    // Generate vector from event features, blended with the
                    // stored vector so one unusual event cannot teleport the
                    // kind across the feature space.
                    let features = crate::similarity::EventFeatures::from_event(&event_clone);
                    let fresh_vector = features.to_vector();

                    let stored_vector = current_point
                        .as_ref()
                        .filter(|p| !payload_needs_revectorization(&p.payload))
                        .and_then(|p| p.vectors.as_ref())
                        .and_then(|v| {
                            if let Some(
                                qdrant_client::qdrant::vectors_output::VectorsOptions::Vector(vec),
                            ) = &v.vectors_options
                            {
                                Some(vec.data.clone())
                            } else {
                                None
                            }
                        });

                    let vector = match stored_vector {
                        Some(old) if old.len() == fresh_vector.len() => {
                            crate::similarity::blend_vectors(&old, &fresh_vector, 0.8)
                        }
                        _ => fresh_vector,
                    };

                    // Create base payload
                    let mut payload_json = serde_json::json!({
                        "kind": u16::from(event_clone.kind),
                        "count": count,
                        "last_updated": Utc::now().timestamp_millis(),
                        "recommended_app": null,  // Will be updated separately
                        "event_id": event_clone.id.to_string(),
                        "event": event_clone.as_json(),
                        "feature_version": crate::similarity::FEATURE_VERSION,
                    });

                    // A plain count update must not wipe fields other flows maintain
                    // (cluster assignment, recommended app).
                    if let Some(ref point) = current_point {
                        if let Some(app) = point
                            .payload
                            .get("recommended_app")
                            .and_then(|v| v.as_str())
                        {
                            payload_json["recommended_app"] = serde_json::json!(app);
                        }
                        if let Some(cluster_id) =
                            point.payload.get("cluster_id").and_then(|v| v.as_integer())
                        {
                            payload_json["cluster_id"] = serde_json::json!(cluster_id);
                        }
                        if let Some(sim) = point
                            .payload
                            .get("cluster_similarity")
                            .and_then(|v| v.as_double())
                        {
                            payload_json["cluster_similarity"] = serde_json::json!(sim);
                        }
                    }

                    // For new events, search for similar events to assign cluster
                    if is_new_event {
                        let search_request =
                            SearchPointsBuilder::new(&collection, vector.clone(), 10)
                                .score_threshold(CLUSTER_SIMILARITY_THRESHOLD)
                                .with_payload(true)
                                .build();

                        if let Ok(response) = client_clone.search_points(search_request).await {
                            // Filter out self (shouldn't exist yet, but be safe)
                            let neighbors: Vec<_> = response.result
                                .iter()
                                .filter(|sp| {
                                    sp.id.as_ref().and_then(|id| {
                                        if let Some(qdrant_client::qdrant::point_id::PointIdOptions::Num(n)) = id.point_id_options.as_ref() {
                                            Some(*n as u16 != kind_u16)
                                        } else {
                                            None
                                        }
                                    }).unwrap_or(false)
                                })
                                .collect();

                            // If we found similar neighbors, assign to their cluster
                            if let Some(first_neighbor) = neighbors.first() {
                                let neighbor_kind = first_neighbor.id.as_ref().and_then(|id| {
                                    if let Some(
                                        qdrant_client::qdrant::point_id::PointIdOptions::Num(n),
                                    ) = id.point_id_options.as_ref()
                                    {
                                        Some(*n as i64)
                                    } else {
                                        None
                                    }
                                });

                                // Prefer the neighbor's existing cluster over its kind
                                // number so incremental assignment matches the batch
                                // clustering scheme (cluster id = lowest member kind).
                                let cluster_id = first_neighbor
                                    .payload
                                    .get("cluster_id")
                                    .and_then(|v| v.as_integer())
                                    .or(neighbor_kind);

                                if let Some(cluster_id) = cluster_id {
                                    let similarity = first_neighbor.score as f64;

                                    payload_json["cluster_id"] = serde_json::json!(cluster_id);
                                    payload_json["cluster_similarity"] =
                                        serde_json::json!(similarity);

                                    info!(
                                        "New event kind {} assigned to cluster {}",
                                        kind_u16, cluster_id
                                    );
                                }
                            }
                        }
                    }

                    let payload = qdrant_client::Payload::from(
                        payload_json.as_object().cloned().unwrap_or_default(),
                    );

                    // Upsert to Qdrant
                    let point = PointStruct::new(kind_id, vector, payload);
                    if let Err(e) = client_clone
                        .upsert_points(UpsertPointsBuilder::new(collection, vec![point]).build())
                        .await
                    {
                        error!("Failed to upsert to Qdrant: {}", e);
                    }
                });

                if let Err(e) = state.maybe_refresh_recommended_app(event.kind) {
                    error!("Failed to refresh recommended app: {}", e);
                }
            }
            JsonActorMessage::RecordRecommendedApp(event, _url) => {
                match get_recommended_app_string::parse_recommended_app(&event) {
                    Ok((kinds, recommended_app)) => {
                        if kinds.is_empty() {
                            debug!("Recommended app event {} lists no kinds", event.id);
                        } else {
                            let client_clone = state.qdrant_client.clone();
                            tokio::spawn(async move {
                                match crate::qdrant_client::set_recommended_app(
                                    &client_clone,
                                    &kinds,
                                    &recommended_app,
                                )
                                .await
                                {
                                    Ok(()) => info!(
                                        "Recorded recommended app '{}' for {} kinds",
                                        recommended_app,
                                        kinds.len()
                                    ),
                                    Err(e) => {
                                        error!("Failed to persist recommended app: {}", e)
                                    }
                                }
                            });
                        }
                    }
                    Err(e) => error!("Failed to parse recommended app: {}", e),
                }
            }
            JsonActorMessage::GetStatsVec(_arg, reply) => {
                if !reply.is_closed() {
                    // Query Qdrant (required)
                    let data_as_sorted_vec = match query_all_from_qdrant(&state.qdrant_client).await
                    {
                        Ok(vec) => {
                            info!("Sending {} events from Qdrant", vec.len());
                            vec
                        }
                        Err(e) => {
                            error!("Failed to query Qdrant: {}", e);
                            Vec::new() // Return empty on error
                        }
                    };

                    if let Err(e) = reply.send(data_as_sorted_vec) {
                        error!("Error when sending sorted vec: {}", e);
                    }
                }
            }
            JsonActorMessage::CleanupDocumentedKinds => {
                // Query Qdrant to find kinds to remove
                let client_clone = state.qdrant_client.clone();
                let collection = crate::qdrant_client::collection_name().to_string();
                let myself_clone = _myself.clone();

                tokio::spawn(async move {
                    use qdrant_client::qdrant::DeletePointsBuilder;

                    // Scroll all points and check which to remove
                    match scroll_all_points(&client_clone, &collection, true, false).await {
                        Ok(response_points) => {
                            let mut ids_to_remove = Vec::new();

                            for point in response_points {
                                if let Some(kind_value) = point.payload.get("kind") {
                                    if let Some(kind_num) = kind_value.as_integer() {
                                        let kind = Kind::from(kind_num as u16);

                                        // Check if documented or old
                                        let is_documented = !is_kind_free(kind);
                                        let is_too_old = point
                                            .payload
                                            .get("last_updated")
                                            .and_then(|v| v.as_integer())
                                            .map(is_old)
                                            .unwrap_or(false);

                                        if is_documented || is_too_old {
                                            if let Some(id) = point.id {
                                                ids_to_remove.push(id);
                                            }
                                        }
                                    }
                                }
                            }

                            if !ids_to_remove.is_empty() {
                                let count_to_remove = ids_to_remove.len();
                                info!("Cleaning up {} kinds from Qdrant", count_to_remove);

                                // Convert PointIds to numeric IDs, then back to PointId type
                                use qdrant_client::qdrant::PointId;
                                let numeric_ids: Vec<u64> = ids_to_remove
                                    .into_iter()
                                    .filter_map(|id| match id.point_id_options {
                                        Some(
                                            qdrant_client::qdrant::point_id::PointIdOptions::Num(n),
                                        ) => Some(n),
                                        _ => None,
                                    })
                                    .collect();

                                let point_ids: Vec<PointId> =
                                    numeric_ids.into_iter().map(Into::into).collect();

                                let delete_request = DeletePointsBuilder::new(collection)
                                    .points(point_ids)
                                    .build();

                                if let Err(e) = client_clone.delete_points(delete_request).await {
                                    error!("Failed to delete old/documented points: {}", e);
                                } else {
                                    info!(
                                        "Successfully cleaned up {} old/documented kinds",
                                        count_to_remove
                                    );

                                    // Trigger re-clustering after cleanup since cluster membership may have changed
                                    if let Err(e) =
                                        cast!(myself_clone, JsonActorMessage::ComputeClusters)
                                    {
                                        error!(
                                            "Failed to trigger re-clustering after cleanup: {}",
                                            e
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => error!("Failed to query Qdrant for cleanup: {}", e),
                    }
                });
            }
            JsonActorMessage::ComputeClusters => {
                // Debounce: skip if clustering is already running
                if state.clustering_in_progress {
                    debug!("Skipping clustering - already in progress");
                    return Ok(());
                }

                state.clustering_in_progress = true;

                let client_clone = state.qdrant_client.clone();
                let myself_clone = _myself.clone();
                let collection = crate::qdrant_client::collection_name().to_string();

                // Spawn background task for clustering using Qdrant similarity search
                tokio::spawn(async move {
                    use qdrant_client::qdrant::SearchPointsBuilder;
                    use std::collections::{HashMap, HashSet};

                    info!("Computing similarity clusters using Qdrant search...");

                    // First, get all points WITH vectors
                    let all_points =
                        match scroll_all_points(&client_clone, &collection, false, true).await {
                            Ok(points) => points,
                            Err(e) => {
                                error!("Failed to scroll Qdrant points: {}", e);
                                return;
                            }
                        };

                    info!("Found {} points to cluster", all_points.len());

                    let mut clusters: HashMap<u16, (u16, f64)> = HashMap::new();
                    let mut assigned_to_cluster: HashSet<u16> = HashSet::new();
                    let mut best_scores: Vec<f32> = Vec::new();

                    // For each point, search for similar neighbors using Qdrant
                    for point in &all_points {
                        let kind_id = if let Some(id) = point.id.as_ref() {
                            if let Some(qdrant_client::qdrant::point_id::PointIdOptions::Num(n)) =
                                id.point_id_options.as_ref()
                            {
                                *n as u16
                            } else {
                                continue;
                            }
                        } else {
                            continue;
                        };

                        // Skip if already assigned to a cluster
                        if assigned_to_cluster.contains(&kind_id) {
                            continue;
                        }

                        // Get the vector for this point
                        let vector = if let Some(vectors) = &point.vectors {
                            if let Some(
                                qdrant_client::qdrant::vectors_output::VectorsOptions::Vector(vec),
                            ) = &vectors.vectors_options
                            {
                                vec.data.clone()
                            } else {
                                continue;
                            }
                        } else {
                            continue;
                        };

                        // Search without a server-side threshold so the full
                        // score distribution is observable; threshold below.
                        let search_request = SearchPointsBuilder::new(&collection, vector, 10)
                            .with_payload(false)
                            .build();

                        let similar_points = match client_clone.search_points(search_request).await
                        {
                            Ok(response) => response.result,
                            Err(e) => {
                                error!("Failed to search for kind {}: {}", kind_id, e);
                                continue;
                            }
                        };

                        // Filter out self (the search includes the query point itself)
                        let neighbors: Vec<_> = similar_points
                            .iter()
                            .filter(|sp| {
                                sp.id
                                    .as_ref()
                                    .and_then(|id| {
                                        if let Some(
                                            qdrant_client::qdrant::point_id::PointIdOptions::Num(n),
                                        ) = id.point_id_options.as_ref()
                                        {
                                            Some(*n as u16 != kind_id)
                                        } else {
                                            None
                                        }
                                    })
                                    .unwrap_or(false)
                            })
                            .collect();

                        if let Some(top) = neighbors.first() {
                            best_scores.push(top.score);
                        }

                        let neighbors: Vec<_> = neighbors
                            .into_iter()
                            .filter(|sp| sp.score >= CLUSTER_SIMILARITY_THRESHOLD)
                            .collect();

                        // If we found at least 1 neighbor (excluding self), create a cluster
                        if !neighbors.is_empty() {
                            // Build cluster from ALL similar points (including self)
                            let mut cluster_members: Vec<u16> = vec![kind_id]; // Start with self

                            // Add all neighbors
                            for sp in &neighbors {
                                if let Some(id) = sp.id.as_ref() {
                                    if let Some(
                                        qdrant_client::qdrant::point_id::PointIdOptions::Num(n),
                                    ) = id.point_id_options.as_ref()
                                    {
                                        cluster_members.push(*n as u16);
                                    }
                                }
                            }

                            cluster_members.sort();

                            // Use the lowest kind number as cluster ID
                            let cluster_id = *cluster_members.first().unwrap();

                            // Assign all members to this cluster
                            for &member_kind in &cluster_members {
                                // Find similarity score for this member
                                let similarity = if member_kind == kind_id {
                                    1.0 // Self always has perfect similarity
                                } else {
                                    neighbors.iter()
                                        .find(|sp| {
                                            sp.id.as_ref().and_then(|id| {
                                                if let Some(qdrant_client::qdrant::point_id::PointIdOptions::Num(n)) = id.point_id_options.as_ref() {
                                                    Some(*n as u16 == member_kind)
                                                } else {
                                                    None
                                                }
                                            }).unwrap_or(false)
                                        })
                                        .map(|sp| sp.score as f64)
                                        .unwrap_or(CLUSTER_SIMILARITY_THRESHOLD as f64)
                                };

                                clusters.insert(member_kind, (cluster_id, similarity));
                                assigned_to_cluster.insert(member_kind);
                            }

                            info!(
                                "Created cluster {} with {} members",
                                cluster_id,
                                cluster_members.len()
                            );
                        }
                    }

                    // Best-neighbor score distribution; use this to recalibrate
                    // CLUSTER_SIMILARITY_THRESHOLD when the featurizer changes.
                    let mut histogram = [0usize; 6];
                    for score in &best_scores {
                        let bucket = match *score {
                            s if s >= 0.95 => 5,
                            s if s >= 0.9 => 4,
                            s if s >= 0.8 => 3,
                            s if s >= 0.7 => 2,
                            s if s >= 0.5 => 1,
                            _ => 0,
                        };
                        histogram[bucket] += 1;
                    }
                    info!(
                        "Best-neighbor score histogram (<0.5, 0.5-0.7, 0.7-0.8, 0.8-0.9, 0.9-0.95, >=0.95): {:?}",
                        histogram
                    );

                    // Count cluster sizes to remove single-member clusters
                    let mut cluster_sizes: HashMap<u16, usize> = HashMap::new();
                    for (cluster_id, _) in clusters.values() {
                        *cluster_sizes.entry(*cluster_id).or_insert(0) += 1;
                    }

                    // Filter out single-member clusters
                    let filtered_clusters: HashMap<u16, (u16, f64)> = clusters
                        .into_iter()
                        .filter(|(_, (cluster_id, _))| {
                            cluster_sizes
                                .get(cluster_id)
                                .map(|&size| size > 1)
                                .unwrap_or(false)
                        })
                        .collect();

                    let cluster_count = filtered_clusters.len();
                    let unique_clusters = filtered_clusters
                        .values()
                        .map(|(cluster_id, _)| cluster_id)
                        .collect::<HashSet<_>>()
                        .len();

                    info!("Qdrant-based clustering completed: {} clustered kinds in {} unique clusters (filtered out single-member clusters)", cluster_count, unique_clusters);

                    // Convert to the format ApplyClusters expects
                    let clusters_with_kind: HashMap<Kind, (u32, f64)> = filtered_clusters
                        .into_iter()
                        .map(|(kind, (cluster_id, similarity))| {
                            (Kind::from(kind), (cluster_id as u32, similarity))
                        })
                        .collect();

                    // Send results back to actor
                    if let Err(e) = cast!(
                        myself_clone,
                        JsonActorMessage::ApplyClusters(clusters_with_kind)
                    ) {
                        error!("Failed to send cluster results to actor: {}", e);
                    }
                });
            }
            JsonActorMessage::ApplyClusters(clusters) => {
                info!("Applying cluster results to {} kinds", clusters.len());

                // Update Qdrant with cluster information
                let client_clone = state.qdrant_client.clone();
                let collection = crate::qdrant_client::collection_name().to_string();
                let clusters_clone = clusters.clone();

                tokio::spawn(async move {
                    use qdrant_client::qdrant::{
                        GetPointsBuilder, PointId, PointStruct, UpsertPointsBuilder,
                    };

                    for (kind, (cluster_id, similarity)) in clusters_clone {
                        let kind_id: u64 = u16::from(kind) as u64;
                        let point_id: PointId = kind_id.into();

                        // Fetch current point to preserve existing data
                        let get_request =
                            GetPointsBuilder::new(&collection, vec![point_id.clone()])
                                .with_payload(true)
                                .with_vectors(true)
                                .build();

                        match client_clone.get_points(get_request).await {
                            Ok(response) if !response.result.is_empty() => {
                                let current_point = &response.result[0];

                                // Merge cluster info into existing payload
                                let mut payload = current_point.payload.clone();
                                payload
                                    .insert("cluster_id".to_string(), (cluster_id as i64).into());
                                payload.insert("cluster_similarity".to_string(), similarity.into());

                                // Get vector (should exist since we set it during RecordEvent)
                                let vector = current_point.vectors.as_ref()
                                        .and_then(|v| match v.vectors_options.as_ref()? {
                                            qdrant_client::qdrant::vectors_output::VectorsOptions::Vector(vec) => Some(vec.data.clone()),
                                            _ => None,
                                        });

                                if let Some(vector) = vector {
                                    // Re-upsert with updated payload
                                    let point = PointStruct::new(kind_id, vector, payload);
                                    let upsert =
                                        UpsertPointsBuilder::new(&collection, vec![point]).build();

                                    if let Err(e) = client_clone.upsert_points(upsert).await {
                                        error!(
                                            "Failed to update cluster info for kind {}: {}",
                                            kind, e
                                        );
                                    }
                                }
                            }
                            Ok(_) => {
                                debug!("Point {} not found, skipping cluster update", kind_id);
                            }
                            Err(e) => {
                                error!(
                                    "Failed to fetch point {} for cluster update: {}",
                                    kind_id, e
                                );
                            }
                        }
                    }

                    info!("Successfully updated cluster information in Qdrant");
                });

                // Mark clustering as complete
                state.clustering_in_progress = false;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qdrant_client::qdrant::Value;

    #[test]
    fn test_payload_without_feature_version_needs_revectorization() {
        let payload: HashMap<String, Value> = HashMap::new();
        assert!(payload_needs_revectorization(&payload));
    }

    #[test]
    fn test_payload_with_old_feature_version_needs_revectorization() {
        let mut payload: HashMap<String, Value> = HashMap::new();
        payload.insert("feature_version".to_string(), Value::from(0i64));
        assert!(payload_needs_revectorization(&payload));
    }

    #[test]
    fn test_payload_with_current_feature_version_is_kept() {
        let mut payload: HashMap<String, Value> = HashMap::new();
        payload.insert(
            "feature_version".to_string(),
            Value::from(crate::similarity::FEATURE_VERSION),
        );
        assert!(!payload_needs_revectorization(&payload));
    }
}
