//! Lockers: stored items and the virtual locker of unlocked
//! equipment classnames.
//!
//! Grounded in `lib/services/src/locker.rs` (`LockerService` +
//! `LockerHotStateService`) and `lib/services/src/v_locker.rs`
//! (`VLockerService` + `VLockerHotStateService`), with the models in
//! `lib/models/src/locker.rs` (`Locker`, `Item`) and
//! `lib/models/src/v_locker.rs` (`VLocker`, `EquipmentCategory`), and
//! the extension commands in `arma/server/extension/src/locker.rs` and
//! `v_locker.rs`.
//!
//! The original's rules, mirrored here:
//!
//! - A locker holds at most **25 unique items** (`update_locker` /
//!   `add_item`). Adding an item whose classname is already stored
//!   overwrites it and does not count against the cap — only a *new*
//!   classname does.
//! - An item is `{category, classname, amount}`; `amount` must be at
//!   least 1 (`Item::validate` / `LockerValidationError::AmountZero`).
//! - The virtual locker is the per-player set of **unlocked equipment
//!   classnames**, split by category (`items`, `weapons`, `magazines`,
//!   `backpacks`). Adding a classname that is already unlocked is a
//!   no-op (the original's `VLocker::add`); the store checkout is what
//!   normally unlocks classnames.

use std::collections::HashMap;

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

/// SurrealDB table for player lockers (stored items).
pub const LOCKER_TABLE: &str = "player_locker";
/// SurrealDB table for player virtual lockers (unlocked classnames).
pub const VLOCKER_TABLE: &str = "player_vlocker";

/// Maximum unique items per locker — the original's capacity rule.
pub const MAX_ITEMS: usize = 25;

// ---------------------------------------------------------------------------
// Stored items
// ---------------------------------------------------------------------------

/// One stored item — the original's `Item` (`lib/models/src/locker.rs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub category: String,
    pub classname: String,
    pub amount: u32,
}

impl Item {
    fn validate(&self) -> Result<(), String> {
        if self.category.trim().is_empty() {
            return Err("Category cannot be empty".to_string());
        }
        if self.classname.trim().is_empty() {
            return Err("Classname cannot be empty".to_string());
        }
        if self.amount == 0 {
            return Err("Amount cannot be zero".to_string());
        }
        Ok(())
    }
}

/// A player's locker — the original's `Locker`: a map of classname →
/// item, keyed here by the player's UID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Locker {
    /// SurrealDB record id — internal only, never part of the API shape.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    pub id: Option<Value>,
    pub uid: String,
    #[serde(default)]
    pub items: HashMap<String, Item>,
}

/// Body for `PUT /players/{uid}/locker` — replace the whole locker
/// (original `update_locker`, bulk sync).
#[derive(Debug, Deserialize)]
pub struct ReplaceLockerBody {
    #[serde(default)]
    pub items: HashMap<String, Item>,
}

/// Body for `PATCH /players/{uid}/locker/items/{classname}` — the
/// original's `patch_item`: only the amount changes.
#[derive(Debug, Deserialize)]
pub struct PatchItemBody {
    pub amount: u32,
}

async fn load_locker(db: &super::Db, uid: &str) -> Result<Locker, (StatusCode, Json<Value>)> {
    load_by_field(db, LOCKER_TABLE, "uid", uid, "Locker").await
}

async fn save_locker(db: &super::Db, locker: &Locker) -> Result<Locker, (StatusCode, Json<Value>)> {
    let content = to_content(locker)?;
    update_record_content(db, LOCKER_TABLE, "uid", &locker.uid, content).await
}

async fn load_or_create_locker(
    db: &super::Db,
    uid: &str,
) -> Result<Locker, (StatusCode, Json<Value>)> {
    let existing: Vec<Locker> = select_by_field(db, LOCKER_TABLE, "uid", uid).await?;
    if let Some(locker) = existing.into_iter().next() {
        return Ok(locker);
    }
    let locker = Locker {
        id: None,
        uid: uid.to_string(),
        items: HashMap::new(),
    };
    create_record(db, LOCKER_TABLE, &locker).await
}

/// POST /players/{uid}/locker — create an empty locker (original
/// `LockerService::create_locker`). 409 when it already exists.
pub async fn create_locker(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let uid = uid.trim().to_string();
    if uid.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "UID cannot be empty"));
    }
    let db = db_or_503(&state).await?;
    let existing: Vec<Locker> = select_by_field(&db, LOCKER_TABLE, "uid", &uid).await?;
    if !existing.is_empty() {
        return Err(err(
            StatusCode::CONFLICT,
            format!("Locker for '{uid}' already exists"),
        ));
    }
    let locker = Locker {
        id: None,
        uid,
        items: HashMap::new(),
    };
    let created = create_record(&db, LOCKER_TABLE, &locker).await?;
    Ok((StatusCode::CREATED, Json(json!(created))))
}

/// GET /players/{uid}/locker — one player's locker (original
/// `get_locker`). 404 when the player has no locker yet.
pub async fn get_locker(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    let locker = load_locker(&db, &uid).await?;
    Ok((StatusCode::OK, Json(json!(locker))))
}

/// PUT /players/{uid}/locker — replace the whole locker (original
/// `update_locker`, bulk sync). More than 25 unique items is rejected
/// with the original's message.
pub async fn replace_locker(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<ReplaceLockerBody>,
) -> ApiResult {
    for item in body.items.values() {
        item.validate().map_err(|error| {
            err(
                StatusCode::BAD_REQUEST,
                format!("Validation failed: {error}"),
            )
        })?;
    }
    if body.items.len() > MAX_ITEMS {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Locker exceeds maximum capacity of 25 items.",
        ));
    }

    let db = db_or_503(&state).await?;
    let mut locker = load_or_create_locker(&db, &uid).await?;
    locker.items = body.items;
    let saved = save_locker(&db, &locker).await?;
    Ok((StatusCode::OK, Json(json!(saved))))
}

/// DELETE /players/{uid}/locker — remove a whole locker (original
/// `LockerService::delete`). 404 when the player has no locker.
pub async fn delete_locker(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    if !delete_by_field(&db, LOCKER_TABLE, "uid", &uid).await? {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Locker for '{uid}' not found."),
        ));
    }
    Ok((StatusCode::OK, Json(json!({ "deleted": true }))))
}

/// POST /players/{uid}/locker/items — store an item (original
/// `add_item`). Storing a classname that is already there overwrites
/// it; a new classname on a full locker (25 unique items) answers 409.
pub async fn add_item(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<Item>,
) -> ApiResult {
    body.validate().map_err(|error| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Validation failed: {error}"),
        )
    })?;

    let db = db_or_503(&state).await?;
    let mut locker = load_or_create_locker(&db, &uid).await?;
    if !locker.items.contains_key(&body.classname) && locker.items.len() >= MAX_ITEMS {
        return Err(err(
            StatusCode::CONFLICT,
            "Locker is full. Maximum of 25 items allowed.",
        ));
    }
    locker.items.insert(body.classname.clone(), body);
    let saved = save_locker(&db, &locker).await?;
    Ok((StatusCode::OK, Json(json!(saved))))
}

/// PATCH /players/{uid}/locker/items/{classname} — change an item's
/// amount (original `patch_item`). An amount of zero is rejected with
/// the original's message; 404 when the locker or the item does not
/// exist.
pub async fn patch_item(
    State(state): State<AppState>,
    Path((uid, classname)): Path<(String, String)>,
    Json(body): Json<PatchItemBody>,
) -> ApiResult {
    if body.amount == 0 {
        return Err(err(StatusCode::BAD_REQUEST, "Amount cannot be zero"));
    }

    let db = db_or_503(&state).await?;
    let mut locker = load_locker(&db, &uid).await?;
    let Some(item) = locker.items.get_mut(&classname) else {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Item with classname '{classname}' not found in locker"),
        ));
    };
    item.amount = body.amount;
    let saved = save_locker(&db, &locker).await?;
    Ok((StatusCode::OK, Json(json!(saved))))
}

/// DELETE /players/{uid}/locker/items/{classname} — remove an item
/// (original `remove_item`). 404 when the locker or the item does not
/// exist.
pub async fn remove_item(
    State(state): State<AppState>,
    Path((uid, classname)): Path<(String, String)>,
) -> ApiResult {
    let db = db_or_503(&state).await?;
    let mut locker = load_locker(&db, &uid).await?;
    if locker.items.remove(&classname).is_none() {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Item with classname '{classname}' not found in locker"),
        ));
    }
    let saved = save_locker(&db, &locker).await?;
    Ok((StatusCode::OK, Json(json!(saved))))
}

// ---------------------------------------------------------------------------
// Virtual locker (unlocked equipment classnames)
// ---------------------------------------------------------------------------

/// A player's virtual locker — the original's `VLocker`: unlocked
/// equipment classnames by category.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VLocker {
    /// SurrealDB record id — internal only, never part of the API shape.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    pub id: Option<Value>,
    pub uid: String,
    #[serde(default)]
    pub items: Vec<String>,
    #[serde(default)]
    pub weapons: Vec<String>,
    #[serde(default)]
    pub magazines: Vec<String>,
    #[serde(default)]
    pub backpacks: Vec<String>,
}

impl VLocker {
    fn category_mut(&mut self, category: &str) -> Option<&mut Vec<String>> {
        match category {
            "items" => Some(&mut self.items),
            "weapons" => Some(&mut self.weapons),
            "magazines" => Some(&mut self.magazines),
            "backpacks" => Some(&mut self.backpacks),
            _ => None,
        }
    }
}

/// Equipment categories — the original's `EquipmentCategory`.
pub fn parse_equipment_category(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "items" => Some("items"),
        "weapons" => Some("weapons"),
        "magazines" => Some("magazines"),
        "backpacks" => Some("backpacks"),
        _ => None,
    }
}

pub const EQUIPMENT_CATEGORIES: &str = "items, weapons, magazines, backpacks";

/// Body for `POST /players/{uid}/vlocker/{category}` — unlock
/// classnames (original `add_locker`).
#[derive(Debug, Deserialize)]
pub struct AddClassnamesBody {
    pub classnames: Vec<String>,
}

async fn load_vlocker(db: &super::Db, uid: &str) -> Result<VLocker, (StatusCode, Json<Value>)> {
    load_by_field(db, VLOCKER_TABLE, "uid", uid, "Virtual locker").await
}

async fn save_vlocker(
    db: &super::Db,
    vlocker: &VLocker,
) -> Result<VLocker, (StatusCode, Json<Value>)> {
    let content = to_content(vlocker)?;
    update_record_content(db, VLOCKER_TABLE, "uid", &vlocker.uid, content).await
}

async fn load_or_create_vlocker(
    db: &super::Db,
    uid: &str,
) -> Result<VLocker, (StatusCode, Json<Value>)> {
    let existing: Vec<VLocker> = select_by_field(db, VLOCKER_TABLE, "uid", uid).await?;
    if let Some(vlocker) = existing.into_iter().next() {
        return Ok(vlocker);
    }
    let vlocker = VLocker {
        uid: uid.to_string(),
        ..Default::default()
    };
    create_record(db, VLOCKER_TABLE, &vlocker).await
}

/// POST /players/{uid}/vlocker — create an empty virtual locker
/// (original `VLockerService::create_locker`). 409 when it exists.
pub async fn create_vlocker(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let uid = uid.trim().to_string();
    if uid.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "UID cannot be empty"));
    }
    let db = db_or_503(&state).await?;
    let existing: Vec<VLocker> = select_by_field(&db, VLOCKER_TABLE, "uid", &uid).await?;
    if !existing.is_empty() {
        return Err(err(
            StatusCode::CONFLICT,
            format!("Virtual locker for '{uid}' already exists"),
        ));
    }
    let vlocker = VLocker {
        uid,
        ..Default::default()
    };
    let created = create_record(&db, VLOCKER_TABLE, &vlocker).await?;
    Ok((StatusCode::CREATED, Json(json!(created))))
}

/// GET /players/{uid}/vlocker — one player's unlocked equipment
/// classnames (original `fetch_locker`). 404 when none exists yet.
pub async fn get_vlocker(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    let vlocker = load_vlocker(&db, &uid).await?;
    Ok((StatusCode::OK, Json(json!(vlocker))))
}

/// GET /players/{uid}/vlocker/{category} — the unlocked classnames of
/// one category (original `get_locker` with a field).
pub async fn get_vlocker_category(
    State(state): State<AppState>,
    Path((uid, category)): Path<(String, String)>,
) -> ApiResult {
    let category = parse_equipment_category(&category).ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Equipment category '{category}' is unsupported. Valid categories: {EQUIPMENT_CATEGORIES}."),
        )
    })?;
    let db = db_or_503(&state).await?;
    let vlocker = load_vlocker(&db, &uid).await?;
    let classnames = match category {
        "items" => &vlocker.items,
        "weapons" => &vlocker.weapons,
        "magazines" => &vlocker.magazines,
        _ => &vlocker.backpacks,
    };
    Ok((
        StatusCode::OK,
        Json(json!({ "category": category, "classnames": classnames })),
    ))
}

/// POST /players/{uid}/vlocker/{category} — unlock classnames
/// (original `add_locker`). Classnames already unlocked are left
/// alone. The record is created empty first when it does not exist,
/// as the original's hot service does.
pub async fn add_vlocker_classnames(
    State(state): State<AppState>,
    Path((uid, category)): Path<(String, String)>,
    Json(body): Json<AddClassnamesBody>,
) -> ApiResult {
    let category = parse_equipment_category(&category).ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Equipment category '{category}' is unsupported. Valid categories: {EQUIPMENT_CATEGORIES}."),
        )
    })?;
    let classnames: Vec<String> = body
        .classnames
        .into_iter()
        .map(|classname| classname.trim().to_string())
        .filter(|classname| !classname.is_empty())
        .collect();
    if classnames.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "At least one classname is required.",
        ));
    }

    let db = db_or_503(&state).await?;
    let mut vlocker = load_or_create_vlocker(&db, &uid).await?;
    let Some(list) = vlocker.category_mut(category) else {
        return Err(err(StatusCode::BAD_REQUEST, "Unknown equipment category."));
    };
    for classname in classnames {
        if !list.iter().any(|entry| entry == &classname) {
            list.push(classname);
        }
    }
    let saved = save_vlocker(&db, &vlocker).await?;
    Ok((StatusCode::OK, Json(json!(saved))))
}

/// DELETE /players/{uid}/vlocker — remove a whole virtual locker
/// (original `VLockerService::delete`). 404 when the player has none.
pub async fn delete_vlocker(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    if !delete_by_field(&db, VLOCKER_TABLE, "uid", &uid).await? {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Virtual locker for '{uid}' not found."),
        ));
    }
    Ok((StatusCode::OK, Json(json!({ "deleted": true }))))
}

/// DELETE /players/{uid}/vlocker/{category}/{classname} — remove one
/// unlocked classname (original `remove_locker`). 404 when the record,
/// category or classname does not exist.
pub async fn remove_vlocker_classname(
    State(state): State<AppState>,
    Path((uid, category, classname)): Path<(String, String, String)>,
) -> ApiResult {
    let category = parse_equipment_category(&category).ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Equipment category '{category}' is unsupported. Valid categories: {EQUIPMENT_CATEGORIES}."),
        )
    })?;

    let db = db_or_503(&state).await?;
    let mut vlocker = load_vlocker(&db, &uid).await?;
    let Some(list) = vlocker.category_mut(category) else {
        return Err(err(StatusCode::BAD_REQUEST, "Unknown equipment category."));
    };
    let Some(position) = list.iter().position(|entry| entry == &classname) else {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Classname '{classname}' not found in virtual locker."),
        ));
    };
    list.remove(position);
    let saved = save_vlocker(&db, &vlocker).await?;
    Ok((StatusCode::OK, Json(json!(saved))))
}

/// Shared by the store checkout: load a locker, creating an empty one
/// when it does not exist (the original's checkout fetches through the
/// hot service, which auto-creates).
pub async fn load_or_create_locker_for_checkout(
    db: &super::Db,
    uid: &str,
) -> Result<Locker, (StatusCode, Json<Value>)> {
    load_or_create_locker(db, uid).await
}

/// Shared by the store checkout: persist an already-mutated locker.
pub async fn save_locker_for_checkout(
    db: &super::Db,
    locker: &Locker,
) -> Result<Locker, (StatusCode, Json<Value>)> {
    save_locker(db, locker).await
}

/// Shared by the store checkout: load a virtual locker, creating an
/// empty one when it does not exist.
pub async fn load_or_create_vlocker_for_checkout(
    db: &super::Db,
    uid: &str,
) -> Result<VLocker, (StatusCode, Json<Value>)> {
    load_or_create_vlocker(db, uid).await
}

/// Shared by the store checkout: persist an already-mutated virtual
/// locker.
pub async fn save_vlocker_for_checkout(
    db: &super::Db,
    vlocker: &VLocker,
) -> Result<VLocker, (StatusCode, Json<Value>)> {
    save_vlocker(db, vlocker).await
}
