//! forge-api — standalone HTTP API for the Forge Arma 3 framework.
//!
//! Serves the Forge game servers outside the Arma process, so session data no
//! longer lives and dies with the game server's Rust extension. All durable
//! state lives in SurrealDB; this process is stateless.

mod cad;
mod config;
mod cors;
mod db;
mod players;
mod routes;

use std::sync::Arc;

use axum::{
    routing::{get, post, put},
    Router,
};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use config::Config;
use db::Db;

/// Shared state handed to every route handler.
#[derive(Clone)]
pub struct AppState {
    /// SurrealDB client. `None` when the database could not be reached at
    /// startup — the API still serves /health so the container stays
    /// observable; the CAD persistence routes answer 503 while it is `None`.
    pub db: Option<Arc<Db>>,
}

#[tokio::main]
async fn main() -> Result<(), String> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("forge_api=info,tower_http=info")
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Config::from_env()?;

    let db = match db::connect(&config).await {
        Ok(db) => {
            tracing::info!(
                "Connected to SurrealDB at {} (namespace '{}', database '{}')",
                config.surreal_endpoint,
                config.surreal_namespace,
                config.surreal_database
            );
            Some(Arc::new(db))
        }
        Err(err) => {
            tracing::warn!(
                "Starting without a database connection: {err}. /health will report database=unavailable."
            );
            None
        }
    };

    let state = AppState { db };

    let app = Router::new()
        .route("/health", get(routes::health))
        .route(
            "/cad/groups",
            get(cad::list_groups).post(cad::register_group),
        )
        .route("/cad/groups/{id}", get(cad::get_group))
        .route("/cad/groups/{id}/status", put(cad::update_group_status))
        .route("/cad/groups/{id}/contracts", get(cad::list_group_contracts))
        .route("/cad/contracts", get(cad::list_contracts))
        .route("/cad/contracts/{id}", get(cad::get_contract))
        .route("/cad/contracts/{id}/assign", post(cad::assign_contract))
        .route("/cad/contracts/{id}/accept", post(cad::accept_contract))
        .route("/cad/contracts/{id}/decline", post(cad::decline_contract))
        .route("/cad/contracts/{id}/complete", post(cad::complete_contract))
        // Player data: bank
        .route(
            "/players/{uid}/bank",
            post(players::bank::create_bank).get(players::bank::get_bank),
        )
        .route("/players/{uid}/bank/deposit", post(players::bank::deposit))
        .route(
            "/players/{uid}/bank/withdraw",
            post(players::bank::withdraw),
        )
        .route(
            "/players/{uid}/bank/deposit-earnings",
            post(players::bank::deposit_earnings),
        )
        .route(
            "/players/{uid}/bank/credit-earnings",
            post(players::bank::credit_earnings),
        )
        .route(
            "/players/{uid}/bank/transfer",
            post(players::bank::transfer),
        )
        .route(
            "/players/{uid}/bank/validate-pin",
            post(players::bank::validate_pin),
        )
        .route(
            "/players/{uid}/bank/change-pin",
            post(players::bank::change_pin),
        )
        .route("/players/{uid}/bank/charge", post(players::bank::charge))
        // Player data: garage + virtual garage
        .route(
            "/players/{uid}/garage",
            post(players::garage::create_garage)
                .get(players::garage::get_garage)
                .put(players::garage::replace_garage),
        )
        .route(
            "/players/{uid}/garage/vehicles",
            post(players::garage::add_vehicle),
        )
        .route(
            "/players/{uid}/garage/vehicles/{plate}",
            axum::routing::patch(players::garage::patch_vehicle)
                .delete(players::garage::remove_vehicle),
        )
        .route(
            "/players/{uid}/vgarage",
            post(players::garage::create_vgarage).get(players::garage::get_vgarage),
        )
        .route(
            "/players/{uid}/vgarage/{category}",
            get(players::garage::get_vgarage_category)
                .post(players::garage::add_vgarage_classnames),
        )
        .route(
            "/players/{uid}/vgarage/{category}/{classname}",
            axum::routing::delete(players::garage::remove_vgarage_classname),
        )
        // Player data: locker + virtual locker
        .route(
            "/players/{uid}/locker",
            post(players::locker::create_locker)
                .get(players::locker::get_locker)
                .put(players::locker::replace_locker),
        )
        .route(
            "/players/{uid}/locker/items",
            post(players::locker::add_item),
        )
        .route(
            "/players/{uid}/locker/items/{classname}",
            axum::routing::patch(players::locker::patch_item).delete(players::locker::remove_item),
        )
        .route(
            "/players/{uid}/vlocker",
            post(players::locker::create_vlocker).get(players::locker::get_vlocker),
        )
        .route(
            "/players/{uid}/vlocker/{category}",
            get(players::locker::get_vlocker_category)
                .post(players::locker::add_vlocker_classnames),
        )
        .route(
            "/players/{uid}/vlocker/{category}/{classname}",
            axum::routing::delete(players::locker::remove_vlocker_classname),
        )
        // Player data: phone
        .route(
            "/players/{uid}/phone",
            post(players::phone::init_phone).get(players::phone::get_phone),
        )
        .route(
            "/players/{uid}/phone/contacts",
            post(players::phone::add_contact),
        )
        .route(
            "/players/{uid}/phone/contacts/{contactUid}",
            axum::routing::delete(players::phone::remove_contact),
        )
        .route(
            "/players/{uid}/phone/messages",
            get(players::phone::list_messages).post(players::phone::send_message),
        )
        .route(
            "/players/{uid}/phone/messages/{messageId}",
            get(players::phone::message_thread).delete(players::phone::delete_message),
        )
        .route(
            "/players/{uid}/phone/messages/{messageId}/read",
            post(players::phone::mark_message_read),
        )
        .route(
            "/players/{uid}/phone/emails",
            get(players::phone::list_emails).post(players::phone::send_email),
        )
        .route(
            "/players/{uid}/phone/emails/{emailId}/read",
            post(players::phone::mark_email_read),
        )
        .route(
            "/players/{uid}/phone/emails/{emailId}",
            axum::routing::delete(players::phone::delete_email),
        )
        // Player data: orgs
        .route("/orgs", post(players::org::register_org))
        .route("/orgs/invites", get(players::org::list_member_invites))
        .route("/orgs/{id}", get(players::org::get_org))
        .route(
            "/orgs/{id}/members",
            get(players::org::list_members).post(players::org::ensure_member),
        )
        .route(
            "/orgs/{id}/members/{uid}",
            axum::routing::delete(players::org::remove_member),
        )
        .route("/orgs/{id}/invites", post(players::org::invite_member))
        .route(
            "/orgs/{id}/invites/accept",
            post(players::org::accept_invite),
        )
        .route(
            "/orgs/{id}/invites/decline",
            post(players::org::decline_invite),
        )
        .route("/orgs/{id}/leave", post(players::org::leave_org))
        .route("/orgs/{id}/disband", post(players::org::disband_org))
        .route(
            "/orgs/{id}/assets",
            get(players::org::get_assets).post(players::org::add_assets),
        )
        .route(
            "/orgs/{id}/fleet",
            get(players::org::get_fleet).post(players::org::add_fleet_vehicles),
        )
        .route(
            "/orgs/{id}/credit-lines",
            post(players::org::assign_credit_line),
        )
        .route(
            "/orgs/{id}/credit-lines/repay",
            post(players::org::repay_credit_line),
        )
        .route("/orgs/{id}/charge", post(players::org::charge_org))
        .route(
            "/orgs/{id}/reputation",
            post(players::org::adjust_reputation),
        )
        // Player data: store
        .route("/store/checkout", post(players::store::checkout))
        .layer(cors::arma_cors_layer())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .map_err(|err| format!("failed to bind {}: {err}", config.bind_addr))?;

    tracing::info!("forge-api listening on {}", config.bind_addr);
    axum::serve(listener, app)
        .await
        .map_err(|err| format!("server error: {err}"))?;

    Ok(())
}
