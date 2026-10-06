use crate::metrics::SharedServiceState;
use anyhow::{Context, Result};
use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
use std::net::SocketAddr;
use tokio::{net::TcpListener, task::JoinHandle};
use tracing::{error, info};

pub fn init_tracing(verbose: bool) {
    let level = if verbose { "debug" } else { "info" };
    let fallback = format!("{},hyper=warn,axum=warn", level);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(fallback));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}

pub async fn spawn_http_server(
    addr: SocketAddr,
    state: SharedServiceState,
) -> Result<JoinHandle<()>> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind observability endpoint on {}", addr))?;
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/stats", get(stats))
        .with_state(state);
    info!("observability endpoint listening on {}", addr);
    Ok(tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            error!("observability server stopped: {:?}", err);
        }
    }))
}

async fn healthz(
    State(state): State<SharedServiceState>,
) -> (StatusCode, Json<crate::metrics::HealthSnapshot>) {
    let snapshot = state.health_snapshot().await;
    let status = if snapshot.ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(snapshot))
}

async fn stats(State(state): State<SharedServiceState>) -> Json<crate::metrics::StatsSnapshot> {
    Json(state.stats_snapshot().await)
}
