//! End-to-end flow tests against the in-memory store (NO database, NO network).
//!
//! Drives the real `Router` in-process via `tower::oneshot`, exercising the internal ingest API
//! (bearer auth + addressing by subject/email), the SSO inbox listing, the double-submit CSRF
//! guard on mark-read, and the public health / VAPID endpoints.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use klaxon::audit::AuditSink;
use klaxon::config::Config;
use klaxon::store::InMemoryStore;
use klaxon::{app, AppState};
use tower::ServiceExt;

const INGEST_TOKEN: &str = "ingest-test-token";
const SUBJECT: &str = "u_alice";
const EMAIL: &str = "alice@w33d.xyz";

fn state() -> AppState {
    let mut config = Config::dev();
    config.ingest_token = Some(INGEST_TOKEN.to_string());
    config.vapid_public_key = Some("BPublicKeyDemo".to_string());
    AppState {
        config: Arc::new(config),
        store: Arc::new(InMemoryStore::new()),
        audit: AuditSink::disabled(),
    }
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Resp {
    fn csrf_cookie(&self) -> Option<String> {
        for hv in self.headers.get_all(header::SET_COOKIE).iter() {
            let raw = hv.to_str().ok()?;
            if let Some(rest) = raw.strip_prefix("__Host-csrf=") {
                return Some(rest.split(';').next().unwrap_or("").to_string());
            }
        }
        None
    }
}

async fn send(state: &AppState, req: Request<Body>) -> Resp {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    Resp {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).to_string(),
    }
}

fn ingest(token: Option<&str>, json: &str) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/api/notify")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::from(json.to_string())).unwrap()
}

fn inbox(subject: &str, email: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/")
        .header("x-auth-subject", subject)
        .header("x-auth-email", email)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn healthz_is_public_ok() {
    let st = state();
    let resp = send(&st, Request::builder().uri("/healthz").body(Body::empty()).unwrap()).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.body, "ok");
}

#[tokio::test]
async fn vapid_public_key_is_returned() {
    let st = state();
    let resp = send(&st, Request::builder().uri("/vapidPublicKey").body(Body::empty()).unwrap()).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.body, "BPublicKeyDemo");
}

#[tokio::test]
async fn ingest_requires_valid_bearer() {
    let st = state();
    let body = format!(r#"{{"user_sub":"{SUBJECT}","source":"loom","title":"PR merged"}}"#);
    // No token -> 401.
    let resp = send(&st, ingest(None, &body)).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    // Wrong token -> 401.
    let resp = send(&st, ingest(Some("nope"), &body)).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    // Right token -> 200.
    let resp = send(&st, ingest(Some(INGEST_TOKEN), &body)).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains("\"source\":\"loom\""));
}

#[tokio::test]
async fn ingest_then_inbox_lists_unread() {
    let st = state();
    let body = format!(r#"{{"user_sub":"{SUBJECT}","source":"sanctum","title":"Secret revealed","body":"db/prod","url":"https://vault.w33d.xyz/s/db"}}"#);
    let resp = send(&st, ingest(Some(INGEST_TOKEN), &body)).await;
    assert_eq!(resp.status, StatusCode::OK);

    let resp = send(&st, inbox(SUBJECT, EMAIL)).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains("Secret revealed"));
    assert!(resp.body.contains("badge-source"));
    // Unread counter reflects the one notification.
    assert!(resp.body.contains(r#"<span class="count" data-zero="0">1</span>"#));
}

#[tokio::test]
async fn ingest_by_email_is_visible_to_that_user() {
    let st = state();
    let body = format!(r#"{{"user_email":"{EMAIL}","source":"corvid","title":"New mail"}}"#);
    send(&st, ingest(Some(INGEST_TOKEN), &body)).await;

    let resp = send(&st, inbox(SUBJECT, EMAIL)).await;
    assert!(resp.body.contains("New mail"), "email-addressed notification must reach the user");
}

#[tokio::test]
async fn inbox_requires_sso_identity() {
    let st = state();
    let resp = send(&st, Request::builder().uri("/").body(Body::empty()).unwrap()).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn ingest_rejects_missing_fields() {
    let st = state();
    // Missing recipient.
    let resp = send(&st, ingest(Some(INGEST_TOKEN), r#"{"source":"x","title":"y"}"#)).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    // Missing title.
    let body = format!(r#"{{"user_sub":"{SUBJECT}","source":"x"}}"#);
    let resp = send(&st, ingest(Some(INGEST_TOKEN), &body)).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn mark_read_requires_csrf_then_succeeds() {
    let st = state();
    let body = format!(r#"{{"user_sub":"{SUBJECT}","source":"beacon","title":"Probe down"}}"#);
    send(&st, ingest(Some(INGEST_TOKEN), &body)).await;

    // Load the inbox to mint a CSRF cookie/token.
    let page = send(&st, inbox(SUBJECT, EMAIL)).await;
    let cookie = page.csrf_cookie().expect("csrf cookie set on first render");

    // POST without CSRF -> 401.
    let req = Request::builder()
        .method("POST")
        .uri("/api/read")
        .header("x-auth-subject", SUBJECT)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("csrf_token=wrong"))
        .unwrap();
    let resp = send(&st, req).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);

    // POST with matching CSRF (mark all) -> 303 redirect.
    let req = Request::builder()
        .method("POST")
        .uri("/api/read")
        .header("x-auth-subject", SUBJECT)
        .header(header::COOKIE, format!("__Host-csrf={cookie}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("csrf_token={cookie}")))
        .unwrap();
    let resp = send(&st, req).await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);

    // Now the inbox shows zero unread.
    let page = send(&st, inbox(SUBJECT, EMAIL)).await;
    assert!(page.body.contains(r#"<span class="count" data-zero="0">0</span>"#));
}
