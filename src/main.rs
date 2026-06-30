//! Concourse — one container hosting the HOLDFAST communication surfaces (chat / calendar /
//! notifications / inbox).
//!
//! Each surface is its OWN library crate (Murmur/Almanac/Klaxon/Atrium), reused verbatim: same
//! schema, same routes, same templates, same OWN database (Atrium owns none — it read-only
//! federates the others), same subdomain, same tokens. This binary only adds a **Host-based vhost
//! demux** so the estate runs ONE deployable instead of four. Sluice points `chat.w33d.xyz`,
//! `cal.w33d.xyz`, `notify.w33d.xyz` and `inbox.w33d.xyz` all at this container; each request is
//! dispatched to the matching surface's router by its `Host` header.
//!
//! **Klaxon has TWO ingress names and BOTH must demux to it:**
//! - `notify.w33d.xyz` — the SSO inbox console (gateway-authenticated browser UI).
//! - bare `klaxon[:9050]` — the internal service-to-service ingest: tempo / pulse / vitals POST to
//!   `http://klaxon:9050/api/notify` with Klaxon's OWN `Bearer` (`KLAXON_INGEST_TOKEN`).
//!
//! Klaxon's OWN router decides SSO-console vs Bearer-ingest per path; the demux only forwards by
//! host. A missing `klaxon` alias/label would silently kill estate-wide notifications, so both
//! labels map to the same router. An unknown host is a 404.
//!
//! Each surface's WebSocket (`chat.w33d.xyz/ws`), SSE stream, VAPID/webhook/SMTP egress and audit
//! emitter are preserved: the full request (headers + body, including the WS HTTP-upgrade) is
//! forwarded to the surface's own router via `oneshot`, so every per-surface behaviour is evaluated
//! exactly as it was standalone.
//!
//! `healthcheck` subcommand: a dependency-free loopback `GET /healthz` (host-agnostic) used as the
//! container HEALTHCHECK, so the image needs no curl.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tower::ServiceExt;

/// Default listen address — internal-only; Sluice fronts the four subdomains at this upstream, and
/// the internal callers reach Klaxon at `klaxon:9050`.
const DEFAULT_BIND_ADDR: &str = "0.0.0.0:9050";

/// The four composed per-surface routers, dispatched by Host. Cheap to clone (each `Router` is
/// `Arc`-backed internally).
#[derive(Clone)]
struct Vhosts {
    chat: Router,
    cal: Router,
    notify: Router,
    inbox: Router,
}

#[tokio::main]
async fn main() {
    // Container HEALTHCHECK path — handled before any setup, exits the process.
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        std::process::exit(run_healthcheck());
    }

    tracing_subscriber::fmt::init();

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string());

    // Each owned surface connects to its OWN database and migrates idempotently — exactly what the
    // standalone service did. A failure here is fatal (the surface cannot serve without its DB).
    let chat = build_chat().await.unwrap_or_else(|e| fatal("chat (murmur)", e));
    let cal = build_cal().await.unwrap_or_else(|e| fatal("cal (almanac)", e));
    let notify = build_notify()
        .await
        .unwrap_or_else(|e| fatal("notify (klaxon)", e));
    let inbox = build_inbox()
        .await
        .unwrap_or_else(|e| fatal("inbox (atrium)", e));

    let app = Router::new()
        // Host-agnostic liveness for the container HEALTHCHECK + estate BEACON probes.
        .route("/healthz", get(|| async { "ok" }))
        .fallback(dispatch)
        .with_state(Vhosts {
            chat,
            cal,
            notify,
            inbox,
        });

    let addr: SocketAddr = bind_addr.parse().expect("invalid BIND_ADDR");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    tracing::info!(%addr, "Concourse listening (chat/cal/notify/inbox vhost demux)");
    axum::serve(listener, app).await.expect("server error");
}

/// Dispatch one request to the surface matching its `Host` header. An unknown host is a 404 — we
/// never silently serve one surface under another's vhost.
///
/// Klaxon is reachable under TWO labels: the gateway subdomain `notify` (SSO console) AND the bare
/// internal service name `klaxon` (Bearer ingest from tempo/pulse/vitals). Both forward to the SAME
/// router, which applies the correct per-path auth itself.
async fn dispatch(State(v): State<Vhosts>, req: Request) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    // Match on the leading label, ignoring any port.
    let label = host
        .split(':')
        .next()
        .unwrap_or("")
        .split('.')
        .next()
        .unwrap_or("");
    let router = match label {
        "chat" => v.chat,
        "cal" => v.cal,
        // BOTH the gateway subdomain (`notify`) and the bare internal name (`klaxon`) -> Klaxon.
        "notify" | "klaxon" => v.notify,
        "inbox" => v.inbox,
        _ => return (StatusCode::NOT_FOUND, "unknown communication host").into_response(),
    };
    // `Router` is a tower `Service` (the exact `app(state).oneshot(req)` path the surfaces' own
    // tests use); its error type is `Infallible`. The HTTP-upgrade WS request rides this path too.
    match router.oneshot(req).await {
        Ok(resp) => resp,
        Err(e) => match e {},
    }
}

/// Build the chat (Murmur) surface router against `MURMUR_DATABASE_URL`.
///
/// State is built EXPLICITLY (not via `murmur::build_state_from_env`, which is gated on
/// `MURMUR_STORE` and would otherwise default to the in-memory store): connect + migrate the OWN
/// database, start a fresh in-process fan-out `Hub` (backs `GET /ws`), and start Murmur's OWN audit
/// emitter exactly as it does (`AUDIT_ENABLED` + `WATCHTOWER_URL` + `AUDIT_INGEST_TOKEN`).
async fn build_chat() -> Result<Router, String> {
    let dsn = require_env("MURMUR_DATABASE_URL")?;
    let pg = murmur::store::PgStore::connect(&dsn)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    pg.migrate().await.map_err(|e| format!("migrate: {e}"))?;
    tracing::info!("chat (murmur) store ready");
    let audit = murmur::audit::AuditSink::start(
        env_truthy("AUDIT_ENABLED"),
        &murmur::config::env_nonempty("WATCHTOWER_URL").unwrap_or_default(),
        murmur::config::env_nonempty("AUDIT_INGEST_TOKEN").as_deref(),
    );
    let state = murmur::AppState {
        config: Arc::new(murmur::config::Config::from_env()),
        store: Arc::new(pg),
        hub: Arc::new(murmur::hub::Hub::new()),
        audit,
    };
    Ok(murmur::app(state))
}

/// Build the calendar (Almanac) surface router against `ALMANAC_DATABASE_URL`.
///
/// Almanac's own `build_state_from_env` read the BARE `DATABASE_URL` for its store (collides when
/// several surfaces share one process), so the vendored copy was fixed to `ALMANAC_DATABASE_URL`
/// (crates/almanac/src/lib.rs) and here we connect that OWN DSN explicitly. Almanac has no audit
/// sink and no background task.
async fn build_cal() -> Result<Router, String> {
    let dsn = require_env("ALMANAC_DATABASE_URL")?;
    let pg = almanac::store::PgStore::connect(&dsn)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    pg.migrate().await.map_err(|e| format!("migrate: {e}"))?;
    tracing::info!("cal (almanac) store ready");
    let state = almanac::AppState {
        config: Arc::new(almanac::config::Config::from_env()),
        store: Arc::new(pg),
    };
    Ok(almanac::app(state))
}

/// Build the notifications (Klaxon) surface router against `KLAXON_DATABASE_URL`.
///
/// State is built EXPLICITLY (not via `klaxon::build_state_from_env`, which is gated on
/// `KLAXON_STORE`): connect + migrate the OWN database and start Klaxon's OWN audit emitter. The
/// VAPID keypair, webhook egress, and optional SMTP channel are all carried on `Config::from_env`
/// (`KLAXON_INGEST_TOKEN` / `VAPID_*` / `KLAXON_SMTP_*`), so Bearer-guarded `POST /api/notify`,
/// Web-Push registration and the per-notify fan-out task all behave exactly as standalone. The
/// router serves BOTH the SSO console (under `notify`) and the internal Bearer ingest (under bare
/// `klaxon`) — see [`dispatch`].
async fn build_notify() -> Result<Router, String> {
    let dsn = require_env("KLAXON_DATABASE_URL")?;
    let pg = klaxon::store::PgStore::connect(&dsn)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    pg.migrate().await.map_err(|e| format!("migrate: {e}"))?;
    tracing::info!("notify (klaxon) store ready");
    let config = klaxon::config::Config::from_env();
    if config.ingest_token.is_none() {
        tracing::warn!("KLAXON_INGEST_TOKEN unset — /api/notify ingest is OPEN (dev mode)");
    }
    let audit = klaxon::audit::AuditSink::start(
        env_truthy("AUDIT_ENABLED"),
        &klaxon::config::env_nonempty("WATCHTOWER_URL").unwrap_or_default(),
        klaxon::config::env_nonempty("AUDIT_INGEST_TOKEN").as_deref(),
    );
    let state = klaxon::AppState {
        config: Arc::new(config),
        store: Arc::new(pg),
        audit,
    };
    Ok(klaxon::app(state))
}

/// Build the inbox (Atrium) surface router. Atrium owns NO database: it read-only federates the
/// Murmur / Klaxon / Current databases per-viewer. Its `build_state_from_env` reads ONLY the
/// prefixed federation DSNs (`MURMUR_DATABASE_URL` / `KLAXON_DATABASE_URL` / `CURRENT_DATABASE_URL`)
/// — never the bare `DATABASE_URL` — so there is no in-process collision and it is reused verbatim.
/// It registers lazy read-only pools (a down source DB never blocks startup) and starts its own
/// audit emitter.
async fn build_inbox() -> Result<Router, String> {
    let state = atrium::build_state_from_env().await?;
    tracing::info!("inbox (atrium) federation ready");
    Ok(atrium::app(state))
}

/// Interpret a boolean-ish env var (`on` / `true` / `1` / `yes`, case-insensitive). Mirrors the
/// private `env_truthy` each surface uses in its own `build_state_from_env`, so audit is gated
/// identically.
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

/// Read a required env var, returning a descriptive error when unset/empty.
fn require_env(key: &str) -> Result<String, String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(format!("{key} is required")),
    }
}

/// Log a fatal startup error for one surface and exit.
fn fatal(surface: &str, err: String) -> ! {
    tracing::error!(surface, error = %err, "failed to build communication surface");
    std::process::exit(1);
}

/// GET `/healthz` over a raw TCP socket on the loopback. Returns the process exit code.
fn run_healthcheck() -> i32 {
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string());
    let port = bind_addr.rsplit(':').next().unwrap_or("9050");
    let target = format!("127.0.0.1:{port}");
    match healthcheck_once(&target) {
        Ok(true) => 0,
        Ok(false) => {
            eprintln!("healthcheck: {target} did not return 200");
            1
        }
        Err(e) => {
            eprintln!("healthcheck: {target} error: {e}");
            1
        }
    }
}

fn healthcheck_once(target: &str) -> std::io::Result<bool> {
    let addr: SocketAddr = target
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{e}")))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf)?;
    Ok(buf.lines().next().unwrap_or("").contains("200"))
}
