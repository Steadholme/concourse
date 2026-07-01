//! Best-effort, non-blocking fan-out of a stored notification.
//!
//! After a notification row is committed, [`fan_out`] spawns a detached task that delivers the
//! event to the recipient's registered channels and then emits a `notify.fanout` audit event. The
//! task is fire-and-forget: a slow or dead downstream (a webhook host, a browser push service, the
//! mail server) NEVER blocks, slows, or fails the ingest request — exactly like the audit emitter.
//!
//! Channels:
//! - **Webhooks.** A JSON `POST` of the notification to each registered `webhooks.url`, with an
//!   `X-Klaxon-Source` header and — when the webhook has a stored secret — an
//!   `X-Klaxon-Signature: sha256=<hex>` HMAC-SHA256 of the exact body. `http://` targets go over a
//!   raw TCP HTTP/1.1 write; `https://` targets go over a rustls (ring) TLS client. Each delivery
//!   gets a per-attempt timeout and is retried ONCE on failure.
//! - **Web Push.** For each stored subscription, a real RFC 8291 `aes128gcm`-encrypted payload
//!   (when the browser keys are present) is POSTed to the subscription endpoint over TLS, carrying
//!   a VAPID (RFC 8292) `Authorization` header signed with the configured keypair. When the VAPID
//!   keypair is absent the channel is skipped (subscriptions remain stored). See [`crate::webpush`].
//! - **Email.** Optional, env-gated (`KLAXON_SMTP_ENABLED`). When the recipient is addressable by
//!   email, a best-effort SMTP submission to `corvid:587` over raw TCP.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

use crate::audit::AuditEvent;
use crate::store::{Notification, PushSubscription};
use crate::webpush;
use crate::AppState;

/// Per-attempt network budget (connect + TLS + write + read).
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a push service should hold an undelivered message, seconds (4 weeks — the common max).
const PUSH_TTL_SECS: u32 = 2_419_200;

/// Header carrying the per-webhook HMAC signature of the body.
const SIG_HEADER: &str = "X-Klaxon-Signature";

/// Spawn the detached fan-out task for a freshly stored notification. Returns immediately.
pub fn fan_out(state: AppState, notification: Notification) {
    tokio::spawn(async move {
        run(state, notification).await;
    });
}

async fn run(state: AppState, n: Notification) {
    let key = n.user_sub.clone();
    let subs = state.store.list_subscriptions(&key).await;
    let hooks = state.store.list_webhooks(&key).await;
    let payload = serde_json::to_string(&n).unwrap_or_else(|_| "{}".to_string());

    // --- Webhooks (best-effort, signed, retry-once) ---
    let mut webhook_ok = 0usize;
    for hook in &hooks {
        match deliver_webhook(&hook.url, &n.source, &hook.secret, &payload).await {
            Ok(status) => {
                webhook_ok += 1;
                tracing::debug!(url = %hook.url, status, "webhook delivered");
            }
            Err(e) => tracing::warn!(url = %hook.url, error = %e, "webhook delivery failed"),
        }
    }

    // --- Web Push (real VAPID + RFC 8291 encrypted send when the keypair is configured) ---
    let push_configured = state.config.push_configured();
    let mut push_ok = 0usize;
    if push_configured && !subs.is_empty() {
        for sub in &subs {
            match deliver_push(&state, sub, payload.as_bytes()).await {
                Ok(status) => {
                    push_ok += 1;
                    tracing::debug!(endpoint = %sub.endpoint, status, "web push delivered");
                }
                Err(e) => tracing::warn!(endpoint = %sub.endpoint, error = %e, "web push failed"),
            }
        }
    } else if !subs.is_empty() {
        tracing::info!(
            user = %key,
            subscriptions = subs.len(),
            "web push subscriptions present but VAPID keypair unconfigured — skipped"
        );
    }

    // --- Email (optional, env-gated, best-effort) ---
    let mut email_ok = false;
    if state.config.smtp_enabled {
        if let Some(rcpt) = recipient_email(&n.user_sub) {
            match deliver_email(&state, &rcpt, &n).await {
                Ok(()) => email_ok = true,
                Err(e) => tracing::warn!(rcpt = %rcpt, error = %e, "email delivery failed"),
            }
        }
    }

    // --- Audit the fan-out (non-blocking, value-free detail) ---
    state.audit.emit(AuditEvent::info(
        "notify.fanout",
        &n.user_sub,
        &n.source,
        &format!(
            "webhooks={}/{} push={}/{} push_configured={} email={}",
            webhook_ok,
            hooks.len(),
            push_ok,
            subs.len(),
            push_configured,
            email_ok
        ),
    ));
}

/// Treat a `user_sub` that looks like an address (`local@domain`) as an email recipient.
fn recipient_email(user_sub: &str) -> Option<String> {
    let s = user_sub.trim();
    if s.contains('@') && !s.contains(char::is_whitespace) {
        Some(s.to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Webhook delivery
// ---------------------------------------------------------------------------

/// The webhook signature header value for `body` under `secret`: `sha256=<hex HMAC-SHA256>`. An
/// empty secret means the webhook is unsigned (returns `None`).
pub fn webhook_signature(secret: &str, body: &str) -> Option<String> {
    if secret.is_empty() {
        return None;
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key len");
    mac.update(body.as_bytes());
    Some(format!("sha256={}", hex::encode(mac.finalize().into_bytes())))
}

/// Deliver one webhook: a JSON `POST` with the source + (optional) HMAC signature headers. Retried
/// ONCE on failure. Returns the HTTP status on success, or the last I/O error.
async fn deliver_webhook(url: &str, source: &str, secret: &str, body: &str) -> std::io::Result<u16> {
    let Some(target) = Endpoint::parse(url) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "webhook url is not http(s)",
        ));
    };
    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("X-Klaxon-Source".to_string(), sanitize_header(source)),
    ];
    if let Some(sig) = webhook_signature(secret, body) {
        headers.push((SIG_HEADER.to_string(), sig));
    }
    send_with_retry(&target, &headers, body.as_bytes()).await
}

// ---------------------------------------------------------------------------
// Web Push delivery
// ---------------------------------------------------------------------------

/// Deliver one Web Push message: RFC 8291 `aes128gcm`-encrypt `payload` to the subscription (when
/// its browser keys are present) and POST it to the endpoint with a VAPID `Authorization` header.
/// Retried ONCE on failure. Returns the HTTP status on success.
async fn deliver_push(
    state: &AppState,
    sub: &PushSubscription,
    payload: &[u8],
) -> std::io::Result<u16> {
    let Some(target) = Endpoint::parse(&sub.endpoint) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "push endpoint is not http(s)",
        ));
    };
    let (private, public) = match (&state.config.vapid_private_key, &state.config.vapid_public_key) {
        (Some(pv), Some(pb)) => (pv, pb),
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "VAPID keypair not configured",
            ))
        }
    };
    let authorization = webpush::vapid_authorization(
        private,
        public,
        &state.config.vapid_subject,
        &sub.endpoint,
        crate::now_secs(),
    )
    .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid VAPID keypair"))?;

    let mut headers = vec![
        ("Authorization".to_string(), authorization),
        ("TTL".to_string(), PUSH_TTL_SECS.to_string()),
    ];

    // Encrypt when the subscription carries its browser keys; otherwise fall back to a valid
    // zero-length VAPID-only ping (still authenticated; some services accept the wakeup).
    let body: Vec<u8> = if !sub.p256dh.is_empty() && !sub.auth.is_empty() {
        match webpush::encrypt_payload(&sub.p256dh, &sub.auth, payload) {
            Some(cipher) => {
                headers.push(("Content-Encoding".to_string(), "aes128gcm".to_string()));
                headers.push((
                    "Content-Type".to_string(),
                    "application/octet-stream".to_string(),
                ));
                cipher
            }
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "subscription payload encryption failed",
                ))
            }
        }
    } else {
        Vec::new()
    };

    send_with_retry(&target, &headers, &body).await
}

// ---------------------------------------------------------------------------
// HTTP(S) client (raw HTTP/1.1 over TCP or a rustls TLS stream) — retry-once
// ---------------------------------------------------------------------------

/// POST `body` to `target` with `headers`, retrying ONCE on any failure. Returns the HTTP status.
async fn send_with_retry(
    target: &Endpoint,
    headers: &[(String, String)],
    body: &[u8],
) -> std::io::Result<u16> {
    let mut last: std::io::Error =
        std::io::Error::new(std::io::ErrorKind::Other, "no attempt made");
    for attempt in 0..2u8 {
        match tokio::time::timeout(DELIVERY_TIMEOUT, post_once(target, headers, body)).await {
            Ok(Ok(status)) => return Ok(status),
            Ok(Err(e)) => last = e,
            Err(_) => {
                last = std::io::Error::new(std::io::ErrorKind::TimedOut, "delivery timed out")
            }
        }
        if attempt == 0 {
            tracing::debug!(host = %target.host, "delivery attempt failed — retrying once");
        }
    }
    Err(last)
}

/// A single POST attempt over the right transport for `target`'s scheme.
async fn post_once(
    target: &Endpoint,
    headers: &[(String, String)],
    body: &[u8],
) -> std::io::Result<u16> {
    let request = build_request(target, headers, body);
    if target.https {
        let tcp = TcpStream::connect((target.host.as_str(), target.port)).await?;
        let server_name = ServerName::try_from(target.host.clone())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid SNI host"))?;
        let mut tls = tls_connector().connect(server_name, tcp).await?;
        exchange(&mut tls, &request).await
    } else {
        let mut tcp = TcpStream::connect((target.host.as_str(), target.port)).await?;
        exchange(&mut tcp, &request).await
    }
}

/// Serialize the HTTP/1.1 request bytes (headers as UTF-8, then the raw body).
fn build_request(target: &Endpoint, headers: &[(String, String)], body: &[u8]) -> Vec<u8> {
    let mut head = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\n",
        path = target.path,
        authority = target.authority,
    );
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()));
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}

/// Write the request and read the response far enough to parse the status line.
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    request: &[u8],
) -> std::io::Result<u16> {
    stream.write_all(request).await?;
    stream.flush().await?;
    let mut buf = Vec::with_capacity(256);
    stream.read_to_end(&mut buf).await?;
    parse_status(&buf)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no HTTP status line"))
}

/// Parse the numeric status from an HTTP response's first line (`HTTP/1.1 201 Created`).
fn parse_status(buf: &[u8]) -> Option<u16> {
    let line_end = buf.iter().position(|&b| b == b'\n').unwrap_or(buf.len());
    let line = std::str::from_utf8(&buf[..line_end]).ok()?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Strip CR/LF from a value destined for a header (prevents header injection via the source name).
fn sanitize_header(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

/// A process-wide rustls TLS client (ring provider, Mozilla roots) for the push/webhook endpoints.
fn tls_connector() -> &'static TlsConnector {
    static CONNECTOR: OnceLock<TlsConnector> = OnceLock::new();
    CONNECTOR.get_or_init(|| {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = ClientConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring provider supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        TlsConnector::from(Arc::new(config))
    })
}

/// A parsed `http`/`https` endpoint: where to connect, the `Host` authority, and the request path.
struct Endpoint {
    https: bool,
    host: String,
    port: u16,
    authority: String,
    path: String,
}

impl Endpoint {
    /// Parse an absolute `http://`/`https://` URL. Returns `None` for other schemes or an empty
    /// authority. The default port is 80 for `http`, 443 for `https`.
    fn parse(url: &str) -> Option<Endpoint> {
        let (scheme, rest) = url.split_once("://")?;
        let https = if scheme.eq_ignore_ascii_case("https") {
            true
        } else if scheme.eq_ignore_ascii_case("http") {
            false
        } else {
            return None;
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return None;
        }
        let default_port = if https { 443 } else { 80 };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => (authority.to_string(), default_port),
        };
        if host.is_empty() {
            return None;
        }
        Some(Endpoint {
            https,
            host,
            port,
            authority: authority.to_string(),
            path: if path.is_empty() { "/".to_string() } else { path.to_string() },
        })
    }
}

// ---------------------------------------------------------------------------
// Email (optional, best-effort SMTP over raw TCP)
// ---------------------------------------------------------------------------

/// Best-effort SMTP submission of the notification to the estate mail server (`corvid:587`) over a
/// raw TCP stream. Plain SMTP (no STARTTLS / no auth): the hop is in-network. Reads each reply just
/// far enough to keep the dialogue moving; non-2xx replies abort with an error.
async fn deliver_email(state: &AppState, rcpt: &str, n: &Notification) -> std::io::Result<()> {
    let cfg = &state.config;
    let (host, port) = split_host_port(&cfg.smtp_addr, 587);
    let result = tokio::time::timeout(DELIVERY_TIMEOUT, async {
        let mut stream = TcpStream::connect((host.as_str(), port)).await?;
        read_reply(&mut stream).await?; // greeting
        write_line(&mut stream, "EHLO klaxon").await?;
        read_reply(&mut stream).await?;
        write_line(&mut stream, &format!("MAIL FROM:<{}>", cfg.smtp_from)).await?;
        read_reply(&mut stream).await?;
        write_line(&mut stream, &format!("RCPT TO:<{rcpt}>")).await?;
        read_reply(&mut stream).await?;
        write_line(&mut stream, "DATA").await?;
        read_reply(&mut stream).await?;
        let subject = n.title.replace(['\r', '\n'], " ");
        let mut data = String::new();
        data.push_str(&format!("From: {}\r\n", cfg.smtp_from));
        data.push_str(&format!("To: {rcpt}\r\n"));
        data.push_str(&format!("Subject: [{}] {}\r\n", n.source, subject));
        data.push_str("Content-Type: text/plain; charset=utf-8\r\n\r\n");
        data.push_str(&n.body.replace("\r\n.\r\n", "\r\n..\r\n"));
        if !n.url.is_empty() {
            data.push_str(&format!("\r\n\r\n{}", n.url));
        }
        data.push_str("\r\n.\r\n");
        stream.write_all(data.as_bytes()).await?;
        stream.flush().await?;
        read_reply(&mut stream).await?;
        write_line(&mut stream, "QUIT").await?;
        Ok::<(), std::io::Error>(())
    })
    .await;
    match result {
        Ok(inner) => inner,
        Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "smtp timed out")),
    }
}

async fn write_line(stream: &mut TcpStream, line: &str) -> std::io::Result<()> {
    stream.write_all(line.as_bytes()).await?;
    stream.write_all(b"\r\n").await?;
    stream.flush().await
}

/// Read one SMTP reply chunk and require a `2xx`/`3xx` status code on the first line.
async fn read_reply(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut buf = [0u8; 512];
    let read = stream.read(&mut buf).await?;
    let line = String::from_utf8_lossy(&buf[..read]);
    let code = line.get(0..1).unwrap_or("5");
    if code == "2" || code == "3" {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "smtp rejected: {}",
            line.lines().next().unwrap_or("").trim()
        )))
    }
}

/// Split `host[:port]` into parts, using `default_port` when none is present.
fn split_host_port(addr: &str, default_port: u16) -> (String, u16) {
    match addr.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(default_port)),
        None => (addr.to_string(), default_port),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parses_http_and_https() {
        let t = Endpoint::parse("http://hooks:9000/in/abc").unwrap();
        assert!(!t.https);
        assert_eq!(t.host, "hooks");
        assert_eq!(t.port, 9000);
        assert_eq!(t.path, "/in/abc");

        let s = Endpoint::parse("https://fcm.googleapis.com/fcm/send/x").unwrap();
        assert!(s.https);
        assert_eq!(s.host, "fcm.googleapis.com");
        assert_eq!(s.port, 443, "https defaults to 443");
        assert_eq!(s.path, "/fcm/send/x");

        let bare = Endpoint::parse("http://hooks").unwrap();
        assert_eq!(bare.port, 80);
        assert_eq!(bare.path, "/");

        assert!(Endpoint::parse("ftp://hooks/x").is_none());
        assert!(Endpoint::parse("http://").is_none());
    }

    #[test]
    fn recipient_email_detection() {
        assert_eq!(recipient_email("a@w33d.xyz").as_deref(), Some("a@w33d.xyz"));
        assert!(recipient_email("u_1234").is_none());
        assert!(recipient_email("bad addr@x").is_none());
    }

    #[test]
    fn webhook_signature_matches_rfc4231_vector() {
        // RFC 4231 Test Case 2: key "Jefe", data "what do ya want for nothing?".
        let sig = webhook_signature("Jefe", "what do ya want for nothing?").unwrap();
        assert_eq!(
            sig,
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // An empty secret means the webhook is unsigned.
        assert!(webhook_signature("", "body").is_none());
    }

    #[test]
    fn webhook_signature_is_body_bound() {
        let a = webhook_signature("k", r#"{"id":"a"}"#).unwrap();
        let b = webhook_signature("k", r#"{"id":"b"}"#).unwrap();
        assert_ne!(a, b, "signature must cover the exact body");
        assert!(a.starts_with("sha256="));
    }

    #[test]
    fn build_request_includes_headers_and_body() {
        let target = Endpoint::parse("https://push.example/ep").unwrap();
        let headers = vec![
            ("Authorization".to_string(), "vapid t=x, k=y".to_string()),
            ("TTL".to_string(), "60".to_string()),
        ];
        let req = build_request(&target, &headers, b"\x01\x02\x03");
        let text = String::from_utf8_lossy(&req);
        assert!(text.starts_with("POST /ep HTTP/1.1\r\n"));
        assert!(text.contains("Host: push.example\r\n"));
        assert!(text.contains("Authorization: vapid t=x, k=y\r\n"));
        assert!(text.contains("TTL: 60\r\n"));
        assert!(text.contains("Content-Length: 3\r\n"));
        assert!(req.ends_with(&[1u8, 2, 3]));
    }

    #[test]
    fn parse_status_reads_code() {
        assert_eq!(parse_status(b"HTTP/1.1 201 Created\r\n\r\n"), Some(201));
        assert_eq!(parse_status(b"HTTP/1.1 404 Not Found\r\n"), Some(404));
        assert_eq!(parse_status(b"garbage"), None);
    }
}
