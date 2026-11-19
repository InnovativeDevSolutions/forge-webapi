//! HTTP routes.

use axum::{Json, extract::State};
use serde_json::{Value, json};

use crate::AppState;

/// GET /health — liveness probe.
///
/// Always returns 200 while the server is up. The `database` field reports
/// whether the SurrealDB connection is live; persistence endpoints (added
/// later) return 503 while it is `unavailable`.
pub async fn health(State(state): State<AppState>) -> Json<Value> {
    let database = if state.db.is_some() {
        "connected"
    } else {
        "unavailable"
    };
    Json(json!({
        "status": "ok",
        "database": database,
    }))
}
