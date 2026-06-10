use anyhow::{Context, Result};
use qdrant_client::qdrant::{
    vectors_config::Config, CreateCollectionBuilder, Distance, PointStruct, UpsertPointsBuilder,
    VectorParamsBuilder, VectorsConfig,
};
use qdrant_client::Qdrant;
use std::collections::HashMap;
use std::env;
use tracing::info;

const COLLECTION_NAME: &str = "nostr_events";
const VECTOR_SIZE: u64 = 64; // Dimension of our feature vectors

/// Initialize Qdrant client and ensure collection exists
pub async fn initialize_qdrant() -> Result<Qdrant> {
    let qdrant_url = env::var("QDRANT_URL").unwrap_or_else(|_| "http://localhost:6334".to_string());

    info!("Connecting to Qdrant at {}", qdrant_url);

    let client = Qdrant::from_url(&qdrant_url)
        .build()
        .context("Failed to create Qdrant client")?;

    // Check if collection exists
    let collection_exists = client
        .collection_exists(COLLECTION_NAME)
        .await
        .context("Failed to check collection existence")?;

    if !collection_exists {
        info!("Creating collection: {}", COLLECTION_NAME);

        // Create collection with dense vector configuration
        let created = client
            .create_collection(
                CreateCollectionBuilder::new(COLLECTION_NAME)
                    .vectors_config(VectorsConfig {
                        config: Some(Config::Params(
                            VectorParamsBuilder::new(VECTOR_SIZE, Distance::Cosine).build(),
                        )),
                    })
                    .build(),
            )
            .await;

        match created {
            Ok(_) => info!("Collection created successfully"),
            // The exists-check and the create are not atomic: a concurrent
            // initializer (parallel tests share one Qdrant) can win the race.
            // Losing it is fine as long as the collection is there now.
            Err(e) => {
                let exists_now = client
                    .collection_exists(COLLECTION_NAME)
                    .await
                    .unwrap_or(false);
                if exists_now {
                    info!("Collection '{}' was created concurrently", COLLECTION_NAME);
                } else {
                    return Err(e).context("Failed to create collection");
                }
            }
        }
    } else {
        info!("Collection '{}' already exists", COLLECTION_NAME);
    }

    Ok(client)
}

/// Get the collection name (for use in other modules)
pub fn collection_name() -> &'static str {
    COLLECTION_NAME
}

/// Get the expected vector dimension
pub fn vector_size() -> usize {
    VECTOR_SIZE as usize
}

/// Whether a stats.json entry is still worth importing: fresh and its kind
/// still undocumented. Anything else would be deleted by the next
/// CleanupDocumentedKinds pass, so importing it only creates churn.
fn is_importable(
    kind: nostr_sdk::prelude::Kind,
    entry: &crate::actors::json_actor::KindEntry,
) -> bool {
    !crate::utils::is_old(entry.last_updated) && crate::utils::is_kind_free(kind)
}

/// Rename stats.json so later boots do not re-attempt migration.
async fn mark_stats_json_migrated(stats_file: &str) {
    let backup_path = format!("{}.migrated", stats_file);
    if let Err(e) = tokio::fs::rename(stats_file, &backup_path).await {
        info!("Could not rename {}: {}", stats_file, e);
    } else {
        info!("Renamed {} to {}", stats_file, backup_path);
    }
}

/// Import stats.json into Qdrant if this is the first startup
/// Returns true if migration was performed
///
/// # Errors
///
/// Fails only past the parse stage (collection lookup or bulk upsert). The
/// caller must treat that as fatal: the collection is still empty, so a
/// restart retries the import, while continuing would let the point-count
/// guard skip migration forever. A parse failure means stats.json is
/// corrupt; it leaves no partial state and recurs identically, so it logs
/// and skips instead of failing the boot.
pub async fn migrate_from_stats_json(client: &Qdrant, stats_file: &str) -> Result<bool> {
    use crate::actors::json_actor::KindEntry;
    use crate::similarity::EventFeatures;
    use nostr_sdk::prelude::Kind;
    use nostr_sdk::JsonUtil;

    // Check if Qdrant already has data
    let collection_info = client.collection_info(COLLECTION_NAME).await?;
    let point_count = collection_info
        .result
        .and_then(|r| r.points_count)
        .unwrap_or(0) as usize;

    if point_count > 0 {
        info!(
            "Qdrant already has {} events, skipping migration",
            point_count
        );
        return Ok(false);
    }

    // Try to load stats.json
    let json_str = match tokio::fs::read_to_string(stats_file).await {
        Ok(s) => s,
        Err(_) => {
            info!("No stats.json found, starting fresh");
            return Ok(false);
        }
    };

    let kind_stats: HashMap<Kind, KindEntry> = match serde_json::from_str(&json_str) {
        Ok(stats) => stats,
        Err(e) => {
            tracing::error!("stats.json is unparseable, skipping migration: {}", e);
            return Ok(false);
        }
    };

    if kind_stats.is_empty() {
        info!("stats.json is empty, nothing to migrate");
        return Ok(false);
    }

    let total = kind_stats.len();
    let importable: Vec<(Kind, KindEntry)> = kind_stats
        .into_iter()
        .filter(|(kind, entry)| is_importable(*kind, entry))
        .collect();

    info!(
        "Importing {} of {} stats.json entries ({} skipped as stale or documented)",
        importable.len(),
        total,
        total - importable.len()
    );

    if importable.is_empty() {
        mark_stats_json_migrated(stats_file).await;
        return Ok(false);
    }

    // Convert to Qdrant points
    let mut points = Vec::new();
    for (kind, entry) in &importable {
        let features = EventFeatures::from_event(&entry.event);
        let vector = features.to_vector();

        let payload_json = serde_json::json!({
            "kind": u16::from(*kind),
            "count": entry.count,
            "last_updated": entry.last_updated,
            "recommended_app": entry.recommended_app,
            "event_id": entry.event.id.to_string(),
            "event": entry.event.as_json(),
            "feature_version": crate::similarity::FEATURE_VERSION,
        });

        let payload =
            qdrant_client::Payload::from(payload_json.as_object().cloned().unwrap_or_default());

        points.push(PointStruct::new(u16::from(*kind) as u64, vector, payload));
    }

    // Bulk upsert to Qdrant
    client
        .upsert_points(UpsertPointsBuilder::new(COLLECTION_NAME, points).build())
        .await
        .context("Failed to bulk upsert to Qdrant")?;

    info!(
        "Successfully migrated {} events to Qdrant",
        importable.len()
    );

    mark_stats_json_migrated(stats_file).await;

    Ok(true)
}

/// Persist the recommended app name for a set of kinds.
///
/// Uses payload merge (`set_payload`) so vectors and other payload keys stay
/// untouched; ids not present in the collection are skipped by Qdrant.
pub async fn set_recommended_app(
    client: &Qdrant,
    kinds: &[nostr_sdk::prelude::Kind],
    app_name: &str,
) -> Result<()> {
    use qdrant_client::qdrant::{PointId, PointsIdsList, SetPayloadPointsBuilder};

    if kinds.is_empty() {
        return Ok(());
    }

    let ids: Vec<PointId> = kinds
        .iter()
        .map(|kind| PointId::from(u16::from(*kind) as u64))
        .collect();

    let payload = qdrant_client::Payload::from(
        serde_json::json!({ "recommended_app": app_name })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    );

    client
        .set_payload(
            SetPayloadPointsBuilder::new(COLLECTION_NAME, payload)
                .points_selector(PointsIdsList { ids })
                .build(),
        )
        .await
        .context("Failed to set recommended app payload")?;

    Ok(())
}

/// Convert Qdrant payload back to KindEntry
pub fn payload_to_kind_entry(
    payload: &HashMap<String, qdrant_client::qdrant::Value>,
) -> Result<crate::actors::json_actor::KindEntry> {
    use crate::actors::json_actor::KindEntry;
    use nostr_sdk::prelude::Event;
    use nostr_sdk::JsonUtil;

    let count_value = payload.get("count").context("Missing 'count' in payload")?;
    let last_updated_value = payload
        .get("last_updated")
        .context("Missing 'last_updated' in payload")?;
    let event_json_value = payload.get("event").context("Missing 'event' in payload")?;

    // Parse event from JSON string
    let event_json_str = event_json_value.as_str().context("Event is not a string")?;
    let event: Event = Event::from_json(event_json_str).context("Failed to parse event JSON")?;

    // Extract other fields
    let count = count_value
        .as_integer()
        .context("count is not an integer")? as u64;
    let last_updated = last_updated_value
        .as_integer()
        .context("last_updated is not an integer")?;

    let recommended_app = payload
        .get("recommended_app")
        .and_then(|v| v.as_str())
        .map(String::from);

    let cluster_id = payload
        .get("cluster_id")
        .and_then(|v| v.as_integer())
        .map(|i| i as u32);

    let cluster_similarity = payload
        .get("cluster_similarity")
        .and_then(|v| v.as_double());

    Ok(KindEntry {
        event,
        count,
        last_updated,
        recommended_app,
        recommended_app_event: None, // Not storing this in Qdrant currently
        cluster_id,
        cluster_similarity,
    })
}

/// Serializes the `#[ignore]` Qdrant integration tests. Disjoint point ids
/// are not enough isolation: cluster apply/clear, re-vectorization and
/// sentinel cleanup scan the whole collection, so tests running in parallel
/// mutate each other's points.
#[cfg(test)]
pub(crate) static QDRANT_TEST_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::json_actor::KindEntry;
    use nostr_sdk::prelude::*;

    async fn signed_event_async(kind: u16) -> Event {
        let keys = Keys::generate();
        let unsigned = EventBuilder::new(Kind::from(kind), "{}").build(keys.public_key());
        keys.sign_event(unsigned).await.unwrap()
    }

    fn kind_entry_from(event: Event, last_updated: i64) -> KindEntry {
        KindEntry {
            event,
            count: 1,
            last_updated,
            recommended_app: None,
            recommended_app_event: None,
            cluster_id: None,
            cluster_similarity: None,
        }
    }

    // For sync #[test]s; inside #[tokio::test] use kind_entry_async (a
    // nested runtime panics).
    fn kind_entry(kind: u16, last_updated: i64) -> KindEntry {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let event = rt.block_on(signed_event_async(kind));
        kind_entry_from(event, last_updated)
    }

    async fn kind_entry_async(kind: u16, last_updated: i64) -> KindEntry {
        kind_entry_from(signed_event_async(kind).await, last_updated)
    }

    #[test]
    fn test_is_importable_keeps_fresh_undocumented_kinds() {
        let now = chrono::Utc::now().timestamp_millis();
        // 64999 is not in the documented-kinds list or ranges
        let entry = kind_entry(64999, now);
        assert!(is_importable(Kind::from(64999u16), &entry));
    }

    #[test]
    fn test_is_importable_drops_stale_entries() {
        let stale =
            chrono::Utc::now().timestamp_millis() - chrono::Duration::days(31).num_milliseconds();
        let entry = kind_entry(64999, stale);
        assert!(
            !is_importable(Kind::from(64999u16), &entry),
            "an entry older than 30 days would be deleted by the next cleanup pass"
        );
    }

    #[test]
    fn test_is_importable_drops_documented_kinds() {
        let now = chrono::Utc::now().timestamp_millis();
        // Kind 1 is documented in every NIPs list including the static fallback
        let entry = kind_entry(1, now);
        assert!(
            !is_importable(Kind::from(1u16), &entry),
            "a documented kind would be deleted by the next cleanup pass"
        );
    }

    #[tokio::test]
    #[ignore] // Needs a local Qdrant whose nostr_events collection is EMPTY
              // apart from other tests' cleaned-up points: migration
              // short-circuits when points exist.
    async fn test_migrate_filters_stale_and_documented_entries() {
        use qdrant_client::qdrant::{DeletePointsBuilder, GetPointsBuilder, PointId};

        let _guard = QDRANT_TEST_GUARD.lock().await;
        let client = initialize_qdrant().await.unwrap();

        let info = client.collection_info(COLLECTION_NAME).await.unwrap();
        let points = info.result.and_then(|r| r.points_count).unwrap_or(0);
        assert_eq!(
            points, 0,
            "this test needs an empty collection: migration short-circuits when points exist"
        );

        let now = chrono::Utc::now().timestamp_millis();
        let stale = now - chrono::Duration::days(31).num_milliseconds();

        let dir = std::env::temp_dir().join(format!("uk-migrate-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stats_path = dir.join("stats.json");
        let stats_file = stats_path.to_str().unwrap();

        // Phase 1: every entry skippable -> no upsert, but the file is
        // still renamed so the next boot does not retry.
        let mut all_skipped: HashMap<Kind, KindEntry> = HashMap::new();
        all_skipped.insert(Kind::from(64998u16), kind_entry_async(64998, stale).await);
        all_skipped.insert(Kind::from(1u16), kind_entry_async(1, now).await);
        std::fs::write(&stats_path, serde_json::to_string(&all_skipped).unwrap()).unwrap();

        let migrated = migrate_from_stats_json(&client, stats_file).await.unwrap();
        assert!(!migrated, "nothing importable -> no migration");
        assert!(!stats_path.exists(), "stats.json must still be renamed");
        assert!(dir.join("stats.json.migrated").exists());
        let info = client.collection_info(COLLECTION_NAME).await.unwrap();
        assert_eq!(info.result.and_then(|r| r.points_count).unwrap_or(0), 0);

        // Phase 2: mixed file -> only the fresh undocumented kind lands.
        let mut mixed: HashMap<Kind, KindEntry> = HashMap::new();
        mixed.insert(Kind::from(64999u16), kind_entry_async(64999, now).await);
        mixed.insert(Kind::from(64998u16), kind_entry_async(64998, stale).await);
        mixed.insert(Kind::from(1u16), kind_entry_async(1, now).await);
        std::fs::write(&stats_path, serde_json::to_string(&mixed).unwrap()).unwrap();

        let migrated = migrate_from_stats_json(&client, stats_file).await.unwrap();
        assert!(migrated);

        let got = client
            .get_points(
                GetPointsBuilder::new(
                    COLLECTION_NAME,
                    vec![
                        PointId::from(64999u64),
                        PointId::from(64998u64),
                        PointId::from(1u64),
                    ],
                )
                .build(),
            )
            .await
            .unwrap();
        let ids: Vec<u64> = got
            .result
            .iter()
            .filter_map(|p| match p.id.as_ref()?.point_id_options.as_ref()? {
                qdrant_client::qdrant::point_id::PointIdOptions::Num(n) => Some(*n),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            vec![64999],
            "only the fresh undocumented kind is imported"
        );

        client
            .delete_points(
                DeletePointsBuilder::new(COLLECTION_NAME)
                    .points(vec![PointId::from(64999u64)])
                    .build(),
            )
            .await
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    #[ignore] // Only run when Qdrant is available locally
    async fn test_set_recommended_app_merges_payload() {
        use nostr_sdk::prelude::Kind;
        use qdrant_client::qdrant::{
            DeletePointsBuilder, GetPointsBuilder, PointId, PointStruct, UpsertPointsBuilder,
        };

        let _guard = QDRANT_TEST_GUARD.lock().await;
        let client = initialize_qdrant().await.unwrap();

        // Seed a point whose existing payload must survive the merge
        let payload = qdrant_client::Payload::from(
            serde_json::json!({"kind": 64999, "count": 7})
                .as_object()
                .cloned()
                .unwrap(),
        );
        let mut vector = vec![0.0f32; VECTOR_SIZE as usize];
        vector[0] = 1.0;
        client
            .upsert_points(
                UpsertPointsBuilder::new(
                    COLLECTION_NAME,
                    vec![PointStruct::new(64999u64, vector, payload)],
                )
                .build(),
            )
            .await
            .unwrap();

        set_recommended_app(&client, &[Kind::from(64999u16)], "TestApp")
            .await
            .unwrap();

        let point_id: PointId = 64999u64.into();
        let got = client
            .get_points(
                GetPointsBuilder::new(COLLECTION_NAME, vec![point_id])
                    .with_payload(true)
                    .build(),
            )
            .await
            .unwrap();
        let point = got.result.first().expect("seeded point exists");

        assert_eq!(
            point
                .payload
                .get("recommended_app")
                .and_then(|v| v.as_str())
                .map(String::as_str),
            Some("TestApp")
        );
        // Merge must not clobber unrelated keys
        assert_eq!(
            point.payload.get("count").and_then(|v| v.as_integer()),
            Some(7)
        );

        client
            .delete_points(
                DeletePointsBuilder::new(COLLECTION_NAME)
                    .points(vec![PointId::from(64999u64)])
                    .build(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore] // Only run when Qdrant is available locally
    async fn test_initialize_qdrant() {
        let _guard = QDRANT_TEST_GUARD.lock().await;
        let result = initialize_qdrant().await;
        assert!(result.is_ok());

        let client = result.unwrap();

        // Verify collection exists
        let exists = client.collection_exists(COLLECTION_NAME).await.unwrap();
        assert!(exists);
    }
}
