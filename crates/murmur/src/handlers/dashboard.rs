//! `GET /` — the server-rendered SSO chat dashboard.
//!
//! Renders the room list, the selected room's recent timeline (bodies sanitized + autolinked
//! server-side via [`crate::text::render_body`]), and a composer. Live updates are layered on by
//! the embedded client script, which opens `/ws` and calls the JSON API. A double-submit CSRF
//! token is minted into the page (cookie + JS-readable value) and echoed by every mutating
//! `fetch`.

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};

use crate::auth;
use crate::config::{LOBBY_ID, MESSAGE_PAGE_LIMIT};
use crate::handlers::{esc, fmt_time, topbar, APP_CSS, APP_JS};
use crate::store::{Message, UserRoom};
use crate::text::render_body;
use crate::{ensure_lobby, AppState};

const DASHBOARD_HTML: &str = include_str!("../../templates/dashboard.html");

/// `GET /` — the chat dashboard for the signed-in user.
pub async fn index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let (sub, email) = match auth::require_user(&headers) {
        Ok(v) => v,
        Err(_) => return unauthorized_page(),
    };

    // First-visit: provision + join the lobby so the dashboard is never empty.
    ensure_lobby(&state, &sub, &email).await;

    let rooms = state.store.list_user_rooms(&sub).await;
    let selected = rooms
        .first()
        .map(|r| r.room.id.clone())
        .unwrap_or_else(|| LOBBY_ID.to_string());
    let selected_name = rooms
        .iter()
        .find(|r| r.room.id == selected)
        .map(|r| r.room.name.clone())
        .unwrap_or_else(|| selected.clone());

    let messages = state
        .store
        .list_messages(&selected, None, MESSAGE_PAGE_LIMIT)
        .await;

    let (csrf, set_cookie) = auth::ensure_csrf(&headers);

    let page = DASHBOARD_HTML
        .replace("{{CSS}}", APP_CSS)
        .replace("{{TOPBAR}}", &topbar("Chat", &email))
        .replace("{{ROOMS}}", &render_room_list(&rooms, &selected))
        .replace("{{ROOM_TITLE}}", &esc(&selected_name))
        .replace("{{MESSAGES}}", &render_messages(&messages))
        .replace("{{CSRF}}", &esc(&csrf))
        .replace("{{ME}}", &esc(&email))
        .replace("{{SELECTED}}", &esc(&selected))
        // {{JS}} is applied last so a stray `{{...}}` token inside the script can never be
        // mistaken for a template slot.
        .replace("{{JS}}", APP_JS);

    html_with_cookie(page, set_cookie)
}

/// Render the sidebar room list, marking the selected room active.
fn render_room_list(rooms: &[UserRoom], selected: &str) -> String {
    if rooms.is_empty() {
        return r#"<li class="room-list__empty">No rooms yet</li>"#.to_string();
    }
    let mut out = String::new();
    for r in rooms {
        let active = if r.room.id == selected { " is-active" } else { "" };
        out.push_str(&format!(
            r#"<li class="room{active}" data-room-id="{id}" data-room-name="{name}">
  <button class="room__btn" type="button">{name}</button>
</li>"#,
            active = active,
            id = esc(&r.room.id),
            name = esc(&r.room.name),
        ));
    }
    out
}

/// Render the timeline (oldest-first). The store returns newest-first, so we reverse for reading.
fn render_messages(messages: &[Message]) -> String {
    if messages.is_empty() {
        return r#"<div class="timeline__empty">No messages yet — say hello.</div>"#.to_string();
    }
    let mut out = String::new();
    for m in messages.iter().rev() {
        out.push_str(&render_message(m));
    }
    out
}

/// One message row. `sender_email` / time are escaped; the body goes through the
/// escape-then-autolink renderer. A soft-deleted message renders a fixed `[deleted]` tombstone
/// (its stored body is already cleared); an edited message carries an `(edited)` marker.
pub fn render_message(m: &Message) -> String {
    let body = if m.deleted {
        r#"<span class="msg__deleted">[deleted]</span>"#.to_string()
    } else {
        render_body(&m.body)
    };
    let edited = if m.edited_at > 0 && !m.deleted {
        r#"<span class="msg__edited">(edited)</span>"#
    } else {
        ""
    };
    format!(
        r#"<div class="msg" data-id="{id}">
  <div class="msg__head"><span class="msg__author">{author}</span><span class="msg__time">{time}</span>{edited}</div>
  <div class="msg__body">{body}</div>
</div>"#,
        id = esc(&m.id),
        author = esc(&m.sender_email),
        time = esc(&fmt_time(m.created_at)),
        edited = edited,
        body = body,
    )
}

/// A minimal HTML "session required" page (defense in depth — the gateway normally guarantees an
/// SSO identity before `/` is reached).
fn unauthorized_page() -> Response {
    let page = format!(
        r#"<!DOCTYPE html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sign in · Murmur</title><style>{css}</style></head>
<body class="page-chat">{topbar}<main class="chat chat--center">
<div class="signin-card"><h1>Session required</h1>
<p>Sign in through the HOLDFAST gateway to use Murmur.</p>
<a class="btn btn-primary" href="/">Reload</a></div>
</main></body></html>"#,
        css = APP_CSS,
        topbar = topbar("Chat", ""),
    );
    (StatusCode::UNAUTHORIZED, Html(page)).into_response()
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
