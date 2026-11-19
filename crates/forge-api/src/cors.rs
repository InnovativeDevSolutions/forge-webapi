//! CORS for Arma 3 clients.
//!
//! ## Why this exists
//!
//! Arma 3's `webRequest` (see <https://wiki.arma3.io/commands/webRequest.html>)
//! forces the request `Origin` header to start with `arma://` — e.g. the
//! `"origin": "forge"` key in the SQF request hashmap becomes
//! `Origin: arma://forge`. The engine also sends a CORS preflight (`OPTIONS`)
//! before every request.
//!
//! That means the API must:
//!   1. Echo back the *exact* Origin the client sent
//!      (`Access-Control-Allow-Origin: arma://forge`), and
//!   2. Answer the preflight with the methods/headers the client may use.
//!
//! A wildcard (`Access-Control-Allow-Origin: *`) does NOT work here: it is
//! rejected whenever credentials are involved, and it does not match the
//! forced `arma://` origin. So instead of `*`, this layer allows any Origin
//! that starts with `arma://` and echoes it back verbatim. Requests whose
//! Origin does not match (curl, browsers, other tools) are simply served
//! without CORS headers — the endpoints themselves stay reachable.

use std::time::Duration;

use axum::http::{header, HeaderValue, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Build the CORS layer for the API.
///
/// Allows any `arma://*` origin (echoed back exactly), the HTTP methods the
/// Forge SQF client uses, and the headers a JSON API needs. Credentials are
/// allowed because the Arma client sends them with the forced origin.
pub fn arma_cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin: &HeaderValue, _req| {
            origin.as_bytes().starts_with(b"arma://")
        }))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, header::ACCEPT])
        .allow_credentials(true)
        .max_age(Duration::from_secs(86_400))
}
