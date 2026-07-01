//! Murmur — real-time team chat / IM server for the HOLDFAST stack.
//!
//! Library root: defines [`AppState`], wires the routes via [`app`], and provides
//! [`build_dev_state`] (in-memory store + disabled audit, no database) and
//! [`build_state_from_env`] (env-selected store + Watchtower audit). Integration tests consume
//! [`app`] directly via `tower::oneshot`, exactly like the rest of the estate.
//!
//! Murmur sits behind a Sluice `auth=sso` route at the subdomain ROOT (`chat.w33d.xyz`); the
//! gateway forwards the path UNMODIFIED, so the routes below are the real paths. Every endpoint
//! is gateway-authenticated SSO (there is NO separate protocol sub-path with its own auth — the
//! WebSocket is upgraded on the same SSO-gated origin).
//!
//! Endpoints:
//! - `GET  /healthz`                  liveness (public; container HEALTHCHECK)
//! - `GET  /`                         dashboard: room list + timeline + composer + live `/ws`
//! - `GET  /api/rooms`                rooms this user belongs to (+ auto-joined `#lobby`)
//! - `POST /api/rooms`                create a room `{name, kind}` (CSRF)
//! - `GET  /api/directory`            people the caller can DM (distinct known subjects/emails)
//! - `POST /api/dms`                  open/reuse a 1:1 DM room `{subject, email}` (CSRF)
//! - `POST /api/rooms/{id}/join`      join a room (CSRF)
//! - `GET  /api/rooms/{id}/messages`  recent messages (keyset `?before=`)
//! - `POST /api/rooms/{id}/messages`  send `{body}` -> insert + broadcast (CSRF)
//! - `POST /api/rooms/{id}/messages/{msg_id}/edit`    author edits `{body}` -> update + broadcast (CSRF)
//! - `POST /api/rooms/{id}/messages/{msg_id}/delete`  author soft-deletes -> `[deleted]` + broadcast (CSRF)
//! - `POST /api/rooms/{id}/messages/{msg_id}/react`   toggle an emoji reaction -> counts + broadcast (CSRF)
//! - `GET  /api/rooms/{id}/messages/{msg_id}/reactions`  per-emoji reaction tallies + the caller's own
//! - `POST /api/rooms/{id}/read`      advance `last_read_at`
//! - `GET  /ws`                       WebSocket: live messages/presence for the user's rooms
//! - `GET  /admin`                    moderator panel: all rooms (admins/infra-admins only)
//! - `GET  /admin/rooms/{id}`         room detail: members + messages with per-row controls
//! - `POST /admin/rooms/{id}/archive` archive a room (CSRF)
//! - `POST /admin/rooms/{id}/delete`  hard-delete a room + its members/messages (CSRF)
//! - `POST /admin/rooms/{id}/members/{user_sub}/remove`  kick a member (CSRF)
//! - `POST /admin/rooms/{id}/members/{user_sub}/ban`     ban a member (CSRF)
//! - `POST /admin/messages/{msg_id}/redact`              redact ANY message (CSRF)

pub mod audit;
pub mod auth;
pub mod config;
pub mod error;
pub mod handlers;
pub mod hub;
pub mod store;
pub mod text;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::routing::{get, post};
use axum::Router;

use crate::audit::AuditSink;
use crate::config::{env_nonempty, Config, LOBBY_ID, LOBBY_NAME};
use crate::hub::Hub;
use crate::store::{InMemoryStore, PgStore, Room, Store};

/// Shared application state. Cheap to clone (everything behind `Arc` / a cloneable sink).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Arc<dyn Store>,
    pub hub: Arc<Hub>,
    pub audit: AuditSink,
}

/// Build the router wiring all endpoints onto `state`. Routes are explicit (no fallback): the
/// service owns its subdomain, so Sluice forwards these exact paths.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handlers::health::healthz))
        .route("/", get(handlers::dashboard::index))
        .route(
            "/api/rooms",
            get(handlers::rooms::list).post(handlers::rooms::create),
        )
        .route("/api/directory", get(handlers::dms::directory))
        .route("/api/dms", post(handlers::dms::open))
        .route("/api/rooms/{id}/join", post(handlers::rooms::join))
        .route(
            "/api/rooms/{id}/messages",
            get(handlers::rooms::messages).post(handlers::rooms::send),
        )
        .route(
            "/api/rooms/{id}/messages/{msg_id}/edit",
            post(handlers::rooms::edit_message),
        )
        .route(
            "/api/rooms/{id}/messages/{msg_id}/delete",
            post(handlers::rooms::delete_message),
        )
        .route(
            "/api/rooms/{id}/messages/{msg_id}/react",
            post(handlers::rooms::react),
        )
        .route(
            "/api/rooms/{id}/messages/{msg_id}/reactions",
            get(handlers::rooms::reactions),
        )
        .route("/api/rooms/{id}/read", post(handlers::rooms::read))
        .route("/ws", get(handlers::ws::ws_handler))
        // --- admin subtree (gated by `require_admin`: admins / infra-admins only) ---
        .route("/admin", get(handlers::admin::index))
        .route("/admin/rooms/{id}", get(handlers::admin::room_detail))
        .route(
            "/admin/rooms/{id}/archive",
            post(handlers::admin::archive_room),
        )
        .route("/admin/rooms/{id}/delete", post(handlers::admin::delete_room))
        .route(
            "/admin/rooms/{id}/members/{user_sub}/remove",
            post(handlers::admin::remove_member),
        )
        .route(
            "/admin/rooms/{id}/members/{user_sub}/ban",
            post(handlers::admin::ban_member),
        )
        .route(
            "/admin/messages/{msg_id}/redact",
            post(handlers::admin::redact_message),
        )
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

/// Construct dev state: dev [`Config`], an empty [`InMemoryStore`], a fresh [`Hub`], and a
/// disabled audit sink (no network). Tests reuse this shape.
pub fn build_dev_state() -> AppState {
    AppState {
        config: Arc::new(Config::dev()),
        store: Arc::new(InMemoryStore::new()),
        hub: Arc::new(Hub::new()),
        audit: AuditSink::disabled(),
    }
}

/// Build runtime state from the environment.
///
/// The store is selected by `MURMUR_STORE`:
/// - `memory` (default): empty [`InMemoryStore`] — no database required.
/// - `postgres`: connect `MURMUR_DATABASE_URL`, run the idempotent migration, wire [`PgStore`].
///
/// The audit sink is enabled by `AUDIT_ENABLED` + `WATCHTOWER_URL` + `AUDIT_INGEST_TOKEN`.
/// Returns an error string on misconfiguration so `main` can fail loudly.
pub async fn build_state_from_env() -> Result<AppState, String> {
    let config = Config::from_env();
    let store_kind = env_nonempty("MURMUR_STORE").unwrap_or_else(|| "memory".to_string());

    let store: Arc<dyn Store> = match store_kind.as_str() {
        "postgres" => {
            let database_url = env_nonempty("MURMUR_DATABASE_URL")
                .ok_or_else(|| "MURMUR_STORE=postgres requires MURMUR_DATABASE_URL".to_string())?;
            tracing::info!("MURMUR_STORE=postgres — connecting to database");
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
        other => return Err(format!("unknown MURMUR_STORE={other} (use memory|postgres)")),
    };

    let audit = AuditSink::start(
        env_truthy("AUDIT_ENABLED"),
        &env_nonempty("WATCHTOWER_URL").unwrap_or_default(),
        env_nonempty("AUDIT_INGEST_TOKEN").as_deref(),
    );

    Ok(AppState {
        config: Arc::new(config),
        store,
        hub: Arc::new(Hub::new()),
        audit,
    })
}

/// Ensure the global `#lobby` exists and that `(sub, email)` is a member of it. Called on the
/// first dashboard / room-list / WS touch so the UI is never empty. Idempotent + best-effort:
/// store errors are logged, never surfaced (a transient DB hiccup must not 500 the dashboard).
pub async fn ensure_lobby(state: &AppState, sub: &str, email: &str) {
    let now = now_secs();
    let lobby = Room {
        id: LOBBY_ID.to_string(),
        name: LOBBY_NAME.to_string(),
        kind: "room".to_string(),
        created_by: "system".to_string(),
        created_at: 0, // created_at=0 keeps the lobby first in the oldest-first room ordering.
        archived: false,
    };
    if let Err(e) = state.store.ensure_room(&lobby).await {
        tracing::warn!(error = %e, "ensure lobby room failed");
    }
    if let Err(e) = state
        .store
        .ensure_membership(LOBBY_ID, sub, email, now)
        .await
    {
        tracing::warn!(error = %e, "ensure lobby membership failed");
    }
}

/// Interpret a boolean-ish env var (`on` / `true` / `1` / `yes`, case-insensitive).
fn env_truthy(key: &str) -> bool {
    matches!(
        std::env::var(key)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "on" | "true" | "1" | "yes"
    )
}

/// Current wall-clock time in epoch seconds (room/message `created_at`, membership timestamps).
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs() as i64
}

/// Monotonic-ish nanosecond counter for unique room/message ids.
pub fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_nanos()
}
