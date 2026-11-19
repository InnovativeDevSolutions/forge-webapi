//! CAD endpoints: groups, group status, and contract availability.
//!
//! ## Scope (Jacob, 2026-11-19)
//!
//! The CAD system in the web API is concerned with exactly three things:
//! groups, group status, and which contracts are available vs not available.
//! No dispatching, units, incidents, or messaging lives here.
//!
//! ## Grounding in the original Forge implementation
//!
//! - **Group status** — the exact values the CAD UI offers and the
//!   server-side `GroupRepository` validates
//!   (`arma/server/addons/cad/functions/fnc_initGroupRepository.sqf`,
//!   `validStatuses`; `arma/client/addons/cad/ui/src/dispatcher/index.js`
//!   and `sidepanel.js`, `statuses`):
//!   `available`, `en_route`, `on_task`, `holding`, `danger`, `unavailable`.
//!   Default status is `available`; default role is `infantry`.
//!   Roles: `infantry`, `recon`, `armor`, `air`, `logistics`, `support`.
//!   A group record carries `groupId` (stable CAD id, `grp:N` in the
//!   original), `callsign` (defaults to the group id), `role`, `status`,
//!   `members` (player roster of `{uid, name}`) and `position`.
//!   In the original, groups are live Arma groups and their profile lives
//!   on the group object; here the API owns persistence, so groups are
//!   SurrealDB records in the `cad_group` table, registered by the game
//!   server bridge (`POST /cad/groups`) as groups come and go.
//! - **Contract status** — from `lib/services/src/task.rs`
//!   (`TaskStateService`): a contract (task catalog entry) defaults to
//!   `available` (`locked: true` makes it `locked`); assignment moves it
//!   to `assigned`, acceptance to `active`, completion to `succeeded` or
//!   `failed`. `list_active_catalog` treats `available`, `assigned` and
//!   `active` as the live board. Availability is therefore derived, never
//!   stored: a contract is available exactly when its status is
//!   `available`; anything else (assigned, active, locked, succeeded,
//!   failed) is not available.
//! - **Assignment** — the task record carries `ASSIGNMENT_FIELDS`
//!   (`lib/services/src/task.rs`): `assignedGroupId`,
//!   `assignedGroupCallsign`, `assignedByUid`, `assignedByName`,
//!   `assignedAt`, `assignmentNote`, `acceptedAt`, plus `accepted`,
//!   `requesterUid`, `orgID`. The CAD store's `toContractEntry`
//!   (`arma/server/addons/cad/functions/fnc_initCadStore.sqf`) derives
//!   `assignmentState` from that: `assigned` → `assigned`, `active` with a
//!   group → `accepted`, otherwise `unassigned`. Contract responses here
//!   expose the same assignment object and `assignmentState`.
//! - **Lifecycle is read-only in this slice.** Contracts are seeded into
//!   the `cad_contract` table by the game server bridge / mission (the
//!   original's `upsert_catalog_entry`); assign/accept/decline mutations
//!   belong to the later task/contract-lifecycle work, not to CAD scope.
//!
//! ## Persistence
//!
//! Tables `cad_group` and `cad_contract` in SurrealDB. Lookups use the
//! business keys (`group_id`, `contract_id`) as fields — the same
//! select-by-field pattern the original extension uses
//! (`arma/server/extension/src/storage/common.rs`) — so no record-id
//! escaping questions arise for ids like `grp:1`.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{db::Db, AppState};

/// SurrealDB table for CAD groups.
pub const GROUP_TABLE: &str = "cad_group";
/// SurrealDB table for CAD contracts (task catalog entries).
pub const CONTRACT_TABLE: &str = "cad_contract";

/// Shared result type: success and error both carry a status + JSON body.
type ApiResult = Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)>;

fn err(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": message.into() })))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Require a database connection; the CAD routes are persistence routes, so
/// they answer 503 while the API is running without SurrealDB (same
/// degraded-mode pattern as `/health` reporting `database: "unavailable"`).
async fn db_or_503(state: &AppState) -> Result<Arc<Db>, (StatusCode, Json<Value>)> {
    match &state.db {
        Some(db) => Ok(Arc::clone(db)),
        None => Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "database unavailable: SurrealDB is not connected",
        )),
    }
}

fn db_err(context: &str, error: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    err(
        StatusCode::BAD_GATEWAY,
        format!("SurrealDB {context} failed: {error}"),
    )
}

fn decode_record<T>(record: Value) -> Result<T, (StatusCode, Json<Value>)>
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

fn decode_records<T>(records: Vec<Value>) -> Result<Vec<T>, (StatusCode, Json<Value>)>
where
    T: for<'de> Deserialize<'de>,
{
    records.into_iter().map(decode_record).collect()
}

async fn select_all<T>(db: &Db, table: &str) -> Result<Vec<T>, (StatusCode, Json<Value>)>
where
    T: for<'de> Deserialize<'de>,
{
    let values: Vec<Value> = db
        .select(table)
        .await
        .map_err(|error| db_err("select", error))?;
    decode_records(values)
}

async fn select_by_field<T>(
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

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

/// Group status. The exact six values the original CAD UI offers and the
/// server-side `GroupRepository` validates (`validStatuses`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupStatus {
    Available,
    EnRoute,
    OnTask,
    Holding,
    Danger,
    Unavailable,
}

impl Default for GroupStatus {
    fn default() -> Self {
        Self::Available
    }
}

impl GroupStatus {
    pub fn all() -> [&'static str; 6] {
        [
            "available",
            "en_route",
            "on_task",
            "holding",
            "danger",
            "unavailable",
        ]
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::EnRoute => "en_route",
            Self::OnTask => "on_task",
            Self::Holding => "holding",
            Self::Danger => "danger",
            Self::Unavailable => "unavailable",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "available" => Some(Self::Available),
            "en_route" => Some(Self::EnRoute),
            "on_task" => Some(Self::OnTask),
            "holding" => Some(Self::Holding),
            "danger" => Some(Self::Danger),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }
}

/// Group role. The exact six values from the original (`validRoles`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupRole {
    Infantry,
    Recon,
    Armor,
    Air,
    Logistics,
    Support,
}

impl Default for GroupRole {
    fn default() -> Self {
        Self::Infantry
    }
}

impl GroupRole {
    pub fn all() -> [&'static str; 6] {
        ["infantry", "recon", "armor", "air", "logistics", "support"]
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Infantry => "infantry",
            Self::Recon => "recon",
            Self::Armor => "armor",
            Self::Air => "air",
            Self::Logistics => "logistics",
            Self::Support => "support",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "infantry" => Some(Self::Infantry),
            "recon" => Some(Self::Recon),
            "armor" => Some(Self::Armor),
            "air" => Some(Self::Air),
            "logistics" => Some(Self::Logistics),
            "support" => Some(Self::Support),
            _ => None,
        }
    }
}

/// One member of a group roster — mirrors `MemberSummary { uid, name }` in
/// the original's `lib/models/src/org.rs`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GroupMember {
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub name: String,
}

/// A CAD group, as stored in the `cad_group` table and returned by the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    /// SurrealDB record id — internal only, never part of the API shape.
    /// Kept so decoding a stored record does not fail on the extra field.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    pub id: Option<Value>,
    /// Stable CAD group id, e.g. `grp:1` (assigned by the game server).
    pub group_id: String,
    /// Display callsign; defaults to `group_id` when not given.
    #[serde(default)]
    pub callsign: String,
    #[serde(default)]
    pub role: GroupRole,
    #[serde(default)]
    pub status: GroupStatus,
    /// Player roster.
    #[serde(default)]
    pub members: Vec<GroupMember>,
    /// Organization the group belongs to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    /// Unix seconds of the last profile/status change.
    #[serde(default)]
    pub updated_at: i64,
}

/// Body for `POST /cad/groups` — register (or re-register) a group.
/// `group_id` is required; everything else falls back to the original's
/// defaults (callsign = group id, role = infantry, status = available).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterGroupBody {
    pub group_id: String,
    #[serde(default)]
    pub callsign: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub members: Vec<GroupMember>,
    #[serde(default)]
    pub org_id: Option<String>,
}

/// Body for `PUT /cad/groups/:id/status`.
#[derive(Debug, Deserialize)]
pub struct StatusUpdateBody {
    pub status: String,
}

/// Query filters for `GET /cad/groups`.
#[derive(Debug, Deserialize)]
pub struct GroupListParams {
    pub status: Option<String>,
    pub role: Option<String>,
}

fn group_json(group: &Group) -> Value {
    serde_json::to_value(group).unwrap_or(Value::Null)
}

/// GET /cad/groups — list groups, optionally filtered by `?status=` and/or
/// `?role=` (both validated against the original's value lists).
pub async fn list_groups(
    State(state): State<AppState>,
    Query(params): Query<GroupListParams>,
) -> ApiResult {
    if let Some(status) = &params.status {
        if GroupStatus::parse(status).is_none() {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!(
                    "Invalid group status '{status}'. Valid statuses: {}.",
                    GroupStatus::all().join(", ")
                ),
            ));
        }
    }
    if let Some(role) = &params.role {
        if GroupRole::parse(role).is_none() {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!(
                    "Invalid group role '{role}'. Valid roles: {}.",
                    GroupRole::all().join(", ")
                ),
            ));
        }
    }

    let db = db_or_503(&state).await?;
    let mut groups: Vec<Group> = select_all(&db, GROUP_TABLE).await?;
    if let Some(status) = &params.status {
        groups.retain(|group| group.status.as_str() == status);
    }
    if let Some(role) = &params.role {
        groups.retain(|group| group.role.as_str() == role);
    }
    // Stable order, like the CAD UI's getSortedGroups (danger first, then
    // callsign); here: callsign, then group id.
    groups.sort_by(|a, b| {
        a.callsign
            .cmp(&b.callsign)
            .then_with(|| a.group_id.cmp(&b.group_id))
    });
    Ok((
        StatusCode::OK,
        Json(json!({ "groups": groups.iter().map(group_json).collect::<Vec<_>>() })),
    ))
}

/// POST /cad/groups — register a group (or update its profile on
/// re-register). Returns 201 when the group is new, 200 when it already
/// existed. This is how the game server bridge reports live Arma groups
/// into the API, mirroring `ensureGroupId` + `updateProfile` in the
/// original.
pub async fn register_group(
    State(state): State<AppState>,
    Json(body): Json<RegisterGroupBody>,
) -> ApiResult {
    let group_id = body.group_id.trim().to_string();
    if group_id.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "Group ID is required."));
    }
    let role = match &body.role {
        Some(role) => GroupRole::parse(role).ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                format!(
                    "Invalid group role '{role}'. Valid roles: {}.",
                    GroupRole::all().join(", ")
                ),
            )
        })?,
        None => GroupRole::default(),
    };
    let status = match &body.status {
        Some(status) => GroupStatus::parse(status).ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                format!(
                    "Invalid group status '{status}'. Valid statuses: {}.",
                    GroupStatus::all().join(", ")
                ),
            )
        })?,
        None => GroupStatus::default(),
    };
    let callsign = body
        .callsign
        .filter(|callsign| !callsign.trim().is_empty())
        .unwrap_or_else(|| group_id.clone());

    let db = db_or_503(&state).await?;
    let existing: Vec<Group> = select_by_field(&db, GROUP_TABLE, "group_id", &group_id).await?;
    let group = Group {
        id: None,
        group_id: group_id.clone(),
        callsign,
        role,
        status,
        members: body.members,
        org_id: body.org_id,
        updated_at: now_unix(),
    };
    let record = serde_json::to_value(&group)
        .map_err(|error| err(StatusCode::BAD_REQUEST, format!("Invalid group: {error}")))?;

    if existing.is_empty() {
        let _: Option<Value> = db
            .create(GROUP_TABLE)
            .content(record)
            .await
            .map_err(|error| db_err("create group", error))?;
        Ok((StatusCode::CREATED, Json(group_json(&group))))
    } else {
        let response = db
            .query(
                "UPDATE cad_group SET callsign = $callsign, role = $role, \
                 status = $status, members = $members, org_id = $org_id, \
                 updated_at = $ts WHERE group_id = $id",
            )
            .bind(("callsign", group.callsign.clone()))
            .bind(("role", group.role.as_str().to_string()))
            .bind(("status", group.status.as_str().to_string()))
            .bind((
                "members",
                serde_json::to_value(&group.members).unwrap_or(Value::Null),
            ))
            .bind((
                "org_id",
                group
                    .org_id
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            ))
            .bind(("ts", group.updated_at))
            .bind(("id", group_id))
            .await
            .map_err(|error| db_err("update group", error))?;
        response
            .check()
            .map_err(|error| db_err("update group", error))?;
        Ok((StatusCode::OK, Json(group_json(&group))))
    }
}

/// GET /cad/groups/:id — one group.
pub async fn get_group(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    let groups: Vec<Group> = select_by_field(&db, GROUP_TABLE, "group_id", &id).await?;
    match groups.into_iter().next() {
        Some(group) => Ok((StatusCode::OK, Json(group_json(&group)))),
        None => Err(err(
            StatusCode::NOT_FOUND,
            format!("Group '{id}' not found."),
        )),
    }
}

/// PUT /cad/groups/:id/status — update a group's status. The value must be
/// one of the original's six statuses; anything else is rejected with the
/// same "Invalid group status." wording the original `updateProfile` uses.
pub async fn update_group_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<StatusUpdateBody>,
) -> ApiResult {
    let status = GroupStatus::parse(&body.status).ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            format!(
                "Invalid group status '{}'. Valid statuses: {}.",
                body.status,
                GroupStatus::all().join(", ")
            ),
        )
    })?;

    let db = db_or_503(&state).await?;
    let mut response = db
        .query("UPDATE cad_group SET status = $status, updated_at = $ts WHERE group_id = $id")
        .bind(("status", status.as_str().to_string()))
        .bind(("ts", now_unix()))
        .bind(("id", id.clone()))
        .await
        .map_err(|error| db_err("update group status", error))?;
    let updated: Vec<Value> = response
        .take(0)
        .map_err(|error| db_err("update group status", error))?;
    match updated.into_iter().next() {
        Some(record) => {
            let group: Group = decode_record(record)?;
            Ok((StatusCode::OK, Json(group_json(&group))))
        }
        None => Err(err(
            StatusCode::NOT_FOUND,
            format!("Group '{id}' not found."),
        )),
    }
}

// ---------------------------------------------------------------------------
// Contracts
// ---------------------------------------------------------------------------

/// Contract status. The lifecycle from `lib/services/src/task.rs`:
/// `available` (default; `locked: true` → `locked`), `assigned`,
/// `active`, `succeeded`, `failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractStatus {
    Available,
    Assigned,
    Active,
    Succeeded,
    Failed,
    Locked,
}

impl Default for ContractStatus {
    fn default() -> Self {
        Self::Available
    }
}

impl ContractStatus {
    pub fn all() -> [&'static str; 6] {
        [
            "available",
            "assigned",
            "active",
            "succeeded",
            "failed",
            "locked",
        ]
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Assigned => "assigned",
            Self::Active => "active",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Locked => "locked",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "available" => Some(Self::Available),
            "assigned" => Some(Self::Assigned),
            "active" => Some(Self::Active),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "locked" => Some(Self::Locked),
            _ => None,
        }
    }
}

/// Who a contract is assigned to — the `ASSIGNMENT_FIELDS` of the original
/// task record (`lib/services/src/task.rs`), plus `accepted`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractAssignment {
    /// Id of the assigned group, e.g. `grp:1`.
    pub group_id: String,
    /// Callsign of the assigned group at assignment time.
    #[serde(default)]
    pub group_callsign: String,
    /// Dispatcher who assigned it.
    #[serde(default)]
    pub assigned_by_uid: String,
    #[serde(default)]
    pub assigned_by_name: String,
    /// Unix seconds (the original stores Arma time as f64).
    #[serde(default)]
    pub assigned_at: f64,
    /// Note left by the dispatcher at assignment time.
    #[serde(default)]
    pub note: String,
    /// Whether the assigned group accepted the contract.
    #[serde(default)]
    pub accepted: bool,
    /// When the group accepted, if it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_at: Option<f64>,
}

/// A CAD contract — one entry of the task catalog (`lib/services/src/task.rs`
/// `TaskRecord`), as stored in the `cad_contract` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Contract {
    /// SurrealDB record id — internal only, never part of the API shape.
    /// Kept so decoding a stored record does not fail on the extra field.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    pub id: Option<Value>,
    /// Catalog id of the contract, e.g. `task-1`.
    pub contract_id: String,
    #[serde(default)]
    pub title: String,
    /// Contract type, e.g. `attack`, `defend`, `delivery`, `destroy`,
    /// `defuse`, `hostage`, `hvtkill`, `hvtcapture` (the CAD UI's
    /// `defaultTaskTypes`).
    #[serde(rename = "type", default)]
    pub task_type: String,
    #[serde(default)]
    pub priority: String,
    #[serde(default)]
    pub status: ContractStatus,
    /// Locked contracts are not available (original `default_catalog_status`).
    #[serde(default)]
    pub locked: bool,
    /// Where the contract came from, e.g. `dispatch` for dispatcher orders
    /// (original `isDispatchOrder`).
    #[serde(default)]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Value>,
    #[serde(default)]
    pub created_at: f64,
    /// Assignment state, if the contract is assigned to a group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment: Option<ContractAssignment>,
}

impl Contract {
    /// Availability is derived from status, exactly as in the original:
    /// a contract is available only while its status is `available`.
    /// Assigned, active, locked, succeeded and failed contracts are all
    /// not available.
    pub fn available(&self) -> bool {
        self.status == ContractStatus::Available
    }

    /// Assignment state, mirroring the CAD store's `toContractEntry`
    /// (`arma/server/addons/cad/functions/fnc_initCadStore.sqf`).
    pub fn assignment_state(&self) -> &'static str {
        match self.status {
            ContractStatus::Assigned => "assigned",
            ContractStatus::Active => {
                if self.assignment.is_some() {
                    "accepted"
                } else {
                    "active"
                }
            }
            _ => "unassigned",
        }
    }
}

/// Serialize a contract with its derived availability fields, so clients
/// never have to re-derive them.
fn contract_json(contract: &Contract) -> Value {
    let mut value = serde_json::to_value(contract).unwrap_or(Value::Null);
    if let Value::Object(ref mut map) = value {
        map.insert("available".to_string(), Value::Bool(contract.available()));
        map.insert(
            "assignmentState".to_string(),
            Value::String(contract.assignment_state().to_string()),
        );
    }
    value
}

/// Query filters for `GET /cad/contracts`.
#[derive(Debug, Deserialize)]
pub struct ContractListParams {
    /// `true` → only available contracts; `false` → only unavailable ones.
    pub available: Option<bool>,
    /// Filter by contract status (`available`, `assigned`, `active`,
    /// `succeeded`, `failed`, `locked`).
    pub status: Option<String>,
    /// Only contracts assigned to this group id.
    pub group_id: Option<String>,
}

fn filter_contracts(contracts: Vec<Contract>, params: &ContractListParams) -> Vec<Contract> {
    contracts
        .into_iter()
        .filter(|contract| {
            if let Some(available) = params.available {
                if contract.available() != available {
                    return false;
                }
            }
            if let Some(status) = &params.status {
                if contract.status.as_str() != status {
                    return false;
                }
            }
            if let Some(group_id) = &params.group_id {
                let assigned_to = contract
                    .assignment
                    .as_ref()
                    .map(|assignment| &assignment.group_id);
                if assigned_to != Some(group_id) {
                    return false;
                }
            }
            true
        })
        .collect()
}

/// GET /cad/contracts — list contracts with availability state and
/// assignment. Filters: `?available=true|false`, `?status=...`,
/// `?group_id=...` (contracts assigned to one group).
pub async fn list_contracts(
    State(state): State<AppState>,
    Query(params): Query<ContractListParams>,
) -> ApiResult {
    if let Some(status) = &params.status {
        if ContractStatus::parse(status).is_none() {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!(
                    "Invalid contract status '{status}'. Valid statuses: {}.",
                    ContractStatus::all().join(", ")
                ),
            ));
        }
    }

    let db = db_or_503(&state).await?;
    let mut contracts: Vec<Contract> = select_all(&db, CONTRACT_TABLE).await?;
    contracts = filter_contracts(contracts, &params);
    contracts.sort_by(|a, b| {
        a.created_at
            .partial_cmp(&b.created_at)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.contract_id.cmp(&b.contract_id))
    });
    Ok((
        StatusCode::OK,
        Json(json!({ "contracts": contracts.iter().map(contract_json).collect::<Vec<_>>() })),
    ))
}

/// GET /cad/contracts/:id — one contract with its availability state and
/// assignment.
pub async fn get_contract(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    let contracts: Vec<Contract> = select_by_field(&db, CONTRACT_TABLE, "contract_id", &id).await?;
    match contracts.into_iter().next() {
        Some(contract) => Ok((StatusCode::OK, Json(contract_json(&contract)))),
        None => Err(err(
            StatusCode::NOT_FOUND,
            format!("Contract '{id}' not found."),
        )),
    }
}

/// GET /cad/groups/:id/contracts — the contracts assigned to one group.
/// 404 when the group does not exist; otherwise the group's contracts
/// (assigned or accepted/active), each with its availability state.
pub async fn list_group_contracts(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult {
    let db = db_or_503(&state).await?;
    let groups: Vec<Group> = select_by_field(&db, GROUP_TABLE, "group_id", &id).await?;
    if groups.is_empty() {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Group '{id}' not found."),
        ));
    }
    let contracts: Vec<Contract> = select_all(&db, CONTRACT_TABLE).await?;
    let group_contracts: Vec<&Contract> = contracts
        .iter()
        .filter(|contract| {
            contract
                .assignment
                .as_ref()
                .is_some_and(|assignment| assignment.group_id == id)
        })
        .collect();
    Ok((
        StatusCode::OK,
        Json(json!({
            "group_id": id,
            "contracts": group_contracts.iter().map(|contract| contract_json(contract)).collect::<Vec<_>>(),
        })),
    ))
}
