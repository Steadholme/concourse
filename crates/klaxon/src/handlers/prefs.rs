//! Owner-scoped delivery-preferences self-management (view / save).
//!
//! Like the inbox and webhooks pages, this sits behind the Sluice `auth=sso` route: the viewer is
//! ALWAYS the gateway-injected `X-Auth-Subject` (never a client field), and preferences are scoped
//! to that subject. The single state-changing POST (`/settings/prefs`) is double-submit CSRF
//! protected and audited after a successful save.
//!
//! Preferences gate the REAL-TIME delivery channels only (push / webhook / email). A muted, quiet,
//! or digest-mode owner still receives every notification in their inbox — nothing is dropped, the
//! fan-out just skips the outbound channels (see `delivery`).

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Form;
use serde::Deserialize;

use crate::audit::AuditEvent;
use crate::config::{MAX_MUTES_PER_KIND, MAX_MUTE_VALUE_CHARS};
use crate::error::AppError;
use crate::handlers::{app_css, esc, topbar};
use crate::store::NotifyPrefs;
use crate::{auth, now_secs, AppState};

const PREFS_HTML: &str = include_str!("../../templates/prefs.html");

/// Save-preferences form. Checkboxes are absent when unchecked (browser semantics), so the two
/// boolean toggles are `Option<String>` and read as "present == on". Quiet-hours arrive as the
/// `HH:MM` value of an `<input type="time">` (empty when cleared). The mute lists are free text.
#[derive(Debug, Deserialize)]
pub struct PrefsForm {
    #[serde(default)]
    pub csrf_token: String,
    #[serde(default)]
    pub mute_all: Option<String>,
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(default)]
    pub quiet_start: String,
    #[serde(default)]
    pub quiet_end: String,
    #[serde(default)]
    pub muted_sources: String,
    #[serde(default)]
    pub muted_severities: String,
}

// ---------------------------------------------------------------------------
// GET /settings/prefs — the preferences page
// ---------------------------------------------------------------------------

/// `GET /settings/prefs` — this owner's saved delivery preferences, pre-filled into the form.
pub async fn index(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    let email = auth::display_email(&headers);
    let (csrf, set_cookie) = auth::ensure_csrf(&headers);

    let prefs = state.store.get_prefs(&keys[0]).await;
    let page = render(&prefs, &csrf, &email);
    Ok(html_with_cookie(page, set_cookie))
}

/// Fill the template. Every interpolated field is HTML-escaped; the boolean toggles become a
/// `checked` attribute; the quiet-hours minutes render back as `HH:MM` (empty when unset).
fn render(prefs: &NotifyPrefs, csrf: &str, email: &str) -> String {
    let checked = |on: bool| if on { "checked" } else { "" };
    PREFS_HTML
        .replace("{{CSS}}", app_css())
        .replace("{{TOPBAR}}", &topbar("Preferences", email))
        .replace("{{CSRF}}", &esc(csrf))
        .replace("{{MUTE_ALL_CHECKED}}", checked(prefs.mute_all))
        .replace("{{DIGEST_CHECKED}}", checked(prefs.digest))
        .replace("{{QUIET_START}}", &esc(&minutes_to_hhmm(prefs.quiet_start)))
        .replace("{{QUIET_END}}", &esc(&minutes_to_hhmm(prefs.quiet_end)))
        .replace("{{MUTED_SOURCES}}", &esc(&prefs.muted_sources.join(", ")))
        .replace("{{MUTED_SEVERITIES}}", &esc(&prefs.muted_severities.join(", ")))
}

// ---------------------------------------------------------------------------
// POST /settings/prefs — save preferences
// ---------------------------------------------------------------------------

/// `POST /settings/prefs` — replace the signed-in owner's delivery preferences.
pub async fn save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<PrefsForm>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    auth::verify_csrf(&headers, &form.csrf_token)?;

    let quiet_start = parse_hhmm(&form.quiet_start)?;
    let quiet_end = parse_hhmm(&form.quiet_end)?;

    let prefs = NotifyPrefs {
        user_sub: keys[0].clone(),
        mute_all: form.mute_all.is_some(),
        quiet_start,
        quiet_end,
        digest: form.digest.is_some(),
        updated_at: now_secs(),
        muted_sources: parse_mute_list(&form.muted_sources),
        muted_severities: parse_mute_list(&form.muted_severities),
    };
    state.store.set_prefs(&prefs).await?;
    tracing::info!(user = %prefs.user_sub, "delivery preferences saved");

    // Audit: value-free flags only (never the muted tokens themselves).
    state.audit.emit(AuditEvent::notice(
        "prefs.update",
        &prefs.user_sub,
        &prefs.user_sub,
        &format!(
            "mute_all={} digest={} quiet={} sources={} severities={}",
            prefs.mute_all,
            prefs.digest,
            quiet_start >= 0 && quiet_end >= 0 && quiet_start != quiet_end,
            prefs.muted_sources.len(),
            prefs.muted_severities.len()
        ),
    ));

    Ok(redirect("/settings/prefs"))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse an `<input type="time">` `HH:MM` value into a minute-of-day (`0..=1439`). An empty value
/// means "unset" (`-1`); a malformed value is a 400.
fn parse_hhmm(raw: &str) -> Result<i64, AppError> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(-1);
    }
    let (h, m) = t
        .split_once(':')
        .ok_or_else(|| AppError::InvalidRequest("quiet hours must be HH:MM".to_string()))?;
    let h: i64 = h
        .parse()
        .map_err(|_| AppError::InvalidRequest("quiet hours must be HH:MM".to_string()))?;
    let m: i64 = m
        .parse()
        .map_err(|_| AppError::InvalidRequest("quiet hours must be HH:MM".to_string()))?;
    if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
        return Err(AppError::InvalidRequest("quiet hours out of range".to_string()));
    }
    Ok(h * 60 + m)
}

/// Render a minute-of-day back to `HH:MM`, or the empty string when unset (`< 0`).
fn minutes_to_hhmm(minutes: i64) -> String {
    if minutes < 0 {
        return String::new();
    }
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// Parse a comma-separated free-text mute list into a normalized set: trimmed, lower-cased, blanks
/// dropped, over-long tokens dropped, de-duplicated, and capped at [`MAX_MUTES_PER_KIND`].
fn parse_mute_list(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let token = part.trim().to_ascii_lowercase();
        if token.is_empty() || token.chars().count() > MAX_MUTE_VALUE_CHARS {
            continue;
        }
        if !out.contains(&token) {
            out.push(token);
        }
        if out.len() >= MAX_MUTES_PER_KIND {
            break;
        }
    }
    out
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
    fn parse_hhmm_roundtrips_and_rejects_garbage() {
        assert_eq!(parse_hhmm("").unwrap(), -1);
        assert_eq!(parse_hhmm("  ").unwrap(), -1);
        assert_eq!(parse_hhmm("00:00").unwrap(), 0);
        assert_eq!(parse_hhmm("22:30").unwrap(), 22 * 60 + 30);
        assert_eq!(parse_hhmm("23:59").unwrap(), 1439);
        assert!(parse_hhmm("24:00").is_err());
        assert!(parse_hhmm("10:60").is_err());
        assert!(parse_hhmm("nope").is_err());
    }

    #[test]
    fn minutes_to_hhmm_formats_and_blanks_unset() {
        assert_eq!(minutes_to_hhmm(-1), "");
        assert_eq!(minutes_to_hhmm(0), "00:00");
        assert_eq!(minutes_to_hhmm(9 * 60 + 5), "09:05");
        assert_eq!(minutes_to_hhmm(1439), "23:59");
    }

    #[test]
    fn parse_mute_list_normalizes_dedupes_and_caps() {
        let got = parse_mute_list("  Loom , sanctum ,LOOM,, beacon ");
        assert_eq!(got, vec!["loom".to_string(), "sanctum".to_string(), "beacon".to_string()]);
        assert!(parse_mute_list("").is_empty());

        let long: String = "x".repeat(MAX_MUTE_VALUE_CHARS + 1);
        assert!(parse_mute_list(&long).is_empty(), "over-long tokens are dropped");

        let many: String = (0..(MAX_MUTES_PER_KIND + 10)).map(|i| format!("s{i},")).collect();
        assert_eq!(parse_mute_list(&many).len(), MAX_MUTES_PER_KIND);
    }
}
