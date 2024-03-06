use anyhow::{Error, Result};
use ractor::{Actor, ActorRef};
use tokio::macros::support::Future;
use tokio::signal;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{error, info};

pub struct ServiceManager<A: Actor> {
    actors: Vec<ActorRef<A::Msg>>,
    tracker: TaskTracker,
    token: CancellationToken,
}

impl<A: Actor> ServiceManager<A> {
    pub fn new() -> Self {
        Self {
            actors: Vec::new(),
            tracker: TaskTracker::new(),
            token: CancellationToken::new(),
        }
    }

    pub async fn spawn_actor(
        &mut self,
        actor: A,
        args: A::Arguments,
    ) -> Result<ActorRef<A::Msg>, Error> {
        let name = Some(
            std::any::type_name::<A>()
                .split("::")
                .last()
                .unwrap()
                .to_string(),
        );
        let (json_actor, json_handle) = Actor::spawn(name, actor, args).await?;
        self.tracker.spawn(json_handle);
        self.actors.push(json_actor.clone());
        Ok(json_actor)
    }

    // Spawn through a function that receives a cancellation token
    pub fn spawn_service<F, Fut>(&self, task: F) -> JoinHandle<()>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send,
    {
        let token = self.token.clone();
        self.tracker.spawn(async move {
            let token_clone = token.clone();
            let task_fut = task(token);
            if let Err(e) = task_fut.await {
                error!("Task failed: {}", e);
                token_clone.cancel();
            }
        })
    }
    async fn stop(&self) -> Result<()> {
        self.token.cancel();

        for actor in self.actors.iter() {
            actor.stop(Some("Terminating".to_string()));
        }

        // TODO: move 5 to some config, it's the same for http actor timeout
        sleep(Duration::from_secs(5)).await;

        Ok(())
    }

    pub async fn manage(&self) -> Result<()> {
        self.tracker.close();
        #[cfg(unix)]
        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("Failed to install signal handler")
                .recv()
                .await;
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = self.tracker.wait() => {},
            _ = signal::ctrl_c() => {
                info!("Starting graceful termination, from ctrl-c");
                self.stop().await?
            },
            _ = terminate => {
                info!("Starting graceful termination, from terminate signal");
                self.stop().await?
            },
        }

        info!("Wait for all tasks to complete after the cancel");
        self.tracker.wait().await;
        info!("All tasks completed bye bye");
        Ok(())
    }
}
