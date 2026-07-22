//! In-place row actions: `POST /api/inbox/read` and `POST /api/inbox/dismiss`.
//!
//! The dashboard's progressive controls use these endpoints to mark one aggregated row read or to
//! dismiss it. JSON clients keep the in-place `fetch()` path; native forms provide the same action
//! with JavaScript disabled and redirect only after success. Each state-changing write is guarded
//! by the double-submit CSRF check ([`crate::csrf`]) AND requires a gateway-injected identity. The
//! action is recorded in the per-viewer overlay ([`crate::store`]); on the next aggregate the row
//! is hidden. A successful mutation emits an audit event (never the row's content — only its source
//! + key ride the event).

use axum::extract::{FromRequest, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Form, Json};
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

/// Native no-JS form body. The identity never comes from this body; only the gateway headers can
/// establish the viewer. Return fields are presentation hints used after a successful mutation.
#[derive(Debug, Deserialize)]
pub(crate) struct FormAction {
    #[serde(default)]
    source: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    return_q: String,
    #[serde(default)]
    return_source: String,
}

pub(crate) enum ActionPayload {
    Json(ActionBody),
    Form(FormAction),
}

impl<S> FromRequest<S> for ActionPayload
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let content_type = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .unwrap_or_default();

        if content_type.eq_ignore_ascii_case("application/x-www-form-urlencoded") {
            return Form::<FormAction>::from_request(req, state)
                .await
                .map(|Form(body)| ActionPayload::Form(body))
                .map_err(IntoResponse::into_response);
        }
        // Preserve the original `Json<ActionBody>` contract for every non-form request, including
        // structured `application/*+json` types and axum's exact rejection status/body.
        Json::<ActionBody>::from_request(req, state)
            .await
            .map(|Json(body)| ActionPayload::Json(body))
            .map_err(IntoResponse::into_response)
    }
}

/// `POST /api/inbox/read` — mark one aggregated row read (hidden from the unread view).
pub(crate) async fn mark_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: ActionPayload,
) -> Response {
    dispatch(&state, &headers, "read", payload).await
}

/// `POST /api/inbox/dismiss` — dismiss one aggregated row (hidden from the unread view).
pub(crate) async fn dismiss(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: ActionPayload,
) -> Response {
    dispatch(&state, &headers, "dismiss", payload).await
}

enum CsrfProof<'a> {
    Header,
    Form(&'a str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActOutcome {
    Recorded,
    NoIdentity,
    BadCsrf,
    UnknownSource,
    MissingKey,
    StoreError,
}

impl ActOutcome {
    fn status(self) -> StatusCode {
        match self {
            ActOutcome::Recorded => StatusCode::OK,
            ActOutcome::NoIdentity => StatusCode::UNAUTHORIZED,
            ActOutcome::BadCsrf => StatusCode::FORBIDDEN,
            ActOutcome::UnknownSource | ActOutcome::MissingKey => StatusCode::BAD_REQUEST,
            ActOutcome::StoreError => StatusCode::BAD_GATEWAY,
        }
    }

    fn message(self) -> &'static str {
        match self {
            ActOutcome::Recorded => "ok",
            ActOutcome::NoIdentity => "no gateway identity",
            ActOutcome::BadCsrf => "csrf check failed",
            ActOutcome::UnknownSource => "unknown source",
            ActOutcome::MissingKey => "missing row key",
            ActOutcome::StoreError => "failed to record inbox action",
        }
    }
}

async fn dispatch(
    state: &AppState,
    headers: &HeaderMap,
    action: &str,
    payload: ActionPayload,
) -> Response {
    match payload {
        ActionPayload::Json(body) => {
            let outcome = act_core(
                state,
                headers,
                action,
                &body.source,
                &body.key,
                CsrfProof::Header,
            )
            .await;
            match outcome {
                ActOutcome::Recorded => {
                    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
                }
                ActOutcome::StoreError => {
                    (StatusCode::BAD_GATEWAY, Json(json!({ "ok": false }))).into_response()
                }
                other => (other.status(), other.message()).into_response(),
            }
        }
        ActionPayload::Form(body) => {
            let outcome = act_core(
                state,
                headers,
                action,
                &body.source,
                &body.key,
                CsrfProof::Form(&body.csrf),
            )
            .await;
            if outcome == ActOutcome::Recorded {
                let location = redirect_location(&body.return_q, &body.return_source);
                return (StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response();
            }
            (outcome.status(), outcome.message()).into_response()
        }
    }
}

/// Single authority path shared by JSON/header and form/body transport.
async fn act_core(
    state: &AppState,
    headers: &HeaderMap,
    action: &str,
    source: &str,
    key: &str,
    proof: CsrfProof<'_>,
) -> ActOutcome {
    // A mutation REQUIRES a gateway identity (the overlay is scoped to the viewer subject).
    let Some(sub) = auth::subject(headers) else {
        return ActOutcome::NoIdentity;
    };
    // Double-submit CSRF: cookie must equal the token carried by the selected transport.
    let csrf_ok = match proof {
        CsrfProof::Header => csrf::valid(headers),
        CsrfProof::Form(token) => csrf::form_valid(headers, token),
    };
    if !csrf_ok {
        return ActOutcome::BadCsrf;
    }
    // Only the three known columns are addressable.
    let Some(kind) = SectionKind::from_slug(source) else {
        return ActOutcome::UnknownSource;
    };
    let key = key.trim();
    if key.is_empty() {
        return ActOutcome::MissingKey;
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
            ActOutcome::Recorded
        }
        Err(e) => {
            tracing::warn!(error = %e, action, "failed to record inbox action");
            ActOutcome::StoreError
        }
    }
}

/// Encode one RFC 3986 query value using only the unreserved byte set. All percent escapes are
/// uppercase and spaces are `%20`, never `+`, so the redirect has one unambiguous round trip.
fn encode_query_value(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push('%');
                encoded.push(HEX[(byte >> 4) as usize] as char);
                encoded.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    encoded
}

fn redirect_location(return_q: &str, return_source: &str) -> String {
    let mut params = Vec::with_capacity(2);
    if !return_q.is_empty() {
        params.push(format!("q={}", encode_query_value(return_q)));
    }
    if let Some(kind) = SectionKind::from_slug(return_source) {
        params.push(format!("source={}", kind.slug()));
    }
    if params.is_empty() {
        "/".to_string()
    } else {
        format!("/?{}", params.join("&"))
    }
}

#[cfg(test)]
mod tests {
    use crate::inbox::Engine;
    use crate::source::{InMemorySource, InboxRow, Section, SectionKind};
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
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

    fn json_post(
        uri: &str,
        body: &str,
        csrf_cookie: Option<&str>,
        csrf_header: Option<&str>,
        with_identity: bool,
        content_type: &str,
    ) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", content_type);
        if with_identity {
            b = b
                .header("x-auth-subject", "u_1")
                .header("x-auth-email", "ops@w33d.xyz");
        }
        if let Some(c) = csrf_cookie {
            b = b.header("cookie", format!("atrium_csrf={c}"));
        }
        if let Some(h) = csrf_header {
            b = b.header("x-csrf-token", h);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    fn post(
        uri: &str,
        body: &str,
        csrf_cookie: Option<&str>,
        csrf_header: Option<&str>,
    ) -> Request<Body> {
        json_post(
            uri,
            body,
            csrf_cookie,
            csrf_header,
            true,
            "application/json",
        )
    }

    fn form_body(
        source: &str,
        key: &str,
        csrf: &str,
        return_q: &str,
        return_source: &str,
    ) -> String {
        [
            ("source", source),
            ("key", key),
            ("csrf", csrf),
            ("return_q", return_q),
            ("return_source", return_source),
        ]
        .into_iter()
        .map(|(name, value)| format!("{name}={}", super::encode_query_value(value)))
        .collect::<Vec<_>>()
        .join("&")
    }

    fn form_post(
        uri: &str,
        body: String,
        csrf_cookie: Option<&str>,
        with_identity: bool,
    ) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded");
        if with_identity {
            builder = builder
                .header("x-auth-subject", "u_1")
                .header("x-auth-email", "ops@w33d.xyz");
        }
        if let Some(cookie) = csrf_cookie {
            builder = builder.header("cookie", format!("atrium_csrf={cookie}"));
        }
        builder.body(Body::from(body)).unwrap()
    }

    async fn columns(state: crate::AppState) -> String {
        let response = crate::app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/inbox")
                    .header("x-auth-subject", "u_1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        payload["columns"].as_str().unwrap().to_string()
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
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
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
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let after: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let cols = after["columns"].as_str().unwrap();
        assert!(!cols.contains("#general"), "dismissed row gone");
        assert!(cols.contains("#random"), "other row stays");
        assert_eq!(
            after["total_unread"], 2,
            "total dropped by the dismissed room's unread"
        );
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

    #[tokio::test]
    async fn form_post_success_returns_303_and_records() {
        let state = state();
        let response = crate::app(state.clone())
            .oneshot(form_post(
                "/api/inbox/read",
                form_body("chat", "r1", CSRF, "general", "chat"),
                Some(CSRF),
                true,
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/?q=general&source=chat"
        );
        let river = columns(state).await;
        assert!(!river.contains("#general"), "acted row is hidden");
        assert!(river.contains("#random"), "unrelated row remains");
    }

    #[tokio::test]
    async fn json_and_form_share_the_same_authority_failures() {
        struct Case<'a> {
            source: &'a str,
            key: &'a str,
            cookie: Option<&'a str>,
            proof: &'a str,
            identity: bool,
            expected: StatusCode,
        }

        let cases = [
            Case {
                source: "chat",
                key: "r1",
                cookie: Some(CSRF),
                proof: CSRF,
                identity: false,
                expected: StatusCode::UNAUTHORIZED,
            },
            Case {
                source: "chat",
                key: "r1",
                cookie: Some(CSRF),
                proof: "wrong-token",
                identity: true,
                expected: StatusCode::FORBIDDEN,
            },
            Case {
                source: "nope",
                key: "r1",
                cookie: Some(CSRF),
                proof: CSRF,
                identity: true,
                expected: StatusCode::BAD_REQUEST,
            },
            Case {
                source: "chat",
                key: "",
                cookie: Some(CSRF),
                proof: CSRF,
                identity: true,
                expected: StatusCode::BAD_REQUEST,
            },
        ];

        for endpoint in ["/api/inbox/read", "/api/inbox/dismiss"] {
            for case in &cases {
                let json = format!(r#"{{"source":"{}","key":"{}"}}"#, case.source, case.key);
                let json_response = crate::app(state())
                    .oneshot(json_post(
                        endpoint,
                        &json,
                        case.cookie,
                        Some(case.proof),
                        case.identity,
                        "application/json",
                    ))
                    .await
                    .unwrap();
                let form_response = crate::app(state())
                    .oneshot(form_post(
                        endpoint,
                        form_body(case.source, case.key, case.proof, "", ""),
                        case.cookie,
                        case.identity,
                    ))
                    .await
                    .unwrap();
                assert_eq!(json_response.status(), case.expected, "JSON {endpoint}");
                assert_eq!(form_response.status(), case.expected, "form {endpoint}");
            }
        }
    }

    #[tokio::test]
    async fn form_missing_empty_or_bad_csrf_is_forbidden() {
        let bodies = [
            "source=chat&key=r1".to_string(),
            form_body("chat", "r1", "", "", ""),
            form_body("chat", "r1", "wrong-token", "", ""),
        ];
        for body in bodies {
            let response = crate::app(state())
                .oneshot(form_post("/api/inbox/read", body, Some(CSRF), true))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert!(response.headers().get(header::LOCATION).is_none());
        }
    }

    #[tokio::test]
    async fn redirect_location_is_rfc3986_encoded_through_the_real_endpoint() {
        let cases = [
            ("hello world", "", "/?q=hello%20world"),
            ("a+b", "", "/?q=a%2Bb"),
            ("a&b=c", "", "/?q=a%26b%3Dc"),
            ("50%", "", "/?q=50%25"),
            ("#tag", "", "/?q=%23tag"),
            ("café", "", "/?q=caf%C3%A9"),
            ("><script>", "", "/?q=%3E%3Cscript%3E"),
            ("a\r\nb", "", "/?q=a%0D%0Ab"),
            ("", "", "/"),
            ("x", "chat", "/?q=x&source=chat"),
            ("x", "bogus", "/?q=x"),
        ];

        for (query, return_source, expected) in cases {
            let response = crate::app(state())
                .oneshot(form_post(
                    "/api/inbox/read",
                    form_body("chat", "r1", CSRF, query, return_source),
                    Some(CSRF),
                    true,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SEE_OTHER, "query={query:?}");
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .unwrap();
            assert_eq!(location, expected, "query={query:?}");
            assert!(!location.contains('\r'));
            assert!(!location.contains('\n'));

            let follow = crate::app(state())
                .oneshot(
                    Request::builder()
                        .uri(location)
                        .header("x-auth-subject", "u_1")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(follow.status(), StatusCode::OK, "GET {location}");
        }
    }

    #[tokio::test]
    async fn form_failures_never_redirect() {
        for endpoint in ["/api/inbox/read", "/api/inbox/dismiss"] {
            let cases = [
                ("chat", "r1", CSRF, false),
                ("chat", "r1", "wrong-token", true),
                ("nope", "r1", CSRF, true),
                ("chat", "", CSRF, true),
            ];
            for (source, key, proof, identity) in cases {
                let response = crate::app(state())
                    .oneshot(form_post(
                        endpoint,
                        form_body(source, key, proof, "ignored", "chat"),
                        Some(CSRF),
                        identity,
                    ))
                    .await
                    .unwrap();
                assert_ne!(response.status(), StatusCode::SEE_OTHER);
                assert!(response.headers().get(header::LOCATION).is_none());
            }
        }
    }

    #[tokio::test]
    async fn form_dismiss_records_the_same_neutral_overlay_action() {
        let state = state();
        let response = crate::app(state.clone())
            .oneshot(form_post(
                "/api/inbox/dismiss",
                form_body("chat", "r2", CSRF, "", ""),
                Some(CSRF),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers().get(header::LOCATION).unwrap(), "/");

        let river = columns(state).await;
        assert!(river.contains("#general"));
        assert!(!river.contains("#random"));
    }

    #[tokio::test]
    async fn structured_json_media_type_keeps_the_original_json_path() {
        let response = crate::app(state())
            .oneshot(json_post(
                "/api/inbox/read",
                r#"{"source":"chat","key":"r1"}"#,
                Some(CSRF),
                Some(CSRF),
                true,
                "application/vnd.w33d+json",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
    }
}
