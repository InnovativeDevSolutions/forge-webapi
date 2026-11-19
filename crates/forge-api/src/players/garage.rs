//! Garages: stored vehicles and the virtual garage of unlocked
//! vehicle classnames.
//!
//! Grounded in `lib/services/src/garage.rs` (`GarageService` +
//! `GarageHotStateService`) and `lib/services/src/v_garage.rs`
//! (`VGarageService` + `VGarageHotStateService`), with the models in
//! `lib/models/src/garage.rs` (`Garage`, `Vehicle`, `HitPoints`) and
//! `lib/models/src/v_garage.rs` (`VGarage`, `VehicleCategory`), and the
//! extension commands in `arma/server/extension/src/garage.rs` and
//! `v_garage.rs`.
//!
//! The original's rules, mirrored here:
//!
//! - A garage holds at most **5 vehicles** (`update_garage` /
//!   `add_vehicle`).
//! - A vehicle's `plate` is its id; the original generates it as a UUID
//!   v4 when a vehicle is stored (`add_vehicle`), so this API does the
//!   same — callers never supply a plate on add.
//! - `fuel` and `damage` are 0.0–1.0; hit-point arrays (`names`,
//!   `selections`, `values`) must all have the same length and values
//!   0.0–1.0 (the original's `Vehicle::validate` /
//!   `GarageValidationError` wording).
//! - The virtual garage is the per-player set of **unlocked vehicle
//!   classnames**, split by category (`cars`, `armor`, `helis`,
//!   `planes`, `naval`, `other`). Adding a classname that is already
//!   unlocked is a no-op (the original's `VGarage::add` only pushes
//!   when absent); the store checkout is what normally unlocks
//!   classnames. A category fetch of an unknown category returns an
//!   empty list in the original (`get_garage`); the API rejects unknown
//!   categories with 400 instead, so a typo is visible rather than
//!   silently empty.
//!
//! JSON uses camelCase, so the original's `hit_points` serializes here
//! as `hitPoints`.

use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{
    create_record, db_or_503, err, load_by_field, select_by_field, to_content,
    update_record_content, ApiResult,
};
use crate::AppState;

/// SurrealDB table for player garages (stored vehicles).
pub const GARAGE_TABLE: &str = "player_garage";
/// SurrealDB table for player virtual garages (unlocked classnames).
pub const VGARAGE_TABLE: &str = "player_vgarage";

/// Maximum vehicles per garage — the original's capacity rule.
pub const MAX_VEHICLES: usize = 5;

// ---------------------------------------------------------------------------
// Stored vehicles
// ---------------------------------------------------------------------------

/// Hit points of a stored vehicle — the original's `HitPoints`
/// (`lib/models/src/garage.rs`). The legacy normalization (a missing
/// `names` array is filled from `selections` and vice versa) is kept:
/// the API accepts either spelling.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HitPoints {
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub selections: Vec<String>,
    #[serde(default)]
    pub values: Vec<f64>,
}

impl HitPoints {
    fn normalize(&mut self) {
        if self.names.is_empty()
            && !self.selections.is_empty()
            && self.selections.len() == self.values.len()
        {
            self.names = self.selections.clone();
        }
        if self.selections.is_empty()
            && !self.names.is_empty()
            && self.names.len() == self.values.len()
        {
            self.selections = self.names.clone();
        }
    }

    fn validate(&self) -> Result<(), String> {
        let names_len = self.names.len();
        let selections_len = self.selections.len();
        let values_len = self.values.len();
        if names_len != selections_len || names_len != values_len {
            return Err(
                "Hit point arrays (names, selections, values) must all have the same length"
                    .to_string(),
            );
        }
        for (i, value) in self.values.iter().enumerate() {
            if !(0.0..=1.0).contains(value) {
                return Err(format!(
                    "Hitpoint value at index {i} is invalid - must be between 0.0 and 1.0"
                ));
            }
        }
        Ok(())
    }
}

/// One stored vehicle — the original's `Vehicle`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Vehicle {
    pub plate: String,
    pub classname: String,
    #[serde(default)]
    pub fuel: f64,
    #[serde(default)]
    pub damage: f64,
    #[serde(default)]
    pub hit_points: HitPoints,
}

impl Vehicle {
    fn validate(&self) -> Result<(), String> {
        if self.classname.trim().is_empty() {
            return Err("Classname cannot be empty".to_string());
        }
        if !(0.0..=1.0).contains(&self.fuel) {
            return Err("Fuel must be between 0.0 and 1.0".to_string());
        }
        if !(0.0..=1.0).contains(&self.damage) {
            return Err("Damage must be between 0.0 and 1.0".to_string());
        }
        self.hit_points.validate()
    }
}

/// A player's garage — the original's `Garage`: a map of plate →
/// vehicle, keyed here by the player's UID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Garage {
    /// SurrealDB record id — internal only, never part of the API shape.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    pub id: Option<Value>,
    pub uid: String,
    #[serde(default)]
    pub vehicles: HashMap<String, Vehicle>,
}

/// Body for `POST /players/{uid}/garage/vehicles` — store a vehicle.
/// The plate is generated server-side (UUID v4), exactly as the
/// original's `add_vehicle` does.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddVehicleBody {
    pub classname: String,
    #[serde(default)]
    pub fuel: f64,
    #[serde(default)]
    pub damage: f64,
    #[serde(default)]
    pub hit_points: HitPoints,
}

/// Body for `PUT /players/{uid}/garage` — replace the whole garage
/// (original `update_garage`, bulk sync).
#[derive(Debug, Deserialize)]
pub struct ReplaceGarageBody {
    #[serde(default)]
    pub vehicles: HashMap<String, Vehicle>,
}

/// Body for `PATCH /players/{uid}/garage/vehicles/{plate}` — the
/// original's `patch_vehicle`: only the fields present change.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchVehicleBody {
    #[serde(default)]
    pub damage: Option<f64>,
    #[serde(default)]
    pub fuel: Option<f64>,
    #[serde(default)]
    pub hit_points: Option<HitPoints>,
}

async fn load_garage(db: &super::Db, uid: &str) -> Result<Garage, (StatusCode, Json<Value>)> {
    load_by_field(db, GARAGE_TABLE, "uid", uid, "Garage").await
}

async fn save_garage(db: &super::Db, garage: &Garage) -> Result<Garage, (StatusCode, Json<Value>)> {
    let content = to_content(garage)?;
    update_record_content(db, GARAGE_TABLE, "uid", &garage.uid, content).await
}

/// Load a garage, creating an empty one when it does not exist — the
/// original's hot `add_vehicle` creates the garage on first store, and
/// the store checkout grants into it the same way.
async fn load_or_create_garage(
    db: &super::Db,
    uid: &str,
) -> Result<Garage, (StatusCode, Json<Value>)> {
    let existing: Vec<Garage> = select_by_field(db, GARAGE_TABLE, "uid", uid).await?;
    if let Some(garage) = existing.into_iter().next() {
        return Ok(garage);
    }
    let garage = Garage {
        id: None,
        uid: uid.to_string(),
        vehicles: HashMap::new(),
    };
    create_record(db, GARAGE_TABLE, &garage).await
}

/// POST /players/{uid}/garage — create an empty garage (original
/// `GarageService::create_garage`). 409 when it already exists.
pub async fn create_garage(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let uid = uid.trim().to_string();
    if uid.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "UID cannot be empty"));
    }
    let db = db_or_503(&state).await?;
    let existing: Vec<Garage> = select_by_field(&db, GARAGE_TABLE, "uid", &uid).await?;
    if !existing.is_empty() {
        return Err(err(
            StatusCode::CONFLICT,
            format!("Garage for '{uid}' already exists"),
        ));
    }
    let garage = Garage {
        id: None,
        uid,
        vehicles: HashMap::new(),
    };
    let created = create_record(&db, GARAGE_TABLE, &garage).await?;
    Ok((StatusCode::CREATED, Json(json!(created))))
}

/// GET /players/{uid}/garage — one player's garage (original
/// `get_garage`). 404 when the player has no garage yet.
pub async fn get_garage(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    let garage = load_garage(&db, &uid).await?;
    Ok((StatusCode::OK, Json(json!(garage))))
}

/// PUT /players/{uid}/garage — replace the whole garage (original
/// `update_garage`, bulk sync). Every vehicle is validated; more than
/// 5 vehicles is rejected with the original's message.
pub async fn replace_garage(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<ReplaceGarageBody>,
) -> ApiResult {
    for vehicle in body.vehicles.values() {
        vehicle.validate().map_err(|error| {
            err(
                StatusCode::BAD_REQUEST,
                format!("Validation failed: {error}"),
            )
        })?;
    }
    if body.vehicles.len() > MAX_VEHICLES {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Garage exceeds maximum capacity of 5 vehicles.",
        ));
    }

    let db = db_or_503(&state).await?;
    let mut garage = load_or_create_garage(&db, &uid).await?;
    garage.vehicles = body.vehicles;
    let saved = save_garage(&db, &garage).await?;
    Ok((StatusCode::OK, Json(json!(saved))))
}

/// POST /players/{uid}/garage/vehicles — store a vehicle (original
/// `add_vehicle`). The plate is a fresh UUID v4, as in the original;
/// a full garage (5 vehicles) answers 409.
pub async fn add_vehicle(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(mut body): Json<AddVehicleBody>,
) -> ApiResult {
    body.hit_points.normalize();
    let vehicle = Vehicle {
        plate: uuid::Uuid::new_v4().to_string(),
        classname: body.classname,
        fuel: body.fuel,
        damage: body.damage,
        hit_points: body.hit_points,
    };
    vehicle.validate().map_err(|error| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Validation failed: {error}"),
        )
    })?;

    let db = db_or_503(&state).await?;
    let mut garage = load_or_create_garage(&db, &uid).await?;
    if garage.vehicles.len() >= MAX_VEHICLES {
        return Err(err(
            StatusCode::CONFLICT,
            "Garage is full. Maximum of 5 vehicles allowed.",
        ));
    }
    garage.vehicles.insert(vehicle.plate.clone(), vehicle);
    let saved = save_garage(&db, &garage).await?;
    Ok((StatusCode::OK, Json(json!(saved))))
}

/// PATCH /players/{uid}/garage/vehicles/{plate} — patch one stored
/// vehicle (original `patch_vehicle`): damage, fuel and/or hit points.
/// 404 when the garage or the vehicle does not exist.
pub async fn patch_vehicle(
    State(state): State<AppState>,
    Path((uid, plate)): Path<(String, String)>,
    Json(mut body): Json<PatchVehicleBody>,
) -> ApiResult {
    if let Some(hit_points) = &mut body.hit_points {
        hit_points.normalize();
        hit_points.validate().map_err(|error| {
            err(
                StatusCode::BAD_REQUEST,
                format!("Validation failed: {error}"),
            )
        })?;
    }
    if let Some(damage) = body.damage {
        if !(0.0..=1.0).contains(&damage) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "Damage must be between 0.0 and 1.0",
            ));
        }
    }
    if let Some(fuel) = body.fuel {
        if !(0.0..=1.0).contains(&fuel) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "Fuel must be between 0.0 and 1.0",
            ));
        }
    }

    let db = db_or_503(&state).await?;
    let mut garage = load_garage(&db, &uid).await?;
    let Some(vehicle) = garage.vehicles.get_mut(&plate) else {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Vehicle with plate {plate} not found"),
        ));
    };
    if let Some(damage) = body.damage {
        vehicle.damage = damage;
    }
    if let Some(fuel) = body.fuel {
        vehicle.fuel = fuel;
    }
    if let Some(hit_points) = body.hit_points {
        vehicle.hit_points = hit_points;
    }
    let saved = save_garage(&db, &garage).await?;
    Ok((StatusCode::OK, Json(json!(saved))))
}

/// DELETE /players/{uid}/garage/vehicles/{plate} — remove a stored
/// vehicle (original `remove_vehicle`). 404 when the garage or the
/// vehicle does not exist.
pub async fn remove_vehicle(
    State(state): State<AppState>,
    Path((uid, plate)): Path<(String, String)>,
) -> ApiResult {
    let db = db_or_503(&state).await?;
    let mut garage = load_garage(&db, &uid).await?;
    if garage.vehicles.remove(&plate).is_none() {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Vehicle with plate '{plate}' not found in garage"),
        ));
    }
    let saved = save_garage(&db, &garage).await?;
    Ok((StatusCode::OK, Json(json!(saved))))
}

// ---------------------------------------------------------------------------
// Virtual garage (unlocked vehicle classnames)
// ---------------------------------------------------------------------------

/// A player's virtual garage — the original's `VGarage`: unlocked
/// vehicle classnames by category.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VGarage {
    /// SurrealDB record id — internal only, never part of the API shape.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    pub id: Option<Value>,
    pub uid: String,
    #[serde(default)]
    pub cars: Vec<String>,
    #[serde(default)]
    pub armor: Vec<String>,
    #[serde(default)]
    pub helis: Vec<String>,
    #[serde(default)]
    pub planes: Vec<String>,
    #[serde(default)]
    pub naval: Vec<String>,
    #[serde(default)]
    pub other: Vec<String>,
}

impl VGarage {
    fn category_mut(&mut self, category: &str) -> Option<&mut Vec<String>> {
        match category {
            "cars" => Some(&mut self.cars),
            "armor" => Some(&mut self.armor),
            "helis" => Some(&mut self.helis),
            "planes" => Some(&mut self.planes),
            "naval" => Some(&mut self.naval),
            "other" => Some(&mut self.other),
            _ => None,
        }
    }
}

/// Vehicle categories — the original's `VehicleCategory`, plus the
/// `"heli"` alias the original's `get_garage` accepts.
pub fn parse_vehicle_category(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "cars" => Some("cars"),
        "armor" => Some("armor"),
        "helis" | "heli" => Some("helis"),
        "planes" => Some("planes"),
        "naval" => Some("naval"),
        "other" => Some("other"),
        _ => None,
    }
}

pub const VEHICLE_CATEGORIES: &str = "cars, armor, helis, planes, naval, other";

/// Body for `POST /players/{uid}/vgarage/{category}` — unlock
/// classnames (original `add_garage`).
#[derive(Debug, Deserialize)]
pub struct AddClassnamesBody {
    pub classnames: Vec<String>,
}

async fn load_vgarage(db: &super::Db, uid: &str) -> Result<VGarage, (StatusCode, Json<Value>)> {
    load_by_field(db, VGARAGE_TABLE, "uid", uid, "Virtual garage").await
}

async fn save_vgarage(
    db: &super::Db,
    vgarage: &VGarage,
) -> Result<VGarage, (StatusCode, Json<Value>)> {
    let content = to_content(vgarage)?;
    update_record_content(db, VGARAGE_TABLE, "uid", &vgarage.uid, content).await
}

async fn load_or_create_vgarage(
    db: &super::Db,
    uid: &str,
) -> Result<VGarage, (StatusCode, Json<Value>)> {
    let existing: Vec<VGarage> = select_by_field(db, VGARAGE_TABLE, "uid", uid).await?;
    if let Some(vgarage) = existing.into_iter().next() {
        return Ok(vgarage);
    }
    let vgarage = VGarage {
        uid: uid.to_string(),
        ..Default::default()
    };
    create_record(db, VGARAGE_TABLE, &vgarage).await
}

/// POST /players/{uid}/vgarage — create an empty virtual garage
/// (original `VGarageService::create_garage`). 409 when it exists.
pub async fn create_vgarage(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let uid = uid.trim().to_string();
    if uid.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "UID cannot be empty"));
    }
    let db = db_or_503(&state).await?;
    let existing: Vec<VGarage> = select_by_field(&db, VGARAGE_TABLE, "uid", &uid).await?;
    if !existing.is_empty() {
        return Err(err(
            StatusCode::CONFLICT,
            format!("Virtual garage for '{uid}' already exists"),
        ));
    }
    let vgarage = VGarage {
        uid,
        ..Default::default()
    };
    let created = create_record(&db, VGARAGE_TABLE, &vgarage).await?;
    Ok((StatusCode::CREATED, Json(json!(created))))
}

/// GET /players/{uid}/vgarage — one player's unlocked vehicle
/// classnames (original `fetch_garage`). 404 when none exists yet.
pub async fn get_vgarage(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    let vgarage = load_vgarage(&db, &uid).await?;
    Ok((StatusCode::OK, Json(json!(vgarage))))
}

/// GET /players/{uid}/vgarage/{category} — the unlocked classnames of
/// one category (original `get_garage` with a field).
pub async fn get_vgarage_category(
    State(state): State<AppState>,
    Path((uid, category)): Path<(String, String)>,
) -> ApiResult {
    let category = parse_vehicle_category(&category).ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Vehicle category '{category}' is unsupported. Valid categories: {VEHICLE_CATEGORIES}."),
        )
    })?;
    let db = db_or_503(&state).await?;
    let vgarage = load_vgarage(&db, &uid).await?;
    let classnames = match category {
        "cars" => &vgarage.cars,
        "armor" => &vgarage.armor,
        "helis" => &vgarage.helis,
        "planes" => &vgarage.planes,
        "naval" => &vgarage.naval,
        _ => &vgarage.other,
    };
    Ok((
        StatusCode::OK,
        Json(json!({ "category": category, "classnames": classnames })),
    ))
}

/// POST /players/{uid}/vgarage/{category} — unlock classnames
/// (original `add_garage`). Classnames already unlocked are left alone.
/// The record is created empty first when it does not exist, as the
/// original's hot service does.
pub async fn add_vgarage_classnames(
    State(state): State<AppState>,
    Path((uid, category)): Path<(String, String)>,
    Json(body): Json<AddClassnamesBody>,
) -> ApiResult {
    let category = parse_vehicle_category(&category).ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Vehicle category '{category}' is unsupported. Valid categories: {VEHICLE_CATEGORIES}."),
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
    let mut vgarage = load_or_create_vgarage(&db, &uid).await?;
    let Some(list) = vgarage.category_mut(category) else {
        return Err(err(StatusCode::BAD_REQUEST, "Unknown vehicle category."));
    };
    for classname in classnames {
        if !list.iter().any(|entry| entry == &classname) {
            list.push(classname);
        }
    }
    let saved = save_vgarage(&db, &vgarage).await?;
    Ok((StatusCode::OK, Json(json!(saved))))
}

/// DELETE /players/{uid}/vgarage/{category}/{classname} — remove one
/// unlocked classname (original `remove_garage`). 404 when the record,
/// category or classname does not exist.
pub async fn remove_vgarage_classname(
    State(state): State<AppState>,
    Path((uid, category, classname)): Path<(String, String, String)>,
) -> ApiResult {
    let category = parse_vehicle_category(&category).ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Vehicle category '{category}' is unsupported. Valid categories: {VEHICLE_CATEGORIES}."),
        )
    })?;

    let db = db_or_503(&state).await?;
    let mut vgarage = load_vgarage(&db, &uid).await?;
    let Some(list) = vgarage.category_mut(category) else {
        return Err(err(StatusCode::BAD_REQUEST, "Unknown vehicle category."));
    };
    let Some(position) = list.iter().position(|entry| entry == &classname) else {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Classname '{classname}' not found in virtual garage."),
        ));
    };
    list.remove(position);
    let saved = save_vgarage(&db, &vgarage).await?;
    Ok((StatusCode::OK, Json(json!(saved))))
}

/// Shared by the store checkout: load a virtual garage, creating an
/// empty one when it does not exist (the original's checkout fetches
/// through the hot service, which auto-creates).
pub async fn load_or_create_vgarage_for_checkout(
    db: &super::Db,
    uid: &str,
) -> Result<VGarage, (StatusCode, Json<Value>)> {
    load_or_create_vgarage(db, uid).await
}

/// Shared by the store checkout: persist an already-mutated virtual
/// garage.
pub async fn save_vgarage_for_checkout(
    db: &super::Db,
    vgarage: &VGarage,
) -> Result<VGarage, (StatusCode, Json<Value>)> {
    save_vgarage(db, vgarage).await
}
