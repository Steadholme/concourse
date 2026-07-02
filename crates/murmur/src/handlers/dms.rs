//! Direct messages + the people directory (all SSO-gated; the open-DM write is CSRF-checked).
//!
//! A DM is just a `kind = "dm"` room keyed by the SORTED pair of the two participants' gateway
//! subjects, so it is deterministic and auto-created (idempotently) on the first DM — no separate
//! DM tables. Once created it reuses the existing rooms/messages/membership machinery verbatim:
//! the timeline, keyset pagination, unread `last_read_at` cursor, and the live Hub fan-out all
//! work unchanged, and it shows up in `GET /api/rooms` for both members.
//!
//! Identity is ALWAYS the gateway-injected `X-Auth-*` subject/email, never a client field. The
//! request body only names WHO to DM (a subject + email picked from `GET /api/directory`).

use axum::extract::State;
use axum::http::StatusCode;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::audit::AuditEvent;
use crate::auth;
use crate::error::AppError;
use crate::store::Room;
use crate::{now_secs, AppState};

/// Body for `POST /api/dms` — the peer to open a direct message with (picked from the directory).
#[derive(Debug, Deserialize)]
pub struct OpenDmReq {
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub email: String,
}

/// `GET /api/directory` — distinct known people the caller can DM (excludes the caller).
pub async fn directory(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let (sub, _email) = auth::require_user(&headers)?;
    let people = state.store.list_directory(&sub).await;
    Ok(Json(json!({ "people": people })).into_response())
}

/// `POST /api/dms` — open (or reuse) the 1:1 DM room with the named peer and join both members.
/// The room id is the sorted-pair key, so repeat calls (from either side) resolve to the SAME
/// room. Returns `{ room }`; the client then selects it and uses the ordinary message API.
pub async fn open(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OpenDmReq>,
) -> Result<Response, AppError> {
    let (sub, email) = auth::require_user(&headers)?;
    auth::verify_csrf(&headers)?;

    let peer_sub = req.subject.trim();
    let peer_email = req.email.trim();
    if peer_sub.is_empty() {
        return Err(AppError::InvalidRequest("peer subject is required".to_string()));
    }
    if peer_sub == sub {
        return Err(AppError::InvalidRequest("cannot DM yourself".to_string()));
    }

    let id = dm_room_id(&sub, peer_sub);
    let name = dm_room_name(&sub, &email, peer_sub, peer_email);
    let now = now_secs();
    let room = Room {
        id: id.clone(),
        name,
        kind: "dm".to_string(),
        created_by: sub.clone(),
        created_at: now,
        archived: false,
        topic: String::new(),
    };
    // Idempotent: `ensure_room` no-ops if the DM already exists (first-creator's row wins).
    state.store.ensure_room(&room).await?;
    state.store.ensure_membership(&id, &sub, &email, now).await?;
    state
        .store
        .ensure_membership(&id, peer_sub, peer_email, now)
        .await?;

    // Re-read so the response reflects the persisted row exactly (name from the first creator).
    let room = state.store.get_room(&id).await.unwrap_or(room);

    state.audit.emit(AuditEvent::info(
        "chat.dm.open",
        &actor(&email, &sub),
        &id,
        &format!("peer={peer_sub}"),
    ));
    tracing::info!(room_id = %id, "dm opened");

    Ok((StatusCode::CREATED, Json(json!({ "room": room }))).into_response())
}

/// The deterministic DM room id: `dm_{lo}__{hi}` over the two subjects sorted, so both sides
/// resolve to one room regardless of who opens it first.
fn dm_room_id(a: &str, b: &str) -> String {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    format!("dm_{lo}__{hi}")
}

/// The DM room's display name: both participants' emails joined, ordered by subject so it is the
/// SAME label no matter which side opens it first. Falls back to the subject when an email is
/// blank. The name is stored/escaped by the render layers like any other room name.
fn dm_room_name(self_sub: &str, self_email: &str, peer_sub: &str, peer_email: &str) -> String {
    let self_label = if self_email.is_empty() { self_sub } else { self_email };
    let peer_label = if peer_email.is_empty() { peer_sub } else { peer_email };
    let (lo, hi) = if self_sub <= peer_sub {
        (self_label, peer_label)
    } else {
        (peer_label, self_label)
    };
    format!("{lo} \u{2194} {hi}")
}

/// Prefer the email as the human-readable actor; fall back to the subject id.
fn actor(email: &str, sub: &str) -> String {
    if email.is_empty() {
        sub.to_string()
    } else {
        email.to_string()
    }
}
