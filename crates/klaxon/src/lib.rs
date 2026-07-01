//! Klaxon — notification & push fan-out hub for the HOLDFAST stack.
//!
//! One owned delivery layer for the whole estate: other internal services POST events to the
//! ingest API, and users see/manage their notifications + register Web Push in an SSO inbox.
//!
//! Library root: defines [`AppState`], wires the routes via [`app`], and provides
//! [`build_dev_state`] (in-memory store + disabled audit, no database) and
//! [`build_state_from_env`] (env-selected store + Watchtower audit). Integration tests consume
//! [`app`] directly via `tower::oneshot`, exactly like the rest of the estate.
//!
//! Two surfaces on one subdomain (`notify.w33d.xyz`), split by auth model:
//! - **SSO inbox (`/`, `/api/read`, `/api/subscribe`, `/api/stream`).** The browser UI behind the
//!   Sluice `auth=sso` route; the gateway injects the verified `X-Auth-*` identity.
//! - **Internal ingest (`POST /api/notify`).** Service-to-service; its OWN `Bearer` token
//!   (`KLAXON_INGEST_TOKEN`), because the calling services are not browsers.
//!
//! Endpoints:
//! - `GET  /healthz`         liveness (container HEALTHCHECK)
//! - `POST /api/notify`      ingest (own Bearer): create a notification + fan out (best-effort)
//! - `GET  /`               inbox: this user's notifications (unread first) + push registration
//! - `POST /api/read`        mark one/all read (SSO, CSRF)
//! - `POST /api/subscribe`   store a Web Push subscription (SSO, CSRF)
//! - `GET  /api/stream`      Server-Sent-Events live stream of new notifications (SSO)
//! - `GET  /settings/webhooks`        list this user's webhooks + create form (SSO)
//! - `POST /settings/webhooks`        register a webhook (SSO, CSRF)
//! - `POST /settings/webhooks/delete` delete one of this user's webhooks (SSO, CSRF)
//! - `GET  /vapidPublicKey`  the configured VAPID public key, or empty

pub mod audit;
pub mod auth;
pub mod config;
pub mod delivery;
pub mod error;
pub mod handlers;
pub mod store;
pub mod webpush;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::routing::{get, post};
use axum::Router;
use rand::rngs::OsRng;
use rand::RngCore;

use crate::audit::AuditSink;
use crate::config::{env_nonempty, env_truthy, Config};
use crate::store::{InMemoryStore, PgStore, Store};

/// Shared application state. Cheap to clone (everything behind `Arc` / a cloneable sink).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Arc<dyn Store>,
    pub audit: AuditSink,
}

/// Build the router wiring all endpoints onto `state`. Routes are explicit (no fallback): the
/// service owns its subdomain, so Sluice forwards these exact paths.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handlers::health::healthz))
        .route("/vapidPublicKey", get(handlers::inbox::vapid_public_key))
        .route("/sw.js", get(handlers::inbox::service_worker))
        .route("/api/notify", post(handlers::notify::notify))
        .route("/", get(handlers::inbox::index))
        .route("/api/read", post(handlers::inbox::mark_read))
        .route("/api/subscribe", post(handlers::inbox::subscribe))
        .route("/api/stream", get(handlers::inbox::stream))
        .route(
            "/settings/webhooks",
            get(handlers::webhooks::index).post(handlers::webhooks::create),
        )
        .route("/settings/webhooks/delete", post(handlers::webhooks::delete))
        // Reject a forged gateway identity (spoofed X-Auth-* from a rogue in-network peer):
        // when GATEWAY_HMAC_KEY is set, an injected identity MUST carry a valid X-Auth-Sig.
        // No-op when the key is unset or no identity is present (health/ingest/dev).
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

/// Construct dev state: dev [`Config`], an empty [`InMemoryStore`], and a disabled audit sink (no
/// network). Tests reuse this shape and swap in their own pieces.
pub fn build_dev_state() -> AppState {
    AppState {
        config: Arc::new(Config::dev()),
        store: Arc::new(InMemoryStore::new()),
        audit: AuditSink::disabled(),
    }
}

/// Build runtime state from the environment.
///
/// The store is selected by `KLAXON_STORE`:
/// - `memory` (default): empty [`InMemoryStore`] — no database required.
/// - `postgres`: connect `KLAXON_DATABASE_URL`, run the idempotent migration, wire [`PgStore`].
///
/// The audit sink is enabled by `AUDIT_ENABLED` + `WATCHTOWER_URL` + `AUDIT_INGEST_TOKEN`. Returns
/// an error string on misconfiguration so `main` can fail loudly.
pub async fn build_state_from_env() -> Result<AppState, String> {
    let config = Config::from_env();
    let store_kind = env_nonempty("KLAXON_STORE").unwrap_or_else(|| "memory".to_string());

    let store: Arc<dyn Store> = match store_kind.as_str() {
        "postgres" => {
            let database_url = env_nonempty("KLAXON_DATABASE_URL")
                .ok_or_else(|| "KLAXON_STORE=postgres requires KLAXON_DATABASE_URL".to_string())?;
            tracing::info!("KLAXON_STORE=postgres — connecting to database");
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
        other => return Err(format!("unknown KLAXON_STORE={other} (use memory|postgres)")),
    };

    if config.ingest_token.is_none() {
        tracing::warn!("KLAXON_INGEST_TOKEN unset — /api/notify ingest is OPEN (dev mode)");
    }
    if config.push_configured() {
        tracing::info!("VAPID keypair present — Web Push is configured");
    } else {
        tracing::info!("VAPID keypair absent — Web Push degraded (subscriptions stored, intent recorded)");
    }

    let audit = AuditSink::start(
        env_truthy("AUDIT_ENABLED"),
        &env_nonempty("WATCHTOWER_URL").unwrap_or_default(),
        env_nonempty("AUDIT_INGEST_TOKEN").as_deref(),
    );

    Ok(AppState {
        config: Arc::new(config),
        store,
        audit,
    })
}

/// Current wall-clock time in epoch seconds (`created_at` / `read_at` granularity).
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs() as i64
}

/// Monotonic-ish millisecond stamp, used as the high bits of generated ids.
pub fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis()
}

/// A fresh, sortable id with the given prefix: `<prefix>_<millis>_<rand>`.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}_{}", now_millis(), random_alnum(8))
}

/// Generate a random URL-safe alphanumeric string of `len` characters from a 62-symbol alphabet,
/// via the OS CSPRNG. Used for ids and the double-submit CSRF token. The modulo over 62 introduces
/// a negligible bias that is irrelevant at these sizes.
pub fn random_alnum(len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut bytes = vec![0u8; len];
    OsRng.fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}
