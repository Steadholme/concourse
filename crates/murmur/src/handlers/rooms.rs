//! JSON room + message API (all SSO-gated; mutating routes are CSRF-checked).
//!
//! Identity is ALWAYS taken from the gateway-injected `X-Auth-*` headers, never a client field.
//! Membership is enforced on every room-scoped read/write: a user can only read or post to rooms
//! they belong to. A successful send persists the message, fans it out to the live [`Hub`], and
//! best-effort audits `chat.message.send` to Watchtower.

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::audit::AuditEvent;
use crate::auth;
use crate::config::{MAX_BODY_CHARS, MAX_EMOJI_CHARS, MAX_ROOM_NAME_CHARS, MESSAGE_PAGE_LIMIT};
use crate::error::AppError;
use crate::handlers::{message_frame, reaction_frame};
use crate::store::{Message, Room};
use crate::{ensure_lobby, now_nanos, now_secs, AppState};

/// Body for `POST /api/rooms`.
#[derive(Debug, Deserialize)]
pub struct CreateRoomReq {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
}

/// Body for `POST /api/rooms/{id}/messages`.
#[derive(Debug, Deserialize)]
pub struct SendReq {
    #[serde(default)]
    pub body: String,
    /// Optional threaded-reply parent (a message id in the SAME room). Omitted/empty => top-level.
    #[serde(default)]
    pub reply_to_id: Option<String>,
}

/// Body for `POST /api/rooms/{id}/messages/{msg_id}/react`.
#[derive(Debug, Deserialize)]
pub struct ReactReq {
    #[serde(default)]
    pub emoji: String,
}

/// Body for `POST /api/rooms/{id}/messages/{msg_id}/edit`.
#[derive(Debug, Deserialize)]
pub struct EditReq {
    #[serde(default)]
    pub body: String,
}

/// Query for `GET /api/rooms/{id}/messages` — keyset cursor (messages strictly older than this).
#[derive(Debug, Deserialize)]
pub struct MessagesQuery {
    pub before: Option<i64>,
}

/// Query for `POST /api/rooms/{id}/read` — mark read up to this `created_at` (defaults to now).
#[derive(Debug, Deserialize)]
pub struct ReadQuery {
    pub at: Option<i64>,
}

/// `GET /api/rooms` — rooms this user belongs to. Ensures the `#lobby` exists and the caller is
/// a member of it (first-visit auto-join), so the list is never empty.
pub async fn list(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AppError> {
    let (sub, email) = auth::require_user(&headers)?;
    ensure_lobby(&state, &sub, &email).await;
    let rooms = state.store.list_user_rooms(&sub).await;
    Ok(Json(json!({ "rooms": rooms })).into_response())
}

/// `POST /api/rooms` — create a room and join the creator to it.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateRoomReq>,
) -> Result<Response, AppError> {
    let (sub, email) = auth::require_user(&headers)?;
    auth::verify_csrf(&headers)?;

    let name = req.name.trim();
    if name.is_empty() {
        return Err(AppError::InvalidRequest("room name is required".to_string()));
    }
    if name.chars().count() > MAX_ROOM_NAME_CHARS {
        return Err(AppError::InvalidRequest("room name too long".to_string()));
    }
    // Only `room` and `dm` are valid kinds; anything else falls back to `room`.
    let kind = match req.kind.as_deref().map(str::trim) {
        Some("dm") => "dm",
        _ => "room",
    }
    .to_string();

    let now = now_secs();
    let room = Room {
        id: format!("room_{}", now_nanos()),
        name: name.to_string(),
        kind,
        created_by: sub.clone(),
        created_at: now,
        archived: false,
    };
    state.store.ensure_room(&room).await?;
    state
        .store
        .ensure_membership(&room.id, &sub, &email, now)
        .await?;

    state.audit.emit(AuditEvent::info(
        "chat.room.create",
        &actor(&email, &sub),
        &room.id,
        &format!("kind={}", room.kind),
    ));
    tracing::info!(room_id = %room.id, "room created");

    Ok((StatusCode::CREATED, Json(json!({ "room": room }))).into_response())
}

/// `POST /api/rooms/{id}/join` — join an existing room.
pub async fn join(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let (sub, email) = auth::require_user(&headers)?;
    auth::verify_csrf(&headers)?;

    let room = state
        .store
        .get_room(&id)
        .await
        .ok_or_else(|| AppError::NotFound("no such room".to_string()))?;
    state
        .store
        .ensure_membership(&room.id, &sub, &email, now_secs())
        .await?;
    tracing::info!(room_id = %room.id, "user joined room");

    Ok(Json(json!({ "room": room })).into_response())
}

/// `GET /api/rooms/{id}/messages` — recent messages (newest-first), keyset-paginated by `before`.
pub async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<MessagesQuery>,
) -> Result<Response, AppError> {
    let (sub, _email) = auth::require_user(&headers)?;
    require_membership(&state, &id, &sub).await?;

    let messages = state
        .store
        .list_messages(&id, q.before, MESSAGE_PAGE_LIMIT)
        .await;
    Ok(Json(json!({ "room_id": id, "messages": messages })).into_response())
}

/// `POST /api/rooms/{id}/messages` — send a message: persist, fan out over the hub, audit.
pub async fn send(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<SendReq>,
) -> Result<Response, AppError> {
    let (sub, email) = auth::require_user(&headers)?;
    auth::verify_csrf(&headers)?;
    require_membership(&state, &id, &sub).await?;

    let body = req.body.trim();
    if body.is_empty() {
        return Err(AppError::InvalidRequest("message body is required".to_string()));
    }
    if body.chars().count() > MAX_BODY_CHARS {
        return Err(AppError::InvalidRequest("message body too long".to_string()));
    }

    // A threaded reply must point at an EXISTING message IN THIS ROOM (a reply can never cross
    // rooms or dangle). An empty/blank id is treated as "no reply".
    let reply_to_id = match req.reply_to_id.as_deref().map(str::trim) {
        Some(pid) if !pid.is_empty() => {
            let parent = state
                .store
                .get_message(pid)
                .await
                .filter(|m| m.room_id == id)
                .ok_or_else(|| AppError::InvalidRequest("reply target not found".to_string()))?;
            Some(parent.id)
        }
        _ => None,
    };

    let message = Message {
        id: format!("msg_{}", now_nanos()),
        room_id: id.clone(),
        sender_sub: sub.clone(),
        sender_email: email.clone(),
        body: body.to_string(),
        created_at: now_secs(),
        edited_at: 0,
        deleted: false,
        reply_to_id,
    };
    state.store.create_message(&message).await?;

    // Fan out to every WebSocket currently subscribed to this room (single-process; the DB is the
    // source of truth, so a missed live frame is recovered on the next GET).
    state.hub.publish(&id, message_frame(&message));

    // Best-effort audit — the body NEVER rides the event, only its length.
    state.audit.emit(AuditEvent::info(
        "chat.message.send",
        &actor(&email, &sub),
        &id,
        &format!("len={}", body.chars().count()),
    ));

    Ok((StatusCode::CREATED, Json(json!({ "message": message }))).into_response())
}

/// `POST /api/rooms/{id}/messages/{msg_id}/edit` — the AUTHOR edits their own message body.
/// Gated by `sender_sub == gateway subject`; CSRF-checked; persists, fans the updated frame out
/// over the hub, and audits.
pub async fn edit_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, msg_id)): Path<(String, String)>,
    Json(req): Json<EditReq>,
) -> Result<Response, AppError> {
    let (sub, email) = auth::require_user(&headers)?;
    auth::verify_csrf(&headers)?;
    require_membership(&state, &id, &sub).await?;

    let existing = require_own_message(&state, &id, &msg_id, &sub).await?;
    if existing.deleted {
        return Err(AppError::InvalidRequest("message is deleted".to_string()));
    }

    let body = req.body.trim();
    if body.is_empty() {
        return Err(AppError::InvalidRequest("message body is required".to_string()));
    }
    if body.chars().count() > MAX_BODY_CHARS {
        return Err(AppError::InvalidRequest("message body too long".to_string()));
    }

    let edited_at = now_secs();
    state
        .store
        .edit_message(&msg_id, body, edited_at)
        .await?;

    // Re-read so the fan-out frame reflects the persisted row exactly.
    let updated = state
        .store
        .get_message(&msg_id)
        .await
        .ok_or_else(|| AppError::NotFound("no such message".to_string()))?;
    state.hub.publish(&id, message_frame(&updated));

    state.audit.emit(AuditEvent::info(
        "chat.message.edit",
        &actor(&email, &sub),
        &msg_id,
        &format!("len={}", body.chars().count()),
    ));

    Ok(Json(json!({ "message": updated })).into_response())
}

/// `POST /api/rooms/{id}/messages/{msg_id}/delete` — the AUTHOR soft-deletes their own message.
/// Gated by `sender_sub == gateway subject`; CSRF-checked; marks deleted + clears the body, fans
/// the updated frame out over the hub, and audits.
pub async fn delete_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, msg_id)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let (sub, email) = auth::require_user(&headers)?;
    auth::verify_csrf(&headers)?;
    require_membership(&state, &id, &sub).await?;

    require_own_message(&state, &id, &msg_id, &sub).await?;

    state.store.delete_message(&msg_id).await?;

    let updated = state
        .store
        .get_message(&msg_id)
        .await
        .ok_or_else(|| AppError::NotFound("no such message".to_string()))?;
    state.hub.publish(&id, message_frame(&updated));

    // Destructive action -> `notice` severity (value-free: which message, by whom).
    state.audit.emit(AuditEvent::notice(
        "chat.message.delete",
        &actor(&email, &sub),
        &msg_id,
        "soft-deleted",
    ));

    Ok(Json(json!({ "message": updated })).into_response())
}

/// `POST /api/rooms/{id}/messages/{msg_id}/react` — toggle the caller's `{emoji}` reaction on a
/// message. CSRF-checked + membership-gated; the message must live in this room. Idempotent per
/// `(message, user, emoji)`: a second identical call removes it. Publishes the updated tallies over
/// the hub and audits. Returns the message's full reaction counts + the caller's own reactions.
pub async fn react(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, msg_id)): Path<(String, String)>,
    Json(req): Json<ReactReq>,
) -> Result<Response, AppError> {
    let (sub, email) = auth::require_user(&headers)?;
    auth::verify_csrf(&headers)?;
    require_membership(&state, &id, &sub).await?;

    // The target must exist AND belong to this room (defense in depth: no cross-room reactions).
    state
        .store
        .get_message(&msg_id)
        .await
        .filter(|m| m.room_id == id)
        .ok_or_else(|| AppError::NotFound("no such message".to_string()))?;

    let emoji = req.emoji.trim();
    if emoji.is_empty() {
        return Err(AppError::InvalidRequest("emoji is required".to_string()));
    }
    if emoji.chars().count() > MAX_EMOJI_CHARS {
        return Err(AppError::InvalidRequest("emoji too long".to_string()));
    }

    let added = state.store.toggle_reaction(&msg_id, &sub, emoji).await?;
    let reactions = state.store.list_reactions(&msg_id).await;
    let mine = state.store.list_user_reactions(&msg_id, &sub).await;

    // Fan the new tallies out to everyone watching the room so live chips update in place.
    state.hub.publish(&id, reaction_frame(&id, &msg_id, &reactions));

    // Best-effort audit — the emoji length + toggle direction only, never who-reacted-with-what text.
    state.audit.emit(AuditEvent::info(
        "chat.message.react",
        &actor(&email, &sub),
        &msg_id,
        &format!("added={} emoji_len={}", added, emoji.chars().count()),
    ));

    Ok(Json(json!({
        "message_id": msg_id,
        "added": added,
        "reactions": reactions,
        "mine": mine,
    }))
    .into_response())
}

/// `GET /api/rooms/{id}/messages/{msg_id}/reactions` — the message's per-emoji reaction tallies
/// plus the caller's own reactions (used to render + highlight chips). Membership-gated.
pub async fn reactions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, msg_id)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let (sub, _email) = auth::require_user(&headers)?;
    require_membership(&state, &id, &sub).await?;

    state
        .store
        .get_message(&msg_id)
        .await
        .filter(|m| m.room_id == id)
        .ok_or_else(|| AppError::NotFound("no such message".to_string()))?;

    let reactions = state.store.list_reactions(&msg_id).await;
    let mine = state.store.list_user_reactions(&msg_id, &sub).await;
    Ok(Json(json!({
        "message_id": msg_id,
        "reactions": reactions,
        "mine": mine,
    }))
    .into_response())
}

/// `POST /api/rooms/{id}/read` — advance the caller's read cursor (no CSRF; non-destructive).
pub async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<ReadQuery>,
) -> Result<Response, AppError> {
    let (sub, _email) = auth::require_user(&headers)?;
    require_membership(&state, &id, &sub).await?;

    let at = q.at.unwrap_or_else(now_secs);
    state.store.update_last_read(&id, &sub, at).await?;
    Ok(Json(json!({ "room_id": id, "last_read_at": at })).into_response())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Enforce that `sub` is a member of `room_id`, else `Forbidden` (or `NotFound` for a missing
/// room). Defense in depth behind the gateway: SSO authenticates, membership authorizes.
async fn require_membership(state: &AppState, room_id: &str, sub: &str) -> Result<(), AppError> {
    if state.store.get_room(room_id).await.is_none() {
        return Err(AppError::NotFound("no such room".to_string()));
    }
    if !state.store.is_member(room_id, sub).await {
        return Err(AppError::Forbidden("not a member of this room".to_string()));
    }
    Ok(())
}

/// Fetch a message and enforce that it lives in `room_id` AND was authored by `sub`
/// (`created_by == gateway subject`). `NotFound` for a missing/other-room message; `Forbidden`
/// when the caller is not the author. Returns the message so callers can inspect its state.
async fn require_own_message(
    state: &AppState,
    room_id: &str,
    msg_id: &str,
    sub: &str,
) -> Result<Message, AppError> {
    let message = state
        .store
        .get_message(msg_id)
        .await
        .filter(|m| m.room_id == room_id)
        .ok_or_else(|| AppError::NotFound("no such message".to_string()))?;
    if message.sender_sub != sub {
        return Err(AppError::Forbidden("not the message author".to_string()));
    }
    Ok(message)
}

/// Prefer the email as the human-readable actor; fall back to the subject id.
fn actor(email: &str, sub: &str) -> String {
    if email.is_empty() {
        sub.to_string()
    } else {
        email.to_string()
    }
}
