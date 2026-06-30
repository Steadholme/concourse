//! Error type + responses.
//!
//! The SSO UI paths render a small branded HTML error page; the JSON ingest path returns a compact
//! JSON error envelope. 401s additionally carry `WWW-Authenticate`. One enum mirrors the
//! inkwell/sanctum error seam.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    /// Malformed/incomplete input (missing source/title, etc.).
    #[error("invalid_request: {0}")]
    InvalidRequest(String),

    /// No gateway-injected identity, a bad ingest token, or a failed CSRF check.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// No such resource.
    #[error("not_found: {0}")]
    NotFound(String),

    /// Unexpected internal failure (store I/O).
    #[error("server_error: {0}")]
    Internal(String),
}

impl AppError {
    fn parts(&self) -> (StatusCode, String, bool) {
        match self {
            AppError::InvalidRequest(d) => (StatusCode::BAD_REQUEST, d.clone(), false),
            AppError::Unauthorized(d) => (StatusCode::UNAUTHORIZED, d.clone(), true),
            AppError::NotFound(d) => (StatusCode::NOT_FOUND, d.clone(), false),
            AppError::Internal(d) => (StatusCode::INTERNAL_SERVER_ERROR, d.clone(), false),
        }
    }

    /// JSON error response for the machine (ingest) paths.
    pub fn into_json(self) -> Response {
        let (status, description, www_authenticate) = self.parts();
        let mut response = (status, Json(json!({ "error": description }))).into_response();
        if www_authenticate {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

/// Default (HTML) rendering — used by the SSO UI paths.
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, description, www_authenticate) = self.parts();
        let body = crate::handlers::error_page(status, &description);
        let mut response = (status, Html(body)).into_response();
        if www_authenticate {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

/// Store failures collapse to a 500.
impl From<crate::store::StoreError> for AppError {
    fn from(e: crate::store::StoreError) -> Self {
        match e {
            crate::store::StoreError::Backend(m) => AppError::Internal(m),
        }
    }
}
