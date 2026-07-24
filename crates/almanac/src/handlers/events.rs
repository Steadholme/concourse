//! Calendar + event handlers: the month grid with an upcoming agenda, plus event create / edit /
//! delete.
//!
//! Every request derives the OWNER from the gateway-injected `X-Auth-Subject` (via
//! [`auth::owner_subject`]) and scopes all store calls to it, so users never see or touch each
//! other's events. Every state-changing POST is double-submit CSRF checked, and all
//! user-supplied text is HTML-escaped on the way out.

use axum::extract::{
    rejection::{FormRejection, PathRejection, QueryRejection},
    Path, Query, State,
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::{Form, Json};
use serde::{Deserialize, Deserializer};

use crate::auth;
use crate::calendar::{self, DayCell, GridDay, MonthView, TimeGridView};
use crate::config::{AGENDA_HORIZON_MS, AGENDA_LIMIT, DAY_CHIP_LIMIT};
use crate::error::AppError;
use crate::handlers::{form_or_invalid, html_with_csrf_cookie, path_or_invalid, query_or_invalid};
use crate::render::{self, esc, layout};
use crate::rrule::{self, Freq};
use crate::store::{
    default_calendar_id, Attendee, AttendeeInput, Calendar, Event, EventBundle, EventException,
    DEFAULT_CALENDAR_COLOR,
};
use crate::{now_ms, quickadd, AppState};

/// Reminder presets offered as checkboxes: `(form field suffix, minutes_before, label)`.
const REMINDER_PRESETS: &[(&str, i64, &str)] = &[
    ("0", 0, "At time of event"),
    ("5", 5, "5 minutes before"),
    ("10", 10, "10 minutes before"),
    ("15", 15, "15 minutes before"),
    ("30", 30, "30 minutes before"),
    ("60", 60, "1 hour before"),
    ("120", 120, "2 hours before"),
    ("1440", 1440, "1 day before"),
];
/// Hard cap on attendees parsed from one submission.
const MAX_ATTENDEES: usize = 100;
const AGENDA_DEFAULT_DAYS: i64 = 30;
const AGENDA_MAX_DAYS: i64 = 366;
const DAY_MS: i64 = 86_400_000;
const SEARCH_LIMIT: usize = 200;

/// Pixel height of one hour row in the week/day time-grid (matches the `.tgrid` CSS).
const HOUR_PX: f64 = 48.0;
/// Pixels per minute — the vertical scale that positions timed events on the hour grid.
const PX_PER_MIN: f64 = HOUR_PX / 60.0;

/// Calendar index query. `view` selects `month` (default) / `week` / `day`; `date=YYYY-MM-DD`
/// anchors the week/day grids; `y`/`m` navigate the month grid (unchanged).
#[derive(Debug, Deserialize)]
pub struct IndexQuery {
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub y: Option<i32>,
    #[serde(default)]
    pub m: Option<u8>,
    /// Repeated `?cal=id` filters visible calendars. Empty means all calendars.
    #[serde(default, deserialize_with = "deserialize_cal_filter")]
    pub cal: Vec<String>,
    /// Agenda/list horizon in days (`?view=agenda&days=N`), clamped to a bounded range.
    #[serde(default)]
    pub days: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default, deserialize_with = "deserialize_cal_filter")]
    pub cal: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CalFilter {
    One(String),
    Many(Vec<String>),
}

fn deserialize_cal_filter<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match Option::<CalFilter>::deserialize(deserializer)? {
        Some(CalFilter::One(one)) => vec![one],
        Some(CalFilter::Many(many)) => many,
        None => Vec::new(),
    })
}

/// Which calendar view the index renders.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewKind {
    Month,
    Week,
    Day,
    Agenda,
}

impl ViewKind {
    /// Parse the `?view=` keyword (anything unrecognised falls back to the month grid).
    fn parse(v: Option<&str>) -> ViewKind {
        match v.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("week") => ViewKind::Week,
            Some("day") => ViewKind::Day,
            Some("agenda") => ViewKind::Agenda,
            _ => ViewKind::Month,
        }
    }

    /// The `?view=` keyword (also the page title stem).
    fn key(self) -> &'static str {
        match self {
            ViewKind::Month => "month",
            ViewKind::Week => "week",
            ViewKind::Day => "day",
            ViewKind::Agenda => "agenda",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct OccurrenceQuery {
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub occurrence: Option<i64>,
}

/// `GET /new?date=YYYY-MM-DD` — the day the operator clicked to add an event.
#[derive(Debug, Deserialize)]
pub struct NewQuery {
    #[serde(default)]
    pub date: Option<String>,
}

/// The event form body. The owner is NEVER taken from here — only from the gateway header.
#[derive(Debug, Deserialize)]
pub struct EventForm {
    #[serde(default)]
    pub csrf_token: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub calendar_id: String,
    /// `datetime-local` value (`YYYY-MM-DDTHH:MM`).
    #[serde(default)]
    pub starts_at: String,
    #[serde(default)]
    pub ends_at: String,
    /// Checkbox: present (`"on"`) when all-day, absent otherwise.
    #[serde(default)]
    pub all_day: Option<String>,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub notes: String,
    /// Recurrence frequency from the compose `<select>`: ``/`daily`/`weekly`/`monthly`/`yearly`.
    #[serde(default)]
    pub repeat: String,
    /// Repeat every N periods (default 1).
    #[serde(default)]
    pub repeat_interval: String,
    /// Optional total-occurrence cap (`COUNT`).
    #[serde(default)]
    pub repeat_count: String,
    /// Optional `YYYY-MM-DD` end date (`UNTIL`).
    #[serde(default)]
    pub repeat_until: String,
    /// Attendees, one per line: `Name <email@host>` or `email@host`.
    #[serde(default)]
    pub attendees: String,
    // Reminder preset checkboxes (`"on"` when ticked). Distinct fields (not a repeated key) so
    // `serde_urlencoded` deserializes them without a Vec.
    #[serde(default)]
    pub rem_0: Option<String>,
    #[serde(default)]
    pub rem_5: Option<String>,
    #[serde(default)]
    pub rem_10: Option<String>,
    #[serde(default)]
    pub rem_15: Option<String>,
    #[serde(default)]
    pub rem_30: Option<String>,
    #[serde(default)]
    pub rem_60: Option<String>,
    #[serde(default)]
    pub rem_120: Option<String>,
    #[serde(default)]
    pub rem_1440: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CalendarForm {
    #[serde(default)]
    pub csrf_token: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub color: String,
}

/// A bare CSRF-only body, used by the delete forms.
#[derive(Debug, Deserialize)]
pub struct DeleteForm {
    #[serde(default)]
    pub csrf_token: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuickAddIntent {
    #[default]
    Review,
    Commit,
    OpenEditor,
}

/// Shared quick-add input for both the HTML form and JSON endpoint.
#[derive(Debug, Deserialize)]
pub struct QuickAddInput {
    #[serde(default)]
    pub csrf_token: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub calendar_id: String,
    #[serde(default)]
    pub intent: QuickAddIntent,
    #[serde(default)]
    pub reviewed_title: Option<String>,
    #[serde(default)]
    pub reviewed_starts_at: Option<i64>,
}

// ---------------------------------------------------------------------------
// GET /  — month grid + agenda
// ---------------------------------------------------------------------------

pub async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<IndexQuery>, QueryRejection>,
) -> Result<Response, AppError> {
    let q = query_or_invalid(query)?;
    let owner = auth::owner_subject(&headers);
    let settings = state.store.get_settings(&owner).await?;
    let off = calendar::tz_offset_minutes(&settings.timezone);
    let monday_first = settings.week_start.eq_ignore_ascii_case("monday");
    let now = now_ms();
    let calendars = state.store.list_calendars(&owner).await?;
    let selected_calendars = selected_calendar_ids(&calendars, &q.cal);
    let listing = state.store.list_events(&owner).await?;
    let source_partial = listing.has_more;
    let stored = listing.events;
    let csrf = auth::new_csrf_token();
    let kind = ViewKind::parse(q.view.as_deref());
    if matches!(kind, ViewKind::Month) && q.m.is_some_and(|month| !(1..=12).contains(&month)) {
        return Err(AppError::BadRequest(format!(
            "“{}” is not a valid calendar month. Use a month from 1 through 12.",
            q.m.unwrap_or_default()
        )));
    }
    let requested_anchor = if matches!(kind, ViewKind::Week | ViewKind::Day) {
        parse_optional_query_date(q.date.as_deref(), "calendar", off)?
    } else {
        None
    };

    let inner = match kind {
        ViewKind::Month => {
            let (cur_y, cur_m) = calendar::month_of_at(now, off);
            let year = q.y.unwrap_or(cur_y);
            let month = q.m.unwrap_or(cur_m);
            let view = calendar::build_month_at(year, month, now, off, monday_first);
            // Expand recurring series into the concrete occurrences visible in this render: the
            // month grid's span plus the upcoming-agenda horizon. One-offs pass through unchanged.
            let (grid_start, grid_end) = grid_bounds(&view);
            let win_start = grid_start.min(now);
            let win_end = grid_end.max(now + AGENDA_HORIZON_MS);
            let expanded = rrule::expand_events(&stored, win_start, win_end);
            let events = filter_events_by_calendar(expanded, &selected_calendars);
            render_calendar(
                &view,
                &events,
                &calendars,
                MonthRenderContext {
                    now,
                    csrf: &csrf,
                    off,
                    monday_first,
                    timezone: &settings.timezone,
                    source_partial,
                },
            )
        }
        ViewKind::Week | ViewKind::Day => {
            // Anchor on `?date=` (owner-local), else today.
            let anchor = requested_anchor.unwrap_or(now);
            let grid = match kind {
                ViewKind::Week => calendar::build_week_at(anchor, now, off, monday_first),
                _ => calendar::build_day_at(anchor, now, off),
            };
            // Expand only across the grid's own span — the same reused event fetch + RRULE expander.
            let expanded = rrule::expand_events(&stored, grid.win_start, grid.win_end);
            let events = filter_events_by_calendar(expanded, &selected_calendars);
            render_time_grid(
                &grid,
                &events,
                &calendars,
                now,
                off,
                kind,
                &settings.timezone,
            )
        }
        ViewKind::Agenda => {
            let days = q
                .days
                .unwrap_or(AGENDA_DEFAULT_DAYS)
                .clamp(1, AGENDA_MAX_DAYS);
            let win_end = now + days * DAY_MS;
            let expanded = rrule::expand_events(&stored, now, win_end);
            let events = filter_events_by_calendar(expanded, &selected_calendars);
            render_agenda_view(
                &events,
                &calendars,
                AgendaRenderContext {
                    now,
                    csrf: &csrf,
                    off,
                    timezone: &settings.timezone,
                    days,
                    source_partial,
                },
            )
        }
    };

    let content = format!(
        "{subnav}<div class=\"view-{view} cal-layout\">\
           <div class=\"cal-main\">{completeness}{bench}{pane}</div>\
           {rail}\
         </div>{enhancer}",
        subnav = render::subnav("calendar"),
        view = kind.key(),
        completeness = event_listing_notice(source_partial),
        bench = render_quick_add(&csrf, &calendars),
        pane = inner,
        rail = render_calendar_sidebar(&calendars, &selected_calendars, &q, kind, &csrf),
        enhancer = QUICKADD_ENHANCER,
    );
    let title = match kind {
        ViewKind::Week => "Week",
        ViewKind::Day => "Day",
        ViewKind::Agenda => "Agenda",
        ViewKind::Month => "Calendar",
    };
    let html = layout(title, &headers, &content);
    Ok(html_with_csrf_cookie(html, &csrf))
}

pub async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<SearchQuery>, QueryRejection>,
) -> Result<Response, AppError> {
    let q = query_or_invalid(query)?;
    let owner = auth::owner_subject(&headers);
    let settings = state.store.get_settings(&owner).await?;
    let off = calendar::tz_offset_minutes(&settings.timezone);
    let now = now_ms();
    let calendars = state.store.list_calendars(&owner).await?;
    let selected_calendars = selected_calendar_ids(&calendars, &q.cal);
    let listing = state.store.list_events(&owner).await?;
    let source_partial = listing.has_more;
    let stored = listing.events;
    let csrf = auth::new_csrf_token();
    let needle = q.q.as_deref().unwrap_or("").trim().to_lowercase();
    let win_start = parse_optional_query_date(q.from.as_deref(), "search start", off)?
        .unwrap_or(now - 365 * DAY_MS);
    let win_end = parse_optional_query_date(q.to.as_deref(), "search end", off)?
        .map(|t| t + DAY_MS)
        .unwrap_or(now + 365 * DAY_MS);
    if win_start >= win_end {
        return Err(AppError::BadRequest(
            "The search start date must be before or equal to the end date.".to_string(),
        ));
    }

    let mut results = if needle.is_empty() {
        Vec::new()
    } else {
        let expanded = rrule::expand_events(&stored, win_start, win_end);
        let filtered = filter_events_by_calendar(expanded, &selected_calendars);
        filtered
            .into_iter()
            .filter(|e| e.starts_at < win_end && e.ends_at >= win_start)
            .filter(|e| {
                e.title.to_lowercase().contains(&needle)
                    || e.location.to_lowercase().contains(&needle)
                    || e.notes.to_lowercase().contains(&needle)
            })
            .take(SEARCH_LIMIT + 1)
            .collect::<Vec<_>>()
    };
    let search_partial = results.len() > SEARCH_LIMIT;
    results.truncate(SEARCH_LIMIT);

    let inner = render_search_results(
        &q,
        &results,
        &calendars,
        SearchRenderContext {
            csrf: &csrf,
            off,
            needle: &needle,
            result_partial: search_partial,
            source_partial,
        },
    );
    let content = format!(
        "{}<div class=\"view-search\">{}{}</div>",
        render::subnav("calendar"),
        event_listing_notice(source_partial),
        inner
    );
    let html = layout("Search", &headers, &content);
    Ok(html_with_csrf_cookie(html, &csrf))
}

fn parse_optional_query_date(
    submitted: Option<&str>,
    field: &str,
    off: i32,
) -> Result<Option<i64>, AppError> {
    let Some(submitted) = submitted.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    calendar::parse_date_at(submitted, off)
        .map(Some)
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "“{submitted}” is not a valid {field} date. Use YYYY-MM-DD."
            ))
        })
}

fn event_listing_notice(source_partial: bool) -> &'static str {
    if source_partial {
        "<section class=\"state state--partial\" role=\"status\">\
           Showing the first loaded events. Empty cells and counts describe loaded rows only; \
           more events exist outside this projection.\
         </section>"
    } else {
        ""
    }
}

/// Progressive enhancement for the typed review → commit contract. A network failure is left
/// uncertain: it never auto-retries and never falls through to another mutating submission.
const QUICKADD_ENHANCER: &str = r##"<div class="toast-host" id="toast-host" aria-live="polite" aria-atomic="true"></div>
<script>
(function () {
  var form = document.querySelector('[data-quickadd]');
  if (!form) return;
  var input = form.querySelector('input[name=text]');
  var stage = document.getElementById('bench-stage');
  var reviewButton = form.querySelector('button[type=submit]');
  var toastHost = document.getElementById('toast-host');
  var inFlight = false;
  function toast(text, kind) {
    if (!toastHost) return;
    var el = document.createElement('div');
    el.className = 'toast ' + (kind === 'err' ? 'toast--err' : 'toast--ok');
    el.setAttribute('role', 'status');
    el.textContent = String(text);
    toastHost.appendChild(el);
    requestAnimationFrame(function () { el.classList.add('is-in'); });
    setTimeout(function () {
      el.classList.remove('is-in');
      setTimeout(function () { if (el.parentNode) el.parentNode.removeChild(el); }, 200);
    }, 2600);
  }
  function value(name) {
    var el = form.querySelector('[name="' + name + '"]');
    return el ? el.value : '';
  }
  function request(payload, trigger) {
    if (inFlight) return;
    inFlight = true;
    if (reviewButton) reviewButton.disabled = true;
    if (trigger) trigger.disabled = true;
    fetch('/quick-add.json', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      credentials: 'same-origin',
      cache: 'no-store',
      body: JSON.stringify(payload)
    })
      .then(function (r) { return r.json(); })
      .then(function (data) {
        if (!data || !stage) return;
        stage.replaceChildren();
        if (data.kind === 'review') {
          var dl = document.createElement('dl'); dl.className = 'bench__review';
          [['When', data.when], ['Title', data.title], ['Calendar', data.calendar_name]].forEach(function (row) {
            var dt = document.createElement('dt'); dt.textContent = row[0];
            var dd = document.createElement('dd'); dd.textContent = row[1] || '';
            dl.appendChild(dt); dl.appendChild(dd);
          });
          var actions = document.createElement('div'); actions.className = 'bench__commit';
          var commit = document.createElement('button'); commit.type = 'button'; commit.className = 'btn btn-primary';
          commit.textContent = 'Add to calendar';
          commit.addEventListener('click', function () {
            request({
              csrf_token: value('csrf_token'), text: value('text'),
              calendar_id: value('calendar_id'), intent: 'commit',
              reviewed_title: data.title, reviewed_starts_at: data.starts_at
            }, commit);
          });
          var editor = document.createElement('button'); editor.type = 'button'; editor.className = 'btn btn-secondary';
          editor.textContent = 'Open in editor';
          editor.addEventListener('click', function () {
            var intent = form.querySelector('input[name=intent]'); intent.value = 'open_editor';
            form.submit();
          });
          actions.appendChild(commit); actions.appendChild(editor);
          stage.appendChild(dl); stage.appendChild(actions);
        } else if (data.kind === 'committed') {
          if (input) input.value = '';
          toast('Added ' + (data.title || 'event') + (data.when ? ' · ' + data.when : ''));
          var done = document.createElement('p'); done.className = 'state state--committed';
          done.textContent = 'Added to calendar.';
          stage.appendChild(done);
        } else {
          var notice = document.createElement('p');
          notice.className = 'bench__notice state state--' + (data.kind || 'unavailable');
          notice.textContent = data.message || 'Time interpretation is unavailable — retry.';
          stage.appendChild(notice);
        }
      })
      .catch(function () {
        toast('Result unknown — check the calendar before trying again.', 'err');
      })
      .finally(function () {
        inFlight = false;
        if (reviewButton) reviewButton.disabled = false;
        if (trigger) trigger.disabled = false;
      });
  }
  form.addEventListener('submit', function (ev) {
    if (value('intent') !== 'review') return;
    if (!input || !input.value.trim()) { ev.preventDefault(); return; }
    ev.preventDefault();
    request({
      csrf_token: value('csrf_token'), text: value('text'),
      calendar_id: value('calendar_id'), intent: 'review'
    }, reviewButton);
  });
})();
</script>"##;

/// Stage one of the Propagation Bench. The same form is complete without JavaScript.
fn render_quick_add(csrf: &str, calendars: &[Calendar]) -> String {
    let options: String = calendars
        .iter()
        .map(|calendar| {
            format!(
                "<option value=\"{}\">{}</option>",
                esc(&calendar.id),
                esc(&calendar.name)
            )
        })
        .collect();
    format!(
        "<section class=\"bench\" aria-labelledby=\"bench-title\">\
           <div class=\"bench__head\"><h2 id=\"bench-title\">Propagation bench</h2>\
             <p>Interpret first, then add.</p></div>\
           <form class=\"bench__raw quickadd\" method=\"post\" action=\"/quick-add\" data-quickadd>\
             <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
             <input type=\"hidden\" name=\"intent\" value=\"review\">\
             <label for=\"quickadd-text\">Quick add</label>\
             <input id=\"quickadd-text\" class=\"quickadd__input\" type=\"text\" name=\"text\" \
               autocomplete=\"off\" maxlength=\"200\" \
               placeholder=\"Lunch tomorrow 12pm\">\
             <label for=\"quickadd-calendar\">Calendar</label>\
             <select id=\"quickadd-calendar\" name=\"calendar_id\">{options}</select>\
             <button class=\"btn btn-primary\" type=\"submit\">Review</button>\
           </form>\
           <div class=\"bench__parse\" id=\"bench-stage\" aria-live=\"polite\"></div>\
         </section>",
        csrf = esc(csrf),
        options = options,
    )
}

/// The Month / Week / Day / Agenda switch pills, threading `anchor` (an owner-local `YYYY-MM-DD`) so
/// switching views keeps the reader on the same date. `active` is the currently-rendered view.
fn view_switch(active: ViewKind, anchor: &str) -> String {
    let pill = |kind: ViewKind, label: &str, href: &str| {
        let cls = if kind == active {
            "btn btn-primary btn-sm"
        } else {
            "btn btn-secondary btn-sm"
        };
        let current = if kind == active {
            " aria-current=\"page\""
        } else {
            ""
        };
        format!(
            "<a class=\"{cls}\" href=\"{}\"{current}>{label}</a>",
            esc(href)
        )
    };
    format!(
        "<span class=\"cal-viewswitch\">{}{}{}{}</span>",
        pill(ViewKind::Month, "Month", "/"),
        pill(
            ViewKind::Week,
            "Week",
            &format!("/?view=week&date={anchor}")
        ),
        pill(ViewKind::Day, "Day", &format!("/?view=day&date={anchor}")),
        pill(ViewKind::Agenda, "Agenda", "/?view=agenda"),
    )
}

fn selected_calendar_ids(calendars: &[Calendar], requested: &[String]) -> Vec<String> {
    let valid: Vec<String> = if requested.is_empty() {
        calendars.iter().map(|c| c.id.clone()).collect()
    } else {
        requested
            .iter()
            .filter(|id| calendars.iter().any(|c| c.id == **id))
            .cloned()
            .collect()
    };
    if valid.is_empty() {
        calendars.iter().map(|c| c.id.clone()).collect()
    } else {
        valid
    }
}

fn filter_events_by_calendar(events: Vec<Event>, selected: &[String]) -> Vec<Event> {
    events
        .into_iter()
        .filter(|e| selected.iter().any(|id| id == &e.calendar_id))
        .collect()
}

fn render_calendar_sidebar(
    calendars: &[Calendar],
    selected: &[String],
    q: &IndexQuery,
    kind: ViewKind,
    csrf: &str,
) -> String {
    let hidden = calendar_filter_hidden(q, kind);
    let filter_rows: String = calendars
        .iter()
        .map(|c| {
            let checked = if selected.iter().any(|id| id == &c.id) {
                " checked"
            } else {
                ""
            };
            let swatch = calendar_swatch(&c.color);
            let default_note = if c.id == default_calendar_id(&c.owner_sub) {
                "<span class=\"calendar-panel__meta\">Default</span>"
            } else {
                ""
            };
            format!(
                "<label class=\"calendar-panel__toggle\">\
                   <input type=\"checkbox\" name=\"cal\" value=\"{id}\"{checked}>\
                   <span class=\"calendar-swatch\" style=\"--cal-color:{color}\"></span>\
                   <span>{name}</span>{default_note}\
                 </label>",
                id = esc(&c.id),
                checked = checked,
                color = swatch,
                name = esc(&c.name),
                default_note = default_note,
            )
        })
        .collect();
    let manage_rows: String = calendars
        .iter()
        .map(|c| {
            let swatch = calendar_swatch(&c.color);
            let delete = if c.id == default_calendar_id(&c.owner_sub) {
                String::new()
            } else {
                format!(
                    "<form method=\"post\" action=\"/calendars/delete/{id}\" class=\"calendar-panel__delete\">\
                       <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
                       <button class=\"btn btn-ghost btn-sm\" type=\"submit\">Delete</button>\
                     </form>",
                    id = esc(&c.id),
                    csrf = esc(csrf),
                )
            };
            format!(
                "<div class=\"calendar-panel__row\">\
                   <form method=\"post\" action=\"/calendars/update/{id}\" class=\"calendar-panel__edit\">\
                     <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
                     <input class=\"calendar-panel__name\" type=\"text\" name=\"name\" value=\"{name}\" maxlength=\"80\" aria-label=\"Calendar name\">\
                     <input class=\"calendar-panel__color\" type=\"color\" name=\"color\" value=\"{color}\" aria-label=\"Calendar color\">\
                     <button class=\"btn btn-secondary btn-sm\" type=\"submit\">Save</button>\
                   </form>\
                   {delete}\
                 </div>",
                id = esc(&c.id),
                color = swatch,
                name = esc(&c.name),
                csrf = esc(csrf),
                delete = delete,
            )
        })
        .collect();
    format!(
        "<aside class=\"card calendar-panel\">\
           <div class=\"calendar-panel__head\"><h2>Calendars</h2><a class=\"btn btn-ghost btn-sm\" href=\"/search\">Search</a></div>\
           <form method=\"get\" action=\"/\" class=\"calendar-panel__filter\">{hidden}{filter_rows}\
             <button class=\"btn btn-primary btn-sm\" type=\"submit\">Apply</button>\
           </form>\
           <div class=\"calendar-panel__manage\">{manage_rows}</div>\
           <form method=\"post\" action=\"/calendars/new\" class=\"calendar-panel__new\">\
             <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
             <input type=\"text\" name=\"name\" maxlength=\"80\" placeholder=\"New calendar\" aria-label=\"New calendar name\">\
             <input type=\"color\" name=\"color\" value=\"{default_color}\" aria-label=\"New calendar color\">\
             <button class=\"btn btn-secondary btn-sm\" type=\"submit\">Create</button>\
           </form>\
        </aside>",
        hidden = hidden,
        filter_rows = filter_rows,
        manage_rows = manage_rows,
        csrf = esc(csrf),
        default_color = DEFAULT_CALENDAR_COLOR,
    )
}

fn calendar_filter_hidden(q: &IndexQuery, kind: ViewKind) -> String {
    let mut out = String::new();
    if kind != ViewKind::Month {
        out.push_str(&format!(
            "<input type=\"hidden\" name=\"view\" value=\"{}\">",
            kind.key()
        ));
    }
    if matches!(kind, ViewKind::Week | ViewKind::Day) {
        if let Some(date) = q.date.as_deref() {
            out.push_str(&format!(
                "<input type=\"hidden\" name=\"date\" value=\"{}\">",
                esc(date)
            ));
        }
    }
    if kind == ViewKind::Month {
        if let Some(y) = q.y {
            out.push_str(&format!("<input type=\"hidden\" name=\"y\" value=\"{y}\">"));
        }
        if let Some(m) = q.m {
            out.push_str(&format!("<input type=\"hidden\" name=\"m\" value=\"{m}\">"));
        }
    }
    if kind == ViewKind::Agenda {
        if let Some(days) = q.days {
            out.push_str(&format!(
                "<input type=\"hidden\" name=\"days\" value=\"{days}\">"
            ));
        }
    }
    out
}

/// A week/day time-grid: a header + nav, an all-day strip, and an hour grid with events placed by
/// their start/end. Reuses the same overlap filter + timezone helpers as the month grid.
fn render_time_grid(
    g: &TimeGridView,
    events: &[Event],
    calendars: &[Calendar],
    now: i64,
    off: i32,
    kind: ViewKind,
    tz_label: &str,
) -> String {
    let vk = kind.key();
    let prev_href = esc(&format!("/?view={vk}&date={}", g.prev_date));
    let next_href = esc(&format!("/?view={vk}&date={}", g.next_date));
    let today_href = esc(&format!("/?view={vk}"));
    let new_href = esc(&format!("/new?date={}", g.anchor_date));
    let head = format!(
        "<div class=\"cal-head\">\
           <h1>{title}</h1>\
           <div class=\"cal-nav\">\
             {switch}\
             <a class=\"btn btn-secondary btn-sm\" href=\"{prev}\" aria-label=\"Previous\">‹ Prev</a>\
             <a class=\"btn btn-secondary btn-sm\" href=\"{today}\">Today</a>\
             <a class=\"btn btn-secondary btn-sm\" href=\"{next}\" aria-label=\"Next\">Next ›</a>\
             <a class=\"btn btn-primary btn-sm\" href=\"{new}\">New event</a>\
           </div>\
         </div>",
        title = esc(&g.title),
        switch = view_switch(kind, &g.anchor_date),
        prev = prev_href,
        today = today_href,
        next = next_href,
        new = new_href,
    );

    // Both the header row and the body use the same column template so the columns line up:
    // a fixed hour-gutter column plus one flexible column per day.
    let cols_style = format!("--days:{}", g.days.len());

    // Day headers + the all-day strip (both share the header grid).
    let mut dayheads = String::new();
    let mut alldays = String::new();
    for d in &g.days {
        let day_href = esc(&format!("/?view=day&date={}", d.iso_date));
        dayheads.push_str(&format!(
            "<a class=\"tgrid__dayhead{today}\" href=\"{href}\" aria-label=\"{date}{today_label}\">\
               <span class=\"tgrid__dow\">{dow}</span>\
               <span class=\"tgrid__dnum\">{mon} {day}</span>\
               {today_text}\
             </a>",
            today = if d.is_today {
                " tgrid__dayhead--today"
            } else {
                ""
            },
            href = day_href,
            date = esc(&d.iso_date),
            today_label = if d.is_today { " — Today" } else { "" },
            dow = d.weekday,
            mon = d.month_abbr,
            day = d.day,
            today_text = if d.is_today {
                "<span class=\"sr-only\">Today</span>"
            } else {
                ""
            },
        ));
        alldays.push_str(&format!(
            "<div class=\"tgrid__allday-cell\">{}</div>",
            render_allday_chips(d, events, calendars, off)
        ));
    }

    // The hour gutter (00:00 … 23:00), one label per hour row.
    let mut gutter = String::new();
    for h in 0..24 {
        gutter.push_str(&format!("<div class=\"tgrid__hour-label\">{h:02}:00</div>"));
    }

    // The day columns: each hosts its timed events, absolutely positioned by minute-of-day.
    let mut columns = String::new();
    for d in &g.days {
        columns.push_str(&format!(
            "<div class=\"tgrid__col{today}\">{now_line}{events}</div>",
            today = if d.is_today { " tgrid__col--today" } else { "" },
            now_line = render_now_line(d, now),
            events = render_day_column(d, events, calendars, off),
        ));
    }

    format!(
        "{head}\
         <section class=\"card tgrid\">\
           <div class=\"tgrid__cols\" style=\"{cols_style}\">\
             <div class=\"tgrid__corner\"></div>{dayheads}\
             <div class=\"tgrid__allday-label\">All-day</div>{alldays}\
           </div>\
           <div class=\"tgrid__body\" style=\"{cols_style}\">\
             <div class=\"tgrid__gutter\">{gutter}</div>{columns}\
           </div>\
         </section>\
         <p class=\"almanac-foot\">Steadholme Almanac · personal calendar &amp; contacts · times shown in {tz}</p>",
        head = head,
        cols_style = cols_style,
        dayheads = dayheads,
        alldays = alldays,
        gutter = gutter,
        columns = columns,
        tz = esc(tz_label),
    )
}

/// The all-day chips overlapping one day column.
fn render_allday_chips(d: &GridDay, events: &[Event], calendars: &[Calendar], off: i32) -> String {
    events
        .iter()
        .filter(|e| e.all_day && e.starts_at <= d.end_ms && e.ends_at >= d.start_ms)
        .map(|e| {
            let tooltip = esc(&format!(
                "{} — {}",
                calendar::fmt_event_when_at(e.starts_at, e.ends_at, e.all_day, off),
                e.title
            ));
            format!(
                "<a class=\"tgrid__allday-event\" href=\"{href}\" title=\"{tip}\"{style}>\
                   <span class=\"sr-only\">All-day · </span>{title}{recur}</a>",
                href = event_edit_href(e),
                tip = tooltip,
                style = event_style(e, calendars),
                title = esc(&e.title),
                recur = recur_mark(e),
            )
        })
        .collect()
}

/// A red "now" indicator line, positioned by the current minute-of-day, on today's column only.
fn render_now_line(d: &GridDay, now: i64) -> String {
    if !d.is_today {
        return String::new();
    }
    let min = (now - d.start_ms) as f64 / 60_000.0;
    if !(0.0..=1440.0).contains(&min) {
        return String::new();
    }
    format!(
        "<div class=\"tgrid__now\" style=\"top:{:.1}px\"><span class=\"sr-only\">Now</span></div>",
        min * PX_PER_MIN
    )
}

/// Place a day column's timed events on the hour grid. Events are absolutely positioned by their
/// minute-of-day (clamped to the visible day) and packed into side-by-side lanes so overlapping
/// events never hide one another. `events` is pre-sorted by `starts_at` (see `expand_events`).
fn render_day_column(d: &GridDay, events: &[Event], calendars: &[Calendar], off: i32) -> String {
    let timed: Vec<&Event> = events
        .iter()
        .filter(|e| !e.all_day && e.starts_at <= d.end_ms && e.ends_at >= d.start_ms)
        .collect();

    // Minutes-from-local-midnight, clamped to the visible [0, 1440] day.
    let mins = |ms: i64| -> f64 { ((ms - d.start_ms) as f64 / 60_000.0).clamp(0.0, 1440.0) };

    let mut out = String::new();
    let mut i = 0;
    while i < timed.len() {
        // Gather one cluster of chain-overlapping events, then lane-pack just that cluster.
        let mut j = i;
        let mut cluster_end = mins(timed[i].ends_at);
        while j + 1 < timed.len() && mins(timed[j + 1].starts_at) < cluster_end {
            j += 1;
            cluster_end = cluster_end.max(mins(timed[j].ends_at));
        }
        let cluster = &timed[i..=j];

        // Greedy lane assignment: reuse the first lane whose last event has ended.
        let mut lane_end: Vec<f64> = Vec::new();
        let mut lane_of: Vec<usize> = Vec::with_capacity(cluster.len());
        for e in cluster {
            let s = mins(e.starts_at);
            let en = mins(e.ends_at).max(s + 1.0);
            let mut placed = None;
            for (li, le) in lane_end.iter_mut().enumerate() {
                if s >= *le {
                    *le = en;
                    placed = Some(li);
                    break;
                }
            }
            match placed {
                Some(li) => lane_of.push(li),
                None => {
                    lane_end.push(en);
                    lane_of.push(lane_end.len() - 1);
                }
            }
        }
        let lanes = lane_end.len().max(1) as f64;
        let width = 100.0 / lanes;

        for (k, e) in cluster.iter().enumerate() {
            let s = mins(e.starts_at);
            let en = mins(e.ends_at).max(s + 1.0);
            let top = s * PX_PER_MIN;
            let height = ((en - s) * PX_PER_MIN).max(16.0);
            let left = lane_of[k] as f64 * width;
            let tooltip = esc(&format!(
                "{} — {}",
                calendar::fmt_event_when_at(e.starts_at, e.ends_at, e.all_day, off),
                e.title
            ));
            out.push_str(&format!(
                "<a class=\"tgrid__event\" href=\"{href}\" \
                   style=\"top:{top:.1}px;height:{height:.1}px;left:{left:.4}%;width:{width:.4}%;--cal-color:{color}\" \
                   title=\"{tip}\">\
                   <span class=\"cal-event__time tgrid__event-time\">{start}</span> {title}{recur}\
                 </a>",
                href = event_edit_href(e),
                top = top,
                height = height,
                left = left,
                width = width,
                color = event_color(e, calendars),
                tip = tooltip,
                start = calendar::human_time_at(e.starts_at, off),
                title = esc(&e.title),
                recur = recur_mark(e),
            ));
        }
        i = j + 1;
    }
    out
}

struct MonthRenderContext<'a> {
    now: i64,
    csrf: &'a str,
    off: i32,
    monday_first: bool,
    timezone: &'a str,
    source_partial: bool,
}

fn render_calendar(
    view: &MonthView,
    events: &[Event],
    calendars: &[Calendar],
    context: MonthRenderContext<'_>,
) -> String {
    let MonthRenderContext {
        now,
        csrf,
        off,
        monday_first,
        timezone,
        source_partial,
    } = context;
    let head = format!(
        "<div class=\"cal-head\">\
           <h1>{month} {year}</h1>\
           <div class=\"cal-nav\">\
             {switch}\
             <a class=\"btn btn-secondary btn-sm\" href=\"/?y={py}&amp;m={pm}\" aria-label=\"Previous month\">‹ Prev</a>\
             <a class=\"btn btn-secondary btn-sm\" href=\"/\">Today</a>\
             <a class=\"btn btn-secondary btn-sm\" href=\"/?y={ny}&amp;m={nm}\" aria-label=\"Next month\">Next ›</a>\
             <a class=\"btn btn-primary btn-sm\" href=\"/new\">New event</a>\
           </div>\
         </div>",
        month = view.month_name,
        year = view.year,
        switch = view_switch(ViewKind::Month, &format!("{:04}-{:02}-01", view.year, view.month)),
        py = view.prev.0,
        pm = view.prev.1,
        ny = view.next.0,
        nm = view.next.1,
    );

    let weekdays: String = calendar::weekday_headers(monday_first)
        .iter()
        .map(|w| format!("<span class=\"cal-weekday\">{w}</span>"))
        .collect();

    let mut weeks = String::new();
    for week in &view.weeks {
        weeks.push_str("<div class=\"cal-week\">");
        for cell in week {
            match cell {
                None => weeks.push_str("<div class=\"cal-day cal-day--pad\"></div>"),
                Some(c) => {
                    weeks.push_str(&render_day_cell(c, events, calendars, off, source_partial))
                }
            }
        }
        weeks.push_str("</div>");
    }

    format!(
        "{head}\
         <section class=\"card cal-grid\">\
           <div class=\"cal-weekdays\">{weekdays}</div>\
           <div class=\"cal-weeks\">{weeks}</div>\
         </section>\
         {agenda}\
         <p class=\"almanac-foot\">Steadholme Almanac · personal calendar &amp; contacts · times shown in {tz}</p>",
        head = head,
        weekdays = weekdays,
        weeks = weeks,
        agenda = render_agenda(events, calendars, now, csrf, off, source_partial),
        tz = esc(timezone),
    )
}

/// The `[earliest, latest]` UTC-ms span covered by the month grid's real (non-padding) cells.
fn grid_bounds(view: &MonthView) -> (i64, i64) {
    let mut start = i64::MAX;
    let mut end = i64::MIN;
    for cell in view.weeks.iter().flatten().flatten() {
        start = start.min(cell.start_ms);
        end = end.max(cell.end_ms);
    }
    if start > end {
        (0, 0)
    } else {
        (start, end)
    }
}

fn recurrence_context(e: &Event) -> Option<(&str, i64)> {
    if !e.series_id.trim().is_empty() && e.override_occurrence_date > 0 {
        Some((&e.series_id, e.override_occurrence_date))
    } else if !e.rrule.trim().is_empty() {
        Some((&e.id, e.starts_at))
    } else {
        None
    }
}

fn event_edit_href(e: &Event) -> String {
    match recurrence_context(e) {
        Some((series_id, occurrence)) => {
            format!("/edit/{}?occurrence={occurrence}", esc(series_id))
        }
        None => format!("/edit/{}", esc(&e.id)),
    }
}

/// Structural recurrence marker: CSS draws the espalier, text preserves meaning without color.
fn recur_mark(e: &Event) -> String {
    if !e.series_id.trim().is_empty() {
        "<span class=\"esp esp--override\" aria-label=\"Changed occurrence\">\
           <span class=\"esp__label\">Changed occurrence</span></span>"
            .to_string()
    } else if !e.rrule.trim().is_empty() {
        "<span class=\"esp esp--occurrence\" aria-label=\"Occurrence\">\
           <span class=\"esp__label\">Occurrence</span></span>"
            .to_string()
    } else {
        String::new()
    }
}

fn render_day_cell(
    c: &DayCell,
    events: &[Event],
    calendars: &[Calendar],
    off: i32,
    source_partial: bool,
) -> String {
    let day_events: Vec<&Event> = events
        .iter()
        .filter(|e| e.starts_at <= c.end_ms && e.ends_at >= c.start_ms)
        .collect();

    let mut chips = String::new();
    for e in day_events.iter().take(DAY_CHIP_LIMIT) {
        let calendar_name = event_calendar(e, calendars)
            .map(|calendar| calendar.name.as_str())
            .unwrap_or("Calendar");
        let label = if e.all_day {
            format!(
                "<span class=\"sr-only\">All-day · </span>\
                 <span class=\"cal-event__title\">{}</span>{}\
                 <span class=\"sr-only\"> · {}</span>",
                esc(&e.title),
                recur_mark(e),
                esc(calendar_name),
            )
        } else {
            format!(
                "<span class=\"cal-event__time\">{}</span> \
                 <span class=\"cal-event__title\">{}</span>{}\
                 <span class=\"sr-only\"> · {}</span>",
                calendar::human_time_at(e.starts_at, off),
                esc(&e.title),
                recur_mark(e),
                esc(calendar_name),
            )
        };
        let tooltip = esc(&format!(
            "{} — {}",
            calendar::fmt_event_when_at(e.starts_at, e.ends_at, e.all_day, off),
            e.title
        ));
        chips.push_str(&format!(
            "<a class=\"cal-event{ad}\" href=\"{href}\" title=\"{tooltip}\"{style}>{label}</a>",
            ad = if e.all_day { " cal-event--allday" } else { "" },
            href = event_edit_href(e),
            tooltip = tooltip,
            style = event_style(e, calendars),
            label = label,
        ));
    }
    if day_events.len() > DAY_CHIP_LIMIT {
        chips.push_str(&format!(
            "<a class=\"cal-more\" href=\"/?view=day&amp;date={iso}\">\
               +{more} more{loaded} <span class=\"cal-tally\" aria-hidden=\"true\">{total}</span>\
             </a>",
            iso = esc(&c.iso_date),
            more = day_events.len() - DAY_CHIP_LIMIT,
            loaded = if source_partial { " loaded" } else { "" },
            total = day_events.len(),
        ));
    }
    let count_label = if source_partial {
        format!(
            "{} loaded events; more may exist outside this projection",
            day_events.len()
        )
    } else {
        format!("{} events", day_events.len())
    };

    format!(
        "<div class=\"cal-day{today}\" aria-label=\"{iso} — {count_label}\">\
           <a class=\"cal-daynum\" href=\"/?view=day&amp;date={iso}\">{day}{today_text}</a>\
           <a class=\"cal-add\" href=\"/new?date={iso}\" aria-label=\"Add an event on {iso}\">+</a>\
           <div class=\"cal-events\">{chips}</div>\
         </div>",
        today = if c.is_today { " cal-day--today" } else { "" },
        iso = esc(&c.iso_date),
        day = c.day,
        count_label = count_label,
        today_text = if c.is_today {
            "<span class=\"sr-only\"> Today</span>"
        } else {
            ""
        },
        chips = chips,
    )
}

fn render_agenda(
    events: &[Event],
    calendars: &[Calendar],
    now: i64,
    csrf: &str,
    off: i32,
    source_partial: bool,
) -> String {
    // `events` is already sorted by starts_at ascending; keep only those that haven't ended.
    let all_upcoming: Vec<&Event> = events.iter().filter(|e| e.ends_at >= now).collect();
    let hidden = all_upcoming.len().saturating_sub(AGENDA_LIMIT);
    let upcoming: Vec<&Event> = all_upcoming.into_iter().take(AGENDA_LIMIT).collect();

    let body = if upcoming.is_empty() {
        if source_partial {
            "<p class=\"agenda-empty state state--partial\">No loaded events in this range; more events exist outside this projection.</p>".to_string()
        } else {
            "<p class=\"agenda-empty state state--empty\">No events in this range. <a href=\"/new\">New event</a></p>".to_string()
        }
    } else {
        upcoming
            .iter()
            .map(|e| render_agenda_item(e, calendars, csrf, off))
            .collect()
    };

    let partial = if hidden > 0 {
        format!("<p class=\"state state--partial\">+{hidden} more loaded events not shown</p>")
    } else {
        String::new()
    };
    format!(
        "<section class=\"agenda\"><h2 class=\"agenda__head\">Upcoming</h2>{body}{partial}</section>",
    )
}

struct AgendaRenderContext<'a> {
    now: i64,
    csrf: &'a str,
    off: i32,
    timezone: &'a str,
    days: i64,
    source_partial: bool,
}

fn render_agenda_view(
    events: &[Event],
    calendars: &[Calendar],
    context: AgendaRenderContext<'_>,
) -> String {
    let AgendaRenderContext {
        now,
        csrf,
        off,
        timezone,
        days,
        source_partial,
    } = context;
    let all_upcoming: Vec<&Event> = events.iter().filter(|e| e.ends_at >= now).collect();
    let hidden = all_upcoming.len().saturating_sub(AGENDA_LIMIT);
    let upcoming: Vec<&Event> = all_upcoming.into_iter().take(AGENDA_LIMIT).collect();
    let body = if upcoming.is_empty() {
        if source_partial {
            "<p class=\"agenda-empty state state--partial\">No loaded events in this range; more events exist outside this projection.</p>".to_string()
        } else {
            "<p class=\"agenda-empty state state--empty\">No events in this range. <a href=\"/new\">New event</a></p>".to_string()
        }
    } else {
        let mut out = String::new();
        let mut current_day: Option<i64> = None;
        for e in upcoming {
            let day = calendar::start_of_day_at(e.starts_at, off);
            if current_day != Some(day) {
                current_day = Some(day);
                out.push_str(&format!(
                    "<h2 class=\"agenda__day\">{}</h2>",
                    esc(&calendar::human_date_at(day, off))
                ));
            }
            out.push_str(&render_agenda_item(e, calendars, csrf, off));
        }
        out
    };
    let partial = if hidden > 0 {
        format!("<p class=\"state state--partial\">+{hidden} more loaded events not shown</p>")
    } else {
        String::new()
    };
    format!(
        "<div class=\"cal-head\">\
           <h1>Agenda</h1>\
           <div class=\"cal-nav\">\
             {switch}\
             <a class=\"btn btn-primary btn-sm\" href=\"/new\">New event</a>\
           </div>\
         </div>\
         <section class=\"agenda agenda--list\" aria-label=\"Next {days} days\">{body}{partial}</section>\
         <p class=\"almanac-foot\">Steadholme Almanac · personal calendar &amp; contacts · times shown in {tz}</p>",
        switch = view_switch(ViewKind::Agenda, &calendar::fmt_date_input(now)),
        days = days,
        body = body,
        partial = partial,
        tz = esc(timezone),
    )
}

struct SearchRenderContext<'a> {
    csrf: &'a str,
    off: i32,
    needle: &'a str,
    result_partial: bool,
    source_partial: bool,
}

fn render_search_results(
    q: &SearchQuery,
    results: &[Event],
    calendars: &[Calendar],
    context: SearchRenderContext<'_>,
) -> String {
    let SearchRenderContext {
        csrf,
        off,
        needle,
        result_partial,
        source_partial,
    } = context;
    let q_str = q.q.as_deref().unwrap_or("").trim();
    let from_str = q.from.as_deref().unwrap_or("").trim();
    let to_str = q.to.as_deref().unwrap_or("").trim();
    let body = if needle.is_empty() {
        "<p class=\"muted\">Type a word to search your events.</p>".to_string()
    } else if results.is_empty() {
        if source_partial {
            format!(
                "<p class=\"state state--partial\">No loaded events match “{}”; additional events were not searched.</p>",
                esc(q_str)
            )
        } else {
            format!("<p class=\"muted\">No events match “{}”.</p>", esc(q_str))
        }
    } else {
        let mut out = String::new();
        let mut current_day: Option<i64> = None;
        for e in results {
            let day = calendar::start_of_day_at(e.starts_at, off);
            if current_day != Some(day) {
                current_day = Some(day);
                out.push_str(&format!(
                    "<h2 class=\"agenda__day\">{}</h2>",
                    esc(&calendar::human_date_at(day, off))
                ));
            }
            out.push_str(&render_agenda_item(e, calendars, csrf, off));
        }
        out
    };

    let bound = if result_partial || source_partial {
        "<p class=\"state state--partial\">Showing a bounded set of loaded matches — narrow the search. More events may exist.</p>"
    } else {
        ""
    };
    format!(
        "<div class=\"cal-head\"><h1>Search</h1></div>\
         <form method=\"get\" action=\"/search\" class=\"search-form\">\
           <input type=\"search\" name=\"q\" value=\"{q}\" placeholder=\"Search events\" aria-label=\"Search events\" autofocus>\
           <input type=\"date\" name=\"from\" value=\"{from}\" aria-label=\"From date\">\
           <input type=\"date\" name=\"to\" value=\"{to}\" aria-label=\"To date\">\
           <button class=\"btn btn-primary btn-sm\" type=\"submit\">Search</button>\
         </form>\
         <section class=\"agenda agenda--list\">{body}{bound}</section>",
        q = esc(q_str),
        from = esc(from_str),
        to = esc(to_str),
        body = body,
        bound = bound,
    )
}

fn render_agenda_item(e: &Event, calendars: &[Calendar], csrf: &str, off: i32) -> String {
    let loc = if e.location.trim().is_empty() {
        String::new()
    } else {
        format!("<span class=\"agenda__loc\">{}</span>", esc(&e.location))
    };
    let cal = event_calendar(e, calendars)
        .map(|c| {
            format!(
                "<span class=\"agenda__cal\"><span class=\"calendar-swatch\" style=\"--cal-color:{color}\"></span>{name}</span>",
                color = calendar_swatch(&c.color),
                name = esc(&c.name),
            )
        })
        .unwrap_or_default();
    let occurrence_actions = occurrence_actions(e, csrf);
    let delete = if recurrence_context(e).is_some() {
        String::new()
    } else {
        format!(
            "<form class=\"agenda__del\" method=\"post\" action=\"/delete/{id}\" \
               onsubmit=\"return confirm('Delete this event?')\">\
               <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
               <button class=\"btn btn-ghost btn-sm\" type=\"submit\">Delete</button>\
             </form>",
            id = esc(&e.id),
            csrf = esc(csrf),
        )
    };
    format!(
        "<div class=\"agenda__item\"{style}>\
           <div class=\"agenda__when\">{when}</div>\
           <div class=\"agenda__body\">\
             <a class=\"agenda__title\" href=\"{href}\">{title}{recur}</a>\
             {cal}\
             {loc}\
           </div>\
           {occurrence_actions}\
           {delete}\
         </div>",
        style = event_style(e, calendars),
        when = esc(&calendar::fmt_event_when_at(
            e.starts_at,
            e.ends_at,
            e.all_day,
            off
        )),
        href = event_edit_href(e),
        title = esc(&e.title),
        recur = recur_mark(e),
        cal = cal,
        loc = loc,
        occurrence_actions = occurrence_actions,
        delete = delete,
    )
}

fn occurrence_actions(e: &Event, csrf: &str) -> String {
    let Some((series_id, occurrence_date)) = recurrence_context(e) else {
        return String::new();
    };
    if occurrence_date <= 0 {
        return String::new();
    }
    format!(
        "<div class=\"agenda__occ esp esp--scope\">\
           <span class=\"esp__label\">Occurrence scope</span>\
           <a class=\"btn btn-secondary btn-sm\" href=\"/edit/{id}?occurrence={occ}\">Choose edit scope</a>\
           <form method=\"post\" action=\"/delete/{id}?scope=occurrence&amp;occurrence={occ}\" onsubmit=\"return confirm('Delete only this occurrence?')\">\
             <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
             <button class=\"btn btn-ghost btn-sm\" type=\"submit\">Delete this occurrence</button>\
           </form>\
         </div>",
        id = esc(series_id),
        occ = occurrence_date,
        csrf = esc(csrf),
    )
}

fn event_calendar<'a>(e: &Event, calendars: &'a [Calendar]) -> Option<&'a Calendar> {
    calendars.iter().find(|c| c.id == e.calendar_id)
}

fn is_hex_color(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 7 && b[0] == b'#' && b[1..].iter().all(u8::is_ascii_hexdigit)
}

fn calendar_swatch(color: &str) -> String {
    if is_hex_color(color) {
        color.to_string()
    } else {
        DEFAULT_CALENDAR_COLOR.to_string()
    }
}

fn event_color(e: &Event, calendars: &[Calendar]) -> String {
    event_calendar(e, calendars)
        .map(|c| calendar_swatch(&c.color))
        .unwrap_or_else(|| DEFAULT_CALENDAR_COLOR.to_string())
}

fn event_style(e: &Event, calendars: &[Calendar]) -> String {
    format!(" style=\"--cal-color:{}\"", event_color(e, calendars))
}

fn occurrence_event(series: &Event, occurrence_date: i64) -> Event {
    let mut event = series.clone();
    let duration = (series.ends_at - series.starts_at).max(0);
    event.starts_at = occurrence_date;
    event.ends_at = occurrence_date + duration;
    event.rrule.clear();
    event.series_id = series.id.clone();
    event.override_occurrence_date = occurrence_date;
    event.exception_dates.clear();
    event
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MutationScope {
    OneOff,
    Series,
    Occurrence(i64),
}

fn mutation_scope(event: &Event, query: &OccurrenceQuery) -> Result<MutationScope, AppError> {
    let recurring = !event.rrule.trim().is_empty();
    if !recurring {
        return if query.scope.is_none() && query.occurrence.is_none() {
            Ok(MutationScope::OneOff)
        } else {
            Err(AppError::BadRequest(
                "A one-off event does not accept recurrence scope.".to_string(),
            ))
        };
    }

    match (query.scope.as_deref().map(str::trim), query.occurrence) {
        (Some("series"), None) => Ok(MutationScope::Series),
        (Some("occurrence"), Some(occurrence))
            if valid_occurrence_for_series(event, occurrence) =>
        {
            Ok(MutationScope::Occurrence(occurrence))
        }
        _ => Err(AppError::BadRequest(
            "Choose the whole series or one valid occurrence before continuing.".to_string(),
        )),
    }
}

fn valid_occurrence_for_series(series: &Event, occurrence: i64) -> bool {
    occurrence > 0
        && !series.exception_dates.contains(&occurrence)
        && rrule::occurrence_belongs_to_series(series.starts_at, &series.rrule, occurrence)
}

fn render_scope_gate(series: &Event, csrf: &str, off: i32, occurrence: Option<i64>) -> String {
    let occurrence_choice = occurrence
        .map(|occurrence| {
            let event = occurrence_event(series, occurrence);
            format!(
                "<form method=\"get\" action=\"/edit/{id}\" class=\"scope-gate__choice\">\
                   <input type=\"hidden\" name=\"scope\" value=\"occurrence\">\
                   <input type=\"hidden\" name=\"occurrence\" value=\"{occurrence}\">\
                   <button class=\"btn btn-secondary\" type=\"submit\">\
                     <strong>Occurrence</strong><span>{when}</span>\
                   </button>\
                 </form>",
                id = esc(&series.id),
                when = esc(&calendar::fmt_event_when_at(
                    event.starts_at,
                    event.ends_at,
                    event.all_day,
                    off,
                )),
            )
        })
        .unwrap_or_else(|| {
            "<p class=\"state state--empty\">Open an occurrence from the calendar to edit only that date.</p>"
                .to_string()
        });
    let occurrence_delete = occurrence
        .map(|occurrence| {
            format!(
                "<form method=\"post\" action=\"/delete/{id}?scope=occurrence&amp;occurrence={occurrence}\">\
                   <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
                   <p>Delete only this occurrence. The series and other occurrences stay.</p>\
                   <button class=\"btn btn-danger\" type=\"submit\">Delete this occurrence</button>\
                 </form>",
                id = esc(&series.id),
                csrf = esc(csrf),
            )
        })
        .unwrap_or_default();
    format!(
        "<section class=\"card scope-gate view-editor\">\
           <h1>Choose what to edit</h1>\
           <p>This event repeats. No scope is selected for you.</p>\
           <div class=\"scope-gate__choices\" aria-label=\"Apply this edit to\">\
             <form method=\"get\" action=\"/edit/{id}\" class=\"scope-gate__choice\">\
               <input type=\"hidden\" name=\"scope\" value=\"series\">\
               <button class=\"btn btn-primary\" type=\"submit\">\
                 <strong>Series</strong><span>The whole recurring event</span>\
               </button>\
             </form>\
             {occurrence_choice}\
           </div>\
           <div class=\"danger-zone scope-gate__delete\">\
             <h2>Delete scope</h2>\
             <form method=\"post\" action=\"/delete/{id}?scope=series\">\
               <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
               <p>Delete the whole series “{title}”. All future occurrences, overrides, attendee responses, and reminders are removed. Shared .ics files and inboxes are not affected.</p>\
               <button class=\"btn btn-danger\" type=\"submit\">Delete whole series</button>\
             </form>\
             {occurrence_delete}\
           </div>\
         </section>",
        id = esc(&series.id),
        title = esc(&series.title),
        csrf = esc(csrf),
        occurrence_choice = occurrence_choice,
        occurrence_delete = occurrence_delete,
    )
}

// ---------------------------------------------------------------------------
// GET /new  — the create form
// ---------------------------------------------------------------------------

pub async fn new_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NewQuery>, QueryRejection>,
) -> Result<Response, AppError> {
    let q = query_or_invalid(query)?;
    let owner = auth::owner_subject(&headers);
    let off = calendar::tz_offset_minutes(&state.store.get_settings(&owner).await?.timezone);
    let calendars = state.store.list_calendars(&owner).await?;
    let csrf = auth::new_csrf_token();

    // If a day was clicked, default to a 09:00–10:00 LOCAL slot on that day; else leave blank.
    let requested_date = parse_optional_query_date(q.date.as_deref(), "new-event", off)?;
    let (starts_local, ends_local) = match requested_date {
        Some(day) => {
            let s = day + 9 * 3_600_000;
            (
                calendar::fmt_datetime_local_at(s, off),
                calendar::fmt_datetime_local_at(s + 3_600_000, off),
            )
        }
        None => (String::new(), String::new()),
    };

    let (repeat, repeat_interval, repeat_count, repeat_until) = FormView::no_recurrence();
    let view = FormView {
        action: "/new".to_string(),
        title: String::new(),
        calendar_id: calendars.first().map(|c| c.id.clone()).unwrap_or_default(),
        calendars,
        starts_local,
        ends_local,
        all_day: false,
        location: String::new(),
        notes: String::new(),
        repeat,
        repeat_interval,
        repeat_count,
        repeat_until,
        attendees: String::new(),
        reminders: Vec::new(),
        csrf: csrf.clone(),
        is_edit: false,
        is_occurrence: false,
        series_id: String::new(),
        occurrence_date: 0,
        id: String::new(),
        errors: Vec::new(),
    };
    let content = format!(
        "{}<div class=\"view-editor\">{}</div>",
        render::subnav("calendar"),
        render_event_form(&view)
    );
    Ok(html_with_csrf_cookie(
        layout("New event", &headers, &content),
        &csrf,
    ))
}

// ---------------------------------------------------------------------------
// POST /new  — create an event
// ---------------------------------------------------------------------------

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: Result<Form<EventForm>, FormRejection>,
) -> Result<Response, AppError> {
    let form = form_or_invalid(form)?;
    require_csrf(&headers, &form.csrf_token)?;
    let owner = auth::owner_subject(&headers);
    let off = calendar::tz_offset_minutes(&state.store.get_settings(&owner).await?.timezone);
    let parsed = match parse_event_form(&form, off) {
        Ok(parsed) => parsed,
        Err(errors) => {
            let calendars = state.store.list_calendars(&owner).await?;
            let view = FormView::from_submission(
                &form,
                calendars,
                "/new".to_string(),
                errors,
                SubmittedEditContext::default(),
            );
            return Ok(invalid_event_form_response(&headers, &view));
        }
    };
    let now = now_ms();
    let id = auth::random_hex();

    let redirect_start = parsed.starts_at;
    state
        .store
        .save_event_bundle(EventBundle {
            event: Event {
                id,
                owner_sub: owner,
                calendar_id: parsed.calendar_id,
                title: parsed.title,
                starts_at: parsed.starts_at,
                ends_at: parsed.ends_at,
                all_day: parsed.all_day,
                location: parsed.location,
                notes: parsed.notes,
                rrule: parsed.rrule,
                series_id: String::new(),
                override_occurrence_date: 0,
                exception_dates: Vec::new(),
                created_at: now,
            },
            attendees: attendee_inputs_from_form(&form),
            reminder_minutes: reminder_minutes_from_form(&form),
            now_ms: now,
            reconcile_series: false,
        })
        .await?;

    tracing::info!(target: "audit", event = "event.created", "event created");
    Ok(redirect_to_month(redirect_start, off))
}

// ---------------------------------------------------------------------------
// GET /edit/{id}  — the edit form
// ---------------------------------------------------------------------------

pub async fn edit_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<OccurrenceQuery>, QueryRejection>,
) -> Result<Response, AppError> {
    let id = path_or_invalid(path)?;
    let q = query_or_invalid(query)?;
    let owner = auth::owner_subject(&headers);
    let off = calendar::tz_offset_minutes(&state.store.get_settings(&owner).await?.timezone);
    let calendars = state.store.list_calendars(&owner).await?;
    let series = state
        .store
        .get_event(&owner, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("That event does not exist.".to_string()))?;

    let csrf = auth::new_csrf_token();
    if !series.rrule.trim().is_empty() && q.scope.is_none() {
        let occurrence_context = match q.occurrence {
            Some(occurrence) if valid_occurrence_for_series(&series, occurrence) => {
                Some(occurrence)
            }
            Some(_) => {
                return Err(AppError::BadRequest(
                    "Choose a valid occurrence before continuing.".to_string(),
                ))
            }
            None => None,
        };
        let content = format!(
            "{}{}",
            render::subnav("calendar"),
            render_scope_gate(&series, &csrf, off, occurrence_context)
        );
        return Ok(html_with_csrf_cookie(
            layout("Choose edit scope", &headers, &content),
            &csrf,
        ));
    }
    let scope = mutation_scope(&series, &q)?;
    let (event, action, is_occurrence, attendee_source_id, occurrence_date) = match scope {
        MutationScope::Occurrence(occ) => {
            let override_event = state
                .store
                .get_event_override(&owner, &series.id, occ)
                .await?;
            let event = override_event.unwrap_or_else(|| occurrence_event(&series, occ));
            let source = if event.id == series.id {
                series.id.clone()
            } else {
                event.id.clone()
            };
            (
                event,
                format!("/edit/{}?scope=occurrence&occurrence={}", series.id, occ),
                true,
                source,
                occ,
            )
        }
        MutationScope::Series => (
            series.clone(),
            format!("/edit/{}?scope=series", series.id),
            false,
            series.id.clone(),
            0,
        ),
        MutationScope::OneOff => (
            series.clone(),
            format!("/edit/{}", series.id),
            false,
            series.id.clone(),
            0,
        ),
    };
    let (repeat, repeat_interval, repeat_count, repeat_until) =
        FormView::recurrence_from(&event.rrule);
    let attendees = state
        .store
        .list_attendees(&owner, &attendee_source_id)
        .await?;
    let reminders: Vec<i64> = state
        .store
        .list_reminders(&owner, &attendee_source_id)
        .await?
        .iter()
        .map(|r| r.minutes_before)
        .collect();
    let view = FormView {
        action,
        title: event.title.clone(),
        calendar_id: event.calendar_id.clone(),
        calendars,
        starts_local: calendar::fmt_datetime_local_at(event.starts_at, off),
        ends_local: calendar::fmt_datetime_local_at(event.ends_at, off),
        all_day: event.all_day,
        location: event.location.clone(),
        notes: event.notes.clone(),
        repeat,
        repeat_interval,
        repeat_count,
        repeat_until,
        attendees: format_attendees_textarea(&attendees),
        reminders,
        csrf: csrf.clone(),
        is_edit: true,
        is_occurrence,
        series_id: series.id.clone(),
        occurrence_date,
        id: if is_occurrence {
            series.id.clone()
        } else {
            event.id.clone()
        },
        errors: Vec::new(),
    };
    let content = format!(
        "{}<div class=\"view-editor\">{}</div>",
        render::subnav("calendar"),
        render_event_form(&view)
    );
    Ok(html_with_csrf_cookie(
        layout("Edit event", &headers, &content),
        &csrf,
    ))
}

// ---------------------------------------------------------------------------
// POST /edit/{id}  — update an event
// ---------------------------------------------------------------------------

pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<OccurrenceQuery>, QueryRejection>,
    form: Result<Form<EventForm>, FormRejection>,
) -> Result<Response, AppError> {
    let id = path_or_invalid(path)?;
    let q = query_or_invalid(query)?;
    let form = form_or_invalid(form)?;
    require_csrf(&headers, &form.csrf_token)?;
    let owner = auth::owner_subject(&headers);
    let off = calendar::tz_offset_minutes(&state.store.get_settings(&owner).await?.timezone);

    let existing = state
        .store
        .get_event(&owner, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("That event does not exist.".to_string()))?;
    let scope = mutation_scope(&existing, &q)?;
    let parsed = match parse_event_form(&form, off) {
        Ok(parsed) => parsed,
        Err(errors) => {
            let calendars = state.store.list_calendars(&owner).await?;
            let (action, edit) = match scope {
                MutationScope::Occurrence(occurrence) => (
                    format!("/edit/{id}?scope=occurrence&occurrence={occurrence}"),
                    SubmittedEditContext {
                        is_edit: true,
                        is_occurrence: true,
                        series_id: existing.id.clone(),
                        occurrence_date: occurrence,
                        id: existing.id.clone(),
                    },
                ),
                MutationScope::Series => (
                    format!("/edit/{id}?scope=series"),
                    SubmittedEditContext {
                        is_edit: true,
                        series_id: existing.id.clone(),
                        id: existing.id.clone(),
                        ..SubmittedEditContext::default()
                    },
                ),
                MutationScope::OneOff => (
                    format!("/edit/{id}"),
                    SubmittedEditContext {
                        is_edit: true,
                        id: existing.id.clone(),
                        ..SubmittedEditContext::default()
                    },
                ),
            };
            let view = FormView::from_submission(&form, calendars, action, errors, edit);
            return Ok(invalid_event_form_response(&headers, &view));
        }
    };
    let now = now_ms();

    if let MutationScope::Occurrence(occ) = scope {
        let prior = state
            .store
            .get_event_override(&owner, &existing.id, occ)
            .await?;
        let override_id = prior
            .as_ref()
            .map(|e| e.id.clone())
            .unwrap_or_else(auth::random_hex);
        let created_at = prior.as_ref().map(|e| e.created_at).unwrap_or(now);
        let redirect_start = parsed.starts_at;
        state
            .store
            .save_event_bundle(EventBundle {
                event: Event {
                    id: override_id,
                    owner_sub: owner,
                    calendar_id: parsed.calendar_id,
                    title: parsed.title,
                    starts_at: parsed.starts_at,
                    ends_at: parsed.ends_at,
                    all_day: parsed.all_day,
                    location: parsed.location,
                    notes: parsed.notes,
                    rrule: String::new(),
                    series_id: existing.id,
                    override_occurrence_date: occ,
                    exception_dates: Vec::new(),
                    created_at,
                },
                attendees: attendee_inputs_from_form(&form),
                reminder_minutes: reminder_minutes_from_form(&form),
                now_ms: now,
                reconcile_series: false,
            })
            .await?;
        tracing::info!(target: "audit", event = "event.occurrence.updated", "event occurrence updated");
        return Ok(redirect_to_month(redirect_start, off));
    }

    let redirect_start = parsed.starts_at;
    state
        .store
        .save_event_bundle(EventBundle {
            event: Event {
                id: existing.id,
                owner_sub: owner,
                calendar_id: parsed.calendar_id,
                title: parsed.title,
                starts_at: parsed.starts_at,
                ends_at: parsed.ends_at,
                all_day: parsed.all_day,
                location: parsed.location,
                notes: parsed.notes,
                rrule: parsed.rrule,
                series_id: existing.series_id,
                override_occurrence_date: existing.override_occurrence_date,
                exception_dates: Vec::new(),
                created_at: existing.created_at,
            },
            attendees: attendee_inputs_from_form(&form),
            reminder_minutes: reminder_minutes_from_form(&form),
            now_ms: now,
            reconcile_series: scope == MutationScope::Series,
        })
        .await?;

    tracing::info!(target: "audit", event = "event.updated", "event updated");
    Ok(redirect_to_month(redirect_start, off))
}

// ---------------------------------------------------------------------------
// POST /delete/{id}  — delete an event
// ---------------------------------------------------------------------------

pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<OccurrenceQuery>, QueryRejection>,
    form: Result<Form<DeleteForm>, FormRejection>,
) -> Result<Response, AppError> {
    let id = path_or_invalid(path)?;
    let q = query_or_invalid(query)?;
    let form = form_or_invalid(form)?;
    require_csrf(&headers, &form.csrf_token)?;
    let owner = auth::owner_subject(&headers);
    let existing = state
        .store
        .get_event(&owner, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("That event does not exist.".to_string()))?;
    let scope = mutation_scope(&existing, &q)?;
    if let MutationScope::Occurrence(occ) = scope {
        let deleted = state
            .store
            .delete_occurrence_bundle(
                &owner,
                &existing.id,
                occ,
                EventException {
                    id: format!("ex:{}:{occ}", existing.id),
                    event_id: existing.id.clone(),
                    owner_sub: owner.clone(),
                    occurrence_date: occ,
                    created_at: now_ms(),
                },
            )
            .await?;
        if !deleted {
            return Err(AppError::NotFound(
                "That occurrence is no longer available.".to_string(),
            ));
        }
        tracing::info!(target: "audit", event = "event.occurrence.deleted", "event occurrence deleted");
        return Ok(Redirect::to("/").into_response());
    }
    if !state.store.delete_event_tree(&owner, &id).await? {
        return Err(AppError::NotFound(
            "That event is no longer available.".to_string(),
        ));
    }
    tracing::info!(target: "audit", event = "event.deleted", "event deleted");
    Ok(Redirect::to("/").into_response())
}

// ---------------------------------------------------------------------------
// POST /quick-add  — natural-language quick-add
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct QuickAddReview {
    title: String,
    starts_at: i64,
    ends_at: i64,
    when: String,
    calendar_id: String,
    calendar_name: String,
    timezone: String,
    offset_minutes: i32,
}

#[derive(Clone, Debug)]
struct QuickAddCommitted {
    id: String,
    title: String,
    starts_at: i64,
    ends_at: i64,
    when: String,
    href: String,
}

#[derive(Clone, Debug)]
enum QuickAddOutcome {
    Review(QuickAddReview),
    InvalidInput {
        field: &'static str,
        message: &'static str,
    },
    Unavailable {
        commit_uncertain: bool,
    },
    Committed(QuickAddCommitted),
}

pub async fn quick_add(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: Result<Form<QuickAddInput>, axum::extract::rejection::FormRejection>,
) -> Response {
    let Form(form) = match form {
        Ok(form) => form,
        Err(_) => {
            return quick_add_html_state(
                StatusCode::BAD_REQUEST,
                "invalid_input",
                "Check the highlighted fields.",
            )
        }
    };
    if !auth::verify_csrf(&headers, &form.csrf_token) {
        return quick_add_html_state(
            StatusCode::FORBIDDEN,
            "csrf",
            "This form expired — reload and try again.",
        );
    }
    let owner = auth::owner_subject(&headers);
    let outcome = evaluate_quick_add(&state, &owner, &form).await;

    if form.intent == QuickAddIntent::OpenEditor {
        return render_quick_add_editor(&state, &headers, &owner, &form, outcome).await;
    }

    match outcome {
        QuickAddOutcome::Review(review) => {
            let content = format!(
                "{}<div class=\"view-editor\">{}</div>",
                render::subnav("calendar"),
                render_quick_add_review(&form, &review),
            );
            Html(layout("Quick add review", &headers, &content)).into_response()
        }
        QuickAddOutcome::InvalidInput { field, message } => {
            let content = format!(
                "{}<div class=\"view-editor\">{}</div>",
                render::subnav("calendar"),
                render_quick_add_invalid(&form, field, message),
            );
            (
                StatusCode::BAD_REQUEST,
                Html(layout("Check quick add", &headers, &content)),
            )
                .into_response()
        }
        QuickAddOutcome::Unavailable { commit_uncertain } => quick_add_html_state(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            quick_add_unavailable_message(commit_uncertain),
        ),
        QuickAddOutcome::Committed(committed) => {
            tracing::info!(target: "audit", event = "event.created", "event quick-added");
            Redirect::to(&committed.href).into_response()
        }
    }
}

async fn evaluate_quick_add(
    state: &AppState,
    owner: &str,
    input: &QuickAddInput,
) -> QuickAddOutcome {
    let settings = match state.store.get_settings(owner).await {
        Ok(settings) => settings,
        Err(_) => {
            return QuickAddOutcome::Unavailable {
                commit_uncertain: false,
            }
        }
    };
    let calendars = match state.store.list_calendars(owner).await {
        Ok(calendars) => calendars,
        Err(_) => {
            return QuickAddOutcome::Unavailable {
                commit_uncertain: false,
            }
        }
    };
    let requested_calendar = input.calendar_id.trim();
    let calendar = if requested_calendar.is_empty() {
        calendars.first()
    } else {
        calendars
            .iter()
            .find(|calendar| calendar.id == requested_calendar)
    };
    let calendar = match calendar {
        Some(calendar) => calendar,
        None => {
            return QuickAddOutcome::InvalidInput {
                field: "calendar_id",
                message: "Choose an available calendar.",
            }
        }
    };
    let qa = match quickadd::parse_quick_add(input.text.trim()) {
        Some(qa) => qa,
        None => {
            return QuickAddOutcome::InvalidInput {
                field: "text",
                message: "We couldn't interpret that phrase.",
            }
        }
    };
    let off = calendar::tz_offset_minutes(&settings.timezone);
    let now = now_ms();
    let starts_at = quick_add_starts_at(&qa, off, now);
    let ends_at = starts_at + 3_600_000;
    let review = QuickAddReview {
        title: qa.title,
        starts_at,
        ends_at,
        when: calendar::fmt_event_when_at(starts_at, ends_at, false, off),
        calendar_id: calendar.id.clone(),
        calendar_name: calendar.name.clone(),
        timezone: settings.timezone,
        offset_minutes: off,
    };

    if input.intent != QuickAddIntent::Commit
        || input.reviewed_title.as_deref() != Some(review.title.as_str())
        || input.reviewed_starts_at != Some(review.starts_at)
    {
        return QuickAddOutcome::Review(review);
    }

    let id = auth::random_hex();
    if state
        .store
        .save_event_bundle(EventBundle {
            event: Event {
                id: id.clone(),
                owner_sub: owner.to_string(),
                calendar_id: review.calendar_id.clone(),
                title: review.title.clone(),
                starts_at: review.starts_at,
                ends_at: review.ends_at,
                all_day: false,
                location: String::new(),
                notes: String::new(),
                rrule: String::new(),
                series_id: String::new(),
                override_occurrence_date: 0,
                exception_dates: Vec::new(),
                created_at: now,
            },
            attendees: Vec::new(),
            reminder_minutes: Vec::new(),
            now_ms: now,
            reconcile_series: false,
        })
        .await
        .is_err()
    {
        return QuickAddOutcome::Unavailable {
            commit_uncertain: true,
        };
    }
    let (year, month) = calendar::month_of_at(review.starts_at, off);
    QuickAddOutcome::Committed(QuickAddCommitted {
        id,
        title: review.title,
        starts_at: review.starts_at,
        ends_at: review.ends_at,
        when: review.when,
        href: format!("/?y={year}&m={month}"),
    })
}

async fn render_quick_add_editor(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    input: &QuickAddInput,
    outcome: QuickAddOutcome,
) -> Response {
    if let QuickAddOutcome::Unavailable { commit_uncertain } = &outcome {
        return quick_add_html_state(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            quick_add_unavailable_message(*commit_uncertain),
        );
    }
    if let QuickAddOutcome::InvalidInput { field, message } = &outcome {
        if *field != "text" {
            let content = format!(
                "{}<div class=\"view-editor\">{}</div>",
                render::subnav("calendar"),
                render_quick_add_invalid(input, field, message),
            );
            return (
                StatusCode::BAD_REQUEST,
                Html(layout("Check quick add", headers, &content)),
            )
                .into_response();
        }
    }
    let calendars = match state.store.list_calendars(owner).await {
        Ok(calendars) => calendars,
        Err(_) => {
            return quick_add_html_state(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "Time interpretation is unavailable — retry.",
            )
        }
    };
    let mut view = blank_form_view(&input.csrf_token, input.text.trim(), &calendars);
    if let QuickAddOutcome::Review(review) = outcome {
        view.title = review.title;
        view.calendar_id = review.calendar_id;
        view.starts_local =
            calendar::fmt_datetime_local_at(review.starts_at, review.offset_minutes);
        view.ends_local = calendar::fmt_datetime_local_at(review.ends_at, review.offset_minutes);
    }
    let content = format!(
        "{}<div class=\"view-editor\">{}</div>",
        render::subnav("calendar"),
        render_event_form(&view)
    );
    Html(layout("New event", headers, &content)).into_response()
}

fn render_quick_add_review(input: &QuickAddInput, review: &QuickAddReview) -> String {
    let hidden = |intent: &str| {
        format!(
            "<input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
             <input type=\"hidden\" name=\"text\" value=\"{text}\">\
             <input type=\"hidden\" name=\"calendar_id\" value=\"{calendar}\">\
             <input type=\"hidden\" name=\"intent\" value=\"{intent}\">",
            csrf = esc(&input.csrf_token),
            text = esc(&input.text),
            calendar = esc(&review.calendar_id),
        )
    };
    format!(
        "<section class=\"bench bench--review\" aria-labelledby=\"bench-review-title\">\
           <div class=\"bench__raw\"><h1 id=\"bench-review-title\">Review quick add</h1>\
             <p>{raw}</p></div>\
           <dl class=\"bench__parse\">\
             <dt>When</dt><dd>{when} ({timezone}, fixed offset)</dd>\
             <dt>Title</dt><dd>{title}</dd>\
             <dt>Calendar</dt><dd>{calendar}</dd>\
           </dl>\
           <div class=\"bench__commit\">\
             <form method=\"post\" action=\"/quick-add\">\
               {commit_hidden}\
               <input type=\"hidden\" name=\"reviewed_title\" value=\"{title}\">\
               <input type=\"hidden\" name=\"reviewed_starts_at\" value=\"{starts}\">\
               <button class=\"btn btn-primary\" type=\"submit\">Add to calendar</button>\
             </form>\
             <form method=\"post\" action=\"/quick-add\">\
               {editor_hidden}\
               <button class=\"btn btn-secondary\" type=\"submit\">Open in editor</button>\
             </form>\
           </div>\
         </section>",
        raw = esc(input.text.trim()),
        when = esc(&review.when),
        timezone = esc(&review.timezone),
        title = esc(&review.title),
        calendar = esc(&review.calendar_name),
        commit_hidden = hidden("commit"),
        editor_hidden = hidden("open_editor"),
        starts = review.starts_at,
    )
}

fn render_quick_add_invalid(
    input: &QuickAddInput,
    field: &'static str,
    message: &'static str,
) -> String {
    format!(
        "<section class=\"bench bench--invalid\">\
           <div class=\"state state--invalid\" role=\"alert\">\
             <h1>Check the highlighted fields.</h1>\
             <p id=\"quickadd-{field}-error\">{message}</p>\
           </div>\
           <form method=\"post\" action=\"/quick-add\" class=\"bench__raw\">\
             <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
             <input type=\"hidden\" name=\"calendar_id\" value=\"{calendar}\">\
             <input type=\"hidden\" name=\"intent\" value=\"open_editor\">\
             <label for=\"quickadd-invalid-text\">Quick add</label>\
             <input id=\"quickadd-invalid-text\" name=\"text\" value=\"{text}\" \
               aria-invalid=\"true\" aria-describedby=\"quickadd-{field}-error\">\
             <button class=\"btn btn-secondary\" type=\"submit\">Open in editor</button>\
           </form>\
         </section>",
        field = field,
        message = esc(message),
        csrf = esc(&input.csrf_token),
        calendar = esc(&input.calendar_id),
        text = esc(&input.text),
    )
}

fn quick_add_html_state(status: StatusCode, kind: &str, message: &str) -> Response {
    let body = render::error_page(
        status.as_u16(),
        if kind == "csrf" {
            "Form expired"
        } else {
            "Quick add unavailable"
        },
        message,
    )
    .replace(
        "class=\"card empty-state\"",
        &format!("class=\"card empty-state state state--{kind}\" data-kind=\"{kind}\""),
    );
    (status, Html(body)).into_response()
}

/// Resolve a parsed quick-add phrase into a real UTC start instant: place the time on the owner's
/// LOCAL target day (relative to their "today"), then convert the local wall clock back to UTC.
/// Shared by the form [`quick_add`] and the JSON [`quick_add_json`] so both agree exactly.
fn quick_add_starts_at(qa: &quickadd::QuickAdd, off: i32, now: i64) -> i64 {
    let local_now = now + off as i64 * 60_000;
    let today_local_mid = calendar::start_of_day(local_now);
    let target_local_mid = quickadd::resolve_local_midnight(qa.day, today_local_mid);
    let local_start = target_local_mid + qa.hour as i64 * 3_600_000 + qa.minute as i64 * 60_000;
    local_start - off as i64 * 60_000
}

/// `POST /quick-add.json` uses the exact same typed evaluator as the HTML form.
pub async fn quick_add_json(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: Result<Json<QuickAddInput>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(form) = match form {
        Ok(form) => form,
        Err(_) => return json_response(
            StatusCode::BAD_REQUEST,
            r#"{"kind":"invalid_input","field":"form","message":"Check the highlighted fields."}"#
                .to_string(),
        ),
    };
    if !auth::verify_csrf(&headers, &form.csrf_token) {
        return json_response(
            StatusCode::FORBIDDEN,
            r#"{"kind":"csrf","message":"This form expired — reload and try again."}"#.to_string(),
        );
    }
    let owner = auth::owner_subject(&headers);
    match evaluate_quick_add(&state, &owner, &form).await {
        QuickAddOutcome::Review(review) => json_response(
            StatusCode::OK,
            format!(
                r#"{{"kind":"review","title":"{title}","starts_at":{starts},"ends_at":{ends},"when":"{when}","calendar_id":"{calendar_id}","calendar_name":"{calendar_name}","timezone":"{timezone}"}}"#,
                title = json_escape(&review.title),
                starts = review.starts_at,
                ends = review.ends_at,
                when = json_escape(&review.when),
                calendar_id = json_escape(&review.calendar_id),
                calendar_name = json_escape(&review.calendar_name),
                timezone = json_escape(&review.timezone),
            ),
        ),
        QuickAddOutcome::InvalidInput { field, message } => json_response(
            StatusCode::BAD_REQUEST,
            format!(
                r#"{{"kind":"invalid_input","field":"{}","message":"{}"}}"#,
                json_escape(field),
                json_escape(message),
            ),
        ),
        QuickAddOutcome::Unavailable { commit_uncertain } => json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                r#"{{"kind":"unavailable","message":"{}"}}"#,
                json_escape(quick_add_unavailable_message(commit_uncertain)),
            ),
        ),
        QuickAddOutcome::Committed(committed) => {
            tracing::info!(target: "audit", event = "event.created", "event quick-added");
            json_response(
                StatusCode::OK,
                format!(
                    r#"{{"kind":"committed","id":"{id}","title":"{title}","starts_at":{starts},"ends_at":{ends},"when":"{when}","href":"{href}"}}"#,
                    id = json_escape(&committed.id),
                    title = json_escape(&committed.title),
                    starts = committed.starts_at,
                    ends = committed.ends_at,
                    when = json_escape(&committed.when),
                    href = json_escape(&committed.href),
                ),
            )
        }
    }
}

fn quick_add_unavailable_message(commit_uncertain: bool) -> &'static str {
    if commit_uncertain {
        "We couldn't confirm whether the event was saved. Check the calendar before deciding what to do next."
    } else {
        "Time interpretation is unavailable — try again later."
    }
}

/// A JSON response with the given status and an explicit JSON content type.
fn json_response(status: StatusCode, body: String) -> Response {
    (
        status,
        [(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// Escape a string for embedding inside a JSON string literal (control chars + quote + backslash).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Calendar management
// ---------------------------------------------------------------------------

pub async fn create_calendar(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: Result<Form<CalendarForm>, FormRejection>,
) -> Result<Response, AppError> {
    let form = form_or_invalid(form)?;
    require_csrf(&headers, &form.csrf_token)?;
    let owner = auth::owner_subject(&headers);
    let calendars = state.store.list_calendars(&owner).await?;
    let position = calendars.iter().map(|c| c.position).max().unwrap_or(0) + 1;
    state
        .store
        .upsert_calendar(Calendar {
            id: auth::random_hex(),
            owner_sub: owner,
            name: normalize_calendar_name(&form.name),
            color: normalize_calendar_color(&form.color),
            position,
        })
        .await?;
    tracing::info!(target: "audit", event = "calendar.created", "calendar created");
    Ok(Redirect::to("/").into_response())
}

pub async fn update_calendar(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    form: Result<Form<CalendarForm>, FormRejection>,
) -> Result<Response, AppError> {
    let id = path_or_invalid(path)?;
    let form = form_or_invalid(form)?;
    require_csrf(&headers, &form.csrf_token)?;
    let owner = auth::owner_subject(&headers);
    let existing = state
        .store
        .get_calendar(&owner, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("That calendar does not exist.".to_string()))?;
    state
        .store
        .upsert_calendar(Calendar {
            id: existing.id,
            owner_sub: owner,
            name: normalize_calendar_name(&form.name),
            color: normalize_calendar_color(&form.color),
            position: existing.position,
        })
        .await?;
    tracing::info!(target: "audit", event = "calendar.updated", "calendar updated");
    Ok(Redirect::to("/").into_response())
}

pub async fn delete_calendar(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    form: Result<Form<DeleteForm>, FormRejection>,
) -> Result<Response, AppError> {
    let id = path_or_invalid(path)?;
    let form = form_or_invalid(form)?;
    require_csrf(&headers, &form.csrf_token)?;
    let owner = auth::owner_subject(&headers);
    if state.store.delete_calendar(&owner, &id).await? {
        tracing::info!(target: "audit", event = "calendar.deleted", "calendar deleted");
    } else {
        tracing::info!(target: "audit", event = "calendar.delete_blocked", "calendar delete blocked");
    }
    Ok(Redirect::to("/").into_response())
}

fn normalize_calendar_name(raw: &str) -> String {
    let name = raw.trim();
    if name.is_empty() {
        "Untitled".to_string()
    } else {
        name.chars().take(80).collect()
    }
}

fn normalize_calendar_color(raw: &str) -> String {
    let color = raw.trim();
    if is_hex_color(color) {
        color.to_ascii_lowercase()
    } else {
        DEFAULT_CALENDAR_COLOR.to_string()
    }
}

// ---------------------------------------------------------------------------
// Form rendering + parsing helpers
// ---------------------------------------------------------------------------

/// View model for [`render_event_form`].
struct FormView {
    action: String,
    title: String,
    calendar_id: String,
    calendars: Vec<Calendar>,
    starts_local: String,
    ends_local: String,
    all_day: bool,
    location: String,
    notes: String,
    /// Recurrence frequency keyword (``/`daily`/`weekly`/`monthly`/`yearly`) for the `<select>`.
    repeat: String,
    repeat_interval: String,
    repeat_count: String,
    repeat_until: String,
    /// Attendees textarea value (`Name <email>` lines).
    attendees: String,
    /// Which reminder presets (minutes-before) are currently checked.
    reminders: Vec<i64>,
    csrf: String,
    is_edit: bool,
    is_occurrence: bool,
    series_id: String,
    occurrence_date: i64,
    id: String,
    errors: Vec<FormFieldError>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FormFieldError {
    field: &'static str,
    message: &'static str,
}

/// A blank (non-recurring) create-form view, optionally prefilled with a `title` — used by the
/// quick-add fallback when a phrase could not be parsed.
fn blank_form_view(csrf: &str, title: &str, calendars: &[Calendar]) -> FormView {
    let (repeat, repeat_interval, repeat_count, repeat_until) = FormView::no_recurrence();
    FormView {
        action: "/new".to_string(),
        title: title.to_string(),
        calendar_id: calendars.first().map(|c| c.id.clone()).unwrap_or_default(),
        calendars: calendars.to_vec(),
        starts_local: String::new(),
        ends_local: String::new(),
        all_day: false,
        location: String::new(),
        notes: String::new(),
        repeat,
        repeat_interval,
        repeat_count,
        repeat_until,
        attendees: String::new(),
        reminders: Vec::new(),
        csrf: csrf.to_string(),
        is_edit: false,
        is_occurrence: false,
        series_id: String::new(),
        occurrence_date: 0,
        id: String::new(),
        errors: Vec::new(),
    }
}

impl FormView {
    /// Recurrence fields for a fresh (non-recurring) form.
    fn no_recurrence() -> (String, String, String, String) {
        (String::new(), "1".to_string(), String::new(), String::new())
    }

    /// Recurrence fields derived from a stored event's RRULE (empty/one-off => `no_recurrence`).
    fn recurrence_from(rrule_text: &str) -> (String, String, String, String) {
        match rrule::RRule::parse(rrule_text) {
            Some(r) => (
                r.freq.keyword().to_string(),
                r.interval.to_string(),
                r.count.map(|c| c.to_string()).unwrap_or_default(),
                r.until_date_input(),
            ),
            None => FormView::no_recurrence(),
        }
    }

    fn from_submission(
        form: &EventForm,
        calendars: Vec<Calendar>,
        action: String,
        errors: Vec<FormFieldError>,
        edit: SubmittedEditContext,
    ) -> Self {
        Self {
            action,
            title: form.title.clone(),
            calendar_id: form.calendar_id.clone(),
            calendars,
            starts_local: form.starts_at.clone(),
            ends_local: form.ends_at.clone(),
            all_day: checkbox_on(form.all_day.as_deref()),
            location: form.location.clone(),
            notes: form.notes.clone(),
            repeat: form.repeat.clone(),
            repeat_interval: form.repeat_interval.clone(),
            repeat_count: form.repeat_count.clone(),
            repeat_until: form.repeat_until.clone(),
            attendees: form.attendees.clone(),
            reminders: reminder_minutes_from_form(form),
            csrf: form.csrf_token.clone(),
            is_edit: edit.is_edit,
            is_occurrence: edit.is_occurrence,
            series_id: edit.series_id,
            occurrence_date: edit.occurrence_date,
            id: edit.id,
            errors,
        }
    }

    fn error(&self, field: &str) -> Option<&'static str> {
        self.errors
            .iter()
            .find(|error| error.field == field)
            .map(|error| error.message)
    }
}

#[derive(Default)]
struct SubmittedEditContext {
    is_edit: bool,
    is_occurrence: bool,
    series_id: String,
    occurrence_date: i64,
    id: String,
}

fn field_aria(v: &FormView, field: &str) -> String {
    if v.error(field).is_some() {
        format!(" aria-invalid=\"true\" aria-describedby=\"{field}-error\"")
    } else {
        String::new()
    }
}

fn field_error(v: &FormView, field: &str) -> String {
    v.error(field)
        .map(|message| {
            format!(
                "<p class=\"field-error\" id=\"{field}-error\">{}</p>",
                esc(message)
            )
        })
        .unwrap_or_default()
}

fn event_error_summary(v: &FormView) -> String {
    if v.errors.is_empty() {
        return String::new();
    }
    let items: String = v
        .errors
        .iter()
        .map(|error| {
            format!(
                "<li><a href=\"#{field}\">{message}</a></li>",
                field = error.field,
                message = esc(error.message),
            )
        })
        .collect();
    format!(
        "<section class=\"error-summary\" role=\"alert\" aria-labelledby=\"event-errors-title\">\
           <h2 id=\"event-errors-title\">Check the event details</h2><ul>{items}</ul>\
         </section>"
    )
}

/// A `<select>` for the recurrence frequency, marking the current choice `selected`.
fn repeat_select(v: &FormView) -> String {
    let opt = |val: &str, label: &str| {
        let sel = if val == v.repeat { " selected" } else { "" };
        format!("<option value=\"{val}\"{sel}>{label}</option>")
    };
    let supported = ["", "daily", "weekly", "monthly", "yearly"];
    let unsupported = if v.error("repeat").is_some()
        && !supported.contains(&v.repeat.trim())
        && !v.repeat.trim().is_empty()
    {
        format!(
            "<option value=\"{}\" selected>Unsupported selection: {}</option>",
            esc(&v.repeat),
            esc(&v.repeat),
        )
    } else {
        String::new()
    };
    format!(
        "<select id=\"repeat\" name=\"repeat\"{}>{}{}{}{}{}{}</select>{}",
        field_aria(v, "repeat"),
        unsupported,
        opt("", "Does not repeat"),
        opt("daily", "Daily"),
        opt("weekly", "Weekly"),
        opt("monthly", "Monthly"),
        opt("yearly", "Yearly"),
        field_error(v, "repeat"),
    )
}

/// The compose recurrence block: frequency + interval + optional COUNT/UNTIL.
fn render_recurrence(v: &FormView) -> String {
    format!(
        "<fieldset class=\"editor__field editor__recur\">\
           <legend>Repeat</legend>\
           <div class=\"editor__row\">\
             <div class=\"editor__field\">\
               <label for=\"repeat\">Frequency</label>\
               {select}\
             </div>\
             <div class=\"editor__field\">\
               <label for=\"repeat_interval\">Every</label>\
               <input id=\"repeat_interval\" type=\"number\" name=\"repeat_interval\" min=\"1\" max=\"999\" value=\"{interval}\"{interval_aria}>\
               {interval_error}\
             </div>\
           </div>\
           <div class=\"editor__row\">\
             <div class=\"editor__field\">\
               <label for=\"repeat_count\">For (occurrences)</label>\
               <input id=\"repeat_count\" type=\"number\" name=\"repeat_count\" min=\"1\" max=\"9999\" value=\"{count}\" placeholder=\"unlimited\"{count_aria}>\
               {count_error}\
             </div>\
             <div class=\"editor__field\">\
               <label for=\"repeat_until\">Until</label>\
               <input id=\"repeat_until\" type=\"date\" name=\"repeat_until\" value=\"{until}\"{until_aria}>\
               {until_error}\
             </div>\
           </div>\
           <p class=\"editor__hint\">Recurring edits and deletes apply to the whole series.</p>\
         </fieldset>",
        select = repeat_select(v),
        interval = esc(&v.repeat_interval),
        interval_aria = field_aria(v, "repeat_interval"),
        interval_error = field_error(v, "repeat_interval"),
        count = esc(&v.repeat_count),
        count_aria = field_aria(v, "repeat_count"),
        count_error = field_error(v, "repeat_count"),
        until = esc(&v.repeat_until),
        until_aria = field_aria(v, "repeat_until"),
        until_error = field_error(v, "repeat_until"),
    )
}

/// The attendees textarea (one `Name <email>` per line).
fn render_attendees_field(v: &FormView) -> String {
    format!(
        "<div class=\"editor__field\">\
           <label for=\"attendees\">Attendees</label>\
           <textarea id=\"attendees\" name=\"attendees\" rows=\"3\" \
             placeholder=\"One per line — Name &lt;email@host&gt; or email@host\">{attendees}</textarea>\
           <p class=\"editor__hint\">Each attendee gets a private RSVP link — open the event page after saving to copy it.</p>\
         </div>",
        attendees = esc(&v.attendees),
    )
}

fn render_calendar_select(v: &FormView) -> String {
    let options: String = v
        .calendars
        .iter()
        .map(|c| {
            let selected = if c.id == v.calendar_id {
                " selected"
            } else {
                ""
            };
            format!(
                "<option value=\"{id}\"{selected}>{name}</option>",
                id = esc(&c.id),
                selected = selected,
                name = esc(&c.name),
            )
        })
        .collect();
    format!(
        "<div class=\"editor__field\">\
           <label for=\"calendar_id\">Calendar</label>\
           <select id=\"calendar_id\" name=\"calendar_id\">{options}</select>\
         </div>",
        options = options,
    )
}

/// The reminder-preset checkbox group.
fn render_reminders(v: &FormView) -> String {
    let boxes: String = REMINDER_PRESETS
        .iter()
        .map(|(suffix, minutes, label)| {
            let checked = if v.reminders.contains(minutes) { " checked" } else { "" };
            format!(
                "<label class=\"check\"><input type=\"checkbox\" name=\"rem_{suffix}\" value=\"on\"{checked}> {label}</label>",
                suffix = suffix,
                checked = checked,
                label = label,
            )
        })
        .collect();
    format!(
        "<fieldset class=\"editor__field editor__recur\">\
           <legend>Reminders</legend>\
           <div class=\"reminders-grid\">{boxes}</div>\
           <p class=\"editor__hint\">Requested as an in-app notification before the event starts.</p>\
         </fieldset>",
        boxes = boxes,
    )
}

/// The reminder minutes-before selected by the form's preset checkboxes.
fn reminder_minutes_from_form(f: &EventForm) -> Vec<i64> {
    let mut out = Vec::new();
    if checkbox_on(f.rem_0.as_deref()) {
        out.push(0);
    }
    if checkbox_on(f.rem_5.as_deref()) {
        out.push(5);
    }
    if checkbox_on(f.rem_10.as_deref()) {
        out.push(10);
    }
    if checkbox_on(f.rem_15.as_deref()) {
        out.push(15);
    }
    if checkbox_on(f.rem_30.as_deref()) {
        out.push(30);
    }
    if checkbox_on(f.rem_60.as_deref()) {
        out.push(60);
    }
    if checkbox_on(f.rem_120.as_deref()) {
        out.push(120);
    }
    if checkbox_on(f.rem_1440.as_deref()) {
        out.push(1440);
    }
    out
}

/// Parse the attendees textarea into `(name, email)` pairs. Lines without a usable email are
/// dropped; duplicate emails (case-insensitive) collapse to the first; capped at [`MAX_ATTENDEES`].
fn parse_attendees(raw: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (name, email) = parse_attendee_line(line);
        if email.is_empty() {
            continue; // an RSVP needs an address
        }
        if seen.insert(email.to_lowercase()) {
            out.push((name, email));
        }
        if out.len() >= MAX_ATTENDEES {
            break;
        }
    }
    out
}

fn attendee_inputs_from_form(form: &EventForm) -> Vec<AttendeeInput> {
    parse_attendees(&form.attendees)
        .into_iter()
        .map(|(name, email)| AttendeeInput { email, name })
        .collect()
}

/// Parse one attendee line: `Name <email>` → (name, email); a bare `email` token → ("", email);
/// otherwise → (line, "") which the caller drops (no address).
fn parse_attendee_line(line: &str) -> (String, String) {
    // The email is the TRAILING `<...>` group, so a name may itself contain angle brackets.
    if let (Some(lt), Some(gt)) = (line.rfind('<'), line.rfind('>')) {
        if lt < gt {
            let email = line[lt + 1..gt].trim();
            let name = line[..lt].trim();
            if email.contains('@') && !email.contains(char::is_whitespace) {
                return (name.to_string(), email.to_string());
            }
        }
    }
    if line.contains('@') && !line.contains(char::is_whitespace) {
        return (String::new(), line.to_string());
    }
    (line.to_string(), String::new())
}

/// Format stored attendees back into textarea lines for the edit form.
fn format_attendees_textarea(attendees: &[Attendee]) -> String {
    attendees
        .iter()
        .map(|a| {
            if a.name.trim().is_empty() {
                a.email.clone()
            } else {
                format!("{} <{}>", a.name, a.email)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Reconcile submitted `(name, email)` pairs against the event's existing attendees: an email that
/// is retained KEEPS its id/token/status (so its RSVP link + reply stay valid), while a new email
/// gets a fresh token and `needs-action`. Removed emails simply fall out of the returned set.
#[cfg(test)]
fn reconcile_attendees(
    owner: &str,
    event_id: &str,
    submitted: &[(String, String)],
    existing: &[Attendee],
    now: i64,
) -> Vec<Attendee> {
    submitted
        .iter()
        .map(|(name, email)| {
            let key = email.to_lowercase();
            match existing
                .iter()
                .find(|e| !e.email.is_empty() && e.email.to_lowercase() == key)
            {
                Some(prev) => Attendee {
                    id: prev.id.clone(),
                    event_id: event_id.to_string(),
                    owner_sub: owner.to_string(),
                    email: email.clone(),
                    name: name.clone(),
                    status: prev.status.clone(),
                    token: prev.token.clone(),
                    created_at: prev.created_at,
                },
                None => Attendee {
                    id: auth::random_hex(),
                    event_id: event_id.to_string(),
                    owner_sub: owner.to_string(),
                    email: email.clone(),
                    name: name.clone(),
                    status: "needs-action".to_string(),
                    token: auth::random_hex(),
                    created_at: now,
                },
            }
        })
        .collect()
}

fn render_event_form(v: &FormView) -> String {
    let recurrence = if v.is_occurrence {
        String::new()
    } else {
        render_recurrence(v)
    };
    let main = format!(
        "<form class=\"card editor\" method=\"post\" action=\"{action}\">\
           <div class=\"editor__head\"><h1>{verb} event</h1></div>\
           {error_summary}\
           <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
           <div class=\"editor__field\">\
             <label for=\"title\">Title</label>\
             <input id=\"title\" type=\"text\" name=\"title\" value=\"{title}\" autocomplete=\"off\" maxlength=\"200\" required{title_aria}>\
             {title_error}\
           </div>\
           {calendar_select}\
           <div class=\"editor__row\">\
             <div class=\"editor__field\">\
               <label for=\"starts_at\">Starts</label>\
               <input id=\"starts_at\" type=\"datetime-local\" name=\"starts_at\" value=\"{starts}\" required{starts_aria}>\
               {starts_error}\
             </div>\
             <div class=\"editor__field\">\
               <label for=\"ends_at\">Ends</label>\
               <input id=\"ends_at\" type=\"datetime-local\" name=\"ends_at\" value=\"{ends}\"{ends_aria}>\
               {ends_error}\
             </div>\
           </div>\
           <div class=\"editor__field\">\
             <label class=\"check\"><input type=\"checkbox\" name=\"all_day\" value=\"on\"{checked}> All-day event</label>\
             <p class=\"editor__hint\">All-day events ignore the time and cover whole UTC days.</p>\
           </div>\
           <div class=\"editor__field\">\
             <label for=\"location\">Location</label>\
             <input id=\"location\" type=\"text\" name=\"location\" value=\"{location}\" autocomplete=\"off\" maxlength=\"200\">\
           </div>\
           <div class=\"editor__field\">\
             <label for=\"notes\">Notes</label>\
             <textarea id=\"notes\" name=\"notes\" rows=\"5\">{notes}</textarea>\
           </div>\
           {attendees}\
           {reminders}\
           {recurrence}\
           <div class=\"editor__actions\">\
             <a class=\"btn btn-secondary\" href=\"/\">Cancel</a>\
             {detail_link}\
             {series_link}\
             <button class=\"btn btn-primary\" type=\"submit\">Save event</button>\
           </div>\
         </form>",
        action = esc(&v.action),
        verb = if v.is_edit { "Edit" } else { "New" },
        error_summary = event_error_summary(v),
        csrf = esc(&v.csrf),
        title = esc(&v.title),
        title_aria = field_aria(v, "title"),
        title_error = field_error(v, "title"),
        calendar_select = render_calendar_select(v),
        starts = esc(&v.starts_local),
        starts_aria = field_aria(v, "starts_at"),
        starts_error = field_error(v, "starts_at"),
        ends = esc(&v.ends_local),
        ends_aria = field_aria(v, "ends_at"),
        ends_error = field_error(v, "ends_at"),
        checked = if v.all_day { " checked" } else { "" },
        location = esc(&v.location),
        notes = esc(&v.notes),
        attendees = render_attendees_field(v),
        reminders = render_reminders(v),
        recurrence = recurrence,
        detail_link = if v.is_edit {
            format!(
                "<a class=\"btn btn-secondary\" href=\"/event/{id}\">Event page</a>",
                id = esc(&v.id)
            )
        } else {
            String::new()
        },
        series_link = if v.is_occurrence {
            format!(
                "<a class=\"btn btn-secondary\" href=\"/edit/{id}?scope=series\">Edit series</a>",
                id = esc(&v.series_id)
            )
        } else {
            String::new()
        },
    );

    if !v.is_edit {
        return main;
    }

    let (delete_action, delete_label, delete_copy, confirm) = if v.is_occurrence {
        (
            format!(
                "/delete/{}?scope=occurrence&occurrence={}",
                v.series_id, v.occurrence_date
            ),
            "Delete occurrence",
            "Delete only this occurrence. The series and other occurrences stay.",
            "Delete only this occurrence?",
        )
    } else if !v.repeat.trim().is_empty() {
        (
            format!("/delete/{}?scope=series", v.id),
            "Delete series",
            "Delete the whole series. All future occurrences, overrides, attendee responses, and reminders are removed. Shared .ics files and inboxes are not affected.",
            "Delete the whole series?",
        )
    } else {
        (
            format!("/delete/{}", v.id),
            "Delete event",
            "This permanently removes the event.",
            "Delete this event?",
        )
    };
    let danger = format!(
        "<form class=\"card danger-zone\" method=\"post\" action=\"{action}\" onsubmit=\"return confirm('{confirm}')\">\
           <div class=\"danger-zone__text\"><strong>{label}</strong><p class=\"muted\">{copy}</p></div>\
           <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
           <button class=\"btn btn-danger\" type=\"submit\">Delete</button>\
         </form>",
        action = esc(&delete_action),
        confirm = confirm,
        label = delete_label,
        csrf = esc(&v.csrf),
        copy = esc(delete_copy),
    );
    format!("{main}{danger}")
}

fn invalid_event_form_response(headers: &HeaderMap, view: &FormView) -> Response {
    let content = format!(
        "{}<div class=\"view-editor\">{}</div>",
        render::subnav("calendar"),
        render_event_form(view)
    );
    let mut response = html_with_csrf_cookie(
        layout(
            if view.is_edit {
                "Edit event"
            } else {
                "New event"
            },
            headers,
            &content,
        ),
        &view.csrf,
    );
    *response.status_mut() = StatusCode::BAD_REQUEST;
    response
}

/// The validated, normalized fields of a submitted event form.
struct ParsedEvent {
    calendar_id: String,
    title: String,
    starts_at: i64,
    ends_at: i64,
    all_day: bool,
    location: String,
    notes: String,
    /// Canonical RRULE built from the compose recurrence fields (empty for a one-off).
    rrule: String,
}

/// Map the compose `repeat` `<select>` keyword to a [`Freq`] (`None` = does not repeat).
fn parse_repeat_freq(v: &str) -> Option<Freq> {
    match v.trim().to_ascii_lowercase().as_str() {
        "daily" => Some(Freq::Daily),
        "weekly" => Some(Freq::Weekly),
        "monthly" => Some(Freq::Monthly),
        "yearly" => Some(Freq::Yearly),
        _ => None,
    }
}

/// Validate + normalize an [`EventForm`] into a [`ParsedEvent`]. The `datetime-local` inputs are
/// interpreted in the owner's timezone (`off` minutes from UTC) and stored as real UTC epoch ms.
/// All-day events snap to whole LOCAL days; a missing/earlier end collapses to the start so a
/// range is never negative.
fn parse_event_form(form: &EventForm, off: i32) -> Result<ParsedEvent, Vec<FormFieldError>> {
    let mut errors = Vec::new();
    let title = form.title.trim().to_string();
    if title.is_empty() {
        errors.push(FormFieldError {
            field: "title",
            message: "An event needs a title.",
        });
    }

    let all_day = checkbox_on(form.all_day.as_deref());
    let starts = calendar::parse_datetime_local_at(&form.starts_at, off);
    if starts.is_none() {
        errors.push(FormFieldError {
            field: "starts_at",
            message: "A valid start date and time is required.",
        });
    }
    let ends = if form.ends_at.trim().is_empty() {
        None
    } else {
        let parsed = calendar::parse_datetime_local_at(&form.ends_at, off);
        if parsed.is_none() {
            errors.push(FormFieldError {
                field: "ends_at",
                message: "Use a valid end date and time, or leave it blank.",
            });
        }
        parsed
    };
    if starts.zip(ends).is_some_and(|(start, end)| end < start) {
        errors.push(FormFieldError {
            field: "ends_at",
            message: "The end cannot be before the start.",
        });
    }

    let repeat = form.repeat.trim();
    let freq = parse_repeat_freq(repeat);
    if !repeat.is_empty() && freq.is_none() {
        errors.push(FormFieldError {
            field: "repeat",
            message: "Choose one of the supported recurrence rules.",
        });
    }
    let interval = if freq.is_some() {
        let parsed = form
            .repeat_interval
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|value| (1..=999).contains(value));
        if parsed.is_none() {
            errors.push(FormFieldError {
                field: "repeat_interval",
                message: "Repeat interval must be a number from 1 through 999.",
            });
        }
        parsed.unwrap_or(1)
    } else {
        1
    };
    let count = if form.repeat_count.trim().is_empty() {
        None
    } else {
        let parsed = form
            .repeat_count
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|value| (1..=9999).contains(value));
        if parsed.is_none() {
            errors.push(FormFieldError {
                field: "repeat_count",
                message: "Repeat count must be a number from 1 through 9999.",
            });
        }
        parsed
    };
    let until = Some(form.repeat_until.trim()).filter(|u| !u.is_empty());
    if until.is_some_and(|date| calendar::parse_date(date).is_none()) {
        errors.push(FormFieldError {
            field: "repeat_until",
            message: "Repeat-until must be a valid date.",
        });
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    let mut starts_at = starts.expect("validated start");
    let mut ends_at = ends.unwrap_or(starts_at);
    if all_day {
        starts_at = calendar::start_of_day_at(starts_at, off);
        ends_at = calendar::end_of_day_at(ends_at, off);
    }
    let rrule = rrule::build_rrule(freq, interval, count, until);

    Ok(ParsedEvent {
        calendar_id: form.calendar_id.trim().to_string(),
        title,
        starts_at,
        ends_at,
        all_day,
        location: form.location.trim().to_string(),
        notes: form.notes.trim().to_string(),
        rrule,
    })
}

fn checkbox_on(v: Option<&str>) -> bool {
    matches!(v, Some("on" | "true" | "1" | "yes"))
}

/// 303 back to the LOCAL month that contains `ms` so the just-saved event is visible.
fn redirect_to_month(ms: i64, off: i32) -> Response {
    let (y, m) = calendar::month_of_at(ms, off);
    Redirect::to(&format!("/?y={y}&m={m}")).into_response()
}

/// CSRF guard shared by every state-changing POST (also used by the contacts handlers).
pub(crate) fn require_csrf(headers: &HeaderMap, token: &str) -> Result<(), AppError> {
    if auth::verify_csrf(headers, token) {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "CSRF token missing or invalid — reload the page and try again.".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_attendees_extracts_name_email_pairs() {
        let raw = "Grace Hopper <grace@navy.mil>\n\
                   bob@x.co\n\
                   Just A Name\n\
                   grace@navy.mil\n\
                   ";
        let got = parse_attendees(raw);
        // Name+email, bare email; the name-only line is dropped (no address); duplicate collapses.
        assert_eq!(
            got,
            vec![
                ("Grace Hopper".to_string(), "grace@navy.mil".to_string()),
                (String::new(), "bob@x.co".to_string()),
            ]
        );
    }

    #[test]
    fn reconcile_preserves_token_and_status_for_retained_email() {
        let existing = vec![Attendee {
            id: "a1".into(),
            event_id: "e1".into(),
            owner_sub: "alice".into(),
            email: "guest@x.co".into(),
            name: "Guest".into(),
            status: "accepted".into(),
            token: "keep-tok".into(),
            created_at: 100,
        }];
        // Same email (different case) retained + a brand-new one added.
        let submitted = vec![
            ("Guest Renamed".to_string(), "GUEST@x.co".to_string()),
            (String::new(), "new@x.co".to_string()),
        ];
        let out = reconcile_attendees("alice", "e1", &submitted, &existing, 200);
        assert_eq!(out.len(), 2);
        let retained = out
            .iter()
            .find(|a| a.email.eq_ignore_ascii_case("guest@x.co"))
            .unwrap();
        assert_eq!(retained.token, "keep-tok", "RSVP link survives the edit");
        assert_eq!(retained.status, "accepted", "status survives the edit");
        assert_eq!(retained.name, "Guest Renamed", "display name updates");
        assert_eq!(retained.id, "a1", "row identity preserved");
        let added = out.iter().find(|a| a.email == "new@x.co").unwrap();
        assert_eq!(added.status, "needs-action");
        assert_ne!(added.token, "keep-tok");
        assert!(!added.token.is_empty());
    }

    #[test]
    fn reminder_minutes_reads_checked_presets() {
        let mut f = EventForm {
            csrf_token: String::new(),
            title: String::new(),
            calendar_id: String::new(),
            starts_at: String::new(),
            ends_at: String::new(),
            all_day: None,
            location: String::new(),
            notes: String::new(),
            repeat: String::new(),
            repeat_interval: String::new(),
            repeat_count: String::new(),
            repeat_until: String::new(),
            attendees: String::new(),
            rem_0: None,
            rem_5: None,
            rem_10: Some("on".to_string()),
            rem_15: None,
            rem_30: None,
            rem_60: Some("on".to_string()),
            rem_120: None,
            rem_1440: None,
        };
        assert_eq!(reminder_minutes_from_form(&f), vec![10, 60]);
        f.rem_0 = Some("on".to_string());
        assert_eq!(reminder_minutes_from_form(&f), vec![0, 10, 60]);
    }

    #[test]
    fn uncertain_commit_copy_requires_calendar_check_not_retry() {
        let message = quick_add_unavailable_message(true);
        assert!(message.contains("couldn't confirm"));
        assert!(message.contains("Check the calendar"));
        assert!(!message.to_ascii_lowercase().contains("retry"));
        assert!(!message.to_ascii_lowercase().contains("try again"));
    }
}
