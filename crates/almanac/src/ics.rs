//! RFC 5545 iCalendar (`.ics`) serialization for the read-only subscription feed + per-event
//! download.
//!
//! Almanac stores every timestamp as a real UTC epoch-ms and the owner's timezone as a FIXED UTC
//! offset (no DST — matching the rest of the crate). We emit each series as ONE `VEVENT` carrying
//! the stored `RRULE`, wrapped in a `VCALENDAR` with a single fixed-offset `VTIMEZONE` that timed
//! events reference by `TZID`; all-day events use `VALUE=DATE`. Every text value is escaped per
//! §3.3.11 (`\\`, `\;`, `\,`, newline → `\n`) and every content line is folded at 75 octets
//! (§3.1). This is a SUBSCRIBE-only feed (Google/Apple "add by URL") — CalDAV write (`PROPFIND`/
//! `PUT`) is a noted future enhancement.
//!
//! No new dependency: the calendar math reuses [`crate::calendar`]'s UTC helpers.

use crate::calendar;
use crate::store::Event;

const DAY_MS: i64 = 86_400_000;

/// Escape a TEXT value per RFC 5545 §3.3.11. Order matters: backslash first.
pub fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {} // CR is dropped; a lone \n already became \\n
            _ => out.push(c),
        }
    }
    out
}

/// Fold one (already-assembled) content line to <=75 OCTETS, per RFC 5545 §3.1: a CRLF followed by
/// a single space continues the line. Splits only on char boundaries so multi-byte UTF-8 is never
/// cut; a continuation's leading space counts toward its 75 octets.
fn fold(line: &str) -> String {
    let mut out = String::with_capacity(line.len() + 8);
    let mut used = 0usize; // octets on the current physical line
    for c in line.chars() {
        let w = c.len_utf8();
        if used + w > 75 {
            out.push_str("\r\n "); // continuation: CRLF + one space (counts toward the next 75)
            used = 1;
        }
        out.push(c);
        used += w;
    }
    out
}

/// `YYYYMMDDTHHMMSSZ` — a UTC DATE-TIME value.
pub fn fmt_utc(ms: i64) -> String {
    let (y, mo, d, h, mi, s) = ymd_hms(ms);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// `YYYYMMDDTHHMMSS` — a local (floating, TZID-qualified) DATE-TIME value. `ms` is already the
/// local wall-clock epoch (caller has applied the offset).
fn fmt_local_dt(local_ms: i64) -> String {
    let (y, mo, d, h, mi, s) = ymd_hms(local_ms);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}")
}

/// `YYYYMMDD` — a DATE value (all-day). `local_ms` is the local wall-clock epoch.
fn fmt_local_date(local_ms: i64) -> String {
    let (y, mo, d, _, _, _) = ymd_hms(local_ms);
    format!("{y:04}{mo:02}{d:02}")
}

fn ymd_hms(ms: i64) -> (i32, u8, u8, u8, u8, u8) {
    match time::OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000)) {
        Ok(dt) => (
            dt.year(),
            dt.month() as u8,
            dt.day(),
            dt.hour(),
            dt.minute(),
            dt.second(),
        ),
        Err(_) => (1970, 1, 1, 0, 0, 0),
    }
}

/// A `TZID` token safe to place unquoted in a parameter value — colons are stripped so the
/// property/value separator is never ambiguous (`UTC`, `UTC+0800`, `UTC-0500`).
pub fn tzid(off_min: i32) -> String {
    if off_min == 0 {
        "UTC".to_string()
    } else {
        format!("UTC{}", offset_hhmm(off_min))
    }
}

/// `+HHMM` / `-HHMM` for a UTC offset in minutes (used by TZID + TZOFFSETFROM/TO).
fn offset_hhmm(off_min: i32) -> String {
    let sign = if off_min < 0 { '-' } else { '+' };
    let abs = off_min.abs();
    format!("{sign}{:02}{:02}", abs / 60, abs % 60)
}

/// The single fixed-offset VTIMEZONE. STANDARD only (no DST); TZOFFSETFROM == TZOFFSETTO.
fn vtimezone(off_min: i32) -> Vec<String> {
    let off = offset_hhmm(off_min);
    let id = tzid(off_min);
    vec![
        "BEGIN:VTIMEZONE".to_string(),
        format!("TZID:{id}"),
        "BEGIN:STANDARD".to_string(),
        "DTSTART:19700101T000000".to_string(),
        format!("TZOFFSETFROM:{off}"),
        format!("TZOFFSETTO:{off}"),
        format!("TZNAME:{id}"),
        "END:STANDARD".to_string(),
        "END:VTIMEZONE".to_string(),
    ]
}

/// Adapt a stored RRULE to a valid ICS `RRULE` line for the given event shape. The only value that
/// needs adjusting is `UNTIL`: our compose UI stores it as a bare `YYYYMMDD`, but RFC 5545 requires
/// UNTIL to match DTSTART's value type — a DATE for all-day events, otherwise a UTC DATE-TIME. For
/// timed events the stored local date is converted to the UTC instant of the owner-local end of
/// that day. Everything else (FREQ/INTERVAL/COUNT/BYDAY) passes through byte-for-byte.
fn rrule_for_ics(stored: &str, all_day: bool, off_min: i32) -> Option<String> {
    let stored = stored.trim();
    if stored.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    for tok in stored.split(';') {
        let (key, val) = match tok.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        if key.eq_ignore_ascii_case("UNTIL") && !all_day {
            let digits: String = val.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.len() == 8 {
                // Treat the stored YYYYMMDD as the owner-LOCAL day; UNTIL = UTC instant of its end.
                if let Some(local_midnight) = calendar::parse_date(&format!(
                    "{}-{}-{}",
                    &digits[0..4],
                    &digits[4..6],
                    &digits[6..8]
                )) {
                    let utc_eod = local_midnight - off_min as i64 * 60_000 + DAY_MS - 1000;
                    parts.push(format!("UNTIL={}", fmt_utc(utc_eod)));
                    continue;
                }
            }
        }
        parts.push(format!("{key}={val}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("RRULE:{}", parts.join(";")))
    }
}

/// The lines of one VEVENT. `now_ms` fills DTSTAMP; `host` forms the UID domain.
fn vevent(e: &Event, off_min: i32, now_ms: i64, host: &str) -> Vec<String> {
    let mut out = vec!["BEGIN:VEVENT".to_string()];
    out.push(format!("UID:{}@{host}", e.id));
    out.push(format!("DTSTAMP:{}", fmt_utc(now_ms)));

    let id = tzid(off_min);
    if e.all_day {
        let start_local = e.starts_at + off_min as i64 * 60_000;
        // DTEND is EXCLUSIVE: the day after the local last day.
        let end_local = e.ends_at + off_min as i64 * 60_000;
        let end_excl = calendar::start_of_day(end_local) + DAY_MS;
        out.push(format!(
            "DTSTART;VALUE=DATE:{}",
            fmt_local_date(start_local)
        ));
        out.push(format!("DTEND;VALUE=DATE:{}", fmt_local_date(end_excl)));
    } else {
        let start_local = e.starts_at + off_min as i64 * 60_000;
        let end_local = e.ends_at + off_min as i64 * 60_000;
        out.push(format!("DTSTART;TZID={id}:{}", fmt_local_dt(start_local)));
        out.push(format!("DTEND;TZID={id}:{}", fmt_local_dt(end_local)));
    }

    out.push(format!("SUMMARY:{}", escape_text(&e.title)));
    if !e.location.trim().is_empty() {
        out.push(format!("LOCATION:{}", escape_text(&e.location)));
    }
    if !e.notes.trim().is_empty() {
        out.push(format!("DESCRIPTION:{}", escape_text(&e.notes)));
    }
    if let Some(rule) = rrule_for_ics(&e.rrule, e.all_day, off_min) {
        out.push(rule);
    }
    out.push("END:VEVENT".to_string());
    out
}

/// True when at least one event is timed (needs a VTIMEZONE reference).
fn any_timed(events: &[Event]) -> bool {
    events.iter().any(|e| !e.all_day)
}

/// Assemble a full VCALENDAR document (CRLF-terminated, folded) from `lines`.
fn wrap_calendar(mut body: Vec<String>, off_min: i32, include_vtimezone: bool) -> String {
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//HOLDFAST//Almanac//EN".to_string(),
        "CALSCALE:GREGORIAN".to_string(),
        "METHOD:PUBLISH".to_string(),
    ];
    if include_vtimezone {
        lines.extend(vtimezone(off_min));
    }
    lines.append(&mut body);
    lines.push("END:VCALENDAR".to_string());
    let mut doc = String::new();
    for line in lines {
        doc.push_str(&fold(&line));
        doc.push_str("\r\n");
    }
    doc
}

/// The whole subscription feed: every event as a VEVENT (+ one VTIMEZONE when any event is timed).
pub fn calendar_feed(events: &[Event], off_min: i32, now_ms: i64, host: &str) -> String {
    let mut body = Vec::new();
    for e in events {
        body.extend(vevent(e, off_min, now_ms, host));
    }
    wrap_calendar(body, off_min, any_timed(events))
}

/// A single-event `.ics` (VTIMEZONE included only when the event is timed).
pub fn event_ics(e: &Event, off_min: i32, now_ms: i64, host: &str) -> String {
    let body = vevent(e, off_min, now_ms, host);
    wrap_calendar(body, off_min, !e.all_day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::{parse_date, parse_datetime_local};

    fn timed_event(title: &str, rrule: &str) -> Event {
        Event {
            id: "evt123".to_string(),
            owner_sub: "alice".to_string(),
            calendar_id: String::new(),
            title: title.to_string(),
            starts_at: parse_datetime_local("2026-06-15T14:30").unwrap(),
            ends_at: parse_datetime_local("2026-06-15T15:30").unwrap(),
            all_day: false,
            location: "War, room".to_string(),
            notes: "Bring; the deck\nrow2".to_string(),
            rrule: rrule.to_string(),
            series_id: String::new(),
            override_occurrence_date: 0,
            exception_dates: Vec::new(),
            created_at: 0,
        }
    }

    #[test]
    fn escapes_text_special_chars() {
        assert_eq!(escape_text("a;b,c\\d\ne"), "a\\;b\\,c\\\\d\\ne");
    }

    #[test]
    fn folds_long_lines_at_75_octets() {
        let line = format!("SUMMARY:{}", "x".repeat(200));
        let folded = fold(&line);
        for physical in folded.split("\r\n") {
            assert!(
                physical.len() <= 75,
                "physical line <=75 octets: {}",
                physical.len()
            );
        }
        // Unfolding (drop CRLF + leading space) restores the original.
        let restored = folded.replace("\r\n ", "");
        assert_eq!(restored, line);
    }

    #[test]
    fn utc_datetime_format() {
        let ms = parse_datetime_local("2026-06-15T14:30").unwrap();
        assert_eq!(fmt_utc(ms), "20260615T143000Z");
    }

    #[test]
    fn feed_has_vevent_and_vtimezone_and_escaping() {
        let off = 480; // UTC+08:00
        let now = parse_datetime_local("2026-06-01T00:00").unwrap();
        let ics = calendar_feed(&[timed_event("Sync", "")], off, now, "cal.w33d.xyz");
        assert!(ics.starts_with("BEGIN:VCALENDAR\r\n"));
        assert!(ics.contains("BEGIN:VTIMEZONE"));
        assert!(ics.contains("TZID:UTC+0800"));
        assert!(ics.contains("TZOFFSETTO:+0800"));
        assert!(ics.contains("BEGIN:VEVENT"));
        assert!(ics.contains("UID:evt123@cal.w33d.xyz"));
        assert!(ics.contains("DTSTAMP:"));
        // 14:30 UTC + 8h offset => 22:30 local wall clock, TZID-qualified.
        assert!(
            ics.contains("DTSTART;TZID=UTC+0800:20260615T223000"),
            "local DTSTART: {ics}"
        );
        assert!(ics.contains("DTEND;TZID=UTC+0800:20260615T233000"));
        // Escaping in SUMMARY/LOCATION/DESCRIPTION.
        assert!(ics.contains("LOCATION:War\\, room"));
        assert!(ics.contains("DESCRIPTION:Bring\\; the deck\\nrow2"));
        assert!(ics.trim_end().ends_with("END:VCALENDAR"));
        // CRLF line endings throughout.
        assert!(ics.contains("\r\n"));
        assert!(!ics.contains("\n\n"));
    }

    #[test]
    fn timed_rrule_until_becomes_utc_datetime() {
        let off = 480; // UTC+08:00
        let now = 0;
        let ics = event_ics(
            &timed_event("R", "FREQ=WEEKLY;COUNT=3;UNTIL=20260630"),
            off,
            now,
            "h",
        );
        // COUNT passes through; UNTIL date -> UTC instant of local end-of-day.
        // Local 2026-06-30 end-of-day (23:59:59) in UTC+8 == 2026-06-30 15:59:59 UTC.
        assert!(
            ics.contains("RRULE:FREQ=WEEKLY;COUNT=3;UNTIL=20260630T155959Z"),
            "{ics}"
        );
    }

    #[test]
    fn all_day_uses_value_date_and_exclusive_end() {
        let off = 0;
        let mut e = timed_event("Conf", "");
        e.all_day = true;
        e.starts_at = parse_date("2026-03-10").unwrap();
        e.ends_at = crate::calendar::end_of_day(e.starts_at);
        let ics = event_ics(&e, off, 0, "h");
        assert!(ics.contains("DTSTART;VALUE=DATE:20260310"));
        // DTEND is the exclusive next day.
        assert!(ics.contains("DTEND;VALUE=DATE:20260311"), "{ics}");
        // A pure all-day, off=0 single event needs no VTIMEZONE.
        assert!(!ics.contains("VTIMEZONE"));
    }

    #[test]
    fn all_day_rrule_until_stays_a_date() {
        let off = 0;
        let mut e = timed_event("Daily", "FREQ=DAILY;UNTIL=20260315");
        e.all_day = true;
        e.starts_at = parse_date("2026-03-10").unwrap();
        e.ends_at = crate::calendar::end_of_day(e.starts_at);
        let ics = event_ics(&e, off, 0, "h");
        assert!(ics.contains("RRULE:FREQ=DAILY;UNTIL=20260315"), "{ics}");
    }
}
