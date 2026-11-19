//! Store checkout: one purchase across bank, locker, virtual locker,
//! virtual garage and org.
//!
//! Grounded in `lib/services/src/store.rs` (`StoreService::checkout`)
//! and `lib/models/src/store.rs` (`StoreCheckoutContext`,
//! `StoreCheckoutResult`, `StoreGrantedItem`, `StoreGrantedVehicle`,
//! `StoreGrantedUnit`), with the extension command in
//! `arma/server/extension/src/store.rs` (`checkout`).
//!
//! The original has **no store catalog and no sell operation**: the
//! caller (the store web UI / bridge) sends the cart as seeds that
//! already carry their price (`priceValue`), and checkout only *buys*.
//! The catalog itself lives in the client's store config
//! (`arma/client/CfgStore.hpp`); the API deliberately does not invent
//! one. A sell endpoint would be new behavior, not a port, so there is
//! none here.
//!
//! What checkout does, exactly as in the original:
//!
//! - Totals the cart (`floor` of the sum of `max(0, price) * quantity`
//!   for items plus `max(0, price)` for vehicles and units).
//! - Items go into the player's **locker** (amounts add onto what is
//!   already there) and unlock their classname in the **virtual
//!   locker** (arsenal); vehicles unlock their classname in the
//!   **virtual garage**; units are only *reported* as granted — the
//!   original has no unit storage to put them in.
//! - Payment is `cash` or `bank` (the player's account), `org_funds`
//!   (leader/CEO only; vehicles bought with org funds also join the
//!   org's **fleet**), or `credit_line` (the requester's assigned line).
//! - The locker may not grow past 25 unique items.
//! - Commit order is locker → virtual locker → virtual garage → org →
//!   bank, and on any failure the original rolls back what was already
//!   written, in reverse order. The API does the same best-effort
//!   rollback (SurrealDB multi-record transactions are not used here,
//!   mirroring the original's manual compensation).

use std::collections::HashMap;

use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{bank, db_or_503, err, garage, locker, org, ApiResult};
use crate::AppState;

/// One item seed — the original's `StoreCheckoutItemSeed`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutItemSeed {
    pub classname: String,
    pub category: String,
    pub price_value: f64,
    pub quantity: u32,
}

/// One vehicle seed — the original's `StoreCheckoutVehicleSeed`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutVehicleSeed {
    pub classname: String,
    pub category: String,
    pub price_value: f64,
}

/// One unit seed — the original's `StoreCheckoutUnitSeed`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutUnitSeed {
    pub classname: String,
    pub category: String,
    pub price_value: f64,
}

/// The checkout request — the original's `StoreCheckoutContext`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutBody {
    pub requester_uid: String,
    // Carried for the original's `StoreCheckoutContext` shape; the
    // checkout itself never uses the requester name.
    #[serde(default)]
    #[allow(dead_code)]
    pub requester_name: String,
    #[serde(default)]
    pub org_id: String,
    #[serde(default)]
    pub requester_is_default_org_ceo: bool,
    pub payment_method: String,
    #[serde(default)]
    pub items: Vec<CheckoutItemSeed>,
    #[serde(default)]
    pub vehicles: Vec<CheckoutVehicleSeed>,
    #[serde(default)]
    pub units: Vec<CheckoutUnitSeed>,
}

/// A granted item — the original's `StoreGrantedItem`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantedItem {
    pub classname: String,
    pub category: String,
    pub quantity: u32,
}

/// A granted vehicle — the original's `StoreGrantedVehicle`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantedVehicle {
    pub classname: String,
    pub category: String,
}

/// A granted unit — the original's `StoreGrantedUnit`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantedUnit {
    pub classname: String,
    pub category: String,
}

fn checkout_total(body: &CheckoutBody) -> f64 {
    let item_total: f64 = body
        .items
        .iter()
        .map(|entry| entry.price_value.max(0.0) * f64::from(entry.quantity))
        .sum();
    let vehicle_total: f64 = body
        .vehicles
        .iter()
        .map(|entry| entry.price_value.max(0.0))
        .sum();
    let unit_total: f64 = body
        .units
        .iter()
        .map(|entry| entry.price_value.max(0.0))
        .sum();
    (item_total + vehicle_total + unit_total).floor()
}

/// The original's `resolve_locker_category`: item/attachment → `item`,
/// weapon → `weapon`, magazine → `magazine`, backpack → `backpack`.
fn resolve_locker_category(category: &str) -> Result<&'static str, String> {
    match category.trim().to_ascii_lowercase().as_str() {
        "item" | "attachment" => Ok("item"),
        "weapon" => Ok("weapon"),
        "magazine" => Ok("magazine"),
        "backpack" => Ok("backpack"),
        other => Err(format!("Store item category '{other}' is unsupported.")),
    }
}

/// The original's `resolve_arsenal_category`: the virtual-locker
/// category an item unlocks.
fn resolve_arsenal_category(category: &str) -> Result<&'static str, String> {
    match category.trim().to_ascii_lowercase().as_str() {
        "item" | "attachment" => Ok("items"),
        "weapon" => Ok("weapons"),
        "magazine" => Ok("magazines"),
        "backpack" => Ok("backpacks"),
        other => Err(format!("Store item category '{other}' is unsupported.")),
    }
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|entry| entry == value) {
        values.push(value.to_string());
    }
}

fn format_currency(amount: f64) -> String {
    let rounded = amount.max(0.0).round() as i64;
    let digits = rounded.to_string();
    let mut formatted = String::new();
    for (index, character) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            formatted.push(',');
        }
        formatted.push(character);
    }
    format!("${}", formatted.chars().rev().collect::<String>())
}

/// POST /store/checkout — buy a cart (original
/// `StoreService::checkout`). Charges the chosen payment source and
/// grants the items / vehicle unlocks / unit grants. All seed and
/// category validation runs before any database access.
pub async fn checkout(State(state): State<AppState>, Json(body): Json<CheckoutBody>) -> ApiResult {
    let requester_uid = body.requester_uid.trim().to_string();
    if requester_uid.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "A valid requester UID is required.",
        ));
    }
    if body.items.is_empty() && body.vehicles.is_empty() && body.units.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Add at least one item before checkout.",
        ));
    }
    let charged_total = checkout_total(&body);
    if charged_total <= 0.0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Checkout total must be greater than zero.",
        ));
    }
    let payment_method = body.payment_method.trim().to_ascii_lowercase();
    match payment_method.as_str() {
        "cash" | "bank" | "org_funds" | "credit_line" => {}
        _ => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "Selected payment source is unsupported.",
            ));
        }
    }
    if (payment_method == "org_funds" || payment_method == "credit_line")
        && body.org_id.trim().is_empty()
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "A valid organization is required for this checkout.",
        ));
    }
    // Validate every seed (classname, quantity, category) before
    // touching the database, as the API's validation-first convention
    // requires.
    for item in &body.items {
        if item.classname.trim().is_empty() || item.quantity == 0 {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "Checkout contains an invalid item entry.",
            ));
        }
        resolve_locker_category(&item.category)
            .map_err(|error| err(StatusCode::BAD_REQUEST, error))?;
        resolve_arsenal_category(&item.category)
            .map_err(|error| err(StatusCode::BAD_REQUEST, error))?;
    }
    for vehicle in &body.vehicles {
        if vehicle.classname.trim().is_empty() {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "Vehicle checkout entry was missing a classname.",
            ));
        }
        garage::parse_vehicle_category(&vehicle.category).ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                format!("Vehicle category '{}' is unsupported.", vehicle.category),
            )
        })?;
    }
    for unit in &body.units {
        if unit.classname.trim().is_empty() {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "Unit checkout entry was missing a classname.",
            ));
        }
        let unit_category = unit.category.trim().to_ascii_lowercase();
        if unit_category != "units" && unit_category != "unit" {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("Unit category '{}' is unsupported.", unit_category),
            ));
        }
    }

    let db = db_or_503(&state).await?;

    // Load the originals. The bank must already exist (the original's
    // hot bank lookup does not auto-create); the locker, virtual locker
    // and virtual garage auto-create empty, as the original's hot
    // services do.
    let original_bank = bank::load_account(&db, &requester_uid).await?;
    let original_locker = locker::load_or_create_locker_for_checkout(&db, &requester_uid).await?;
    let original_vlocker = locker::load_or_create_vlocker_for_checkout(&db, &requester_uid).await?;
    let original_vgarage = garage::load_or_create_vgarage_for_checkout(&db, &requester_uid).await?;

    let mut next_locker = original_locker.clone();
    let mut next_vlocker = original_vlocker.clone();
    let mut next_vgarage = original_vgarage.clone();

    let mut locker_patch: HashMap<String, Value> = HashMap::new();
    let mut va_patch: HashMap<String, Value> = HashMap::new();
    let mut vgarage_patch: HashMap<String, Value> = HashMap::new();
    let mut locker_granted: Vec<GrantedItem> = Vec::new();
    let mut vehicle_granted: Vec<GrantedVehicle> = Vec::new();
    let mut unit_granted: Vec<GrantedUnit> = Vec::new();
    let mut va_categories_changed: Vec<&str> = Vec::new();
    let mut vgarage_categories_changed: Vec<&str> = Vec::new();

    for item_seed in &body.items {
        let locker_category = resolve_locker_category(&item_seed.category)
            .map_err(|error| err(StatusCode::BAD_REQUEST, error))?;
        let arsenal_category = resolve_arsenal_category(&item_seed.category)
            .map_err(|error| err(StatusCode::BAD_REQUEST, error))?;

        let existing_amount = next_locker
            .items
            .get(&item_seed.classname)
            .map(|entry| entry.amount)
            .unwrap_or(0);
        let updated_item = locker::Item {
            category: locker_category.to_string(),
            classname: item_seed.classname.clone(),
            amount: existing_amount.saturating_add(item_seed.quantity),
        };
        next_locker
            .items
            .insert(item_seed.classname.clone(), updated_item.clone());
        locker_patch.insert(
            item_seed.classname.clone(),
            serde_json::to_value(&updated_item).map_err(|error| {
                err(
                    StatusCode::BAD_REQUEST,
                    format!("Failed to serialize locker patch: {error}"),
                )
            })?,
        );
        locker_granted.push(GrantedItem {
            classname: item_seed.classname.clone(),
            category: locker_category.to_string(),
            quantity: item_seed.quantity,
        });

        match arsenal_category {
            "items" => {
                push_unique(&mut next_vlocker.items, &item_seed.classname);
                if !va_categories_changed.contains(&"items") {
                    va_categories_changed.push("items");
                }
            }
            "weapons" => {
                push_unique(&mut next_vlocker.weapons, &item_seed.classname);
                if !va_categories_changed.contains(&"weapons") {
                    va_categories_changed.push("weapons");
                }
            }
            "magazines" => {
                push_unique(&mut next_vlocker.magazines, &item_seed.classname);
                if !va_categories_changed.contains(&"magazines") {
                    va_categories_changed.push("magazines");
                }
            }
            _ => {
                push_unique(&mut next_vlocker.backpacks, &item_seed.classname);
                if !va_categories_changed.contains(&"backpacks") {
                    va_categories_changed.push("backpacks");
                }
            }
        }
    }

    if next_locker.items.len() > locker::MAX_ITEMS {
        return Err(err(
            StatusCode::CONFLICT,
            "Locker capacity would exceed 25 unique items. Clear space before checkout.",
        ));
    }

    for category in va_categories_changed {
        let value = match category {
            "items" => json!(next_vlocker.items),
            "weapons" => json!(next_vlocker.weapons),
            "magazines" => json!(next_vlocker.magazines),
            _ => json!(next_vlocker.backpacks),
        };
        va_patch.insert(category.to_string(), value);
    }

    for vehicle_seed in &body.vehicles {
        let vehicle_category =
            garage::parse_vehicle_category(&vehicle_seed.category).ok_or_else(|| {
                err(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "Vehicle category '{}' is unsupported.",
                        vehicle_seed.category
                    ),
                )
            })?;
        match vehicle_category {
            "cars" => {
                push_unique(&mut next_vgarage.cars, &vehicle_seed.classname);
                if !vgarage_categories_changed.contains(&"cars") {
                    vgarage_categories_changed.push("cars");
                }
            }
            "armor" => {
                push_unique(&mut next_vgarage.armor, &vehicle_seed.classname);
                if !vgarage_categories_changed.contains(&"armor") {
                    vgarage_categories_changed.push("armor");
                }
            }
            "helis" => {
                push_unique(&mut next_vgarage.helis, &vehicle_seed.classname);
                if !vgarage_categories_changed.contains(&"helis") {
                    vgarage_categories_changed.push("helis");
                }
            }
            "planes" => {
                push_unique(&mut next_vgarage.planes, &vehicle_seed.classname);
                if !vgarage_categories_changed.contains(&"planes") {
                    vgarage_categories_changed.push("planes");
                }
            }
            "naval" => {
                push_unique(&mut next_vgarage.naval, &vehicle_seed.classname);
                if !vgarage_categories_changed.contains(&"naval") {
                    vgarage_categories_changed.push("naval");
                }
            }
            _ => {
                push_unique(&mut next_vgarage.other, &vehicle_seed.classname);
                if !vgarage_categories_changed.contains(&"other") {
                    vgarage_categories_changed.push("other");
                }
            }
        }
        vehicle_granted.push(GrantedVehicle {
            classname: vehicle_seed.classname.clone(),
            category: vehicle_seed.category.clone(),
        });
    }

    for unit_seed in &body.units {
        // Units are reported as granted only — the original has no unit
        // storage to persist them into.
        unit_granted.push(GrantedUnit {
            classname: unit_seed.classname.clone(),
            category: "units".to_string(),
        });
    }

    for category in vgarage_categories_changed {
        let value = match category {
            "cars" => json!(next_vgarage.cars),
            "armor" => json!(next_vgarage.armor),
            "helis" => json!(next_vgarage.helis),
            "planes" => json!(next_vgarage.planes),
            "naval" => json!(next_vgarage.naval),
            _ => json!(next_vgarage.other),
        };
        vgarage_patch.insert(category.to_string(), value);
    }

    // Payment: compute the final bank / org in memory. The bank charge
    // is the original's `charge_checkout` preview (commit: false);
    // the org charge is the original's `charge_checkout` on the org.
    let mut bank_patch: HashMap<String, Value> = HashMap::new();
    let mut final_bank: Option<bank::Bank> = None;
    let mut org_patch: HashMap<String, Value> = HashMap::new();
    let mut org_target_uids: Vec<String> = Vec::new();
    let mut final_org: Option<org::OrgRecord> = None;
    let mut original_org: Option<org::OrgRecord> = None;

    match payment_method.as_str() {
        "cash" | "bank" => {
            let source_field = payment_method.as_str();
            let source_balance = match source_field {
                "cash" => original_bank.cash,
                _ => original_bank.bank,
            };
            if source_balance < charged_total {
                return Err(err(
                    StatusCode::CONFLICT,
                    match source_field {
                        "cash" => "Cash on hand cannot cover this checkout.",
                        _ => "Bank balance cannot cover this checkout.",
                    },
                ));
            }
            let mut charged = original_bank.clone();
            match source_field {
                "cash" => charged.cash -= charged_total,
                _ => charged.bank -= charged_total,
            }
            charged.validate().map_err(|error| {
                err(
                    StatusCode::BAD_REQUEST,
                    format!("Validation failed: {error}"),
                )
            })?;
            bank_patch = bank::account_patch(&charged, source_field);
            final_bank = Some(charged);
        }
        "org_funds" | "credit_line" => {
            let org_id = body.org_id.trim().to_string();
            let mut org_record = org::load_org_for_checkout(&db, &org_id).await?;
            original_org = Some(org_record.clone());
            let charge_body = org::ChargeOrgBody {
                requester_uid: requester_uid.clone(),
                requester_is_default_org_ceo: body.requester_is_default_org_ceo,
                allow_member_charge: false,
                record_member_debt: false,
                source: payment_method.clone(),
                amount: charged_total,
            };
            let fields = org::apply_org_charge(&mut org_record, &charge_body)?;
            // The charge returns the patch fields that changed; the
            // store result rebuilds the patch from the mutated record
            // (plus `fleet` when vehicles were bought — added below).
            let _ = fields;
            org_patch = org_patch_fields(&org_record, &payment_method)?;
            if payment_method == "org_funds" && !body.vehicles.is_empty() {
                add_org_fleet_vehicles(&mut org_record, &body.vehicles);
                org_patch.insert("fleet".to_string(), json!(org_record.fleet));
            }
            org_target_uids = org::org_member_uids(&org_record, Some(&requester_uid));
            final_org = Some(org_record);
        }
        _ => unreachable!(),
    }

    // Commit, in the original's order (locker → virtual locker →
    // virtual garage → org → bank), rolling back what was already
    // written if any step fails.
    let mut locker_saved = false;
    let mut vlocker_saved = false;
    let mut vgarage_saved = false;
    let mut org_saved = false;
    let mut bank_saved = false;

    let commit_result: Result<(), (StatusCode, Json<Value>)> = (async {
        if !locker_patch.is_empty() {
            locker::save_locker_for_checkout(&db, &next_locker).await?;
            locker_saved = true;
        }
        if !va_patch.is_empty() {
            locker::save_vlocker_for_checkout(&db, &next_vlocker).await?;
            vlocker_saved = true;
        }
        if !vgarage_patch.is_empty() {
            garage::save_vgarage_for_checkout(&db, &next_vgarage).await?;
            vgarage_saved = true;
        }
        if let Some(org_record) = final_org.clone() {
            org::save_org_for_checkout(&db, &org_record).await?;
            org_saved = true;
        }
        if let Some(bank_record) = final_bank.as_ref() {
            bank::save_account(&db, bank_record).await?;
            bank_saved = true;
        }
        Ok(())
    })
    .await;

    if let Err(error) = commit_result {
        if org_saved {
            if let Some(org_record) = original_org {
                let _ = org::save_org_for_checkout(&db, &org_record).await;
            }
        }
        if vgarage_saved {
            let _ = garage::save_vgarage_for_checkout(&db, &original_vgarage).await;
        }
        if vlocker_saved {
            let _ = locker::save_vlocker_for_checkout(&db, &original_vlocker).await;
        }
        if locker_saved {
            let _ = locker::save_locker_for_checkout(&db, &original_locker).await;
        }
        if bank_saved {
            let _ = bank::save_account(&db, &original_bank).await;
        }
        return Err(error);
    }

    Ok((
        StatusCode::OK,
        Json(json!({
            "charged_total": charged_total,
            "payment_method": payment_method,
            "message": format!(
                "Checkout completed. {} charged, {} locker grant(s), {} vehicle unlock(s), {} unit grant(s).",
                format_currency(charged_total),
                locker_granted.len(),
                vehicle_granted.len(),
                unit_granted.len()
            ),
            "locker_granted": locker_granted,
            "vehicle_granted": vehicle_granted,
            "unit_granted": unit_granted,
            "locker_patch": locker_patch,
            "va_patch": va_patch,
            "vgarage_patch": vgarage_patch,
            "bank_patch": bank_patch,
            "org_patch": org_patch,
            "org_target_uids": org_target_uids,
        })),
    ))
}

/// The patch fields the org charge changed, mirroring the original's
/// `build_org_patch` call in the store service: `funds` for org-funds
/// payments (plus `fleet` when vehicles were bought — added by the
/// caller), `credit_lines` for credit-line payments.
fn org_patch_fields(
    org_record: &org::OrgRecord,
    payment_method: &str,
) -> Result<HashMap<String, Value>, (StatusCode, Json<Value>)> {
    let mut patch = HashMap::new();
    match payment_method {
        "org_funds" => {
            patch.insert("funds".to_string(), json!(org_record.funds));
        }
        "credit_line" => {
            patch.insert(
                "credit_lines".to_string(),
                serde_json::to_value(&org_record.credit_lines).map_err(|error| {
                    err(
                        StatusCode::BAD_REQUEST,
                        format!("Failed to serialize org credit lines: {error}"),
                    )
                })?,
            );
        }
        _ => {}
    }
    Ok(patch)
}

/// The original's `add_org_fleet_vehicles` (store service): vehicles
/// bought with org funds join the org fleet, keyed
/// `{classname}_{index}`, status `"Ready"`, damage `"0%"`.
fn add_org_fleet_vehicles(org_record: &mut org::OrgRecord, vehicles: &[CheckoutVehicleSeed]) {
    let mut fleet_index = org_record.fleet.len();
    for vehicle in vehicles {
        if vehicle.classname.trim().is_empty() {
            continue;
        }
        let fleet_type = vehicle.category.trim().to_ascii_lowercase();
        let mut fleet_key = format!("{}_{}", vehicle.classname, fleet_index);
        while org_record.fleet.contains_key(&fleet_key) {
            fleet_index += 1;
            fleet_key = format!("{}_{}", vehicle.classname, fleet_index);
        }
        org_record.fleet.insert(
            fleet_key,
            org::OrgFleetEntry {
                classname: vehicle.classname.clone(),
                name: vehicle.classname.clone(),
                fleet_type,
                status: "Ready".to_string(),
                damage: "0%".to_string(),
            },
        );
        fleet_index += 1;
    }
}
