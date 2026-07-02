//! `GET /rsvp/{token}` — the PUBLIC (no-SSO) per-attendee RSVP page.
//!
//! The unguessable `token` (minted per attendee) is the capability: it identifies the attendee row
//! WITHOUT any gateway identity, so an invitee can respond from a plain email link. `?reply=` sets
//! the status (`accepted` | `declined` | `tentative`); absent/invalid replies render the choice
//! page. Every remote string is HTML-escaped and the page uses the no-nav public shell.
//!
//! DEPLOY NOTE: this path must be exposed publicly (a Sluice public-path exception on
//! `cal.w33d.xyz`); the rest of the surface stays behind `auth=sso`.

use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use crate::calendar;
use crate::render::{esc, public_shell, status_pill};
use crate::store::{Attendee, ATTENDEE_STATUSES};
use crate::AppState;

/// `?reply=accepted|declined|tentative`.
#[derive(Debug, Deserialize)]
pub struct RsvpQuery {
    #[serde(default)]
    pub reply: Option<String>,
}

pub async fn rsvp(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Query(q): Query<RsvpQuery>,
) -> Response {
    // Token lookup is the ONLY non-owner-scoped read: the token IS the authorization.
    let attendee = match state.store.get_attendee_by_token(&token).await {
        Ok(Some(a)) => a,
        Ok(None) => return not_found(),
        Err(_) => return error_page(),
    };

    // A valid reply mutates the status; anything else just shows the choice page.
    let reply = q.reply.as_deref().map(str::trim).unwrap_or("").to_ascii_lowercase();
    let attendee = if is_rsvp_reply(&reply) {
        match state.store.set_attendee_status_by_token(&token, &reply).await {
            Ok(Some(updated)) => {
                // Audit: value-free notice of an RSVP mutation.
                tracing::info!(target: "audit", event = "attendee.rsvp", "attendee rsvp recorded");
                updated
            }
            Ok(None) => return not_found(),
            Err(_) => return error_page(),
        }
    } else {
        attendee
    };

    // Event context (organizer-scoped), shown for orientation; may be gone if the event was deleted.
    let when = match state
        .store
        .get_event(&attendee.owner_sub, &attendee.event_id)
        .await
    {
        Ok(Some(event)) => {
            let off = state
                .store
                .get_settings(&attendee.owner_sub)
                .await
                .map(|s| calendar::tz_offset_minutes(&s.timezone))
                .unwrap_or(0);
            Some((
                event.title.clone(),
                calendar::fmt_event_when_at(event.starts_at, event.ends_at, event.all_day, off),
            ))
        }
        _ => None,
    };

    let just_replied = is_rsvp_reply(&reply);
    Html(public_shell("RSVP", &render_page(&attendee, when.as_ref(), just_replied))).into_response()
}

/// Whether `s` is one of the three settable RSVP replies (not `needs-action`).
fn is_rsvp_reply(s: &str) -> bool {
    ATTENDEE_STATUSES.contains(&s) && s != "needs-action"
}

fn render_page(
    attendee: &Attendee,
    when: Option<&(String, String)>,
    just_replied: bool,
) -> String {
    let greeting = if attendee.name.trim().is_empty() {
        esc(&attendee.email)
    } else {
        esc(&attendee.name)
    };

    let event_block = match when {
        Some((title, whenlabel)) => format!(
            "<div class=\"rsvp__event\"><div class=\"rsvp__title\">{title}</div>\
             <div class=\"rsvp__when muted\">{whenlabel}</div></div>",
            title = esc(title),
            whenlabel = esc(whenlabel),
        ),
        None => "<p class=\"muted\">This invitation is no longer available.</p>".to_string(),
    };

    let confirm = if just_replied {
        format!(
            "<p class=\"rsvp__confirm\">Thanks, {greeting} — your response is recorded as {pill}.</p>",
            greeting = greeting,
            pill = status_pill(&attendee.status),
        )
    } else {
        format!(
            "<p class=\"muted\">Hi {greeting}, your current response is {pill}.</p>",
            greeting = greeting,
            pill = status_pill(&attendee.status),
        )
    };

    // The reply choices always render (the invitee can change their mind).
    let choices = format!(
        "<div class=\"rsvp__choices\">\
           <a class=\"btn btn-primary\" href=\"/rsvp/{token}?reply=accepted\">Accept</a>\
           <a class=\"btn btn-secondary\" href=\"/rsvp/{token}?reply=tentative\">Maybe</a>\
           <a class=\"btn btn-danger\" href=\"/rsvp/{token}?reply=declined\">Decline</a>\
         </div>",
        token = esc(&attendee.token),
    );

    format!(
        "<section class=\"card rsvp\">\
           <div class=\"rsvp__brand\">HOLDFAST Almanac</div>\
           <h1>You're invited</h1>\
           {event_block}\
           {confirm}\
           {choices}\
         </section>",
        event_block = event_block,
        confirm = confirm,
        choices = choices,
    )
}

fn not_found() -> Response {
    let body = public_shell(
        "Invitation not found",
        "<section class=\"card empty-state\">\
           <div class=\"empty-state__code\">404</div>\
           <h1>Invitation not found</h1>\
           <p class=\"muted\">This RSVP link is invalid or has expired.</p>\
         </section>",
    );
    (axum::http::StatusCode::NOT_FOUND, Html(body)).into_response()
}

fn error_page() -> Response {
    let body = public_shell(
        "Something went wrong",
        "<section class=\"card empty-state\">\
           <div class=\"empty-state__code\">500</div>\
           <h1>Something went wrong</h1>\
           <p class=\"muted\">Please try the link again shortly.</p>\
         </section>",
    );
    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Html(body)).into_response()
}
