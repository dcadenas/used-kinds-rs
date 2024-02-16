use crate::utils::is_kind_free;
use anyhow::Result;
use chrono::{DateTime, Utc};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::broadcast;
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[derive(Serialize, Deserialize)]
struct KindEntry {
    event_id: String,
    count: u64,
    #[serde(with = "chrono::serde::ts_seconds")]
    last_updated: DateTime<Utc>,
}

pub struct JsonService {
    cancellation_token: CancellationToken,
    new_kind_event_rx: broadcast::Receiver<(Event, Url)>,
    kind_stats: HashMap<u32, KindEntry>,
}

impl JsonService {
    pub async fn init(
        cancellation_token: CancellationToken,
        new_kind_event_rx: broadcast::Receiver<(Event, Url)>,
    ) -> Result<Self> {
        let json_str = tokio::fs::read_to_string("data/stats.json")
            .await
            .unwrap_or_else(|_| "{}".to_string());
        let mut kind_stats: HashMap<u32, KindEntry> =
            serde_json::from_str(&json_str).unwrap_or_default();

        // Remove any entries that are older than 1 month or for which is_kind_free is false
        kind_stats.retain(|kind, entry| {
            let is_old = entry.last_updated < Utc::now() - chrono::Duration::days(30);
            !is_old && is_kind_free(*kind)
        });

        Ok(JsonService {
            cancellation_token,
            new_kind_event_rx,
            kind_stats,
        })
    }

    async fn save_stats_to_json(&self) -> Result<()> {
        let json_str = serde_json::to_string_pretty(&self.kind_stats)?;
        tokio::fs::write("data/stats.json", json_str).await?;
        Ok(())
    }

    pub async fn run(&mut self) -> Result<()> {
        loop {
            tokio::select! {
                _ = sleep(Duration::from_secs(60 * 5)) => {
                    info!("Saving the stats to json file");
                    self.save_stats_to_json().await?;
                },
                _ = self.cancellation_token.cancelled() => {
                    info!("Broadcast saver exited");
                    break;
                },
                recv_result = self.new_kind_event_rx.recv() => {
                    if let Ok((new_kind_event, relay_url)) = recv_result {
                        let relay_urls = vec![relay_url.to_string()];
                        let event_id = match Nip19Event::new(new_kind_event.id, relay_urls).to_bech32() {
                            Ok(id) => id,
                            Err(e) => {
                                warn!("Error converting event ID to bech32: {:?}", e);
                                continue;
                            }
                        };

                        self.kind_stats.entry(new_kind_event.kind.as_u32())
                        .and_modify(|e| {
                            e.count += 1;
                            e.last_updated = Utc::now();
                        })
                        .or_insert_with(|| KindEntry {
                            event_id: event_id,
                            count: 1,
                            last_updated: Utc::now(),
                        });

                    }
                },
            }
        }
        Ok(())
    }
}
