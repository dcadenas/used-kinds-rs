mod services;
mod utils;

use crate::services::http_service::HttpService;
use crate::services::json_service::{JsonActor, JsonActorMessage};
use crate::services::service_manager::ServiceManager;
use anyhow::Result;
use ractor::cast;
use tracing::{error, info};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let service_manager = ServiceManager::new();

    let json_actor = service_manager.spawn_actor(JsonActor, ()).await?;

    service_manager.spawn(|cancellation_token| async move {
        let http_service = HttpService::new(cancellation_token);

        http_service.run().await.map_err(|e| {
            error!("Http service exited with error: {}", e);
            // Errors returned stop the other services too
            e
        })?;

        cast!(json_actor, JsonActorMessage::Stop).map_err(|e| {
            error!("Failed to stop json actor: {}", e);
            e
        })?;

        info!("Http service exited");
        Ok(())
    });

    service_manager.manage().await
}
