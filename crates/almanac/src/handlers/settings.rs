//! Per-owner display settings: timezone + week-start.
//!
//! `GET /settings` renders the form pre-filled with the owner's saved preferences (or the UTC /
//! Sunday-first defaults when nothing is stored yet). `POST /settings` validates the submission
//! against a fixed allow-list, upserts the single per-owner row, emits a value-free audit record,
//! and redirects back. Like every other write in the crate the POST is double-submit CSRF checked;
//! the OWNER is always the gateway-injected `X-Auth-Subject`, never a form field.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;

use crate::auth;
use crate::error::AppError;
use crate::handlers::events::require_csrf;
use crate::handlers::html_with_csrf_cookie;
use crate::render::{self, esc, layout};
use crate::store::Settings;
use crate::{now_ms, AppState};

/// Selectable timezones. Value = the stored string (parsed to a fixed UTC offset by
/// [`crate::calendar::tz_offset_minutes`]); label = what the owner sees. Curated so the stored
/// value is always one this list recognises (no free-text tz to sanitise).
const TIMEZONES: &[(&str, &str)] = &[
    ("UTC-12:00", "UTC-12:00"),
    ("UTC-10:00", "UTC-10:00 · Hawaii"),
    ("UTC-08:00", "UTC-08:00 · US Pacific"),
    ("UTC-07:00", "UTC-07:00 · US Mountain"),
    ("UTC-06:00", "UTC-06:00 · US Central"),
    ("UTC-05:00", "UTC-05:00 · US Eastern"),
    ("UTC-03:00", "UTC-03:00 · Buenos Aires"),
    ("UTC", "UTC"),
    ("UTC+01:00", "UTC+01:00 · Central Europe"),
    ("UTC+02:00", "UTC+02:00 · Eastern Europe"),
    ("UTC+03:00", "UTC+03:00 · Moscow"),
    ("UTC+05:30", "UTC+05:30 · India"),
    ("UTC+08:00", "UTC+08:00 · China / Singapore"),
    ("UTC+09:00", "UTC+09:00 · Japan / Korea"),
    ("UTC+10:00", "UTC+10:00 · Sydney"),
    ("UTC+12:00", "UTC+12:00 · Auckland"),
];

/// The settings form body. Owner is NEVER taken from here.
#[derive(Debug, Deserialize)]
pub struct SettingsForm {
    #[serde(default)]
    pub csrf_token: String,
    #[serde(default)]
    pub timezone: String,
    #[serde(default)]
    pub week_start: String,
}

// ---------------------------------------------------------------------------
// GET /settings  — the settings form
// ---------------------------------------------------------------------------

pub async fn index(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AppError> {
    let owner = auth::owner_subject(&headers);
    let settings = state.store.get_settings(&owner).await?;
    let csrf = auth::new_csrf_token();

    let content = format!(
        "{}{}",
        render::subnav("settings"),
        render_settings_form(&settings, &csrf)
    );
    Ok(html_with_csrf_cookie(
        layout("Settings", &headers, &content),
        &csrf,
    ))
}

fn render_settings_form(s: &Settings, csrf: &str) -> String {
    let tz_options: String = TIMEZONES
        .iter()
        .map(|(value, label)| {
            format!(
                "<option value=\"{value}\"{sel}>{label}</option>",
                value = esc(value),
                sel = if *value == s.timezone { " selected" } else { "" },
                label = esc(label),
            )
        })
        .collect();

    let monday = s.week_start.eq_ignore_ascii_case("monday");
    format!(
        "<form class=\"card editor\" method=\"post\" action=\"/settings\">\
           <div class=\"editor__head\"><h1>Settings</h1></div>\
           <p class=\"muted\">Choose the timezone your events are shown in and which day the week \
            starts on. These apply across the calendar grid, the agenda and the event editor.</p>\
           <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
           <div class=\"editor__field\">\
             <label for=\"timezone\">Timezone</label>\
             <select id=\"timezone\" name=\"timezone\">{tz_options}</select>\
           </div>\
           <div class=\"editor__field\">\
             <label for=\"week_start\">Week starts on</label>\
             <select id=\"week_start\" name=\"week_start\">\
               <option value=\"sunday\"{sun}>Sunday</option>\
               <option value=\"monday\"{mon}>Monday</option>\
             </select>\
           </div>\
           <div class=\"editor__actions\">\
             <a class=\"btn btn-secondary\" href=\"/\">Cancel</a>\
             <button class=\"btn btn-primary\" type=\"submit\">Save settings</button>\
           </div>\
         </form>",
        csrf = esc(csrf),
        tz_options = tz_options,
        sun = if monday { "" } else { " selected" },
        mon = if monday { " selected" } else { "" },
    )
}

// ---------------------------------------------------------------------------
// POST /settings  — save the owner's preferences
// ---------------------------------------------------------------------------

pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SettingsForm>,
) -> Result<Response, AppError> {
    require_csrf(&headers, &form.csrf_token)?;
    let owner = auth::owner_subject(&headers);

    // Validate against the allow-lists; an unrecognised value falls back to the safe default so a
    // hand-crafted POST can never store an unknown timezone/week-start string.
    let timezone = normalize_timezone(&form.timezone);
    let week_start = normalize_week_start(&form.week_start);

    state
        .store
        .upsert_settings(Settings {
            owner_sub: owner,
            timezone,
            week_start,
            updated_at: now_ms(),
        })
        .await?;

    // Audit: value-free notice of a successful mutation (no timezone/identity payload).
    tracing::info!(target: "audit", event = "settings.updated", "owner settings saved");

    Ok(Redirect::to("/settings").into_response())
}

/// Snap the submitted timezone to a known value, else `UTC`.
fn normalize_timezone(submitted: &str) -> String {
    let t = submitted.trim();
    if TIMEZONES.iter().any(|(value, _)| *value == t) {
        t.to_string()
    } else {
        "UTC".to_string()
    }
}

/// Only `sunday` / `monday` are valid; anything else snaps to `sunday`.
fn normalize_week_start(submitted: &str) -> String {
    if submitted.trim().eq_ignore_ascii_case("monday") {
        "monday".to_string()
    } else {
        "sunday".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_against_allow_lists() {
        assert_eq!(normalize_timezone("UTC+08:00"), "UTC+08:00");
        assert_eq!(normalize_timezone("Europe/Paris"), "UTC", "unknown => UTC");
        assert_eq!(normalize_timezone(" UTC-05:00 "), "UTC-05:00", "trimmed");
        assert_eq!(normalize_week_start("monday"), "monday");
        assert_eq!(normalize_week_start("MONDAY"), "monday");
        assert_eq!(normalize_week_start("whatever"), "sunday");
    }
}
