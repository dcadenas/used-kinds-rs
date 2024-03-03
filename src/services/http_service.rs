use base64::{engine::general_purpose::URL_SAFE, Engine as _};

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
use chrono::{DateTime, NaiveDateTime, Utc};
use handlebars::{Handlebars, Helper, Output, RenderContext, RenderError, RenderErrorReason};
use lazy_static::lazy_static;
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::fs::read_to_string as async_read_to_string;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};
use tracing::{error, info};

lazy_static! {
    static ref STATS_FILE: String =
        env::var("STATS_FILE").unwrap_or_else(|_| "/var/data/stats.json".to_string());
}

pub struct HttpService {
    cancellation_token: CancellationToken,
    state: AppState,
}
#[derive(Clone)]
struct AppState {
    hb: Arc<Handlebars<'static>>,
    last_file_load: Arc<RwLock<Option<DateTime<Utc>>>>,
    data: Arc<RwLock<Vec<(String, Stat)>>>,
}

impl AppState {
    fn new() -> Self {
        let mut hb = Handlebars::new();
        hb.register_helper("date_relative", Box::new(date_relative));
        hb.register_helper("json", Box::new(json_helper));

        if let Err(e) = hb.register_template_file("stats", "templates/stats.hbs") {
            error!("Failed to load template: {}", e);
        }

        AppState {
            hb: Arc::new(hb),
            last_file_load: Arc::new(RwLock::new(None)),
            data: Arc::new(RwLock::new(Vec::new())),
        }
    }

    async fn maybe_refresh_data(&mut self) -> Result<(), anyhow::Error> {
        let refresh_interval = 60;
        let last_file_load = self.last_file_load.read().await.clone();

        let should_refresh = if last_file_load.is_none() {
            true
        } else {
            let asdf = last_file_load.unwrap() + chrono::Duration::seconds(refresh_interval);
            if asdf < Utc::now() {
                true
            } else {
                false
            }
        };

        if should_refresh {
            match async_read_to_string(&*STATS_FILE).await {
                Ok(content) => {
                    let data: HashMap<String, Stat> = serde_json::from_str(&content)
                        .unwrap_or_else(|e| {
                            error!("Failed to parse stats file, defaulting to empty: {}", e);
                            HashMap::default()
                        });

                    let mut data_as_sorted_vec: Vec<(String, Stat)> = data.into_iter().collect();
                    data_as_sorted_vec.sort_by_key(|(kind, _)| kind.parse::<u32>().unwrap_or(0));

                    {
                        let mut stats_vec = self.data.write().await;
                        *stats_vec = data_as_sorted_vec;
                    }

                    let mut last_file_load = self.last_file_load.write().await;
                    *last_file_load = Some(Utc::now());

                    info!(
                        "Cache refreshed at {}",
                        last_file_load.unwrap().format("%Y-%m-%d %H:%M:%S")
                    );
                }
                Err(e) => error!("Failed to read stats file: {}", e),
            }
        } else {
            let next_check = last_file_load.unwrap() + chrono::Duration::seconds(refresh_interval);
            info!(
                "Using cached data until {}",
                next_check.format("%Y-%m-%d %H:%M:%S")
            );
        }

        Ok(())
    }
}

impl HttpService {
    pub fn new(cancellation_token: CancellationToken) -> Self {
        let state = AppState::new();
        HttpService {
            cancellation_token,
            state,
        }
    }

    pub async fn run(&self) -> Result<()> {
        let router = Router::new()
            .route("/", get(fetch_stats))
            .layer(TraceLayer::new_for_http())
            .layer(TimeoutLayer::new(Duration::from_secs(1)))
            .with_state(self.state.clone());

        let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let token_clone = self.cancellation_token.clone();
        let server = async {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown(token_clone))
                .await
        };

        let force_shutdown = async {
            self.cancellation_token.cancelled().await;
            sleep(Duration::from_secs(5)).await;
        };

        tokio::select! {
            _ = server => info!("HTTP service exited"),
            _ = force_shutdown => info!("HTTP service exited due to force shutdown"),
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

#[derive(Serialize, Deserialize, Clone)]
struct Stat {
    event: Event,
    count: u64,
    last_updated: i64, // Assuming UNIX timestamp for simplicity
}

async fn fetch_stats(
    _: Method,
    _: HeaderMap,
    State(mut app_state): State<AppState>,
) -> impl IntoResponse {
    app_state
        .maybe_refresh_data()
        .await
        .unwrap_or_else(|e| error!("Error loading data: {}", e));

    let stats_vec: Vec<(String, Stat)> = app_state.data.read().await.clone();
    let data = serde_json::json!({ "stats": stats_vec });

    match app_state.hb.render("stats", &data) {
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

fn date_relative(
    h: &Helper,
    _: &Handlebars,
    _: &handlebars::Context,
    _: &mut RenderContext,
    out: &mut dyn Output,
) -> Result<(), RenderError> {
    let timestamp = h.param(0).unwrap().value().as_u64().unwrap();

    let dt: Option<NaiveDateTime> = NaiveDateTime::from_timestamp_opt((timestamp / 1000) as i64, 0);
    let ago = match dt {
        Some(dt) => {
            let now = Utc::now().naive_utc();
            let duration = now.signed_duration_since(dt);
            if duration.num_seconds() < 60 {
                "just now".to_string()
            } else if duration.num_minutes() < 60 {
                let mins = duration.num_minutes();
                format!("{} minute{} ago", mins, if mins == 1 { "" } else { "s" })
            } else if duration.num_hours() < 24 {
                let hours = duration.num_hours();
                format!("{} hour{} ago", hours, if hours == 1 { "" } else { "s" })
            } else {
                let days = duration.num_days();
                format!("{} day{} ago", days, if days == 1 { "" } else { "s" })
            }
        }
        None => "Invalid timestamp".to_string(),
    };

    out.write(&ago)?;

    Ok(())
}

fn json_helper(
    h: &Helper,
    _: &Handlebars,
    _: &handlebars::Context,
    _: &mut RenderContext,
    out: &mut dyn Output,
) -> Result<(), RenderError> {
    let param = h
        .param(0)
        .ok_or(RenderErrorReason::ParamNotFoundForIndex("json", 0))?;

    let serialized = serde_json::to_string_pretty(param.value())
        .map_err(|_e| RenderErrorReason::InvalidJsonPath("json".to_string()))?;

    let encoded = URL_SAFE.encode(&serialized);

    out.write(&encoded)?;
    Ok(())
}
