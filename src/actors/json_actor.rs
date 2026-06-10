mod get_recommended_app_string;
use super::nostr_actor::NostrActorMessage;
use crate::actors::http_actor::{HttpActor, HttpActorMessage};
use crate::utils::is_kind_free;
use crate::utils::is_old;
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
    /// The clustering run ended — payloads written or the run aborted.
    /// Releases the schedule and replays a coalesced request.
    ClusteringFinished,
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
    clustering: ClusteringSchedule,
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

/// Debounce-with-coalescing for clustering runs.
///
/// A recompute requested while one is running is remembered and replayed
/// when the run finishes instead of being dropped. The old plain debounce
/// silently lost the post-cleanup recompute whenever the 3h timer fired
/// together with the 6h cleanup tick (3h divides 6h, and both intervals
/// start at boot). The run is "finished" only after its payload writes
/// complete, so a replayed run never overlaps an in-flight apply.
#[derive(Debug, Default)]
struct ClusteringSchedule {
    in_progress: bool,
    pending: bool,
}

impl ClusteringSchedule {
    /// A recompute was requested; true when the caller must start one now.
    fn request(&mut self) -> bool {
        if self.in_progress {
            self.pending = true;
            false
        } else {
            self.in_progress = true;
            true
        }
    }

    /// The running recompute finished (applied or failed); true when a
    /// coalesced request must be replayed.
    fn finished(&mut self) -> bool {
        self.in_progress = false;
        std::mem::take(&mut self.pending)
    }
}

/// Cosine score two kinds must reach to be clustered together. Recalibrate
/// against the logged best-neighbor histogram whenever the featurizer changes.
///
/// 0.9 sits in the valley of the featurizer-v4 distribution over the real
/// 658-kind dataset: of 227 seeds, only 4 scored in [0.9, 0.95) while 111
/// scored >= 0.95 and 112 below 0.9, so it separates same-codebase
/// near-duplicates from coincidental similarity (at 0.8 about 92% of all
/// kinds ended up clustered, which made the badge meaningless). v2 and v3
/// showed the same valley.
const CLUSTER_SIMILARITY_THRESHOLD: f32 = 0.9;

/// Outcome of fetching a kind's stored point before an update.
enum StoredPoint {
    /// The point exists; the update must preserve its history.
    Existing(Box<qdrant_client::qdrant::RetrievedPoint>),
    /// Confirmed absent: the count=1 new-point path is safe.
    New,
    /// Fetch failed: skip the update rather than risk resetting count and
    /// EMA history by treating an unreadable point as new.
    Unavailable(String),
}

fn classify_stored_point<E: std::fmt::Display>(
    result: Result<Vec<qdrant_client::qdrant::RetrievedPoint>, E>,
) -> StoredPoint {
    match result {
        Ok(points) => match points.into_iter().next() {
            Some(point) => StoredPoint::Existing(Box::new(point)),
            None => StoredPoint::New,
        },
        Err(e) => StoredPoint::Unavailable(e.to_string()),
    }
}

/// Cluster id for a new point joining via the incremental path: the
/// neighbor's cluster if it already has one, otherwise derived from the two
/// founding members. Batch clustering uses the lowest member kind as the
/// cluster id, and the periodic recompute assumes the same contract.
fn incremental_cluster_id(
    neighbor_cluster_id: Option<i64>,
    neighbor_kind: Option<i64>,
    new_kind: u16,
) -> Option<i64> {
    neighbor_cluster_id.or_else(|| neighbor_kind.map(|nk| nk.min(i64::from(new_kind))))
}

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

/// Remove the legacy "None found" sentinel from stored payloads.
///
/// Earlier releases persisted the literal string "None found" as a real
/// recommended_app value; it leaked into the stats page and shadowed
/// later lookups. parse_recommended_app no longer produces it, so this
/// scrub converges and becomes a no-op.
async fn cleanup_recommended_app_sentinel(client: &qdrant_client::Qdrant) -> Result<usize> {
    use qdrant_client::qdrant::{DeletePayloadPointsBuilder, PointsIdsList};

    let collection = crate::qdrant_client::collection_name();
    let mut ids = Vec::new();
    for point in scroll_all_points(client, collection, true, false).await? {
        if point
            .payload
            .get("recommended_app")
            .and_then(|v| v.as_str())
            .map(String::as_str)
            == Some("None found")
        {
            if let Some(id) = point.id {
                ids.push(id);
            }
        }
    }

    if ids.is_empty() {
        return Ok(0);
    }

    let count = ids.len();
    client
        .delete_payload(
            DeletePayloadPointsBuilder::new(collection, vec!["recommended_app".to_string()])
                .points_selector(PointsIdsList { ids })
                .build(),
        )
        .await?;
    Ok(count)
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

/// Persist cluster assignments and clear stale ones.
///
/// New assignments merge via `set_payload` so vectors and unrelated payload
/// keys stay untouched. Points that carry an assignment from a previous run
/// but are absent from `clusters` get their cluster fields deleted —
/// otherwise a kind that falls out of a cluster keeps a stale badge forever.
async fn apply_cluster_assignments(
    client: &qdrant_client::Qdrant,
    clusters: &HashMap<Kind, (u32, f64)>,
) -> Result<()> {
    use anyhow::Context;
    use qdrant_client::qdrant::{
        DeletePayloadPointsBuilder, PointId, PointsIdsList, SetPayloadPointsBuilder,
    };
    use std::collections::HashSet;

    let collection = crate::qdrant_client::collection_name();

    for (kind, (cluster_id, similarity)) in clusters {
        let payload = qdrant_client::Payload::from(
            serde_json::json!({
                "cluster_id": cluster_id,
                "cluster_similarity": similarity,
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        );

        client
            .set_payload(
                SetPayloadPointsBuilder::new(collection, payload)
                    .points_selector(PointsIdsList {
                        ids: vec![PointId::from(u16::from(*kind) as u64)],
                    })
                    .build(),
            )
            .await
            .with_context(|| format!("Failed to set cluster payload for kind {}", kind))?;
    }

    let clustered: HashSet<u64> = clusters
        .keys()
        .map(|kind| u16::from(*kind) as u64)
        .collect();

    let mut stale_ids: Vec<PointId> = Vec::new();
    for point in scroll_all_points(client, collection, true, false).await? {
        let Some(num) = point.id.as_ref().and_then(|id| {
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

        if clustered.contains(&num) {
            continue;
        }

        if point.payload.contains_key("cluster_id")
            || point.payload.contains_key("cluster_similarity")
        {
            stale_ids.push(PointId::from(num));
        }
    }

    if !stale_ids.is_empty() {
        info!(
            "Clearing stale cluster assignment from {} points",
            stale_ids.len()
        );
        client
            .delete_payload(
                DeletePayloadPointsBuilder::new(
                    collection,
                    vec!["cluster_id".to_string(), "cluster_similarity".to_string()],
                )
                .points_selector(PointsIdsList { ids: stale_ids })
                .build(),
            )
            .await
            .context("Failed to clear stale cluster payloads")?;
    }

    Ok(())
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
        // Periodic recompute re-derives assignments from current vectors as
        // EMA updates drift them, overwriting or clearing stale incremental
        // assignments. This timer aligns with the cleanup tick every 6h;
        // that is safe because ClusteringSchedule coalesces requests that
        // land mid-run and replays them when the run (including its payload
        // writes) finishes — nothing is dropped.
        myself.send_interval(Duration::from_secs(60 * 60 * 3), || {
            JsonActorMessage::ComputeClusters
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

        // Migrate stats.json on first startup. A failure here must fail the
        // boot: once anything lands in the collection, the point-count guard
        // skips migration forever, so the only safe retry window is while
        // the collection is still empty. Parse failures are handled inside
        // (logged and skipped — they leave no partial state).
        match crate::qdrant_client::migrate_from_stats_json(&qdrant_client, &STATS_FILE).await {
            Ok(true) => info!("Completed one-time migration from stats.json to Qdrant"),
            Ok(false) => info!("No migration needed"),
            Err(e) => {
                return Err(ActorProcessingErr::from(format!(
                    "Migration from stats.json failed: {e}"
                )))
            }
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

        // Scrub the legacy "None found" sentinel; reruns harmlessly.
        match cleanup_recommended_app_sentinel(&qdrant_client).await {
            Ok(0) => {}
            Ok(n) => info!("Removed 'None found' sentinel from {} points", n),
            Err(e) => error!("Sentinel cleanup failed (continuing anyway): {}", e),
        }

        let state = State {
            http_actor,
            nostr_actor,
            recommended_apps,
            clustering: ClusteringSchedule::default(),
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

                    let fetched = client_clone
                        .get_points(get_request)
                        .await
                        .map(|resp| resp.result);

                    let current_point = match classify_stored_point(fetched) {
                        StoredPoint::Existing(point) => Some(*point),
                        StoredPoint::New => None,
                        StoredPoint::Unavailable(e) => {
                            error!(
                                "Failed to fetch stored point for kind {}, skipping this event: {}",
                                kind_u16, e
                            );
                            return;
                        }
                    };

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
                                let cluster_id = incremental_cluster_id(
                                    first_neighbor
                                        .payload
                                        .get("cluster_id")
                                        .and_then(|v| v.as_integer()),
                                    neighbor_kind,
                                    kind_u16,
                                );

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
                let (kinds, recommended_app) =
                    get_recommended_app_string::parse_recommended_app(&event);
                match recommended_app {
                    Some(recommended_app) if !kinds.is_empty() => {
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
                    Some(_) => debug!("Recommended app event {} lists no kinds", event.id),
                    // No usable name: persist nothing rather than a sentinel.
                    None => debug!("Recommended app event {} has no usable name", event.id),
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
                if !state.clustering.request() {
                    debug!("Clustering already in progress; queued a follow-up recompute");
                    return Ok(());
                }

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
                                // Release the schedule; a bare return here
                                // would disable re-clustering until restart.
                                if let Err(e) =
                                    cast!(myself_clone, JsonActorMessage::ClusteringFinished)
                                {
                                    error!("Failed to report clustering failure: {}", e);
                                }
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

                // Update Qdrant with cluster information. The schedule stays
                // claimed until the writes are done, so a replayed recompute
                // can never race an in-flight apply.
                let client_clone = state.qdrant_client.clone();
                let myself_clone = _myself.clone();

                tokio::spawn(async move {
                    match apply_cluster_assignments(&client_clone, &clusters).await {
                        Ok(()) => info!("Successfully updated cluster information in Qdrant"),
                        Err(e) => error!("Failed to apply cluster assignments: {}", e),
                    }
                    if let Err(e) = cast!(myself_clone, JsonActorMessage::ClusteringFinished) {
                        error!("Failed to report clustering completion: {}", e);
                    }
                });
            }
            JsonActorMessage::ClusteringFinished => {
                if state.clustering.finished() {
                    info!("Replaying recompute request coalesced during the last run");
                    _myself.send_message(JsonActorMessage::ComputeClusters)?;
                }
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
    fn test_classify_stored_point_distinguishes_error_from_absent() {
        let existing = classify_stored_point::<String>(Ok(vec![
            qdrant_client::qdrant::RetrievedPoint::default(),
        ]));
        assert!(matches!(existing, StoredPoint::Existing(_)));

        let absent = classify_stored_point::<String>(Ok(vec![]));
        assert!(matches!(absent, StoredPoint::New));

        let failed = classify_stored_point(Err("connection refused".to_string()));
        assert!(
            matches!(failed, StoredPoint::Unavailable(_)),
            "a fetch error must not be treated as a confirmed-new point: \
             the count=1 path would overwrite the stored count and EMA vector"
        );
    }

    #[test]
    fn test_clustering_schedule_starts_when_idle() {
        let mut schedule = ClusteringSchedule::default();
        assert!(schedule.request(), "idle schedule starts a run");
        assert!(
            !schedule.finished(),
            "nothing was queued during the run, so no replay"
        );
        assert!(schedule.request(), "idle again after the run finished");
    }

    #[test]
    fn test_clustering_schedule_coalesces_requests_made_mid_run() {
        let mut schedule = ClusteringSchedule::default();
        assert!(schedule.request());
        assert!(
            !schedule.request(),
            "a request landing mid-run must not start a parallel run"
        );
        assert!(!schedule.request(), "many mid-run requests coalesce");
        assert!(
            schedule.finished(),
            "the coalesced request replays when the run finishes \
             (this is the post-cleanup recompute that the old debounce dropped)"
        );
        assert!(
            schedule.request(),
            "the replayed request claims the schedule"
        );
        assert!(!schedule.finished(), "the queue drained");
    }

    #[test]
    fn test_incremental_cluster_id_prefers_neighbor_cluster() {
        assert_eq!(incremental_cluster_id(Some(7), Some(100), 200), Some(7));
    }

    #[test]
    fn test_incremental_cluster_id_uses_lowest_member_kind_as_fallback() {
        assert_eq!(
            incremental_cluster_id(None, Some(100), 200),
            Some(100),
            "neighbor kind lower than new kind"
        );
        assert_eq!(
            incremental_cluster_id(None, Some(300), 200),
            Some(200),
            "new kind lower than neighbor kind must become the cluster id"
        );
        assert_eq!(incremental_cluster_id(None, None, 200), None);
    }

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

    #[tokio::test]
    #[ignore] // Only run when Qdrant is available locally
    async fn test_cleanup_recommended_app_sentinel_removes_only_sentinel_values() {
        use nostr_sdk::JsonUtil;
        use qdrant_client::qdrant::{
            DeletePointsBuilder, GetPointsBuilder, PointId, PointStruct, UpsertPointsBuilder,
        };

        let client = crate::qdrant_client::initialize_qdrant().await.unwrap();
        let collection = crate::qdrant_client::collection_name();

        let keys = Keys::generate();
        let unsigned = EventBuilder::new(Kind::from(64901u16), "x").build(keys.public_key());
        let event = keys.sign_event(unsigned).await.unwrap();

        let seed = |kind: u64, app: &str| {
            let payload = qdrant_client::Payload::from(
                serde_json::json!({
                    "kind": kind,
                    "count": 2,
                    "last_updated": 1,
                    "recommended_app": app,
                    "event": event.as_json(),
                    "feature_version": crate::similarity::FEATURE_VERSION,
                })
                .as_object()
                .cloned()
                .unwrap(),
            );
            let mut vector = vec![0.0f32; crate::qdrant_client::vector_size()];
            vector[0] = 1.0;
            PointStruct::new(kind, vector, payload)
        };

        client
            .upsert_points(
                UpsertPointsBuilder::new(
                    collection,
                    vec![seed(64901, "None found"), seed(64902, "RealApp")],
                )
                .build(),
            )
            .await
            .unwrap();

        let removed = cleanup_recommended_app_sentinel(&client).await.unwrap();
        assert!(removed >= 1, "the sentinel point must be scrubbed");

        let got = client
            .get_points(
                GetPointsBuilder::new(
                    collection,
                    vec![PointId::from(64901u64), PointId::from(64902u64)],
                )
                .with_payload(true)
                .build(),
            )
            .await
            .unwrap();

        for point in &got.result {
            let kind = point.payload.get("kind").and_then(|v| v.as_integer());
            let app = point
                .payload
                .get("recommended_app")
                .and_then(|v| v.as_str())
                .map(String::from);
            match kind {
                Some(64901) => {
                    assert_eq!(app, None, "sentinel value must be deleted");
                    assert_eq!(
                        point.payload.get("count").and_then(|v| v.as_integer()),
                        Some(2),
                        "other payload keys must survive the scrub"
                    );
                }
                Some(64902) => assert_eq!(app.as_deref(), Some("RealApp")),
                other => panic!("unexpected point in result: {other:?}"),
            }
        }

        client
            .delete_points(
                DeletePointsBuilder::new(collection)
                    .points(vec![PointId::from(64901u64), PointId::from(64902u64)])
                    .build(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore] // Only run when Qdrant is available locally
    async fn test_revectorize_outdated_points_upgrades_old_versions() {
        use nostr_sdk::JsonUtil;
        use qdrant_client::qdrant::{
            DeletePointsBuilder, GetPointsBuilder, PointId, PointStruct, UpsertPointsBuilder,
        };

        let client = crate::qdrant_client::initialize_qdrant().await.unwrap();
        let collection = crate::qdrant_client::collection_name();

        let keys = Keys::generate();
        let unsigned =
            EventBuilder::new(Kind::from(64903u16), "hello world").build(keys.public_key());
        let event = keys.sign_event(unsigned).await.unwrap();

        // A point written by an older featurizer: stale vector, old version,
        // plus unrelated fields that the rebuild must preserve.
        let payload = qdrant_client::Payload::from(
            serde_json::json!({
                "kind": 64903,
                "count": 3,
                "last_updated": 1,
                "event_id": event.id.to_string(),
                "event": event.as_json(),
                "feature_version": 1,
                "cluster_id": 7,
            })
            .as_object()
            .cloned()
            .unwrap(),
        );
        let mut stale_vector = vec![0.0f32; crate::qdrant_client::vector_size()];
        stale_vector[1] = 1.0;
        client
            .upsert_points(
                UpsertPointsBuilder::new(
                    collection,
                    vec![PointStruct::new(64903u64, stale_vector.clone(), payload)],
                )
                .build(),
            )
            .await
            .unwrap();

        let updated = revectorize_outdated_points(&client).await.unwrap();
        assert!(updated >= 1, "the seeded point must be re-vectorized");

        let got = client
            .get_points(
                GetPointsBuilder::new(collection, vec![PointId::from(64903u64)])
                    .with_payload(true)
                    .with_vectors(true)
                    .build(),
            )
            .await
            .unwrap();
        let point = got.result.first().expect("seeded point exists");

        assert_eq!(
            point
                .payload
                .get("feature_version")
                .and_then(|v| v.as_integer()),
            Some(crate::similarity::FEATURE_VERSION)
        );
        assert_eq!(
            point.payload.get("count").and_then(|v| v.as_integer()),
            Some(3),
            "rebuild must preserve unrelated payload fields"
        );
        assert_eq!(
            point.payload.get("cluster_id").and_then(|v| v.as_integer()),
            Some(7)
        );

        let expected = crate::similarity::EventFeatures::from_event(&event).to_vector();
        let stored = point
            .vectors
            .as_ref()
            .and_then(|v| match v.vectors_options.as_ref() {
                Some(qdrant_client::qdrant::vectors_output::VectorsOptions::Vector(vec)) => {
                    Some(vec.data.clone())
                }
                _ => None,
            })
            .expect("vector present");
        assert_ne!(stored, stale_vector, "stale vector must be replaced");
        assert_eq!(stored.len(), expected.len());
        for (s, e) in stored.iter().zip(expected.iter()) {
            assert!((s - e).abs() < 1e-6, "vector rebuilt from event JSON");
        }

        client
            .delete_points(
                DeletePointsBuilder::new(collection)
                    .points(vec![PointId::from(64903u64)])
                    .build(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore] // Only run when Qdrant is available locally
    async fn test_apply_cluster_assignments_sets_new_and_clears_stale() {
        use qdrant_client::qdrant::{
            DeletePointsBuilder, GetPointsBuilder, PointId, PointStruct, UpsertPointsBuilder,
        };

        let client = crate::qdrant_client::initialize_qdrant().await.unwrap();
        let collection = crate::qdrant_client::collection_name();

        // Two points carrying a previous run's assignment; only one stays
        // clustered after the recompute.
        let mut points = Vec::new();
        for kind in [64901u64, 64902u64] {
            let payload = qdrant_client::Payload::from(
                serde_json::json!({
                    "kind": kind,
                    "count": 5,
                    "cluster_id": 999,
                    "cluster_similarity": 0.5,
                })
                .as_object()
                .cloned()
                .unwrap(),
            );
            let mut vector = vec![0.0f32; crate::qdrant_client::vector_size()];
            vector[0] = 1.0;
            points.push(PointStruct::new(kind, vector, payload));
        }
        client
            .upsert_points(UpsertPointsBuilder::new(collection, points).build())
            .await
            .unwrap();

        let mut clusters: HashMap<Kind, (u32, f64)> = HashMap::new();
        clusters.insert(Kind::from(64901u16), (64901u32, 0.93f64));

        apply_cluster_assignments(&client, &clusters).await.unwrap();

        let got = client
            .get_points(
                GetPointsBuilder::new(
                    collection,
                    vec![PointId::from(64901u64), PointId::from(64902u64)],
                )
                .with_payload(true)
                .build(),
            )
            .await
            .unwrap();

        let by_id: HashMap<u64, _> = got
            .result
            .into_iter()
            .filter_map(|p| {
                let num = p.id.as_ref().and_then(|id| {
                    if let Some(qdrant_client::qdrant::point_id::PointIdOptions::Num(n)) =
                        id.point_id_options.as_ref()
                    {
                        Some(*n)
                    } else {
                        None
                    }
                })?;
                Some((num, p.payload))
            })
            .collect();

        let kept = &by_id[&64901];
        assert_eq!(
            kept.get("cluster_id").and_then(|v| v.as_integer()),
            Some(64901)
        );
        let similarity = kept
            .get("cluster_similarity")
            .and_then(|v| v.as_double())
            .unwrap();
        assert!((similarity - 0.93).abs() < 1e-9);
        assert_eq!(
            kept.get("count").and_then(|v| v.as_integer()),
            Some(5),
            "merge must not clobber unrelated keys"
        );

        let cleared = &by_id[&64902];
        assert!(
            !cleared.contains_key("cluster_id"),
            "stale cluster_id must be cleared"
        );
        assert!(!cleared.contains_key("cluster_similarity"));
        assert_eq!(
            cleared.get("count").and_then(|v| v.as_integer()),
            Some(5),
            "clearing must not touch unrelated keys"
        );

        client
            .delete_points(
                DeletePointsBuilder::new(collection)
                    .points(vec![PointId::from(64901u64), PointId::from(64902u64)])
                    .build(),
            )
            .await
            .unwrap();
    }
}
