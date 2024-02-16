use crate::utils::is_kind_free;
use anyhow::Result;
use nostr_sdk::prelude::*;
use tokio::sync::broadcast::Sender;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

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
        let opts = Options::new().wait_for_send(false);
        let client = ClientBuilder::new().opts(opts).build();

        for relay in POPULAR_RELAYS.into_iter() {
            client.add_relay(relay).await?;
        }

        client.connect().await;
        let subscription = Filter::new().since(Timestamp::now());

        client.subscribe(vec![subscription]).await;

        tokio::select! {
            _ = self.cancellation_token.cancelled() => {
                client.disconnect().await?;
                info!("Cancellation token is cancelled");
            },
            _ = client.handle_notifications(|notification| async {
                if self.cancellation_token.is_cancelled() {
                    info!("Cancellation token is cancelled, returning Ok(true) from notification handler");
                    return Ok(true); // Stop the loop
                }

                match notification {
                    RelayPoolNotification::Event { event, relay_url } => {
                        if is_kind_free(event.kind.as_u32()) {
                            debug!("Received a new kind event: {:?}", event);
                            info!("Received a new kind {} event from {}", event.kind, relay_url);
                            self.new_kind_event_tx.send((event, relay_url))?;
                        }
                    }
                    _ => {}
                }

                Ok(false) // Keep the loop running
            }) => {
                info!("Notification handler exited");
            }

        }

        info!("Disconnecting from all relays");
        client.disconnect().await?;

        Ok(())
    }
}
