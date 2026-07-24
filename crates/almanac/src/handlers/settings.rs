//! Per-owner display settings: timezone + week-start.
//!
//! `GET /settings` renders the form pre-filled with the owner's saved preferences (or the UTC /
//! Sunday-first defaults when nothing is stored yet). `POST /settings` validates the submission
//! against a fixed allow-list, upserts the single per-owner row, emits a value-free audit record,
//! and redirects back. Like every other write in the crate the POST is double-submit CSRF checked;
//! the OWNER is always the gateway-injected `X-Auth-Subject`, never a form field.

use axum::extract::{rejection::FormRejection, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;

use crate::auth;
use crate::error::AppError;
use crate::handlers::events::require_csrf;
use crate::handlers::{form_or_invalid, html_with_csrf_cookie};
use crate::render::{self, esc, layout};
use crate::store::Settings;
use crate::{now_ms, AppState};

/// Selectable timezones. Value = the stored string (parsed to a fixed UTC offset by
/// [`crate::calendar::tz_offset_minutes`]); label = what the owner sees. Curated so the stored
/// value is always one this list recognises (no free-text tz to sanitise).
const TIMEZONES: &[(&str, &str)] = &[
    ("UTC-12:00", "UTC-12:00"),
    ("UTC-10:00", "UTC-10:00"),
    ("UTC-08:00", "UTC-08:00"),
    ("UTC-07:00", "UTC-07:00"),
    ("UTC-06:00", "UTC-06:00"),
    ("UTC-05:00", "UTC-05:00"),
    ("UTC-03:00", "UTC-03:00"),
    ("UTC", "UTC"),
    ("UTC+01:00", "UTC+01:00"),
    ("UTC+02:00", "UTC+02:00"),
    ("UTC+03:00", "UTC+03:00"),
    ("UTC+05:30", "UTC+05:30"),
    ("UTC+08:00", "UTC+08:00"),
    ("UTC+09:00", "UTC+09:00"),
    ("UTC+10:00", "UTC+10:00"),
    ("UTC+12:00", "UTC+12:00"),
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

#[derive(Clone, Copy, Debug, Default)]
struct SettingsErrors {
    timezone: bool,
    week_start: bool,
}

impl SettingsErrors {
    fn any(self) -> bool {
        self.timezone || self.week_start
    }
}

// ---------------------------------------------------------------------------
// GET /settings  — the settings form
// ---------------------------------------------------------------------------

pub async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let owner = auth::owner_subject(&headers);
    let settings = state.store.get_settings(&owner).await?;
    let csrf = auth::new_csrf_token();

    let content = format!(
        "{}<div class=\"view-settings\">{}</div>",
        render::subnav("settings"),
        render_settings_form(&settings, &csrf, SettingsErrors::default())
    );
    Ok(html_with_csrf_cookie(
        layout("Settings", &headers, &content),
        &csrf,
    ))
}

fn render_settings_form(s: &Settings, csrf: &str, errors: SettingsErrors) -> String {
    let mut tz_options: String = TIMEZONES
        .iter()
        .map(|(value, label)| {
            format!(
                "<option value=\"{value}\"{sel}>{label}</option>",
                value = esc(value),
                sel = if *value == s.timezone {
                    " selected"
                } else {
                    ""
                },
                label = esc(label),
            )
        })
        .collect();
    if errors.timezone {
        tz_options.insert_str(
            0,
            &format!(
                "<option value=\"{}\" selected>Unsupported selection: {}</option>",
                esc(&s.timezone),
                esc(&s.timezone),
            ),
        );
    }

    let monday = s.week_start == "monday";
    let unknown_week_start = errors.week_start.then(|| {
        format!(
            "<option value=\"{}\" selected>Unsupported selection: {}</option>",
            esc(&s.week_start),
            esc(&s.week_start),
        )
    });
    let summary = render_settings_error_summary(errors);
    let timezone_error = errors.timezone.then_some(
        "<p class=\"field-error\" id=\"timezone-error\">Choose one of the listed fixed UTC offsets.</p>"
    );
    let week_start_error = errors
        .week_start
        .then_some("<p class=\"field-error\" id=\"week-start-error\">Choose Sunday or Monday.</p>");
    format!(
        "<form class=\"card editor\" method=\"post\" action=\"/settings\">\
           <div class=\"editor__head\"><h1>Settings</h1></div>\
           <p class=\"muted\">Choose the fixed UTC offset your events are shown in and which day \
            the week starts on. This is a fixed offset, not a daylight-saving timezone.</p>\
           {summary}\
           <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
           <div class=\"editor__field\">\
             <label for=\"timezone\">Fixed UTC offset</label>\
             <select id=\"timezone\" name=\"timezone\"{timezone_aria}>{tz_options}</select>\
             {timezone_error}\
           </div>\
           <div class=\"editor__field\">\
             <label for=\"week_start\">Week starts on</label>\
             <select id=\"week_start\" name=\"week_start\"{week_start_aria}>\
               {unknown_week_start}\
               <option value=\"sunday\"{sun}>Sunday</option>\
               <option value=\"monday\"{mon}>Monday</option>\
             </select>\
             {week_start_error}\
           </div>\
           <div class=\"editor__actions\">\
             <a class=\"btn btn-secondary\" href=\"/\">Cancel</a>\
             <button class=\"btn btn-primary\" type=\"submit\">Save settings</button>\
           </div>\
         </form>",
        csrf = esc(csrf),
        summary = summary,
        tz_options = tz_options,
        timezone_aria = if errors.timezone {
            " aria-invalid=\"true\" aria-describedby=\"timezone-error\""
        } else {
            ""
        },
        timezone_error = timezone_error.unwrap_or_default(),
        unknown_week_start = unknown_week_start.unwrap_or_default(),
        sun = if !errors.week_start && !monday {
            " selected"
        } else {
            ""
        },
        mon = if monday { " selected" } else { "" },
        week_start_aria = if errors.week_start {
            " aria-invalid=\"true\" aria-describedby=\"week-start-error\""
        } else {
            ""
        },
        week_start_error = week_start_error.unwrap_or_default(),
    )
}

fn render_settings_error_summary(errors: SettingsErrors) -> String {
    if !errors.any() {
        return String::new();
    }
    let mut items = String::new();
    if errors.timezone {
        items.push_str(
            "<li><a href=\"#timezone\">Choose one of the listed fixed UTC offsets.</a></li>",
        );
    }
    if errors.week_start {
        items.push_str("<li><a href=\"#week_start\">Choose Sunday or Monday.</a></li>");
    }
    format!(
        "<section class=\"error-summary\" role=\"alert\" aria-labelledby=\"settings-errors-title\">\
           <h2 id=\"settings-errors-title\">Check your settings</h2><ul>{items}</ul>\
         </section>"
    )
}

// ---------------------------------------------------------------------------
// POST /settings  — save the owner's preferences
// ---------------------------------------------------------------------------

pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: Result<Form<SettingsForm>, FormRejection>,
) -> Result<Response, AppError> {
    let form = form_or_invalid(form)?;
    require_csrf(&headers, &form.csrf_token)?;
    let owner = auth::owner_subject(&headers);

    let timezone = form.timezone.trim().to_string();
    let week_start = form.week_start.trim().to_string();
    let errors = SettingsErrors {
        timezone: !TIMEZONES.iter().any(|(value, _)| *value == timezone),
        week_start: !matches!(week_start.as_str(), "sunday" | "monday"),
    };
    if errors.any() {
        let submitted = Settings {
            owner_sub: owner,
            timezone,
            week_start,
            updated_at: 0,
        };
        let content = format!(
            "{}<div class=\"view-settings\">{}</div>",
            render::subnav("settings"),
            render_settings_form(&submitted, &form.csrf_token, errors)
        );
        let mut response =
            html_with_csrf_cookie(layout("Settings", &headers, &content), &form.csrf_token);
        *response.status_mut() = StatusCode::BAD_REQUEST;
        return Ok(response);
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_settings_keep_values_and_link_errors() {
        let settings = Settings {
            owner_sub: "u".to_string(),
            timezone: "Europe/Paris".to_string(),
            week_start: "friday".to_string(),
            updated_at: 0,
        };
        let html = render_settings_form(
            &settings,
            "csrf",
            SettingsErrors {
                timezone: true,
                week_start: true,
            },
        );
        assert!(html.contains("value=\"Europe/Paris\" selected"));
        assert!(html.contains("value=\"friday\" selected"));
        assert!(html.contains("href=\"#timezone\""));
        assert!(html.contains("href=\"#week_start\""));
        assert!(html.contains(
            "id=\"timezone\" name=\"timezone\" aria-invalid=\"true\" aria-describedby=\"timezone-error\""
        ));
        assert!(html.contains(
            "id=\"week_start\" name=\"week_start\" aria-invalid=\"true\" aria-describedby=\"week-start-error\""
        ));
    }
}
