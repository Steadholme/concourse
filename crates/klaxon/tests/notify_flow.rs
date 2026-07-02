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

/// GET the webhooks settings page as `subject`.
fn webhooks_page(subject: &str, email: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/settings/webhooks")
        .header("x-auth-subject", subject)
        .header("x-auth-email", email)
        .body(Body::empty())
        .unwrap()
}

/// GET the delivery-preferences page as `subject`.
fn prefs_page(subject: &str, email: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/settings/prefs")
        .header("x-auth-subject", subject)
        .header("x-auth-email", email)
        .body(Body::empty())
        .unwrap()
}

/// POST a form to `uri` as `subject` with the double-submit CSRF cookie+field set to `csrf`.
fn form_post(uri: &str, subject: &str, csrf: &str, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("x-auth-subject", subject)
        .header(header::COOKIE, format!("__Host-csrf={csrf}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
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
async fn sw_js_is_served_as_javascript_with_root_scope() {
    let st = state();
    let resp = send(&st, Request::builder().uri("/sw.js").body(Body::empty()).unwrap()).await;
    assert_eq!(resp.status, StatusCode::OK);
    let ct = resp
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(ct.starts_with("application/javascript"), "content type: {ct}");
    assert_eq!(
        resp.headers.get("service-worker-allowed").and_then(|v| v.to_str().ok()),
        Some("/"),
        "root scope must be explicitly allowed"
    );
    assert!(resp.body.contains("showNotification"), "push display handler present");
    assert!(resp.body.contains("notificationclick"), "click handler present");
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

/// POST a JSON body to `uri` as `subject` with the double-submit CSRF cookie set to `csrf`.
fn json_post(uri: &str, subject: &str, csrf: &str, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("x-auth-subject", subject)
        .header(header::COOKIE, format!("__Host-csrf={csrf}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn optimistic_read_and_dismiss_json_endpoints() {
    let st = state();
    // Two notifications for the signed-in user.
    for i in 0..2 {
        let body = format!(r#"{{"user_sub":"{SUBJECT}","source":"loom","title":"Item {i}"}}"#);
        assert_eq!(send(&st, ingest(Some(INGEST_TOKEN), &body)).await.status, StatusCode::OK);
    }
    // Mint a CSRF cookie.
    let page = send(&st, inbox(SUBJECT, EMAIL)).await;
    let cookie = page.csrf_cookie().expect("csrf cookie");
    // Grab the two ids straight from the store (order-independent).
    let ids: Vec<String> = st
        .store
        .list_notifications(&[SUBJECT.to_string()])
        .await
        .iter()
        .map(|n| n.id.clone())
        .collect();
    assert_eq!(ids.len(), 2);

    // read.json with a bad CSRF -> 401.
    let bad = json_post(
        "/api/read.json",
        SUBJECT,
        &cookie,
        format!(r#"{{"id":"{}","csrf_token":"wrong"}}"#, ids[0]),
    );
    assert_eq!(send(&st, bad).await.status, StatusCode::UNAUTHORIZED);

    // read.json marks one read and returns the fresh unread count.
    let ok = json_post(
        "/api/read.json",
        SUBJECT,
        &cookie,
        format!(r#"{{"id":"{}","csrf_token":"{cookie}"}}"#, ids[0]),
    );
    let resp = send(&st, ok).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains(r#""unread":1"#), "one unread remains: {}", resp.body);

    // A FOREIGN dismiss (owner-scoped) removes nothing from the owner's inbox.
    let mallory = send(&st, inbox("u_mallory", "m@w33d.xyz")).await;
    let mcookie = mallory.csrf_cookie().unwrap();
    let forged = json_post(
        "/api/dismiss.json",
        "u_mallory",
        &mcookie,
        format!(r#"{{"id":"{}","csrf_token":"{mcookie}"}}"#, ids[1]),
    );
    assert_eq!(send(&st, forged).await.status, StatusCode::OK);
    assert_eq!(
        st.store.list_notifications(&[SUBJECT.to_string()]).await.len(),
        2,
        "foreign dismiss must not touch the owner's rows"
    );

    // The owner dismisses the remaining unread -> the row is gone, unread hits 0.
    let del = json_post(
        "/api/dismiss.json",
        SUBJECT,
        &cookie,
        format!(r#"{{"id":"{}","csrf_token":"{cookie}"}}"#, ids[1]),
    );
    let resp = send(&st, del).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.body.contains(r#""unread":0"#), "no unread left: {}", resp.body);
    assert_eq!(st.store.list_notifications(&[SUBJECT.to_string()]).await.len(), 1);

    // The progressive-enhancement form route still works (no-JS dismiss) -> 303 redirect.
    let form = form_post(
        "/api/dismiss",
        SUBJECT,
        &cookie,
        format!("csrf_token={cookie}&id={}", ids[0]),
    );
    assert_eq!(send(&st, form).await.status, StatusCode::SEE_OTHER);
    assert!(st.store.list_notifications(&[SUBJECT.to_string()]).await.is_empty());
}

#[tokio::test]
async fn subscribe_unsubscribe_roundtrip_is_owner_scoped() {
    let st = state();
    let page = send(&st, inbox(SUBJECT, EMAIL)).await;
    let cookie = page.csrf_cookie().expect("csrf cookie set on first render");

    // Subscribe this browser (the page JS posts endpoint + browser keys).
    let req = form_post(
        "/api/subscribe",
        SUBJECT,
        &cookie,
        format!("csrf_token={cookie}&endpoint=https://push.example/ep1&p256dh=BKey&auth=AuthSecret"),
    );
    assert_eq!(send(&st, req).await.status, StatusCode::SEE_OTHER);
    assert_eq!(st.store.list_subscriptions(SUBJECT).await.len(), 1);

    // Unsubscribe requires CSRF.
    let req = form_post(
        "/api/unsubscribe",
        SUBJECT,
        &cookie,
        "csrf_token=wrong&endpoint=https://push.example/ep1".to_string(),
    );
    assert_eq!(send(&st, req).await.status, StatusCode::UNAUTHORIZED);

    // A foreign unsubscribe for the same endpoint removes nothing (owner-scoped delete).
    let mallory_page = send(&st, inbox("u_mallory", "m@w33d.xyz")).await;
    let mallory_cookie = mallory_page.csrf_cookie().unwrap();
    let req = form_post(
        "/api/unsubscribe",
        "u_mallory",
        &mallory_cookie,
        format!("csrf_token={mallory_cookie}&endpoint=https://push.example/ep1"),
    );
    assert_eq!(send(&st, req).await.status, StatusCode::SEE_OTHER);
    assert_eq!(
        st.store.list_subscriptions(SUBJECT).await.len(),
        1,
        "owner's subscription survives a foreign unsubscribe"
    );

    // The owner unsubscribes -> the row is gone.
    let req = form_post(
        "/api/unsubscribe",
        SUBJECT,
        &cookie,
        format!("csrf_token={cookie}&endpoint=https://push.example/ep1"),
    );
    assert_eq!(send(&st, req).await.status, StatusCode::SEE_OTHER);
    assert!(st.store.list_subscriptions(SUBJECT).await.is_empty());
}

#[tokio::test]
async fn test_notification_requires_identity_and_csrf() {
    let st = state();

    // No SSO identity -> 401 (identity guard).
    let req = Request::builder()
        .method("POST")
        .uri("/api/test")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("csrf_token=x"))
        .unwrap();
    assert_eq!(send(&st, req).await.status, StatusCode::UNAUTHORIZED);

    // Identity but mismatched CSRF -> 401 (double-submit guard).
    let page = send(&st, inbox(SUBJECT, EMAIL)).await;
    let cookie = page.csrf_cookie().expect("csrf cookie set on first render");
    let req = form_post("/api/test", SUBJECT, &cookie, "csrf_token=wrong".to_string());
    assert_eq!(send(&st, req).await.status, StatusCode::UNAUTHORIZED);

    // Valid -> 303, and the test notification lands in the CALLER's own inbox.
    let req = form_post("/api/test", SUBJECT, &cookie, format!("csrf_token={cookie}"));
    assert_eq!(send(&st, req).await.status, StatusCode::SEE_OTHER);
    let page = send(&st, inbox(SUBJECT, EMAIL)).await;
    assert!(page.body.contains("Test notification"));

    // Nobody else sees it (self-addressed only).
    let other = send(&st, inbox("u_mallory", "m@w33d.xyz")).await;
    assert!(!other.body.contains("Test notification"));
}

#[tokio::test]
async fn prefs_page_requires_sso_identity() {
    let st = state();
    let resp = send(
        &st,
        Request::builder().uri("/settings/prefs").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn prefs_save_requires_csrf() {
    let st = state();
    let page = send(&st, prefs_page(SUBJECT, EMAIL)).await;
    let cookie = page.csrf_cookie().expect("csrf cookie set on first render");

    // Wrong CSRF field -> 401 (cookie present, mismatched field).
    let req = form_post("/settings/prefs", SUBJECT, &cookie, "csrf_token=wrong&mute_all=on".to_string());
    assert_eq!(send(&st, req).await.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn prefs_save_roundtrip_and_prefill() {
    let st = state();
    let page = send(&st, prefs_page(SUBJECT, EMAIL)).await;
    assert_eq!(page.status, StatusCode::OK);
    // Fresh owner: nothing checked, quiet-hours blank.
    assert!(!page.body.contains("checkbox\" name=\"mute_all\" value=\"on\" checked"));
    let cookie = page.csrf_cookie().unwrap();

    // Save: mute-all on, quiet 22:00–07:00, mute the beacon source + warning severity.
    let req = form_post(
        "/settings/prefs",
        SUBJECT,
        &cookie,
        format!("csrf_token={cookie}&mute_all=on&quiet_start=22:00&quiet_end=07:00&muted_sources=Beacon,+loom&muted_severities=warning"),
    );
    assert_eq!(send(&st, req).await.status, StatusCode::SEE_OTHER);

    // Re-render pre-fills the saved values (normalized + escaped).
    let page = send(&st, prefs_page(SUBJECT, EMAIL)).await;
    assert!(page.body.contains(r#"name="mute_all" value="on" checked"#));
    assert!(page.body.contains(r#"name="quiet_start" value="22:00""#));
    assert!(page.body.contains(r#"name="quiet_end" value="07:00""#));
    assert!(page.body.contains(r#"value="beacon, loom""#), "sources normalized to lower-case, comma-joined");
    assert!(page.body.contains(r#"value="warning""#));

    // Another user's prefs are independent (owner scoping).
    let other = send(&st, prefs_page("u_mallory", "m@w33d.xyz")).await;
    assert!(!other.body.contains(r#"name="mute_all" value="on" checked"#));
}

#[tokio::test]
async fn prefs_reject_bad_quiet_hours() {
    let st = state();
    let page = send(&st, prefs_page(SUBJECT, EMAIL)).await;
    let cookie = page.csrf_cookie().unwrap();
    let req = form_post(
        "/settings/prefs",
        SUBJECT,
        &cookie,
        format!("csrf_token={cookie}&quiet_start=25:00"),
    );
    assert_eq!(send(&st, req).await.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn muted_source_still_lands_in_inbox() {
    let st = state();
    // Mute the "beacon" source for this owner.
    let page = send(&st, prefs_page(SUBJECT, EMAIL)).await;
    let cookie = page.csrf_cookie().unwrap();
    let req = form_post(
        "/settings/prefs",
        SUBJECT,
        &cookie,
        format!("csrf_token={cookie}&muted_sources=beacon"),
    );
    assert_eq!(send(&st, req).await.status, StatusCode::SEE_OTHER);

    // Ingest a beacon notification: real-time delivery is suppressed, but it MUST still be stored.
    let body = format!(r#"{{"user_sub":"{SUBJECT}","source":"beacon","severity":"warning","title":"Probe down"}}"#);
    assert_eq!(send(&st, ingest(Some(INGEST_TOKEN), &body)).await.status, StatusCode::OK);

    let inbox = send(&st, inbox(SUBJECT, EMAIL)).await;
    assert!(inbox.body.contains("Probe down"), "muted notifications still appear in the inbox");
}

#[tokio::test]
async fn webhooks_page_requires_sso_identity() {
    let st = state();
    let resp = send(
        &st,
        Request::builder().uri("/settings/webhooks").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn webhook_create_requires_csrf() {
    let st = state();
    // Mint a CSRF cookie by loading the page first.
    let page = send(&st, webhooks_page(SUBJECT, EMAIL)).await;
    let cookie = page.csrf_cookie().expect("csrf cookie set on first render");

    // Wrong CSRF field -> 401 (cookie present, mismatched field).
    let req = form_post(
        "/settings/webhooks",
        SUBJECT,
        &cookie,
        "csrf_token=wrong&url=https://example.com/hook".to_string(),
    );
    let resp = send(&st, req).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn webhook_create_rejects_non_http_url() {
    let st = state();
    let page = send(&st, webhooks_page(SUBJECT, EMAIL)).await;
    let cookie = page.csrf_cookie().unwrap();

    let req = form_post(
        "/settings/webhooks",
        SUBJECT,
        &cookie,
        format!("csrf_token={cookie}&url=ftp://example.com/x"),
    );
    let resp = send(&st, req).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn webhook_create_list_and_delete_roundtrip() {
    let st = state();
    let page = send(&st, webhooks_page(SUBJECT, EMAIL)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("No webhooks"), "empty state before any create");
    let cookie = page.csrf_cookie().unwrap();

    // Create -> 303 redirect back to the settings page.
    let req = form_post(
        "/settings/webhooks",
        SUBJECT,
        &cookie,
        format!("csrf_token={cookie}&url=https://example.com/hook&secret=s3cr3t"),
    );
    let resp = send(&st, req).await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);

    // The page now lists it; the secret VALUE is never rendered, only its presence.
    let page = send(&st, webhooks_page(SUBJECT, EMAIL)).await;
    assert!(page.body.contains("https://example.com/hook"));
    assert!(page.body.contains("secret set"));
    assert!(!page.body.contains("s3cr3t"), "secret value must never be rendered");
    assert!(page.body.contains(r#"<span class="count" data-zero="0">1</span>"#));

    // Another user cannot see it (owner scoping).
    let other = send(&st, webhooks_page("u_mallory", "m@w33d.xyz")).await;
    assert!(other.body.contains("No webhooks"));

    // Extract the webhook id from the delete form's hidden input.
    let marker = r#"name="id" value=""#;
    let start = page.body.find(marker).expect("id field present") + marker.len();
    let id: String = page.body[start..].chars().take_while(|&c| c != '"').collect();
    assert!(id.starts_with("whk_"), "generated id has the whk prefix: {id}");

    // A forged delete from another user removes nothing (owner-scoped store delete).
    let mallory_page = send(&st, webhooks_page("u_mallory", "m@w33d.xyz")).await;
    let mallory_cookie = mallory_page.csrf_cookie().unwrap();
    let req = form_post(
        "/settings/webhooks/delete",
        "u_mallory",
        &mallory_cookie,
        format!("csrf_token={mallory_cookie}&id={id}"),
    );
    assert_eq!(send(&st, req).await.status, StatusCode::SEE_OTHER);
    let page = send(&st, webhooks_page(SUBJECT, EMAIL)).await;
    assert!(page.body.contains("https://example.com/hook"), "owner's webhook survives a foreign delete");

    // The owner deletes it -> 303, then the list is empty again.
    let req = form_post(
        "/settings/webhooks/delete",
        SUBJECT,
        &cookie,
        format!("csrf_token={cookie}&id={id}"),
    );
    assert_eq!(send(&st, req).await.status, StatusCode::SEE_OTHER);
    let page = send(&st, webhooks_page(SUBJECT, EMAIL)).await;
    assert!(page.body.contains("No webhooks"));
}
