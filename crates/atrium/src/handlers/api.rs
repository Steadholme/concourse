//! `GET /api/inbox` — the JSON feed the dashboard's live auto-refresh poll consumes.
//!
//! The client polls this endpoint every ~20 s and swaps the returned, freshly server-rendered
//! `summary` + `columns` HTML fragments into their slots WITHOUT a full page reload (preserving the
//! window scroll position). The fragments are produced by the SAME render helpers as the full page
//! ([`crate::handlers::dashboard`]), so the poll can never drift from the initial render and every
//! field stays HTML-escaped exactly as on first paint.
//!
//! Like the page, this is a pure read scoped to the gateway-injected viewer subject, served off the
//! shared ~10 s inbox cache. It emits NO audit event: unlike the human-initiated page view, a
//! background poll fires every 20 s per open tab, so auditing it would flood Watchtower for zero
//! extra signal (the `inbox.view` / `source_unavailable` events already ride the page load).

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde::Serialize;

use crate::auth;
use crate::config::SECTION_LIMIT;
use crate::handlers::dashboard::{empty_inbox, render_columns, render_summary};
use crate::inbox::Inbox;
use crate::AppState;

/// The live-refresh payload: the grand unread total plus the two pre-rendered HTML fragments the
/// client drops into `#summary-slot` and `#columns-slot`.
#[derive(Serialize)]
pub struct InboxPayload {
    /// Grand total unread across every available column (drives any title/badge the client keeps).
    pub total_unread: i64,
    /// Rendered summary-bar HTML (server-escaped) for `#summary-slot`.
    pub summary: String,
    /// Rendered three-column HTML (server-escaped) for `#columns-slot`.
    pub columns: String,
}

/// `GET /api/inbox` — the aggregated inbox as JSON for the client poll. Scoped to the injected
/// viewer subject; an unauthenticated probe gets the same empty shell the page renders.
pub async fn api_inbox(State(state): State<AppState>, headers: HeaderMap) -> Json<InboxPayload> {
    let now = crate::now_secs();

    // No gateway identity -> nothing to federate; return the calm empty shell (matches the page).
    let Some(sub) = auth::subject(&headers) else {
        return Json(payload(&empty_inbox(), now));
    };

    let (inbox, _fresh) = state.cache.get(&state.engine, &sub, SECTION_LIMIT).await;
    Json(payload(&inbox, now))
}

/// Build the JSON payload by reusing the page's own render helpers.
fn payload(inbox: &Inbox, now: i64) -> InboxPayload {
    InboxPayload {
        total_unread: inbox.total_unread(),
        summary: render_summary(inbox),
        columns: render_columns(inbox, now),
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
            Some(Arc::new(InMemorySource::new(SectionKind::Chat, section(2, "#general")))),
            Some(Arc::new(InMemorySource::down(SectionKind::Notifications))),
            Some(Arc::new(InMemorySource::new(SectionKind::Feed, Section::empty()))),
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
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // Chat has 2 unread; down notifications contributes 0; empty feed 0 => grand total 2.
        assert_eq!(json["total_unread"], 2);
        let summary = json["summary"].as_str().unwrap();
        let columns = json["columns"].as_str().unwrap();
        assert!(summary.contains("summary"), "summary fragment present");
        assert!(columns.contains("#general"), "chat row rendered in columns fragment");
        assert!(columns.contains("Source unavailable"), "down column degrades");
        assert!(columns.contains("No fresh feed items."), "empty feed caught-up state");
    }

    #[tokio::test]
    async fn api_inbox_without_identity_is_empty_shell() {
        let app = crate::app(crate::build_dev_state());
        let res = app
            .oneshot(Request::builder().uri("/api/inbox").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total_unread"], 0);
        // Every column degrades to its empty "caught up" state.
        assert!(json["columns"]
            .as_str()
            .unwrap()
            .contains("all caught up on chat."));
    }
}
