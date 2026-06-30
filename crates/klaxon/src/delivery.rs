//! Best-effort, non-blocking fan-out of a stored notification.
//!
//! After a notification row is committed, [`fan_out`] spawns a detached task that delivers the
//! event to the recipient's registered channels and then emits a `notify.fanout` audit event. The
//! task is fire-and-forget: a slow or dead downstream (a webhook host, the mail server) NEVER
//! blocks, slows, or fails the ingest request — exactly like the audit emitter.
//!
//! Channels:
//! - **Webhooks.** A JSON `POST` of the notification to each registered `webhooks.url`, with an
//!   `X-Klaxon-Source` header. Plain `http://` targets (in-network services) are delivered over a
//!   raw TCP HTTP/1.1 write — no TLS client, so the image links no OpenSSL. `https://` targets are
//!   DEGRADED: the intent is logged, not delivered (a pure-Rust TLS push client is a deferred
//!   refinement, not a hypothetical to solve now).
//! - **Web Push.** Subscriptions are stored and the delivery INTENT is recorded (logged). Actual
//!   RFC8291 encrypted delivery requires the VAPID keypair (`configured`); when keys are absent the
//!   channel is reported as not configured. The full encrypted send is a deferred refinement; the
//!   inbox + SSE stream work regardless.
//! - **Email.** Optional, env-gated (`KLAXON_SMTP_ENABLED`). When the recipient is addressable by
//!   email, a best-effort SMTP submission to `corvid:587` over raw TCP.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::audit::AuditEvent;
use crate::store::Notification;
use crate::AppState;

/// Per-delivery network budget (connect + write + read).
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);

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

    // --- Webhooks (best-effort) ---
    let mut webhook_ok = 0usize;
    for hook in &hooks {
        match deliver_webhook(&hook.url, &n.source, &payload).await {
            Ok(true) => webhook_ok += 1,
            Ok(false) => tracing::debug!(url = %hook.url, "webhook intent recorded (non-http target)"),
            Err(e) => tracing::warn!(url = %hook.url, error = %e, "webhook delivery failed"),
        }
    }

    // --- Web Push (record intent; encrypted send is a deferred refinement) ---
    let push_configured = state.config.push_configured();
    if !subs.is_empty() {
        tracing::info!(
            user = %key,
            subscriptions = subs.len(),
            configured = push_configured,
            "web push fan-out intent recorded"
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
            "webhooks={}/{} push_subs={} push_configured={} email={}",
            webhook_ok,
            hooks.len(),
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

/// Deliver one webhook. Returns `Ok(true)` on a delivered `http://` POST, `Ok(false)` when the
/// target is not a plain-http URL (intent only), or an I/O error on a failed delivery.
async fn deliver_webhook(url: &str, source: &str, body: &str) -> std::io::Result<bool> {
    let Some(target) = HttpTarget::parse(url) else {
        return Ok(false);
    };
    let result = tokio::time::timeout(DELIVERY_TIMEOUT, async {
        let mut stream = TcpStream::connect((target.host.as_str(), target.port)).await?;
        let req = format!(
            "POST {path} HTTP/1.1\r\n\
             Host: {authority}\r\n\
             X-Klaxon-Source: {source}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {len}\r\n\
             Connection: close\r\n\r\n{body}",
            path = target.path,
            authority = target.authority,
            len = body.len(),
        );
        stream.write_all(req.as_bytes()).await?;
        stream.flush().await?;
        let mut buf = Vec::with_capacity(256);
        stream.read_to_end(&mut buf).await?;
        Ok::<(), std::io::Error>(())
    })
    .await;
    match result {
        Ok(Ok(())) => Ok(true),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "webhook timed out")),
    }
}

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

/// A parsed plain-`http` webhook target. `https`/other schemes return `None` (intent only).
struct HttpTarget {
    host: String,
    port: u16,
    authority: String,
    path: String,
}

impl HttpTarget {
    fn parse(url: &str) -> Option<HttpTarget> {
        let rest = url.strip_prefix("http://")?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return None;
        }
        let (host, port) = split_host_port(authority, 80);
        if host.is_empty() {
            return None;
        }
        Some(HttpTarget {
            host,
            port,
            authority: authority.to_string(),
            path: if path.is_empty() { "/".to_string() } else { path.to_string() },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_target_parses_and_rejects_https() {
        let t = HttpTarget::parse("http://hooks:9000/in/abc").unwrap();
        assert_eq!(t.host, "hooks");
        assert_eq!(t.port, 9000);
        assert_eq!(t.path, "/in/abc");
        let t2 = HttpTarget::parse("http://hooks").unwrap();
        assert_eq!(t2.port, 80);
        assert_eq!(t2.path, "/");
        assert!(HttpTarget::parse("https://hooks/x").is_none());
    }

    #[test]
    fn recipient_email_detection() {
        assert_eq!(recipient_email("a@w33d.xyz").as_deref(), Some("a@w33d.xyz"));
        assert!(recipient_email("u_1234").is_none());
        assert!(recipient_email("bad addr@x").is_none());
    }
}
