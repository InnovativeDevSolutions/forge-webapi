//! Bank accounts: balances, deposits, withdrawals, transfers, PINs.
//!
//! Grounded in `lib/services/src/bank.rs` (`BankHotStateService` +
//! `BankService`) and `lib/models/src/bank.rs` (`Bank`,
//! `BankMutationResult`, `BankTransferResult`, `BankOperationContext`,
//! `BankTransferContext`, `BankCheckoutContext`, `BankPinContext`), with
//! the extension commands in `arma/server/extension/src/bank.rs`
//! (`get_bank`, `create_bank`, `update_bank`, `deposit_hot_bank`,
//! `withdraw_hot_bank`, `deposit_earnings_hot_bank`,
//! `transfer_hot_bank`, `validate_pin_hot_bank`, `change_pin_hot_bank`,
//! `charge_checkout_hot_bank`).
//!
//! The record is the original's `Bank`: `uid`, `name`, `bank`, `cash`,
//! `earnings`, `pin` (a four-digit number, 1000–9999), `transactions`.
//! Money is `f64`, exactly as the original stores it; the bank service
//! does not round balances.
//!
//! ## Modes
//!
//! The original distinguishes the full bank interface (`mode: "bank"`,
//! the web UI) from an ATM session (`mode: "atm"`). Deposits and
//! withdrawals work in either mode but an ATM session needs
//! `atmAuthorized` first; earnings deposits, transfers and PIN changes
//! are bank-interface only, and PIN validation is ATM only. The API
//! defaults `mode` to `"bank"` when the caller omits it (the web UI is
//! the full bank interface).
//!
//! ## Reward hook
//!
//! `POST /players/{uid}/bank/credit-earnings` credits pending
//! `earnings`. It mirrors what the original's task reward service does
//! (`arma/server/addons/task/functions/objects/fnc_TaskRewardService.sqf`,
//! `applyRatingOutcome`): on a successful contract each participant's
//! `earnings` grows by their share of the rating delta, via a BankStore
//! `mset` patch of the `earnings` field. The bridge flow after
//! `POST /cad/contracts/{id}/complete` with `succeeded` is therefore:
//! read the contract's `requesterUid` (and, for org reputation, its
//! `orgId`), credit each participant's earnings here, and let the player
//! move earnings into the bank with `deposit-earnings`. There is no
//! direct "credit bank" operation in the original — rewards always land
//! in `earnings` first.

use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{
    create_record, db_or_503, delete_by_field, err, load_by_field, to_content,
    update_record_content, ApiResult,
};
use crate::AppState;

/// SurrealDB table for player bank accounts.
pub const BANK_TABLE: &str = "player_bank";

/// A bank account — the original's `Bank` (`lib/models/src/bank.rs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bank {
    /// SurrealDB record id — internal only, never part of the API shape.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    pub id: Option<Value>,
    pub uid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub bank: f64,
    #[serde(default)]
    pub cash: f64,
    #[serde(default)]
    pub earnings: f64,
    #[serde(default)]
    pub pin: u64,
    #[serde(default)]
    pub transactions: Vec<String>,
}

impl Bank {
    /// Validation from `Bank::validate` (`lib/models/src/bank.rs`) with
    /// the original's `BankValidationError` wording
    /// (`lib/shared/src/validation.rs`).
    pub fn validate(&self) -> Result<(), String> {
        if self.uid.trim().is_empty() {
            return Err("UID cannot be empty".to_string());
        }
        if self.name.trim().is_empty() {
            return Err("Name cannot be empty".to_string());
        }
        if self.bank < 0.0 {
            return Err("Bank balance cannot be negative".to_string());
        }
        if self.cash < 0.0 {
            return Err("Cash cannot be negative".to_string());
        }
        if !(1000..=9999).contains(&self.pin) {
            return Err(format!(
                "Invalid PIN format '{}' - must be a 4-digit number",
                self.pin
            ));
        }
        Ok(())
    }
}

/// Body for `POST /players/{uid}/bank` — create an account. The original's
/// `Bank::new` starts every balance at zero.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateBankBody {
    pub name: String,
    pub pin: u64,
}

/// Body for the amount operations (deposit / withdraw / deposit-earnings /
/// credit-earnings / charge). Extra fields per operation are separate
/// structs below.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AmountBody {
    pub amount: f64,
    #[serde(default = "default_bank_mode")]
    pub mode: String,
    #[serde(default)]
    pub atm_authorized: bool,
}

/// Body for `POST /players/{uid}/bank/transfer` — the original's
/// `BankTransferContext` (`mode`, `atmAuthorized`, `fromField`) plus the
/// target account. The original's transfer validates the mode but never
/// consults `atmAuthorized` (transfers are bank-interface only), so the
/// field is accepted and ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferBody {
    pub target_uid: String,
    pub amount: f64,
    #[serde(default = "default_from_field")]
    pub from_field: String,
    #[serde(default = "default_bank_mode")]
    pub mode: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub atm_authorized: bool,
}

fn default_bank_mode() -> String {
    "bank".to_string()
}

fn default_from_field() -> String {
    "bank".to_string()
}

/// Body for `POST /players/{uid}/bank/validate-pin` — the original's
/// `BankPinContext`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidatePinBody {
    pub pin: String,
    #[serde(default = "default_bank_mode")]
    pub mode: String,
}

/// Body for `POST /players/{uid}/bank/change-pin`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangePinBody {
    pub current_pin: String,
    pub new_pin: String,
    #[serde(default = "default_bank_mode")]
    pub mode: String,
}

/// Body for `POST /players/{uid}/bank/charge` — the original's
/// `BankCheckoutContext` (`sourceField`, `commit`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChargeBody {
    pub amount: f64,
    #[serde(default = "default_from_field")]
    pub source_field: String,
    #[serde(default = "default_commit_true")]
    pub commit: bool,
}

fn default_commit_true() -> bool {
    true
}

fn is_four_digit_pin(pin: &str) -> bool {
    pin.len() == 4 && pin.chars().all(|c| c.is_ascii_digit())
}

/// The original's `validate_atm_access`: an ATM session needs
/// authorization before it can move money.
fn validate_atm_access(mode: &str, atm_authorized: bool, action: &str) -> Result<(), String> {
    if mode.eq_ignore_ascii_case("atm") && !atm_authorized {
        return Err(format!("ATM authorization is required before {action}."));
    }
    Ok(())
}

/// The original's `validate_bank_mode`: some operations only exist on
/// the full bank interface.
fn validate_bank_mode(mode: &str, action: &str) -> Result<(), String> {
    if !mode.eq_ignore_ascii_case("bank") {
        return Err(format!(
            "{action} are only available from the full bank interface."
        ));
    }
    Ok(())
}

fn bank_patch(bank: &Bank, fields: &[&str]) -> Result<HashMap<String, Value>, String> {
    let mut patch = HashMap::new();
    for field in fields {
        let value = match *field {
            "uid" => json!(bank.uid),
            "name" => json!(bank.name),
            "bank" => json!(bank.bank),
            "cash" => json!(bank.cash),
            "earnings" => json!(bank.earnings),
            "pin" => json!(bank.pin),
            "transactions" => json!(bank.transactions),
            _ => return Err(format!("Unknown field: {field}")),
        };
        patch.insert((*field).to_string(), value);
    }
    Ok(patch)
}

/// Serialize an account as the original's `BankMutationResult`:
/// `{account, patch}`.
fn mutation_json(bank: &Bank, fields: &[&str]) -> Result<Value, (StatusCode, Json<Value>)> {
    let patch = bank_patch(bank, fields).map_err(|error| err(StatusCode::BAD_REQUEST, error))?;
    Ok(json!({ "account": bank, "patch": patch }))
}

async fn load_bank(db: &super::Db, uid: &str) -> Result<Bank, (StatusCode, Json<Value>)> {
    load_by_field(db, BANK_TABLE, "uid", uid, "Bank account").await
}

async fn save_bank(db: &super::Db, bank: &Bank) -> Result<Bank, (StatusCode, Json<Value>)> {
    let content = to_content(bank)?;
    update_record_content(db, BANK_TABLE, "uid", &bank.uid, content).await
}

/// POST /players/{uid}/bank — create a bank account (original
/// `BankService::create`). 409 when the account already exists.
pub async fn create_bank(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<CreateBankBody>,
) -> ApiResult {
    let uid = uid.trim().to_string();
    if uid.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "UID cannot be empty"));
    }
    let bank = Bank {
        id: None,
        uid: uid.clone(),
        name: body.name,
        bank: 0.0,
        cash: 0.0,
        earnings: 0.0,
        pin: body.pin,
        transactions: Vec::new(),
    };
    bank.validate().map_err(|error| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Validation failed: {error}"),
        )
    })?;

    let db = db_or_503(&state).await?;
    let existing: Vec<Bank> = super::select_by_field(&db, BANK_TABLE, "uid", &uid).await?;
    if !existing.is_empty() {
        return Err(err(
            StatusCode::CONFLICT,
            format!("Bank with uid '{uid}' already exists"),
        ));
    }
    let created = create_record(&db, BANK_TABLE, &bank).await?;
    Ok((StatusCode::CREATED, Json(json!(created))))
}

/// GET /players/{uid}/bank — one account (original `BankService::get_bank`).
/// 404 when the player has no account; the bridge creates accounts on
/// player join (there is no auto-create on read in the original).
pub async fn get_bank(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    let bank = load_bank(&db, &uid).await?;
    Ok((StatusCode::OK, Json(json!(bank))))
}

/// PUT /players/{uid}/bank — replace the whole account (original
/// `BankHotStateService` save: the hot state persists a whole `Bank`
/// record, balances and transaction log included). Creates the account
/// when it does not exist yet, so the hot-state save path never 404s.
/// The `uid` in the body must match the path.
pub async fn replace_bank(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<Bank>,
) -> ApiResult {
    let uid = uid.trim().to_string();
    if uid.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "UID cannot be empty"));
    }
    if body.uid.trim() != uid {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!(
                "Bank UID '{}' does not match the path UID '{uid}'.",
                body.uid
            ),
        ));
    }
    body.validate().map_err(|error| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Validation failed: {error}"),
        )
    })?;

    let db = db_or_503(&state).await?;
    let existing: Vec<Bank> = super::select_by_field(&db, BANK_TABLE, "uid", &uid).await?;
    if existing.is_empty() {
        let created = create_record(&db, BANK_TABLE, &body).await?;
        return Ok((StatusCode::OK, Json(json!(created))));
    }
    let saved = save_bank(&db, &body).await?;
    Ok((StatusCode::OK, Json(json!(saved))))
}

/// DELETE /players/{uid}/bank — remove an account (original
/// `BankService::delete`). 404 when the player has no account.
pub async fn delete_bank(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    if !delete_by_field(&db, BANK_TABLE, "uid", &uid).await? {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Bank with uid '{uid}' not found."),
        ));
    }
    Ok((StatusCode::OK, Json(json!({ "deleted": true }))))
}

/// POST /players/{uid}/bank/deposit — move cash into the bank
/// (original `deposit`: cash must cover the amount; ATM sessions need
/// authorization).
pub async fn deposit(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<AmountBody>,
) -> ApiResult {
    if body.amount <= 0.0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Deposit amount must be greater than zero",
        ));
    }
    validate_atm_access(&body.mode, body.atm_authorized, "deposit")
        .map_err(|error| err(StatusCode::BAD_REQUEST, error))?;

    let db = db_or_503(&state).await?;
    let mut bank = load_bank(&db, &uid).await?;
    if bank.cash < body.amount {
        return Err(err(
            StatusCode::CONFLICT,
            "Cash on hand cannot cover that deposit.",
        ));
    }
    bank.cash -= body.amount;
    bank.bank += body.amount;
    bank.validate().map_err(|error| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Validation failed: {error}"),
        )
    })?;
    let saved = save_bank(&db, &bank).await?;
    Ok((
        StatusCode::OK,
        Json(mutation_json(&saved, &["bank", "cash"])?),
    ))
}

/// POST /players/{uid}/bank/withdraw — move bank balance into cash
/// (original `withdraw`).
pub async fn withdraw(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<AmountBody>,
) -> ApiResult {
    if body.amount <= 0.0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Withdrawal amount must be greater than zero",
        ));
    }
    validate_atm_access(&body.mode, body.atm_authorized, "withdrawal")
        .map_err(|error| err(StatusCode::BAD_REQUEST, error))?;

    let db = db_or_503(&state).await?;
    let mut bank = load_bank(&db, &uid).await?;
    if bank.bank < body.amount {
        return Err(err(
            StatusCode::CONFLICT,
            "Bank balance cannot cover that withdrawal.",
        ));
    }
    bank.bank -= body.amount;
    bank.cash += body.amount;
    bank.validate().map_err(|error| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Validation failed: {error}"),
        )
    })?;
    let saved = save_bank(&db, &bank).await?;
    Ok((
        StatusCode::OK,
        Json(mutation_json(&saved, &["bank", "cash"])?),
    ))
}

/// POST /players/{uid}/bank/deposit-earnings — move pending earnings
/// into the bank (original `deposit_earnings`, bank interface only).
pub async fn deposit_earnings(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<AmountBody>,
) -> ApiResult {
    if body.amount <= 0.0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Deposit earnings amount must be greater than zero",
        ));
    }
    validate_bank_mode(&body.mode, "Earnings deposits")
        .map_err(|error| err(StatusCode::BAD_REQUEST, error))?;

    let db = db_or_503(&state).await?;
    let mut bank = load_bank(&db, &uid).await?;
    if bank.earnings < body.amount {
        return Err(err(
            StatusCode::CONFLICT,
            "Pending earnings cannot cover that deposit request.",
        ));
    }
    bank.bank += body.amount;
    bank.earnings -= body.amount;
    bank.validate().map_err(|error| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Validation failed: {error}"),
        )
    })?;
    let saved = save_bank(&db, &bank).await?;
    Ok((
        StatusCode::OK,
        Json(mutation_json(&saved, &["bank", "earnings"])?),
    ))
}

/// POST /players/{uid}/bank/credit-earnings — add to pending earnings.
/// This is the contract-reward hook (see the module docs): the original
/// has no standalone "credit" operation, but its task reward service
/// credits `earnings` with a BankStore `mset` patch, which is exactly
/// what this endpoint does.
pub async fn credit_earnings(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<AmountBody>,
) -> ApiResult {
    if body.amount <= 0.0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Credit amount must be greater than zero",
        ));
    }

    let db = db_or_503(&state).await?;
    let mut bank = load_bank(&db, &uid).await?;
    bank.earnings += body.amount;
    bank.validate().map_err(|error| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Validation failed: {error}"),
        )
    })?;
    let saved = save_bank(&db, &bank).await?;
    Ok((StatusCode::OK, Json(mutation_json(&saved, &["earnings"])?)))
}

/// POST /players/{uid}/bank/transfer — move money to another player's
/// bank account (original `transfer`, bank interface only). The target
/// always receives into `bank`; the source pays from `cash` or `bank`
/// (`fromField`, default `bank`).
pub async fn transfer(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<TransferBody>,
) -> ApiResult {
    if body.amount <= 0.0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Transfer amount must be greater than zero",
        ));
    }
    validate_bank_mode(&body.mode, "Transfers")
        .map_err(|error| err(StatusCode::BAD_REQUEST, error))?;
    let target_uid = body.target_uid.trim().to_string();
    if target_uid.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "Target UID is required."));
    }
    if uid == target_uid {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "You cannot transfer funds to yourself.",
        ));
    }
    let from_field = match body.from_field.trim().to_ascii_lowercase().as_str() {
        "cash" => "cash",
        _ => "bank",
    };

    let db = db_or_503(&state).await?;
    let mut source = load_bank(&db, &uid).await?;
    let mut target = load_bank(&db, &target_uid).await?;
    let source_balance = match from_field {
        "cash" => source.cash,
        _ => source.bank,
    };
    if source_balance < body.amount {
        return Err(err(
            StatusCode::CONFLICT,
            match from_field {
                "cash" => "Cash on hand cannot cover that transfer.",
                _ => "Bank balance cannot cover that transfer.",
            },
        ));
    }
    match from_field {
        "cash" => source.cash -= body.amount,
        _ => source.bank -= body.amount,
    }
    target.bank += body.amount;
    source.validate().map_err(|error| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Validation failed: {error}"),
        )
    })?;
    target.validate().map_err(|error| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Validation failed: {error}"),
        )
    })?;
    // Same order as the original: source first, then target.
    let saved_source = save_bank(&db, &source).await?;
    let saved_target = save_bank(&db, &target).await?;
    let source_patch = bank_patch(&saved_source, &[from_field])
        .map_err(|error| err(StatusCode::BAD_REQUEST, error))?;
    let target_patch = bank_patch(&saved_target, &["bank"])
        .map_err(|error| err(StatusCode::BAD_REQUEST, error))?;
    Ok((
        StatusCode::OK,
        Json(json!({
            "source_account": saved_source,
            "source_patch": source_patch,
            "target_account": saved_target,
            "target_patch": target_patch,
        })),
    ))
}

/// POST /players/{uid}/bank/validate-pin — check a PIN (original
/// `validate_pin`, ATM sessions only). 200 `{"valid": true}` on success;
/// 401 on a wrong PIN.
pub async fn validate_pin(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<ValidatePinBody>,
) -> ApiResult {
    if !body.mode.eq_ignore_ascii_case("atm") {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "PIN entry is only available from an ATM session.",
        ));
    }
    if !is_four_digit_pin(&body.pin) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Enter your four-digit access PIN.",
        ));
    }

    let db = db_or_503(&state).await?;
    let bank = load_bank(&db, &uid).await?;
    if body.pin != bank.pin.to_string() {
        return Err(err(StatusCode::UNAUTHORIZED, "Incorrect PIN."));
    }
    Ok((StatusCode::OK, Json(json!({ "valid": true }))))
}

/// POST /players/{uid}/bank/change-pin — change a PIN (original
/// `change_pin`, bank interface only).
pub async fn change_pin(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<ChangePinBody>,
) -> ApiResult {
    if !body.mode.trim().eq_ignore_ascii_case("bank") {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "PIN changes are only available from the full bank interface.",
        ));
    }
    if !is_four_digit_pin(&body.current_pin) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Enter your current four-digit PIN.",
        ));
    }
    if !is_four_digit_pin(&body.new_pin) {
        return Err(err(StatusCode::BAD_REQUEST, "Choose a new four-digit PIN."));
    }
    if body.current_pin == body.new_pin {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Choose a different PIN from your current PIN.",
        ));
    }

    let db = db_or_503(&state).await?;
    let mut bank = load_bank(&db, &uid).await?;
    if body.current_pin != bank.pin.to_string() {
        return Err(err(StatusCode::UNAUTHORIZED, "Current PIN is incorrect."));
    }
    bank.pin = body
        .new_pin
        .parse::<u64>()
        .map_err(|error| err(StatusCode::BAD_REQUEST, format!("Invalid new PIN: {error}")))?;
    bank.validate().map_err(|error| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Validation failed: {error}"),
        )
    })?;
    let saved = save_bank(&db, &bank).await?;
    Ok((StatusCode::OK, Json(mutation_json(&saved, &["pin"])?)))
}

/// POST /players/{uid}/bank/charge — charge a checkout against cash or
/// bank (original `charge_checkout`). With `commit: false` the charge is
/// previewed only: the returned account is what the balance would be,
/// and nothing is saved — that is how the store checkout previews a
/// charge before committing it.
pub async fn charge(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<ChargeBody>,
) -> ApiResult {
    if body.amount <= 0.0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Checkout amount must be greater than zero",
        ));
    }
    let source_field = match body.source_field.trim().to_ascii_lowercase().as_str() {
        "cash" => "cash",
        "bank" => "bank",
        _ => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "Selected bank payment source is unsupported.",
            ));
        }
    };

    let db = db_or_503(&state).await?;
    let mut bank = load_bank(&db, &uid).await?;
    let source_balance = match source_field {
        "cash" => bank.cash,
        _ => bank.bank,
    };
    if source_balance < body.amount {
        return Err(err(
            StatusCode::CONFLICT,
            match source_field {
                "cash" => "Cash on hand cannot cover this checkout.",
                _ => "Bank balance cannot cover this checkout.",
            },
        ));
    }
    match source_field {
        "cash" => bank.cash -= body.amount,
        _ => bank.bank -= body.amount,
    }
    bank.validate().map_err(|error| {
        err(
            StatusCode::BAD_REQUEST,
            format!("Validation failed: {error}"),
        )
    })?;
    if body.commit {
        let saved = save_bank(&db, &bank).await?;
        Ok((
            StatusCode::OK,
            Json(mutation_json(&saved, &[source_field])?),
        ))
    } else {
        Ok((StatusCode::OK, Json(mutation_json(&bank, &[source_field])?)))
    }
}

/// Shared by the store checkout: load an account without the HTTP
/// wrapper, so the checkout can charge it as part of its own commit.
pub async fn load_account(db: &super::Db, uid: &str) -> Result<Bank, (StatusCode, Json<Value>)> {
    load_bank(db, uid).await
}

/// Shared by the store checkout: persist an already-mutated account.
pub async fn save_account(db: &super::Db, bank: &Bank) -> Result<Bank, (StatusCode, Json<Value>)> {
    save_bank(db, bank).await
}

/// Shared by the store checkout: the `bank`/`cash` patch of an account.
pub fn account_patch(bank: &Bank, field: &str) -> HashMap<String, Value> {
    bank_patch(bank, &[field]).unwrap_or_default()
}
