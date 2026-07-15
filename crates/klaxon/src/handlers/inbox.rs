//! The SSO notification inbox: list, mark-read, Web Push subscribe/unsubscribe, the self test
//! notification, and the live SSE stream.
//!
//! Every endpoint here is mounted behind the Sluice `auth=sso` route: the viewer identity is
//! ALWAYS taken from the injected `X-Auth-Subject` / `X-Auth-Email` (never a client field), and the
//! inbox is scoped to that identity. State-changing POSTs (`/api/read`, `/api/subscribe`,
//! `/api/unsubscribe`, `/api/test`) are double-submit CSRF protected.

use std::collections::HashSet;
use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::{Form, Json};
use serde::Deserialize;

use crate::audit::AuditEvent;
use crate::auth;
use crate::delivery;
use crate::error::AppError;
use crate::handlers::{app_css, esc, fmt_datetime, safe_url, topbar};
use crate::store::{Notification, PushSubscription};
use crate::{new_id, now_secs, AppState};

const DASHBOARD_HTML: &str = include_str!("../../templates/dashboard.html");
const SERVICE_WORKER_JS: &str = include_str!("../../static/sw.js");

/// The Odyssey v2 `.empty` kit state shown when the inbox has no notifications. Reused verbatim by
/// the page JS when the last card is dismissed, so the calm empty state is identical either way.
const EMPTY_INBOX_HTML: &str = r#"<div class="empty" id="inbox-empty">
  <div class="empty__ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9"/><path d="M10.3 21a1.94 1.94 0 0 0 3.4 0"/></svg></div>
  <h3>You're all caught up</h3>
  <p>New notifications from across the Steadholme estate will land here.</p>
  <a class="btn btn-primary btn-sm" href="/settings/prefs">Delivery preferences</a>
</div>"#;

/// Mark-read form: an optional `id` (mark one) or none (mark all unread), plus the CSRF token.
#[derive(Debug, Deserialize)]
pub struct ReadForm {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub csrf_token: String,
}

/// Web Push subscription form (posted by the page's registration JS).
#[derive(Debug, Deserialize)]
pub struct SubscribeForm {
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub p256dh: String,
    #[serde(default)]
    pub auth: String,
    #[serde(default)]
    pub csrf_token: String,
}

/// Web Push unsubscribe form (posted by the page's registration JS): the subscription `endpoint`
/// to remove, plus the CSRF token.
#[derive(Debug, Deserialize)]
pub struct UnsubscribeForm {
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub csrf_token: String,
}

/// Test-notification form: just the CSRF token (the recipient is ALWAYS the signed-in user).
#[derive(Debug, Deserialize)]
pub struct TestForm {
    #[serde(default)]
    pub csrf_token: String,
}

// ---------------------------------------------------------------------------
// GET / — the inbox
// ---------------------------------------------------------------------------

/// `GET /` — this user's notifications (unread first) + delivery prefs + push registration.
pub async fn index(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    let email = auth::display_email(&headers);
    let (csrf, set_cookie) = auth::ensure_csrf(&headers);

    let notifications = state.store.list_notifications(&keys).await;
    let unread = notifications.iter().filter(|n| n.read_at == 0).count();

    let mut items = String::new();
    if notifications.is_empty() {
        items.push_str(EMPTY_INBOX_HTML);
    } else {
        for n in &notifications {
            items.push_str(&render_item(n, &csrf));
        }
    }

    let push_state = if state.config.push_configured() {
        r#"<span class="badge badge-ok">Configured</span>"#
    } else {
        r#"<span class="badge badge-muted">Not configured</span>"#
    };
    let mark_all = if unread > 0 {
        format!(
            r#"<form class="inline-form" method="post" action="/api/read">
    <input type="hidden" name="csrf_token" value="{csrf}">
    <button class="btn btn-secondary btn-sm" type="submit">Mark all read</button>
  </form>"#,
            csrf = esc(&csrf)
        )
    } else {
        String::new()
    };

    let vapid_public = state.config.vapid_public_key.clone().unwrap_or_default();

    let page = DASHBOARD_HTML
        .replace("{{CSS}}", app_css())
        .replace("{{TOPBAR}}", &topbar("Notifications", &email))
        .replace("{{UNREAD}}", &unread.to_string())
        .replace("{{MARK_ALL}}", &mark_all)
        .replace("{{PUSH_STATE}}", push_state)
        .replace("{{CSRF}}", &esc(&csrf))
        .replace("{{VAPID_PUBLIC}}", &esc(&vapid_public))
        .replace("{{ITEMS}}", &items);

    Ok(html_with_cookie(page, set_cookie))
}

/// One notification card. Every interpolated field is HTML-escaped; the open-link URL is scheme-
/// sanitized. Unread cards carry a `Mark read` inline form.
fn render_item(n: &crate::store::Notification, csrf: &str) -> String {
    let unread_class = if n.read_at == 0 { " card-note--unread" } else { "" };
    let unread_dot = if n.read_at == 0 {
        r#"<span class="note-dot" aria-label="unread"></span>"#
    } else {
        ""
    };
    let open_link = if n.url.is_empty() {
        String::new()
    } else {
        format!(
            r#" · <a class="note__open" href="{href}" rel="noopener noreferrer">Open ↗</a>"#,
            href = esc(&safe_url(&n.url))
        )
    };
    let mark = if n.read_at == 0 {
        format!(
            r#"<form class="inline-form note__read" method="post" action="/api/read">
      <input type="hidden" name="csrf_token" value="{csrf}">
      <input type="hidden" name="id" value="{id}">
      <button class="btn btn-ghost btn-sm" type="submit">Mark read</button>
    </form>"#,
            csrf = esc(csrf),
            id = esc(&n.id)
        )
    } else {
        String::new()
    };
    // Dismiss is available on every card (read or not); it hits the additive /api/dismiss route
    // (progressive-enhancement form — the page JS upgrades it to an optimistic /api/dismiss.json).
    let dismiss = format!(
        r#"<form class="inline-form note__dismiss" method="post" action="/api/dismiss">
      <input type="hidden" name="csrf_token" value="{csrf}">
      <input type="hidden" name="id" value="{id}">
      <button class="btn btn-subtle btn-sm" type="submit">Dismiss</button>
    </form>"#,
        csrf = esc(csrf),
        id = esc(&n.id)
    );
    let body_html = if n.body.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="note__body">{}</p>"#, esc(&n.body))
    };
    format!(
        r#"<article class="card-note{unread_class}" data-id="{data_id}" data-read="{read}" data-source="{source}">
  <div class="note__head">
    <span class="badge badge-source">{source}</span>
    <h2 class="note__title">{dot}{title}</h2>
  </div>
  {body}
  <div class="note__meta">{date}{open}</div>
  <div class="note__actions">{mark}{dismiss}</div>
</article>"#,
        unread_class = unread_class,
        data_id = esc(&n.id),
        read = if n.read_at == 0 { "0" } else { "1" },
        source = esc(&n.source),
        dot = unread_dot,
        title = esc(&n.title),
        body = body_html,
        date = esc(&fmt_datetime(n.created_at)),
        open = open_link,
        mark = mark,
        dismiss = dismiss,
    )
}

// ---------------------------------------------------------------------------
// POST /api/read — mark one/all read
// ---------------------------------------------------------------------------

/// `POST /api/read` — mark one notification (when `id` is present) or all unread read.
pub async fn mark_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ReadForm>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    auth::verify_csrf(&headers, &form.csrf_token)?;

    let id = {
        let t = form.id.trim();
        if t.is_empty() { None } else { Some(t) }
    };
    let updated = state.store.mark_read(&keys, id, now_secs()).await?;
    tracing::info!(updated, all = id.is_none(), "notifications marked read");

    Ok(redirect("/"))
}

// ---------------------------------------------------------------------------
// JSON siblings for the no-reload inbox (progressive enhancement)
// ---------------------------------------------------------------------------

/// JSON body for the optimistic inbox actions: an optional `id` (omit to mark all read) plus the
/// double-submit CSRF token (echoed from the page cookie, exactly like the form field).
#[derive(Debug, Deserialize)]
pub struct ActionJson {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub csrf_token: String,
}

/// `POST /api/read.json` — JSON sibling of [`mark_read`]. Same identity + CSRF + owner scope as the
/// form route (which is unchanged); returns `{ok, updated, unread}` so the page updates the badge in
/// place instead of reloading. No-JS users still use the `/api/read` form.
pub async fn mark_read_json(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ActionJson>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    auth::verify_csrf(&headers, &body.csrf_token)?;
    let id = {
        let t = body.id.trim();
        if t.is_empty() { None } else { Some(t) }
    };
    let updated = state.store.mark_read(&keys, id, now_secs()).await?;
    let unread = state.store.unread_count(&keys).await;
    tracing::info!(updated, all = id.is_none(), "notifications marked read (json)");
    Ok(Json(serde_json::json!({ "ok": true, "updated": updated, "unread": unread })).into_response())
}

/// `POST /api/dismiss` — remove one notification from the inbox. A progressive-enhancement FORM
/// route (no-JS users get a 303 redirect), owner-scoped + CSRF, audited like the read path.
pub async fn dismiss(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ActionJson>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    auth::verify_csrf(&headers, &form.csrf_token)?;
    let id = form.id.trim();
    if id.is_empty() {
        return Err(AppError::InvalidRequest("id is required".to_string()));
    }
    let removed = state.store.delete_notification(&keys, id).await?;
    if removed > 0 {
        state.audit.emit(AuditEvent::notice(
            "notify.dismiss",
            &keys[0],
            id,
            "notification dismissed",
        ));
    }
    tracing::info!(user = %keys[0], removed, "notification dismissed");
    Ok(redirect("/"))
}

/// `POST /api/dismiss.json` — JSON sibling of [`dismiss`] for the optimistic inbox: same CSRF +
/// owner-scoped delete + audit; returns `{ok, removed, unread}`.
pub async fn dismiss_json(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ActionJson>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    auth::verify_csrf(&headers, &body.csrf_token)?;
    let id = body.id.trim();
    if id.is_empty() {
        return Err(AppError::InvalidRequest("id is required".to_string()));
    }
    let removed = state.store.delete_notification(&keys, id).await?;
    if removed > 0 {
        state.audit.emit(AuditEvent::notice(
            "notify.dismiss",
            &keys[0],
            id,
            "notification dismissed",
        ));
    }
    let unread = state.store.unread_count(&keys).await;
    Ok(Json(serde_json::json!({ "ok": true, "removed": removed, "unread": unread })).into_response())
}

// ---------------------------------------------------------------------------
// POST /api/subscribe — store a Web Push subscription
// ---------------------------------------------------------------------------

/// `POST /api/subscribe` — persist this browser's Web Push subscription for the signed-in user.
pub async fn subscribe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SubscribeForm>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    auth::verify_csrf(&headers, &form.csrf_token)?;

    let endpoint = form.endpoint.trim();
    if endpoint.is_empty() {
        return Err(AppError::InvalidRequest("endpoint is required".to_string()));
    }
    let sub = PushSubscription {
        id: new_id("psub"),
        // The subject is always the first key (the gateway-injected X-Auth-Subject).
        user_sub: keys[0].clone(),
        endpoint: endpoint.to_string(),
        p256dh: form.p256dh.trim().to_string(),
        auth: form.auth.trim().to_string(),
        created_at: now_secs(),
    };
    state.store.upsert_subscription(&sub).await?;
    tracing::info!(user = %sub.user_sub, "web push subscription stored");

    // Audit: value-free detail (a push endpoint is a capability URL — never log it).
    state.audit.emit(AuditEvent::notice(
        "push.subscribe",
        &sub.user_sub,
        &sub.id,
        "web push subscription stored",
    ));

    Ok(redirect("/"))
}

// ---------------------------------------------------------------------------
// POST /api/unsubscribe — remove a Web Push subscription
// ---------------------------------------------------------------------------

/// `POST /api/unsubscribe` — remove this browser's Web Push subscription for the signed-in user.
/// The delete is owner-scoped in the store, so a forged `endpoint` belonging to another user's
/// subscription removes nothing.
pub async fn unsubscribe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<UnsubscribeForm>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    auth::verify_csrf(&headers, &form.csrf_token)?;

    let endpoint = form.endpoint.trim();
    if endpoint.is_empty() {
        return Err(AppError::InvalidRequest("endpoint is required".to_string()));
    }

    let owner = keys[0].clone();
    let removed = state.store.delete_subscription(&owner, endpoint).await?;
    tracing::info!(user = %owner, removed, "web push unsubscribe");

    if removed > 0 {
        state.audit.emit(AuditEvent::notice(
            "push.unsubscribe",
            &owner,
            &owner,
            "web push subscription removed",
        ));
    }

    Ok(redirect("/"))
}

// ---------------------------------------------------------------------------
// POST /api/test — send yourself a test notification
// ---------------------------------------------------------------------------

/// `POST /api/test` — store a self-addressed test notification and fan it out through the normal
/// delivery path (Web Push / webhooks / email), honoring the owner's delivery preferences exactly
/// like a real ingest. Lets a user verify their freshly-registered channels end-to-end.
pub async fn send_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TestForm>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    auth::verify_csrf(&headers, &form.csrf_token)?;

    let notification = Notification {
        id: new_id("ntf"),
        // The recipient is ALWAYS the signed-in subject — a user can only test their own channels.
        user_sub: keys[0].clone(),
        source: "klaxon".to_string(),
        severity: "info".to_string(),
        title: "Test notification".to_string(),
        body: "This is a test from your Klaxon inbox. If it reached this browser or your webhook, delivery works.".to_string(),
        url: state.config.public_base_url.clone(),
        created_at: now_secs(),
        read_at: 0,
    };
    state.store.create_notification(&notification).await?;
    tracing::info!(user = %notification.user_sub, id = %notification.id, "test notification created");

    state.audit.emit(AuditEvent::notice(
        "notify.test",
        &notification.user_sub,
        &notification.id,
        "self test notification",
    ));

    // Best-effort fan-out on a detached task — never blocks this response.
    delivery::fan_out(state.clone(), notification);

    Ok(redirect("/"))
}

// ---------------------------------------------------------------------------
// GET /api/stream — Server-Sent-Events live stream
// ---------------------------------------------------------------------------

/// `GET /api/stream` — SSE of new notifications for this user. Polls the store every ~2s and emits
/// one `data:` frame (the notification JSON) per newly-seen row; a keep-alive comment holds the
/// connection open between events.
pub async fn stream(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;

    let body = async_stream::stream! {
        let mut since = now_secs();
        let mut seen: HashSet<String> = HashSet::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        loop {
            ticker.tick().await;
            let fresh = state.store.list_since(&keys, since).await;
            for n in fresh {
                if !seen.insert(n.id.clone()) {
                    continue;
                }
                since = since.max(n.created_at);
                let json = serde_json::to_string(&n).unwrap_or_else(|_| "{}".to_string());
                yield Ok::<Event, Infallible>(Event::default().event("notification").data(json));
            }
        }
    };

    Ok(Sse::new(body)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response())
}

// ---------------------------------------------------------------------------
// GET /vapidPublicKey — the configured VAPID application-server public key (or empty)
// ---------------------------------------------------------------------------

/// `GET /vapidPublicKey` — the configured VAPID public key as plain text, or an empty body when
/// Web Push is not configured. No auth (a public key is not a secret).
pub async fn vapid_public_key(State(state): State<AppState>) -> Response {
    let key = state.config.vapid_public_key.clone().unwrap_or_default();
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], key).into_response()
}

/// `GET /sw.js` — the tiny push-display service worker registered by the inbox page. Served with a
/// JS content type so the browser accepts it as a same-origin worker, plus an explicit
/// `Service-Worker-Allowed: /` so the root scope is always permitted.
pub async fn service_worker() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (header::HeaderName::from_static("service-worker-allowed"), "/"),
        ],
        SERVICE_WORKER_JS,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Render helpers
// ---------------------------------------------------------------------------

/// A 303 redirect (post/redirect/get).
fn redirect(location: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, HeaderValue::from_str(location).expect("valid location"))],
    )
        .into_response()
}

/// An HTML response, optionally attaching a freshly-minted CSRF `Set-Cookie`.
fn html_with_cookie(body: String, set_cookie: Option<String>) -> Response {
    let mut resp = Html(body).into_response();
    if let Some(c) = set_cookie {
        if let Ok(value) = HeaderValue::from_str(&c) {
            resp.headers_mut().insert(header::SET_COOKIE, value);
        }
    }
    resp
}
