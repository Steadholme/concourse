//! Calendar + event handlers: the month grid with an upcoming agenda, plus event create / edit /
//! delete.
//!
//! Every request derives the OWNER from the gateway-injected `X-Auth-Subject` (via
//! [`auth::owner_subject`]) and scopes all store calls to it, so users never see or touch each
//! other's events. Every state-changing POST is double-submit CSRF checked, and all
//! user-supplied text is HTML-escaped on the way out.

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;

use crate::auth;
use crate::calendar::{self, DayCell, MonthView};
use crate::config::{AGENDA_HORIZON_MS, AGENDA_LIMIT, DAY_CHIP_LIMIT};
use crate::error::AppError;
use crate::handlers::html_with_csrf_cookie;
use crate::render::{self, esc, layout};
use crate::rrule::{self, Freq};
use crate::store::Event;
use crate::{now_ms, AppState};

/// `?y=&m=` month navigation on the calendar index.
#[derive(Debug, Deserialize)]
pub struct MonthQuery {
    #[serde(default)]
    pub y: Option<i32>,
    #[serde(default)]
    pub m: Option<u8>,
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
}

/// A bare CSRF-only body, used by the delete forms.
#[derive(Debug, Deserialize)]
pub struct DeleteForm {
    #[serde(default)]
    pub csrf_token: String,
}

// ---------------------------------------------------------------------------
// GET /  — month grid + agenda
// ---------------------------------------------------------------------------

pub async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<MonthQuery>,
) -> Result<Response, AppError> {
    let owner = auth::owner_subject(&headers);
    let settings = state.store.get_settings(&owner).await?;
    let off = calendar::tz_offset_minutes(&settings.timezone);
    let monday_first = settings.week_start.eq_ignore_ascii_case("monday");
    let now = now_ms();
    let (cur_y, cur_m) = calendar::month_of_at(now, off);
    let year = q.y.unwrap_or(cur_y);
    let month = match q.m {
        Some(m) if (1..=12).contains(&m) => m,
        _ => cur_m,
    };

    let stored = state.store.list_events(&owner).await?;
    let view = calendar::build_month_at(year, month, now, off, monday_first);

    // Expand recurring series into concrete occurrences visible in this render: the month grid's
    // span plus the upcoming-agenda horizon. Non-recurring events pass through unchanged.
    let (grid_start, grid_end) = grid_bounds(&view);
    let win_start = grid_start.min(now);
    let win_end = grid_end.max(now + AGENDA_HORIZON_MS);
    let events = rrule::expand_events(&stored, win_start, win_end);

    let csrf = auth::new_csrf_token();

    let content = format!(
        "{}{}",
        render::subnav("calendar"),
        render_calendar(&view, &events, now, &csrf, off, monday_first, &settings.timezone)
    );
    let html = layout("Calendar", &headers, &content);
    Ok(html_with_csrf_cookie(html, &csrf))
}

fn render_calendar(
    view: &MonthView,
    events: &[Event],
    now: i64,
    csrf: &str,
    off: i32,
    monday_first: bool,
    tz_label: &str,
) -> String {
    let head = format!(
        "<div class=\"cal-head\">\
           <h1>{month} {year}</h1>\
           <div class=\"cal-nav\">\
             <a class=\"btn btn-secondary btn-sm\" href=\"/?y={py}&amp;m={pm}\" aria-label=\"Previous month\">‹ Prev</a>\
             <a class=\"btn btn-secondary btn-sm\" href=\"/\">Today</a>\
             <a class=\"btn btn-secondary btn-sm\" href=\"/?y={ny}&amp;m={nm}\" aria-label=\"Next month\">Next ›</a>\
             <a class=\"btn btn-primary btn-sm\" href=\"/new\">New event</a>\
           </div>\
         </div>",
        month = view.month_name,
        year = view.year,
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
                Some(c) => weeks.push_str(&render_day_cell(c, events, off)),
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
         <p class=\"site-foot\">HOLDFAST Almanac · personal calendar &amp; contacts · times shown in {tz}</p>",
        head = head,
        weekdays = weekdays,
        weeks = weeks,
        agenda = render_agenda(events, now, csrf, off),
        tz = esc(tz_label),
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

/// A small "repeats" glyph appended to recurring-series chips/labels (occurrences carry the series
/// `rrule`). Empty for one-off events.
fn recur_mark(e: &Event) -> &'static str {
    if e.rrule.trim().is_empty() {
        ""
    } else {
        " ↻"
    }
}

fn render_day_cell(c: &DayCell, events: &[Event], off: i32) -> String {
    let day_events: Vec<&Event> = events
        .iter()
        .filter(|e| e.starts_at <= c.end_ms && e.ends_at >= c.start_ms)
        .collect();

    let mut chips = String::new();
    for e in day_events.iter().take(DAY_CHIP_LIMIT) {
        let label = if e.all_day {
            format!("{}{}", esc(&e.title), recur_mark(e))
        } else {
            format!(
                "{} {}{}",
                calendar::human_time_at(e.starts_at, off),
                esc(&e.title),
                recur_mark(e)
            )
        };
        let tooltip = esc(&format!(
            "{} — {}",
            calendar::fmt_event_when_at(e.starts_at, e.ends_at, e.all_day, off),
            e.title
        ));
        chips.push_str(&format!(
            "<a class=\"cal-event{ad}\" href=\"/edit/{id}\" title=\"{tooltip}\">{label}</a>",
            ad = if e.all_day { " cal-event--allday" } else { "" },
            id = esc(&e.id),
            tooltip = tooltip,
            label = label,
        ));
    }
    if day_events.len() > DAY_CHIP_LIMIT {
        chips.push_str(&format!(
            "<span class=\"cal-more\">+{} more</span>",
            day_events.len() - DAY_CHIP_LIMIT
        ));
    }

    format!(
        "<div class=\"cal-day{today}\">\
           <a class=\"cal-daynum\" href=\"/new?date={iso}\" title=\"Add an event on this day\">{day}</a>\
           <div class=\"cal-events\">{chips}</div>\
         </div>",
        today = if c.is_today { " cal-day--today" } else { "" },
        iso = esc(&c.iso_date),
        day = c.day,
        chips = chips,
    )
}

fn render_agenda(events: &[Event], now: i64, csrf: &str, off: i32) -> String {
    // `events` is already sorted by starts_at ascending; keep only those that haven't ended.
    let upcoming: Vec<&Event> = events
        .iter()
        .filter(|e| e.ends_at >= now)
        .take(AGENDA_LIMIT)
        .collect();

    let body = if upcoming.is_empty() {
        "<p class=\"agenda-empty\">No upcoming events. <a href=\"/new\">Add one.</a></p>".to_string()
    } else {
        upcoming.iter().map(|e| render_agenda_item(e, csrf, off)).collect()
    };

    format!(
        "<section class=\"agenda\"><h2 class=\"agenda__head\">Upcoming</h2>{body}</section>",
        body = body,
    )
}

fn render_agenda_item(e: &Event, csrf: &str, off: i32) -> String {
    let loc = if e.location.trim().is_empty() {
        String::new()
    } else {
        format!("<span class=\"agenda__loc\">{}</span>", esc(&e.location))
    };
    format!(
        "<div class=\"agenda__item\">\
           <div class=\"agenda__when\">{when}</div>\
           <div class=\"agenda__body\">\
             <a class=\"agenda__title\" href=\"/edit/{id}\">{title}{recur}</a>\
             {loc}\
           </div>\
           <form class=\"agenda__del\" method=\"post\" action=\"/delete/{id}\" onsubmit=\"return confirm('Delete this event?')\">\
             <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
             <button class=\"btn btn-ghost btn-sm\" type=\"submit\">Delete</button>\
           </form>\
         </div>",
        when = esc(&calendar::fmt_event_when_at(e.starts_at, e.ends_at, e.all_day, off)),
        id = esc(&e.id),
        title = esc(&e.title),
        recur = recur_mark(e),
        loc = loc,
        csrf = esc(csrf),
    )
}

// ---------------------------------------------------------------------------
// GET /new  — the create form
// ---------------------------------------------------------------------------

pub async fn new_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<NewQuery>,
) -> Result<Response, AppError> {
    let owner = auth::owner_subject(&headers);
    let off = calendar::tz_offset_minutes(&state.store.get_settings(&owner).await?.timezone);
    let csrf = auth::new_csrf_token();

    // If a day was clicked, default to a 09:00–10:00 LOCAL slot on that day; else leave blank.
    let (starts_local, ends_local) = match q.date.as_deref().and_then(|d| calendar::parse_date_at(d, off)) {
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
        starts_local,
        ends_local,
        all_day: false,
        location: String::new(),
        notes: String::new(),
        repeat,
        repeat_interval,
        repeat_count,
        repeat_until,
        csrf: csrf.clone(),
        is_edit: false,
        id: String::new(),
    };
    let content = format!("{}{}", render::subnav("calendar"), render_event_form(&view));
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
    Form(form): Form<EventForm>,
) -> Result<Response, AppError> {
    require_csrf(&headers, &form.csrf_token)?;
    let owner = auth::owner_subject(&headers);
    let off = calendar::tz_offset_minutes(&state.store.get_settings(&owner).await?.timezone);
    let parsed = parse_event_form(&form, off)?;

    state
        .store
        .upsert_event(Event {
            id: auth::random_hex(),
            owner_sub: owner,
            title: parsed.title,
            starts_at: parsed.starts_at,
            ends_at: parsed.ends_at,
            all_day: parsed.all_day,
            location: parsed.location,
            notes: parsed.notes,
            rrule: parsed.rrule,
            created_at: now_ms(),
        })
        .await?;

    Ok(redirect_to_month(parsed.starts_at, off))
}

// ---------------------------------------------------------------------------
// GET /edit/{id}  — the edit form
// ---------------------------------------------------------------------------

pub async fn edit_form(
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

    let csrf = auth::new_csrf_token();
    let (repeat, repeat_interval, repeat_count, repeat_until) =
        FormView::recurrence_from(&event.rrule);
    let view = FormView {
        action: format!("/edit/{}", event.id),
        title: event.title.clone(),
        starts_local: calendar::fmt_datetime_local_at(event.starts_at, off),
        ends_local: calendar::fmt_datetime_local_at(event.ends_at, off),
        all_day: event.all_day,
        location: event.location.clone(),
        notes: event.notes.clone(),
        repeat,
        repeat_interval,
        repeat_count,
        repeat_until,
        csrf: csrf.clone(),
        is_edit: true,
        id: event.id.clone(),
    };
    let content = format!("{}{}", render::subnav("calendar"), render_event_form(&view));
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
    Path(id): Path<String>,
    Form(form): Form<EventForm>,
) -> Result<Response, AppError> {
    require_csrf(&headers, &form.csrf_token)?;
    let owner = auth::owner_subject(&headers);
    let off = calendar::tz_offset_minutes(&state.store.get_settings(&owner).await?.timezone);

    let existing = state
        .store
        .get_event(&owner, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("That event does not exist.".to_string()))?;
    let parsed = parse_event_form(&form, off)?;

    state
        .store
        .upsert_event(Event {
            id: existing.id,
            owner_sub: owner,
            title: parsed.title,
            starts_at: parsed.starts_at,
            ends_at: parsed.ends_at,
            all_day: parsed.all_day,
            location: parsed.location,
            notes: parsed.notes,
            rrule: parsed.rrule,
            created_at: existing.created_at,
        })
        .await?;

    Ok(redirect_to_month(parsed.starts_at, off))
}

// ---------------------------------------------------------------------------
// POST /delete/{id}  — delete an event
// ---------------------------------------------------------------------------

pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<DeleteForm>,
) -> Result<Response, AppError> {
    require_csrf(&headers, &form.csrf_token)?;
    let owner = auth::owner_subject(&headers);
    state.store.delete_event(&owner, &id).await?;
    Ok(Redirect::to("/").into_response())
}

// ---------------------------------------------------------------------------
// Form rendering + parsing helpers
// ---------------------------------------------------------------------------

/// View model for [`render_event_form`].
struct FormView {
    action: String,
    title: String,
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
    csrf: String,
    is_edit: bool,
    id: String,
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
}

/// A `<select>` for the recurrence frequency, marking the current choice `selected`.
fn repeat_select(current: &str) -> String {
    let opt = |val: &str, label: &str| {
        let sel = if val == current { " selected" } else { "" };
        format!("<option value=\"{val}\"{sel}>{label}</option>")
    };
    format!(
        "<select id=\"repeat\" name=\"repeat\">{}{}{}{}{}</select>",
        opt("", "Does not repeat"),
        opt("daily", "Daily"),
        opt("weekly", "Weekly"),
        opt("monthly", "Monthly"),
        opt("yearly", "Yearly"),
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
               <input id=\"repeat_interval\" type=\"number\" name=\"repeat_interval\" min=\"1\" max=\"999\" value=\"{interval}\">\
             </div>\
           </div>\
           <div class=\"editor__row\">\
             <div class=\"editor__field\">\
               <label for=\"repeat_count\">For (occurrences)</label>\
               <input id=\"repeat_count\" type=\"number\" name=\"repeat_count\" min=\"1\" max=\"9999\" value=\"{count}\" placeholder=\"unlimited\">\
             </div>\
             <div class=\"editor__field\">\
               <label for=\"repeat_until\">Until</label>\
               <input id=\"repeat_until\" type=\"date\" name=\"repeat_until\" value=\"{until}\">\
             </div>\
           </div>\
           <p class=\"editor__hint\">Recurring edits and deletes apply to the whole series.</p>\
         </fieldset>",
        select = repeat_select(&v.repeat),
        interval = esc(&v.repeat_interval),
        count = esc(&v.repeat_count),
        until = esc(&v.repeat_until),
    )
}

fn render_event_form(v: &FormView) -> String {
    let main = format!(
        "<form class=\"card editor\" method=\"post\" action=\"{action}\">\
           <div class=\"editor__head\"><h1>{verb} event</h1></div>\
           <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
           <div class=\"editor__field\">\
             <label for=\"title\">Title</label>\
             <input id=\"title\" type=\"text\" name=\"title\" value=\"{title}\" autocomplete=\"off\" maxlength=\"200\" required>\
           </div>\
           <div class=\"editor__row\">\
             <div class=\"editor__field\">\
               <label for=\"starts_at\">Starts</label>\
               <input id=\"starts_at\" type=\"datetime-local\" name=\"starts_at\" value=\"{starts}\" required>\
             </div>\
             <div class=\"editor__field\">\
               <label for=\"ends_at\">Ends</label>\
               <input id=\"ends_at\" type=\"datetime-local\" name=\"ends_at\" value=\"{ends}\">\
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
           {recurrence}\
           <div class=\"editor__actions\">\
             <a class=\"btn btn-secondary\" href=\"/\">Cancel</a>\
             <button class=\"btn btn-primary\" type=\"submit\">Save event</button>\
           </div>\
         </form>",
        action = esc(&v.action),
        verb = if v.is_edit { "Edit" } else { "New" },
        csrf = esc(&v.csrf),
        title = esc(&v.title),
        starts = esc(&v.starts_local),
        ends = esc(&v.ends_local),
        checked = if v.all_day { " checked" } else { "" },
        location = esc(&v.location),
        notes = esc(&v.notes),
        recurrence = render_recurrence(v),
    );

    if !v.is_edit {
        return main;
    }

    let danger = format!(
        "<form class=\"card danger-zone\" method=\"post\" action=\"/delete/{id}\" onsubmit=\"return confirm('Delete this event?')\">\
           <div class=\"danger-zone__text\"><strong>Delete event</strong><p class=\"muted\">This permanently removes the event.</p></div>\
           <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
           <button class=\"btn btn-danger\" type=\"submit\">Delete</button>\
         </form>",
        id = esc(&v.id),
        csrf = esc(&v.csrf),
    );
    format!("{main}{danger}")
}

/// The validated, normalized fields of a submitted event form.
struct ParsedEvent {
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
fn parse_event_form(form: &EventForm, off: i32) -> Result<ParsedEvent, AppError> {
    let title = form.title.trim().to_string();
    if title.is_empty() {
        return Err(AppError::BadRequest("An event needs a title.".to_string()));
    }

    let all_day = checkbox_on(form.all_day.as_deref());
    let mut starts_at = calendar::parse_datetime_local_at(&form.starts_at, off)
        .ok_or_else(|| AppError::BadRequest("A valid start date and time is required.".to_string()))?;
    let mut ends_at = calendar::parse_datetime_local_at(&form.ends_at, off).unwrap_or(starts_at);

    if all_day {
        starts_at = calendar::start_of_day_at(starts_at, off);
        ends_at = calendar::end_of_day_at(ends_at.max(starts_at), off);
    } else if ends_at < starts_at {
        ends_at = starts_at;
    }

    let freq = parse_repeat_freq(&form.repeat);
    let interval = form.repeat_interval.trim().parse::<u32>().unwrap_or(1).max(1);
    let count = form.repeat_count.trim().parse::<u32>().ok().filter(|c| *c > 0);
    let until = Some(form.repeat_until.trim()).filter(|u| !u.is_empty());
    let rrule = rrule::build_rrule(freq, interval, count, until);

    Ok(ParsedEvent {
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
