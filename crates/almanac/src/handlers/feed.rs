//! Read-only iCalendar endpoints.
//!
//! - `GET /calendar.ics` — the caller's whole calendar as a subscribe-able VCALENDAR feed (add the
//!   URL in Google/Apple Calendar for a read-only mirror).
//! - `GET /event/{id}/ics` — a single event's `.ics` download.
//!
//! Both are SSO (owner scoped by the gateway `X-Auth-Subject`) and emit `text/calendar` with an
//! attachment disposition. CalDAV write (`PROPFIND`/`PUT`) is a noted future enhancement.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};

use crate::auth;
use crate::calendar;
use crate::error::AppError;
use crate::{ics, now_ms, AppState};

/// The host used for VEVENT UID domains.
const CAL_HOST: &str = "cal.w33d.xyz";

/// `GET /calendar.ics` — the whole calendar as a subscription feed.
pub async fn calendar_ics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let owner = auth::owner_subject(&headers);
    let off = calendar::tz_offset_minutes(&state.store.get_settings(&owner).await?.timezone);
    let events = state.store.list_events(&owner).await?;
    let body = ics::calendar_feed(&events, off, now_ms(), CAL_HOST);
    Ok(ics_response(body, "calendar.ics"))
}

/// `GET /event/{id}/ics` — one event's `.ics`.
pub async fn event_ics(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let owner = auth::owner_subject(&headers);
    let off = calendar::tz_offset_minutes(&state.store.get_settings(&owner).await?.timezone);
    let event = state
        .store
        .get_event(&owner, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("That event does not exist.".to_string()))?;
    let body = ics::event_ics(&event, off, now_ms(), CAL_HOST);
    Ok(ics_response(body, "event.ics"))
}

/// Wrap an ICS string in a `text/calendar` attachment response.
fn ics_response(body: String, filename: &str) -> Response {
    let mut resp = body.into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/calendar; charset=utf-8"),
    );
    if let Ok(cd) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, cd);
    }
    resp
}
