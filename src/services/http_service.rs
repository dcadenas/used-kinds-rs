use anyhow::Result;
use axum::{
    extract::State,
    http::{HeaderMap, Method, StatusCode},
    response::Html,
};
use axum::{
    response::IntoResponse, // Import Json for JSON responses
    routing::get,
    Router,
};
use chrono::NaiveDateTime;
use handlebars::{Context, Handlebars, Helper, Output, RenderContext, RenderError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::fs::read_to_string;
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};
use tracing::{error, info};

pub struct HttpService {
    cancellation_token: CancellationToken,
    state: AppState,
}
#[derive(Clone)]
struct AppState {
    hb: Arc<Handlebars<'static>>,
}

impl HttpService {
    pub fn new(cancellation_token: CancellationToken) -> Self {
        let mut hb = Handlebars::new();
        hb.register_helper("date", Box::new(date_helper));
        if let Err(e) = hb.register_template_file("stats", "templates/stats.hbs") {
            error!("Failed to load template: {}", e);
        }
        let state = AppState { hb: Arc::new(hb) };
        HttpService {
            cancellation_token,
            state,
        }
    }

    pub async fn run(&self) -> Result<()> {
        // let app = Router::new()
        //     .route("/", get(fetch_stats))
        //     .layer(Extension(self.state))
        //     .layer(TraceLayer::new_for_http())
        //     .layer(TimeoutLayer::new(Duration::from_secs(1)));
        let router = Router::new()
            .route("/", get(fetch_stats))
            .layer(TraceLayer::new_for_http())
            .layer(TimeoutLayer::new(Duration::from_secs(1)))
            .with_state(self.state.clone());

        let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let token_clone = self.cancellation_token.clone();

        let task = tokio::spawn(async move {
            match axum::serve(listener, router)
                .with_graceful_shutdown(shutdown(token_clone))
                .await
            {
                Ok(_) => info!("HTTP server finished"),
                Err(e) => info!("HTTP server finished with error: {}", e),
            }
        });

        self.cancellation_token.cancelled().await;
        sleep(Duration::from_secs(1)).await;

        if !task.is_finished() {
            info!("Forcefully shutting down the HTTP server in 5 seconds");
            sleep(Duration::from_secs(5)).await;
            task.abort();
        }
        info!("HTTP service exited");

        Ok(())
    }
}

fn shutdown(cancellation_token: CancellationToken) -> impl Future<Output = ()> {
    async move {
        cancellation_token.cancelled().await;
        info!("Exiting the process");
    }
}

#[derive(Serialize, Deserialize)]
struct Stat {
    event_id: String,
    count: u64,
    last_updated: u64, // Assuming UNIX timestamp for simplicity
}

async fn fetch_stats(
    _: Method,
    _: HeaderMap,
    State(AppState { hb, .. }): State<AppState>,
) -> impl IntoResponse {
    match read_to_string("/var/data/stats.json").await {
        Ok(content) => {
            let stats: HashMap<String, Stat> = serde_json::from_str(&content).unwrap_or_default();
            let mut stats_vec: Vec<(String, Stat)> = stats.into_iter().collect();
            stats_vec.sort_by_key(|(kind, _)| kind.parse::<u32>().unwrap_or(0));

            // Directly prepare the sorted vector for the template, no need to convert back to HashMap
            let data = serde_json::json!({ "stats": stats_vec });

            match hb.render("stats", &data) {
                Ok(html) => (StatusCode::OK, Html(html)),
                Err(e) => {
                    error!("Error rendering template: {}", e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Html("<h1>Error rendering the page</h1>".to_string()),
                    )
                }
            }
        }
        Err(e) => {
            error!("Failed to read stats file: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<h1>Failed to read stats file</h1>".to_string()),
            )
        }
    }
}

fn date_helper(
    h: &Helper,
    _: &Handlebars,
    _: &Context,
    _: &mut RenderContext,
    out: &mut dyn Output,
) -> Result<(), RenderError> {
    let timestamp = h.param(0).unwrap().value().as_u64().unwrap();
    match NaiveDateTime::from_timestamp_opt(timestamp as i64, 0) {
        Some(dt) => {
            out.write(&dt.format("%Y-%m-%d %H:%M:%S").to_string())?;
            Ok(())
        }
        None => {
            tracing::error!("Invalid timestamp encountered: {}", timestamp);
            out.write("Invalid timestamp")?;
            Ok(())
        }
    }
}
