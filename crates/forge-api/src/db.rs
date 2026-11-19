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

    Ok(db)
}
