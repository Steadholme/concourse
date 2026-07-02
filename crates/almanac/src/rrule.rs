//! A focused RFC 5545 RRULE parser + occurrence expander (no external `icalendar` crate).
//!
//! Almanac stores an event's recurrence as an opaque RRULE TEXT column on the row (empty = a
//! one-off). The FIRST occurrence is the event's own `starts_at`/`ends_at` (DTSTART); the RRULE
//! describes how it repeats. Expansion is VIRTUAL: [`expand_events`] turns a stored event into a
//! list of concrete [`Event`] clones (same `id`, shifted times) whose `[start, end]` range overlaps
//! the rendered window, so the month grid + agenda render occurrences with NO changes to their
//! per-cell / per-item filters. Because occurrences are virtual and share the series `id`,
//! edit/delete naturally apply to the whole series (v1 semantics).
//!
//! Supported: `FREQ=DAILY|WEEKLY|MONTHLY|YEARLY` with `INTERVAL`, `COUNT`, `UNTIL`, and `BYDAY`.
//! - `BYDAY` on `WEEKLY` selects which weekdays repeat (default: DTSTART's weekday); any ordinal
//!   prefix is ignored (ordinals are not meaningful for WEEKLY).
//! - `BYDAY` on `DAILY` acts as a weekday filter (ordinal prefix ignored).
//! - `BYDAY` on `MONTHLY`/`YEARLY` selects weekdays with an optional ordinal: `3WE` = the 3rd
//!   Wednesday, `-1FR` = the last Friday, plain `WE` = every Wednesday of the period. For YEARLY
//!   the selection is restricted to DTSTART's month (so `-1FR` yearly = the last Friday of that
//!   month, each year). An ordinal that does not exist in a period (e.g. a 5th Wednesday in a
//!   month with only four) is skipped and does not count toward COUNT.
//! - Without `BYDAY`, `MONTHLY`/`YEARLY` recur on DTSTART's day-of-month (a day that does not exist
//!   in a given month/year — e.g. the 31st, or Feb 29 — is skipped, per RFC, and does not count).
//! All timestamps are UTC epoch milliseconds, matching the rest of the crate (no DST/tz math here).

use std::collections::HashSet;

use time::{Date, Month, OffsetDateTime, Weekday};

use crate::store::Event;

const DAY_MS: i64 = 86_400_000;
/// Hard safety cap on generated candidates, so a pathological rule (huge COUNT, far-past DTSTART)
/// can never loop unbounded while rendering a single request.
const MAX_ITERS: u32 = 20_000;

/// Recurrence frequency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl Freq {
    /// The lowercase keyword used by the compose UI's `<select>` (round-trips through the form).
    pub fn keyword(self) -> &'static str {
        match self {
            Freq::Daily => "daily",
            Freq::Weekly => "weekly",
            Freq::Monthly => "monthly",
            Freq::Yearly => "yearly",
        }
    }

    fn from_rrule(s: &str) -> Option<Freq> {
        match s.trim().to_ascii_uppercase().as_str() {
            "DAILY" => Some(Freq::Daily),
            "WEEKLY" => Some(Freq::Weekly),
            "MONTHLY" => Some(Freq::Monthly),
            "YEARLY" => Some(Freq::Yearly),
            _ => None,
        }
    }
}

/// One `BYDAY` entry: a weekday with an optional ordinal. `ord` is `None` for a bare weekday code
/// (e.g. `WE`), `Some(3)` for `3WE` (the 3rd Wednesday), `Some(-1)` for `-1FR` (the last Friday).
/// The ordinal only takes effect for MONTHLY/YEARLY expansion; DAILY/WEEKLY use just the weekday.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByDay {
    pub ord: Option<i32>,
    pub weekday: Weekday,
}

/// A parsed recurrence rule. `interval >= 1`. `count` includes the first occurrence. `until` is an
/// inclusive UTC epoch-ms bound. `byday` is the ordered set of `BYDAY` entries (weekday + optional
/// ordinal); the ordinal is honoured only for MONTHLY/YEARLY.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RRule {
    pub freq: Freq,
    pub interval: u32,
    pub count: Option<u32>,
    pub until: Option<i64>,
    pub byday: Vec<ByDay>,
}

impl RRule {
    /// Parse an RRULE value (e.g. `FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;COUNT=10`). Returns `None`
    /// for an empty string or any value without a valid `FREQ` — such an event is treated as a
    /// plain one-off, so a garbage column never breaks rendering.
    pub fn parse(s: &str) -> Option<RRule> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        let mut freq: Option<Freq> = None;
        let mut interval: u32 = 1;
        let mut count: Option<u32> = None;
        let mut until: Option<i64> = None;
        let mut byday: Vec<ByDay> = Vec::new();

        for part in s.split(';') {
            let (key, val) = match part.split_once('=') {
                Some(kv) => kv,
                None => continue,
            };
            match key.trim().to_ascii_uppercase().as_str() {
                "FREQ" => freq = Freq::from_rrule(val),
                "INTERVAL" => {
                    if let Ok(n) = val.trim().parse::<u32>() {
                        interval = n.max(1);
                    }
                }
                "COUNT" => count = val.trim().parse::<u32>().ok(),
                "UNTIL" => until = parse_until(val.trim()),
                "BYDAY" => {
                    byday = val
                        .split(',')
                        .filter_map(|d| parse_byday(d.trim()))
                        .collect();
                }
                _ => {}
            }
        }

        freq.map(|freq| RRule {
            freq,
            interval,
            count,
            until,
            byday,
        })
    }

    /// The recurrence's UNTIL as a `YYYY-MM-DD` string for pre-filling the compose `date` input
    /// (empty when the rule has no UNTIL).
    pub fn until_date_input(&self) -> String {
        match self.until {
            Some(ms) => crate::calendar::fmt_date_input(ms),
            None => String::new(),
        }
    }

    /// Expand the series into concrete `[start, end]` occurrence ranges (UTC epoch ms) that OVERLAP
    /// `[win_start, win_end]`. `dtstart`/`duration` come from the event. COUNT/UNTIL are honoured
    /// against the full series (from DTSTART), not just the window.
    pub fn expand(
        &self,
        dtstart: i64,
        duration: i64,
        win_start: i64,
        win_end: i64,
    ) -> Vec<(i64, i64)> {
        let mut c = Collector {
            count: self.count,
            until: self.until,
            win_start,
            win_end,
            duration,
            emitted: 0,
            iters: 0,
            out: Vec::new(),
            stop: false,
        };
        match self.freq {
            Freq::Daily => self.expand_daily(dtstart, &mut c),
            Freq::Weekly => self.expand_weekly(dtstart, &mut c),
            Freq::Monthly => self.expand_monthly(dtstart, &mut c),
            Freq::Yearly => self.expand_yearly(dtstart, &mut c),
        }
        c.out
    }

    fn expand_daily(&self, dtstart: i64, c: &mut Collector) {
        let step = self.interval as i64 * DAY_MS;
        let mut k: i64 = 0;
        loop {
            if c.stop || c.bump_iter() {
                break;
            }
            let start = dtstart + k * step;
            if c.past_end(start) {
                break;
            }
            // BYDAY on DAILY is a weekday filter; filtered-out days do not count toward COUNT.
            if self.byday.is_empty() || self.byday.iter().any(|bd| bd.weekday == weekday_of(start))
            {
                if !c.push(start) {
                    break;
                }
            } else if c.until_exceeded(start) {
                break;
            }
            k += 1;
        }
    }

    fn expand_weekly(&self, dtstart: i64, c: &mut Collector) {
        // Default to DTSTART's own weekday when no BYDAY is given. Ordinals are ignored for WEEKLY.
        let mut days: Vec<Weekday> = self.byday.iter().map(|bd| bd.weekday).collect();
        if days.is_empty() {
            days.push(weekday_of(dtstart));
        }
        days.sort_by_key(|w| w.number_days_from_monday());
        days.dedup();

        let tod = dtstart - start_of_day(dtstart);
        // Monday (00:00 UTC) of DTSTART's week; every `interval` weeks advances from here.
        let week0_monday =
            start_of_day(dtstart) - weekday_of(dtstart).number_days_from_monday() as i64 * DAY_MS;
        let week_step = self.interval as i64 * 7 * DAY_MS;

        let mut w: i64 = 0;
        loop {
            if c.stop {
                break;
            }
            let week_monday = week0_monday + w * week_step;
            // If even the last selected day of this week is past the window, we are done.
            if c.past_end(week_monday) && week_monday > c.win_end {
                break;
            }
            let mut advanced = false;
            for wd in &days {
                if c.bump_iter() {
                    return;
                }
                let start = week_monday + wd.number_days_from_monday() as i64 * DAY_MS + tod;
                // The first (partial) week skips any selected day that falls before DTSTART.
                if start < dtstart {
                    continue;
                }
                advanced = true;
                if c.past_end(start) {
                    c.stop = true;
                    break;
                }
                if !c.push(start) {
                    break;
                }
            }
            if c.stop {
                break;
            }
            // Safety: if a week produced nothing and we are already past the window end, stop.
            if !advanced && week_monday > c.win_end {
                break;
            }
            w += 1;
        }
    }

    fn expand_monthly(&self, dtstart: i64, c: &mut Collector) {
        let (y0, mo0, d0) = ymd_of(dtstart);
        let tod = dtstart - start_of_day(dtstart);
        let base = y0 as i64 * 12 + (mo0 as i64 - 1);
        let mut k: i64 = 0;
        loop {
            if c.stop || c.bump_iter() {
                break;
            }
            let total = base + k * self.interval as i64;
            let y = total.div_euclid(12) as i32;
            let mo = (total.rem_euclid(12) + 1) as u8;
            // Chronological stop: once the first of this month is past the window, no more overlap.
            if let Some(first) = build_ms(y, mo, 1, 0) {
                if first > c.win_end {
                    break;
                }
            }
            if self.byday.is_empty() {
                if let Some(start) = build_ms(y, mo, d0, tod) {
                    if !c.push(start) {
                        break;
                    }
                }
                // else: DTSTART's day-of-month does not exist this month — skipped, does not count.
            } else {
                // BYDAY: emit each selected weekday-of-month, in chronological order within the month.
                for day in byday_days(y, mo, &self.byday) {
                    if let Some(start) = build_ms(y, mo, day, tod) {
                        // The first (partial) period skips any match that falls before DTSTART.
                        if start < dtstart {
                            continue;
                        }
                        if !c.push(start) {
                            break;
                        }
                    }
                }
            }
            if c.stop {
                break;
            }
            k += 1;
        }
    }

    fn expand_yearly(&self, dtstart: i64, c: &mut Collector) {
        let (y0, mo0, d0) = ymd_of(dtstart);
        let tod = dtstart - start_of_day(dtstart);
        let mut k: i64 = 0;
        loop {
            if c.stop || c.bump_iter() {
                break;
            }
            let y = y0 + k as i32 * self.interval as i32;
            if let Some(first) = build_ms(y, mo0, 1, 0) {
                if first > c.win_end {
                    break;
                }
            }
            if self.byday.is_empty() {
                if let Some(start) = build_ms(y, mo0, d0, tod) {
                    if !c.push(start) {
                        break;
                    }
                }
                // else: Feb 29 in a non-leap year — skipped, does not count.
            } else {
                // BYDAY on YEARLY: restrict the selection to DTSTART's month, each year.
                for day in byday_days(y, mo0, &self.byday) {
                    if let Some(start) = build_ms(y, mo0, day, tod) {
                        if start < dtstart {
                            continue;
                        }
                        if !c.push(start) {
                            break;
                        }
                    }
                }
            }
            if c.stop {
                break;
            }
            k += 1;
        }
    }
}

/// Shared occurrence sink: applies COUNT/UNTIL/window and collects overlapping ranges.
struct Collector {
    count: Option<u32>,
    until: Option<i64>,
    win_start: i64,
    win_end: i64,
    duration: i64,
    emitted: u32,
    iters: u32,
    out: Vec<(i64, i64)>,
    stop: bool,
}

impl Collector {
    /// Register one real occurrence. Returns `false` (and sets `stop`) when the series is finished.
    fn push(&mut self, start: i64) -> bool {
        if self.until_exceeded(start) {
            return false;
        }
        if let Some(c) = self.count {
            if self.emitted >= c {
                self.stop = true;
                return false;
            }
        }
        self.emitted += 1;
        let end = start + self.duration;
        if start <= self.win_end && end >= self.win_start {
            self.out.push((start, end));
        }
        if start > self.win_end {
            self.stop = true;
            return false;
        }
        true
    }

    fn until_exceeded(&mut self, start: i64) -> bool {
        if let Some(u) = self.until {
            if start > u {
                self.stop = true;
                return true;
            }
        }
        false
    }

    /// A candidate whose start is already past the window end (chronological terminator).
    fn past_end(&self, start: i64) -> bool {
        start > self.win_end
    }

    /// Increment the iteration guard; returns `true` when the hard cap is hit (caller must stop).
    fn bump_iter(&mut self) -> bool {
        self.iters += 1;
        if self.iters > MAX_ITERS {
            self.stop = true;
            true
        } else {
            false
        }
    }
}

/// Expand a slice of stored events into the concrete occurrences visible in `[win_start, win_end]`,
/// sorted by `starts_at`. Non-recurring events (empty/invalid RRULE) pass through unchanged; each
/// occurrence of a recurring event is a clone of the series row with only its times shifted (its
/// `id` is preserved, so edit/delete links target the whole series).
pub fn expand_events(events: &[Event], win_start: i64, win_end: i64) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::new();
    let overrides: HashSet<(String, i64)> = events
        .iter()
        .filter(|e| !e.series_id.is_empty() && e.override_occurrence_date > 0)
        .map(|e| (e.series_id.clone(), e.override_occurrence_date))
        .collect();
    for e in events {
        if !e.series_id.is_empty() {
            out.push(e.clone());
            continue;
        }
        match RRule::parse(&e.rrule) {
            Some(rule) => {
                let duration = (e.ends_at - e.starts_at).max(0);
                let exceptions: HashSet<i64> = e.exception_dates.iter().copied().collect();
                for (start, end) in rule.expand(e.starts_at, duration, win_start, win_end) {
                    if exceptions.contains(&start) || overrides.contains(&(e.id.clone(), start)) {
                        continue;
                    }
                    let mut occ = e.clone();
                    occ.starts_at = start;
                    occ.ends_at = end;
                    out.push(occ);
                }
            }
            None => out.push(e.clone()),
        }
    }
    out.sort_by(|a, b| a.starts_at.cmp(&b.starts_at).then_with(|| a.id.cmp(&b.id)));
    out
}

// --------------------------------------------------------------------------------------
// Small UTC date helpers (kept local so the module is self-contained; mirror calendar.rs).
// --------------------------------------------------------------------------------------

fn num_to_month(n: u8) -> Month {
    Month::try_from(n.clamp(1, 12)).unwrap_or(Month::January)
}

fn to_dt(ms: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000)).ok()
}

fn weekday_of(ms: i64) -> Weekday {
    to_dt(ms).map(|dt| dt.weekday()).unwrap_or(Weekday::Monday)
}

fn start_of_day(ms: i64) -> i64 {
    match to_dt(ms) {
        Some(dt) => dt.date().midnight().assume_utc().unix_timestamp() * 1000,
        None => ms,
    }
}

fn ymd_of(ms: i64) -> (i32, u8, u8) {
    match to_dt(ms) {
        Some(dt) => (dt.year(), dt.month() as u8, dt.day()),
        None => (1970, 1, 1),
    }
}

/// Build an epoch-ms from calendar parts + a within-day offset. `None` when the day is invalid for
/// the month/year (e.g. Feb 30, or Feb 29 in a common year), which the expander treats as skipped.
fn build_ms(year: i32, month: u8, day: u8, tod_ms: i64) -> Option<i64> {
    let date = Date::from_calendar_date(year, num_to_month(month), day).ok()?;
    Some(date.midnight().assume_utc().unix_timestamp() * 1000 + tod_ms)
}

/// Parse an RRULE `UNTIL` value: a date `YYYYMMDD` (treated as inclusive end-of-day UTC) or a
/// datetime `YYYYMMDDTHHMMSS[Z]`. Returns the inclusive UTC epoch-ms bound.
fn parse_until(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date_part, time_part) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    if date_part.len() != 8 || !date_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: i32 = date_part[0..4].parse().ok()?;
    let mo: u8 = date_part[4..6].parse().ok()?;
    let d: u8 = date_part[6..8].parse().ok()?;
    let day_ms = build_ms(y, mo, d, 0)?;
    match time_part {
        None => Some(day_ms + DAY_MS - 1), // date-only UNTIL includes the whole day.
        Some(t) => {
            let t = t.trim_end_matches('Z');
            if t.len() < 6 || !t.bytes().take(6).all(|b| b.is_ascii_digit()) {
                return Some(day_ms + DAY_MS - 1);
            }
            let h: i64 = t[0..2].parse().ok()?;
            let mi: i64 = t[2..4].parse().ok()?;
            let se: i64 = t[4..6].parse().ok()?;
            Some(day_ms + (h * 3600 + mi * 60 + se) * 1000)
        }
    }
}

/// Parse a `BYDAY` entry: an optional signed ordinal followed by a two-letter weekday code
/// (`MO`,`TU`,`WE`,`TH`,`FR`,`SA`,`SU`) — e.g. `WE`, `3WE`, `+3WE`, `-1FR`. Returns `None` for an
/// unknown code. A `0` ordinal (invalid per RFC) is treated as no ordinal.
fn parse_byday(s: &str) -> Option<ByDay> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (ord_str, code) = s.split_at(split);
    let weekday = match code.to_ascii_uppercase().as_str() {
        "MO" => Weekday::Monday,
        "TU" => Weekday::Tuesday,
        "WE" => Weekday::Wednesday,
        "TH" => Weekday::Thursday,
        "FR" => Weekday::Friday,
        "SA" => Weekday::Saturday,
        "SU" => Weekday::Sunday,
        _ => return None,
    };
    // A bare `+` / `-` or unparsable prefix falls back to no ordinal (bare weekday).
    let ord = ord_str
        .trim_start_matches('+')
        .parse::<i32>()
        .ok()
        .filter(|n| *n != 0);
    Some(ByDay { ord, weekday })
}

/// The days-of-month (ascending, deduped) selected by `byday` in the given month. For an entry
/// with an ordinal, resolves the nth matching weekday (negative counts from the month's end); an
/// ordinal beyond the month's matching weekdays yields nothing. For a bare weekday, every matching
/// day is returned.
fn byday_days(year: i32, month: u8, byday: &[ByDay]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for bd in byday {
        // All days in the month whose weekday matches, in ascending order.
        let matches: Vec<u8> = (1u8..=31)
            .filter(|&d| match build_ms(year, month, d, 0) {
                Some(ms) => weekday_of(ms) == bd.weekday,
                None => false,
            })
            .collect();
        match bd.ord {
            None => out.extend(matches),
            Some(n) if n > 0 => {
                if let Some(&d) = matches.get((n - 1) as usize) {
                    out.push(d);
                }
            }
            Some(n) => {
                // n < 0: count from the end (-1 = last).
                let idx = matches.len() as i32 + n;
                if idx >= 0 {
                    out.push(matches[idx as usize]);
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Build an RRULE string from the compose UI's simple fields (empty when `freq` is `None`). Only
/// non-default parts are emitted, so a plain weekly rule is just `FREQ=WEEKLY`.
pub fn build_rrule(
    freq: Option<Freq>,
    interval: u32,
    count: Option<u32>,
    until_yyyymmdd: Option<&str>,
) -> String {
    let freq = match freq {
        Some(f) => f,
        None => return String::new(),
    };
    let mut out = format!("FREQ={}", freq.keyword().to_ascii_uppercase());
    if interval > 1 {
        out.push_str(&format!(";INTERVAL={interval}"));
    }
    if let Some(c) = count {
        if c > 0 {
            out.push_str(&format!(";COUNT={c}"));
        }
    }
    if let Some(u) = until_yyyymmdd {
        let digits: String = u.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() == 8 {
            out.push_str(&format!(";UNTIL={digits}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::parse_datetime_local;

    fn ms(s: &str) -> i64 {
        parse_datetime_local(s).unwrap()
    }

    /// Expand a rule and return just the occurrence START times as `YYYY-MM-DDTHH:MM`.
    fn starts(rrule: &str, dtstart: &str, win_a: &str, win_b: &str) -> Vec<String> {
        let rule = RRule::parse(rrule).expect("valid rrule");
        rule.expand(ms(dtstart), 3_600_000, ms(win_a), ms(win_b))
            .into_iter()
            .map(|(s, _)| crate::calendar::fmt_datetime_local(s))
            .collect()
    }

    #[test]
    fn parse_extracts_all_fields() {
        let r = RRule::parse("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;COUNT=10").unwrap();
        assert_eq!(r.freq, Freq::Weekly);
        assert_eq!(r.interval, 2);
        assert_eq!(r.count, Some(10));
        assert_eq!(
            r.byday,
            vec![
                ByDay {
                    ord: None,
                    weekday: Weekday::Monday
                },
                ByDay {
                    ord: None,
                    weekday: Weekday::Wednesday
                },
            ]
        );
        assert!(r.until.is_none());
        // Empty / no-FREQ => not a recurrence.
        assert!(RRule::parse("").is_none());
        assert!(RRule::parse("INTERVAL=2").is_none());
        // interval floors at 1.
        assert_eq!(RRule::parse("FREQ=DAILY;INTERVAL=0").unwrap().interval, 1);
    }

    #[test]
    fn daily_every_day_bounded_by_count() {
        // 5 daily occurrences starting 2026-06-10, all inside a wide window.
        let got = starts(
            "FREQ=DAILY;COUNT=5",
            "2026-06-10T09:00",
            "2026-06-01T00:00",
            "2026-06-30T23:59",
        );
        assert_eq!(
            got,
            vec![
                "2026-06-10T09:00",
                "2026-06-11T09:00",
                "2026-06-12T09:00",
                "2026-06-13T09:00",
                "2026-06-14T09:00",
            ]
        );
    }

    #[test]
    fn daily_with_interval_and_window_clip() {
        // Every 3 days; the window only reveals the ones that fall inside it.
        let got = starts(
            "FREQ=DAILY;INTERVAL=3",
            "2026-06-01T08:00",
            "2026-06-05T00:00",
            "2026-06-12T23:59",
        );
        // Series: Jun 1,4,7,10,13,... window [5..12] keeps 7 and 10.
        assert_eq!(got, vec!["2026-06-07T08:00", "2026-06-10T08:00"]);
    }

    #[test]
    fn daily_byday_filters_to_weekdays() {
        // DAILY but only on Mon/Fri: a weekday filter, bounded by UNTIL.
        let got = starts(
            "FREQ=DAILY;BYDAY=MO,FR;UNTIL=20260619",
            "2026-06-08T07:30", // Mon 2026-06-08
            "2026-06-01T00:00",
            "2026-06-30T23:59",
        );
        // Mon 8, Fri 12, Mon 15, Fri 19.
        assert_eq!(
            got,
            vec![
                "2026-06-08T07:30",
                "2026-06-12T07:30",
                "2026-06-15T07:30",
                "2026-06-19T07:30",
            ]
        );
    }

    #[test]
    fn weekly_default_weekday_is_dtstart() {
        // No BYDAY => repeats on DTSTART's weekday (Wednesday) each week.
        let got = starts(
            "FREQ=WEEKLY;COUNT=3",
            "2026-06-03T12:00", // Wed
            "2026-06-01T00:00",
            "2026-07-31T23:59",
        );
        assert_eq!(
            got,
            vec!["2026-06-03T12:00", "2026-06-10T12:00", "2026-06-17T12:00"]
        );
    }

    #[test]
    fn weekly_byday_multiple_days_and_interval() {
        // Every 2 weeks on Mon+Wed, starting Wed 2026-06-03.
        let got = starts(
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE",
            "2026-06-03T09:00", // Wed
            "2026-06-01T00:00",
            "2026-06-30T23:59",
        );
        // Week0 (Jun 1 Mon..): Mon Jun 1 is BEFORE dtstart -> skipped; Wed Jun 3 kept.
        // Skip a week (interval 2) -> week starting Jun 15: Mon Jun 15, Wed Jun 17.
        // Next -> week of Jun 29: Mon Jun 29 (Wed Jul 1 outside window).
        assert_eq!(
            got,
            vec![
                "2026-06-03T09:00",
                "2026-06-15T09:00",
                "2026-06-17T09:00",
                "2026-06-29T09:00",
            ]
        );
    }

    #[test]
    fn monthly_same_day_each_month() {
        let got = starts(
            "FREQ=MONTHLY;COUNT=4",
            "2026-01-15T10:00",
            "2026-01-01T00:00",
            "2026-12-31T23:59",
        );
        assert_eq!(
            got,
            vec![
                "2026-01-15T10:00",
                "2026-02-15T10:00",
                "2026-03-15T10:00",
                "2026-04-15T10:00",
            ]
        );
    }

    #[test]
    fn monthly_skips_months_without_the_day() {
        // The 31st: Feb/Apr/Jun... have no 31st and are skipped (do not count toward COUNT).
        let got = starts(
            "FREQ=MONTHLY;COUNT=4",
            "2026-01-31T08:00",
            "2026-01-01T00:00",
            "2026-12-31T23:59",
        );
        // Jan 31, Mar 31, May 31, Jul 31 (Feb, Apr, Jun skipped).
        assert_eq!(
            got,
            vec![
                "2026-01-31T08:00",
                "2026-03-31T08:00",
                "2026-05-31T08:00",
                "2026-07-31T08:00",
            ]
        );
    }

    #[test]
    fn yearly_same_month_day() {
        let got = starts(
            "FREQ=YEARLY;COUNT=3",
            "2026-07-04T18:00",
            "2026-01-01T00:00",
            "2030-12-31T23:59",
        );
        assert_eq!(
            got,
            vec!["2026-07-04T18:00", "2027-07-04T18:00", "2028-07-04T18:00"]
        );
    }

    #[test]
    fn yearly_leap_day_skips_common_years() {
        // Feb 29 recurs only on leap years (2028, 2032, ...); common years are skipped.
        let got = starts(
            "FREQ=YEARLY;COUNT=2",
            "2028-02-29T00:00",
            "2028-01-01T00:00",
            "2040-12-31T23:59",
        );
        assert_eq!(got, vec!["2028-02-29T00:00", "2032-02-29T00:00"]);
    }

    #[test]
    fn parse_byday_ordinals() {
        let r = RRule::parse("FREQ=MONTHLY;BYDAY=3WE,-1FR,SU").unwrap();
        assert_eq!(
            r.byday,
            vec![
                ByDay {
                    ord: Some(3),
                    weekday: Weekday::Wednesday
                },
                ByDay {
                    ord: Some(-1),
                    weekday: Weekday::Friday
                },
                ByDay {
                    ord: None,
                    weekday: Weekday::Sunday
                },
            ]
        );
        // Explicit '+' sign and a stray zero ordinal (invalid) degrade to a bare weekday.
        let r2 = RRule::parse("FREQ=MONTHLY;BYDAY=+2MO,0TU").unwrap();
        assert_eq!(
            r2.byday,
            vec![
                ByDay {
                    ord: Some(2),
                    weekday: Weekday::Monday
                },
                ByDay {
                    ord: None,
                    weekday: Weekday::Tuesday
                },
            ]
        );
    }

    #[test]
    fn monthly_nth_weekday() {
        // 3rd Wednesday each month, starting on the 3rd Wed of Jan 2026 (Jan 21).
        let got = starts(
            "FREQ=MONTHLY;BYDAY=3WE;COUNT=3",
            "2026-01-21T10:00",
            "2026-01-01T00:00",
            "2026-12-31T23:59",
        );
        assert_eq!(
            got,
            vec!["2026-01-21T10:00", "2026-02-18T10:00", "2026-03-18T10:00"]
        );
    }

    #[test]
    fn monthly_last_weekday() {
        // Last Friday each month, starting on the last Fri of Jan 2026 (Jan 30).
        let got = starts(
            "FREQ=MONTHLY;BYDAY=-1FR;COUNT=4",
            "2026-01-30T17:00",
            "2026-01-01T00:00",
            "2026-12-31T23:59",
        );
        assert_eq!(
            got,
            vec![
                "2026-01-30T17:00",
                "2026-02-27T17:00",
                "2026-03-27T17:00",
                "2026-04-24T17:00",
            ]
        );
    }

    #[test]
    fn monthly_bare_weekday_is_every_matching_day() {
        // BYDAY=WE (no ordinal) => every Wednesday of the month.
        let got = starts(
            "FREQ=MONTHLY;BYDAY=WE",
            "2026-01-07T09:00", // first Wed of Jan 2026
            "2026-01-01T00:00",
            "2026-01-31T23:59",
        );
        assert_eq!(
            got,
            vec![
                "2026-01-07T09:00",
                "2026-01-14T09:00",
                "2026-01-21T09:00",
                "2026-01-28T09:00",
            ]
        );
    }

    #[test]
    fn monthly_missing_ordinal_is_skipped_and_not_counted() {
        // 5th Wednesday: months without one (May, Jun) are skipped and do NOT count toward COUNT.
        let got = starts(
            "FREQ=MONTHLY;BYDAY=5WE;COUNT=2",
            "2026-04-29T08:00", // 5th Wed of Apr 2026
            "2026-01-01T00:00",
            "2026-12-31T23:59",
        );
        // Apr has a 5th Wed (29); May & Jun do not; Jul does (29).
        assert_eq!(got, vec!["2026-04-29T08:00", "2026-07-29T08:00"]);
    }

    #[test]
    fn yearly_last_weekday_of_dtstart_month() {
        // YEARLY BYDAY restricts to DTSTART's month: last Friday of January, each year.
        let got = starts(
            "FREQ=YEARLY;BYDAY=-1FR;COUNT=3",
            "2026-01-30T17:00",
            "2026-01-01T00:00",
            "2030-12-31T23:59",
        );
        assert_eq!(
            got,
            vec!["2026-01-30T17:00", "2027-01-29T17:00", "2028-01-28T17:00"]
        );
    }

    #[test]
    fn until_bounds_the_series() {
        let got = starts(
            "FREQ=DAILY;UNTIL=20260612",
            "2026-06-10T09:00",
            "2026-06-01T00:00",
            "2026-06-30T23:59",
        );
        // Inclusive of the UNTIL day (date-only => whole day).
        assert_eq!(
            got,
            vec!["2026-06-10T09:00", "2026-06-11T09:00", "2026-06-12T09:00"]
        );
    }

    #[test]
    fn expand_events_preserves_id_and_shifts_times() {
        let base = Event {
            id: "series1".to_string(),
            owner_sub: "alice".to_string(),
            calendar_id: String::new(),
            title: "Standup".to_string(),
            starts_at: ms("2026-06-01T09:00"),
            ends_at: ms("2026-06-01T09:15"),
            all_day: false,
            location: String::new(),
            notes: String::new(),
            rrule: "FREQ=DAILY;COUNT=3".to_string(),
            series_id: String::new(),
            override_occurrence_date: 0,
            exception_dates: Vec::new(),
            created_at: 0,
        };
        let occ = expand_events(
            std::slice::from_ref(&base),
            ms("2026-06-01T00:00"),
            ms("2026-06-30T23:59"),
        );
        assert_eq!(occ.len(), 3);
        assert!(
            occ.iter().all(|o| o.id == "series1"),
            "occurrences share the series id"
        );
        assert_eq!(occ[0].starts_at, ms("2026-06-01T09:00"));
        assert_eq!(occ[2].starts_at, ms("2026-06-03T09:00"));
        // Duration (15 min) preserved on every occurrence.
        assert_eq!(occ[2].ends_at - occ[2].starts_at, 15 * 60_000);
    }

    #[test]
    fn non_recurring_events_pass_through() {
        let one_off = Event {
            id: "e1".to_string(),
            owner_sub: "alice".to_string(),
            calendar_id: String::new(),
            title: "One-off".to_string(),
            starts_at: ms("2026-06-10T09:00"),
            ends_at: ms("2026-06-10T10:00"),
            all_day: false,
            location: String::new(),
            notes: String::new(),
            rrule: String::new(),
            series_id: String::new(),
            override_occurrence_date: 0,
            exception_dates: Vec::new(),
            created_at: 0,
        };
        let out = expand_events(std::slice::from_ref(&one_off), 0, i64::MAX);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], one_off);
    }

    #[test]
    fn expand_events_skips_exceptions_and_series_overrides() {
        let mut base = Event {
            id: "series1".to_string(),
            owner_sub: "alice".to_string(),
            calendar_id: "default".to_string(),
            title: "Standup".to_string(),
            starts_at: ms("2026-06-01T09:00"),
            ends_at: ms("2026-06-01T09:15"),
            all_day: false,
            location: String::new(),
            notes: String::new(),
            rrule: "FREQ=DAILY;COUNT=3".to_string(),
            series_id: String::new(),
            override_occurrence_date: 0,
            exception_dates: vec![ms("2026-06-02T09:00")],
            created_at: 0,
        };
        let override_event = Event {
            id: "override1".to_string(),
            owner_sub: "alice".to_string(),
            calendar_id: "default".to_string(),
            title: "Moved standup".to_string(),
            starts_at: ms("2026-06-03T12:00"),
            ends_at: ms("2026-06-03T12:15"),
            all_day: false,
            location: String::new(),
            notes: String::new(),
            rrule: String::new(),
            series_id: base.id.clone(),
            override_occurrence_date: ms("2026-06-03T09:00"),
            exception_dates: Vec::new(),
            created_at: 0,
        };
        let out = expand_events(
            &[base.clone(), override_event.clone()],
            ms("2026-06-01T00:00"),
            ms("2026-06-04T00:00"),
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "series1");
        assert_eq!(out[0].starts_at, ms("2026-06-01T09:00"));
        assert_eq!(out[1].id, "override1");
        assert_eq!(out[1].starts_at, ms("2026-06-03T12:00"));

        base.exception_dates.clear();
        let without_exception = expand_events(
            &[base, override_event],
            ms("2026-06-01T00:00"),
            ms("2026-06-04T00:00"),
        );
        assert_eq!(
            without_exception.len(),
            3,
            "only the overridden third occurrence is skipped"
        );
    }

    #[test]
    fn build_rrule_round_trips_through_parse() {
        let s = build_rrule(Some(Freq::Weekly), 2, Some(5), Some("2026-12-31"));
        assert_eq!(s, "FREQ=WEEKLY;INTERVAL=2;COUNT=5;UNTIL=20261231");
        let r = RRule::parse(&s).unwrap();
        assert_eq!(r.freq, Freq::Weekly);
        assert_eq!(r.interval, 2);
        assert_eq!(r.count, Some(5));
        assert_eq!(r.until_date_input(), "2026-12-31");
        // No recurrence selected => empty string (a one-off).
        assert_eq!(build_rrule(None, 1, None, None), "");
        // Defaults omitted: a plain weekly rule is just FREQ=WEEKLY.
        assert_eq!(build_rrule(Some(Freq::Daily), 1, None, None), "FREQ=DAILY");
    }
}
