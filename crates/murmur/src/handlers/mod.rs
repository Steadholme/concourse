//! HTTP/WS handlers + shared server-render helpers.
//!
//! `health` is the unauthenticated liveness probe; `dashboard` renders the SSO dashboard;
//! `rooms` carries the JSON room/message API; `ws` upgrades the live stream.
//!
//! The shared design tokens / CSS + dashboard JS are embedded (via `include_str!`) and inlined
//! into the rendered page, matching the HOLDFAST enterprise brand (the same look as the Keystone
//! / Inkwell UI): brand gradient, indigo accent, dark command-center chrome.

pub mod admin;
pub mod dashboard;
pub mod dms;
pub mod health;
pub mod rooms;
pub mod search;
pub mod ws;

use serde_json::json;
use std::sync::OnceLock;

use crate::store::{Message, ReactionCount};

/// Embedded service CSS layered after Odyssey's canonical HOLDFAST design system.
pub const SERVICE_CSS: &str = include_str!("../../static/service.css");
static APP_CSS: OnceLock<String> = OnceLock::new();

/// Embedded dashboard client script, inlined into the dashboard `<script>`.
pub const APP_JS: &str = include_str!("../../static/app.js");

/// Cross-subdomain gateway logout (Murmur lives at chat.w33d.xyz; the IdP is at id.w33d.xyz).
pub const LOGOUT_URL: &str = "https://sso.w33d.xyz/_gw/auth/logout";

/// The HOLDFAST shield glyph (small, for the app-bar brand lockup).
pub const SHIELD_SVG: &str = r##"<svg viewBox="0 0 48 48" fill="none" xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="hf-shield-sm" x1="8" y1="4" x2="40" y2="44" gradientUnits="userSpaceOnUse"><stop stop-color="#818CF8"/><stop offset="1" stop-color="#4F46E5"/></linearGradient></defs><path d="M24 4 8 9.5V22c0 11 7 17.4 16 21.5C33 39.4 40 33 40 22V9.5L24 4Z" fill="url(#hf-shield-sm)"/><rect x="20" y="19" width="8" height="13" rx="1" fill="#fff" fill-opacity="0.92"/><path d="M20 19v-2.5a4 4 0 0 1 8 0V19" stroke="#fff" stroke-width="2" stroke-opacity="0.92" fill="none"/></svg>"##;

/// Full CSS payload: canonical Odyssey first, Murmur's service layer second.
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
    crate::text::esc(s)
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

/// The three-state theme switcher (light / dark / system) rendered into the app-bar, just before
/// the user menu — the same display-only control loom/cellar thread through. Pure `<a href>` links
/// to the gateway-owned `/_gw/theme` route (no JS, no new service route); the active option mirrors
/// the resolved theme. `__Secure-theme` is display-only and lives OUTSIDE the gateway HMACs, so this
/// control can only repaint — never forge identity/CSRF. The `.themeswitch` styles live in the
/// vendored Odyssey `APP_CSS`, not the service layer.
fn themeswitch(theme: &str) -> String {
    let (l, lc) = if theme == "light" { (" is-active", r#" aria-current="true""#) } else { ("", "") };
    let (d, dc) = if theme == "dark" { (" is-active", r#" aria-current="true""#) } else { ("", "") };
    let (a, ac) = if theme == "auto" { (" is-active", r#" aria-current="true""#) } else { ("", "") };
    format!(
        r#"<div class="themeswitch" role="group" aria-label="Theme">
  <a class="themeswitch__opt{l}" href="/_gw/theme?to=light" title="Light" aria-label="Light"{lc}><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/></svg></a>
  <a class="themeswitch__opt{d}" href="/_gw/theme?to=dark" title="Dark" aria-label="Dark"{dc}><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 3a6.5 6.5 0 0 0 9 9 9 9 0 1 1-9-9Z"/></svg></a>
  <a class="themeswitch__opt{a}" href="/_gw/theme?to=auto" title="System" aria-label="System"{ac}><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="2" y="3" width="20" height="14" rx="2"/><path d="M8 21h8M12 17v4"/></svg></a>
</div>"#,
        l = l, lc = lc, d = d, dc = dc, a = a, ac = ac,
    )
}

/// Render the shared v2 app-bar: an app-tile (message-circle in Chat's green) + the Murmur/host
/// lockup on the left; the surface's nav (Chat, plus Admin on moderator pages); then an "All apps"
/// button back to the apex portal, the display-only theme switcher, and the signed-in avatar menu on
/// the right. Same routes and the same gateway logout as before — only the chrome is modernized.
pub fn topbar(page_title: &str, email: &str, theme: &str) -> String {
    let tile = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21 11.5a8.38 8.38 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.38 8.38 0 0 1-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.38 8.38 0 0 1 3.8-.9h.5a8.48 8.48 0 0 1 8 8v.5z"/></svg>"#;
    let chat_ic = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21 11.5a8.38 8.38 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.38 8.38 0 0 1-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.38 8.38 0 0 1 3.8-.9h.5a8.48 8.48 0 0 1 8 8v.5z"/></svg>"#;
    let admin_ic = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/></svg>"#;
    // Regular chat pages surface only the Chat tab; moderator pages add an active Admin tab (both
    // routes already exist — no new destinations invented).
    let nav = if page_title == "Admin" {
        format!(
            r#"<a class="appnav" href="/">{chat}Chat</a><a class="appnav is-active" href="/admin">{admin}Admin</a>"#,
            chat = chat_ic,
            admin = admin_ic,
        )
    } else {
        format!(r#"<a class="appnav is-active" href="/">{chat}Chat</a>"#, chat = chat_ic)
    };
    format!(
        r#"<header class="appbar">
  <a class="appbar__brand" href="/" aria-label="HOLDFAST Murmur">
    <span class="app-tile" style="--app:#16a34a;--app-soft:#e6f6ec" aria-hidden="true">{tile}</span>
    <span class="appbar__name"><b>Murmur</b><span>chat.w33d.xyz</span></span>
  </a>
  <nav class="appbar__nav" aria-label="Murmur">{nav}</nav>
  <div class="appbar__spacer"></div>
  <div class="appbar__right">
    <a class="iconbtn" href="https://w33d.xyz" title="All apps" aria-label="All apps">{grid}</a>
    {themeswitch}
    {usermenu}
  </div>
</header>"#,
        tile = tile,
        nav = nav,
        grid = IC_GRID,
        themeswitch = themeswitch(theme),
        usermenu = usermenu(email),
    )
}

/// Format epoch seconds as a compact UTC time `HH:MM` for the timeline. std `time` only.
pub fn fmt_time(secs: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(secs) {
        Ok(dt) => format!("{:02}:{:02}", dt.hour(), dt.minute()),
        Err(_) => secs.to_string(),
    }
}

/// Build the WS/SSE-style JSON frame for a new message (the wire shape the dashboard JS reads).
/// The `body` rides raw (JSON-encoded); the client escapes it before insertion into the DOM.
pub fn message_frame(m: &Message) -> String {
    json!({
        "type": "message",
        "room_id": m.room_id,
        "id": m.id,
        "sender_sub": m.sender_sub,
        "sender_email": m.sender_email,
        "body": m.body,
        "created_at": m.created_at,
        "edited_at": m.edited_at,
        "deleted": m.deleted,
        "reply_to_id": m.reply_to_id,
    })
    .to_string()
}

/// Build the JSON reaction frame fanned out when a message's reaction tallies change. Carries the
/// full per-emoji counts for the message so a live client can replace its chip row wholesale.
pub fn reaction_frame(room_id: &str, message_id: &str, reactions: &[ReactionCount]) -> String {
    json!({
        "type": "reaction",
        "room_id": room_id,
        "message_id": message_id,
        "reactions": reactions,
    })
    .to_string()
}

/// Build the JSON presence frame (`online` / `offline`) for a user in a room.
pub fn presence_frame(room_id: &str, email: &str, online: bool) -> String {
    json!({
        "type": "presence",
        "room_id": room_id,
        "user_email": email,
        "status": if online { "online" } else { "offline" },
    })
    .to_string()
}
