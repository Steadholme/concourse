//! HTTP handlers + shared server-render helpers.
//!
//! `health` is the unauthenticated liveness probe; `dashboard` renders the unified activity inbox.
//!
//! The shared design tokens / CSS are embedded (via `include_str!`) and inlined into every page,
//! matching the HOLDFAST service UI kit (the same polished, light-canvas look as the apex portal):
//! shield wordmark, refined app-bar (All-apps pill + user chip), indigo accent, cards, WCAG-AA contrast.

pub mod actions;
pub mod api;
pub mod dashboard;
pub mod health;

use axum::http::StatusCode;
use serde::Deserialize;
use std::sync::OnceLock;

/// The unified search + source-filter query string shared by the dashboard page and the JSON poll
/// (`?q=` free text, `?source=` one of `chat` / `notifications` / `feed` / `all`). Both fields are
/// optional so a bare `/` or `/api/inbox` still deserializes.
#[derive(Debug, Default, Deserialize)]
pub struct InboxQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

/// Embedded service CSS layered after Odyssey's canonical HOLDFAST design system.
pub const SERVICE_CSS: &str = include_str!("../../static/service.css");
static APP_CSS: OnceLock<String> = OnceLock::new();

/// Cross-subdomain gateway logout (Atrium lives at inbox.w33d.xyz; the IdP is at id.w33d.xyz).
pub const LOGOUT_URL: &str = "https://sso.w33d.xyz/_gw/auth/logout";

/// The HOLDFAST shield glyph (small, for the app-bar brand lockup).
pub const SHIELD_SVG: &str = r##"<svg viewBox="0 0 48 48" fill="none" xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="hf-shield-sm" x1="8" y1="4" x2="40" y2="44" gradientUnits="userSpaceOnUse"><stop stop-color="#818CF8"/><stop offset="1" stop-color="#4F46E5"/></linearGradient></defs><path d="M24 4 8 9.5V22c0 11 7 17.4 16 21.5C33 39.4 40 33 40 22V9.5L24 4Z" fill="url(#hf-shield-sm)"/><rect x="20" y="19" width="8" height="13" rx="1" fill="#fff" fill-opacity="0.92"/><path d="M20 19v-2.5a4 4 0 0 1 8 0V19" stroke="#fff" stroke-width="2" stroke-opacity="0.92" fill="none"/></svg>"##;

/// Full CSS payload: canonical Odyssey first, Atrium's service layer second.
pub fn app_css() -> &'static str {
    APP_CSS.get_or_init(|| {
        let mut css = String::with_capacity(odyssey::APP_CSS.len() + SERVICE_CSS.len() + 1);
        css.push_str(odyssey::APP_CSS);
        css.push('\n');
        css.push_str(SERVICE_CSS);
        css
    })
}

/// Minimal HTML escaping for text/attribute interpolation (defense-in-depth on every field —
/// chat/notification/feed text comes from other services and is treated as untrusted user content).
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Truncate a snippet to `max` chars on a char boundary, appending an ellipsis when cut. Keeps a
/// long chat line / feed summary from blowing out a column.
pub fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Lucide-style line icons (viewBox 0 0 24 24, no fill, rounded caps) used across the app-bar.
const IC_GRID: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/></svg>"#;
const IC_USER: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/></svg>"#;
const IC_LOGOUT: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><polyline points="16 17 21 12 16 7"/><line x1="21" y1="12" x2="9" y2="12"/></svg>"#;
const IC_INBOX: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><polyline points="22 12 16 12 14 15 10 15 8 12 2 12"/><path d="M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z"/></svg>"#;

/// The signed-in avatar menu — a CSS focus-within dropdown from the v2 kit. The button shows the
/// user's initial + email; the dropdown lists Account, All apps, and the PRESERVED gateway sign-out
/// link (same route/method as before). A blank identity falls back to a static glyph so the chrome
/// always renders.
fn usermenu(email: &str) -> String {
    let signed_in = !(email.is_empty() || email == "—");
    let (initial, name, head) = if signed_in {
        let initial = email
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "H".to_string());
        let initial = esc(&initial);
        let head = format!(
            r#"<div class="usermenu__head"><span class="avatar" aria-hidden="true">{initial}</span><div><b>{email}</b><span>Signed in</span></div></div>"#,
            initial = initial,
            email = esc(email),
        );
        (initial, esc(email), head)
    } else {
        ("?".to_string(), "Account".to_string(), String::new())
    };
    format!(
        r#"<div class="usermenu">
        <button class="usermenu__btn" type="button" aria-haspopup="menu" aria-expanded="false">
          <span class="avatar" aria-hidden="true">{initial}</span>
          <span class="usermenu__name">{name}</span>
          <svg class="usermenu__caret" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m6 9 6 6 6-6"/></svg>
        </button>
        <div class="usermenu__pop" role="menu">
          {head}
          <a class="menuitem" role="menuitem" href="https://account.w33d.xyz">{user}Account</a>
          <a class="menuitem" role="menuitem" href="https://w33d.xyz">{grid}All apps</a>
          <a class="menuitem menuitem--danger" role="menuitem" href="{logout}">{out}Log out</a>
        </div>
      </div>"#,
        initial = initial,
        name = name,
        head = head,
        user = IC_USER,
        grid = IC_GRID,
        out = IC_LOGOUT,
        logout = LOGOUT_URL,
    )
}

/// Render the shared v2 app-bar: an app-tile (inbox in Inbox's blue) + the Atrium/host lockup on
/// the left; the surface's Inbox nav; then an "All apps" button back to the apex portal and the
/// signed-in avatar menu on the right. A probe with no identity (email `"—"`) still renders a
/// minimal avatar. Routes and the gateway logout are unchanged — only the chrome is modernized.
pub fn topbar(page_title: &str, email: &str) -> String {
    let _ = page_title;
    format!(
        r#"<header class="appbar">
  <a class="appbar__brand" href="/" aria-label="HOLDFAST Atrium">
    <span class="app-tile" style="--app:#2563eb;--app-soft:#e6effe" aria-hidden="true">{tile}</span>
    <span class="appbar__name"><b>Atrium</b><span>inbox.w33d.xyz</span></span>
  </a>
  <nav class="appbar__nav" aria-label="Atrium"><a class="appnav is-active" href="/">{tab}Inbox</a></nav>
  <div class="appbar__spacer"></div>
  <div class="appbar__right">
    <a class="iconbtn" href="https://w33d.xyz" title="All apps" aria-label="All apps">{grid}</a>
    {usermenu}
  </div>
</header>"#,
        tile = IC_INBOX,
        tab = IC_INBOX,
        grid = IC_GRID,
        usermenu = usermenu(email),
    )
}

/// Format epoch seconds as a compact relative age (`just now`, `5m`, `3h`, `2d`), falling back to
/// a UTC date for anything older than ~30 days. std `time` only — no extra C deps.
pub fn rel_time(then: i64, now: i64) -> String {
    let d = now - then;
    if d < 0 {
        return "just now".to_string();
    }
    match d {
        0..=44 => "just now".to_string(),
        45..=5399 => format!("{}m", (d + 59) / 60),       // up to ~90m -> minutes
        5400..=86399 => format!("{}h", (d + 1799) / 3600), // up to ~24h -> hours
        86400..=2591999 => format!("{}d", d / 86400),      // up to ~30d -> days
        _ => fmt_date(then),
    }
}

/// Format epoch seconds as a compact UTC date `Mon D, YYYY` (e.g. `Jun 29, 2026`).
pub fn fmt_date(secs: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(secs) {
        Ok(dt) => format!("{} {}, {}", month_abbr(dt.month()), dt.day(), dt.year()),
        Err(_) => secs.to_string(),
    }
}

fn month_abbr(m: time::Month) -> &'static str {
    use time::Month::*;
    match m {
        January => "Jan",
        February => "Feb",
        March => "Mar",
        April => "Apr",
        May => "May",
        June => "Jun",
        July => "Jul",
        August => "Aug",
        September => "Sep",
        October => "Oct",
        November => "Nov",
        December => "Dec",
    }
}

/// A small, branded HTML error page (used by [`crate::error::AppError`]).
pub fn error_page(status: StatusCode, message: &str) -> String {
    let code = status.as_u16();
    let reason = status.canonical_reason().unwrap_or("Error");
    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark light">
<title>{code} {reason} · Atrium</title><style>{css}</style></head>
<body class="page-app">
{topbar}
<main class="shell">
  <div class="error-card">
    <div class="error-card__code">{code}</div>
    <h1 class="error-card__title">{reason}</h1>
    <p class="error-card__msg">{msg}</p>
    <a class="btn btn-primary" href="/">Back to the inbox</a>
  </div>
</main>
</body></html>"#,
        css = app_css(),
        topbar = topbar("Atrium", "—"),
        code = code,
        reason = esc(reason),
        msg = esc(message),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esc_neutralizes_markup() {
        assert_eq!(esc("<b>&\"'"), "&lt;b&gt;&amp;&quot;&#x27;");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hell…");
    }

    #[test]
    fn rel_time_buckets() {
        assert_eq!(rel_time(1000, 1010), "just now");
        assert_eq!(rel_time(1000, 1000 + 600), "10m");
        assert_eq!(rel_time(1000, 1000 + 7200), "2h");
        assert_eq!(rel_time(1000, 1000 + 3 * 86400), "3d");
    }
}
