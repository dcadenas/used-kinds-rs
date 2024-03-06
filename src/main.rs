mod actors;
mod service_manager;
mod utils;

use crate::actors::nostr_actor::NostrActor;
use crate::service_manager::ServiceManager;
use anyhow::{Context, Result};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let mut manager = ServiceManager::new();

    manager.spawn_actor(NostrActor, ()).await?;

    manager
        .manage()
        .await
        .context("Failed to spawn Nostr actor")
}
