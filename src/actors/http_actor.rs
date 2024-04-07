use super::json_actor::JsonActorMessage;
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
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{NaiveDateTime, Utc};
use handlebars::{Handlebars, Helper, Output, RenderContext, RenderError, RenderErrorReason};
use lazy_static::lazy_static;
use nostr_sdk::prelude::*;
use ractor::{call, concurrency::Duration, Actor, ActorProcessingErr, ActorRef};
use std::env;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::macros::support::Pin;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};
use tracing::{error, info};

lazy_static! {
    static ref STATS_FILE: String =
        env::var("STATS_FILE").unwrap_or_else(|_| "/var/data/stats.json".to_string());
}

// TODO: Currently pretty anemic to be an actor but can be refactored later
// while learning the actor model and how it merges with an http axum server
pub struct HttpActor;

#[derive(Clone)]
pub struct WebAppState {
    hb: Arc<Handlebars<'static>>,
    json_actor: ActorRef<JsonActorMessage>,
}

pub struct HttpActorState {
    shutdown: Pin<Box<dyn Future<Output = Result<(), std::io::Error>> + Send>>,
}

pub enum HttpActorMessage {}

#[ractor::async_trait]
impl Actor for HttpActor {
    type Msg = HttpActorMessage;
    type State = HttpActorState;
    type Arguments = ActorRef<JsonActorMessage>;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        json_actor: ActorRef<JsonActorMessage>,
    ) -> Result<Self::State, ActorProcessingErr> {
        let mut hb = Handlebars::new();
        hb.register_helper("date_relative", Box::new(date_relative));
        hb.register_helper("json", Box::new(json_helper));
        hb.register_helper("ellipsis", Box::new(ellipsis));

        if let Err(e) = hb.register_template_file("stats", "templates/stats.hbs") {
            error!("Failed to load template: {}", e);
        }

        let web_app_state = WebAppState {
            hb: Arc::new(hb),
            json_actor,
        };

        let router = Router::new()
            .route("/", get(fetch_stats))
            .route("/health", get(|| async { "OK" }))
            .layer(TraceLayer::new_for_http())
            .layer(TimeoutLayer::new(Duration::from_secs(1)))
            .with_state(web_app_state);

        let port = env::var("PORT")
            .unwrap_or_else(|_| "8080".to_string())
            .parse::<u16>()?;
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let cancellation_token = CancellationToken::new();
        let token_clone = cancellation_token.clone();
        let server = tokio::spawn(async {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown_hook(token_clone))
                .await
        });
        let shutdown = Box::pin(async move {
            cancellation_token.cancel();

            if let Err(e) = timeout(Duration::from_secs(5), server).await {
                info!("HTTP service exited after timeout: {}", e);
            } else {
                info!("HTTP service exited");
            }

            Ok(())
        });

        let state = HttpActorState { shutdown };

        Ok(state)
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        info!("Waiting for HTTP service to stop");
        if let Err(e) = state.shutdown.as_mut().await {
            error!("Failed to shutdown HTTP server: {}", e);
        }

        info!("HTTP actor stopped");

        Ok(())
    }
}

async fn shutdown_hook(cancellation_token: CancellationToken) {
    cancellation_token.cancelled().await;
    info!("Exiting the process");
}

async fn fetch_stats(
    _: Method,
    _: HeaderMap,
    State(app_state): State<WebAppState>,
) -> impl IntoResponse {
    if let Ok(stats_vec) = call!(app_state.json_actor, JsonActorMessage::GetStatsVec, ()) {
        let data = serde_json::json!({ "stats": stats_vec });
        match app_state.hb.render("stats", &data) {
            Ok(html) => return (StatusCode::OK, Html(html)),
            Err(e) => {
                error!("Error rendering template: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Html("<h1>Error rendering the page</h1>".to_string()),
                );
            }
        }
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Html("<h1>Error rendering the page</h1>".to_string()),
    )
}

fn shorten_with_ellipsis(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        format!("{}...", &s[..max_len])
    } else {
        s.to_string()
    }
}

fn ellipsis(
    h: &Helper,
    _: &Handlebars,
    _: &handlebars::Context,
    _: &mut RenderContext,
    out: &mut dyn Output,
) -> Result<(), RenderError> {
    let param = h
        .param(0)
        .ok_or(RenderErrorReason::ParamNotFoundForIndex("ellipsis", 0))?;

    let s = param
        .value()
        .as_str()
        .ok_or(RenderErrorReason::InvalidJsonPath("ellipsis".to_string()))?;
    let max_len =
        h.param(1)
            .ok_or(RenderErrorReason::ParamNotFoundForIndex("ellipsis", 1))?
            .value()
            .as_u64()
            .ok_or(RenderErrorReason::InvalidJsonPath("ellipsis".to_string()))? as usize;

    out.write(&shorten_with_ellipsis(s, max_len))?;
    Ok(())
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
    let encoded = STANDARD.encode(serialized);

    out.write(&encoded)?;
    Ok(())
}
