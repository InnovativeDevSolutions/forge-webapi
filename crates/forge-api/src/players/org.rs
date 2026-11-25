//! Organizations: registration, membership, invites, assets, fleet,
//! credit lines, treasury.
//!
//! Grounded in `lib/services/src/org.rs` (`OrgService` +
//! `OrgHotStateService`) and `lib/models/src/org.rs` (`Org`,
//! `HotOrgRecord`, `CreditLineSummary`, `OrgAssetEntry`,
//! `OrgFleetEntry`, `MemberSummary`, `OrgInviteRecord`, and the
//! `Org*Context` / `Org*Result` types), with the extension commands in
//! `arma/server/extension/src/org.rs`.
//!
//! The original's rules, mirrored here:
//!
//! - Every player starts in the **`default`** organization. Registering
//!   a new org moves the requester out of `default`; leaving an org
//!   moves them back into `default`; disbanding moves every member into
//!   `default`. The original hydrates `default` from its cold store; the
//!   API auto-creates it on first use (owner `"server"`, name
//!   `"Default"`) — a new org record is otherwise indistinguishable.
//! - Treasury actions (invites, credit lines, org-funds charges) are
//!   for the org **owner**, or — for the `default` org / server-owned
//!   orgs — a caller flagged `requesterIsDefaultOrgCeo`
//!   (`can_manage_treasury`).
//! - An org id must be alphanumeric plus underscores, the name at most
//!   100 characters (`Org::validate`). The original also requires the
//!   owner to be a 17-digit Steam ID or `"server"`; the API does **not**
//!   enforce that, so non-Steam callers (tests, bridges) can register
//!   orgs — noted as a deliberate relaxation.
//! - Credit lines carry a 10% default interest rate
//!   (`DEFAULT_CREDIT_LINE_INTEREST_RATE`); amounts are rounded to
//!   cents (`round_currency`). Assigning a line reserves the amount
//!   from org funds; repaying splits the payment between principal and
//!   interest proportionally.
//! - Charging `org_funds` with `recordMemberDebt` records the charge as
//!   member debt at face value; charging a `credit_line` adds interest
//!   to the amount due (`amount * (1 + rate)`) — the two paths really do
//!   differ in the original, and the API keeps them different.
//! - Fleet vehicles are keyed `{classname}_{index}` with status
//!   `"Ready"` and damage `"0%"`.
//! - `POST /orgs/{id}/reputation` mirrors the org side of the task
//!   reward service
//!   (`arma/server/addons/task/functions/objects/fnc_TaskRewardService.sqf`,
//!   `applyRatingOutcome`): a successful contract adds the rating delta
//!   to the owning org's reputation. Reputation is clamped at zero
//!   (the original's `Org::validate` rejects negative reputation).
//!
//! JSON uses camelCase (`creditLines`, `pendingInvites`, …), matching
//! the rest of this API; the original's `orgID` serializes here as
//! `orgId`, consistent with the CAD slice's `Group.orgId`.

use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{
    create_record, db_or_503, delete_by_field, err, load_by_field, round_currency, select_all,
    select_by_field, to_content, update_record_content, ApiResult,
};
use crate::AppState;

/// SurrealDB table for organizations (the original's hot org record).
pub const ORG_TABLE: &str = "org";

/// The default organization every player starts in.
pub const DEFAULT_ORG_ID: &str = "default";

/// Default credit-line interest rate — the original's
/// `DEFAULT_CREDIT_LINE_INTEREST_RATE`.
pub const DEFAULT_CREDIT_LINE_INTEREST_RATE: f64 = 0.10;

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// One member's credit line — the original's `CreditLineSummary`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreditLineSummary {
    pub uid: String,
    pub name: String,
    #[serde(default)]
    pub approved_amount: f64,
    #[serde(default)]
    pub available_amount: f64,
    #[serde(default)]
    pub outstanding_principal: f64,
    #[serde(default = "default_credit_line_interest_rate")]
    pub interest_rate: f64,
    #[serde(default)]
    pub amount_due: f64,
    #[serde(default)]
    pub amount: f64,
}

fn default_credit_line_interest_rate() -> f64 {
    DEFAULT_CREDIT_LINE_INTEREST_RATE
}

impl CreditLineSummary {
    fn new(uid: String, name: String) -> Self {
        Self {
            uid,
            name,
            approved_amount: 0.0,
            available_amount: 0.0,
            outstanding_principal: 0.0,
            interest_rate: DEFAULT_CREDIT_LINE_INTEREST_RATE,
            amount_due: 0.0,
            amount: 0.0,
        }
    }

    /// The original's `CreditLineSummary::normalize`: round to cents,
    /// backfill approved/available from the legacy `amount`, default the
    /// interest rate, and derive `amount_due` from the outstanding
    /// principal when it is missing.
    fn normalize(&mut self) {
        let legacy_amount = round_currency(self.amount);
        self.approved_amount = round_currency(self.approved_amount);
        self.available_amount = round_currency(self.available_amount);
        self.outstanding_principal = round_currency(self.outstanding_principal);
        self.amount_due = round_currency(self.amount_due);

        if self.approved_amount <= 0.0 && self.available_amount <= 0.0 && legacy_amount > 0.0 {
            self.approved_amount = legacy_amount;
            self.available_amount = legacy_amount;
        } else if self.approved_amount <= 0.0 && self.available_amount > 0.0 {
            self.approved_amount = self.available_amount;
        } else if self.available_amount <= 0.0 && self.approved_amount > 0.0 {
            self.available_amount = self.approved_amount;
        }

        if self.interest_rate <= 0.0 {
            self.interest_rate = DEFAULT_CREDIT_LINE_INTEREST_RATE;
        }

        if self.amount_due <= 0.0 && self.outstanding_principal > 0.0 {
            self.amount_due =
                round_currency(self.outstanding_principal * (1.0 + self.interest_rate));
        }

        self.amount = self.available_amount;
    }
}

/// One org asset entry — the original's `OrgAssetEntry`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgAssetEntry {
    pub classname: String,
    #[serde(rename = "type")]
    pub asset_type: String,
    pub quantity: i64,
}

/// One org fleet entry — the original's `OrgFleetEntry`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgFleetEntry {
    pub classname: String,
    pub name: String,
    #[serde(rename = "type")]
    pub fleet_type: String,
    pub status: String,
    pub damage: String,
}

/// One org member — the original's `MemberSummary`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberSummary {
    pub uid: String,
    pub name: String,
}

/// One pending invite — the original's `OrgInviteRecord`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgInviteRecord {
    pub org_id: String,
    pub org_name: String,
    pub inviter_uid: String,
    pub inviter_name: String,
    pub target_uid: String,
    pub target_name: String,
}

/// An organization — the original's `HotOrgRecord`: the durable core
/// (`Org`: id, owner, name, funds, reputation, credit lines) plus the
/// hot-only collections (assets, fleet, members, pending invites). The
/// API persists the whole record, so the hot/cold split disappears.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgRecord {
    /// SurrealDB record id — internal only, never part of the API shape.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    pub id: Option<Value>,
    pub org_id: String,
    pub owner: String,
    pub name: String,
    #[serde(default)]
    pub funds: f64,
    #[serde(default)]
    pub reputation: i64,
    #[serde(default)]
    pub credit_lines: HashMap<String, CreditLineSummary>,
    #[serde(default)]
    pub assets: HashMap<String, HashMap<String, OrgAssetEntry>>,
    #[serde(default)]
    pub fleet: HashMap<String, OrgFleetEntry>,
    #[serde(default)]
    pub members: HashMap<String, MemberSummary>,
    #[serde(default)]
    pub pending_invites: HashMap<String, OrgInviteRecord>,
}

impl OrgRecord {
    /// Validation from `Org::validate` (`lib/models/src/org.rs`),
    /// minus the 17-digit-owner rule (see the module docs).
    fn validate(&self) -> Result<(), String> {
        if self.org_id.trim().is_empty() {
            return Err("Organization ID cannot be empty".to_string());
        }
        if self.owner.trim().is_empty() {
            return Err("Organization owner cannot be empty".to_string());
        }
        if self.name.trim().is_empty() {
            return Err("Organization name cannot be empty".to_string());
        }
        if self.funds < 0.0 {
            return Err("Organization funds cannot be negative".to_string());
        }
        if self.reputation < 0 {
            return Err("Organization reputation cannot be negative".to_string());
        }
        if !self.org_id.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return Err(format!(
                "Invalid organization ID '{}' - must contain only alphanumeric characters and underscores",
                self.org_id
            ));
        }
        if self.name.len() > 100 || self.name.chars().any(|c| c.is_control()) {
            return Err(format!(
                "Invalid organization name '{}' - cannot exceed 100 characters or contain control characters",
                self.name
            ));
        }
        for (uid, credit_line) in &self.credit_lines {
            let resolved_uid = if credit_line.uid.trim().is_empty() {
                uid
            } else {
                &credit_line.uid
            };
            if credit_line.approved_amount < 0.0
                || credit_line.available_amount < 0.0
                || credit_line.outstanding_principal < 0.0
                || credit_line.amount_due < 0.0
                || credit_line.amount < 0.0
            {
                return Err(format!(
                    "Credit line for '{resolved_uid}' cannot be negative"
                ));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

/// Body for `POST /orgs` — the original's `OrgRegisterContext`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterOrgBody {
    pub org_id: String,
    pub org_name: String,
    pub requester_uid: String,
    #[serde(default)]
    pub requester_name: String,
    #[serde(default)]
    pub existing_org_id: String,
}

/// Body for `POST /orgs/{id}/members` — the original's
/// `OrgEnsureMemberContext`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnsureMemberBody {
    pub member_uid: String,
    #[serde(default)]
    pub member_name: String,
}

/// Body for `POST /orgs/{id}/invites` — the original's
/// `OrgInviteContext`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteBody {
    pub requester_uid: String,
    #[serde(default)]
    pub requester_name: String,
    #[serde(default)]
    pub requester_is_default_org_ceo: bool,
    pub target_uid: String,
    #[serde(default)]
    pub target_name: String,
    #[serde(default)]
    pub target_org_id: String,
}

/// Body for invite accept / decline — the original's
/// `OrgInviteDecisionContext`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteDecisionBody {
    pub requester_uid: String,
    #[serde(default)]
    pub requester_name: String,
    #[serde(default)]
    pub existing_org_id: String,
}

/// Body for `POST /orgs/{id}/leave` and `POST /orgs/{id}/disband` —
/// the original's `OrgLeaveContext`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaveBody {
    pub requester_uid: String,
    #[serde(default)]
    pub requester_name: String,
}

/// Body for `POST /orgs/{id}/assets` — the original's
/// `OrgAssetGrantSeed` list.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddAssetsBody {
    pub assets: Vec<AssetSeed>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetSeed {
    pub classname: String,
    pub category: String,
    pub quantity: i64,
}

/// Body for `POST /orgs/{id}/fleet` — the original's
/// `OrgFleetGrantSeed` list.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddFleetBody {
    pub vehicles: Vec<FleetSeed>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetSeed {
    pub classname: String,
    pub category: String,
}

/// Body for `POST /orgs/{id}/credit-lines` — the original's
/// `OrgCreditLineContext`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignCreditLineBody {
    pub requester_uid: String,
    #[serde(default)]
    pub requester_is_default_org_ceo: bool,
    pub member_uid: String,
    #[serde(default)]
    pub member_name: String,
    pub amount: f64,
}

/// Body for `POST /orgs/{id}/credit-lines/repay` — the original's
/// `OrgCreditLineRepaymentContext`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepayCreditLineBody {
    pub requester_uid: String,
    pub amount: f64,
}

/// Body for `POST /orgs/{id}/charge` — the original's
/// `OrgCheckoutContext`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChargeOrgBody {
    pub requester_uid: String,
    #[serde(default)]
    pub requester_is_default_org_ceo: bool,
    #[serde(default)]
    pub allow_member_charge: bool,
    #[serde(default)]
    pub record_member_debt: bool,
    pub source: String,
    pub amount: f64,
}

/// Body for `POST /orgs/{id}/reputation`.
#[derive(Debug, Deserialize)]
pub struct ReputationBody {
    pub delta: i64,
}

/// Query for `GET /orgs/invites`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvitesQuery {
    pub member_uid: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn load_org(db: &super::Db, org_id: &str) -> Result<OrgRecord, (StatusCode, Json<Value>)> {
    load_by_field(db, ORG_TABLE, "orgId", org_id, "Organization").await
}

async fn save_org(db: &super::Db, org: &OrgRecord) -> Result<OrgRecord, (StatusCode, Json<Value>)> {
    let content = to_content(org)?;
    update_record_content(db, ORG_TABLE, "orgId", &org.org_id, content).await
}

/// The original's `can_manage_treasury`: the org owner, or — for the
/// default org / server-owned orgs — a caller flagged as the default
/// org's CEO.
fn can_manage_treasury(
    org: &OrgRecord,
    requester_uid: &str,
    requester_is_default_org_ceo: bool,
) -> bool {
    org.owner == requester_uid
        || ((org.org_id.eq_ignore_ascii_case(DEFAULT_ORG_ID)
            || org.owner.eq_ignore_ascii_case("server"))
            && requester_is_default_org_ceo)
}

fn resolve_member_uids(org: &OrgRecord, requester_uid: Option<&str>) -> Vec<String> {
    let mut member_uids: Vec<String> = org.members.keys().cloned().collect();
    if let Some(uid) = requester_uid {
        if !uid.is_empty() && !member_uids.iter().any(|member_uid| member_uid == uid) {
            member_uids.push(uid.to_string());
        }
    }
    member_uids
}

fn org_patch(org: &OrgRecord, fields: &[&str]) -> Result<HashMap<String, Value>, String> {
    let mut patch = HashMap::new();
    for field in fields {
        let value = match *field {
            "orgId" => json!(org.org_id),
            "owner" => json!(org.owner),
            "name" => json!(org.name),
            "funds" => json!(org.funds),
            "reputation" => json!(org.reputation),
            "credit_lines" => serde_json::to_value(&org.credit_lines)
                .map_err(|error| format!("Failed to serialize org credit lines: {error}"))?,
            "assets" => serde_json::to_value(&org.assets)
                .map_err(|error| format!("Failed to serialize org assets: {error}"))?,
            "fleet" => serde_json::to_value(&org.fleet)
                .map_err(|error| format!("Failed to serialize org fleet: {error}"))?,
            "members" => serde_json::to_value(&org.members)
                .map_err(|error| format!("Failed to serialize org members: {error}"))?,
            "pending_invites" => serde_json::to_value(&org.pending_invites)
                .map_err(|error| format!("Failed to serialize org invites: {error}"))?,
            _ => return Err(format!("Unknown field: {field}")),
        };
        patch.insert((*field).to_string(), value);
    }
    Ok(patch)
}

fn mutation_json(
    org: &OrgRecord,
    fields: &[&str],
    requester_uid: Option<&str>,
    message: &str,
) -> Result<Value, (StatusCode, Json<Value>)> {
    let patch = org_patch(org, fields).map_err(|error| err(StatusCode::BAD_REQUEST, error))?;
    Ok(json!({
        "org": org,
        "patch": patch,
        "member_uids": resolve_member_uids(org, requester_uid),
        "message": message,
    }))
}

/// Load the default org, creating it when it does not exist yet — the
/// original hydrates it from its cold store; the API's cold store is
/// the same table, so creation here is the equivalent first use.
async fn load_or_create_default_org(
    db: &super::Db,
) -> Result<OrgRecord, (StatusCode, Json<Value>)> {
    let existing: Vec<OrgRecord> = select_by_field(db, ORG_TABLE, "orgId", DEFAULT_ORG_ID).await?;
    if let Some(org) = existing.into_iter().next() {
        return Ok(org);
    }
    let org = OrgRecord {
        id: None,
        org_id: DEFAULT_ORG_ID.to_string(),
        owner: "server".to_string(),
        name: "Default".to_string(),
        funds: 0.0,
        reputation: 0,
        credit_lines: HashMap::new(),
        assets: HashMap::new(),
        fleet: HashMap::new(),
        members: HashMap::new(),
        pending_invites: HashMap::new(),
    };
    create_record(db, ORG_TABLE, &org).await
}

fn display_name(name: &str) -> String {
    if name.trim().is_empty() {
        "Unknown".to_string()
    } else {
        name.to_string()
    }
}

// ---------------------------------------------------------------------------
// Registration / reads
// ---------------------------------------------------------------------------

/// POST /orgs — register an organization (original
/// `OrgHotStateService::register_org`). The requester becomes the owner
/// and first member; a requester coming from the default org is removed
/// from it. 409 when an org with that id already exists.
pub async fn register_org(
    State(state): State<AppState>,
    Json(body): Json<RegisterOrgBody>,
) -> ApiResult {
    let org_id = body.org_id.trim().to_string();
    let requester_uid = body.requester_uid.trim().to_string();
    if requester_uid.is_empty() || org_id.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "A valid requester and organization ID are required.",
        ));
    }
    if !org_id.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!(
                "Invalid organization ID '{org_id}' - must contain only alphanumeric characters and underscores"
            ),
        ));
    }
    if body.org_name.trim().is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Organization name cannot be empty.",
        ));
    }
    if !body.existing_org_id.trim().is_empty()
        && !body.existing_org_id.eq_ignore_ascii_case(DEFAULT_ORG_ID)
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Player already belongs to an organization.",
        ));
    }

    let db = db_or_503(&state).await?;
    let existing: Vec<OrgRecord> = select_by_field(&db, ORG_TABLE, "orgId", &org_id).await?;
    if !existing.is_empty() {
        return Err(err(
            StatusCode::CONFLICT,
            "An organization already exists for this phone number.",
        ));
    }

    let mut org = OrgRecord {
        id: None,
        org_id: org_id.clone(),
        owner: requester_uid.clone(),
        name: body.org_name.clone(),
        funds: 0.0,
        reputation: 0,
        credit_lines: HashMap::new(),
        assets: HashMap::new(),
        fleet: HashMap::new(),
        members: HashMap::new(),
        pending_invites: HashMap::new(),
    };
    org.members.insert(
        requester_uid.clone(),
        MemberSummary {
            uid: requester_uid.clone(),
            name: display_name(&body.requester_name),
        },
    );
    org.validate().map_err(|error| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Validation failed: {error}"),
        )
    })?;
    let created = create_record(&db, ORG_TABLE, &org).await?;

    if body.existing_org_id.eq_ignore_ascii_case(DEFAULT_ORG_ID) {
        let mut default_org = load_or_create_default_org(&db).await?;
        if default_org.members.remove(&requester_uid).is_some() {
            save_org(&db, &default_org).await?;
        }
    }

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "org": created,
            "actor_organization": org_id,
            "message": "",
        })),
    ))
}

/// GET /orgs/{id} — one organization (original `get_org`).
pub async fn get_org(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    let org = load_org(&db, &id).await?;
    Ok((StatusCode::OK, Json(json!(org))))
}

/// PUT /orgs/{id} — replace the whole organization record (original
/// `OrgHotStateService::save_org`: the hot state persists a whole
/// `HotOrgRecord` — core, credit lines, assets, fleet, members,
/// pending invites). Creates the record when it does not exist yet.
///
/// Replace semantics: the record stored is exactly the body supplied.
/// The extension's partial updates (`update`, `update_assets`,
/// `update_fleet`) read the current record first and PUT the merged
/// whole back, so a collection the caller did not intend to replace
/// survives; only a caller that sends an empty collection clears it.
/// The API's own `add_assets` / `add_fleet_vehicles` stay add-only.
///
/// Legacy shape: the extension used to write org records with
/// snake_case `org_id`, which this `OrgRecord` (camelCase `orgId`)
/// cannot decode. The PUT deletes any such legacy-shaped record for
/// this org first, converging the table to the one camelCase record —
/// so a legacy org migrated through this endpoint never leaves a
/// duplicate behind.
pub async fn replace_org(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(mut body): Json<OrgRecord>,
) -> ApiResult {
    let id = id.trim().to_string();
    if id.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Organization ID cannot be empty",
        ));
    }
    if body.org_id.trim() != id {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!(
                "Organization ID '{}' does not match the path ID '{id}'.",
                body.org_id
            ),
        ));
    }
    body.funds = round_currency(body.funds);
    for credit_line in body.credit_lines.values_mut() {
        credit_line.normalize();
    }
    body.validate().map_err(|error| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Validation failed: {error}"),
        )
    })?;

    let db = db_or_503(&state).await?;
    // Drop any legacy-shaped record for this org (see the doc above);
    // it can never be decoded as an OrgRecord, so it must go before
    // the upsert rather than linger as a duplicate.
    let _ = delete_by_field(&db, ORG_TABLE, "org_id", &id).await?;
    let existing: Vec<OrgRecord> = select_by_field(&db, ORG_TABLE, "orgId", &id).await?;
    if existing.is_empty() {
        let created = create_record(&db, ORG_TABLE, &body).await?;
        return Ok((StatusCode::OK, Json(json!(created))));
    }
    let saved = save_org(&db, &body).await?;
    Ok((StatusCode::OK, Json(json!(saved))))
}

/// DELETE /orgs/{id} — remove an organization record (original
/// `OrgService::delete_org` / repository `delete`). A plain record
/// delete: the caller moves members first, exactly as the disband
/// flow does — this endpoint does not migrate anyone. 404 when no
/// record (current or legacy-shaped) exists for the id.
pub async fn delete_org(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    let deleted = delete_by_field(&db, ORG_TABLE, "orgId", &id).await?;
    let deleted_legacy = delete_by_field(&db, ORG_TABLE, "org_id", &id).await?;
    if !deleted && !deleted_legacy {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Organization '{id}' not found."),
        ));
    }
    Ok((StatusCode::OK, Json(json!({ "deleted": true }))))
}

/// GET /orgs/invites?memberUid=… — the pending invites of one player
/// across all orgs (original `get_member_invites`), sorted by org name.
pub async fn list_member_invites(
    State(state): State<AppState>,
    Query(query): Query<InvitesQuery>,
) -> ApiResult {
    if query.member_uid.trim().is_empty() {
        return Ok((StatusCode::OK, Json(json!({ "invites": [] }))));
    }
    let db = db_or_503(&state).await?;
    let orgs: Vec<OrgRecord> = select_all(&db, ORG_TABLE).await?;
    let mut invites: Vec<OrgInviteRecord> = orgs
        .iter()
        .filter_map(|org| org.pending_invites.get(&query.member_uid).cloned())
        .collect();
    invites.sort_by(|left, right| left.org_name.cmp(&right.org_name));
    Ok((StatusCode::OK, Json(json!({ "invites": invites }))))
}

// ---------------------------------------------------------------------------
// Members
// ---------------------------------------------------------------------------

/// GET /orgs/{id}/members — the member roster (original
/// `OrgService::get_members`).
pub async fn list_members(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    let org = load_org(&db, &id).await?;
    let mut members: Vec<MemberSummary> = org.members.values().cloned().collect();
    members.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.uid.cmp(&b.uid)));
    Ok((StatusCode::OK, Json(json!({ "members": members }))))
}

/// POST /orgs/{id}/members — ensure a player is on the roster
/// (original `ensure_member`). Adds them when missing; refreshes the
/// stored name when it is empty or `"Unknown"` and a real name is now
/// given.
pub async fn ensure_member(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<EnsureMemberBody>,
) -> ApiResult {
    let member_uid = body.member_uid.trim().to_string();
    if id.trim().is_empty() || member_uid.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "A valid organization and member UID are required.",
        ));
    }
    let member_name = display_name(&body.member_name);

    let db = db_or_503(&state).await?;
    let mut org = load_org(&db, &id).await?;
    let should_refresh = org
        .members
        .get(&member_uid)
        .map(|member| {
            let existing_name = member.name.trim();
            !member_name.eq_ignore_ascii_case("unknown")
                && (existing_name.is_empty() || existing_name.eq_ignore_ascii_case("unknown"))
        })
        .unwrap_or(false);
    if !org.members.contains_key(&member_uid) || should_refresh {
        org.members.insert(
            member_uid.clone(),
            MemberSummary {
                uid: member_uid,
                name: member_name,
            },
        );
        let saved = save_org(&db, &org).await?;
        Ok((StatusCode::OK, Json(json!(saved))))
    } else {
        Ok((StatusCode::OK, Json(json!(org))))
    }
}

/// DELETE /orgs/{id}/members/{uid} — remove a member from the roster
/// (original `OrgService::remove_member`). 404 when the org or the
/// member does not exist.
pub async fn remove_member(
    State(state): State<AppState>,
    Path((id, member_uid)): Path<(String, String)>,
) -> ApiResult {
    let db = db_or_503(&state).await?;
    let mut org = load_org(&db, &id).await?;
    if org.members.remove(&member_uid).is_none() {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Member '{member_uid}' not found in organization."),
        ));
    }
    let saved = save_org(&db, &org).await?;
    Ok((StatusCode::OK, Json(json!(saved))))
}

// ---------------------------------------------------------------------------
// Invites
// ---------------------------------------------------------------------------

/// POST /orgs/{id}/invites — invite a player (original
/// `invite_member`). Only the org leader (or default-org CEO) can
/// invite; the target must not already be a member and must not belong
/// to another org.
pub async fn invite_member(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<InviteBody>,
) -> ApiResult {
    let requester_uid = body.requester_uid.trim().to_string();
    let target_uid = body.target_uid.trim().to_string();
    if requester_uid.is_empty() || target_uid.is_empty() || id.trim().is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "A valid organization invite request is required.",
        ));
    }

    let db = db_or_503(&state).await?;
    let mut org = load_org(&db, &id).await?;
    if !can_manage_treasury(&org, &requester_uid, body.requester_is_default_org_ceo) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "Only the organization leader or CEO can send organization invites.",
        ));
    }
    if target_uid == requester_uid {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "You cannot invite yourself to the organization.",
        ));
    }
    if org.members.contains_key(&target_uid) {
        return Err(err(
            StatusCode::CONFLICT,
            "Selected player is already a member of this organization.",
        ));
    }
    if !body.target_org_id.trim().is_empty()
        && !body.target_org_id.eq_ignore_ascii_case(DEFAULT_ORG_ID)
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Selected player must leave their current organization before joining another.",
        ));
    }

    let target_name = display_name(&body.target_name);
    let inviter_name = display_name(&body.requester_name);
    org.pending_invites.insert(
        target_uid.clone(),
        OrgInviteRecord {
            org_id: org.org_id.clone(),
            org_name: org.name.clone(),
            inviter_uid: requester_uid,
            inviter_name,
            target_uid: target_uid.clone(),
            target_name: target_name.clone(),
        },
    );
    let saved = save_org(&db, &org).await?;
    Ok((
        StatusCode::OK,
        Json(json!({
            "org": saved,
            "target_uid": target_uid,
            "message": format!("Invitation sent to {target_name}."),
        })),
    ))
}

/// POST /orgs/{id}/invites/accept — accept an invite (original
/// `accept_invite`). The player leaves their current org (unless it is
/// this one) and joins this org.
pub async fn accept_invite(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<InviteDecisionBody>,
) -> ApiResult {
    let requester_uid = body.requester_uid.trim().to_string();
    if requester_uid.is_empty() || id.trim().is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "A valid organization invite acceptance is required.",
        ));
    }
    if !body.existing_org_id.trim().is_empty()
        && !body.existing_org_id.eq_ignore_ascii_case(DEFAULT_ORG_ID)
        && !body.existing_org_id.eq_ignore_ascii_case(&id)
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Leave your current organization before accepting another invite.",
        ));
    }

    let db = db_or_503(&state).await?;
    let mut invited_org = load_org(&db, &id).await?;
    let invite = match invited_org.pending_invites.remove(&requester_uid) {
        Some(invite) => invite,
        None => {
            return Err(err(
                StatusCode::CONFLICT,
                "That organization invite is no longer available.",
            ));
        }
    };
    if invited_org.members.contains_key(&requester_uid) {
        let saved = save_org(&db, &invited_org).await?;
        return Ok((
            StatusCode::OK,
            Json(json!({
                "invited_org": saved,
                "previous_org": Value::Null,
                "actor_organization": invited_org.org_id,
                "message": "You are already a member of that organization.",
            })),
        ));
    }

    let requester_name = if body.requester_name.trim().is_empty() {
        invite.target_name.clone()
    } else {
        body.requester_name.clone()
    };
    let mut previous_org = None;
    if !body.existing_org_id.trim().is_empty()
        && !body
            .existing_org_id
            .eq_ignore_ascii_case(&invited_org.org_id)
    {
        let mut current_org = load_org(&db, &body.existing_org_id).await?;
        current_org.members.remove(&requester_uid);
        let saved = save_org(&db, &current_org).await?;
        previous_org = Some(saved);
    }

    invited_org.members.insert(
        requester_uid.clone(),
        MemberSummary {
            uid: requester_uid,
            name: requester_name,
        },
    );
    let saved = save_org(&db, &invited_org).await?;
    let org_name = saved.name.clone();
    let actor_organization = saved.org_id.clone();
    Ok((
        StatusCode::OK,
        Json(json!({
            "invited_org": saved,
            "previous_org": previous_org,
            "actor_organization": actor_organization,
            "message": format!("You joined {org_name}."),
        })),
    ))
}

/// POST /orgs/{id}/invites/decline — decline an invite (original
/// `decline_invite`).
pub async fn decline_invite(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<InviteDecisionBody>,
) -> ApiResult {
    let requester_uid = body.requester_uid.trim().to_string();
    if requester_uid.is_empty() || id.trim().is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "A valid organization invite decline is required.",
        ));
    }

    let db = db_or_503(&state).await?;
    let mut invited_org = load_org(&db, &id).await?;
    let invite = match invited_org.pending_invites.remove(&requester_uid) {
        Some(invite) => invite,
        None => {
            return Err(err(
                StatusCode::CONFLICT,
                "That organization invite is no longer available.",
            ));
        }
    };
    let saved = save_org(&db, &invited_org).await?;
    let org_name = invite.org_name.clone();
    Ok((
        StatusCode::OK,
        Json(json!({
            "invited_org": saved,
            "previous_org": Value::Null,
            "actor_organization": body.existing_org_id,
            "message": format!("Invitation from {org_name} declined."),
        })),
    ))
}

// ---------------------------------------------------------------------------
// Leave / disband
// ---------------------------------------------------------------------------

/// POST /orgs/{id}/leave — leave an org (original `leave_org`). The
/// owner cannot leave (they must disband); everyone else returns to the
/// default org.
pub async fn leave_org(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<LeaveBody>,
) -> ApiResult {
    let requester_uid = body.requester_uid.trim().to_string();
    if requester_uid.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "A valid player UID is required.",
        ));
    }
    if id.trim().is_empty() || id.eq_ignore_ascii_case(DEFAULT_ORG_ID) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "You are already assigned to the default organization.",
        ));
    }

    let db = db_or_503(&state).await?;
    let mut org = load_org(&db, &id).await?;
    if org.owner == requester_uid {
        return Err(err(
            StatusCode::CONFLICT,
            "Organization owners must disband the organization instead of leaving it.",
        ));
    }
    let org_name = org.name.clone();
    org.members.remove(&requester_uid);
    save_org(&db, &org).await?;

    let mut default_org = load_or_create_default_org(&db).await?;
    default_org.members.insert(
        requester_uid.clone(),
        MemberSummary {
            uid: requester_uid,
            name: display_name(&body.requester_name),
        },
    );
    save_org(&db, &default_org).await?;

    Ok((
        StatusCode::OK,
        Json(json!({
            "actor_organization": DEFAULT_ORG_ID,
            "message": format!("You left {org_name} and returned to the default organization."),
        })),
    ))
}

/// POST /orgs/{id}/disband — disband an org (original `disband_org`).
/// Owner only; every member returns to the default org.
pub async fn disband_org(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<LeaveBody>,
) -> ApiResult {
    let requester_uid = body.requester_uid.trim().to_string();
    if requester_uid.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "A valid player UID is required.",
        ));
    }
    if id.trim().is_empty() || id.eq_ignore_ascii_case(DEFAULT_ORG_ID) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Only active player organizations can be disbanded.",
        ));
    }

    let db = db_or_503(&state).await?;
    let org = load_org(&db, &id).await?;
    if org.owner != requester_uid {
        return Err(err(
            StatusCode::FORBIDDEN,
            "Only the organization owner can disband this organization.",
        ));
    }

    let org_name = org.name.clone();
    let mut default_org = load_or_create_default_org(&db).await?;
    let mut member_results = Vec::new();
    for (member_uid, member) in &org.members {
        default_org
            .members
            .insert(member_uid.clone(), member.clone());
        member_results.push(json!({
            "uid": member_uid,
            "requester": member_uid == &requester_uid,
            "actor_organization": DEFAULT_ORG_ID,
            "message": if member_uid == &requester_uid {
                format!("Your organization, {org_name}, has been disbanded.")
            } else {
                format!("{org_name} has been disbanded.")
            },
        }));
    }
    if !default_org.members.contains_key(&requester_uid) {
        default_org.members.insert(
            requester_uid.clone(),
            MemberSummary {
                uid: requester_uid.clone(),
                name: display_name(&body.requester_name),
            },
        );
        member_results.push(json!({
            "uid": requester_uid,
            "requester": true,
            "actor_organization": DEFAULT_ORG_ID,
            "message": format!("Your organization, {org_name}, has been disbanded."),
        }));
    }
    save_org(&db, &default_org).await?;

    // Delete the org record itself.
    let mut response = db
        .query("DELETE org WHERE orgId = $id")
        .bind(("id", id.clone()))
        .await
        .map_err(|error| super::db_err("delete org", error))?;
    let _: Vec<Value> = response
        .take(0)
        .map_err(|error| super::db_err("delete org", error))?;

    Ok((
        StatusCode::OK,
        Json(json!({
            "message": format!("{org_name} has been disbanded."),
            "members": member_results,
        })),
    ))
}

// ---------------------------------------------------------------------------
// Assets / fleet
// ---------------------------------------------------------------------------

/// GET /orgs/{id}/assets — the org's assets (original
/// `OrgService::get_assets`).
pub async fn get_assets(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    let org = load_org(&db, &id).await?;
    Ok((StatusCode::OK, Json(json!({ "assets": org.assets }))))
}

/// POST /orgs/{id}/assets — add assets (original `add_assets`).
/// Entries with an empty classname or a quantity of zero or less are
/// skipped, as in the original; quantities add onto what is already
/// there.
pub async fn add_assets(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AddAssetsBody>,
) -> ApiResult {
    let db = db_or_503(&state).await?;
    let mut org = load_org(&db, &id).await?;
    for asset in &body.assets {
        if asset.classname.trim().is_empty() || asset.quantity <= 0 {
            continue;
        }
        let category = asset.category.trim().to_ascii_lowercase();
        let category_assets = org.assets.entry(category.clone()).or_default();
        let entry = category_assets
            .entry(asset.classname.clone())
            .or_insert_with(|| OrgAssetEntry {
                classname: asset.classname.clone(),
                asset_type: category.clone(),
                quantity: 0,
            });
        entry.quantity += asset.quantity;
    }
    let saved = save_org(&db, &org).await?;
    Ok((
        StatusCode::OK,
        Json(mutation_json(&saved, &["assets"], None, "")?),
    ))
}

/// GET /orgs/{id}/fleet — the org's fleet (original
/// `OrgService::get_fleet`).
pub async fn get_fleet(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    let org = load_org(&db, &id).await?;
    Ok((StatusCode::OK, Json(json!({ "fleet": org.fleet }))))
}

/// POST /orgs/{id}/fleet — add fleet vehicles (original
/// `add_fleet_vehicles`). Keys are `{classname}_{index}`, status
/// `"Ready"`, damage `"0%"`.
pub async fn add_fleet_vehicles(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AddFleetBody>,
) -> ApiResult {
    let db = db_or_503(&state).await?;
    let mut org = load_org(&db, &id).await?;
    let mut fleet_index = org.fleet.len();
    for vehicle in &body.vehicles {
        if vehicle.classname.trim().is_empty() {
            continue;
        }
        let fleet_type = vehicle.category.trim().to_ascii_lowercase();
        let mut fleet_key = format!("{}_{}", vehicle.classname, fleet_index);
        while org.fleet.contains_key(&fleet_key) {
            fleet_index += 1;
            fleet_key = format!("{}_{}", vehicle.classname, fleet_index);
        }
        org.fleet.insert(
            fleet_key,
            OrgFleetEntry {
                classname: vehicle.classname.clone(),
                name: vehicle.classname.clone(),
                fleet_type,
                status: "Ready".to_string(),
                damage: "0%".to_string(),
            },
        );
        fleet_index += 1;
    }
    let saved = save_org(&db, &org).await?;
    Ok((
        StatusCode::OK,
        Json(mutation_json(&saved, &["fleet"], None, "")?),
    ))
}

// ---------------------------------------------------------------------------
// Credit lines / treasury
// ---------------------------------------------------------------------------

/// POST /orgs/{id}/credit-lines — assign (or re-assign) a member's
/// credit line (original `assign_credit_line`). Leader/CEO only; the
/// amount is reserved out of org funds, so the funds must cover any
/// increase.
pub async fn assign_credit_line(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AssignCreditLineBody>,
) -> ApiResult {
    let requester_uid = body.requester_uid.trim().to_string();
    let member_uid = body.member_uid.trim().to_string();
    if requester_uid.is_empty() || member_uid.is_empty() || id.trim().is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "A valid requester, member, and organization are required.",
        ));
    }
    if body.amount <= 0.0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "A valid credit amount is required.",
        ));
    }

    let db = db_or_503(&state).await?;
    let mut org = load_org(&db, &id).await?;
    if !can_manage_treasury(&org, &requester_uid, body.requester_is_default_org_ceo) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "Only the organization leader or CEO can manage treasury actions.",
        ));
    }
    let member_record = match org.members.get(&member_uid) {
        Some(member) => member.clone(),
        None => {
            return Err(err(
                StatusCode::NOT_FOUND,
                "Selected member was not found in the organization roster.",
            ));
        }
    };
    let member_name = if body.member_name.trim().is_empty() {
        member_record.name.clone()
    } else {
        body.member_name.clone()
    };

    let mut credit_line = org
        .credit_lines
        .get(&member_uid)
        .cloned()
        .unwrap_or_else(|| CreditLineSummary::new(member_uid.clone(), member_name.clone()));
    credit_line.normalize();

    let next_reserved_amount = round_currency(body.amount);
    let previous_reserved_amount = round_currency(credit_line.available_amount);
    let treasury_delta = round_currency(next_reserved_amount - previous_reserved_amount);
    if treasury_delta > 0.0 && org.funds < treasury_delta {
        return Err(err(
            StatusCode::CONFLICT,
            "Organization funds cannot cover that credit assignment.",
        ));
    }

    org.funds = round_currency(org.funds - treasury_delta);
    credit_line.uid = member_uid.clone();
    credit_line.name = member_name.clone();
    credit_line.approved_amount = next_reserved_amount;
    credit_line.available_amount = next_reserved_amount;
    credit_line.amount = next_reserved_amount;
    if credit_line.interest_rate <= 0.0 {
        credit_line.interest_rate = DEFAULT_CREDIT_LINE_INTEREST_RATE;
    }
    org.credit_lines.insert(member_uid.clone(), credit_line);
    let saved = save_org(&db, &org).await?;
    Ok((
        StatusCode::OK,
        Json(mutation_json(
            &saved,
            &["funds", "credit_lines"],
            Some(&requester_uid),
            &format!(
                "Credit line for {member_name} set to ${}.",
                format_currency(next_reserved_amount)
            ),
        )?),
    ))
}

/// POST /orgs/{id}/credit-lines/repay — repay a member's credit line
/// (original `repay_credit_line`). The payment is split between
/// principal and interest in proportion to what is owed, and the org's
/// funds grow by the amount paid.
pub async fn repay_credit_line(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RepayCreditLineBody>,
) -> ApiResult {
    let requester_uid = body.requester_uid.trim().to_string();
    if requester_uid.is_empty() || id.trim().is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "A valid requester and organization are required.",
        ));
    }
    if body.amount <= 0.0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Repayment amount must be greater than zero.",
        ));
    }

    let db = db_or_503(&state).await?;
    let mut org = load_org(&db, &id).await?;
    let mut credit_line = match org.credit_lines.get(&requester_uid) {
        Some(credit_line) => credit_line.clone(),
        None => {
            return Err(err(
                StatusCode::NOT_FOUND,
                "No active credit line is assigned to this member.",
            ));
        }
    };
    credit_line.normalize();
    if credit_line.amount_due <= 0.0 {
        return Err(err(
            StatusCode::CONFLICT,
            "This credit line has no outstanding balance.",
        ));
    }

    let paid_amount = round_currency(body.amount.min(credit_line.amount_due));
    let principal_paid = if paid_amount >= credit_line.amount_due {
        credit_line.outstanding_principal
    } else {
        round_currency(paid_amount * (credit_line.outstanding_principal / credit_line.amount_due))
            .min(credit_line.outstanding_principal)
            .min(paid_amount)
    };
    let interest_paid = round_currency(paid_amount - principal_paid);

    credit_line.outstanding_principal =
        round_currency(credit_line.outstanding_principal - principal_paid);
    credit_line.amount_due = round_currency(credit_line.amount_due - paid_amount);
    if credit_line.outstanding_principal <= 0.0 {
        credit_line.outstanding_principal = 0.0;
    }
    if credit_line.amount_due <= 0.0 {
        credit_line.amount_due = 0.0;
    }
    credit_line.amount = credit_line.available_amount;

    org.funds = round_currency(org.funds + paid_amount);
    org.credit_lines
        .insert(requester_uid.clone(), credit_line.clone());
    let saved = save_org(&db, &org).await?;
    let patch = org_patch(&saved, &["funds", "credit_lines"])
        .map_err(|error| err(StatusCode::BAD_REQUEST, error))?;
    let message = if credit_line.amount_due > 0.0 {
        format!(
            "Credit repayment posted. ${} paid with ${} still due.",
            format_currency(paid_amount),
            format_currency(credit_line.amount_due)
        )
    } else {
        format!(
            "Credit repayment posted. ${} cleared the outstanding balance.",
            format_currency(paid_amount)
        )
    };
    Ok((
        StatusCode::OK,
        Json(json!({
            "org": saved,
            "patch": patch,
            "member_uids": resolve_member_uids(&saved, Some(&requester_uid)),
            "paid_amount": paid_amount,
            "principal_paid": principal_paid,
            "interest_paid": interest_paid,
            "remaining_amount_due": credit_line.amount_due,
            "message": message,
        })),
    ))
}

/// POST /orgs/{id}/charge — charge a checkout against org funds or a
/// member's credit line (original `OrgHotStateService::charge_checkout`).
/// The store checkout calls this shape internally; this endpoint exposes
/// it directly for anything else that charges an org.
pub async fn charge_org(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ChargeOrgBody>,
) -> ApiResult {
    let requester_uid = body.requester_uid.trim().to_string();
    if requester_uid.is_empty() || id.trim().is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "A valid requester and organization are required.",
        ));
    }
    if body.amount <= 0.0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Checkout amount must be greater than zero.",
        ));
    }

    let db = db_or_503(&state).await?;
    let mut org = load_org(&db, &id).await?;
    let fields = apply_org_charge(&mut org, &body)?;
    let saved = save_org(&db, &org).await?;
    Ok((
        StatusCode::OK,
        Json(mutation_json(&saved, &fields, Some(&requester_uid), "")?),
    ))
}

/// The charge itself, shared with the store checkout: mutates the org
/// in memory and returns the patch fields that changed; the caller
/// persists. Returns the patch fields that changed.
pub fn apply_org_charge(
    org: &mut OrgRecord,
    body: &ChargeOrgBody,
) -> Result<Vec<&'static str>, (StatusCode, Json<Value>)> {
    match body.source.trim().to_ascii_lowercase().as_str() {
        "org_funds" => {
            let charged_amount = round_currency(body.amount);
            let can_charge =
                can_manage_treasury(org, &body.requester_uid, body.requester_is_default_org_ceo)
                    || (body.allow_member_charge && org.members.contains_key(&body.requester_uid));
            if !can_charge {
                return Err(err(
                    StatusCode::FORBIDDEN,
                    "Only the organization leader or CEO can charge org funds.",
                ));
            }
            if org.funds < charged_amount {
                return Err(err(
                    StatusCode::CONFLICT,
                    "Organization funds cannot cover this checkout.",
                ));
            }
            org.funds = round_currency(org.funds - charged_amount);
            if body.record_member_debt {
                let member_name = org
                    .members
                    .get(&body.requester_uid)
                    .map(|member| member.name.clone())
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| "Unknown".to_string());
                let mut credit_line = org
                    .credit_lines
                    .get(&body.requester_uid)
                    .cloned()
                    .unwrap_or_else(|| {
                        CreditLineSummary::new(body.requester_uid.clone(), member_name.clone())
                    });
                credit_line.normalize();
                credit_line.uid = body.requester_uid.clone();
                credit_line.name = member_name;
                if credit_line.interest_rate <= 0.0 {
                    credit_line.interest_rate = DEFAULT_CREDIT_LINE_INTEREST_RATE;
                }
                credit_line.outstanding_principal =
                    round_currency(credit_line.outstanding_principal + charged_amount);
                credit_line.amount_due = round_currency(credit_line.amount_due + charged_amount);
                credit_line.amount = credit_line.available_amount;
                org.credit_lines
                    .insert(body.requester_uid.clone(), credit_line);
            }
            let fields = if body.record_member_debt {
                vec!["funds", "credit_lines"]
            } else {
                vec!["funds"]
            };
            Ok(fields)
        }
        "credit_line" => {
            let mut credit_line = match org.credit_lines.get(&body.requester_uid) {
                Some(credit_line) => credit_line.clone(),
                None => {
                    return Err(err(
                        StatusCode::NOT_FOUND,
                        "Assigned credit line cannot cover this checkout.",
                    ));
                }
            };
            credit_line.normalize();
            if credit_line.available_amount < body.amount {
                return Err(err(
                    StatusCode::CONFLICT,
                    "Assigned credit line cannot cover this checkout.",
                ));
            }
            let charged_amount = round_currency(body.amount);
            credit_line.available_amount =
                round_currency(credit_line.available_amount - charged_amount);
            credit_line.approved_amount = credit_line.available_amount;
            credit_line.outstanding_principal =
                round_currency(credit_line.outstanding_principal + charged_amount);
            credit_line.amount_due = round_currency(
                credit_line.amount_due + (charged_amount * (1.0 + credit_line.interest_rate)),
            );
            credit_line.amount = credit_line.available_amount;
            org.credit_lines
                .insert(body.requester_uid.clone(), credit_line);
            Ok(vec!["credit_lines"])
        }
        _ => Err(err(
            StatusCode::BAD_REQUEST,
            "Selected organization payment source is unsupported.",
        )),
    }
}

/// POST /orgs/{id}/reputation — adjust the org's reputation by a
/// delta (the org side of the original's task reward service,
/// `applyRatingOutcome`: a successful contract adds the rating delta
/// to the owning org's reputation). Clamped at zero, since the
/// original's `Org::validate` rejects negative reputation.
pub async fn adjust_reputation(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ReputationBody>,
) -> ApiResult {
    let db = db_or_503(&state).await?;
    let mut org = load_org(&db, &id).await?;
    org.reputation = (org.reputation + body.delta).max(0);
    org.validate().map_err(|error| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Validation failed: {error}"),
        )
    })?;
    let saved = save_org(&db, &org).await?;
    Ok((
        StatusCode::OK,
        Json(mutation_json(&saved, &["reputation"], None, "")?),
    ))
}

/// Shared by the store checkout: load an org record.
pub async fn load_org_for_checkout(
    db: &super::Db,
    org_id: &str,
) -> Result<OrgRecord, (StatusCode, Json<Value>)> {
    load_org(db, org_id).await
}

/// Shared by the store checkout: persist an already-mutated org.
pub async fn save_org_for_checkout(
    db: &super::Db,
    org: &OrgRecord,
) -> Result<OrgRecord, (StatusCode, Json<Value>)> {
    save_org(db, org).await
}

/// Shared by the store checkout: the org's member UIDs (the original's
/// `resolve_member_uids`).
pub fn org_member_uids(org: &OrgRecord, requester_uid: Option<&str>) -> Vec<String> {
    resolve_member_uids(org, requester_uid)
}

fn format_currency(amount: f64) -> String {
    let rounded = round_currency(amount).round() as i64;
    let digits = rounded.to_string();
    let mut formatted = String::new();
    for (index, character) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            formatted.push(',');
        }
        formatted.push(character);
    }
    formatted.chars().rev().collect()
}
