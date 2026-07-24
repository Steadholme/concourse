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

use axum::extract::{
    rejection::{FormRejection, PathRejection},
    Form, Path, State,
};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::audit::AuditEvent;
use crate::auth;
use crate::config::{LOBBY_ID, MESSAGE_PAGE_LIMIT};
use crate::error::AppError;
use crate::handlers::{
    app_css, esc, fmt_time, form_or_invalid, path_or_invalid, render_template, topbar,
};
use crate::store::{Member, Message, Room};
use crate::AppState;

const ADMIN_ROOMS_HTML: &str = include_str!("../../templates/admin_rooms.html");
const ADMIN_ROOM_HTML: &str = include_str!("../../templates/admin_room.html");
const DELETE_TOKEN_TTL_SECS: i64 = 5 * 60;

/// Hidden CSRF field carried by every admin form POST (double-submit vs. the cookie).
#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    #[serde(default)]
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct DeleteRoomForm {
    #[serde(default)]
    pub csrf: String,
    #[serde(default)]
    pub confirm: String,
    #[serde(default)]
    pub consequence_token: String,
}

// ---------------------------------------------------------------------------
// GET pages
// ---------------------------------------------------------------------------

/// `GET /admin` — list ALL rooms with archive/delete controls.
pub async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if auth::require_admin(&headers).is_err() {
        return Ok(forbidden_page());
    }
    let rooms = state.store.list_all_rooms().await?;
    let (csrf, set_cookie) = auth::ensure_csrf(&headers);
    let theme = odyssey::resolve_theme(
        headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok()),
    );

    let topbar_html = topbar("Admin", &auth::display_email(&headers), theme);
    let room_rows_html = render_room_rows(&rooms, &csrf);
    let page = match render_template(
        ADMIN_ROOMS_HTML,
        &[
            ("CSS", app_css()),
            ("THEME", odyssey::html_theme_attr(theme)),
            ("COLOR_SCHEME", odyssey::color_scheme_meta(theme)),
            ("TOPBAR", &topbar_html),
            ("ROOMS", &room_rows_html),
        ],
    ) {
        Ok(page) => page,
        Err(error) => {
            tracing::error!(error = %error, "admin rooms template rendering failed");
            return Err(AppError::Unavailable(
                "admin rendering unavailable".to_string(),
            ));
        }
    };

    Ok(html_with_cookie(page, set_cookie))
}

/// `GET /admin/rooms/{id}` — one room's members + messages, with per-row controls.
pub async fn room_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
) -> Result<Response, AppError> {
    if auth::require_admin(&headers).is_err() {
        return Ok(forbidden_page());
    }
    let id = path_or_invalid(path)?;
    let (csrf, set_cookie) = auth::ensure_csrf(&headers);
    let (actor_sub, _actor_email) = auth::require_user(&headers)?;
    let now = crate::now_secs();
    let (room, consequence_token) = if id == LOBBY_ID {
        let Some(room) = state.store.get_room(&id).await? else {
            return Ok(
                (StatusCode::NOT_FOUND, Html(not_found_page("no such room"))).into_response(),
            );
        };
        (room, String::new())
    } else {
        let token = auth::new_csrf_token();
        let room = state
            .store
            .issue_room_delete_token(
                &digest_binding(&token),
                &id,
                &actor_sub,
                &digest_binding(&csrf),
                now,
                now + DELETE_TOKEN_TTL_SECS,
            )
            .await?;
        (room, token)
    };
    let members = state.store.list_room_members(&id).await?;
    let messages = state
        .store
        .list_messages(&id, None, MESSAGE_PAGE_LIMIT)
        .await?;
    let theme = odyssey::resolve_theme(
        headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok()),
    );

    let topbar_html = topbar("Admin", &auth::display_email(&headers), theme);
    let room_title_html = esc(&room.name);
    let room_id_html = esc(&room.id);
    let consequence_html = render_delete_consequence(&room, &csrf, &consequence_token);
    let members_html = render_member_rows(&id, &members, &csrf);
    let messages_html = render_message_rows(&messages, &csrf);
    let page = match render_template(
        ADMIN_ROOM_HTML,
        &[
            ("CSS", app_css()),
            ("THEME", odyssey::html_theme_attr(theme)),
            ("COLOR_SCHEME", odyssey::color_scheme_meta(theme)),
            ("TOPBAR", &topbar_html),
            ("ROOM_TITLE", &room_title_html),
            ("ROOM_ID", &room_id_html),
            ("CONSEQUENCE", &consequence_html),
            ("MEMBERS", &members_html),
            ("MESSAGES", &messages_html),
        ],
    ) {
        Ok(page) => page,
        Err(error) => {
            tracing::error!(error = %error, "admin room template rendering failed");
            return Err(AppError::Unavailable(
                "admin rendering unavailable".to_string(),
            ));
        }
    };

    Ok(html_with_cookie(page, set_cookie))
}

// ---------------------------------------------------------------------------
// POST actions (admin-gated + CSRF-checked)
// ---------------------------------------------------------------------------

/// `POST /admin/rooms/{id}/archive` — make a room authorized-reader-visible but read-only.
pub async fn archive_room(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    form: Result<Form<CsrfForm>, FormRejection>,
) -> Result<Response, AppError> {
    auth::require_admin(&headers)?;
    let id = path_or_invalid(path)?;
    let form = form_or_invalid(form)?;
    guard(&headers, &form)?;
    if id == LOBBY_ID {
        return Err(AppError::Conflict(
            "the lobby cannot be archived".to_string(),
        ));
    }
    state
        .store
        .get_room(&id)
        .await?
        .ok_or_else(|| AppError::NotFound("resource unavailable".to_string()))?;
    state.store.set_room_archived(&id, true).await?;
    state.hub.invalidate(&id);
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
    path: Result<Path<String>, PathRejection>,
    form: Result<Form<DeleteRoomForm>, FormRejection>,
) -> Result<Response, AppError> {
    auth::require_admin(&headers)?;
    let (actor_sub, _actor_email) = auth::require_user(&headers)?;
    let id = path_or_invalid(path)?;
    let form = form_or_invalid(form)?;
    auth::verify_csrf_field(&headers, &form.csrf)?;
    if form.confirm != "delete" {
        return Err(AppError::InvalidRequest(
            "confirm must equal delete".to_string(),
        ));
    }
    if id == LOBBY_ID {
        return Err(AppError::Conflict(
            "the lobby cannot be deleted".to_string(),
        ));
    }
    if !valid_consequence_token(&form.consequence_token) {
        return Err(AppError::Conflict(
            "delete authorization invalid".to_string(),
        ));
    }
    let consequence = state
        .store
        .delete_room_with_token(
            &id,
            &digest_binding(&form.consequence_token),
            &actor_sub,
            &digest_binding(&form.csrf),
            crate::now_secs(),
        )
        .await?;
    state.hub.invalidate(&id);
    tracing::info!(
        room_id = %id,
        audit_consequence_id = %consequence.id,
        "admin deleted room with durable audit consequence"
    );
    Ok(Redirect::to("/admin").into_response())
}

/// `POST /admin/rooms/{id}/members/{user_sub}/remove` — kick a member from a room.
pub async fn remove_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
    form: Result<Form<CsrfForm>, FormRejection>,
) -> Result<Response, AppError> {
    auth::require_admin(&headers)?;
    let (id, user_sub) = path_or_invalid(path)?;
    let form = form_or_invalid(form)?;
    guard(&headers, &form)?;
    state.store.remove_member(&id, &user_sub).await?;
    state.hub.invalidate(&id);
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
    path: Result<Path<(String, String)>, PathRejection>,
    form: Result<Form<CsrfForm>, FormRejection>,
) -> Result<Response, AppError> {
    auth::require_admin(&headers)?;
    let (id, user_sub) = path_or_invalid(path)?;
    let form = form_or_invalid(form)?;
    guard(&headers, &form)?;
    state.store.ban_member(&id, &user_sub).await?;
    state.hub.invalidate(&id);
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
    path: Result<Path<String>, PathRejection>,
    form: Result<Form<CsrfForm>, FormRejection>,
) -> Result<Response, AppError> {
    auth::require_admin(&headers)?;
    let msg_id = path_or_invalid(path)?;
    let form = form_or_invalid(form)?;
    guard(&headers, &form)?;
    // Look up the room to redirect back to the detail page (also 404s a stray id gracefully).
    let room_id = state.store.get_message(&msg_id).await?.map(|m| m.room_id);
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

/// One row per room in the `/admin` table. Hard deletion is deliberately a link to the detail
/// page's consequence section; the destructive POST remains a second, CSRF-protected step.
fn render_room_rows(rooms: &[Room], csrf: &str) -> String {
    if rooms.is_empty() {
        return r#"<tr><td colspan="5" class="empty">No rooms.</td></tr>"#.to_string();
    }
    let mut out = String::new();
    for r in rooms {
        let status = if r.id == LOBBY_ID {
            "System room"
        } else if r.archived {
            "Archived"
        } else {
            "Active"
        };
        let actions = if r.id == LOBBY_ID {
            String::new()
        } else {
            format!(
                r#"<div class="cut-sheet__actions">{archive}<a class="cut-link" href="/admin/rooms/{id}#cut-delete">Delete…</a></div>"#,
                archive = archive_form(&r.id, r.archived, csrf),
                id = esc(&r.id),
            )
        };
        out.push_str(&format!(
            r#"<tr>
  <td data-label="Room"><a class="cut-sheet__room" href="/admin/rooms/{id}">{name}</a><span class="mono cut-sheet__id">{id}</span></td>
  <td data-label="Kind">{kind}</td>
  <td data-label="Created by">{creator}</td>
  <td data-label="Status"><span class="cut-sheet__state">{status}</span></td>
  <td data-label="Actions">{actions}</td>
</tr>"#,
            id = esc(&r.id),
            name = esc(&r.name),
            kind = esc(&r.kind),
            creator = esc(&r.created_by),
            status = status,
            actions = actions,
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
            r#"<form method="post" action="/admin/rooms/{id}/archive">{csrf}<button type="submit">Archive</button></form>"#,
            id = esc(room_id),
            csrf = csrf_input(csrf),
        )
    }
}

fn render_delete_consequence(room: &Room, csrf: &str, consequence_token: &str) -> String {
    if room.id == LOBBY_ID {
        return String::new();
    }
    format!(
        r#"<section class="consequence" id="cut-delete" aria-labelledby="cut-delete-title">
  <h2 class="consequence__title" id="cut-delete-title"><span class="consequence__glyph" aria-hidden="true">&#9986;</span>Delete this room</h2>
  <p class="consequence__scope">Permanently deletes <b>{name}</b> (<span class="mono">{id}</span>) — every message, member, reaction, pin, and mention. <b>This cannot be undone.</b></p>
  <form class="consequence__form" method="post" action="/admin/rooms/{id}/delete">
    <input type="hidden" name="confirm" value="delete">
    <input type="hidden" name="consequence_token" value="{consequence_token}">
    {csrf}
    <button class="cut-btn" type="submit">Delete permanently</button>
    <a class="consequence__cancel" href="/admin/rooms/{id}">Cancel</a>
  </form>
</section>"#,
        name = esc(&room.name),
        id = esc(&room.id),
        consequence_token = esc(consequence_token),
        csrf = csrf_input(csrf),
    )
}

fn digest_binding(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn valid_consequence_token(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// One row per member: email, subject, status, and remove/ban forms.
fn render_member_rows(room_id: &str, members: &[Member], csrf: &str) -> String {
    if members.is_empty() {
        return r#"<tr><td colspan="4" class="empty">No members.</td></tr>"#.to_string();
    }
    let mut out = String::new();
    for m in members {
        let status = if m.banned { "Banned" } else { "Member" };
        let ban = if m.banned {
            String::new()
        } else {
            format!(
                r#"<form method="post" action="/admin/rooms/{room}/members/{sub}/ban">{csrf}<button class="cut-btn" type="submit">Ban</button></form>"#,
                room = esc(room_id),
                sub = esc(&m.user_sub),
                csrf = csrf_input(csrf),
            )
        };
        out.push_str(&format!(
            r#"<tr>
  <td data-label="Email">{email}</td>
  <td data-label="Subject" class="mono">{sub}</td>
  <td data-label="Status">{status}</td>
  <td data-label="Actions"><div class="cut-sheet__actions">
    <form method="post" action="/admin/rooms/{room}/members/{sub}/remove">{csrf}<button type="submit">Remove</button></form>
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
            let label = if m.body.is_empty() {
                "[deleted]"
            } else {
                m.body.as_str()
            };
            (
                format!(r#"<span class="msg__deleted">{}</span>"#, esc(label)),
                String::new(),
            )
        } else {
            (
                format!(
                    r#"<span class="admin__body">{}</span>"#,
                    esc(&preview(&m.body))
                ),
                format!(
                    r#"<form method="post" action="/admin/messages/{id}/redact">{csrf}<button class="cut-btn" type="submit">Redact</button></form>"#,
                    id = esc(&m.id),
                    csrf = csrf_input(csrf),
                ),
            )
        };
        out.push_str(&format!(
            r#"<tr>
  <td data-label="Time">{time}</td>
  <td data-label="Author">{author}</td>
  <td data-label="Body">{body}</td>
  <td data-label="Actions"><div class="cut-sheet__actions">{action}</div></td>
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
