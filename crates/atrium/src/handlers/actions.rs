//! In-place row actions: `POST /api/inbox/read` and `POST /api/inbox/dismiss`.
//!
//! The dashboard's action buttons `fetch()` these endpoints to mark a single aggregated row read or
//! to dismiss it, then remove the row from the DOM and re-poll — no full reload. Each is a
//! state-changing write, so (unlike the pure-read page + poll) it is guarded by the double-submit
//! CSRF check ([`crate::csrf`]) AND requires a gateway-injected identity. The action is recorded in
//! the per-viewer overlay ([`crate::store`]); on the next aggregate the row is hidden. A successful
//! mutation emits an audit event (never the row's content — only its source + key ride the event).

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::audit::AuditEvent;
use crate::auth;
use crate::csrf;
use crate::source::SectionKind;
use crate::AppState;

/// The action POST body: which column (`source` slug) and which row (`key`). The CSRF token is NOT
/// in the body — it travels in the `X-CSRF-Token` header, paired with the cookie.
#[derive(Debug, Deserialize)]
pub struct ActionBody {
    pub source: String,
    pub key: String,
}

/// `POST /api/inbox/read` — mark one aggregated row read (hidden from the unread view).
pub async fn mark_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Json<ActionBody>,
) -> Response {
    act(&state, &headers, "read", body.0).await
}

/// `POST /api/inbox/dismiss` — dismiss one aggregated row (hidden from the unread view).
pub async fn dismiss(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Json<ActionBody>,
) -> Response {
    act(&state, &headers, "dismiss", body.0).await
}

/// Shared action path: authenticate, verify CSRF, validate the target, record + audit.
async fn act(state: &AppState, headers: &HeaderMap, action: &str, body: ActionBody) -> Response {
    // A mutation REQUIRES a gateway identity (the overlay is scoped to the viewer subject).
    let Some(sub) = auth::subject(headers) else {
        return (StatusCode::UNAUTHORIZED, "no gateway identity").into_response();
    };
    // Double-submit CSRF: cookie must equal the echoed X-CSRF-Token header.
    if !csrf::valid(headers) {
        return (StatusCode::FORBIDDEN, "csrf check failed").into_response();
    }
    // Only the three known columns are addressable.
    let Some(kind) = SectionKind::from_slug(&body.source) else {
        return (StatusCode::BAD_REQUEST, "unknown source").into_response();
    };
    let key = body.key.trim();
    if key.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing row key").into_response();
    }

    match state.store.record(&sub, kind.slug(), key, action).await {
        Ok(()) => {
            // Audit the deliberate action. Only the source column + the opaque row key ride the
            // event — never the message/notification/feed content itself.
            let email = auth::display_email(headers);
            state.audit.emit(AuditEvent::notice(
                &format!("inbox.{action}"),
                &email,
                kind.slug(),
                key,
            ));
            (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, action, "failed to record inbox action");
            (StatusCode::BAD_GATEWAY, Json(json!({ "ok": false }))).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::inbox::Engine;
    use crate::source::{InMemorySource, InboxRow, Section, SectionKind};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    const CSRF: &str = "test-csrf-token";

    fn row(key: &str, title: &str) -> InboxRow {
        InboxRow {
            key: key.to_string(),
            title: title.to_string(),
            snippet: "preview".to_string(),
            link: "https://chat.w33d.xyz/r/x".to_string(),
            at: Some(crate::now_secs()),
            count: Some(2),
            ..Default::default()
        }
    }

    fn state() -> crate::AppState {
        let chat = Section {
            total: 4,
            rows: vec![row("r1", "#general"), row("r2", "#random")],
        };
        let engine = Engine::new(
            Some(Arc::new(InMemorySource::new(SectionKind::Chat, chat))),
            None,
            None,
        );
        crate::build_state_with_engine(engine)
    }

    fn post(uri: &str, body: &str, csrf_cookie: Option<&str>, csrf_header: Option<&str>) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri(uri)
            .header("x-auth-subject", "u_1")
            .header("x-auth-email", "ops@w33d.xyz")
            .header("content-type", "application/json");
        if let Some(c) = csrf_cookie {
            b = b.header("cookie", format!("atrium_csrf={c}"));
        }
        if let Some(h) = csrf_header {
            b = b.header("x-csrf-token", h);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn dismiss_hides_the_row_from_the_next_view() {
        let state = state();

        // Precondition: both rooms show up in the JSON feed.
        let res = crate::app(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/inbox")
                    .header("x-auth-subject", "u_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let before: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(before["columns"].as_str().unwrap().contains("#general"));
        assert!(before["columns"].as_str().unwrap().contains("#random"));
        assert_eq!(before["total_unread"], 4);

        // Dismiss #general (key r1) with a valid double-submit CSRF pair.
        let res = crate::app(state.clone())
            .oneshot(post(
                "/api/inbox/dismiss",
                r#"{"source":"chat","key":"r1"}"#,
                Some(CSRF),
                Some(CSRF),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // The dismissed row is now hidden; the other remains; the total dropped by its count.
        let res = crate::app(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/inbox")
                    .header("x-auth-subject", "u_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let after: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let cols = after["columns"].as_str().unwrap();
        assert!(!cols.contains("#general"), "dismissed row gone");
        assert!(cols.contains("#random"), "other row stays");
        assert_eq!(after["total_unread"], 2, "total dropped by the dismissed room's unread");
    }

    #[tokio::test]
    async fn missing_csrf_is_rejected() {
        let res = crate::app(state())
            .oneshot(post(
                "/api/inbox/read",
                r#"{"source":"chat","key":"r1"}"#,
                Some(CSRF),
                None, // no echoed header
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn mismatched_csrf_is_rejected() {
        let res = crate::app(state())
            .oneshot(post(
                "/api/inbox/read",
                r#"{"source":"chat","key":"r1"}"#,
                Some(CSRF),
                Some("a-different-token"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn action_without_identity_is_unauthorized() {
        let res = crate::app(state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/inbox/dismiss")
                    .header("content-type", "application/json")
                    .header("cookie", format!("atrium_csrf={CSRF}"))
                    .header("x-csrf-token", CSRF)
                    .body(Body::from(r#"{"source":"chat","key":"r1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unknown_source_is_rejected() {
        let res = crate::app(state())
            .oneshot(post(
                "/api/inbox/read",
                r#"{"source":"nope","key":"r1"}"#,
                Some(CSRF),
                Some(CSRF),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
}
