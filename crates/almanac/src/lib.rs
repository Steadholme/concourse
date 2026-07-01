//! Almanac — server-rendered personal calendar + contacts for the HOLDFAST stack.
//!
//! Library root: defines [`AppState`], wires the routes via [`app`], and provides
//! [`build_dev_state`] (in-memory store) and [`build_state_from_env`] (env-selected store).
//! Integration tests consume [`app`] directly via `tower::oneshot`, exactly like the rest of
//! the estate.
//!
//! Almanac does NO login of its own: it sits behind a Sluice `auth=sso` route on
//! `cal.w33d.xyz`, trusts the gateway-injected `X-Auth-Subject` as the row OWNER (never a
//! client-supplied field) and `X-Auth-Email` only for the signed-in display. Everything it
//! stores is scoped to that subject, so two users never see each other's events or contacts.
//! It serves at the subdomain ROOT (Sluice forwards the path unmodified — no prefix to strip).
//!
//! Endpoints:
//! - `GET /healthz` — liveness (public; the container HEALTHCHECK).
//! - `GET /` — this-month calendar grid + the owner's upcoming agenda (`?y=&m=` to navigate).
//! - `GET /new` / `POST /new` — create an event (`?date=YYYY-MM-DD` pre-fills the day).
//! - `GET /edit/{id}` / `POST /edit/{id}` — edit one of the owner's events.
//! - `POST /delete/{id}` — delete one of the owner's events.
//! - `GET /contacts` — the address book + add form.
//! - `POST /contacts/new` — add a contact.
//! - `GET /contacts/edit/{id}` / `POST /contacts/edit/{id}` — edit a contact.
//! - `POST /contacts/delete/{id}` — delete a contact.
//! - `GET /settings` / `POST /settings` — the owner's timezone + week-start preferences.

pub mod auth;
pub mod calendar;
pub mod config;
pub mod error;
pub mod handlers;
pub mod render;
pub mod rrule;
pub mod store;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::routing::{get, post};
use axum::Router;

use crate::config::Config;
use crate::store::{InMemoryStore, PgStore, Store};

/// Shared application state. Cheap to clone (everything behind `Arc`).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Arc<dyn Store>,
}

/// Build the router wiring all endpoints onto `state`.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handlers::health::healthz))
        // Calendar + events.
        .route("/", get(handlers::events::index))
        .route(
            "/new",
            get(handlers::events::new_form).post(handlers::events::create),
        )
        .route(
            "/edit/{id}",
            get(handlers::events::edit_form).post(handlers::events::update),
        )
        .route("/delete/{id}", post(handlers::events::delete))
        // Contacts / address book.
        .route("/contacts", get(handlers::contacts::index))
        .route("/contacts/new", post(handlers::contacts::create))
        .route(
            "/contacts/edit/{id}",
            get(handlers::contacts::edit_form).post(handlers::contacts::update),
        )
        .route("/contacts/delete/{id}", post(handlers::contacts::delete))
        // Per-owner settings (timezone + week-start).
        .route(
            "/settings",
            get(handlers::settings::index).post(handlers::settings::update),
        )
        .fallback(handlers::not_found)
        // Reject a forged gateway identity (spoofed X-Auth-* from a rogue in-network peer):
        // when GATEWAY_HMAC_KEY is set, an injected identity MUST carry a valid X-Auth-Sig.
        // No-op when the key is unset or no identity is present (healthz / dev).
        .layer(axum::middleware::from_fn(require_gateway_sig))
        .with_state(state)
}

/// Middleware enforcing [`auth::gateway_identity_ok`] — 401 on a missing/invalid signature.
async fn require_gateway_sig(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if auth::gateway_identity_ok(req.headers()) {
        next.run(req).await
    } else {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            "invalid or missing gateway identity signature",
        )
            .into_response()
    }
}

/// Construct dev state: dev [`Config`] + an empty [`InMemoryStore`]. Used by `main`'s memory
/// mode and by the integration tests, so they need no database.
pub fn build_dev_state() -> AppState {
    AppState {
        config: Arc::new(Config::dev()),
        store: Arc::new(InMemoryStore::new()),
    }
}

/// Build runtime state from the environment.
///
/// [`Config`] comes from [`Config::from_env`]. The store is selected by `ALMANAC_STORE`:
/// - `memory` (default): empty [`InMemoryStore`] — no database required.
/// - `postgres`: connect `ALMANAC_DATABASE_URL`, run the idempotent migration, wire [`PgStore`].
pub async fn build_state_from_env() -> Result<AppState, String> {
    let config = Config::from_env();

    let store_kind = std::env::var("ALMANAC_STORE").unwrap_or_else(|_| "memory".to_string());
    let store: Arc<dyn Store> = match store_kind.as_str() {
        "postgres" => {
            let database_url = std::env::var("ALMANAC_DATABASE_URL")
                .map_err(|_| "ALMANAC_STORE=postgres requires ALMANAC_DATABASE_URL".to_string())?;
            tracing::info!("ALMANAC_STORE=postgres — connecting to database");
            let pg = PgStore::connect(&database_url)
                .await
                .map_err(|e| format!("connect postgres: {e}"))?;
            pg.migrate()
                .await
                .map_err(|e| format!("run migration: {e}"))?;
            tracing::info!("postgres store ready (migrated)");
            Arc::new(pg)
        }
        "memory" => Arc::new(InMemoryStore::new()),
        other => return Err(format!("unknown ALMANAC_STORE={other} (use memory|postgres)")),
    };

    Ok(AppState {
        config: Arc::new(config),
        store,
    })
}

/// Current wall-clock time in epoch milliseconds (event `created_at`, "is today" math).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as i64
}
