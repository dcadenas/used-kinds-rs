use anyhow::Result;
use log::info;
use tokio::macros::support::Future;
use tokio::signal;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub struct ServiceManager {
    tracker: TaskTracker,
    token: CancellationToken,
}

impl ServiceManager {
    pub fn new() -> Self {
        Self {
            tracker: TaskTracker::new(),
            token: CancellationToken::new(),
        }
    }

    pub fn spawn<F, Fut>(&self, task: F) -> JoinHandle<()>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send,
    {
        let token = self.token.clone();
        self.tracker.spawn(async move {
            let task_fut = task(token);
            if let Err(e) = task_fut.await {
                log::error!("Task failed: {}", e);
            }
        })
    }

    pub async fn manage(&self) -> Result<()> {
        self.tracker.close();
        let token_clone = self.token.clone();
        #[cfg(unix)]
        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("failed to install signal handler")
                .recv()
                .await;
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = self.tracker.wait() => {},
            _ = signal::ctrl_c() => {
                info!("Starting graceful termination, from ctrl-c");
                token_clone.cancel();
            },
            _ = terminate => {
                info!("Starting graceful termination, from terminate signal");
                token_clone.cancel();
            },
        }

        info!("Wait for all tasks to complete after the cancel");
        self.tracker.wait().await;
        info!("All tasks completed bye bye");
        Ok(())
    }
}
