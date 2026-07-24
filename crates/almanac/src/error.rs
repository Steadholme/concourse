//! Error responses.
//!
//! Failures render a small Steadholme-styled HTML error page with the correct status code, so a
//! browser hitting the calendar always sees a coherent page (never a raw framework string).

use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    /// A submitted form was missing/invalid (e.g. an unparseable start time).
    #[error("bad_request: {0}")]
    BadRequest(String),

    /// CSRF check failed on a state-changing POST.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// The requested event/contact does not exist for this owner.
    #[error("not_found: {0}")]
    NotFound(String),

    /// Unexpected internal failure (store I/O).
    #[error("server_error: {0}")]
    Internal(String),

    /// Store data could not be read or written. Copy is deliberately generic.
    #[error("unavailable: {0}")]
    Unavailable(String),
}

impl AppError {
    fn parts(&self) -> (StatusCode, String) {
        match self {
            AppError::BadRequest(d) => (StatusCode::BAD_REQUEST, d.clone()),
            AppError::Forbidden(d) => (StatusCode::FORBIDDEN, d.clone()),
            AppError::NotFound(d) => (StatusCode::NOT_FOUND, d.clone()),
            AppError::Internal(d) => (StatusCode::INTERNAL_SERVER_ERROR, d.clone()),
            AppError::Unavailable(d) => (StatusCode::SERVICE_UNAVAILABLE, d.clone()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, detail) = self.parts();
        let title = match status {
            StatusCode::BAD_REQUEST => "Bad request",
            StatusCode::FORBIDDEN => "Forbidden",
            StatusCode::NOT_FOUND => "Not found",
            _ => "Something went wrong",
        };
        let body = crate::render::error_page(status.as_u16(), title, &detail);
        (status, Html(body)).into_response()
    }
}

/// Store failures are logged server-side and collapse to one non-sensitive unavailable state.
impl From<crate::store::StoreError> for AppError {
    fn from(e: crate::store::StoreError) -> Self {
        tracing::error!(error = %e, "Almanac store operation failed");
        AppError::Unavailable(
            "Calendar data is unavailable right now. Nothing shown is current.".to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_failures_do_not_expose_backend_detail() {
        let error = AppError::from(crate::store::StoreError::Backend(
            "postgres://secret@internal.example".to_string(),
        ));
        let (status, detail) = error.parts();

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            detail,
            "Calendar data is unavailable right now. Nothing shown is current."
        );
        assert!(!detail.contains("secret"));
        assert!(!detail.contains("internal.example"));
    }
}
