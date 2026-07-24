//! JSON room + message API (all SSO-gated; mutating routes are CSRF-checked).
//!
//! Identity is ALWAYS taken from the gateway-injected `X-Auth-*` headers, never a client field.
//! Membership is enforced on every room-scoped read/write: a user can only read or post to rooms
//! they belong to. A successful send persists the message, fans it out to the live [`Hub`], and
//! best-effort audits `chat.message.send` to Watchtower.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::body::to_bytes;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::audit::AuditEvent;
use crate::auth;
use crate::config::{
    MAX_BODY_CHARS, MAX_EMOJI_CHARS, MAX_ROOM_NAME_CHARS, MAX_TOPIC_CHARS, MESSAGE_PAGE_LIMIT,
};
use crate::error::AppError;
use crate::handlers::{message_frame, path_or_invalid, query_or_invalid, reaction_frame};
use crate::store::{Member, Message, MessageCursor, Person, Room};
use crate::text::parse_mentions;
use crate::{ensure_lobby, now_nanos, now_secs, AppState, KlaxonNotifier};

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

#[derive(Debug)]
enum SendMode {
    Json,
    Form { csrf: String },
}

#[derive(Debug)]
enum ReadMode {
    Json,
    Form,
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

/// Body for `POST /api/rooms/{id}/topic` — the new room topic (empty clears it).
#[derive(Debug, Deserialize)]
pub struct SetTopicReq {
    #[serde(default)]
    pub topic: String,
}

/// Query for `GET /api/rooms/{id}/messages` — keyset cursor (messages strictly older than this).
#[derive(Debug, Deserialize)]
pub struct MessagesQuery {
    pub before: Option<String>,
}

/// Legacy query for `POST /api/rooms/{id}/read`. `at` is parsed for compatibility but never used
/// as read authority.
#[derive(Debug, Deserialize)]
pub struct ReadQuery {
    pub at: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ReadReq {
    #[serde(default)]
    pub message_id: String,
}

/// `GET /api/rooms` — rooms this user belongs to. Ensures the `#lobby` exists and the caller is
/// a member of it (first-visit auto-join), so the list is never empty.
pub async fn list(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AppError> {
    let (sub, email) = auth::require_user(&headers)?;
    ensure_lobby(&state, &sub, &email).await?;
    let rooms = state.store.list_user_rooms(&sub).await?;
    Ok(Json(json!({ "rooms": rooms })).into_response())
}

/// `POST /api/rooms` — create a room and join the creator to it.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateRoomReq>, JsonRejection>,
) -> Result<Response, AppError> {
    let (sub, email) = auth::require_user(&headers)?;
    auth::verify_csrf(&headers)?;
    let Json(req) =
        payload.map_err(|_| AppError::InvalidRequest("invalid JSON body".to_string()))?;

    let name = req.name.trim();
    if name.is_empty() {
        return Err(AppError::InvalidRequest(
            "room name is required".to_string(),
        ));
    }
    if name.chars().count() > MAX_ROOM_NAME_CHARS {
        return Err(AppError::InvalidRequest("room name too long".to_string()));
    }
    // DMs are created only by the dedicated `/api/dms` authority.
    if req.kind.as_deref().is_some_and(|kind| kind.trim() == "dm") {
        return Err(AppError::InvalidRequest(
            "generic room creation cannot create a DM".to_string(),
        ));
    }
    let kind = "room".to_string();

    let now = now_secs();
    let room = Room {
        id: format!("room_{}", now_nanos()),
        name: name.to_string(),
        kind,
        created_by: sub.clone(),
        created_at: now,
        archived: false,
        topic: String::new(),
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
    path: Result<Path<String>, PathRejection>,
) -> Result<Response, AppError> {
    let id = path_or_invalid(path)?;
    let (sub, email) = auth::require_user(&headers)?;
    auth::verify_csrf(&headers)?;

    let room = state
        .store
        .join_active_room(&id, &sub, &email, now_secs())
        .await?;
    tracing::info!(room_id = %room.id, "user joined room");

    Ok(Json(json!({ "room": room })).into_response())
}

/// `GET /api/rooms/{id}/messages` — recent messages (newest-first), keyset-paginated by `before`.
pub async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<MessagesQuery>, QueryRejection>,
) -> Result<Response, AppError> {
    let id = path_or_invalid(path)?;
    let q = query_or_invalid(query)?;
    let (sub, _email) = auth::require_user(&headers)?;
    let before = parse_message_cursor(q.before.as_deref())?;
    let mut messages = state
        .store
        .list_messages_authorized(&id, &sub, before, MESSAGE_PAGE_LIMIT + 1)
        .await?;
    let has_more = messages.len() > MESSAGE_PAGE_LIMIT as usize;
    messages.truncate(MESSAGE_PAGE_LIMIT as usize);
    let next_cursor = has_more.then(|| {
        messages
            .last()
            .map(MessageCursor::from_message)
            .map(|cursor| cursor.encode())
    });
    Ok(Json(json!({
        "room_id": id,
        "messages": messages,
        "next_cursor": next_cursor.flatten(),
    }))
    .into_response())
}

/// `POST /api/rooms/{id}/messages` — send a message: persist, fan out over the hub, audit.
pub async fn send(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    request: axum::extract::Request,
) -> Response {
    let id = match path_or_invalid(path) {
        Ok(id) => id,
        Err(error) => return error.into_response(),
    };
    let is_form = is_form_content_type(&headers);
    let (sub, email) = match auth::require_user(&headers) {
        Ok(identity) => identity,
        Err(error) => return error.into_response(),
    };
    let (req, mode) = match decode_send_request(&headers, request).await {
        Ok(decoded) => decoded,
        Err((error, draft, csrf)) => {
            return if is_form {
                send_form_error(&id, &draft, &csrf, &error)
            } else {
                error.into_response()
            };
        }
    };
    let form_csrf = match &mode {
        SendMode::Json => String::new(),
        SendMode::Form { csrf } => csrf.clone(),
    };

    let result = async {
        let room = require_membership(&state, &id, &sub).await?;
        require_active_room(&room)?;

        let body = req.body.trim();
        if body.is_empty() {
            return Err(AppError::InvalidRequest(
                "message body is required".to_string(),
            ));
        }
        if body.chars().count() > MAX_BODY_CHARS {
            return Err(AppError::InvalidRequest(
                "message body too long".to_string(),
            ));
        }

        // The Store validates this parent against the same room inside the message transaction.
        let reply_to_id = req
            .reply_to_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string);

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
        let mention_tokens = parse_mentions(&message.body);
        let mentioned = state
            .store
            .create_message_authorized(&message, &mention_tokens)
            .await?;
        // Notification projection is best-effort after the durable transaction. It cannot turn a
        // persisted send into an ambiguous 5xx.
        let reply_parent = match message.reply_to_id.as_deref() {
            Some(parent_id) => match state.store.get_message(parent_id).await {
                Ok(parent) => parent.filter(|parent| parent.room_id == id),
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        room_id = %id,
                        message_id = %parent_id,
                        "resolve reply notification context failed"
                    );
                    None
                }
            },
            None => None,
        };
        spawn_message_notifications(&state, &room, &message, reply_parent.as_ref(), mentioned);

        state.hub.publish(&id, message_frame(&message));
        state.audit.emit(AuditEvent::info(
            "chat.message.send",
            &actor(&email, &sub),
            &id,
            &format!("len={}", body.chars().count()),
        ));
        Ok::<Message, AppError>(message)
    }
    .await;

    match result {
        Ok(message) => match mode {
            SendMode::Json => {
                let message_id = message.id.clone();
                let created_at = message.created_at;
                (
                    StatusCode::CREATED,
                    Json(json!({
                        "message": message,
                        "receipt": {
                            "state": "persisted",
                            "message_id": message_id,
                            "created_at": created_at,
                        }
                    })),
                )
                    .into_response()
            }
            SendMode::Form { .. } => {
                let location = format!(
                    "/?room={}&receipt_message={}#msg-{}",
                    url_query_escape(&id),
                    url_query_escape(&message.id),
                    url_fragment_escape(&message.id),
                );
                Redirect::to(&location).into_response()
            }
        },
        Err(error) => match mode {
            SendMode::Json => error.into_response(),
            SendMode::Form { .. } => send_form_error(&id, &req, &form_csrf, &error),
        },
    }
}

/// `POST /api/rooms/{id}/messages/{msg_id}/edit` — the AUTHOR edits their own message body.
/// Gated by `sender_sub == gateway subject`; CSRF-checked; persists, fans the updated frame out
/// over the hub, and audits.
pub async fn edit_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
    payload: Result<Json<EditReq>, JsonRejection>,
) -> Result<Response, AppError> {
    let (id, msg_id) = path_or_invalid(path)?;
    let (sub, email) = auth::require_user(&headers)?;
    auth::verify_csrf(&headers)?;
    let Json(req) =
        payload.map_err(|_| AppError::InvalidRequest("invalid JSON body".to_string()))?;

    let body = req.body.trim();
    if body.is_empty() {
        return Err(AppError::InvalidRequest(
            "message body is required".to_string(),
        ));
    }
    if body.chars().count() > MAX_BODY_CHARS {
        return Err(AppError::InvalidRequest(
            "message body too long".to_string(),
        ));
    }

    let edited_at = now_secs();
    let updated = state
        .store
        .edit_message_authorized(&id, &msg_id, &sub, body, edited_at)
        .await?;
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
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<Response, AppError> {
    let (id, msg_id) = path_or_invalid(path)?;
    let (sub, email) = auth::require_user(&headers)?;
    auth::verify_csrf(&headers)?;
    let updated = state
        .store
        .delete_message_authorized(&id, &msg_id, &sub)
        .await?;
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
    path: Result<Path<(String, String)>, PathRejection>,
    payload: Result<Json<ReactReq>, JsonRejection>,
) -> Result<Response, AppError> {
    let (id, msg_id) = path_or_invalid(path)?;
    let (sub, email) = auth::require_user(&headers)?;
    auth::verify_csrf(&headers)?;
    let Json(req) =
        payload.map_err(|_| AppError::InvalidRequest("invalid JSON body".to_string()))?;

    let emoji = req.emoji.trim();
    if emoji.is_empty() {
        return Err(AppError::InvalidRequest("emoji is required".to_string()));
    }
    if emoji.chars().count() > MAX_EMOJI_CHARS {
        return Err(AppError::InvalidRequest("emoji too long".to_string()));
    }

    let mutation = state
        .store
        .toggle_reaction_authorized(&id, &msg_id, &sub, emoji)
        .await?;

    // Fan the new tallies out to everyone watching the room so live chips update in place.
    state
        .hub
        .publish(&id, reaction_frame(&id, &msg_id, &mutation.reactions));

    // Best-effort audit — the emoji length + toggle direction only, never who-reacted-with-what text.
    state.audit.emit(AuditEvent::info(
        "chat.message.react",
        &actor(&email, &sub),
        &msg_id,
        &format!(
            "added={} emoji_len={}",
            mutation.added,
            emoji.chars().count()
        ),
    ));

    Ok(Json(json!({
        "message_id": msg_id,
        "added": mutation.added,
        "reactions": mutation.reactions,
        "mine": mutation.mine,
    }))
    .into_response())
}

/// `GET /api/rooms/{id}/messages/{msg_id}/reactions` — the message's per-emoji reaction tallies
/// plus the caller's own reactions (used to render + highlight chips). Membership-gated.
pub async fn reactions(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<Response, AppError> {
    let (id, msg_id) = path_or_invalid(path)?;
    let (sub, _email) = auth::require_user(&headers)?;
    let projection = state
        .store
        .reaction_projection_authorized(&id, &msg_id, &sub)
        .await?;
    Ok(Json(json!({
        "message_id": msg_id,
        "reactions": projection.reactions,
        "mine": projection.mine,
    }))
    .into_response())
}

/// `POST /api/rooms/{id}/read` — advance the caller's read cursor to a server-owned message
/// timestamp. The legacy `?at=` value is parsed but never trusted as authority.
pub async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<ReadQuery>, QueryRejection>,
    request: axum::extract::Request,
) -> Result<Response, AppError> {
    let id = path_or_invalid(path)?;
    let _legacy = query_or_invalid(query)?;
    let (sub, _email) = auth::require_user(&headers)?;
    let (req, mode) = decode_read_request(&headers, request).await?;
    let message_id = req.message_id.trim();
    if message_id.is_empty() {
        return Err(AppError::InvalidRequest(
            "message_id is required".to_string(),
        ));
    }
    let message = state
        .store
        .update_last_read_authorized(&id, message_id, &sub)
        .await?;
    Ok(match mode {
        ReadMode::Json => Json(json!({
            "room_id": id,
            "message_id": message.id,
            "last_read_at": message.created_at,
            "last_read_cursor": MessageCursor::from_message(&message).encode(),
            "persisted": true,
        }))
        .into_response(),
        ReadMode::Form => Redirect::to(&format!(
            "/?room={}&message={}#msg-{}",
            url_query_escape(&id),
            url_query_escape(&message.id),
            url_fragment_escape(&message.id),
        ))
        .into_response(),
    })
}

/// `POST /api/rooms/{id}/topic` — set the room's topic. Gated to room admins/mods (reuse the
/// moderation gate); CSRF-checked. The room must exist. Persists + audits + returns the room.
pub async fn set_topic(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    payload: Result<Json<SetTopicReq>, JsonRejection>,
) -> Result<Response, AppError> {
    let id = path_or_invalid(path)?;
    let (sub, email) = auth::require_user(&headers)?;
    auth::verify_csrf(&headers)?;
    // Moderation gate: only admins/mods may edit a room topic (reuses admin_groups()).
    auth::require_admin(&headers)?;
    let Json(req) =
        payload.map_err(|_| AppError::InvalidRequest("invalid JSON body".to_string()))?;

    let topic = req.topic.trim();
    if topic.chars().count() > MAX_TOPIC_CHARS {
        return Err(AppError::InvalidRequest("topic too long".to_string()));
    }
    let updated = state.store.set_room_topic_authorized(&id, topic).await?;

    state.audit.emit(AuditEvent::info(
        "chat.room.topic",
        &actor(&email, &sub),
        &id,
        &format!("len={}", topic.chars().count()),
    ));
    tracing::info!(room_id = %id, "room topic set");

    Ok(Json(json!({ "room": updated })).into_response())
}

/// `POST /api/rooms/{id}/messages/{msg_id}/pin` — pin a message in the room. Gated to mods+
/// (moderation gate); CSRF-checked; the message must live in this room. Idempotent + audited.
pub async fn pin(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<Response, AppError> {
    let (id, msg_id) = path_or_invalid(path)?;
    let (sub, email) = auth::require_user(&headers)?;
    auth::verify_csrf(&headers)?;
    auth::require_admin(&headers)?;
    let pinned = state
        .store
        .pin_message_authorized(&id, &msg_id, &sub, now_secs())
        .await?;

    state.audit.emit(AuditEvent::notice(
        "chat.message.pin",
        &actor(&email, &sub),
        &msg_id,
        "pinned",
    ));
    tracing::info!(room_id = %id, %msg_id, "message pinned");

    Ok(Json(json!({ "room_id": id, "pinned": true, "messages": pinned })).into_response())
}

/// `POST /api/rooms/{id}/messages/{msg_id}/unpin` — unpin a message. Gated to mods+; CSRF-checked.
/// Idempotent (no-op when it was not pinned) + audited.
pub async fn unpin(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<Response, AppError> {
    let (id, msg_id) = path_or_invalid(path)?;
    let (sub, email) = auth::require_user(&headers)?;
    auth::verify_csrf(&headers)?;
    auth::require_admin(&headers)?;
    let pinned = state.store.unpin_message_authorized(&id, &msg_id).await?;

    state.audit.emit(AuditEvent::notice(
        "chat.message.unpin",
        &actor(&email, &sub),
        &msg_id,
        "unpinned",
    ));
    tracing::info!(room_id = %id, %msg_id, "message unpinned");

    Ok(Json(json!({ "room_id": id, "pinned": false, "messages": pinned })).into_response())
}

/// `GET /api/rooms/{id}/pinned` — the room's pinned-message panel. Membership-gated (a non-member
/// can never read a room's pinned content), newest-pinned first.
pub async fn pinned(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
) -> Result<Response, AppError> {
    let id = path_or_invalid(path)?;
    let (sub, _email) = auth::require_user(&headers)?;
    let messages = state.store.list_pinned_authorized(&id, &sub).await?;
    Ok(Json(json!({ "room_id": id, "messages": messages })).into_response())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const MUTATION_BODY_LIMIT: usize = 128 * 1024;

fn content_type(headers: &HeaderMap) -> &str {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or("")
}

fn is_form_content_type(headers: &HeaderMap) -> bool {
    content_type(headers).eq_ignore_ascii_case("application/x-www-form-urlencoded")
}

async fn decode_send_request(
    headers: &HeaderMap,
    request: axum::extract::Request,
) -> Result<(SendReq, SendMode), (AppError, SendReq, String)> {
    let empty = || SendReq {
        body: String::new(),
        reply_to_id: None,
    };
    let bytes = to_bytes(request.into_body(), MUTATION_BODY_LIMIT)
        .await
        .map_err(|_| {
            (
                AppError::InvalidRequest("request body too large".to_string()),
                empty(),
                String::new(),
            )
        })?;
    match content_type(headers) {
        value if value.eq_ignore_ascii_case("application/json") => {
            auth::verify_csrf(headers).map_err(|error| (error, empty(), String::new()))?;
            let request = serde_json::from_slice(&bytes).map_err(|_| {
                (
                    AppError::InvalidRequest("invalid JSON body".to_string()),
                    empty(),
                    String::new(),
                )
            })?;
            Ok((request, SendMode::Json))
        }
        value if value.eq_ignore_ascii_case("application/x-www-form-urlencoded") => {
            let mut fields =
                decode_form(&bytes).map_err(|error| (error, empty(), String::new()))?;
            let csrf = fields.remove("csrf").unwrap_or_default();
            let request = SendReq {
                body: fields.remove("body").unwrap_or_default(),
                reply_to_id: fields
                    .remove("reply_to_id")
                    .filter(|value| !value.trim().is_empty()),
            };
            if let Err(error) = auth::verify_csrf_field(headers, &csrf) {
                return Err((error, request, String::new()));
            }
            Ok((request, SendMode::Form { csrf }))
        }
        _ => Err((
            AppError::InvalidRequest(
                "content type must be application/json or form-urlencoded".to_string(),
            ),
            empty(),
            String::new(),
        )),
    }
}

async fn decode_read_request(
    headers: &HeaderMap,
    request: axum::extract::Request,
) -> Result<(ReadReq, ReadMode), AppError> {
    let bytes = to_bytes(request.into_body(), MUTATION_BODY_LIMIT)
        .await
        .map_err(|_| AppError::InvalidRequest("request body too large".to_string()))?;
    match content_type(headers) {
        value if value.eq_ignore_ascii_case("application/json") => {
            auth::verify_csrf(headers)?;
            let request = serde_json::from_slice(&bytes)
                .map_err(|_| AppError::InvalidRequest("invalid JSON body".to_string()))?;
            Ok((request, ReadMode::Json))
        }
        value if value.eq_ignore_ascii_case("application/x-www-form-urlencoded") => {
            let mut fields = decode_form(&bytes)?;
            let csrf = fields.remove("csrf").unwrap_or_default();
            auth::verify_csrf_field(headers, &csrf)?;
            Ok((
                ReadReq {
                    message_id: fields.remove("message_id").unwrap_or_default(),
                },
                ReadMode::Form,
            ))
        }
        _ => Err(AppError::InvalidRequest(
            "content type must be application/json or form-urlencoded".to_string(),
        )),
    }
}

fn decode_form(bytes: &[u8]) -> Result<HashMap<String, String>, AppError> {
    let raw = std::str::from_utf8(bytes)
        .map_err(|_| AppError::InvalidRequest("form body must be UTF-8".to_string()))?;
    let mut fields = HashMap::new();
    for pair in raw.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode_form_component(key)?;
        let value = decode_form_component(value)?;
        fields.insert(key, value);
    }
    Ok(fields)
}

fn decode_form_component(value: &str) -> Result<String, AppError> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let high = decode_hex(bytes[index + 1])
                    .ok_or_else(|| AppError::InvalidRequest("invalid form encoding".to_string()))?;
                let low = decode_hex(bytes[index + 2])
                    .ok_or_else(|| AppError::InvalidRequest("invalid form encoding".to_string()))?;
                decoded.push((high << 4) | low);
                index += 3;
            }
            b'%' => {
                return Err(AppError::InvalidRequest(
                    "invalid form encoding".to_string(),
                ));
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded)
        .map_err(|_| AppError::InvalidRequest("form body must be UTF-8".to_string()))
}

fn decode_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_message_cursor(value: Option<&str>) -> Result<Option<MessageCursor>, AppError> {
    value
        .map(|value| {
            MessageCursor::decode(value)
                .ok_or_else(|| AppError::InvalidRequest("invalid query parameters".to_string()))
        })
        .transpose()
}

fn send_form_error(room_id: &str, request: &SendReq, csrf: &str, error: &AppError) -> Response {
    let reply = request.reply_to_id.as_deref().unwrap_or("");
    let (title, message) = match error {
        AppError::Unavailable(_) | AppError::Internal(_) => (
            "Send uncertain",
            "Send uncertain — your text is preserved; check the tape before retrying".to_string(),
        ),
        _ => ("Not sent", error.safe_message()),
    };
    let recovery = match error {
        AppError::InvalidRequest(_) if !csrf.is_empty() => format!(
            r#"<form method="post" action="/api/rooms/{room}/messages"><textarea name="body" maxlength="{max}">{body}</textarea><input type="hidden" name="reply_to_id" value="{reply}"><input type="hidden" name="csrf" value="{csrf}"><button type="submit">Correct and send</button></form>"#,
            room = crate::handlers::esc(room_id),
            max = MAX_BODY_CHARS,
            body = crate::handlers::esc(&request.body),
            reply = crate::handlers::esc(reply),
            csrf = crate::handlers::esc(csrf),
        ),
        AppError::CsrfInvalid | AppError::Unauthorized(_) => format!(
            r#"<p>Reload the tape to recover a valid request token. Your text remains below for copying.</p><textarea readonly>{}</textarea>"#,
            crate::handlers::esc(&request.body),
        ),
        _ => format!(
            r#"<p>Your text remains below for checking before any later action.</p><textarea readonly>{}</textarea>"#,
            crate::handlers::esc(&request.body),
        ),
    };
    let page = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title} · Murmur</title></head><body><main><h1>{title}</h1><p role="alert">{message}</p>{recovery}<a href="/?room={room}">Back to tape</a></main></body></html>"#,
        title = crate::handlers::esc(title),
        message = crate::handlers::esc(&message),
        recovery = recovery,
        room = crate::handlers::esc(room_id),
    );
    (error.status_code(), Html(page)).into_response()
}

fn spawn_message_notifications(
    state: &AppState,
    room: &Room,
    message: &Message,
    reply_parent: Option<&Message>,
    mentioned: Vec<Member>,
) {
    let Some(notifier) = state.klaxon.clone() else {
        return;
    };
    let store = state.store.clone();
    let room = room.clone();
    let message = message.clone();
    let reply_parent_id = reply_parent.map(|m| m.id.clone());

    tokio::spawn(async move {
        let sender = actor(&message.sender_email, &message.sender_sub);
        let body = message_summary(&message.body);
        let url = message_url(&message.room_id, &message.id);
        let mut notified = HashSet::new();

        for member in mentioned {
            notify_once(
                &notifier,
                &mut notified,
                &message.sender_sub,
                &member.user_sub,
                &mention_title(&sender, &room),
                &body,
                &url,
            );
        }

        if room.kind == "dm" {
            match store.list_room_members(&room.id).await {
                Ok(members) => {
                    for member in members.into_iter().filter(|m| !m.banned) {
                        notify_once(
                            &notifier,
                            &mut notified,
                            &message.sender_sub,
                            &member.user_sub,
                            &dm_title(&sender),
                            &body,
                            &url,
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, room_id = %room.id, "resolve DM notifications failed");
                }
            }
        }

        if let Some(parent_id) = reply_parent_id {
            match store.list_thread_participants(&room.id, &parent_id).await {
                Ok(participants) => {
                    for person in participants {
                        notify_person_once(
                            &notifier,
                            &mut notified,
                            &message.sender_sub,
                            &person,
                            &thread_title(&sender, &room),
                            &body,
                            &url,
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, room_id = %room.id, "resolve thread notifications failed");
                }
            }
        }
    });
}

fn notify_person_once(
    notifier: &Arc<KlaxonNotifier>,
    notified: &mut HashSet<String>,
    sender_sub: &str,
    person: &Person,
    title: &str,
    body: &str,
    url: &str,
) {
    notify_once(
        notifier,
        notified,
        sender_sub,
        &person.user_sub,
        title,
        body,
        url,
    );
}

fn notify_once(
    notifier: &Arc<KlaxonNotifier>,
    notified: &mut HashSet<String>,
    sender_sub: &str,
    recipient_sub: &str,
    title: &str,
    body: &str,
    url: &str,
) {
    if recipient_sub == sender_sub || !notified.insert(recipient_sub.to_string()) {
        return;
    }
    notifier.notify("murmur", recipient_sub, title, body, url);
}

fn mention_title(sender: &str, room: &Room) -> String {
    if room.kind == "dm" {
        format!("{sender} 在私信中提到了你")
    } else {
        format!("{sender} 在 {} 提到了你", room_label(room))
    }
}

fn dm_title(sender: &str) -> String {
    format!("{sender} 发来一条私信")
}

fn thread_title(sender: &str, room: &Room) -> String {
    if room.kind == "dm" {
        format!("{sender} 回复了你的私信线程")
    } else {
        format!("{sender} 在 {} 回复了你参与的线程", room_label(room))
    }
}

fn room_label(room: &Room) -> String {
    let name = room.name.trim();
    if name.is_empty() {
        return room.id.clone();
    }
    if name.starts_with('#') {
        name.to_string()
    } else {
        format!("#{name}")
    }
}

fn message_summary(body: &str) -> String {
    const SUMMARY_CHARS: usize = 180;
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = collapsed.chars().count();
    let mut out = collapsed.chars().take(SUMMARY_CHARS).collect::<String>();
    if count > SUMMARY_CHARS {
        out.push_str("...");
    }
    out
}

fn message_url(room_id: &str, message_id: &str) -> String {
    format!(
        "https://chat.w33d.xyz/?room={}&message={}",
        url_query_escape(room_id),
        url_query_escape(message_id)
    )
}

fn url_query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn url_fragment_escape(s: &str) -> String {
    url_query_escape(s)
}

/// Enforce that `sub` is a member of `room_id`, else `Forbidden` (or `NotFound` for a missing
/// room). Defense in depth behind the gateway: SSO authenticates, membership authorizes.
async fn require_membership(state: &AppState, room_id: &str, sub: &str) -> Result<Room, AppError> {
    state
        .store
        .authorize_room_read(room_id, sub, false)
        .await
        .map_err(AppError::from)
}

fn require_active_room(room: &Room) -> Result<(), AppError> {
    if room.archived {
        Err(AppError::RoomArchived)
    } else {
        Ok(())
    }
}

/// Prefer the email as the human-readable actor; fall back to the subject id.
fn actor(email: &str, sub: &str) -> String {
    if email.is_empty() {
        sub.to_string()
    } else {
        email.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(id: &str, name: &str, kind: &str) -> Room {
        Room {
            id: id.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            created_by: "u_alice".to_string(),
            created_at: 1,
            archived: false,
            topic: String::new(),
        }
    }

    #[test]
    fn notification_helper_shapes_are_stable() {
        let lobby = room("room one/alpha", "#lobby", "room");
        assert_eq!(room_label(&lobby), "#lobby");
        assert_eq!(
            mention_title("alice@hf", &lobby),
            "alice@hf 在 #lobby 提到了你"
        );
        assert_eq!(
            message_url(&lobby.id, "msg 1/2"),
            "https://chat.w33d.xyz/?room=room%20one%2Falpha&message=msg%201%2F2"
        );

        let body = format!("hello\n{}\tend", "x".repeat(190));
        let summary = message_summary(&body);
        assert!(summary.starts_with("hello "));
        assert!(summary.ends_with("..."));
        assert!(summary.chars().count() <= 183);
    }

    #[test]
    fn dm_and_thread_titles_use_private_context() {
        let dm = room("dm_u_alice__u_bob", "alice@hf ↔ bob@hf", "dm");
        assert_eq!(mention_title("alice@hf", &dm), "alice@hf 在私信中提到了你");
        assert_eq!(dm_title("alice@hf"), "alice@hf 发来一条私信");
        assert_eq!(thread_title("alice@hf", &dm), "alice@hf 回复了你的私信线程");
    }

    #[tokio::test]
    async fn form_store_failure_is_uncertain_and_never_offers_retry() {
        let response = send_form_error(
            "lobby",
            &SendReq {
                body: "preserve this exact draft".to_string(),
                reply_to_id: None,
            },
            "valid-token",
            &AppError::Unavailable("raw backend detail".to_string()),
        );
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body
            .contains("Send uncertain — your text is preserved; check the tape before retrying"));
        assert!(body.contains("preserve this exact draft"));
        assert!(!body.contains("raw backend detail"));
        assert!(!body.contains(r#"<button type="submit">"#));
    }
}
