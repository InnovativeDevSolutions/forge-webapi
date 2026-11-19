//! forge-api — standalone HTTP API for the Forge Arma 3 framework.
//!
//! Serves the Forge game servers outside the Arma process, so session data no
//! longer lives and dies with the game server's Rust extension. All durable
//! state lives in SurrealDB; this process is stateless.

mod cad;
mod config;
mod cors;
mod db;
mod routes;

use std::sync::Arc;

use axum::{
    routing::{get, put},
    Router,
};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use config::Config;
use db::Db;

/// Shared state handed to every route handler.
#[derive(Clone)]
pub struct AppState {
    /// SurrealDB client. `None` when the database could not be reached at
    /// startup — the API still serves /health so the container stays
    /// observable; the CAD persistence routes answer 503 while it is `None`.
    pub db: Option<Arc<Db>>,
}

#[tokio::main]
async fn main() -> Result<(), String> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("forge_api=info,tower_http=info")
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Config::from_env()?;

    let db = match db::connect(&config).await {
        Ok(db) => {
            tracing::info!(
                "Connected to SurrealDB at {} (namespace '{}', database '{}')",
                config.surreal_endpoint,
                config.surreal_namespace,
                config.surreal_database
            );
            Some(Arc::new(db))
        }
        Err(err) => {
            tracing::warn!(
                "Starting without a database connection: {err}. /health will report database=unavailable."
            );
            None
        }
    };

    let state = AppState { db };

    let app = Router::new()
        .route("/health", get(routes::health))
        .route(
            "/cad/groups",
            get(cad::list_groups).post(cad::register_group),
        )
        .route("/cad/groups/{id}", get(cad::get_group))
        .route("/cad/groups/{id}/status", put(cad::update_group_status))
        .route("/cad/groups/{id}/contracts", get(cad::list_group_contracts))
        .route("/cad/contracts", get(cad::list_contracts))
        .route("/cad/contracts/{id}", get(cad::get_contract))
        .layer(cors::arma_cors_layer())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .map_err(|err| format!("failed to bind {}: {err}", config.bind_addr))?;

    tracing::info!("forge-api listening on {}", config.bind_addr);
    axum::serve(listener, app)
        .await
        .map_err(|err| format!("server error: {err}"))?;

    Ok(())
}
