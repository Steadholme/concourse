//! `GET /api/inbox` — the JSON feed the dashboard's live auto-refresh poll consumes.
//!
//! The client polls this endpoint every ~20 s and swaps the returned, freshly server-rendered gauge
//! and river fragments into their legacy-compatible `summary` + `columns` slots WITHOUT a full
//! page reload (preserving the window scroll position). The fragments are produced by the SAME
//! render helpers as the full page ([`crate::handlers::dashboard`]), so the poll cannot drift from
//! the initial render and every field stays HTML-escaped exactly as on first paint.
//!
//! Like the page, this is a pure read scoped to the gateway-injected viewer subject, served off the
//! shared ~10 s inbox cache. It emits NO audit event: unlike the human-initiated page view, a
//! background poll fires every 20 s per open tab, so auditing it would flood Watchtower for zero
//! extra signal (the `inbox.view` / `source_unavailable` events already ride the page load).

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::Serialize;

use crate::auth;
use crate::config::SECTION_LIMIT;
use crate::csrf;
use crate::handlers::dashboard::{empty_inbox, render_columns, render_summary};
use crate::handlers::InboxQuery;
use crate::inbox::{self, Inbox, ViewFilter};
use crate::AppState;

/// The live-refresh payload: the grand unread total plus the two pre-rendered HTML fragments the
/// client drops into `#summary-slot` and `#columns-slot`.
#[derive(Serialize)]
pub struct InboxPayload {
    /// Grand total unread across every available column (drives any title/badge the client keeps).
    pub total_unread: i64,
    /// Rendered summary-bar HTML (server-escaped) for `#summary-slot`.
    pub summary: String,
    /// Rendered chronological river HTML (server-escaped) for `#columns-slot`.
    pub columns: String,
}

/// `GET /api/inbox` — the aggregated inbox as JSON for the client poll. Scoped to the injected
/// viewer subject; an unauthenticated probe gets the same empty shell the page renders.
pub async fn api_inbox(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<InboxQuery>,
) -> Json<InboxPayload> {
    let now = crate::now_secs();
    let filter = ViewFilter::new(query.q, query.source);
    let token = csrf::cookie_token(&headers).unwrap_or_default();

    // No gateway identity -> nothing to federate; return the calm empty shell (matches the page).
    let Some(sub) = auth::subject(&headers) else {
        let empty = empty_inbox();
        let view = inbox::view(&empty, &Default::default(), &filter);
        return Json(payload(&view, now, &token, &filter));
    };

    let (raw, _fresh) = state.cache.get(&state.engine, &sub, SECTION_LIMIT).await;
    // Hide acted-on rows (fail-open on a down overlay DB) and apply the search + source filter, so
    // the live poll re-renders exactly what the filtered page shows.
    let hidden = state.store.hidden(&sub).await.unwrap_or_default();
    let view = inbox::view(&raw, &hidden, &filter);
    Json(payload(&view, now, &token, &filter))
}

/// Build the JSON payload by reusing the page's own render helpers.
fn payload(inbox: &Inbox, now: i64, token: &str, filter: &ViewFilter) -> InboxPayload {
    InboxPayload {
        total_unread: inbox.total_unread(),
        summary: render_summary(inbox),
        columns: render_columns(inbox, now, token, filter),
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

    fn section(total: i64, title: &str) -> Section {
        Section {
            total,
            rows: vec![InboxRow {
                key: "row-1".to_string(),
                title: title.to_string(),
                snippet: "preview".to_string(),
                link: "https://chat.w33d.xyz/r/x".to_string(),
                at: Some(crate::now_secs()),
                count: Some(2),
                ..Default::default()
            }],
        }
    }

    fn state() -> crate::AppState {
        let engine = Engine::new(
            Some(Arc::new(InMemorySource::new(
                SectionKind::Chat,
                section(2, "#general"),
            ))),
            Some(Arc::new(InMemorySource::down(SectionKind::Notifications))),
            Some(Arc::new(InMemorySource::new(
                SectionKind::Feed,
                Section::empty(),
            ))),
        );
        crate::build_state_with_engine(engine)
    }

    #[tokio::test]
    async fn api_inbox_returns_json_fragments_and_total() {
        let app = crate::app(state());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/inbox")
                    .header("x-auth-subject", "u_1")
                    .header("x-auth-email", "ops@w33d.xyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default(),
            "application/json"
        );
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // Chat has 2 unread; down notifications contributes 0; empty feed 0 => grand total 2.
        assert_eq!(json["total_unread"], 2);
        let summary = json["summary"].as_str().unwrap();
        let columns = json["columns"].as_str().unwrap();
        assert!(summary.contains("gauge"), "gauge fragment present");
        assert!(
            columns.contains("#general"),
            "chat row rendered in columns fragment"
        );
        assert!(
            columns.contains("Notifications unavailable"),
            "down source degrades explicitly"
        );
        assert!(
            columns.contains(r#"class="river""#),
            "single river fragment returned"
        );
    }

    #[tokio::test]
    async fn api_inbox_without_identity_is_empty_shell() {
        let app = crate::app(crate::build_dev_state());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/inbox")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total_unread"], 0);
        assert!(json["columns"]
            .as_str()
            .unwrap()
            .contains("You're all caught up."));
    }

    #[tokio::test]
    async fn api_inbox_retains_exact_payload_keys() {
        let res = crate::app(state())
            .oneshot(
                Request::builder()
                    .uri("/api/inbox")
                    .header("x-auth-subject", "u_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let object = json.as_object().expect("payload object");
        assert_eq!(object.len(), 3);
        for key in ["summary", "columns", "total_unread"] {
            assert!(object.contains_key(key), "missing frozen payload key {key}");
        }
    }

    #[tokio::test]
    async fn api_poll_river_carries_native_forms_and_existing_cookie_token() {
        let res = crate::app(state())
            .oneshot(
                Request::builder()
                    .uri("/api/inbox?q=general&source=chat")
                    .header("x-auth-subject", "u_1")
                    .header("cookie", "atrium_csrf=poll-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let columns = json["columns"].as_str().unwrap();
        assert!(columns.contains(r#"class="river__actform""#));
        assert!(columns.contains(r#"name="csrf" value="poll-token""#));
        assert!(columns.contains(r#"name="return_q" value="general""#));
        assert!(columns.contains(r#"name="return_source" value="chat""#));
    }
}
