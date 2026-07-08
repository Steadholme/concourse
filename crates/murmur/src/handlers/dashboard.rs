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
use crate::handlers::{app_css, esc, fmt_time, topbar, APP_JS};
use crate::store::{Message, ReactionCount, UserRoom};
use crate::text::{render_body, render_preview};
use crate::{ensure_lobby, AppState};

/// Longest quoted-parent snippet shown above a threaded reply, in characters (the full parent is
/// one click away in the timeline).
const QUOTE_SNIPPET_CHARS: usize = 120;

const DASHBOARD_HTML: &str = include_str!("../../templates/dashboard.html");

/// `GET /` — the chat dashboard for the signed-in user.
pub async fn index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let (sub, email) = match auth::require_user(&headers) {
        Ok(v) => v,
        Err(_) => return unauthorized_page(),
    };

    // First-visit: provision + join the lobby so the dashboard is never empty.
    ensure_lobby(&state, &sub, &email).await;

    // Moderation affordances (edit topic, pin) are gated to admins/mods — the same gate the
    // endpoints enforce; the flag only controls whether the UI OFFERS the controls.
    let is_mod = auth::is_admin(&headers);

    let rooms = state.store.list_user_rooms(&sub).await;
    let selected = rooms
        .first()
        .map(|r| r.room.id.clone())
        .unwrap_or_else(|| LOBBY_ID.to_string());
    let selected_room = rooms.iter().find(|r| r.room.id == selected);
    let selected_name = selected_room
        .map(|r| r.room.name.clone())
        .unwrap_or_else(|| selected.clone());
    let selected_topic = selected_room.map(|r| r.room.topic.clone()).unwrap_or_default();

    let messages = state
        .store
        .list_messages(&selected, None, MESSAGE_PAGE_LIMIT)
        .await;
    let timeline = render_timeline(&state, &sub, &messages, is_mod).await;
    let pinned = state.store.list_pinned(&selected).await;

    let (csrf, set_cookie) = auth::ensure_csrf(&headers);

    let theme = odyssey::resolve_theme(
        headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok()),
    );

    let page = DASHBOARD_HTML
        .replace("{{CSS}}", app_css())
        .replace("{{THEME}}", odyssey::html_theme_attr(theme))
        .replace("{{COLOR_SCHEME}}", odyssey::color_scheme_meta(theme))
        .replace("{{TOPBAR}}", &topbar("Chat", &email, theme))
        .replace("{{ROOMS}}", &render_room_list(&rooms, &selected))
        .replace("{{ROOM_TITLE}}", &esc(&selected_name))
        .replace("{{TOPIC}}", &esc(&selected_topic))
        .replace("{{PINNED}}", &render_pinned(&pinned, is_mod))
        .replace("{{MESSAGES}}", &timeline)
        .replace("{{CSRF}}", &esc(&csrf))
        .replace("{{ME}}", &esc(&email))
        .replace("{{SELECTED}}", &esc(&selected))
        .replace("{{IS_MOD}}", if is_mod { "true" } else { "false" })
        // {{JS}} is applied last so a stray `{{...}}` token inside the script can never be
        // mistaken for a template slot.
        .replace("{{JS}}", APP_JS);

    html_with_cookie(page, set_cookie)
}

/// Render the sidebar room list, marking the selected room active. Each room carries an unread
/// badge (count of messages past the read cursor not authored by the caller) and a mention dot when
/// the caller has an unread @mention there. Both are `data-*`-backed so the live client can update
/// them in place. The selected room renders with a cleared badge (opening a room marks it read).
fn render_room_list(rooms: &[UserRoom], selected: &str) -> String {
    if rooms.is_empty() {
        return r#"<li class="room-list__empty">No rooms yet</li>"#.to_string();
    }
    let mut out = String::new();
    for r in rooms {
        let is_selected = r.room.id == selected;
        let active = if is_selected { " is-active" } else { "" };
        // The open room is read: never show a badge on it even if the stored cursor lags.
        let unread = if is_selected { 0 } else { r.unread.max(0) };
        let mentioned = !is_selected && r.mentioned;
        out.push_str(&format!(
            r#"<li class="room{active}" data-room-id="{id}" data-room-name="{name}" data-room-topic="{topic}">
  <button class="room__btn" type="button">{name}</button>
  {badge}
</li>"#,
            active = active,
            id = esc(&r.room.id),
            name = esc(&r.room.name),
            topic = esc(&r.room.topic),
            badge = room_badge_html(unread, mentioned),
        ));
    }
    out
}

/// The unread badge for a room-list row: a mention dot (when the caller has an unread @mention) and
/// an unread count pill. Hidden (but present in the DOM) when there is nothing unread, so the live
/// client can reveal/update it without rebuilding the row.
fn room_badge_html(unread: i64, mentioned: bool) -> String {
    let dot = if mentioned {
        r#"<span class="room__mention" title="You were mentioned" aria-label="You were mentioned"></span>"#
    } else {
        ""
    };
    let hidden = if unread > 0 { "" } else { " hidden" };
    format!(
        r#"<span class="room__badge">{dot}<span class="room__unread"{hidden}>{count}</span></span>"#,
        dot = dot,
        hidden = hidden,
        count = unread,
    )
}

/// Render the timeline (oldest-first). The store returns newest-first, so we reverse for reading.
/// Each row is enriched with its threaded-reply context (quoted parent + reply count) and its
/// reaction tallies (with the caller's own reactions highlighted) — all fetched from the store.
async fn render_timeline(state: &AppState, sub: &str, messages: &[Message], is_mod: bool) -> String {
    if messages.is_empty() {
        return r#"<div class="timeline__empty">No messages yet — say hello.</div>"#.to_string();
    }
    let mut out = String::new();
    for m in messages.iter().rev() {
        // Threaded-reply parent (best-effort: a purged parent simply renders no quote).
        let parent = match &m.reply_to_id {
            Some(pid) => state.store.get_message(pid).await,
            None => None,
        };
        let reply_count = state.store.count_replies(&m.id).await;
        let reactions = state.store.list_reactions(&m.id).await;
        let mine = state.store.list_user_reactions(&m.id, sub).await;
        out.push_str(&render_message_row(
            m,
            parent.as_ref(),
            reply_count,
            &reactions,
            &mine,
            is_mod,
        ));
    }
    out
}

/// Render a message body to safe HTML: a soft-deleted / redacted row shows a fixed tombstone
/// (`[deleted]` or the stored moderator text, escaped); a live body goes through the
/// escape-then-autolink renderer.
fn message_body_html(m: &Message) -> String {
    if m.deleted {
        let label = if m.body.is_empty() {
            "[deleted]".to_string()
        } else {
            esc(&m.body)
        };
        format!(r#"<span class="msg__deleted">{label}</span>"#)
    } else {
        render_body(&m.body)
    }
}

/// The `(edited)` marker for a live, edited message (empty otherwise).
fn message_edited_html(m: &Message) -> &'static str {
    if m.edited_at > 0 && !m.deleted {
        r#"<span class="msg__edited">(edited)</span>"#
    } else {
        ""
    }
}

/// One bare message row (no reply/reaction chrome). `sender_email` / time are escaped; the body is
/// sanitized. Retained for callers that render a plain message.
pub fn render_message(m: &Message) -> String {
    format!(
        r#"<div class="msg" data-id="{id}">
  <div class="msg__head"><span class="msg__author">{author}</span><span class="msg__time">{time}</span>{edited}</div>
  <div class="msg__body">{body}</div>
</div>"#,
        id = esc(&m.id),
        author = esc(&m.sender_email),
        time = esc(&fmt_time(m.created_at)),
        edited = message_edited_html(m),
        body = message_body_html(m),
    )
}

/// One enriched timeline row: an optional quoted parent (threaded reply), the message head/body, a
/// reply-count marker, and the reaction chip row. Every interpolated field is escaped/sanitized.
fn render_message_row(
    m: &Message,
    parent: Option<&Message>,
    reply_count: i64,
    reactions: &[ReactionCount],
    mine: &[String],
    is_mod: bool,
) -> String {
    format!(
        r#"<div class="msg" data-id="{id}" data-author="{author}">
  {quote}<div class="msg__head"><span class="msg__author">{author}</span><span class="msg__time">{time}</span>{edited}{replies}<span class="msg__tools"><button type="button" class="msg__tool" data-act="react">React</button><button type="button" class="msg__tool" data-act="reply">Reply</button>{pin_tool}</span></div>
  <div class="msg__body">{body}</div>
  {reactions}
</div>"#,
        id = esc(&m.id),
        author = esc(&m.sender_email),
        time = esc(&fmt_time(m.created_at)),
        edited = message_edited_html(m),
        replies = reply_count_html(reply_count),
        quote = quote_html(parent),
        body = message_body_html(m),
        reactions = reactions_html(reactions, mine),
        pin_tool = if is_mod {
            r#"<button type="button" class="msg__tool" data-act="pin">Pin</button>"#
        } else {
            ""
        },
    )
}

/// Render the pinned-message panel body: one row per pinned message (author + escaped snippet), an
/// "Unpin" control for mods, and a jump link back to the message. Empty state when none are pinned.
fn render_pinned(messages: &[Message], is_mod: bool) -> String {
    if messages.is_empty() {
        return r#"<div class="pinned-panel__empty">No pinned messages.</div>"#.to_string();
    }
    let mut out = String::new();
    for m in messages {
        out.push_str(&pinned_item_html(m, is_mod));
    }
    out
}

/// One pinned-panel row. The body is shown as an escaped one-line snippet (never live markup); a
/// deleted/redacted pinned message shows its tombstone.
fn pinned_item_html(m: &Message, is_mod: bool) -> String {
    let snippet = if m.deleted {
        if m.body.is_empty() {
            "[deleted]".to_string()
        } else {
            m.body.clone()
        }
    } else {
        m.body.clone()
    };
    let mut oneline: String = snippet.chars().take(140).collect();
    if snippet.chars().count() > 140 {
        oneline.push('…');
    }
    let oneline = oneline.replace(['\n', '\r'], " ");
    let unpin = if is_mod {
        format!(
            r#"<button type="button" class="pinned-item__unpin" data-unpin="{id}">Unpin</button>"#,
            id = esc(&m.id),
        )
    } else {
        String::new()
    };
    format!(
        r#"<div class="pinned-item" data-jump-id="{id}">
  <button type="button" class="pinned-item__jump"><span class="pinned-item__author">{author}</span><span class="pinned-item__body">{body}</span></button>
  {unpin}
</div>"#,
        id = esc(&m.id),
        author = esc(&m.sender_email),
        body = esc(&oneline),
        unpin = unpin,
    )
}

/// The quoted-parent block shown above a threaded reply. Empty for a top-level post (or when the
/// parent was purged). The parent author + a short snippet are escaped as plain text.
fn quote_html(parent: Option<&Message>) -> String {
    let Some(p) = parent else {
        return String::new();
    };
    let snippet = if p.deleted {
        if p.body.is_empty() {
            "[deleted]".to_string()
        } else {
            p.body.clone()
        }
    } else {
        p.body.clone()
    };
    // A one-line, length-capped plain-text snippet (escaped) — no markup, no autolinking.
    let mut oneline: String = snippet.chars().take(QUOTE_SNIPPET_CHARS).collect();
    if snippet.chars().count() > QUOTE_SNIPPET_CHARS {
        oneline.push('…');
    }
    format!(
        r#"<div class="msg__quote" data-parent-id="{id}"><span class="msg__quote-author">{author}</span><span class="msg__quote-body">{body}</span></div>
  "#,
        id = esc(&p.id),
        author = esc(&p.sender_email),
        body = render_preview(&oneline),
    )
}

/// The reply-count marker on a parent message (empty when it has no replies).
fn reply_count_html(reply_count: i64) -> String {
    if reply_count <= 0 {
        return String::new();
    }
    let label = if reply_count == 1 { "reply" } else { "replies" };
    format!(
        r#"<span class="msg__replies">{count} {label}</span>"#,
        count = reply_count,
        label = label,
    )
}

/// The reaction chip row for a message. Each chip carries its emoji + distinct-user count; a chip
/// the caller reacted with gets `is-mine`. Empty when the message has no reactions.
fn reactions_html(reactions: &[ReactionCount], mine: &[String]) -> String {
    if reactions.is_empty() {
        return String::new();
    }
    let mut chips = String::new();
    for r in reactions {
        let is_mine = if mine.iter().any(|e| e == &r.emoji) {
            " is-mine"
        } else {
            ""
        };
        chips.push_str(&format!(
            r#"<button type="button" class="reaction{mine}" data-emoji="{emoji}"><span class="reaction__emoji">{emoji}</span><span class="reaction__count">{count}</span></button>"#,
            mine = is_mine,
            emoji = esc(&r.emoji),
            count = r.count,
        ));
    }
    format!(r#"<div class="msg__reactions">{chips}</div>"#, chips = chips)
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
        css = app_css(),
        topbar = topbar("Chat", "", "light"),
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
