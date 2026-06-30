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

    /// Authenticated, but not a member of the targeted room.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// No such room / message.
    #[error("not_found: {0}")]
    NotFound(String),

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
            AppError::Unauthorized(d) => {
                (StatusCode::UNAUTHORIZED, "unauthorized", d.clone(), true)
            }
            AppError::Forbidden(d) => (StatusCode::FORBIDDEN, "forbidden", d.clone(), false),
            AppError::NotFound(d) => (StatusCode::NOT_FOUND, "not_found", d.clone(), false),
            AppError::Internal(d) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "server_error", d.clone(), false)
            }
        }
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

/// Store failures collapse to their HTTP shape: everything is an internal 500 (Murmur has no
/// user-facing uniqueness conflicts — joins/room creates are idempotent via `ON CONFLICT`).
impl From<crate::store::StoreError> for AppError {
    fn from(e: crate::store::StoreError) -> Self {
        AppError::Internal(e.to_string())
    }
}
