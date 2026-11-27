//! Player-data endpoints: bank, garages, lockers, phones, orgs, stores.
//!
//! ## Scope (Jacob, 2026-11-19)
//!
//! The persistent player state the original Forge extension used to hold
//! in memory (and therefore could lose on a server crash): bank accounts,
//! garages (stored vehicles + the virtual garage of unlocked vehicle
//! classnames), lockers (stored items + the virtual locker of unlocked
//! equipment classnames), phones (contacts, messages, emails),
//! organizations (members, invites, assets, fleet, credit lines), and the
//! store checkout that ties them together.
//!
//! ## Auth
//!
//! These endpoints are **unauthenticated for now**, exactly like the CAD
//! and lifecycle slices — auth (player login screen in-game, API-issued
//! token, player endpoints only answering with a valid token) was parked
//! by Jacob as a to-do for later. Do not mistake the absence of auth here
//! for a decision that it is unnecessary: any caller can currently read
//! and mutate any player's bank, garage, locker, phone and org data.
//!
//! ## Grounding in the original Forge implementation
//!
//! Every subsystem mirrors the service layer in `lib/services/src/` and
//! the record shapes in `lib/models/src/` of
//! InnovativeDevSolutions/forge, with the extension command surface in
//! `arma/server/extension/src/` (`bank.rs`, `garage.rs`, `locker.rs`,
//! `phone.rs`, `store.rs`, `org.rs`, `v_garage.rs`, `v_locker.rs`).
//! Where the original keeps state only in its hot (in-memory) layer with
//! no persisted shape of its own, the API stores the same record in
//! SurrealDB — the whole point of this slice is that the record now
//! survives a server crash. Each submodule's docs name the original
//! functions it mirrors and any place where the API had to choose.
//!
//! ## Persistence
//!
//! One table per record kind, keyed by a business-key field (player UID
//! or org id) — the same select-by-field pattern the original extension
//! uses (`arma/server/extension/src/storage/common.rs`) and the CAD slice
//! uses. Records are fully modeled (unlike the free-form CAD catalog
//! entries), so mutations load the record, apply the original's business
//! rules in memory, and write the whole record back with `UPDATE ...
//! CONTENT`.
//!
//! ## Conventions (shared with the CAD slice)
//!
//! - Validation runs before any database access; bad bodies/ids answer
//!   `400` with the original's wording where one exists.
//! - State conflicts (garage full, insufficient funds, already exists)
//!   answer `409`; "only the leader can…" answers `403`; missing records
//!   answer `404`.
//! - No database connection → `503 database unavailable`; SurrealDB
//!   errors → `502`. `/health` and the `arma://` CORS handling are
//!   untouched.
//! - Money is `f64`, exactly as the original stores it. Org funds and
//!   credit lines are rounded to cents, as the original's
//!   `round_currency` does; bank balances are not rounded, as the
//!   original's bank service does not round them.

pub mod actor;
pub mod bank;
pub mod garage;
pub mod locker;
pub mod org;
pub mod phone;
pub mod store;

use axum::{http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::Db;

// The shared helpers, re-exported so the submodules keep using `super::err`,
// `super::db_or_503` and so on.
pub use crate::common::{
    db_err, db_or_503, decode_record, err, now_unix_f64, select_all,
    select_by_field, ApiResult,
};

/// Load one record by its business key; 404 when it does not exist.
pub async fn load_by_field<T>(
    db: &Db,
    table: &str,
    field: &str,
    value: &str,
    label: &str,
) -> Result<T, (StatusCode, Json<Value>)>
where
    T: for<'de> Deserialize<'de>,
{
    let records: Vec<T> = select_by_field(db, table, field, value).await?;
    records.into_iter().next().ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            format!("{label} '{value}' not found."),
        )
    })
}

/// Create a record in `table` from a serializable value and return the
/// stored record, decoded.
pub async fn create_record<T>(
    db: &Db,
    table: &str,
    record: &T,
) -> Result<T, (StatusCode, Json<Value>)>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    let content = serde_json::to_value(record)
        .map_err(|error| err(StatusCode::BAD_REQUEST, format!("Invalid record: {error}")))?;
    let created: Option<Value> = db
        .create(table)
        .content(content)
        .await
        .map_err(|error| db_err("create record", error))?;
    match created {
        Some(record) => decode_record(record),
        None => Err(db_err("create record", "no record returned")),
    }
}

/// Replace a record's whole content (`UPDATE ... CONTENT`) by its business
/// key and return the updated record. Used for player records because they
/// are fully modeled — unlike the CAD catalog entries, there are no
/// unmodeled fields a whole-record write could drop. 404 when no record
/// matches the key.
pub async fn update_record_content<T>(
    db: &Db,
    table: &str,
    key_field: &str,
    key: &str,
    content: Value,
) -> Result<T, (StatusCode, Json<Value>)>
where
    T: for<'de> Deserialize<'de>,
{
    let mut response = db
        .query(format!(
            "UPDATE {table} CONTENT $content WHERE {key_field} = $id"
        ))
        .bind(("content", content))
        .bind(("id", key.to_string()))
        .await
        .map_err(|error| db_err("update record", error))?;
    let updated: Vec<Value> = response
        .take(0)
        .map_err(|error| db_err("update record", error))?;
    match updated.into_iter().next() {
        Some(record) => decode_record(record),
        None => Err(err(
            StatusCode::NOT_FOUND,
            format!("Record '{key}' not found."),
        )),
    }
}

/// Delete records by a business-key field. Returns `true` when at
/// least one record was deleted, `false` when nothing matched — the
/// caller decides whether "nothing matched" is a 404 (record deletes)
/// or a no-op (idempotent deletes).
///
/// `RETURN BEFORE` is required: a plain `DELETE ... WHERE` in
/// SurrealDB 3.x returns an empty result even when it deleted rows,
/// so the count can only be read from the before-image.
pub async fn delete_by_field(
    db: &Db,
    table: &str,
    key_field: &str,
    key: &str,
) -> Result<bool, (StatusCode, Json<Value>)> {
    // Table and field names come from constants, never from user input;
    // the value is a bound parameter.
    let mut response = db
        .query(format!(
            "DELETE {table} WHERE {key_field} = $id RETURN BEFORE"
        ))
        .bind(("id", key.to_string()))
        .await
        .map_err(|error| db_err("delete record", error))?;
    let deleted: Vec<Value> = response
        .take(0)
        .map_err(|error| db_err("delete record", error))?;
    Ok(!deleted.is_empty())
}

/// Serialize a record for `update_record_content` / `create_record`.
pub fn to_content<T>(record: &T) -> Result<Value, (StatusCode, Json<Value>)>
where
    T: Serialize,
{
    serde_json::to_value(record)
        .map_err(|error| err(StatusCode::BAD_REQUEST, format!("Invalid record: {error}")))
}

/// Round to cents, exactly as the original's `round_currency` in
/// `lib/models/src/org.rs` and `lib/services/src/store.rs`: never
/// negative, two decimal places.
pub fn round_currency(value: f64) -> f64 {
    (value.max(0.0) * 100.0).round() / 100.0
}
