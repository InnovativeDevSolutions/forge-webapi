//! Configuration loaded from environment variables.
//!
//! No credentials are ever hardcoded: the SurrealDB password has no default
//! and startup fails loudly if it is missing.

use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    /// Address the HTTP server binds to, e.g. `0.0.0.0:8080`.
    pub bind_addr: String,
    /// SurrealDB HTTP endpoint as `host:port` — no scheme. The `Http`
    /// engine supplies `http://` itself, so `http://127.0.0.1:8000` would
    /// break URL construction. Local default `127.0.0.1:8000`; inside
    /// docker compose it is `surrealdb:8000`.
    pub surreal_endpoint: String,
    /// SurrealDB namespace selected after connecting.
    pub surreal_namespace: String,
    /// SurrealDB database selected after connecting.
    pub surreal_database: String,
    /// SurrealDB username. Defaults to `root` (SurrealDB's built-in admin).
    pub surreal_user: String,
    /// SurrealDB password. REQUIRED — there is intentionally no default.
    /// The original Forge extension shipped `root`/`root` defaults; this API
    /// refuses to repeat that pattern.
    pub surreal_password: String,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let surreal_password = env::var("SURREAL_PASSWORD").map_err(|_| {
            "SURREAL_PASSWORD is not set. Set it in your environment or .env file \
             (see .env.example); the API will not start with a default password."
                .to_string()
        })?;
        if surreal_password.trim().is_empty() {
            return Err("SURREAL_PASSWORD is empty. Set a real password.".to_string());
        }

        Ok(Self {
            bind_addr: env::var("FORGE_API_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
            surreal_endpoint: env::var("SURREAL_ENDPOINT")
                .unwrap_or_else(|_| "127.0.0.1:8000".to_string()),
            surreal_namespace: env::var("SURREAL_NAMESPACE")
                .unwrap_or_else(|_| "forge".to_string()),
            surreal_database: env::var("SURREAL_DATABASE")
                .unwrap_or_else(|_| "main".to_string()),
            surreal_user: env::var("SURREAL_USER").unwrap_or_else(|_| "root".to_string()),
            surreal_password,
        })
    }
}
