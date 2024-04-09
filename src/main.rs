mod actors;
mod service_manager;
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

    let mut manager = ServiceManager::new();

    manager.spawn_actor(Supervisor, ()).await?;

    manager
        .manage()
        .await
        .context("Failed to spawn Nostr actor")
}
