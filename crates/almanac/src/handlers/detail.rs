//! `GET /event/{id}` — the per-event detail page (SSO, owner-scoped).
//!
//! Shows the event, its attendees with their RSVP status + the shareable per-attendee RSVP link,
//! and its reminders, plus links to edit the event and download its `.ics`. Every remote/user
//! string is HTML-escaped on the way out.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Response};

use crate::auth;
use crate::calendar;
use crate::error::AppError;
use crate::render::{self, esc, layout, status_pill};
use crate::store::{Attendee, Reminder};
use crate::AppState;

/// The public host the shareable RSVP links point at.
const CAL_HOST: &str = "cal.w33d.xyz";

pub async fn show(
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
    let attendees = state.store.list_attendees(&owner, &id).await?;
    let reminders = state.store.list_reminders(&owner, &id).await?;

    let when = calendar::fmt_event_when_at(event.starts_at, event.ends_at, event.all_day, off);
    let recur = if event.rrule.trim().is_empty() {
        String::new()
    } else {
        "<span class=\"pill pill-accent\">Repeats ↻</span>".to_string()
    };

    let meta = {
        let mut rows = String::new();
        if !event.location.trim().is_empty() {
            rows.push_str(&format!(
                "<div class=\"detail__row\"><span class=\"detail__k\">Location</span><span class=\"detail__v\">{}</span></div>",
                esc(&event.location)
            ));
        }
        if !event.notes.trim().is_empty() {
            rows.push_str(&format!(
                "<div class=\"detail__row\"><span class=\"detail__k\">Notes</span><span class=\"detail__v\">{}</span></div>",
                esc(&event.notes)
            ));
        }
        rows
    };

    let content = format!(
        "{subnav}\
         <div class=\"page-head\">\
           <div><h1>{title}</h1><p class=\"muted\">{when} {recur}</p></div>\
           <div class=\"page-head__actions\">\
             <a class=\"btn btn-secondary btn-sm\" href=\"/\">Back</a>\
             <a class=\"btn btn-secondary btn-sm\" href=\"/event/{id}/ics\">Download .ics</a>\
             <a class=\"btn btn-primary btn-sm\" href=\"/edit/{id}\">Edit</a>\
           </div>\
         </div>\
         <section class=\"card detail\">{meta}</section>\
         {attendees}\
         {reminders}\
         <p class=\"almanac-foot\">HOLDFAST Almanac · <a href=\"/calendar.ics\">Subscribe to this calendar</a> (read-only .ics)</p>",
        subnav = render::subnav("calendar"),
        title = esc(&event.title),
        when = esc(&when),
        recur = recur,
        id = esc(&id),
        meta = if meta.is_empty() {
            "<p class=\"muted\">No location or notes.</p>".to_string()
        } else {
            meta
        },
        attendees = render_attendees(&attendees, &id),
        reminders = render_reminders(&reminders),
    );

    Ok(Html(layout("Event", &headers, &content)).into_response())
}

fn render_attendees(attendees: &[Attendee], event_id: &str) -> String {
    let head = format!(
        "<div class=\"almanac-card__head\"><h2>Attendees</h2><span class=\"muted\">{n} invited</span></div>",
        n = attendees.len()
    );
    if attendees.is_empty() {
        return format!(
            "<section class=\"card\">{head}<div class=\"list-empty\">No attendees yet. Add them from the \
             <a href=\"/edit/{id}\">event editor</a>.</div></section>",
            head = head,
            id = esc(event_id),
        );
    }
    let rows: String = attendees.iter().map(render_attendee_row).collect();
    format!(
        "<section class=\"card\">{head}\
           <table class=\"data\">\
             <thead><tr><th>Name</th><th>Email</th><th>Status</th><th>RSVP link</th></tr></thead>\
             <tbody>{rows}</tbody>\
           </table>\
         </section>",
        head = head,
        rows = rows,
    )
}

fn render_attendee_row(a: &Attendee) -> String {
    let name = if a.name.trim().is_empty() {
        "<span class=\"muted\">—</span>".to_string()
    } else {
        esc(&a.name)
    };
    // The token is hex (safe) but escaped defensively; shown as a full shareable URL.
    let link = format!("https://{CAL_HOST}/rsvp/{}", esc(&a.token));
    format!(
        "<tr>\
           <td class=\"strong\">{name}</td>\
           <td>{email}</td>\
           <td>{status}</td>\
           <td><a class=\"rsvp-link\" href=\"/rsvp/{token}\">{link}</a></td>\
         </tr>",
        name = name,
        email = esc(&a.email),
        status = status_pill(&a.status),
        token = esc(&a.token),
        link = link,
    )
}

fn render_reminders(reminders: &[Reminder]) -> String {
    let head = "<div class=\"almanac-card__head\"><h2>Reminders</h2></div>";
    if reminders.is_empty() {
        return format!(
            "<section class=\"card\">{head}<div class=\"list-empty\">No reminders set.</div></section>"
        );
    }
    let chips: String = reminders
        .iter()
        .map(|r| format!("<span class=\"pill pill-info\">{}</span>", esc(&reminder_label(r.minutes_before))))
        .collect();
    format!(
        "<section class=\"card\">{head}<div class=\"reminder-chips\">{chips}</div></section>",
        head = head,
        chips = chips,
    )
}

/// Human label for a reminder's lead time.
fn reminder_label(minutes: i64) -> String {
    match minutes {
        0 => "At time of event".to_string(),
        1440 => "1 day before".to_string(),
        m if m % 60 == 0 && m >= 60 => {
            let h = m / 60;
            format!("{h} hour{} before", if h == 1 { "" } else { "s" })
        }
        m => format!("{m} minutes before"),
    }
}
