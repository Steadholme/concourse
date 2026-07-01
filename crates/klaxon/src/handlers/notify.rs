//! `POST /api/notify` — the internal ingest API.
//!
//! Called by other internal services (NOT a browser), so it uses its OWN `Bearer` auth
//! (`KLAXON_INGEST_TOKEN`) rather than the SSO gateway identity. It creates one `notifications`
//! row addressed to a user (by `user_sub` or `user_email`) and kicks off a best-effort,
//! non-blocking fan-out to that user's Web Push subscriptions, webhooks, and (optionally) email.
//! The request returns as soon as the row is committed — fan-out happens on a detached task, so a
//! slow/down downstream never delays the caller.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Json;
use serde::Deserialize;

use crate::config::{MAX_BODY_CHARS, MAX_SEVERITY_CHARS, MAX_TITLE_CHARS};
use crate::delivery;
use crate::error::AppError;
use crate::store::Notification;
use crate::{auth, new_id, now_secs, AppState};

/// Ingest envelope. The recipient is addressed by `user_sub` (preferred) OR `user_email`; the
/// notification is stored under whichever resolves (the inbox matches by either).
#[derive(Debug, Deserialize)]
pub struct NotifyBody {
    #[serde(default)]
    pub user_sub: Option<String>,
    #[serde(default)]
    pub user_email: Option<String>,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// `POST /api/notify` — create a notification + fan out. Returns the created row as JSON.
pub async fn notify(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<NotifyBody>,
) -> Response {
    match handle(&state, &headers, body).await {
        Ok(notification) => Json(notification).into_response_json(),
        Err(e) => e.into_json(),
    }
}

/// Inner logic, returning the created notification or a typed error.
async fn handle(state: &AppState, headers: &HeaderMap, body: NotifyBody) -> Result<Notification, AppError> {
    if !auth::ingest_authorized(headers, state.config.ingest_token.as_deref()) {
        return Err(AppError::Unauthorized("invalid or missing ingest token".to_string()));
    }

    // Recipient key: prefer the stable subject, fall back to the email address.
    let user_key = body
        .user_sub
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| body.user_email.as_deref().map(str::trim).filter(|s| !s.is_empty()))
        .ok_or_else(|| AppError::InvalidRequest("user_sub or user_email is required".to_string()))?
        .to_string();

    let source = body.source.trim();
    if source.is_empty() {
        return Err(AppError::InvalidRequest("source is required".to_string()));
    }
    let title = body.title.trim();
    if title.is_empty() {
        return Err(AppError::InvalidRequest("title is required".to_string()));
    }

    let mut body_text = body.body.unwrap_or_default().trim().to_string();
    body_text.truncate_chars(MAX_BODY_CHARS);
    let mut title = title.to_string();
    title.truncate_chars(MAX_TITLE_CHARS);
    let url = body.url.unwrap_or_default().trim().to_string();

    // Severity is an optional producer classification used only for per-severity muting; normalize
    // to a short lower-case token and default to `info` when absent/blank.
    let mut severity = body
        .severity
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if severity.is_empty() {
        severity = "info".to_string();
    }
    severity.truncate_chars(MAX_SEVERITY_CHARS);

    let now = now_secs();
    let notification = Notification {
        id: new_id("ntf"),
        user_sub: user_key,
        source: source.to_string(),
        severity,
        title,
        body: body_text,
        url,
        created_at: now,
        read_at: 0,
    };

    state.store.create_notification(&notification).await?;
    tracing::info!(id = %notification.id, source = %notification.source, "notification ingested");

    // Best-effort fan-out on a detached task — never blocks this response.
    delivery::fan_out(state.clone(), notification.clone());

    Ok(notification)
}

/// Bridge so the JSON success path returns a `Response` of the same type as the error path.
trait IntoJsonResponse {
    fn into_response_json(self) -> Response;
}

impl IntoJsonResponse for Json<Notification> {
    fn into_response_json(self) -> Response {
        use axum::response::IntoResponse;
        self.into_response()
    }
}

/// Char-bounded truncation helper (never splits a UTF-8 boundary).
trait TruncateChars {
    fn truncate_chars(&mut self, max: usize);
}

impl TruncateChars for String {
    fn truncate_chars(&mut self, max: usize) {
        if self.chars().count() > max {
            let truncated: String = self.chars().take(max).collect();
            *self = truncated;
        }
    }
}
