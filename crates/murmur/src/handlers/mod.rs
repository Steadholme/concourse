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
pub mod ws;

use serde_json::json;

use crate::store::{Message, ReactionCount};

/// Embedded design system, inlined into the dashboard `<style>`.
pub const APP_CSS: &str = include_str!("../../static/app.css");

/// Embedded dashboard client script, inlined into the dashboard `<script>`.
pub const APP_JS: &str = include_str!("../../static/app.js");

/// Cross-subdomain gateway logout (Murmur lives at chat.w33d.xyz; the IdP is at id.w33d.xyz).
pub const LOGOUT_URL: &str = "https://sso.w33d.xyz/_gw/auth/logout";

/// The HOLDFAST shield glyph (small, for the app-bar brand lockup).
pub const SHIELD_SVG: &str = r##"<svg viewBox="0 0 48 48" fill="none" xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="hf-shield-sm" x1="8" y1="4" x2="40" y2="44" gradientUnits="userSpaceOnUse"><stop stop-color="#818CF8"/><stop offset="1" stop-color="#4F46E5"/></linearGradient></defs><path d="M24 4 8 9.5V22c0 11 7 17.4 16 21.5C33 39.4 40 33 40 22V9.5L24 4Z" fill="url(#hf-shield-sm)"/><rect x="20" y="19" width="8" height="13" rx="1" fill="#fff" fill-opacity="0.92"/><path d="M20 19v-2.5a4 4 0 0 1 8 0V19" stroke="#fff" stroke-width="2" stroke-opacity="0.92" fill="none"/></svg>"##;

/// Minimal HTML escaping for text/attribute interpolation (defense-in-depth on every field).
pub fn esc(s: &str) -> String {
    crate::text::esc(s)
}

/// Render the shared app-bar: shield + HOLDFAST wordmark + page title on the left; an
/// "All apps" pill back to the apex portal, the signed-in user chip (avatar initial + email),
/// and a Logout link to the gateway on the right. Same chrome as every HOLDFAST service.
pub fn topbar(page_title: &str, email: &str) -> String {
    // The user chip: an avatar initial (first letter of the email) + the address. Omitted when
    // no identity is known (e.g. public pages), keeping the rest of the chrome intact.
    let chip = if email.is_empty() {
        String::new()
    } else {
        let initial = email
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "H".to_string());
        format!(
            "<span class=\"userchip\"><span class=\"userchip__avatar\" aria-hidden=\"true\">{}</span><span class=\"user-email\">{}</span></span>",
            esc(&initial),
            esc(email),
        )
    };
    format!(
        r#"<header class="topbar">
  <div class="topbar__inner">
    <a class="brand" href="/" aria-label="HOLDFAST Murmur">
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
