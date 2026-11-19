//! Phones: contacts, text messages, emails.
//!
//! Grounded in `lib/services/src/phone.rs` (`PhoneStateService`) and
//! `lib/models/src/phone.rs` (`PhonePayload`, `PhoneMessage`,
//! `PhoneEmail`), with the extension commands in
//! `arma/server/extension/src/phone.rs`.
//!
//! The original's rules, mirrored here:
//!
//! - `init` seeds the contact list with the player themself and the
//!   Field Commander (`field_commander`) — the original's
//!   `init_seeds_owner_and_field_commander_contacts` test.
//! - Messages and emails are indexed on **both** sides: sending a
//!   message appends it to the sender's and the receiver's lists (a
//!   self-message is indexed once); sending an email appends it to the
//!   receiver's list and to the sender's list marked read.
//! - The Field Commander **cannot receive** player messages or emails,
//!   but can send them (`validate_send_target`).
//! - Message ids are `phone-message:{from}:{to}:{sequence}` and email
//!   ids `phone-email:{from}:{to}:{sequence}` in the original, where
//!   the sequence comes from the repository. The API has no shared
//!   sequence counter, so it uses the current time in nanoseconds as
//!   the sequence part — same shape, still unique.
//! - An empty email subject becomes `"No subject"`.
//! - Deleting a message or email removes only the requesting player's
//!   copy; the other side keeps theirs.
//!
//! The phone has no hot/cold split in the original — one
//! `PhoneStateService` over a `PhoneRepository`. The API stores one
//! `player_phone` record per player holding the whole payload
//! (`contacts`, `messages`, `emails`), which is the repository's shape.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{
    create_record, db_or_503, err, load_by_field, now_unix_f64, select_by_field, to_content,
    update_record_content, ApiResult,
};
use crate::AppState;

/// SurrealDB table for player phones.
pub const PHONE_TABLE: &str = "player_phone";

/// The Field Commander UID — the original's `FIELD_COMMANDER_UID`
/// (`lib/services/src/phone.rs`). Seeded as a contact on init; can send
/// to players but cannot receive from them.
pub const FIELD_COMMANDER_UID: &str = "field_commander";

/// One text message — the original's `PhoneMessage`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhoneMessage {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub timestamp: f64,
    #[serde(default)]
    pub read: bool,
}

/// One email — the original's `PhoneEmail`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhoneEmail {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub timestamp: f64,
    #[serde(default)]
    pub read: bool,
}

/// A player's whole phone payload — the original's `PhonePayload`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhonePayload {
    /// SurrealDB record id — internal only, never part of the API shape.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    pub id: Option<Value>,
    pub uid: String,
    #[serde(default)]
    pub contacts: Vec<String>,
    #[serde(default)]
    pub messages: Vec<PhoneMessage>,
    #[serde(default)]
    pub emails: Vec<PhoneEmail>,
}

/// Body for `POST /players/{uid}/phone/contacts`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContactBody {
    pub contact_uid: String,
}

/// Body for `POST /players/{uid}/phone/messages`.
#[derive(Debug, Deserialize)]
pub struct SendMessageBody {
    pub to: String,
    pub message: String,
    #[serde(default)]
    pub timestamp: Option<f64>,
}

/// Body for `POST /players/{uid}/phone/emails`.
#[derive(Debug, Deserialize)]
pub struct SendEmailBody {
    pub to: String,
    #[serde(default)]
    pub subject: String,
    pub body: String,
    #[serde(default)]
    pub timestamp: Option<f64>,
}

fn validate_uid(uid: &str) -> Result<String, (StatusCode, Json<Value>)> {
    let uid = uid.trim().to_string();
    if uid.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "UID is required."));
    }
    Ok(uid)
}

fn validate_non_empty(value: &str, message: &str) -> Result<String, (StatusCode, Json<Value>)> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, message));
    }
    Ok(value)
}

/// The original's `validate_send_target`: the Field Commander cannot
/// receive player messages or emails.
fn validate_send_target(
    from_uid: &str,
    to_uid: &str,
    kind: &str,
) -> Result<(), (StatusCode, Json<Value>)> {
    if to_uid == FIELD_COMMANDER_UID && from_uid != FIELD_COMMANDER_UID {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("Field Commander cannot receive player {kind}."),
        ));
    }
    Ok(())
}

fn sequence_id(prefix: &str, from_uid: &str, to_uid: &str) -> String {
    // The original uses a repository sequence counter here; the API has
    // no shared counter, so the current time in nanoseconds plays that
    // role — same `prefix:from:to:sequence` shape, still unique.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}:{from_uid}:{to_uid}:{nanos}")
}

async fn load_phone(db: &super::Db, uid: &str) -> Result<PhonePayload, (StatusCode, Json<Value>)> {
    load_by_field(db, PHONE_TABLE, "uid", uid, "Phone").await
}

async fn save_phone(
    db: &super::Db,
    phone: &PhonePayload,
) -> Result<PhonePayload, (StatusCode, Json<Value>)> {
    let content = to_content(phone)?;
    update_record_content(db, PHONE_TABLE, "uid", &phone.uid, content).await
}

/// POST /players/{uid}/phone — initialize a phone (original
/// `PhoneStateService::init`). Seeds contacts with the player themself
/// and the Field Commander. Idempotent: 201 when new, 200 when the
/// phone already exists.
pub async fn init_phone(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let uid = validate_uid(&uid)?;
    let db = db_or_503(&state).await?;
    let existing: Vec<PhonePayload> = select_by_field(&db, PHONE_TABLE, "uid", &uid).await?;
    if let Some(phone) = existing.into_iter().next() {
        return Ok((StatusCode::OK, Json(json!(phone))));
    }
    let phone = PhonePayload {
        id: None,
        uid: uid.clone(),
        contacts: vec![uid, FIELD_COMMANDER_UID.to_string()],
        messages: Vec::new(),
        emails: Vec::new(),
    };
    let created = create_record(&db, PHONE_TABLE, &phone).await?;
    Ok((StatusCode::CREATED, Json(json!(created))))
}

/// GET /players/{uid}/phone — the whole payload: contacts, messages,
/// emails (original `payload_for`). 404 when the phone was never
/// initialized.
pub async fn get_phone(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    let phone = load_phone(&db, &uid).await?;
    Ok((StatusCode::OK, Json(json!(phone))))
}

/// POST /players/{uid}/phone/contacts — add a contact (original
/// `add_contact`). A player may add themself (the original's
/// `contact_can_reference_self_for_owner_card` test).
pub async fn add_contact(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<ContactBody>,
) -> ApiResult {
    let uid = validate_uid(&uid)?;
    let contact_uid = validate_uid(&body.contact_uid)?;

    let db = db_or_503(&state).await?;
    let mut phone = load_phone(&db, &uid).await?;
    let added = if phone.contacts.iter().any(|entry| entry == &contact_uid) {
        false
    } else {
        phone.contacts.push(contact_uid);
        true
    };
    let saved = save_phone(&db, &phone).await?;
    Ok((
        StatusCode::OK,
        Json(json!({ "added": added, "contacts": saved.contacts })),
    ))
}

/// DELETE /players/{uid}/phone/contacts/{contactUid} — remove a contact
/// (original `remove_contact`).
pub async fn remove_contact(
    State(state): State<AppState>,
    Path((uid, contact_uid)): Path<(String, String)>,
) -> ApiResult {
    let uid = validate_uid(&uid)?;
    let contact_uid = validate_uid(&contact_uid)?;

    let db = db_or_503(&state).await?;
    let mut phone = load_phone(&db, &uid).await?;
    let removed = match phone
        .contacts
        .iter()
        .position(|entry| entry == &contact_uid)
    {
        Some(position) => {
            phone.contacts.remove(position);
            true
        }
        None => false,
    };
    let saved = save_phone(&db, &phone).await?;
    Ok((
        StatusCode::OK,
        Json(json!({ "removed": removed, "contacts": saved.contacts })),
    ))
}

/// GET /players/{uid}/phone/messages — list messages (original
/// `list_messages`).
pub async fn list_messages(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    let phone = load_phone(&db, &uid).await?;
    Ok((StatusCode::OK, Json(json!({ "messages": phone.messages }))))
}

/// POST /players/{uid}/phone/messages — send a message (original
/// `send_message`). The message is indexed on both the sender's and the
/// receiver's phones (once for a self-message). The Field Commander
/// cannot receive player messages.
pub async fn send_message(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<SendMessageBody>,
) -> ApiResult {
    let from_uid = validate_uid(&uid)?;
    let to_uid = validate_uid(&body.to)?;
    let message = validate_non_empty(&body.message, "Message body is required.")?;
    validate_send_target(&from_uid, &to_uid, "messages")?;
    let timestamp = body.timestamp.unwrap_or_else(now_unix_f64);

    let db = db_or_503(&state).await?;
    let record = PhoneMessage {
        id: sequence_id("phone-message", &from_uid, &to_uid),
        from: from_uid.clone(),
        to: to_uid.clone(),
        message,
        timestamp,
        read: false,
    };
    let mut sender = load_phone(&db, &from_uid).await?;
    sender.messages.push(record.clone());
    save_phone(&db, &sender).await?;
    if to_uid != from_uid {
        // The receiver may not have a phone yet — the original's
        // repository appends regardless; here a missing receiver phone
        // is created first so the message is not lost.
        let mut receiver = match load_phone(&db, &to_uid).await {
            Ok(phone) => phone,
            Err(_) => {
                let phone = PhonePayload {
                    id: None,
                    uid: to_uid.clone(),
                    contacts: vec![to_uid.clone(), FIELD_COMMANDER_UID.to_string()],
                    messages: Vec::new(),
                    emails: Vec::new(),
                };
                create_record(&db, PHONE_TABLE, &phone).await?
            }
        };
        receiver.messages.push(record.clone());
        save_phone(&db, &receiver).await?;
    }
    Ok((StatusCode::CREATED, Json(json!(record))))
}

/// GET /players/{uid}/phone/messages/{otherUid} — the message thread
/// with one other player (original `message_thread`). This shares its
/// path shape with `DELETE .../messages/{messageId}`, so the two are
/// registered on one route in `main.rs`; the segment is the other
/// player's UID here and a message id there.
pub async fn message_thread(
    State(state): State<AppState>,
    Path((uid, other_uid)): Path<(String, String)>,
) -> ApiResult {
    let uid = validate_uid(&uid)?;
    let other_uid = validate_uid(&other_uid)?;

    let db = db_or_503(&state).await?;
    let phone = load_phone(&db, &uid).await?;
    let thread: Vec<&PhoneMessage> = phone
        .messages
        .iter()
        .filter(|message| {
            (message.from == uid && message.to == other_uid)
                || (message.from == other_uid && message.to == uid)
        })
        .collect();
    Ok((StatusCode::OK, Json(json!({ "messages": thread }))))
}

/// POST /players/{uid}/phone/messages/{messageId}/read — mark a
/// message read (original `mark_message_read`). 404 when the message
/// is not on this player's phone.
pub async fn mark_message_read(
    State(state): State<AppState>,
    Path((uid, message_id)): Path<(String, String)>,
) -> ApiResult {
    let uid = validate_uid(&uid)?;
    let message_id = validate_non_empty(&message_id, "Message ID is required.")?;

    let db = db_or_503(&state).await?;
    let mut phone = load_phone(&db, &uid).await?;
    let Some(message) = phone.messages.iter_mut().find(|m| m.id == message_id) else {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Message '{message_id}' not found."),
        ));
    };
    message.read = true;
    let saved = save_phone(&db, &phone).await?;
    let message = saved
        .messages
        .into_iter()
        .find(|m| m.id == message_id)
        .unwrap_or_else(|| PhoneMessage {
            id: message_id,
            ..Default::default()
        });
    Ok((StatusCode::OK, Json(json!(message))))
}

/// DELETE /players/{uid}/phone/messages/{messageId} — delete a message
/// (original `delete_message`). Only the requesting player's copy is
/// removed; the other side keeps theirs.
pub async fn delete_message(
    State(state): State<AppState>,
    Path((uid, message_id)): Path<(String, String)>,
) -> ApiResult {
    let uid = validate_uid(&uid)?;
    let message_id = validate_non_empty(&message_id, "Message ID is required.")?;

    let db = db_or_503(&state).await?;
    let mut phone = load_phone(&db, &uid).await?;
    let Some(position) = phone.messages.iter().position(|m| m.id == message_id) else {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Message '{message_id}' not found."),
        ));
    };
    phone.messages.remove(position);
    save_phone(&db, &phone).await?;
    Ok((StatusCode::OK, Json(json!({ "deleted": true }))))
}

/// GET /players/{uid}/phone/emails — list emails (original
/// `list_emails`).
pub async fn list_emails(State(state): State<AppState>, Path(uid): Path<String>) -> ApiResult {
    let db = db_or_503(&state).await?;
    let phone = load_phone(&db, &uid).await?;
    Ok((StatusCode::OK, Json(json!({ "emails": phone.emails }))))
}

/// POST /players/{uid}/phone/emails — send an email (original
/// `send_email`). Appended to the receiver's list and to the sender's
/// list marked read (unless it is a self-email). An empty subject
/// becomes `"No subject"`. The Field Commander cannot receive player
/// emails.
pub async fn send_email(
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(body): Json<SendEmailBody>,
) -> ApiResult {
    let from_uid = validate_uid(&uid)?;
    let to_uid = validate_uid(&body.to)?;
    let subject = {
        let subject = body.subject.trim().to_string();
        if subject.is_empty() {
            "No subject".to_string()
        } else {
            subject
        }
    };
    let body_text = validate_non_empty(&body.body, "Email body is required.")?;
    validate_send_target(&from_uid, &to_uid, "emails")?;
    let timestamp = body.timestamp.unwrap_or_else(now_unix_f64);

    let db = db_or_503(&state).await?;
    let record = PhoneEmail {
        id: sequence_id("phone-email", &from_uid, &to_uid),
        from: from_uid.clone(),
        to: to_uid.clone(),
        subject,
        body: body_text,
        timestamp,
        read: false,
    };
    let mut receiver = match load_phone(&db, &to_uid).await {
        Ok(phone) => phone,
        Err(_) => {
            let phone = PhonePayload {
                id: None,
                uid: to_uid.clone(),
                contacts: vec![to_uid.clone(), FIELD_COMMANDER_UID.to_string()],
                messages: Vec::new(),
                emails: Vec::new(),
            };
            create_record(&db, PHONE_TABLE, &phone).await?
        }
    };
    receiver.emails.push(record.clone());
    save_phone(&db, &receiver).await?;
    if from_uid != to_uid {
        let mut sender = load_phone(&db, &from_uid).await?;
        let mut sender_copy = record.clone();
        sender_copy.read = true;
        sender.emails.push(sender_copy);
        save_phone(&db, &sender).await?;
    }
    Ok((StatusCode::CREATED, Json(json!(record))))
}

/// POST /players/{uid}/phone/emails/{emailId}/read — mark an email
/// read (original `mark_email_read`). 404 when the email is not on
/// this player's phone.
pub async fn mark_email_read(
    State(state): State<AppState>,
    Path((uid, email_id)): Path<(String, String)>,
) -> ApiResult {
    let uid = validate_uid(&uid)?;
    let email_id = validate_non_empty(&email_id, "Email ID is required.")?;

    let db = db_or_503(&state).await?;
    let mut phone = load_phone(&db, &uid).await?;
    let Some(email) = phone.emails.iter_mut().find(|e| e.id == email_id) else {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Email '{email_id}' not found."),
        ));
    };
    email.read = true;
    let saved = save_phone(&db, &phone).await?;
    let email = saved
        .emails
        .into_iter()
        .find(|e| e.id == email_id)
        .unwrap_or_else(|| PhoneEmail {
            id: email_id,
            ..Default::default()
        });
    Ok((StatusCode::OK, Json(json!(email))))
}

/// DELETE /players/{uid}/phone/emails/{emailId} — delete an email
/// (original `delete_email`). Only the requesting player's copy is
/// removed.
pub async fn delete_email(
    State(state): State<AppState>,
    Path((uid, email_id)): Path<(String, String)>,
) -> ApiResult {
    let uid = validate_uid(&uid)?;
    let email_id = validate_non_empty(&email_id, "Email ID is required.")?;

    let db = db_or_503(&state).await?;
    let mut phone = load_phone(&db, &uid).await?;
    let Some(position) = phone.emails.iter().position(|e| e.id == email_id) else {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("Email '{email_id}' not found."),
        ));
    };
    phone.emails.remove(position);
    save_phone(&db, &phone).await?;
    Ok((StatusCode::OK, Json(json!({ "deleted": true }))))
}
