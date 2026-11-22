//! SurrealDB client setup.
//!
//! Mirrors the connection flow of the Forge Arma extension
//! (`arma/server/extension/src/surreal.rs` in InnovativeDevSolutions/forge):
//! connect over HTTP, sign in, then select the namespace/database. The API
//! retries a few times at startup so `docker compose up` works even if
//! SurrealDB is still booting.

use std::time::Duration;

use surrealdb::engine::remote::http::{Client, Http};
use surrealdb::opt::auth::Root;
use surrealdb::Surreal;
use tokio::time::sleep;

use crate::config::Config;

/// The SurrealDB client type used across the API.
pub type Db = Surreal<Client>;

const CONNECT_ATTEMPTS: usize = 5;
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Tables the API persists records to.
///
/// A fresh SurrealDB — exactly what a first `docker compose up` creates —
/// has none of these, and handlers SELECT before they ever CREATE, so every
/// persistence call failed with "The table '...' does not exist" until the
/// tables were defined. Startup defines them (see `ensure_tables`).
///
/// Keep in sync with the table constants: `cad::GROUP_TABLE`,
/// `cad::CONTRACT_TABLE`, `players::bank::BANK_TABLE`,
/// `players::garage::{GARAGE_TABLE, VGARAGE_TABLE}`,
/// `players::locker::{LOCKER_TABLE, VLOCKER_TABLE}`,
/// `players::phone::PHONE_TABLE`, `players::org::ORG_TABLE`. Invites and
/// members live inside the `org` record, and the store checkout reuses the
/// player tables, so neither needs its own table.
const TABLES: &[&str] = &[
    "cad_group",
    "cad_contract",
    "player_bank",
    "player_garage",
    "player_vgarage",
    "player_locker",
    "player_vlocker",
    "player_phone",
    "org",
];

/// Define every table the API uses, creating any that are missing.
///
/// `DEFINE TABLE IF NOT EXISTS ... SCHEMALESS` is idempotent: on a database
/// that already has the tables (and data), it changes nothing. Runs once at
/// startup, after the namespace/database are selected, so a fresh install
/// works on first boot without a manual migration step.
async fn ensure_tables(db: &Db) -> Result<(), String> {
    let statements = TABLES
        .iter()
        .map(|table| format!("DEFINE TABLE IF NOT EXISTS {table} SCHEMALESS"))
        .collect::<Vec<_>>()
        .join("; ");
    db.query(statements)
        .await
        .map_err(|err| err.to_string())?
        .check()
        .map_err(|err| err.to_string())?;
    tracing::info!("SurrealDB tables ensured: {}", TABLES.join(", "));
    Ok(())
}

/// Connect to SurrealDB, retrying a few times. Returns the ready client.
pub async fn connect(config: &Config) -> Result<Db, String> {
    let mut last_error = String::new();

    for attempt in 1..=CONNECT_ATTEMPTS {
        match try_connect(config).await {
            Ok(db) => return Ok(db),
            Err(err) => {
                last_error = err;
                if attempt < CONNECT_ATTEMPTS {
                    tracing::warn!(
                        "SurrealDB connect attempt {attempt}/{CONNECT_ATTEMPTS} failed: {last_error}; retrying in {}s",
                        RETRY_DELAY.as_secs()
                    );
                    sleep(RETRY_DELAY).await;
                }
            }
        }
    }

    Err(format!(
        "could not connect to SurrealDB at {} after {CONNECT_ATTEMPTS} attempts: {last_error}",
        config.surreal_endpoint
    ))
}

async fn try_connect(config: &Config) -> Result<Db, String> {
    let db = Surreal::new::<Http>(config.surreal_endpoint.as_str())
        .await
        .map_err(|err| err.to_string())?;

    db.signin(Root {
        username: config.surreal_user.clone(),
        password: config.surreal_password.clone(),
    })
    .await
    .map_err(|err| err.to_string())?;

    db.use_ns(config.surreal_namespace.as_str())
        .use_db(config.surreal_database.as_str())
        .await
        .map_err(|err| err.to_string())?;

    ensure_tables(&db).await?;

    Ok(db)
}
