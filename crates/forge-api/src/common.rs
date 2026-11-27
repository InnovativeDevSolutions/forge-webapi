//! Helpers shared by every persistence route (CAD and player data):
//! the JSON error shape, the 503 degraded mode, SurrealDB error mapping,
//! record decoding, and the select-by-field lookup.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{http::StatusCode, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{db::Db, AppState};

/// Shared result type: success and error both carry a status + JSON body.
pub type ApiResult = Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)>;

/// An error response: `status` with the body `{ "error": message }`.
pub fn err(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": message.into() })))
}

/// Current time as whole Unix seconds.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Current time as an f64 of Unix seconds — the original stores Arma time
/// as f64, and phone message/email timestamps are f64.
pub fn now_unix_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Require a database connection; persistence routes answer 503 while the
/// API is running without SurrealDB (same degraded-mode pattern as
/// `/health` reporting `database: "unavailable"`).
pub async fn db_or_503(state: &AppState) -> Result<Arc<Db>, (StatusCode, Json<Value>)> {
    match &state.db {
        Some(db) => Ok(Arc::clone(db)),
        None => Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "database unavailable: SurrealDB is not connected",
        )),
    }
}

/// A SurrealDB failure, answered as 502.
pub fn db_err(context: &str, error: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    err(
        StatusCode::BAD_GATEWAY,
        format!("SurrealDB {context} failed: {error}"),
    )
}

pub fn decode_record<T>(record: Value) -> Result<T, (StatusCode, Json<Value>)>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(record).map_err(|error| {
        err(
            StatusCode::BAD_GATEWAY,
            format!("SurrealDB decode failed: {error}"),
        )
    })
}

pub fn decode_records<T>(records: Vec<Value>) -> Result<Vec<T>, (StatusCode, Json<Value>)>
where
    T: for<'de> Deserialize<'de>,
{
    records.into_iter().map(decode_record).collect()
}

pub async fn select_all<T>(db: &Db, table: &str) -> Result<Vec<T>, (StatusCode, Json<Value>)>
where
    T: for<'de> Deserialize<'de>,
{
    let values: Vec<Value> = db
        .select(table)
        .await
        .map_err(|error| db_err("select", error))?;
    decode_records(values)
}

pub async fn select_by_field<T>(
    db: &Db,
    table: &str,
    field: &str,
    value: &str,
) -> Result<Vec<T>, (StatusCode, Json<Value>)>
where
    T: for<'de> Deserialize<'de>,
{
    // Table and field names come from constants, never from user input; the
    // value is a bound parameter.
    let mut response = db
        .query(format!("SELECT * FROM {table} WHERE {field} = $value"))
        .bind(("value", value.to_string()))
        .await
        .map_err(|error| db_err("select", error))?;
    let values: Vec<Value> = response.take(0).map_err(|error| db_err("select", error))?;
    decode_records(values)
}
