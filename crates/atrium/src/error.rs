//! Error type + responses.
//!
//! Atrium is a pure read aggregator: the dashboard NEVER errors on a down source (it degrades
//! that column to "unavailable" and renders the rest). This small enum exists only for the few
//! defensive failure paths (no gateway identity) and renders a branded HTML error page, mirroring
//! the keystone/inkwell/cortex error seam.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    /// No gateway-injected identity (defense in depth behind the SSO gateway).
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// Unexpected internal failure.
    #[error("server_error: {0}")]
    Internal(String),
}

impl AppError {
    fn parts(&self) -> (StatusCode, String, bool) {
        match self {
            AppError::Unauthorized(d) => (StatusCode::UNAUTHORIZED, d.clone(), true),
            AppError::Internal(d) => (StatusCode::INTERNAL_SERVER_ERROR, d.clone(), false),
        }
    }
}

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
