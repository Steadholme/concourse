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
use crate::calendar::{self, DayCell, GridDay, MonthView, TimeGridView};
use crate::config::{AGENDA_HORIZON_MS, AGENDA_LIMIT, DAY_CHIP_LIMIT};
use crate::error::AppError;
use crate::handlers::html_with_csrf_cookie;
use crate::render::{self, esc, layout};
use crate::rrule::{self, Freq};
use crate::store::{Attendee, Event, Reminder};
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
}

/// Which calendar view the index renders.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewKind {
    Month,
    Week,
    Day,
}

impl ViewKind {
    /// Parse the `?view=` keyword (anything unrecognised falls back to the month grid).
    fn parse(v: Option<&str>) -> ViewKind {
        match v.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("week") => ViewKind::Week,
            Some("day") => ViewKind::Day,
            _ => ViewKind::Month,
        }
    }

    /// The `?view=` keyword (also the page title stem).
    fn key(self) -> &'static str {
        match self {
            ViewKind::Month => "month",
            ViewKind::Week => "week",
            ViewKind::Day => "day",
        }
    }
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

/// A bare CSRF-only body, used by the delete forms.
#[derive(Debug, Deserialize)]
pub struct DeleteForm {
    #[serde(default)]
    pub csrf_token: String,
}

/// The quick-add box body: a raw phrase like `Lunch tomorrow 12pm`.
#[derive(Debug, Deserialize)]
pub struct QuickAddForm {
    #[serde(default)]
    pub csrf_token: String,
    #[serde(default)]
    pub text: String,
}

// ---------------------------------------------------------------------------
// GET /  — month grid + agenda
// ---------------------------------------------------------------------------

pub async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<IndexQuery>,
) -> Result<Response, AppError> {
    let owner = auth::owner_subject(&headers);
    let settings = state.store.get_settings(&owner).await?;
    let off = calendar::tz_offset_minutes(&settings.timezone);
    let monday_first = settings.week_start.eq_ignore_ascii_case("monday");
    let now = now_ms();
    let stored = state.store.list_events(&owner).await?;
    let csrf = auth::new_csrf_token();
    let kind = ViewKind::parse(q.view.as_deref());

    let inner = match kind {
        ViewKind::Month => {
            let (cur_y, cur_m) = calendar::month_of_at(now, off);
            let year = q.y.unwrap_or(cur_y);
            let month = match q.m {
                Some(m) if (1..=12).contains(&m) => m,
                _ => cur_m,
            };
            let view = calendar::build_month_at(year, month, now, off, monday_first);
            // Expand recurring series into the concrete occurrences visible in this render: the
            // month grid's span plus the upcoming-agenda horizon. One-offs pass through unchanged.
            let (grid_start, grid_end) = grid_bounds(&view);
            let win_start = grid_start.min(now);
            let win_end = grid_end.max(now + AGENDA_HORIZON_MS);
            let events = rrule::expand_events(&stored, win_start, win_end);
            render_calendar(&view, &events, now, &csrf, off, monday_first, &settings.timezone)
        }
        ViewKind::Week | ViewKind::Day => {
            // Anchor on `?date=` (owner-local), else today.
            let anchor = q
                .date
                .as_deref()
                .and_then(|d| calendar::parse_date_at(d, off))
                .unwrap_or(now);
            let grid = match kind {
                ViewKind::Week => calendar::build_week_at(anchor, now, off, monday_first),
                _ => calendar::build_day_at(anchor, now, off),
            };
            // Expand only across the grid's own span — the same reused event fetch + RRULE expander.
            let events = rrule::expand_events(&stored, grid.win_start, grid.win_end);
            render_time_grid(&grid, &events, now, off, kind, &settings.timezone)
        }
    };

    let content = format!(
        "{}{}{}",
        render::subnav("calendar"),
        render_quick_add(&csrf),
        inner
    );
    let title = match kind {
        ViewKind::Week => "Week",
        ViewKind::Day => "Day",
        ViewKind::Month => "Calendar",
    };
    let html = layout(title, &headers, &content);
    Ok(html_with_csrf_cookie(html, &csrf))
}

/// The natural-language quick-add box shown above every calendar view. Posts the raw phrase to
/// `/quick-add`; an unparseable phrase re-renders the editor prefilled (see [`quick_add`]).
fn render_quick_add(csrf: &str) -> String {
    format!(
        "<form class=\"card quickadd\" method=\"post\" action=\"/quick-add\">\
           <input type=\"hidden\" name=\"csrf_token\" value=\"{csrf}\">\
           <input class=\"quickadd__input\" type=\"text\" name=\"text\" autocomplete=\"off\" \
             maxlength=\"200\" placeholder=\"Quick add — e.g. Lunch tomorrow 12pm\" \
             aria-label=\"Quick add an event\">\
           <button class=\"btn btn-primary\" type=\"submit\">Add</button>\
         </form>",
        csrf = esc(csrf),
    )
}

/// The Month / Week / Day switch pills, threading `anchor` (an owner-local `YYYY-MM-DD`) so
/// switching views keeps the reader on the same date. `active` is the currently-rendered view.
fn view_switch(active: ViewKind, anchor: &str) -> String {
    let pill = |kind: ViewKind, label: &str, href: &str| {
        let cls = if kind == active {
            "btn btn-primary btn-sm"
        } else {
            "btn btn-secondary btn-sm"
        };
        format!("<a class=\"{cls}\" href=\"{}\">{label}</a>", esc(href))
    };
    format!(
        "<span class=\"cal-viewswitch\">{}{}{}</span>",
        pill(ViewKind::Month, "Month", "/"),
        pill(ViewKind::Week, "Week", &format!("/?view=week&date={anchor}")),
        pill(ViewKind::Day, "Day", &format!("/?view=day&date={anchor}")),
    )
}

/// A week/day time-grid: a header + nav, an all-day strip, and an hour grid with events placed by
/// their start/end. Reuses the same overlap filter + timezone helpers as the month grid.
fn render_time_grid(
    g: &TimeGridView,
    events: &[Event],
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
    let cols_style = format!(
        "grid-template-columns: 56px repeat({}, minmax(0, 1fr))",
        g.days.len()
    );

    // Day headers + the all-day strip (both share the header grid).
    let mut dayheads = String::new();
    let mut alldays = String::new();
    for d in &g.days {
        let day_href = esc(&format!("/?view=day&date={}", d.iso_date));
        dayheads.push_str(&format!(
            "<a class=\"tgrid__dayhead{today}\" href=\"{href}\">\
               <span class=\"tgrid__dow\">{dow}</span>\
               <span class=\"tgrid__dnum\">{mon} {day}</span>\
             </a>",
            today = if d.is_today { " tgrid__dayhead--today" } else { "" },
            href = day_href,
            dow = d.weekday,
            mon = d.month_abbr,
            day = d.day,
        ));
        alldays.push_str(&format!(
            "<div class=\"tgrid__allday-cell\">{}</div>",
            render_allday_chips(d, events, off)
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
            events = render_day_column(d, events, off),
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
         <p class=\"site-foot\">HOLDFAST Almanac · personal calendar &amp; contacts · times shown in {tz}</p>",
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
fn render_allday_chips(d: &GridDay, events: &[Event], off: i32) -> String {
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
                "<a class=\"tgrid__allday-event\" href=\"/edit/{id}\" title=\"{tip}\">{title}{recur}</a>",
                id = esc(&e.id),
                tip = tooltip,
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
        "<div class=\"tgrid__now\" style=\"top:{:.1}px\"></div>",
        min * PX_PER_MIN
    )
}

/// Place a day column's timed events on the hour grid. Events are absolutely positioned by their
/// minute-of-day (clamped to the visible day) and packed into side-by-side lanes so overlapping
/// events never hide one another. `events` is pre-sorted by `starts_at` (see `expand_events`).
fn render_day_column(d: &GridDay, events: &[Event], off: i32) -> String {
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
                "<a class=\"tgrid__event\" href=\"/edit/{id}\" \
                   style=\"top:{top:.1}px;height:{height:.1}px;left:{left:.4}%;width:{width:.4}%\" \
                   title=\"{tip}\">\
                   <span class=\"tgrid__event-time\">{start}</span> {title}{recur}\
                 </a>",
                id = esc(&e.id),
                top = top,
                height = height,
                left = left,
                width = width,
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
        attendees: String::new(),
        reminders: Vec::new(),
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
    let now = now_ms();
    let id = auth::random_hex();

    state
        .store
        .upsert_event(Event {
            id: id.clone(),
            owner_sub: owner.clone(),
            title: parsed.title,
            starts_at: parsed.starts_at,
            ends_at: parsed.ends_at,
            all_day: parsed.all_day,
            location: parsed.location,
            notes: parsed.notes,
            rrule: parsed.rrule,
            created_at: now,
        })
        .await?;
    save_attendees_and_reminders(&state, &owner, &id, &form, now).await?;

    tracing::info!(target: "audit", event = "event.created", "event created");
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
    let attendees = state.store.list_attendees(&owner, &id).await?;
    let reminders: Vec<i64> = state
        .store
        .list_reminders(&owner, &id)
        .await?
        .iter()
        .map(|r| r.minutes_before)
        .collect();
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
        attendees: format_attendees_textarea(&attendees),
        reminders,
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
    let now = now_ms();

    state
        .store
        .upsert_event(Event {
            id: existing.id,
            owner_sub: owner.clone(),
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
    save_attendees_and_reminders(&state, &owner, &id, &form, now).await?;

    tracing::info!(target: "audit", event = "event.updated", "event updated");
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
    // Cascade: an event's attendees + reminders go with it (both ownership-scoped).
    state.store.delete_event_attendees(&owner, &id).await?;
    state.store.delete_event_reminders(&owner, &id).await?;
    tracing::info!(target: "audit", event = "event.deleted", "event deleted");
    Ok(Redirect::to("/").into_response())
}

// ---------------------------------------------------------------------------
// POST /quick-add  — natural-language quick-add
// ---------------------------------------------------------------------------

pub async fn quick_add(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<QuickAddForm>,
) -> Result<Response, AppError> {
    require_csrf(&headers, &form.csrf_token)?;
    let owner = auth::owner_subject(&headers);
    let off = calendar::tz_offset_minutes(&state.store.get_settings(&owner).await?.timezone);
    let raw = form.text.trim();

    match quickadd::parse_quick_add(raw) {
        Some(qa) => {
            let now = now_ms();
            // Resolve the relative day against the owner's LOCAL "today", place the time, then
            // convert the local wall clock back to a real UTC instant to store.
            let local_now = now + off as i64 * 60_000;
            let today_local_mid = calendar::start_of_day(local_now);
            let target_local_mid = quickadd::resolve_local_midnight(qa.day, today_local_mid);
            let local_start =
                target_local_mid + qa.hour as i64 * 3_600_000 + qa.minute as i64 * 60_000;
            let starts_at = local_start - off as i64 * 60_000;

            let id = auth::random_hex();
            state
                .store
                .upsert_event(Event {
                    id,
                    owner_sub: owner,
                    title: qa.title,
                    starts_at,
                    ends_at: starts_at + 3_600_000,
                    all_day: false,
                    location: String::new(),
                    notes: String::new(),
                    rrule: String::new(),
                    created_at: now,
                })
                .await?;
            tracing::info!(target: "audit", event = "event.created", "event quick-added");
            Ok(redirect_to_month(starts_at, off))
        }
        None => {
            // Unparseable: fall back to the normal editor, prefilled with the raw text as the title.
            let csrf = auth::new_csrf_token();
            let view = blank_form_view(&csrf, raw);
            let content = format!("{}{}", render::subnav("calendar"), render_event_form(&view));
            Ok(html_with_csrf_cookie(
                layout("New event", &headers, &content),
                &csrf,
            ))
        }
    }
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
    /// Attendees textarea value (`Name <email>` lines).
    attendees: String,
    /// Which reminder presets (minutes-before) are currently checked.
    reminders: Vec<i64>,
    csrf: String,
    is_edit: bool,
    id: String,
}

/// A blank (non-recurring) create-form view, optionally prefilled with a `title` — used by the
/// quick-add fallback when a phrase could not be parsed.
fn blank_form_view(csrf: &str, title: &str) -> FormView {
    let (repeat, repeat_interval, repeat_count, repeat_until) = FormView::no_recurrence();
    FormView {
        action: "/new".to_string(),
        title: title.to_string(),
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
        id: String::new(),
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
           <p class=\"editor__hint\">Delivered as an in-app notification before the event starts.</p>\
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

/// Persist the submitted attendees + reminders for an event. Attendees reconcile against the stored
/// set (retained RSVP links survive); reminders preserve `delivered_at` for a retained minutes value
/// so a trivial edit does not re-fire an already-delivered reminder.
async fn save_attendees_and_reminders(
    state: &AppState,
    owner: &str,
    event_id: &str,
    form: &EventForm,
    now: i64,
) -> Result<(), AppError> {
    let submitted = parse_attendees(&form.attendees);
    let existing_att = state.store.list_attendees(owner, event_id).await?;
    let attendees = reconcile_attendees(owner, event_id, &submitted, &existing_att, now);
    state.store.replace_attendees(owner, event_id, attendees).await?;

    let minutes = reminder_minutes_from_form(form);
    let existing_rem = state.store.list_reminders(owner, event_id).await?;
    let reminders: Vec<Reminder> = minutes
        .into_iter()
        .map(|m| {
            let prev = existing_rem.iter().find(|r| r.minutes_before == m);
            Reminder {
                id: prev.map(|r| r.id.clone()).unwrap_or_else(auth::random_hex),
                event_id: event_id.to_string(),
                owner_sub: owner.to_string(),
                minutes_before: m,
                delivered_at: prev.map(|r| r.delivered_at).unwrap_or(0),
                created_at: prev.map(|r| r.created_at).unwrap_or(now),
            }
        })
        .collect();
    state.store.replace_reminders(owner, event_id, reminders).await?;
    Ok(())
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
           {attendees}\
           {reminders}\
           {recurrence}\
           <div class=\"editor__actions\">\
             <a class=\"btn btn-secondary\" href=\"/\">Cancel</a>\
             {detail_link}\
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
        attendees = render_attendees_field(v),
        reminders = render_reminders(v),
        recurrence = render_recurrence(v),
        detail_link = if v.is_edit {
            format!(
                "<a class=\"btn btn-secondary\" href=\"/event/{id}\">Event page</a>",
                id = esc(&v.id)
            )
        } else {
            String::new()
        },
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
        let retained = out.iter().find(|a| a.email.eq_ignore_ascii_case("guest@x.co")).unwrap();
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
}
