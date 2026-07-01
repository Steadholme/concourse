//! Owner-scoped webhook self-management (list / create / delete).
//!
//! These endpoints sit behind the Sluice `auth=sso` route, exactly like the inbox: the viewer
//! identity is ALWAYS the gateway-injected `X-Auth-Subject` (never a client field), and every
//! webhook is scoped to that subject — a user only ever sees, adds, or deletes their OWN rows. The
//! two state-changing POSTs (`/settings/webhooks`, `/settings/webhooks/delete`) are double-submit
//! CSRF protected and audited after a successful mutation.
//!
//! The webhook store + fan-out already exist (see `store` / `delivery`); this module only adds the
//! self-service management surface. No Web Push crypto is involved here.

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Form;
use serde::Deserialize;

use crate::audit::AuditEvent;
use crate::config::{MAX_WEBHOOKS_PER_USER, MAX_WEBHOOK_SECRET_CHARS, MAX_WEBHOOK_URL_CHARS};
use crate::error::AppError;
use crate::handlers::{esc, fmt_datetime, topbar, APP_CSS};
use crate::store::Webhook;
use crate::{auth, new_id, now_secs, AppState};

const WEBHOOKS_HTML: &str = include_str!("../../templates/webhooks.html");

/// Create-webhook form: the endpoint URL, an optional signing secret, and the CSRF token.
#[derive(Debug, Deserialize)]
pub struct CreateForm {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub secret: String,
    #[serde(default)]
    pub csrf_token: String,
}

/// Delete-webhook form: the target webhook `id` plus the CSRF token.
#[derive(Debug, Deserialize)]
pub struct DeleteForm {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub csrf_token: String,
}

// ---------------------------------------------------------------------------
// GET /settings/webhooks — the management page
// ---------------------------------------------------------------------------

/// `GET /settings/webhooks` — this user's registered webhooks + a create form.
pub async fn index(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    let email = auth::display_email(&headers);
    let (csrf, set_cookie) = auth::ensure_csrf(&headers);

    // Webhooks are keyed by the subject (the stable gateway id), like Web Push subscriptions.
    let hooks = state.store.list_webhooks(&keys[0]).await;

    let items = if hooks.is_empty() {
        r#"<div class="empty-state"><h2>No webhooks</h2><p>Add an endpoint above and your notifications will be delivered there.</p></div>"#.to_string()
    } else {
        let mut s = String::from(r#"<ul class="list">"#);
        for h in &hooks {
            s.push_str(&render_item(h, &csrf));
        }
        s.push_str("</ul>");
        s
    };

    let page = WEBHOOKS_HTML
        .replace("{{CSS}}", APP_CSS)
        .replace("{{TOPBAR}}", &topbar("Webhooks", &email))
        .replace("{{COUNT}}", &hooks.len().to_string())
        .replace("{{CSRF}}", &esc(&csrf))
        .replace("{{ITEMS}}", &items);

    Ok(html_with_cookie(page, set_cookie))
}

/// One webhook row: the (escaped) URL, its creation date, and an inline CSRF-protected delete form.
/// The secret is NEVER rendered back — only its presence is indicated.
fn render_item(h: &Webhook, csrf: &str) -> String {
    let secret_tag = if h.secret.is_empty() {
        String::new()
    } else {
        r#" · <span class="badge badge-muted">secret set</span>"#.to_string()
    };
    format!(
        r#"<li>
  <span class="title">{url}</span>
  <span class="list__meta">{date}{secret}</span>
  <span class="spacer"></span>
  <form class="inline-form" method="post" action="/settings/webhooks/delete">
    <input type="hidden" name="csrf_token" value="{csrf}">
    <input type="hidden" name="id" value="{id}">
    <button class="btn btn-danger btn-sm" type="submit">Delete</button>
  </form>
</li>"#,
        url = esc(&h.url),
        date = esc(&fmt_datetime(h.created_at)),
        secret = secret_tag,
        csrf = esc(csrf),
        id = esc(&h.id),
    )
}

// ---------------------------------------------------------------------------
// POST /settings/webhooks — create a webhook
// ---------------------------------------------------------------------------

/// `POST /settings/webhooks` — register a new outbound webhook for the signed-in user.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreateForm>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    auth::verify_csrf(&headers, &form.csrf_token)?;

    let url = form.url.trim();
    if url.is_empty() {
        return Err(AppError::InvalidRequest("url is required".to_string()));
    }
    if url.chars().count() > MAX_WEBHOOK_URL_CHARS {
        return Err(AppError::InvalidRequest("url is too long".to_string()));
    }
    if !is_http_url(url) {
        return Err(AppError::InvalidRequest(
            "url must be an http:// or https:// endpoint".to_string(),
        ));
    }
    let secret = form.secret.trim();
    if secret.chars().count() > MAX_WEBHOOK_SECRET_CHARS {
        return Err(AppError::InvalidRequest("secret is too long".to_string()));
    }

    let owner = keys[0].clone();
    if state.store.list_webhooks(&owner).await.len() >= MAX_WEBHOOKS_PER_USER {
        return Err(AppError::InvalidRequest("webhook limit reached".to_string()));
    }

    let hook = Webhook {
        id: new_id("whk"),
        user_sub: owner,
        url: url.to_string(),
        secret: secret.to_string(),
        created_at: now_secs(),
    };
    state.store.create_webhook(&hook).await?;
    tracing::info!(user = %hook.user_sub, id = %hook.id, "webhook created");

    // Audit: short, value-free detail (never the URL or secret).
    state.audit.emit(AuditEvent::notice(
        "webhook.create",
        &hook.user_sub,
        &hook.id,
        "webhook registered",
    ));

    Ok(redirect("/settings/webhooks"))
}

// ---------------------------------------------------------------------------
// POST /settings/webhooks/delete — delete a webhook
// ---------------------------------------------------------------------------

/// `POST /settings/webhooks/delete` — remove one of the signed-in user's webhooks by id. The delete
/// is owner-scoped in the store, so a forged `id` for another user's row deletes nothing.
pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<DeleteForm>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    auth::verify_csrf(&headers, &form.csrf_token)?;

    let id = form.id.trim();
    if id.is_empty() {
        return Err(AppError::InvalidRequest("id is required".to_string()));
    }

    let owner = keys[0].clone();
    let removed = state.store.delete_webhook(&owner, id).await?;
    tracing::info!(user = %owner, id = %id, removed, "webhook delete");

    if removed > 0 {
        state.audit.emit(AuditEvent::notice(
            "webhook.delete",
            &owner,
            id,
            "webhook removed",
        ));
    }

    Ok(redirect("/settings/webhooks"))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// True when `url` is an absolute `http://`/`https://` endpoint with a non-empty authority. Both
/// schemes are accepted (fan-out delivers `http://` and records intent for `https://`).
fn is_http_url(url: &str) -> bool {
    let rest = match url.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") => rest,
        _ => return false,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    !authority.is_empty()
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_http_url_accepts_http_and_https_rejects_others() {
        assert!(is_http_url("http://hooks:9000/in/abc"));
        assert!(is_http_url("https://example.com/hook"));
        assert!(is_http_url("HTTPS://example.com"));
        assert!(!is_http_url("ftp://example.com"));
        assert!(!is_http_url("javascript:alert(1)"));
        assert!(!is_http_url("http://"));
        assert!(!is_http_url("example.com"));
        assert!(!is_http_url(""));
    }
}
