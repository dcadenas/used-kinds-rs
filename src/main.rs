mod actors;
mod nips_fetcher;
mod normalization;
mod qdrant_client;
mod service_manager;
mod similarity;
mod utils;

use crate::actors::nostr_actor::NostrActor;
use crate::service_manager::ServiceManager;
use anyhow::{Context, Result};
use axum::async_trait;
use ractor::{Actor, ActorProcessingErr, ActorRef, SupervisionEvent};
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

struct Supervisor;
#[async_trait]
impl Actor for Supervisor {
    type Msg = ();
    type State = ();
    type Arguments = ();

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        _: (),
    ) -> Result<Self::State, ActorProcessingErr> {
        Actor::spawn_linked(
            Some("NostrActor".to_string()),
            NostrActor,
            (),
            myself.get_cell(),
        )
        .await?;
        Ok(())
    }

    async fn handle_supervisor_evt(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: SupervisionEvent,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        info!("Supervisor received event: {:?}", message);
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    // Initialize NIPs fetcher with cache
    let cache_dir = if let Ok(stats_file) = std::env::var("STATS_FILE") {
        std::path::Path::new(&stats_file)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("/var/data"))
    } else {
        std::path::PathBuf::from("/var/data")
    };

    let cache_path = cache_dir.join("nips_kinds_cache.json");

    info!(
        "Initializing NIPs kinds fetcher with cache at: {:?}",
        cache_path
    );
    if let Err(e) = crate::nips_fetcher::load_or_fetch_kinds(Some(&cache_path)).await {
        tracing::warn!(
            "Failed to initialize NIPs kinds: {}. Using fallback list.",
            e
        );
    }

    // Spawn background task to periodically update NIPs kinds
    let cache_path_clone = cache_path.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60 * 60 * 24)); // Daily
        loop {
            interval.tick().await;
            info!("Updating NIPs kinds from GitHub");
            if let Err(e) = crate::nips_fetcher::load_or_fetch_kinds(Some(&cache_path_clone)).await
            {
                tracing::error!("Failed to update NIPs kinds: {}", e);
            }
        }
    });

    let mut manager = ServiceManager::new();

    manager.spawn_actor(Supervisor, ()).await?;

    manager
        .manage()
        .await
        .context("Failed to spawn Nostr actor")
}
