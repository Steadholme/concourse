//! Error type + responses.
//!
//! Murmur is an API-first service (one server-rendered dashboard + JSON endpoints + a
//! WebSocket), so [`AppError`] renders a compact JSON envelope `{ "error", "message" }` with the
//! right status code. 401s additionally carry `WWW-Authenticate`. Keeping one enum mirrors the
//! inkwell/sanctum error seam.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    /// Malformed/incomplete request input (empty body, oversized name, …).
    #[error("invalid_request: {0}")]
    InvalidRequest(String),

    /// No gateway-injected identity, or a failed CSRF check.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// Double-submit CSRF verification failed.
    #[error("csrf_invalid")]
    CsrfInvalid,

    /// Authenticated, but not a member of the targeted room.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// No such room / message.
    #[error("not_found: {0}")]
    NotFound(String),

    /// The room exists and is readable, but its mutation surface is frozen.
    #[error("room_archived")]
    RoomArchived,

    /// State or invariant conflict.
    #[error("conflict: {0}")]
    Conflict(String),

    /// Backend dependency is unavailable. Detail is retained only for logs.
    #[error("unavailable: {0}")]
    Unavailable(String),

    /// Unexpected internal failure (store I/O).
    #[error("server_error: {0}")]
    Internal(String),
}

impl AppError {
    fn parts(&self) -> (StatusCode, &'static str, String, bool) {
        match self {
            AppError::InvalidRequest(d) => {
                (StatusCode::BAD_REQUEST, "invalid_request", d.clone(), false)
            }
            AppError::Unauthorized(_) => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Session required".to_string(),
                true,
            ),
            AppError::CsrfInvalid => (
                StatusCode::FORBIDDEN,
                "csrf_invalid",
                "Request could not be verified".to_string(),
                false,
            ),
            AppError::Forbidden(_) => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Action not allowed".to_string(),
                false,
            ),
            AppError::NotFound(_) => (
                StatusCode::NOT_FOUND,
                "not_found",
                "Resource unavailable".to_string(),
                false,
            ),
            AppError::RoomArchived => (
                StatusCode::CONFLICT,
                "room_archived",
                "Room is archived and read-only".to_string(),
                false,
            ),
            AppError::Conflict(_) => (
                StatusCode::CONFLICT,
                "conflict",
                "State changed; refresh and retry".to_string(),
                false,
            ),
            AppError::Unavailable(_) | AppError::Internal(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "Murmur is temporarily unavailable".to_string(),
                false,
            ),
        }
    }

    pub fn status_code(&self) -> StatusCode {
        self.parts().0
    }

    pub fn safe_message(&self) -> String {
        self.parts().2
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message, www_authenticate) = self.parts();
        let mut response =
            (status, Json(json!({ "error": code, "message": message }))).into_response();
        if www_authenticate {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

/// Store failures collapse to a fixed safe 503. Raw backend detail remains available through the
/// error's `Display` implementation for structured server logs, never in the response body.
impl From<crate::store::StoreError> for AppError {
    fn from(e: crate::store::StoreError) -> Self {
        tracing::error!(error = %e, "store operation failed");
        AppError::Unavailable(e.to_string())
    }
}

impl From<crate::store::StoreMutationError> for AppError {
    fn from(error: crate::store::StoreMutationError) -> Self {
        use crate::store::StoreMutationError;
        match error {
            StoreMutationError::Backend(detail) => {
                tracing::error!(error = %detail, "store mutation failed");
                AppError::Unavailable(detail)
            }
            StoreMutationError::ResourceUnavailable
            | StoreMutationError::RoomNotFound
            | StoreMutationError::NotMember
            | StoreMutationError::MemberBanned => {
                AppError::NotFound("resource unavailable".to_string())
            }
            StoreMutationError::RoomArchived => AppError::RoomArchived,
            StoreMutationError::Forbidden => AppError::Forbidden("action not allowed".to_string()),
            StoreMutationError::MessageDeleted => {
                AppError::InvalidRequest("message is deleted".to_string())
            }
            StoreMutationError::ReplyUnavailable => {
                AppError::InvalidRequest("reply target not found".to_string())
            }
            StoreMutationError::DirectMessageJoin => {
                AppError::InvalidRequest("direct messages cannot be joined".to_string())
            }
            StoreMutationError::ConsequenceTokenInvalid => {
                AppError::Conflict("delete authorization expired or already used".to_string())
            }
        }
    }
}
