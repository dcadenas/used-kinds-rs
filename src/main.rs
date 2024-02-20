mod services;
mod utils;

use crate::services::http_service::HttpService;
use crate::services::json_service::JsonService;
use crate::services::nostr_service::NostrService;
use crate::services::service_manager::ServiceManager;
use anyhow::Result;
use log::info;
use nostr_sdk::prelude::*;
use tracing::error;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let service_manager = ServiceManager::new();

    let (new_kind_event_tx, new_kind_event_rx) =
        tokio::sync::broadcast::channel::<(Event, Url)>(1000);

    service_manager.spawn(|cancellation_token| async move {
        let mut json_service = JsonService::init(cancellation_token, new_kind_event_rx).await?;

        json_service.run().await.map_err(|e| {
            error!("Json service exited with error: {}", e);
            // Errors returned stop the other services too
            e
        })?;

        info!("Json service exited");
        Ok(())
    });

    service_manager.spawn(|cancellation_token| async move {
        let nostr_service = NostrService::new(cancellation_token, new_kind_event_tx);

        nostr_service.run().await.map_err(|e| {
            error!("Nostr service exited with error: {}", e);
            // Errors returned stop the other services too
            e
        })?;

        info!("Nostr service exited");
        Ok(())
    });

    service_manager.spawn(|cancellation_token| async move {
        let http_service = HttpService::new(cancellation_token);

        http_service.run().await.map_err(|e| {
            error!("Http service exited with error: {}", e);
            // Errors returned stop the other services too
            e
        })?;

        info!("Http service exited");
        Ok(())
    });

    service_manager.manage().await
}
