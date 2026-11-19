//! HTTP routes.

use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::AppState;

/// GET /health — liveness probe.
///
/// Always returns 200 while the server is up. The `database` field reports
/// whether the SurrealDB connection is live; the CAD persistence routes
/// return 503 while it is `unavailable`.
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
