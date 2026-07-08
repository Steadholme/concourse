//! `/admin` — the server-rendered moderator panel (admin-gated).
//!
//! The whole subtree is gated by [`auth::require_admin`]: only members of `admins` /
//! `infra-admins` (per the HMAC-verified `X-Auth-Groups`) may reach it; an ordinary signed-in
//! user gets `403`. `admin` OVERRIDES ownership — none of these actions consult `created_by`.
//!
//! Scope:
//! - room lifecycle: list ALL rooms, archive a room, hard-delete a room (+ its members/messages);
//! - membership control: remove or ban a member from a room;
//! - message redaction: redact ANY message to the fixed `[removed by moderator]` tombstone.
//!
//! Every state-changing action is a real `<form>` POST, double-submit CSRF protected via a hidden
//! `csrf` field checked against the `__Host-csrf` cookie ([`auth::verify_csrf_field`]). Each
//! mutation emits an [`AuditEvent`] (`notice` for the destructive ones). All interpolated user
//! input is HTML-escaped.

use axum::extract::{Form, Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;

use crate::audit::AuditEvent;
use crate::auth;
use crate::config::MESSAGE_PAGE_LIMIT;
use crate::error::AppError;
use crate::handlers::{app_css, esc, fmt_time, topbar};
use crate::store::{Member, Message, Room};
use crate::AppState;

const ADMIN_ROOMS_HTML: &str = include_str!("../../templates/admin_rooms.html");
const ADMIN_ROOM_HTML: &str = include_str!("../../templates/admin_room.html");

/// Hidden CSRF field carried by every admin form POST (double-submit vs. the cookie).
#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    #[serde(default)]
    pub csrf: String,
}

// ---------------------------------------------------------------------------
// GET pages
// ---------------------------------------------------------------------------

/// `GET /admin` — list ALL rooms with archive/delete controls.
pub async fn index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !auth::is_admin(&headers) {
        return forbidden_page();
    }
    let rooms = state.store.list_all_rooms().await;
    let (csrf, set_cookie) = auth::ensure_csrf(&headers);
    let theme = odyssey::resolve_theme(
        headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok()),
    );

    let page = ADMIN_ROOMS_HTML
        .replace("{{CSS}}", app_css())
        .replace("{{THEME}}", odyssey::html_theme_attr(theme))
        .replace("{{COLOR_SCHEME}}", odyssey::color_scheme_meta(theme))
        .replace("{{TOPBAR}}", &topbar("Admin", &auth::display_email(&headers), theme))
        .replace("{{ROOMS}}", &render_room_rows(&rooms, &csrf));

    html_with_cookie(page, set_cookie)
}

/// `GET /admin/rooms/{id}` — one room's members + messages, with per-row controls.
pub async fn room_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !auth::is_admin(&headers) {
        return forbidden_page();
    }
    let Some(room) = state.store.get_room(&id).await else {
        return (StatusCode::NOT_FOUND, Html(not_found_page("no such room"))).into_response();
    };
    let members = state.store.list_room_members(&id).await;
    let messages = state
        .store
        .list_messages(&id, None, MESSAGE_PAGE_LIMIT)
        .await;
    let (csrf, set_cookie) = auth::ensure_csrf(&headers);
    let theme = odyssey::resolve_theme(
        headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok()),
    );

    let page = ADMIN_ROOM_HTML
        .replace("{{CSS}}", app_css())
        .replace("{{THEME}}", odyssey::html_theme_attr(theme))
        .replace("{{COLOR_SCHEME}}", odyssey::color_scheme_meta(theme))
        .replace("{{TOPBAR}}", &topbar("Admin", &auth::display_email(&headers), theme))
        .replace("{{ROOM_TITLE}}", &esc(&room.name))
        .replace("{{ROOM_ID}}", &esc(&room.id))
        .replace("{{MEMBERS}}", &render_member_rows(&id, &members, &csrf))
        .replace("{{MESSAGES}}", &render_message_rows(&messages, &csrf));

    html_with_cookie(page, set_cookie)
}

// ---------------------------------------------------------------------------
// POST actions (admin-gated + CSRF-checked)
// ---------------------------------------------------------------------------

/// `POST /admin/rooms/{id}/archive` — soft-archive a room (drops out of users' room lists).
pub async fn archive_room(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    guard(&headers, &form)?;
    state.store.set_room_archived(&id, true).await?;
    state.audit.emit(AuditEvent::notice(
        "chat.admin.room.archive",
        &actor(&headers),
        &id,
        "archived",
    ));
    tracing::info!(room_id = %id, "admin archived room");
    Ok(Redirect::to("/admin").into_response())
}

/// `POST /admin/rooms/{id}/delete` — hard-delete a room and all its members + messages.
pub async fn delete_room(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    guard(&headers, &form)?;
    state.store.delete_room(&id).await?;
    state.audit.emit(AuditEvent::notice(
        "chat.admin.room.delete",
        &actor(&headers),
        &id,
        "hard-deleted",
    ));
    tracing::info!(room_id = %id, "admin deleted room");
    Ok(Redirect::to("/admin").into_response())
}

/// `POST /admin/rooms/{id}/members/{user_sub}/remove` — kick a member from a room.
pub async fn remove_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, user_sub)): Path<(String, String)>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    guard(&headers, &form)?;
    state.store.remove_member(&id, &user_sub).await?;
    state.audit.emit(AuditEvent::notice(
        "chat.admin.member.remove",
        &actor(&headers),
        &id,
        &format!("sub={user_sub}"),
    ));
    tracing::info!(room_id = %id, %user_sub, "admin removed member");
    Ok(Redirect::to(&format!("/admin/rooms/{id}")).into_response())
}

/// `POST /admin/rooms/{id}/members/{user_sub}/ban` — ban a member (kept row, can no longer act).
pub async fn ban_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, user_sub)): Path<(String, String)>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    guard(&headers, &form)?;
    state.store.ban_member(&id, &user_sub).await?;
    state.audit.emit(AuditEvent::notice(
        "chat.admin.member.ban",
        &actor(&headers),
        &id,
        &format!("sub={user_sub}"),
    ));
    tracing::info!(room_id = %id, %user_sub, "admin banned member");
    Ok(Redirect::to(&format!("/admin/rooms/{id}")).into_response())
}

/// `POST /admin/messages/{msg_id}/redact` — redact ANY message to `[removed by moderator]`.
pub async fn redact_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(msg_id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    guard(&headers, &form)?;
    // Look up the room to redirect back to the detail page (also 404s a stray id gracefully).
    let room_id = state.store.get_message(&msg_id).await.map(|m| m.room_id);
    state.store.redact_message(&msg_id).await?;
    state.audit.emit(AuditEvent::notice(
        "chat.admin.message.redact",
        &actor(&headers),
        &msg_id,
        "redacted",
    ));
    tracing::info!(%msg_id, "admin redacted message");
    let dest = match room_id {
        Some(r) => format!("/admin/rooms/{r}"),
        None => "/admin".to_string(),
    };
    Ok(Redirect::to(&dest).into_response())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Shared guard for every mutating admin POST: admin group membership, then double-submit CSRF.
fn guard(headers: &HeaderMap, form: &CsrfForm) -> Result<(), AppError> {
    auth::require_admin(headers)?;
    auth::verify_csrf_field(headers, &form.csrf)?;
    Ok(())
}

/// The admin actor for audit: prefer the email, fall back to the subject, then a neutral label.
fn actor(headers: &HeaderMap) -> String {
    auth::user_email(headers)
        .or_else(|| auth::user_sub(headers))
        .unwrap_or_else(|| "admin".to_string())
}

/// One row per room in the `/admin` table: name (linking to detail), kind, creator, status, and
/// archive/delete forms.
fn render_room_rows(rooms: &[Room], csrf: &str) -> String {
    if rooms.is_empty() {
        return r#"<tr><td colspan="5" class="empty">No rooms.</td></tr>"#.to_string();
    }
    let mut out = String::new();
    for r in rooms {
        let status = if r.archived {
            r#"<span class="pill pill-warn">archived</span>"#
        } else {
            r#"<span class="pill pill-ok">active</span>"#
        };
        out.push_str(&format!(
            r#"<tr>
  <td><a class="btn-link" href="/admin/rooms/{id}">{name}</a><div class="mono admin__sub">{id}</div></td>
  <td>{kind}</td>
  <td>{creator}</td>
  <td>{status}</td>
  <td class="admin__col-actions"><div class="admin__actions">
    {archive}
    <form method="post" action="/admin/rooms/{id}/delete" onsubmit="return confirm('Delete this room and all its messages?')">{csrf_input}<button class="btn btn-danger btn-sm" type="submit">Delete</button></form>
  </div></td>
</tr>"#,
            id = esc(&r.id),
            name = esc(&r.name),
            kind = esc(&r.kind),
            creator = esc(&r.created_by),
            status = status,
            archive = archive_form(&r.id, r.archived, csrf),
            csrf_input = csrf_input(csrf),
        ));
    }
    out
}

/// The archive toggle form: "Archive" when active, "Unarchive" when already archived.
fn archive_form(room_id: &str, archived: bool, csrf: &str) -> String {
    // A room is un-archived by archiving to `false`; the panel exposes the inverse action.
    if archived {
        // No unarchive route in scope; show a disabled marker instead of a live control.
        String::new()
    } else {
        format!(
            r#"<form method="post" action="/admin/rooms/{id}/archive">{csrf}<button class="btn btn-secondary btn-sm" type="submit">Archive</button></form>"#,
            id = esc(room_id),
            csrf = csrf_input(csrf),
        )
    }
}

/// One row per member: email, subject, status, and remove/ban forms.
fn render_member_rows(room_id: &str, members: &[Member], csrf: &str) -> String {
    if members.is_empty() {
        return r#"<tr><td colspan="4" class="empty">No members.</td></tr>"#.to_string();
    }
    let mut out = String::new();
    for m in members {
        let status = if m.banned {
            r#"<span class="pill pill-down">banned</span>"#
        } else {
            r#"<span class="pill pill-ok">member</span>"#
        };
        let ban = if m.banned {
            String::new()
        } else {
            format!(
                r#"<form method="post" action="/admin/rooms/{room}/members/{sub}/ban">{csrf}<button class="btn btn-danger btn-sm" type="submit">Ban</button></form>"#,
                room = esc(room_id),
                sub = esc(&m.user_sub),
                csrf = csrf_input(csrf),
            )
        };
        out.push_str(&format!(
            r#"<tr>
  <td>{email}</td>
  <td class="mono">{sub}</td>
  <td>{status}</td>
  <td class="admin__col-actions"><div class="admin__actions">
    <form method="post" action="/admin/rooms/{room}/members/{sub}/remove">{csrf}<button class="btn btn-secondary btn-sm" type="submit">Remove</button></form>
    {ban}
  </div></td>
</tr>"#,
            email = esc(&m.user_email),
            sub = esc(&m.user_sub),
            status = status,
            room = esc(room_id),
            csrf = csrf_input(csrf),
            ban = ban,
        ));
    }
    out
}

/// One row per message (newest-first from the store): time, author, an escaped body preview, and
/// a Redact control (omitted once the message is already deleted/redacted).
fn render_message_rows(messages: &[Message], csrf: &str) -> String {
    if messages.is_empty() {
        return r#"<tr><td colspan="4" class="empty">No messages.</td></tr>"#.to_string();
    }
    let mut out = String::new();
    for m in messages {
        let (body_cell, action) = if m.deleted {
            let label = if m.body.is_empty() { "[deleted]" } else { m.body.as_str() };
            (
                format!(r#"<span class="msg__deleted">{}</span>"#, esc(label)),
                String::new(),
            )
        } else {
            (
                format!(r#"<span class="admin__body">{}</span>"#, esc(&preview(&m.body))),
                format!(
                    r#"<form method="post" action="/admin/messages/{id}/redact">{csrf}<button class="btn btn-danger btn-sm" type="submit">Redact</button></form>"#,
                    id = esc(&m.id),
                    csrf = csrf_input(csrf),
                ),
            )
        };
        out.push_str(&format!(
            r#"<tr>
  <td>{time}</td>
  <td>{author}</td>
  <td>{body}</td>
  <td class="admin__col-actions"><div class="admin__actions">{action}</div></td>
</tr>"#,
            time = esc(&fmt_time(m.created_at)),
            author = esc(&m.sender_email),
            body = body_cell,
            action = action,
        ));
    }
    out
}

/// A short single-line preview of a message body (escaping is applied by the caller).
fn preview(body: &str) -> String {
    const MAX: usize = 80;
    let one_line = body.replace('\n', " ");
    if one_line.chars().count() > MAX {
        let mut s: String = one_line.chars().take(MAX).collect();
        s.push('…');
        s
    } else {
        one_line
    }
}

/// The hidden double-submit CSRF input embedded in every admin form. The token is escaped even
/// though it is hex — defense in depth on every interpolated value.
fn csrf_input(csrf: &str) -> String {
    format!(r#"<input type="hidden" name="csrf" value="{}">"#, esc(csrf))
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

/// The `403` page shown to a signed-in non-admin who hits any `/admin` GET route.
fn forbidden_page() -> Response {
    let page = format!(
        r#"<!DOCTYPE html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Forbidden · Murmur</title><style>{css}</style></head>
<body class="page-chat">{topbar}<main class="chat chat--center">
<div class="signin-card"><h1>Admins only</h1>
<p>The Murmur admin panel is restricted to the <code>admins</code> / <code>infra-admins</code> groups.</p>
<a class="btn btn-primary" href="/">Back to chat</a></div>
</main></body></html>"#,
        css = app_css(),
        topbar = topbar("Admin", "", "light"),
    );
    (StatusCode::FORBIDDEN, Html(page)).into_response()
}

/// A minimal `404` body for a missing admin room.
fn not_found_page(msg: &str) -> String {
    format!(
        r#"<!DOCTYPE html><html lang="en"><head><meta charset="utf-8">
<title>Not found · Murmur</title><style>{css}</style></head>
<body class="page-chat">{topbar}<main class="chat chat--center">
<div class="signin-card"><h1>Not found</h1><p>{msg}</p>
<a class="btn btn-primary" href="/admin">Back to rooms</a></div>
</main></body></html>"#,
        css = app_css(),
        topbar = topbar("Admin", "", "light"),
        msg = esc(msg),
    )
}
