use anyhow::{Context, Result};
use qdrant_client::qdrant::{
    vectors_config::Config, CreateCollectionBuilder, Distance, VectorParamsBuilder, VectorsConfig,
};
use qdrant_client::Qdrant;
use std::env;
use tracing::{info, warn};

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
