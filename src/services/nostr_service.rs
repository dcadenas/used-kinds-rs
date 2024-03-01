use crate::utils::is_kind_free;
use anyhow::Result;
use nostr_sdk::prelude::*;
use tokio::sync::broadcast::Sender;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

const POPULAR_RELAYS: [&str; 4] = [
    "wss://relay.damus.io",
    "wss://relay.plebstr.com",
    "wss://relayable.org",
    "wss://relay.n057r.club",
];
pub struct NostrService {
    cancellation_token: CancellationToken,
    new_kind_event_tx: Sender<(Event, Url)>,
}
impl NostrService {
    pub fn new(
        cancellation_token: CancellationToken,
        new_kind_event_tx: Sender<(Event, Url)>,
    ) -> Self {
        NostrService {
            cancellation_token,
            new_kind_event_tx,
        }
    }

    pub async fn run(&self) -> Result<()> {
        let opts = Options::new()
            .wait_for_send(false)
            .connection_timeout(Some(Duration::from_secs(5)))
            .shutdown_on_drop(true)
            .wait_for_subscription(true);

        opts.pool.shutdown_on_drop(true);
        let client = ClientBuilder::new().opts(opts).build();

        for relay in POPULAR_RELAYS.into_iter() {
            client.add_relay(relay).await?;
        }

        client.connect().await;

        self.periodically_check_for_new_kinds(&client).await?;
        info!("Notification handler exited");

        Ok(())
    }

    async fn get_events(&self, client: &Client) {
        let amount = 1000;

        let duration = Duration::from_secs(60 * 5);

        let since = Timestamp::now() - duration;
        let filters = vec![Filter::new().limit(amount).since(since)];

        match client
            .get_events_of_with_opts(
                filters,
                Some(Duration::from_secs(60)),
                FilterOptions::WaitDurationAfterEOSE(Duration::from_secs(10)),
            )
            .await
        {
            Ok(events) => {
                for event in events {
                    if is_kind_free(event.kind.as_u32()) {
                        debug!("Received a new kind event: {:?}", event);
                        info!("Received a new event of kind {}", event.kind);
                        // TODO: How can I know where was this event found? FTM it's hardcoded
                        let relay_url = Url::parse("wss://relay.damus.io").unwrap();
                        if let Err(e) = self.new_kind_event_tx.send((event, relay_url)) {
                            error!("Failed to send the new kind event: {}", e);
                        }
                    }
                }
            }
            Err(e) => error!("Failed to get events: {}", e),
        }
    }

    async fn periodically_check_for_new_kinds(&self, client: &Client) -> Result<()> {
        let duration = Duration::from_secs(60 * 5);
        let mut interval = tokio::time::interval(duration);

        loop {
            interval.tick().await;
            info!("Checking for new kind events...");

            tokio::select! {
                _ = self.cancellation_token.cancelled() => {
                    info!("Cancellation token received, stopping the check for new kind events");
                    break;
                },
                // It's a shame that get_events doesn't support cancellation. TODO: contribute to nostr-sdk
                _ = self.get_events(client) => {}
            }
        }

        info!("Cancellation token received, stopping the check for new kind events");
        Ok(())
    }
}
