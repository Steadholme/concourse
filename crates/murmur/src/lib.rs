//! Murmur — real-time team chat / IM server for the Steadholme stack.
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
//! - `GET  /api/search`               search `?q=` over messages in the caller's member rooms (keyset)
//! - `GET  /api/mentions`             the caller's global @mentions view (keyset `?before=`)
//! - `POST /api/rooms/{id}/join`      join a room (CSRF)
//! - `POST /api/rooms/{id}/topic`     set the room topic `{topic}` (mods+, CSRF)
//! - `GET  /api/rooms/{id}/pinned`    the room's pinned-message panel (members only)
//! - `GET  /api/rooms/{id}/messages`  recent messages (keyset `?before=`)
//! - `POST /api/rooms/{id}/messages`  send `{body}` -> insert + broadcast (CSRF)
//! - `POST /api/rooms/{id}/messages/{msg_id}/edit`    author edits `{body}` -> update + broadcast (CSRF)
//! - `POST /api/rooms/{id}/messages/{msg_id}/delete`  author soft-deletes -> `[deleted]` + broadcast (CSRF)
//! - `POST /api/rooms/{id}/messages/{msg_id}/react`   toggle an emoji reaction -> counts + broadcast (CSRF)
//! - `GET  /api/rooms/{id}/messages/{msg_id}/reactions`  per-emoji reaction tallies + the caller's own
//! - `POST /api/rooms/{id}/messages/{msg_id}/pin`     pin a message in the room (mods+, CSRF)
//! - `POST /api/rooms/{id}/messages/{msg_id}/unpin`   unpin a message (mods+, CSRF)
//! - `POST /api/rooms/{id}/read`      advance the `(last_read_at,last_read_message_id)` marker
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
mod notify;
pub mod store;
pub mod text;
pub use notify::KlaxonNotifier;

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::routing::{get, post};
use axum::Router;

use crate::audit::{AuditEvent, AuditSink};
use crate::config::{env_nonempty, Config, LOBBY_ID, LOBBY_NAME};
use crate::hub::Hub;
use crate::store::{InMemoryStore, PgStore, Room, Store, StoreError};

const ROOM_DELETE_AUDIT_BATCH_SIZE: i64 = 64;
const ROOM_DELETE_AUDIT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Shared application state. Cheap to clone (everything behind `Arc` / a cloneable sink).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Arc<dyn Store>,
    pub hub: Arc<Hub>,
    pub audit: AuditSink,
    pub klaxon: Option<Arc<KlaxonNotifier>>,
}

/// Frozen Patchbay v1 route surface. This is review/test evidence only; the router below remains
/// the sole runtime authority.
pub const ROUTE_SNAPSHOT_V1: &[(&str, &str)] = &[
    ("GET", "/healthz"),
    ("GET", "/"),
    ("GET", "/api/rooms"),
    ("POST", "/api/rooms"),
    ("GET", "/api/directory"),
    ("POST", "/api/dms"),
    ("GET", "/api/search"),
    ("GET", "/api/mentions"),
    ("POST", "/api/rooms/{id}/join"),
    ("POST", "/api/rooms/{id}/topic"),
    ("GET", "/api/rooms/{id}/pinned"),
    ("GET", "/api/rooms/{id}/messages"),
    ("POST", "/api/rooms/{id}/messages"),
    ("POST", "/api/rooms/{id}/messages/{msg_id}/edit"),
    ("POST", "/api/rooms/{id}/messages/{msg_id}/delete"),
    ("POST", "/api/rooms/{id}/messages/{msg_id}/react"),
    ("GET", "/api/rooms/{id}/messages/{msg_id}/reactions"),
    ("POST", "/api/rooms/{id}/messages/{msg_id}/pin"),
    ("POST", "/api/rooms/{id}/messages/{msg_id}/unpin"),
    ("POST", "/api/rooms/{id}/read"),
    ("GET", "/ws"),
    ("GET", "/admin"),
    ("GET", "/admin/rooms/{id}"),
    ("POST", "/admin/rooms/{id}/archive"),
    ("POST", "/admin/rooms/{id}/delete"),
    ("POST", "/admin/rooms/{id}/members/{user_sub}/remove"),
    ("POST", "/admin/rooms/{id}/members/{user_sub}/ban"),
    ("POST", "/admin/messages/{msg_id}/redact"),
];

/// Build the router wiring all endpoints onto `state`. Routes are explicit (no fallback): the
/// service owns its subdomain, so Sluice forwards these exact paths.
pub fn app(state: AppState) -> Router {
    let security_config = state.config.clone();
    Router::new()
        .route("/healthz", get(handlers::health::healthz))
        .route("/", get(handlers::dashboard::index))
        .route(
            "/api/rooms",
            get(handlers::rooms::list).post(handlers::rooms::create),
        )
        .route("/api/directory", get(handlers::dms::directory))
        .route("/api/dms", post(handlers::dms::open))
        // Cross-room discovery: search over member rooms + the global @mentions view.
        .route("/api/search", get(handlers::search::search))
        .route("/api/mentions", get(handlers::search::mentions))
        .route("/api/rooms/{id}/join", post(handlers::rooms::join))
        .route("/api/rooms/{id}/topic", post(handlers::rooms::set_topic))
        .route("/api/rooms/{id}/pinned", get(handlers::rooms::pinned))
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
        .route(
            "/api/rooms/{id}/messages/{msg_id}/pin",
            post(handlers::rooms::pin),
        )
        .route(
            "/api/rooms/{id}/messages/{msg_id}/unpin",
            post(handlers::rooms::unpin),
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
        .route(
            "/admin/rooms/{id}/delete",
            post(handlers::admin::delete_room),
        )
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
        // Exact `/healthz` is public. Every other outcome first requires a non-empty subject and,
        // in production, a valid gateway HMAC. The same boundary applies privacy headers to HTML,
        // JSON, redirects and framework-generated errors.
        .layer(axum::middleware::from_fn_with_state(
            security_config,
            require_gateway_identity,
        ))
        .with_state(state)
}

/// Fail-closed identity and privacy boundary for every route except exact `/healthz`.
async fn require_gateway_identity(
    axum::extract::State(config): axum::extract::State<Arc<Config>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.method() == axum::http::Method::GET && req.uri().path() == "/healthz" {
        return next.run(req).await;
    }

    let signature_ok = auth::gateway_identity_ok(req.headers(), config.gateway_hmac_key.as_deref());
    let identity_present = signature_ok && auth::require_user(req.headers()).is_ok();
    let origin_ok = req.uri().path() != "/ws"
        || auth::verify_websocket_origin(req.headers(), config.websocket_origin.as_deref()).is_ok();
    let mut response = if identity_present && signature_ok && origin_ok {
        next.run(req).await
    } else if !origin_ok {
        crate::error::AppError::Forbidden("websocket origin is not allowed".to_string())
            .into_response()
    } else {
        crate::error::AppError::Unauthorized("invalid gateway identity".to_string()).into_response()
    };
    apply_private_headers(&mut response);
    response
}

fn apply_private_headers(response: &mut axum::response::Response) {
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("private, no-store"),
    );
    response.headers_mut().insert(
        axum::http::header::REFERRER_POLICY,
        axum::http::HeaderValue::from_static("no-referrer"),
    );
}

/// Construct dev state: dev [`Config`], an empty [`InMemoryStore`], a fresh [`Hub`], and a
/// disabled audit sink (no network). Tests reuse this shape.
pub fn build_dev_state() -> AppState {
    AppState {
        config: Arc::new(Config::dev()),
        store: Arc::new(InMemoryStore::new()),
        hub: Arc::new(Hub::new()),
        audit: AuditSink::disabled(),
        klaxon: None,
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
    let config = Config::from_env()?;
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
        other => {
            return Err(format!(
                "unknown MURMUR_STORE={other} (use memory|postgres)"
            ))
        }
    };

    let audit = AuditSink::start(
        env_truthy("AUDIT_ENABLED"),
        &env_nonempty("WATCHTOWER_URL").unwrap_or_default(),
        env_nonempty("AUDIT_INGEST_TOKEN").as_deref(),
    );
    start_room_delete_audit_poller(store.clone(), audit.clone());

    Ok(AppState {
        config: Arc::new(config),
        store,
        hub: Arc::new(Hub::new()),
        audit,
        klaxon: KlaxonNotifier::from_env().map(Arc::new),
    })
}

/// Start the production-only durable hard-delete audit outbox worker.
///
/// A disabled/misconfigured sink leaves every outbox row pending for the next correctly
/// configured process start. An enabled sink polls at a fixed interval, so Watchtower failures
/// cannot create a busy retry loop.
pub fn start_room_delete_audit_poller(store: Arc<dyn Store>, audit: AuditSink) {
    if !audit.is_enabled() {
        tracing::warn!("durable room-delete audit poller disabled — outbox rows remain pending");
        return;
    }
    tokio::spawn(async move {
        loop {
            if let Err(error) = deliver_pending_room_delete_audits_once(&store, &audit).await {
                tracing::warn!(
                    error = %error,
                    "query or acknowledge durable room-delete audit outbox failed"
                );
            }
            tokio::time::sleep(ROOM_DELETE_AUDIT_POLL_INTERVAL).await;
        }
    });
}

/// Attempt one bounded, oldest-first outbox batch.
///
/// Delivery is at-least-once: a Watchtower 2xx happens before the Store acknowledgement. If that
/// acknowledgement fails, the same stable `consequence_id` remains visible in the next delivery
/// attempt so Watchtower can deduplicate it.
async fn deliver_pending_room_delete_audits_once(
    store: &Arc<dyn Store>,
    audit: &AuditSink,
) -> Result<usize, StoreError> {
    let pending = store
        .pending_room_delete_audits(ROOM_DELETE_AUDIT_BATCH_SIZE)
        .await?;
    let mut delivered = 0;
    for consequence in pending {
        let event = AuditEvent::warning(
            "chat.admin.room.delete",
            &consequence.actor_sub,
            &consequence.room_id,
            &format!(
                "consequence_id={};occurred_at={}",
                consequence.id, consequence.occurred_at
            ),
        );
        if let Err(error) = audit
            .deliver_direct_idempotent(&event, &consequence.id)
            .await
        {
            tracing::warn!(
                audit_consequence_id = %consequence.id,
                room_id = %consequence.room_id,
                error = %error,
                "durable room-delete audit delivery failed — consequence remains pending"
            );
            break;
        }
        store
            .mark_room_delete_audit_delivered(&consequence.id, now_secs())
            .await?;
        delivered += 1;
        tracing::info!(
            audit_consequence_id = %consequence.id,
            room_id = %consequence.room_id,
            "durable room-delete audit delivered"
        );
    }
    Ok(delivered)
}

/// Ensure the global `#lobby` exists and that `(sub, email)` is a member of it. Called on the
/// first dashboard / room-list / WS touch so the UI is never empty. Idempotent; a failure is
/// propagated because callers must not misreport an unavailable Store as an empty room list.
pub async fn ensure_lobby(state: &AppState, sub: &str, email: &str) -> Result<(), StoreError> {
    let now = now_secs();
    let lobby = Room {
        id: LOBBY_ID.to_string(),
        name: LOBBY_NAME.to_string(),
        kind: "room".to_string(),
        created_by: "system".to_string(),
        created_at: 0, // created_at=0 keeps the lobby first in the oldest-first room ordering.
        archived: false,
        topic: String::new(),
    };
    state.store.ensure_room(&lobby).await?;
    state
        .store
        .ensure_membership(LOBBY_ID, sub, email, now)
        .await?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use axum::response::IntoResponse;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    use super::*;

    #[test]
    fn privacy_headers_apply_to_unavailable_responses() {
        let mut response =
            crate::error::AppError::Unavailable("backend detail".to_string()).into_response();
        apply_private_headers(&mut response);
        assert_eq!(
            response.headers()[axum::http::header::CACHE_CONTROL],
            "private, no-store"
        );
        assert_eq!(
            response.headers()[axum::http::header::REFERRER_POLICY],
            "no-referrer"
        );
    }

    #[tokio::test]
    async fn durable_delete_audit_is_marked_only_after_direct_success() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (body_tx, body_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for status in ["503 Service Unavailable", "204 No Content"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 4096];
                let read = stream.read(&mut request).await.unwrap();
                requests.push(String::from_utf8_lossy(&request[..read]).to_string());
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
            body_tx.send(requests).unwrap();
        });

        let concrete_store = Arc::new(InMemoryStore::new());
        concrete_store
            .ensure_room(&Room {
                id: "room-delete".to_string(),
                name: "#delete".to_string(),
                kind: "room".to_string(),
                created_by: "admin".to_string(),
                created_at: 1,
                archived: false,
                topic: String::new(),
            })
            .await
            .unwrap();
        concrete_store
            .issue_room_delete_token("digest", "room-delete", "admin", "csrf", 1, 100)
            .await
            .unwrap();
        let consequence = concrete_store
            .delete_room_with_token("room-delete", "digest", "admin", "csrf", 50)
            .await
            .unwrap();
        let store: Arc<dyn Store> = concrete_store.clone();
        let sink = AuditSink::start(true, &format!("http://{address}"), Some("test-token"));

        assert_eq!(
            deliver_pending_room_delete_audits_once(&store, &sink)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            concrete_store.pending_room_delete_audits(10).await.unwrap(),
            vec![consequence.clone()]
        );
        assert_eq!(
            deliver_pending_room_delete_audits_once(&store, &sink)
                .await
                .unwrap(),
            1
        );
        assert!(concrete_store
            .pending_room_delete_audits(10)
            .await
            .unwrap()
            .is_empty());
        let requests = body_rx.await.unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests {
            assert!(request.contains("\"action\":\"chat.admin.room.delete\""));
            assert!(request.contains(&format!("consequence_id={}", consequence.id)));
            assert!(request.contains(&format!("Idempotency-Key: {}", consequence.id)));
        }
        server.await.unwrap();
    }
}
