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
use chrono::{NaiveDateTime, Utc};
use handlebars::{
    Context, Handlebars, Helper, Output, RenderContext, RenderError, RenderErrorReason,
};
use lazy_static::lazy_static;
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::fs::read_to_string;
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
}

impl HttpService {
    pub fn new(cancellation_token: CancellationToken) -> Self {
        let mut hb = Handlebars::new();
        hb.register_helper("date_relative", Box::new(date_relative));
        hb.register_helper("json", Box::new(json_helper));
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
    event: Event,
    count: u64,
    last_updated: u64, // Assuming UNIX timestamp for simplicity
}

async fn fetch_stats(
    _: Method,
    _: HeaderMap,
    State(AppState { hb, .. }): State<AppState>,
) -> impl IntoResponse {
    match read_to_string(&*STATS_FILE).await {
        Ok(content) => {
            let stats: HashMap<String, Stat> = serde_json::from_str(&content).unwrap_or_else(|e| {
                error!("Failed to parse stats file, defaulting to empty: {}", e);
                HashMap::default()
            });
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

fn date_relative(
    h: &Helper,
    _: &Handlebars,
    _: &Context,
    _: &mut RenderContext,
    out: &mut dyn Output,
) -> Result<(), RenderError> {
    let timestamp = h.param(0).unwrap().value().as_u64().unwrap();

    let dt = NaiveDateTime::from_timestamp_opt(timestamp as i64, 0);
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
    _: &Context,
    _: &mut RenderContext,
    out: &mut dyn Output,
) -> Result<(), RenderError> {
    // Attempt to get the parameter passed to the helper
    let param = h
        .param(0)
        .ok_or(RenderErrorReason::ParamNotFoundForIndex("json", 0))?;

    // Serialize the parameter to a JSON string
    let serialized = serde_json::to_string_pretty(param.value())
        .map_err(|_e| RenderErrorReason::InvalidJsonPath("json".to_string()))?;

    // Write the JSON string to the template output
    out.write(&serialized)?;
    Ok(())
}
