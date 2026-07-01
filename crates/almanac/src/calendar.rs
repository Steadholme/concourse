//! Calendar math + datetime helpers (all UTC).
//!
//! Almanac stores every timestamp as epoch milliseconds and treats the wall clock as UTC — a
//! deliberately boring v1 with no timezone/DST edge cases (per-user timezones are a noted future
//! enhancement). This module owns three concerns:
//!
//! 1. The month grid ([`build_month`]): weeks of 7 day-cells (Sunday-first), with leading/
//!    trailing padding cells from the adjacent months, each cell carrying its UTC `[start,end]`
//!    millisecond range so the handler can test event overlap.
//! 2. Parsing the HTML form inputs ([`parse_datetime_local`], [`parse_date`]) WITHOUT pulling in
//!    `time`'s parsing feature — we split the fixed `YYYY-MM-DDTHH:MM` / `YYYY-MM-DD` shapes by
//!    hand and build the value through `Date`/`Time` constructors.
//! 3. Formatting an epoch back into a form value ([`fmt_datetime_local`], [`fmt_date_input`]) or
//!    a human label ([`human_date`], [`human_time`], [`fmt_event_when`]).

use time::{Date, Month, OffsetDateTime, Time};

/// One calendar day in the month grid. `Clone` so weeks can be `chunks(7).to_vec()`.
#[derive(Clone, Debug)]
pub struct DayCell {
    /// Day-of-month, 1..=31.
    pub day: u8,
    /// UTC midnight of this day, epoch ms (inclusive lower bound for overlap tests).
    pub start_ms: i64,
    /// Last millisecond of this day, epoch ms (inclusive upper bound).
    pub end_ms: i64,
    /// Whether this cell is the real "today".
    pub is_today: bool,
    /// `YYYY-MM-DD` for the "add on this day" link (`/new?date=…`).
    pub iso_date: String,
}

/// A fully-computed month: header metadata, the week grid, and prev/next navigation targets.
pub struct MonthView {
    pub year: i32,
    pub month: u8,
    pub month_name: &'static str,
    /// Weeks, each a `Vec` of exactly 7 cells; `None` = padding from an adjacent month.
    pub weeks: Vec<Vec<Option<DayCell>>>,
    /// `(year, month)` of the previous month (for the ‹ link).
    pub prev: (i32, u8),
    /// `(year, month)` of the next month (for the › link).
    pub next: (i32, u8),
}

const MONTH_NAMES: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];
const MONTH_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
/// Weekday column headers, Sunday-first (matches [`time::Weekday::number_days_from_sunday`]).
pub const WEEKDAY_HEADERS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// Full month name for a 1..=12 month number (out-of-range clamps to January).
pub fn month_name(month: u8) -> &'static str {
    MONTH_NAMES[month.clamp(1, 12) as usize - 1]
}

fn num_to_month(n: u8) -> Month {
    Month::try_from(n.clamp(1, 12)).unwrap_or(Month::January)
}

/// `(year, month)` of the month containing `ms`.
pub fn month_of(ms: i64) -> (i32, u8) {
    match OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000)) {
        Ok(dt) => (dt.year(), dt.month() as u8),
        Err(_) => (1970, 1),
    }
}

/// UTC midnight (start of day) for the day containing `ms`.
pub fn start_of_day(ms: i64) -> i64 {
    match OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000)) {
        Ok(dt) => dt.date().midnight().assume_utc().unix_timestamp() * 1000,
        Err(_) => ms,
    }
}

/// Last millisecond of the day containing `ms`.
pub fn end_of_day(ms: i64) -> i64 {
    start_of_day(ms) + 86_400_000 - 1
}

/// Build the grid for `year`/`month` (1..=12) in UTC, Sunday-first. `today_ms` marks the live
/// "today" cell. Thin wrapper over [`build_month_at`] for the plain UTC callers/tests.
pub fn build_month(year: i32, month: u8, today_ms: i64) -> MonthView {
    build_month_at(year, month, today_ms, 0, false)
}

/// Build the grid in the owner's timezone. `off_min` shifts UTC into the owner's local wall clock
/// (so day cells span LOCAL midnights and "today" is the owner's local date); `monday_first`
/// selects a Monday-first column order. `off_min == 0 && !monday_first` reproduces [`build_month`].
pub fn build_month_at(
    year: i32,
    month: u8,
    today_ms: i64,
    off_min: i32,
    monday_first: bool,
) -> MonthView {
    let month = month.clamp(1, 12);
    let m = num_to_month(month);
    let days = m.length(year);
    let first = Date::from_calendar_date(year, m, 1).expect("first-of-month is always valid");
    // Number of leading padding cells, relative to the chosen first column.
    let lead = if monday_first {
        first.weekday().number_days_from_monday()
    } else {
        first.weekday().number_days_from_sunday()
    } as usize;

    // "Today" is the owner's LOCAL calendar date.
    let local_today = shift(today_ms, off_min);
    let (ty, tm, td) = match OffsetDateTime::from_unix_timestamp(local_today.div_euclid(1000)) {
        Ok(dt) => (dt.year(), dt.month() as u8, dt.day()),
        Err(_) => (0, 0, 0),
    };

    let mut cells: Vec<Option<DayCell>> = Vec::with_capacity(42);
    for _ in 0..lead {
        cells.push(None);
    }
    for d in 1..=days {
        let date = Date::from_calendar_date(year, m, d).expect("day in range is valid");
        // Local midnight of this day, expressed as the real UTC instant (subtract the offset),
        // so overlap tests against events' UTC `starts_at`/`ends_at` are apples-to-apples.
        let local_midnight = date.midnight().assume_utc().unix_timestamp() * 1000;
        let start_ms = shift(local_midnight, -off_min);
        cells.push(Some(DayCell {
            day: d,
            start_ms,
            end_ms: start_ms + 86_400_000 - 1,
            is_today: ty == year && tm == month && td == d,
            iso_date: format!("{year:04}-{month:02}-{d:02}"),
        }));
    }
    // Pad the trailing partial week so every week has 7 cells.
    while !cells.len().is_multiple_of(7) {
        cells.push(None);
    }
    let weeks = cells.chunks(7).map(<[Option<DayCell>]>::to_vec).collect();

    let prev = if month == 1 { (year - 1, 12) } else { (year, month - 1) };
    let next = if month == 12 { (year + 1, 1) } else { (year, month + 1) };

    MonthView {
        year,
        month,
        month_name: month_name(month),
        weeks,
        prev,
        next,
    }
}

// ---------------------------------------------------------------------------
// Per-user timezone: a fixed UTC offset (minutes) shifts every render, and
// week-start selects the grid's first column. No IANA tz database / DST — a
// deliberately portable, dependency-free model matching the estate's constraints.
// ---------------------------------------------------------------------------

/// Minutes to ADD to a UTC epoch to reach the owner's local wall clock (e.g. `+480` for
/// `UTC+08:00`, `-300` for `UTC-05:00`). Parsed from the stored timezone string; an unknown or
/// blank value maps to `0` (UTC) so rendering never breaks. Clamped to +-14h.
pub fn tz_offset_minutes(tz: &str) -> i32 {
    let t = tz.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("UTC") {
        return 0;
    }
    // Accept "UTC+HH:MM" / "UTC-HH:MM" and the bare "+HH:MM" / "-HH:MM" forms.
    let rest = t.strip_prefix("UTC").or_else(|| t.strip_prefix("utc")).unwrap_or(t);
    let (sign, hm) = match rest.as_bytes().first() {
        Some(b'+') => (1, &rest[1..]),
        Some(b'-') => (-1, &rest[1..]),
        _ => return 0,
    };
    let (h, m) = hm.split_once(':').unwrap_or((hm, "0"));
    let h: i32 = h.trim().parse().unwrap_or(0);
    let m: i32 = m.trim().parse().unwrap_or(0);
    (sign * (h * 60 + m)).clamp(-14 * 60, 14 * 60)
}

/// Shift a UTC epoch-ms by `off_min` minutes — the primitive behind every `_at` renderer.
fn shift(ms: i64, off_min: i32) -> i64 {
    ms + off_min as i64 * 60_000
}

/// Weekday column headers for the grid, respecting the owner's week-start.
pub fn weekday_headers(monday_first: bool) -> [&'static str; 7] {
    if monday_first {
        ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
    } else {
        WEEKDAY_HEADERS
    }
}

/// `(year, month)` of the LOCAL month containing `ms` (for "current month" + post-save redirect).
pub fn month_of_at(ms: i64, off_min: i32) -> (i32, u8) {
    month_of(shift(ms, off_min))
}

/// Real UTC instant of the owner's LOCAL midnight for the day containing `ms`.
pub fn start_of_day_at(ms: i64, off_min: i32) -> i64 {
    shift(start_of_day(shift(ms, off_min)), -off_min)
}

/// Real UTC instant of the last millisecond of the owner's LOCAL day containing `ms`.
pub fn end_of_day_at(ms: i64, off_min: i32) -> i64 {
    start_of_day_at(ms, off_min) + 86_400_000 - 1
}

// ---------------------------------------------------------------------------
// Parsing form inputs (UTC). No `time` parsing feature — split the fixed shapes by hand.
// ---------------------------------------------------------------------------

fn parse_ymd(s: &str) -> Option<(i32, u8, u8)> {
    let mut it = s.split('-');
    let y: i32 = it.next()?.parse().ok()?;
    let mo: u8 = it.next()?.parse().ok()?;
    let d: u8 = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((y, mo, d))
}

fn parse_hms(s: &str) -> Option<(u8, u8, u8)> {
    let mut it = s.split(':');
    let h: u8 = it.next()?.parse().ok()?;
    let mi: u8 = it.next()?.parse().ok()?;
    // Seconds are optional in datetime-local (most browsers omit them).
    let se: u8 = match it.next() {
        Some(part) => part.parse().ok()?,
        None => 0,
    };
    Some((h, mi, se))
}

/// Parse an HTML `datetime-local` value (`YYYY-MM-DDTHH:MM[:SS]`) as UTC -> epoch ms.
/// Returns `None` for empty/invalid input.
pub fn parse_datetime_local(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (date_part, time_part) = match s.split_once('T') {
        Some((d, t)) => (d, t),
        None => (s, "00:00"),
    };
    let (y, mo, d) = parse_ymd(date_part)?;
    let (h, mi, se) = parse_hms(time_part)?;
    let date = Date::from_calendar_date(y, num_to_month(mo), d).ok()?;
    let time = Time::from_hms(h, mi, se).ok()?;
    Some(date.with_time(time).assume_utc().unix_timestamp() * 1000)
}

/// Parse an HTML `date` value (`YYYY-MM-DD`) as UTC midnight -> epoch ms.
pub fn parse_date(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (y, mo, d) = parse_ymd(s)?;
    let date = Date::from_calendar_date(y, num_to_month(mo), d).ok()?;
    Some(date.midnight().assume_utc().unix_timestamp() * 1000)
}

// ---------------------------------------------------------------------------
// Formatting epochs back to form values / human labels (UTC).
// ---------------------------------------------------------------------------

fn to_dt(ms: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000)).ok()
}

/// `YYYY-MM-DDTHH:MM` for pre-filling a `datetime-local` input.
pub fn fmt_datetime_local(ms: i64) -> String {
    match to_dt(ms) {
        Some(dt) => format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}",
            dt.year(),
            dt.month() as u8,
            dt.day(),
            dt.hour(),
            dt.minute()
        ),
        None => String::new(),
    }
}

/// `YYYY-MM-DD` for pre-filling a `date` input.
pub fn fmt_date_input(ms: i64) -> String {
    match to_dt(ms) {
        Some(dt) => format!("{:04}-{:02}-{:02}", dt.year(), dt.month() as u8, dt.day()),
        None => String::new(),
    }
}

/// Human day label, e.g. `Jun 29, 2026`.
pub fn human_date(ms: i64) -> String {
    match to_dt(ms) {
        Some(dt) => format!(
            "{} {}, {}",
            MONTH_ABBR[(dt.month() as u8 - 1) as usize],
            dt.day(),
            dt.year()
        ),
        None => ms.to_string(),
    }
}

/// Human time label, e.g. `14:30` (24-hour, UTC).
pub fn human_time(ms: i64) -> String {
    match to_dt(ms) {
        Some(dt) => format!("{:02}:{:02}", dt.hour(), dt.minute()),
        None => String::new(),
    }
}

/// A friendly "when" label for an event, collapsing same-day ranges.
///
/// - all-day, single day: `All day · Jun 29, 2026`
/// - all-day, multi day:  `All day · Jun 29, 2026 – Jul 1, 2026`
/// - timed, same day:     `Jun 29, 2026 · 14:30 – 15:30`
/// - timed, spanning:     `Jun 29, 2026 14:30 – Jul 1, 2026 09:00`
pub fn fmt_event_when(starts_at: i64, ends_at: i64, all_day: bool) -> String {
    let same_day = start_of_day(starts_at) == start_of_day(ends_at);
    if all_day {
        if same_day {
            format!("All day · {}", human_date(starts_at))
        } else {
            format!("All day · {} – {}", human_date(starts_at), human_date(ends_at))
        }
    } else if same_day {
        format!(
            "{} · {} – {}",
            human_date(starts_at),
            human_time(starts_at),
            human_time(ends_at)
        )
    } else {
        format!(
            "{} {} – {} {}",
            human_date(starts_at),
            human_time(starts_at),
            human_date(ends_at),
            human_time(ends_at)
        )
    }
}

// ---------------------------------------------------------------------------
// Timezone-aware wrappers: format in / parse against the owner's local wall clock.
// Each shifts the epoch by the offset, reuses the UTC primitive, then (for parsing)
// shifts back so what is stored stays a real UTC instant.
// ---------------------------------------------------------------------------

/// `YYYY-MM-DDTHH:MM` in the owner's local wall clock, for pre-filling a `datetime-local` input.
pub fn fmt_datetime_local_at(ms: i64, off_min: i32) -> String {
    fmt_datetime_local(shift(ms, off_min))
}

/// Parse a `datetime-local` value the owner typed in LOCAL time -> the real UTC epoch ms to store.
pub fn parse_datetime_local_at(s: &str, off_min: i32) -> Option<i64> {
    parse_datetime_local(s).map(|ms| shift(ms, -off_min))
}

/// Parse a `date` value (`YYYY-MM-DD`) as the owner's LOCAL midnight -> real UTC epoch ms.
pub fn parse_date_at(s: &str, off_min: i32) -> Option<i64> {
    parse_date(s).map(|ms| shift(ms, -off_min))
}

/// Human time label in the owner's local wall clock, e.g. `14:30`.
pub fn human_time_at(ms: i64, off_min: i32) -> String {
    human_time(shift(ms, off_min))
}

/// A friendly "when" label rendered in the owner's local wall clock (see [`fmt_event_when`]).
pub fn fmt_event_when_at(starts_at: i64, ends_at: i64, all_day: bool, off_min: i32) -> String {
    fmt_event_when(shift(starts_at, off_min), shift(ends_at, off_min), all_day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_grid_pads_to_full_weeks() {
        // June 2026: June 1 is a Monday, 30 days.
        let view = build_month(2026, 6, parse_date("2026-06-15").unwrap());
        assert_eq!(view.month_name, "June");
        assert_eq!(view.prev, (2026, 5));
        assert_eq!(view.next, (2026, 7));
        // Every week is exactly 7 cells.
        for week in &view.weeks {
            assert_eq!(week.len(), 7);
        }
        // Monday-first day => one leading pad cell (Sunday).
        let first_week = &view.weeks[0];
        assert!(first_week[0].is_none(), "Sunday cell is padding");
        assert_eq!(first_week[1].as_ref().unwrap().day, 1, "Monday is the 1st");
        // The 15th cell is flagged today.
        let today = view
            .weeks
            .iter()
            .flatten()
            .flatten()
            .find(|c| c.day == 15)
            .unwrap();
        assert!(today.is_today);
    }

    #[test]
    fn year_rolls_over_in_navigation() {
        let dec = build_month(2026, 12, 0);
        assert_eq!(dec.next, (2027, 1));
        let jan = build_month(2026, 1, 0);
        assert_eq!(jan.prev, (2025, 12));
    }

    #[test]
    fn datetime_local_round_trips_through_epoch() {
        let ms = parse_datetime_local("2026-06-29T14:30").unwrap();
        assert_eq!(fmt_datetime_local(ms), "2026-06-29T14:30");
        assert_eq!(human_date(ms), "Jun 29, 2026");
        assert_eq!(human_time(ms), "14:30");
    }

    #[test]
    fn date_input_parses_to_utc_midnight() {
        let ms = parse_date("2026-06-29").unwrap();
        assert_eq!(start_of_day(ms), ms);
        assert_eq!(fmt_date_input(ms), "2026-06-29");
        // 1719619200000 = 2024-06-29 is not this one; just assert midnight invariant.
        assert_eq!(ms % 86_400_000, 0);
    }

    #[test]
    fn rejects_garbage_input() {
        assert!(parse_datetime_local("").is_none());
        assert!(parse_datetime_local("not-a-date").is_none());
        assert!(parse_date("2026-13-40").is_none());
        assert!(parse_datetime_local("2026-06-29T25:00").is_none());
    }

    #[test]
    fn event_when_labels() {
        let s = parse_datetime_local("2026-06-29T14:30").unwrap();
        let e = parse_datetime_local("2026-06-29T15:30").unwrap();
        assert_eq!(fmt_event_when(s, e, false), "Jun 29, 2026 · 14:30 – 15:30");
        let day = parse_date("2026-06-29").unwrap();
        assert_eq!(fmt_event_when(day, end_of_day(day), true), "All day · Jun 29, 2026");
    }

    #[test]
    fn tz_offset_parses_common_forms() {
        assert_eq!(tz_offset_minutes("UTC"), 0);
        assert_eq!(tz_offset_minutes(""), 0);
        assert_eq!(tz_offset_minutes("UTC+08:00"), 480);
        assert_eq!(tz_offset_minutes("UTC-05:00"), -300);
        assert_eq!(tz_offset_minutes("UTC+05:30"), 330);
        assert_eq!(tz_offset_minutes("-05:00"), -300);
        assert_eq!(tz_offset_minutes("garbage"), 0, "unknown => UTC");
        assert_eq!(tz_offset_minutes("UTC+99:00"), 14 * 60, "clamped to +14h");
    }

    #[test]
    fn datetime_local_round_trips_in_a_timezone() {
        // The owner in UTC+8 types 10:00 local; we store 02:00 UTC and render it back as 10:00.
        let off = tz_offset_minutes("UTC+08:00");
        let utc = parse_datetime_local_at("2026-06-29T10:00", off).unwrap();
        // Stored value is the real UTC instant (02:00 UTC).
        assert_eq!(fmt_datetime_local(utc), "2026-06-29T02:00");
        assert_eq!(human_time(utc), "02:00");
        // Rendered back through the offset, the owner sees their local 10:00.
        assert_eq!(fmt_datetime_local_at(utc, off), "2026-06-29T10:00");
        assert_eq!(human_time_at(utc, off), "10:00");
    }

    #[test]
    fn grid_today_and_cells_shift_with_offset() {
        // 2026-01-01 00:30 UTC is still 2025-12-31 (19:30) in UTC-05:00.
        let now = parse_datetime_local("2026-01-01T00:30").unwrap();
        let off = tz_offset_minutes("UTC-05:00");
        let view = build_month_at(2025, 12, now, off, false);
        let dec31 = view
            .weeks
            .iter()
            .flatten()
            .flatten()
            .find(|c| c.day == 31)
            .unwrap();
        assert!(dec31.is_today, "local date is Dec 31, not Jan 1");
        // The cell's start is the real UTC instant of local midnight (05:00 UTC).
        assert_eq!(human_time(dec31.start_ms), "05:00");
    }

    #[test]
    fn grid_monday_first_reorders_columns() {
        assert_eq!(weekday_headers(false)[0], "Sun");
        assert_eq!(weekday_headers(true)[0], "Mon");
        // June 2026: the 1st is a Monday => Monday-first has zero leading pads.
        let view = build_month_at(2026, 6, 0, 0, true);
        assert_eq!(view.weeks[0][0].as_ref().unwrap().day, 1, "Monday column holds the 1st");
    }
}
