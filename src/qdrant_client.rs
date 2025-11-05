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
        client
            .create_collection(
                CreateCollectionBuilder::new(COLLECTION_NAME)
                    .vectors_config(VectorsConfig {
                        config: Some(Config::Params(
                            VectorParamsBuilder::new(VECTOR_SIZE, Distance::Cosine).build(),
                        )),
                    })
                    .build(),
            )
            .await
            .context("Failed to create collection")?;

        info!("Collection created successfully");
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

/// Import stats.json into Qdrant if this is the first startup
/// Returns true if migration was performed
pub async fn migrate_from_stats_json(client: &Qdrant, stats_file: &str) -> Result<bool> {
    use crate::actors::json_actor::KindEntry;
    use crate::similarity::EventFeatures;
    use nostr_sdk::prelude::Kind;
    use nostr_sdk::JsonUtil;

    // Check if Qdrant already has data
    let collection_info = client.collection_info(COLLECTION_NAME).await?;
    let point_count = collection_info.result.and_then(|r| r.points_count).unwrap_or(0) as usize;

    if point_count > 0 {
        info!("Qdrant already has {} events, skipping migration", point_count);
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

    // Parse stats.json
    let kind_stats: HashMap<Kind, KindEntry> = serde_json::from_str(&json_str)
        .context("Failed to parse stats.json")?;

    if kind_stats.is_empty() {
        info!("stats.json is empty, nothing to migrate");
        return Ok(false);
    }

    info!("Migrating {} events from stats.json to Qdrant...", kind_stats.len());

    // Convert to Qdrant points
    let mut points = Vec::new();
    for (kind, entry) in &kind_stats {
        let features = EventFeatures::from_event(&entry.event);
        let vector = features.to_vector();

        let payload_json = serde_json::json!({
            "kind": u16::from(*kind),
            "count": entry.count,
            "last_updated": entry.last_updated,
            "recommended_app": entry.recommended_app,
            "event_id": entry.event.id.to_string(),
            "event": entry.event.as_json(),
        });

        let payload = qdrant_client::Payload::from(
            payload_json.as_object().cloned().unwrap_or_default()
        );

        points.push(PointStruct::new(u16::from(*kind) as u64, vector, payload));
    }

    // Bulk upsert to Qdrant
    client
        .upsert_points(UpsertPointsBuilder::new(COLLECTION_NAME, points).build())
        .await
        .context("Failed to bulk upsert to Qdrant")?;

    info!("Successfully migrated {} events to Qdrant", kind_stats.len());

    // Rename stats.json to mark migration complete
    let backup_path = format!("{}.migrated", stats_file);
    if let Err(e) = tokio::fs::rename(stats_file, &backup_path).await {
        info!("Could not rename stats.json: {}. Migration complete anyway.", e);
    } else {
        info!("Renamed stats.json to {}", backup_path);
    }

    Ok(true)
}

/// Convert Qdrant payload back to KindEntry
pub fn payload_to_kind_entry(payload: &HashMap<String, qdrant_client::qdrant::Value>) -> Result<crate::actors::json_actor::KindEntry> {
    use crate::actors::json_actor::KindEntry;
    use nostr_sdk::prelude::Event;
    use nostr_sdk::JsonUtil;

    let count_value = payload.get("count")
        .context("Missing 'count' in payload")?;
    let last_updated_value = payload.get("last_updated")
        .context("Missing 'last_updated' in payload")?;
    let event_json_value = payload.get("event")
        .context("Missing 'event' in payload")?;

    // Parse event from JSON string
    let event_json_str = event_json_value
        .as_str()
        .context("Event is not a string")?;
    let event: Event = Event::from_json(event_json_str)
        .context("Failed to parse event JSON")?;

    // Extract other fields
    let count = count_value.as_integer()
        .context("count is not an integer")? as u64;
    let last_updated = last_updated_value.as_integer()
        .context("last_updated is not an integer")?;

    let recommended_app = payload.get("recommended_app")
        .and_then(|v| v.as_str())
        .map(String::from);

    Ok(KindEntry {
        event,
        count,
        last_updated,
        recommended_app,
        recommended_app_event: None,  // Not storing this in Qdrant currently
        cluster_id: None,  // Computed separately
        cluster_similarity: None,  // Computed separately
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore] // Only run when Qdrant is available locally
    async fn test_initialize_qdrant() {
        let result = initialize_qdrant().await;
        assert!(result.is_ok());

        let client = result.unwrap();

        // Verify collection exists
        let exists = client.collection_exists(COLLECTION_NAME).await.unwrap();
        assert!(exists);
    }
}
