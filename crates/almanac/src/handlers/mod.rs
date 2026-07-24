//! HTTP handlers.
//!
//! - [`health`] — unauthenticated liveness probe (`/healthz`).
//! - [`events`] — the calendar: month grid + agenda, event create/edit/delete, all server-
//!   rendered and scoped to the Sluice-injected `X-Auth-Subject` owner.
//! - [`contacts`] — the address book: list + add/edit/delete, same owner scoping.
//!
//! Shared helpers ([`html_with_csrf_cookie`], the 404 [`not_found`] fallback) live here.

pub mod contacts;
pub mod detail;
pub mod events;
pub mod feed;
pub mod health;
pub mod rsvp;
pub mod settings;

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::{
    extract::{
        rejection::{FormRejection, PathRejection, QueryRejection},
        Path, Query,
    },
    Form,
};

use crate::auth;
use crate::error::AppError;
use crate::render::layout;

/// Recover Axum extractor failures into product-owned safe error pages. Rejection
/// text is deliberately discarded because it may contain framework internals or
/// reflected request material.
pub(crate) fn query_or_invalid<T>(result: Result<Query<T>, QueryRejection>) -> Result<T, AppError> {
    result
        .map(|Query(value)| value)
        .map_err(|_| AppError::BadRequest("Check the URL parameters and try again.".to_string()))
}

pub(crate) fn form_or_invalid<T>(result: Result<Form<T>, FormRejection>) -> Result<T, AppError> {
    result
        .map(|Form(value)| value)
        .map_err(|_| AppError::BadRequest("Check the submitted form and try again.".to_string()))
}

pub(crate) fn path_or_invalid<T>(result: Result<Path<T>, PathRejection>) -> Result<T, AppError> {
    result
        .map(|Path(value)| value)
        .map_err(|_| AppError::BadRequest("Check the requested path and try again.".to_string()))
}

/// Build an HTML response that also sets the double-submit CSRF cookie.
pub fn html_with_csrf_cookie(body: String, csrf: &str) -> Response {
    let mut response = Html(body).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&auth::csrf_cookie(csrf)) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

/// Router fallback: a Steadholme-styled 404 for any unmatched route.
pub async fn not_found(headers: HeaderMap) -> Response {
    let content = "<section class=\"card empty-state state state--not-found\">\
           <div class=\"empty-state__code\">404</div>\
           <h1>Not found</h1>\
           <p class=\"muted\">There is no such page. The calendar lives at <code>/</code> and the \
            address book at <code>/contacts</code>.</p>\
           <a class=\"btn btn-primary\" href=\"/\">Back to the calendar</a>\
         </section>";
    (
        StatusCode::NOT_FOUND,
        Html(layout("Not found", &headers, content)),
    )
        .into_response()
}
