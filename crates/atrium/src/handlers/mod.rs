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

/// Embedded design system, inlined into each rendered page's `<style>`.
pub const APP_CSS: &str = include_str!("../../static/app.css");

/// Cross-subdomain gateway logout (Atrium lives at inbox.w33d.xyz; the IdP is at id.w33d.xyz).
pub const LOGOUT_URL: &str = "https://sso.w33d.xyz/_gw/auth/logout";

/// The HOLDFAST shield glyph (small, for the app-bar brand lockup).
pub const SHIELD_SVG: &str = r##"<svg viewBox="0 0 48 48" fill="none" xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="hf-shield-sm" x1="8" y1="4" x2="40" y2="44" gradientUnits="userSpaceOnUse"><stop stop-color="#818CF8"/><stop offset="1" stop-color="#4F46E5"/></linearGradient></defs><path d="M24 4 8 9.5V22c0 11 7 17.4 16 21.5C33 39.4 40 33 40 22V9.5L24 4Z" fill="url(#hf-shield-sm)"/><rect x="20" y="19" width="8" height="13" rx="1" fill="#fff" fill-opacity="0.92"/><path d="M20 19v-2.5a4 4 0 0 1 8 0V19" stroke="#fff" stroke-width="2" stroke-opacity="0.92" fill="none"/></svg>"##;

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

/// Render the shared app-bar: shield + HOLDFAST wordmark on the left; the page title, an
/// "All apps" pill back to the apex portal, the signed-in user chip (avatar initial + email),
/// and a Logout link to the gateway on the right. A probe with no known identity (email "—")
/// drops the user chip but keeps the rest of the chrome.
pub fn topbar(page_title: &str, email: &str) -> String {
    let chip = if email.is_empty() || email == "—" {
        String::new()
    } else {
        let initial = email
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "H".to_string());
        format!(
            r#"<span class="userchip"><span class="userchip__avatar" aria-hidden="true">{initial}</span><span class="user-email">{email}</span></span>"#,
            initial = esc(&initial),
            email = esc(email),
        )
    };
    format!(
        r#"<header class="topbar">
  <div class="topbar__inner">
    <a class="brand" href="/" aria-label="HOLDFAST Atrium">
      <span class="brand__glyph" aria-hidden="true">{shield}</span>
      <span class="brand__word">HOLDFAST</span>
    </a>
    <div class="topbar__right">
      <span class="topbar__title">{title}</span>
      <a class="allapps" href="https://w33d.xyz" title="All apps"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/></svg>All apps</a>
      {chip}
      <a class="btn btn-ghost btn-sm" href="{logout}">Log out</a>
    </div>
  </div>
</header>"#,
        shield = SHIELD_SVG,
        title = esc(page_title),
        chip = chip,
        logout = LOGOUT_URL,
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
        css = APP_CSS,
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
