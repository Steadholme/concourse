//! Atrium — unified activity inbox (one pane over chat + notifications + feeds) for the HOLDFAST
//! estate.
//!
//! Library root: defines [`AppState`], wires the routes via [`app`], and provides
//! [`build_dev_state`] (no sources — DB-free) and [`build_state_from_env`] (env-selected source
//! DSNs + Watchtower audit). Integration tests consume [`app`] directly via `tower::oneshot`,
//! exactly like the rest of the estate.
//!
//! Atrium is INTERNAL-ONLY and sits behind a Sluice `auth=sso` route at the subdomain ROOT
//! (`inbox.w33d.xyz`); the gateway forwards the path UNMODIFIED, so the routes below are the real
//! paths. It owns NO data: every column is a READ-ONLY federation of a sibling service's database
//! (Murmur chat / Klaxon notifications / Current feeds), scoped to the gateway-injected viewer.
//!
//! Endpoints:
//! - `GET /healthz`  liveness (container HEALTHCHECK), unauthenticated
//! - `GET /`         the unified dashboard: three columns (Chat unread / Notifications / Feed
//!                   river) + a summary bar of total unread, concurrently fetched, ~10 s cached,
//!                   and resilient (a down source renders "unavailable", the page still renders)
//! - `GET /api/inbox` the same viewer-scoped, cached aggregate as JSON (pre-rendered `summary` +
//!                   `columns` fragments) — the payload the page's ~20 s live-refresh poll swaps in
//!                   without a full reload (scroll preserved)

pub mod audit;
pub mod auth;
pub mod config;
pub mod csrf;
pub mod error;
pub mod handlers;
pub mod inbox;
pub mod source;
pub mod store;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::routing::{get, post};
use axum::Router;

use crate::audit::AuditSink;
use crate::config::Config;
use crate::inbox::{Engine, InboxCache, CACHE_TTL};
use crate::source::{CurrentSource, KlaxonSource, MurmurSource, Source};
use crate::store::{ActionStore, InMemoryActionStore, PgActionStore};

/// Shared application state. Cheap to clone (everything behind `Arc` / a cloneable sink+cache).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub engine: Arc<Engine>,
    pub cache: InboxCache,
    pub audit: AuditSink,
    /// The per-viewer mark-read / dismiss overlay (see [`crate::store`]).
    pub store: Arc<dyn ActionStore>,
}

/// Build the router wiring all endpoints onto `state`.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handlers::health::healthz))
        .route("/", get(handlers::dashboard::dashboard))
        // JSON feed for the dashboard's live auto-refresh poll (re-renders unread sections without
        // a full page reload). Same viewer-scoped, cached read as `/`, just no page chrome.
        .route("/api/inbox", get(handlers::api::api_inbox))
        // In-place row actions: mark-read / dismiss one aggregated row and re-render WITHOUT a full
        // reload. State-changing POSTs, so each is guarded by the double-submit CSRF check.
        .route("/api/inbox/read", post(handlers::actions::mark_read))
        .route("/api/inbox/dismiss", post(handlers::actions::dismiss))
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

/// Construct dev state: dev [`Config`] + an [`Engine`] with NO sources + a disabled audit sink.
/// Used by `main`'s zero-config boot and by tests, so they need no database and no network.
pub fn build_dev_state() -> AppState {
    AppState {
        config: Arc::new(Config::dev()),
        engine: Arc::new(Engine::default()),
        cache: InboxCache::new(CACHE_TTL),
        audit: AuditSink::disabled(),
        store: Arc::new(InMemoryActionStore::new()),
    }
}

/// Build a dev state around a pre-built [`Engine`] (used by handler tests with fake sources). The
/// action overlay is in-process, so tests can dismiss a row and observe it disappear.
pub fn build_state_with_engine(engine: Engine) -> AppState {
    AppState {
        config: Arc::new(Config::dev()),
        engine: Arc::new(engine),
        cache: InboxCache::new(CACHE_TTL),
        audit: AuditSink::disabled(),
        store: Arc::new(InMemoryActionStore::new()),
    }
}

/// Build runtime state from the environment.
///
/// [`Config`] comes from [`Config::from_env`]. For each configured per-source DSN
/// (`MURMUR_DATABASE_URL` / `KLAXON_DATABASE_URL` / `CURRENT_DATABASE_URL`) a lazily-connected,
/// READ-ONLY Postgres source is registered (so a down/unreachable source DB never blocks startup —
/// it is contained and rendered "unavailable" only when a query runs). An unset DSN means that
/// column simply renders empty. The audit sink is enabled by `AUDIT_ENABLED` + `WATCHTOWER_URL` +
/// `AUDIT_INGEST_TOKEN`. Returns an error string only on an unparseable DSN (loud misconfiguration).
pub async fn build_state_from_env() -> Result<AppState, String> {
    let config = Config::from_env();

    let chat: Option<Arc<dyn Source>> = match &config.murmur_dsn {
        Some(dsn) => {
            let pool = source::lazy_pool(dsn).map_err(|e| format!("MURMUR_DATABASE_URL: {e}"))?;
            tracing::info!("federating Chat source (Murmur, read-only)");
            Some(Arc::new(MurmurSource::new(pool)))
        }
        None => None,
    };
    let notifications: Option<Arc<dyn Source>> = match &config.klaxon_dsn {
        Some(dsn) => {
            let pool = source::lazy_pool(dsn).map_err(|e| format!("KLAXON_DATABASE_URL: {e}"))?;
            tracing::info!("federating Notifications source (Klaxon, read-only)");
            Some(Arc::new(KlaxonSource::new(pool)))
        }
        None => None,
    };
    let feed: Option<Arc<dyn Source>> = match &config.current_dsn {
        Some(dsn) => {
            let pool = source::lazy_pool(dsn).map_err(|e| format!("CURRENT_DATABASE_URL: {e}"))?;
            tracing::info!("federating Feed source (Current, read-only)");
            Some(Arc::new(CurrentSource::new(pool)))
        }
        None => None,
    };

    if chat.is_none() && notifications.is_none() && feed.is_none() {
        tracing::warn!(
            "no source DSNs configured — Atrium will render an empty inbox. Set \
             MURMUR_DATABASE_URL / KLAXON_DATABASE_URL / CURRENT_DATABASE_URL to federate."
        );
    }

    let audit = AuditSink::start(
        config::env_truthy("AUDIT_ENABLED"),
        &config::env_nonempty("WATCHTOWER_URL").unwrap_or_default(),
        config::env_nonempty("AUDIT_INGEST_TOKEN").as_deref(),
    );

    // The mark-read / dismiss overlay Atrium owns. A configured DSN backs it with Postgres (schema
    // ensured idempotently up front); if that bootstrap fails — or no DSN is set — fall back to the
    // in-process store so actions still work single-node and startup is never fatal.
    let store: Arc<dyn ActionStore> = match &config.atrium_dsn {
        Some(dsn) => match PgActionStore::connect(dsn).await {
            Ok(s) => {
                tracing::info!("action overlay backed by ATRIUM_DATABASE_URL (Postgres)");
                Arc::new(s)
            }
            Err(e) => {
                tracing::warn!(error = %e, "ATRIUM_DATABASE_URL unusable — actions overlay is in-process only");
                Arc::new(InMemoryActionStore::new())
            }
        },
        None => {
            tracing::info!("no ATRIUM_DATABASE_URL — actions overlay is in-process only");
            Arc::new(InMemoryActionStore::new())
        }
    };

    Ok(AppState {
        config: Arc::new(config),
        engine: Arc::new(Engine::new(chat, notifications, feed)),
        cache: InboxCache::new(CACHE_TTL),
        audit,
        store,
    })
}

/// Current wall-clock time in epoch seconds (used for the relative-time lines).
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs() as i64
}
