//! Message search + the global @mentions view (both SSO-gated, read-only).
//!
//! `search` scans message bodies with a case-insensitive substring match, but ONLY over the rooms
//! the caller is a non-banned member of — the membership scope is enforced in the store's JOIN, so
//! a non-member room's content can NEVER surface. `mentions` lists the messages that @mention the
//! caller across every room. Both are keyset-paginated (newest-first, `?before=`) and every hit
//! carries its room's display name so the client can link back to the room.

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::auth;
use crate::config::{
    MAX_SEARCH_QUERY_CHARS, MENTIONS_PAGE_LIMIT, MIN_SEARCH_QUERY_CHARS, SEARCH_PAGE_LIMIT,
};
use crate::error::AppError;
use crate::AppState;

/// Query for `GET /api/search` — the needle `q` and an optional keyset cursor.
#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: String,
    pub before: Option<i64>,
}

/// Query for `GET /api/mentions` — an optional keyset cursor.
#[derive(Debug, Deserialize)]
pub struct MentionsQuery {
    pub before: Option<i64>,
}

/// `GET /api/search?q=&before=` — search the caller's member rooms. Strictly membership-scoped:
/// results can only ever come from rooms the caller belongs to.
pub async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SearchQuery>,
) -> Result<Response, AppError> {
    let (sub, _email) = auth::require_user(&headers)?;

    let needle = q.q.trim();
    let len = needle.chars().count();
    if len < MIN_SEARCH_QUERY_CHARS {
        return Err(AppError::InvalidRequest(format!(
            "search query must be at least {MIN_SEARCH_QUERY_CHARS} characters"
        )));
    }
    if len > MAX_SEARCH_QUERY_CHARS {
        return Err(AppError::InvalidRequest("search query too long".to_string()));
    }

    let query_lower = needle.to_lowercase();
    let hits = state
        .store
        .search_user_messages(&sub, &query_lower, q.before, SEARCH_PAGE_LIMIT)
        .await;
    Ok(Json(json!({ "query": needle, "results": hits })).into_response())
}

/// `GET /api/mentions?before=` — every message that @mentions the caller, across all their rooms.
pub async fn mentions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<MentionsQuery>,
) -> Result<Response, AppError> {
    let (sub, _email) = auth::require_user(&headers)?;
    let hits = state
        .store
        .list_user_mentions(&sub, q.before, MENTIONS_PAGE_LIMIT)
        .await;
    Ok(Json(json!({ "results": hits })).into_response())
}
