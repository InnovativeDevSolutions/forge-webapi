//! Player actors: the persistent per-player record (identity, loadout,
//! position, phone, org).
//!
//! Grounded in `lib/models/src/actor.rs` (`Actor`) and the extension
//! commands in `arma/server/extension/src/actor.rs` (`actor:create`,
//! `actor:get`, `actor:hot:get`, `actor:hot:save`, `actor:hot:delete`).
//!
//! The original keeps actors in a hot in-memory store backed by the
//! legacy `actor` SurrealDB table. The API stores the same record in
//! its own `player_actor` table — deliberately **not** the legacy
//! `actor` table: the legacy record serializes `phone_number`
//! (snake_case) while this API is camelCase (`phoneNumber`), so
//! sharing the table would silently drop the phone number on read in
//! either direction. The bridge migrates legacy actors into this
//! table on first read (see the extension's `ApiActorRepository`).
//!
//! JSON uses camelCase (`phoneNumber`), matching the rest of this API;
//! the extension translates on the way in and out (see
//! `arma/server/extension/src/api.rs`).

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{
    create_record, db_or_503, delete_by_field, err, load_by_field, select_by_field, to_content,
    update_record_content, ApiResult,
};
use crate::AppState;

/// SurrealDB table for player actors.
pub const ACTOR_TABLE: &str = "player_actor";

/// A player actor — the original's `Actor` (`lib/models/src/actor.rs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Actor {
    /// SurrealDB record id — internal only, never part of the API shape.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    pub id: Option<Value>,
    pub uid: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub loadout: Value,
    #[serde(default)]
    pub position: Option<Vec<f64>>,
    #[serde(default)]
    pub direction: f64,
    #[serde(default)]
    pub stance: Option<String>,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub phone_number: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub holster: bool,
    #[serde(default)]
    pub rank: Option<String>,
    #[serde(default)]
    pub organization: String,
}

impl Actor {
    /// Validation mirroring `Actor::validate`
    /// (`lib/models/src/actor.rs`): the UID must be a numeric id of
    /// 12–17 digits (the relaxed single-player rule — see the
    /// extension's `is_valid_uid`), the name at most 50 characters,
    /// the direction a heading, the phone number empty or a generated
    /// `0160…` number, the email empty or a `.mil` address, the org
    /// at most 100 characters.
    pub fn validate(&self) -> Result<(), String> {
        if self.uid.trim().is_empty() {
            return Err("UID cannot be empty".to_string());
        }
        if !is_valid_uid(&self.uid) {
            return Err(format!(
                "Invalid UID '{}' - must be a numeric ID of 12-17 digits",
                self.uid
            ));
        }
        if let Some(name) = &self.name {
            if name.trim().is_empty() || name.len() > 50 {
                return Err(format!("Invalid name '{name}'"));
            }
        }
        if !self.direction.is_finite() || !(0.0..360.0).contains(&self.direction) {
            return Err("Direction must be between 0.0 and 360.0".to_string());
        }
        if !self.phone_number.is_empty()
            && (!self.phone_number.starts_with("0160") || self.phone_number.len() != 10)
        {
            return Err(format!("Invalid phone number '{}'", self.phone_number));
        }
        if !self.email.is_empty() && (!self.email.contains('@') || !self.email.ends_with(".mil")) {
            return Err(format!("Invalid email '{}'", self.email));
        }
        if self.organization.len() > 100 {
            return Err("Organization cannot exceed 100 characters".to_string());
        }
        Ok(())
    }
}

/// The extension's relaxed UID rule (`lib/shared/src/validation.rs`
/// `is_valid_uid`): a numeric UID of 12–17 digits. Multiplayer Steam
/// IDs are 17 digits; single-player profile IDs are 12–13.
fn is_valid_uid(uid: &str) -> bool {
    (12..=17).contains(&uid.len()) && uid.chars().all(|c| c.is_ascii_digit())
}

async fn load_actor(db: &super::Db, uid: &str) -> Result<Actor, (StatusCode, Json<Value>)> {
    load_by_field(db, ACTOR_TABLE, "uid", uid, "Actor").await
}

async fn save_actor(db: &super::Db, actor: &Actor) -> Result<Actor, (StatusCode, Json<Value>)> {
    let content = to_content(actor)?;
    update_record_content(db, ACTOR_TABLE, "uid", &actor.uid, content).await
}

fn check_uid_match(path_uid: &str, body_uid: &str) -> Result<(), (StatusCode, Json<Value>)> {
    if path_uid != body_uid {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("Actor UID '{body_uid}' does not match the path UID '{path_uid}'."),
        ));
    }
    Ok(())
}

fn validate_actor_body(uid: &str, body: &Actor) -> Result<(), (StatusCode, Json<Value>)> {
    if uid.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "UID cannot be empty"));
    }
    check_uid_match(uid, body.uid.trim())?;
    body.validate().map_err(|error| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Validation failed: {error}"),
        )
    })
}

/// POST /players/{uid}/actor — create an actor (original
/// `ActorService::create`). 409 when the actor already exists.
pub async fn create_actor(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<Actor>,
) -> ApiResult {
    let uid = uid.trim().to_string();
    validate_actor_body(&uid, &body)?;

    let db = db_or_503(&state).await?;
    let existing: Vec<Actor> = select_by_field(&db, ACTOR_TABLE, "uid", &uid).await?;
    if !existing.is_empty() {
        return Err(err(
            StatusCode::CONFLICT,
            format!("Actor with uid '{uid}' already exists"),
        ));
    }
    let created = create_record(&db, ACTOR_TABLE, &body).await?;
    Ok((StatusCode::CREATED, Json(json!(created))))
}

/// GET /players/{uid}/actor — one player's actor (original
/// `ActorService::get_actor`). 404 when the player has no actor yet.
pub async fn get_actor(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    let actor = load_actor(&db, &uid).await?;
    Ok((StatusCode::OK, Json(json!(actor))))
}

/// PUT /players/{uid}/actor — replace the whole actor (original
/// `ActorService::update`, upsert). Creates the record when it does
/// not exist yet, so the hot-state save path never 404s.
pub async fn replace_actor(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<Actor>,
) -> ApiResult {
    let uid = uid.trim().to_string();
    validate_actor_body(&uid, &body)?;

    let db = db_or_503(&state).await?;
    let existing: Vec<Actor> = select_by_field(&db, ACTOR_TABLE, "uid", &uid).await?;
    if existing.is_empty() {
        let created = create_record(&db, ACTOR_TABLE, &body).await?;
        return Ok((StatusCode::OK, Json(json!(created))));
    }
    let saved = save_actor(&db, &body).await?;
    Ok((StatusCode::OK, Json(json!(saved))))
}

/// DELETE /players/{uid}/actor — remove an actor (original
/// `ActorService::delete_actor`). 404 when the player has no actor.
pub async fn delete_actor(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    if !delete_by_field(&db, ACTOR_TABLE, "uid", &uid).await? {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Actor '{uid}' not found."),
        ));
    }
    Ok((StatusCode::OK, Json(json!({ "deleted": true }))))
}
