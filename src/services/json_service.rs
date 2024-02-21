use crate::utils::is_kind_free;
use anyhow::Result;
use chrono::{DateTime, Utc};
use lazy_static::lazy_static;
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use tokio::time::{interval, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

lazy_static! {
    static ref STATS_FILE: String =
        env::var("STATS_FILE").unwrap_or_else(|_| "/var/data/stats.json".to_string());
}

#[derive(Serialize, Deserialize)]
struct KindEntry {
    event: Event,
    count: u64,
    #[serde(with = "chrono::serde::ts_seconds")]
    last_updated: DateTime<Utc>,
}

pub struct JsonService {
    cancellation_token: CancellationToken,
    new_kind_event_rx: broadcast::Receiver<(Event, Url)>,
    kind_stats: Arc<Mutex<HashMap<u32, KindEntry>>>,
}

impl JsonService {
    pub async fn init(
        cancellation_token: CancellationToken,
        new_kind_event_rx: broadcast::Receiver<(Event, Url)>,
    ) -> Result<Self> {
        let json_str = tokio::fs::read_to_string(&*STATS_FILE)
            .await
            .unwrap_or_else(|e| {
                error!("Failed to read stats file, defaulting to empty: {}", e);
                "{}".to_string()
            });

        let mut kind_stats: HashMap<u32, KindEntry> = serde_json::from_str(&json_str)
            .unwrap_or_else(|e| {
                error!("Failed to read stats file, defaulting to empty: {}", e);
                HashMap::default()
            });

        // Remove any entries that are older than 1 month or for which is_kind_free is false
        kind_stats.retain(|kind, entry| {
            let is_old = entry.last_updated < Utc::now() - chrono::Duration::days(30);
            !is_old && is_kind_free(*kind)
        });

        Ok(JsonService {
            cancellation_token,
            new_kind_event_rx: new_kind_event_rx,
            kind_stats: Arc::new(Mutex::new(kind_stats)),
        })
    }

    async fn save_stats_to_json(
        kind_stats_arc: &Arc<Mutex<HashMap<u32, KindEntry>>>,
    ) -> Result<()> {
        let kind_stats = kind_stats_arc.lock().await;
        let json_str = serde_json::to_string_pretty(&*kind_stats)?;
        tokio::fs::write(&*STATS_FILE, json_str).await?;
        Ok(())
    }

    pub async fn run(&mut self) -> Result<()> {
        let cancellation_token = self.cancellation_token.clone();
        let kind_stats = self.kind_stats.clone();

        tokio::spawn(async move {
            let mut task_interval = interval(Duration::from_secs(60));
            while !cancellation_token.is_cancelled() {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        info!("Cancellation token received, stopping the json service");
                        break;
                    },
                    _ = task_interval.tick() => {
                        info!("Saving the stats to json file");
                        if let Err(err) = JsonService::save_stats_to_json(&kind_stats).await {
                            error!("Error saving stats to json file: {:?}", err);
                        }
                    }
                }
            }
        });

        loop {
            {
                tokio::select! {
                    _ = self.cancellation_token.cancelled() => {
                        info!("Cancellation token received, stopping the json service");
                        break;
                    },
                    recv_result = self.new_kind_event_rx.recv() => {
                        if let Ok((new_kind_event, _relay_url)) = recv_result {

                            let mut kind_stats = self.kind_stats.lock().await;
                            kind_stats.entry(new_kind_event.kind.as_u32())
                            .and_modify(|e| {
                                e.event = new_kind_event.clone();
                                e.count += 1;
                                e.last_updated = Utc::now();
                            })
                            .or_insert_with(|| KindEntry {
                                event: new_kind_event,
                                count: 1,
                                last_updated: Utc::now(),
                            });

                        }
                    },
                }
            }
        }
        Ok(())
    }
}
