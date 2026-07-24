//! Audit delivery -> Watchtower.
//!
//! Mirrors the estate's canonical sink (keystone/sanctum): [`AuditSink`] holds a bounded
//! `tokio::mpsc` sender drained by a spawned worker that POSTs each event to
//! `WATCHTOWER_URL/events` (`Authorization: Bearer AUDIT_INGEST_TOKEN`) over plain HTTP/1.1 with
//! a short timeout. [`AuditSink::emit`] is sync + infallible: it `try_send`s and DROPS (warn +
//! counter) when the queue is full or the worker is gone, so a slow/down Watchtower NEVER blocks,
//! slows, or fails a request. When audit is disabled (the dev/test default) the sink is a no-op —
//! no channel, no worker.
//!
//! Durable consequences use [`AuditSink::deliver_direct`] from a background outbox poller. That
//! path waits for a definite Watchtower 2xx before the Store marks the consequence delivered.
//! This does not change the ordinary request-path [`AuditSink::emit`] behavior.
//!
//! Only the logical fields ride an event (actor / action / target / severity / detail); message
//! bodies and other user content NEVER do — `chat.message.send` records WHO sent to WHICH room
//! WHEN, never the message text.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// Bounded queue depth. Beyond this, events are dropped rather than blocking the caller.
const QUEUE_CAPACITY: usize = 1024;
/// Per-POST budget (connect + write + read). Watchtower is in-network; keep it short.
const POST_TIMEOUT: Duration = Duration::from_secs(2);
/// Fixed producer name stamped on every event this process emits.
const SOURCE: &str = "murmur";

/// One audit record. Only the logical fields are carried — Watchtower assigns seq/ts/hash.
/// `severity` is one of `info` | `notice` | `warning`.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEvent {
    pub actor: String,
    pub action: String,
    pub target: String,
    pub severity: &'static str,
    pub detail: String,
    pub source: &'static str,
}

impl AuditEvent {
    fn new(severity: &'static str, action: &str, actor: &str, target: &str, detail: &str) -> Self {
        AuditEvent {
            actor: actor.to_string(),
            action: action.to_string(),
            target: target.to_string(),
            severity,
            detail: detail.to_string(),
            source: SOURCE,
        }
    }

    /// `info`-severity event (normal lifecycle: message send, room create).
    pub fn info(action: &str, actor: &str, target: &str, detail: &str) -> Self {
        Self::new("info", action, actor, target, detail)
    }

    /// `notice`-severity event (a deliberate notable action).
    pub fn notice(action: &str, actor: &str, target: &str, detail: &str) -> Self {
        Self::new("notice", action, actor, target, detail)
    }

    /// `warning`-severity event (destructive / security-relevant).
    pub fn warning(action: &str, actor: &str, target: &str, detail: &str) -> Self {
        Self::new("warning", action, actor, target, detail)
    }
}

/// Non-blocking audit sink. Cheap to clone (shared sender + drop counter behind `Arc`). A
/// disabled sink (`inner == None`) makes [`emit`](Self::emit) a no-op.
#[derive(Clone)]
pub struct AuditSink {
    inner: Option<Inner>,
}

#[derive(Clone)]
struct Inner {
    tx: mpsc::Sender<AuditEvent>,
    dropped: Arc<AtomicU64>,
    target: Target,
    token: String,
}

/// A direct delivery did not receive a definite Watchtower success.
#[derive(Debug, Error)]
pub enum AuditDeliveryError {
    #[error("audit delivery is disabled")]
    Disabled,
    #[error("invalid audit idempotency key")]
    InvalidIdempotencyKey,
    #[error("serialize audit event: {0}")]
    Serialize(String),
    #[error("audit POST timed out")]
    Timeout,
    #[error("audit POST failed: {0}")]
    Io(String),
    #[error("watchtower rejected audit event with status {0}")]
    Rejected(u16),
}

impl AuditSink {
    /// Disabled sink: `emit` is a no-op, no channel and no worker. The dev/test default.
    pub fn disabled() -> Self {
        AuditSink { inner: None }
    }

    /// Build the sink from config and (when enabled) spawn the background worker.
    ///
    /// Returns a disabled sink when `enabled` is false, when no ingest `token` is set, or when
    /// `watchtower_url` is unparseable — those misconfigurations only warn and turn audit OFF;
    /// they never fail startup or affect the request path. Must be called from within a tokio
    /// runtime when enabled (the worker is `tokio::spawn`ed).
    pub fn start(enabled: bool, watchtower_url: &str, token: Option<&str>) -> Self {
        if !enabled {
            return Self::disabled();
        }
        let Some(token) = token.filter(|t| !t.is_empty()) else {
            tracing::warn!("AUDIT_ENABLED but AUDIT_INGEST_TOKEN is empty — audit disabled");
            return Self::disabled();
        };
        let Some(target) = Target::parse(watchtower_url) else {
            tracing::warn!(url = %watchtower_url, "invalid WATCHTOWER_URL — audit disabled");
            return Self::disabled();
        };

        let (tx, rx) = mpsc::channel::<AuditEvent>(QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let token = token.to_string();
        tokio::spawn(worker(rx, target.clone(), token.clone()));
        tracing::info!(url = %watchtower_url, "audit emitter enabled (watchtower)");
        AuditSink {
            inner: Some(Inner {
                tx,
                dropped,
                target,
                token,
            }),
        }
    }

    /// Emit one event. Sync, non-blocking, infallible: `try_send` only; on a full queue or a dead
    /// worker the event is DROPPED (drop counter + warn). NEVER blocks or errors the request path.
    pub fn emit(&self, event: AuditEvent) {
        let Some(inner) = &self.inner else { return };
        if let Err(e) = inner.tx.try_send(event) {
            let total = inner.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            match e {
                mpsc::error::TrySendError::Full(ev) => tracing::warn!(
                    action = %ev.action,
                    dropped_total = total,
                    "audit queue full — event dropped"
                ),
                mpsc::error::TrySendError::Closed(ev) => tracing::warn!(
                    action = %ev.action,
                    dropped_total = total,
                    "audit worker gone — event dropped"
                ),
            }
        }
    }

    /// Total events dropped so far (full queue or dead worker). `0` for a disabled sink.
    pub fn dropped(&self) -> u64 {
        self.inner
            .as_ref()
            .map(|i| i.dropped.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Whether this sink has a valid Watchtower target and ingest token.
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Deliver one event directly and return only after a definite Watchtower 2xx.
    ///
    /// This method is intended for durable outbox workers. It does not enqueue, drop, or mutate
    /// the ordinary [`emit`](Self::emit) counters. A timeout, transport failure, disabled sink, or
    /// non-2xx response is returned to the caller so its durable record remains pending.
    pub async fn deliver_direct(&self, event: &AuditEvent) -> Result<(), AuditDeliveryError> {
        self.deliver_direct_with_key(event, None).await
    }

    /// Deliver one durable event with a stable, server-generated idempotency key.
    ///
    /// Only canonical 64-character lowercase hex keys are accepted, preventing header injection
    /// and keeping the producer/Watchtower contract deliberately narrow.
    pub async fn deliver_direct_idempotent(
        &self,
        event: &AuditEvent,
        idempotency_key: &str,
    ) -> Result<(), AuditDeliveryError> {
        if idempotency_key.len() != 64
            || !idempotency_key
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(AuditDeliveryError::InvalidIdempotencyKey);
        }
        self.deliver_direct_with_key(event, Some(idempotency_key))
            .await
    }

    async fn deliver_direct_with_key(
        &self,
        event: &AuditEvent,
        idempotency_key: Option<&str>,
    ) -> Result<(), AuditDeliveryError> {
        let Some(inner) = &self.inner else {
            return Err(AuditDeliveryError::Disabled);
        };
        let body = serde_json::to_string(event)
            .map_err(|error| AuditDeliveryError::Serialize(error.to_string()))?;
        match tokio::time::timeout(
            POST_TIMEOUT,
            post(&inner.target, &inner.token, &body, idempotency_key),
        )
        .await
        {
            Ok(Ok(status)) if (200..300).contains(&status) => Ok(()),
            Ok(Ok(status)) => Err(AuditDeliveryError::Rejected(status)),
            Ok(Err(error)) => Err(AuditDeliveryError::Io(error.to_string())),
            Err(_) => Err(AuditDeliveryError::Timeout),
        }
    }
}

/// Drain the queue and POST each event. Delivery failures only warn — an event is never retried
/// and a down Watchtower never affects anything but this background task.
async fn worker(mut rx: mpsc::Receiver<AuditEvent>, target: Target, token: String) {
    while let Some(event) = rx.recv().await {
        let body = match serde_json::to_string(&event) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "audit event serialize failed — skipped");
                continue;
            }
        };
        match tokio::time::timeout(POST_TIMEOUT, post(&target, &token, &body, None)).await {
            Ok(Ok(status)) if (200..300).contains(&status) => {
                tracing::debug!(action = %event.action, status, "audit event delivered")
            }
            Ok(Ok(status)) => {
                tracing::warn!(action = %event.action, status, "watchtower rejected audit event")
            }
            Ok(Err(e)) => {
                tracing::warn!(action = %event.action, error = %e, "audit POST failed")
            }
            Err(_) => tracing::warn!(action = %event.action, "audit POST timed out"),
        }
    }
}

/// Send one `POST /events`. Hand-rolled HTTP/1.1 over a raw TCP stream (the target is the
/// internal plaintext `http://watchtower:8500`, so no TLS client is needed — mirrors the
/// dependency-free healthcheck probe in `main`). Returns the response status code.
async fn post(
    target: &Target,
    token: &str,
    body: &str,
    idempotency_key: Option<&str>,
) -> std::io::Result<u16> {
    let mut stream = TcpStream::connect((target.host.as_str(), target.port)).await?;
    let idempotency_header = idempotency_key
        .map(|key| format!("Idempotency-Key: {key}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {authority}\r\n\
         Authorization: Bearer {token}\r\n\
         {idempotency_header}\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n{body}",
        path = target.path,
        authority = target.authority,
        idempotency_header = idempotency_header,
        len = body.len(),
    );
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;
    let mut buf = Vec::with_capacity(256);
    stream.read_to_end(&mut buf).await?;
    parse_status(&buf)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no HTTP status line"))
}

/// Parse the numeric status from an HTTP response's first line (`HTTP/1.1 200 OK`).
fn parse_status(buf: &[u8]) -> Option<u16> {
    let line_end = buf.iter().position(|&b| b == b'\n').unwrap_or(buf.len());
    let line = std::str::from_utf8(&buf[..line_end]).ok()?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Parsed Watchtower target: where to connect (`host`/`port`), the `Host` header authority, and
/// the request `path` (`<base>/events`).
#[derive(Clone)]
struct Target {
    host: String,
    port: u16,
    authority: String,
    path: String,
}

impl Target {
    /// Parse `http://host[:port][/base]` into a connect target with `/events` appended. Only
    /// plain `http` is accepted (the internal hop is plaintext); anything else returns `None`.
    fn parse(url: &str) -> Option<Target> {
        let rest = url.strip_prefix("http://")?;
        let (authority, base_path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority.is_empty() {
            return None;
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => (authority.to_string(), 80u16),
        };
        if host.is_empty() {
            return None;
        }
        let base = base_path.trim_end_matches('/');
        Some(Target {
            host,
            port,
            authority: authority.to_string(),
            path: format!("{base}/events"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn disabled_sink_is_noop_and_never_drops() {
        let sink = AuditSink::disabled();
        for _ in 0..1000 {
            sink.emit(AuditEvent::info("chat.message.send", "a@b", "lobby", ""));
        }
        assert_eq!(sink.dropped(), 0);
    }

    #[test]
    fn target_parse_variants() {
        let t = Target::parse("http://watchtower:8500").unwrap();
        assert_eq!(t.host, "watchtower");
        assert_eq!(t.port, 8500);
        assert_eq!(t.authority, "watchtower:8500");
        assert_eq!(t.path, "/events");

        let t = Target::parse("http://host/base/").unwrap();
        assert_eq!(t.port, 80);
        assert_eq!(t.path, "/base/events");

        assert!(Target::parse("https://watchtower:8500").is_none());
        assert!(Target::parse("watchtower:8500").is_none());
        assert!(Target::parse("http://").is_none());
    }

    /// A send event serializes to exactly the safe shared fields — and NEVER the message body.
    #[test]
    fn send_serializes_without_body() {
        let ev = AuditEvent::info("chat.message.send", "alice@w33d.xyz", "lobby", "len=12");
        let json = serde_json::to_string(&ev).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["action", "actor", "detail", "severity", "source", "target"]
        );
        assert_eq!(v["action"], "chat.message.send");
        assert_eq!(v["source"], "murmur");
    }

    #[tokio::test]
    async fn emit_never_blocks_when_sink_unreachable() {
        let sink = AuditSink::start(true, "http://127.0.0.1:1/", Some("token"));
        for _ in 0..(QUEUE_CAPACITY * 8) {
            sink.emit(AuditEvent::info("chat.message.send", "u", "lobby", ""));
        }
        assert!(
            sink.dropped() > 0,
            "expected drops once the bounded queue saturated, got {}",
            sink.dropped()
        );
    }

    #[tokio::test]
    async fn direct_delivery_requires_a_2xx_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for status in ["503 Service Unavailable", "204 No Content"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 4096];
                let read = stream.read(&mut request).await.unwrap();
                assert!(String::from_utf8_lossy(&request[..read])
                    .contains("Authorization: Bearer test-token"));
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
        });
        let sink = AuditSink::start(true, &format!("http://{address}"), Some("test-token"));
        let event = AuditEvent::warning(
            "chat.admin.room.delete",
            "admin",
            "room-1",
            "consequence_id=stable-id",
        );

        assert!(matches!(
            sink.deliver_direct(&event).await,
            Err(AuditDeliveryError::Rejected(503))
        ));
        sink.deliver_direct(&event).await.unwrap();
        server.await.unwrap();
    }
}
