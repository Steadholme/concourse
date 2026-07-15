//! HTTP handlers + shared server-render helpers.
//!
//! `health` is the unauthenticated liveness probe; `notify` is the internal bearer-auth ingest
//! API; `inbox` carries the SSO notification inbox (list, mark-read, push subscribe, SSE stream).
//!
//! The shared design tokens / CSS are embedded (via `include_str!`) and inlined into every page,
//! matching the Steadholme enterprise brand (the same look as the Keystone/inkwell UI): brand
//! gradient, indigo accent, cards, app-bar.

pub mod health;
pub mod inbox;
pub mod notify;
pub mod prefs;
pub mod webhooks;

use axum::http::StatusCode;
use std::sync::OnceLock;

/// Embedded service CSS layered after Odyssey's canonical Steadholme design system.
pub const SERVICE_CSS: &str = include_str!("../../static/service.css");
static APP_CSS: OnceLock<String> = OnceLock::new();

/// Cross-subdomain gateway logout (Klaxon lives at notify.w33d.xyz; the IdP is at id.w33d.xyz).
pub const LOGOUT_URL: &str = "https://sso.w33d.xyz/_gw/auth/logout";

/// The Steadholme shield glyph (small, for the app-bar brand lockup).
pub const SHIELD_SVG: &str = r##"<svg viewBox="0 0 48 48" fill="none" xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="hf-shield-sm" x1="8" y1="4" x2="40" y2="44" gradientUnits="userSpaceOnUse"><stop stop-color="#818CF8"/><stop offset="1" stop-color="#4F46E5"/></linearGradient></defs><path d="M24 4 8 9.5V22c0 11 7 17.4 16 21.5C33 39.4 40 33 40 22V9.5L24 4Z" fill="url(#hf-shield-sm)"/><rect x="20" y="19" width="8" height="13" rx="1" fill="#fff" fill-opacity="0.92"/><path d="M20 19v-2.5a4 4 0 0 1 8 0V19" stroke="#fff" stroke-width="2" stroke-opacity="0.92" fill="none"/></svg>"##;

/// Full CSS payload: canonical Odyssey first, Klaxon's service layer second.
pub fn app_css() -> &'static str {
    APP_CSS.get_or_init(|| {
        let mut css = String::with_capacity(odyssey::APP_CSS.len() + SERVICE_CSS.len() + 1);
        css.push_str(odyssey::APP_CSS);
        css.push('\n');
        css.push_str(SERVICE_CSS);
        css
    })
}

/// Minimal HTML escaping for text/attribute interpolation (defense-in-depth on every field).
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Allow only `http`/`https`/`mailto` (and relative) URLs for the notification "open" link; any
/// other scheme (`javascript:`, `data:`, …) is rewritten to a harmless `#`. The notification `url`
/// is producer-supplied, so it is sanitized before becoming an `href`.
pub fn safe_url(url: &str) -> String {
    let cleaned: String = url
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_ascii_control())
        .collect::<String>()
        .to_ascii_lowercase();
    let safe = match cleaned.find(':') {
        Some(idx) => {
            let scheme = &cleaned[..idx];
            let is_scheme = !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'));
            if is_scheme {
                matches!(scheme, "http" | "https" | "mailto")
            } else {
                true // relative path / fragment
            }
        }
        None => true,
    };
    if safe {
        url.to_string()
    } else {
        "#".to_string()
    }
}

/// Lucide-style line icons (viewBox 0 0 24 24, no fill, rounded caps) used across the app-bar.
const IC_GRID: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/></svg>"#;
const IC_USER: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/></svg>"#;
const IC_LOGOUT: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><polyline points="16 17 21 12 16 7"/><line x1="21" y1="12" x2="9" y2="12"/></svg>"#;

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

/// Render the shared v2 app-bar: an app-tile (bell in Notify's amber) + the Klaxon/host lockup on
/// the left; the surface's nav (Notifications, Preferences, Webhooks) with the current page active;
/// then an "All apps" button back to the apex portal and the signed-in avatar menu on the right.
/// A probe with no identity (email `"—"`) still renders a minimal avatar. Routes and the gateway
/// logout are unchanged — only the chrome is modernized.
pub fn topbar(page_title: &str, email: &str) -> String {
    let tile = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9"/><path d="M10.3 21a1.94 1.94 0 0 0 3.4 0"/></svg>"#;
    let bell = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9"/><path d="M10.3 21a1.94 1.94 0 0 0 3.4 0"/></svg>"#;
    let sliders = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><line x1="4" y1="21" x2="4" y2="14"/><line x1="4" y1="10" x2="4" y2="3"/><line x1="12" y1="21" x2="12" y2="12"/><line x1="12" y1="8" x2="12" y2="3"/><line x1="20" y1="21" x2="20" y2="16"/><line x1="20" y1="12" x2="20" y2="3"/><line x1="2" y1="14" x2="6" y2="14"/><line x1="10" y1="8" x2="14" y2="8"/><line x1="18" y1="16" x2="22" y2="16"/></svg>"#;
    let link = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/></svg>"#;
    let a = |on: bool| if on { " is-active" } else { "" };
    let nav = format!(
        r#"<a class="appnav{n_a}" href="/">{bell}Notifications</a><a class="appnav{p_a}" href="/settings/prefs">{sliders}Preferences</a><a class="appnav{w_a}" href="/settings/webhooks">{link}Webhooks</a>"#,
        n_a = a(page_title == "Notifications"),
        p_a = a(page_title == "Preferences"),
        w_a = a(page_title == "Webhooks"),
        bell = bell,
        sliders = sliders,
        link = link,
    );
    format!(
        r#"<header class="appbar">
  <a class="appbar__brand" href="/" aria-label="Steadholme Klaxon">
    <span class="app-tile" style="--app:#d97706;--app-soft:#fdf1de" aria-hidden="true">{tile}</span>
    <span class="appbar__name"><b>Klaxon</b><span>notify.w33d.xyz</span></span>
  </a>
  <nav class="appbar__nav" aria-label="Klaxon">{nav}</nav>
  <div class="appbar__spacer"></div>
  <div class="appbar__right">
    <a class="iconbtn" href="https://w33d.xyz" title="All apps" aria-label="All apps">{grid}</a>
    {usermenu}
  </div>
</header>"#,
        tile = tile,
        nav = nav,
        grid = IC_GRID,
        usermenu = usermenu(email),
    )
}

/// Format epoch seconds as a compact UTC `Mon D, YYYY · HH:MM` line. std `time` only, no C deps.
pub fn fmt_datetime(secs: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(secs) {
        Ok(dt) => format!(
            "{} {}, {} · {:02}:{:02}",
            month_abbr(dt.month()),
            dt.day(),
            dt.year(),
            dt.hour(),
            dt.minute()
        ),
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
<meta name="color-scheme" content="light">
<title>{code} {reason} · Klaxon</title><style>{css}</style></head>
<body class="page-reading">
{topbar}
<main class="reader">
  <div class="error-card">
    <div class="error-card__code">{code}</div>
    <h1 class="error-card__title">{reason}</h1>
    <p class="error-card__msg">{msg}</p>
    <a class="btn btn-primary" href="/">Back to notifications</a>
  </div>
</main>
</body></html>"#,
        css = app_css(),
        topbar = topbar("Klaxon", "—"),
        code = code,
        reason = esc(reason),
        msg = esc(message),
    )
}
