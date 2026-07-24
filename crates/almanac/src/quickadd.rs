//! A tiny, PURE natural-language quick-add grammar: `<title> [day] <time>`.
//!
//! Parses phrases like `Lunch tomorrow 12pm`, `Standup mon 9:30am`, `Review 14:00` into a
//! [`QuickAdd`] (title + a relative [`DaySpec`] + hour/minute). It is deliberately SMALL and
//! well-tested — NOT an NLP engine: a phrase is parseable only when it contains a recognisable
//! TIME token (`h(:mm)(am|pm)`, requiring either `:` or an am/pm suffix so bare numbers in a title
//! are never mistaken for a clock) AND a non-empty title. The day defaults to today when absent.
//! Anything else returns `None`, so the handler falls back to the normal editor prefilled with the
//! raw text. Resolving a [`DaySpec`] against "today" is a separate pure function so both halves are
//! unit-testable with no clock.

use time::Weekday;

const DAY_MS: i64 = 86_400_000;

/// A relative day reference parsed from the phrase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaySpec {
    Today,
    Tomorrow,
    /// The nearest date on/after today with this weekday (today counts when it matches).
    Weekday(Weekday),
}

/// The parsed result of a quick-add phrase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickAdd {
    pub title: String,
    pub day: DaySpec,
    /// 0..=23.
    pub hour: u8,
    /// 0..=59.
    pub minute: u8,
}

/// Parse a quick-add phrase. Returns `None` when there is no clear time or no title left.
pub fn parse_quick_add(input: &str) -> Option<QuickAdd> {
    let tokens: Vec<&str> = input.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }

    let mut day: Option<DaySpec> = None;
    let mut time: Option<(u8, u8)> = None;
    let mut title_words: Vec<&str> = Vec::new();

    for tok in tokens {
        // A recognised time (first one wins); later time-looking tokens fall through to the title.
        if time.is_none() {
            if let Some(hm) = parse_time(tok) {
                time = Some(hm);
                continue;
            }
        }
        // A recognised day keyword (first one wins).
        if day.is_none() {
            if let Some(d) = parse_day(tok) {
                day = Some(d);
                continue;
            }
        }
        // Drop a bare connector so "Lunch at 12pm" titles as "Lunch".
        if tok.eq_ignore_ascii_case("at") {
            continue;
        }
        title_words.push(tok);
    }

    let (hour, minute) = time?; // no time => not parseable
    let title = title_words.join(" ");
    if title.trim().is_empty() {
        return None; // an event needs a title
    }

    Some(QuickAdd {
        title,
        day: day.unwrap_or(DaySpec::Today),
        hour,
        minute,
    })
}

/// Parse a day keyword: `today`, `tomorrow`, or a weekday name/abbreviation.
fn parse_day(tok: &str) -> Option<DaySpec> {
    match tok.to_ascii_lowercase().as_str() {
        "today" | "tod" => Some(DaySpec::Today),
        "tomorrow" | "tmr" | "tmrw" => Some(DaySpec::Tomorrow),
        "mon" | "monday" => Some(DaySpec::Weekday(Weekday::Monday)),
        "tue" | "tues" | "tuesday" => Some(DaySpec::Weekday(Weekday::Tuesday)),
        "wed" | "weds" | "wednesday" => Some(DaySpec::Weekday(Weekday::Wednesday)),
        "thu" | "thur" | "thurs" | "thursday" => Some(DaySpec::Weekday(Weekday::Thursday)),
        "fri" | "friday" => Some(DaySpec::Weekday(Weekday::Friday)),
        "sat" | "saturday" => Some(DaySpec::Weekday(Weekday::Saturday)),
        "sun" | "sunday" => Some(DaySpec::Weekday(Weekday::Sunday)),
        _ => None,
    }
}

/// Parse a clock token: `h`, `h:mm`, optionally suffixed `am`/`pm`. Requires a `:` or an am/pm
/// suffix (a bare number is NOT a time), so title words like "Room 12" are never misread. Returns
/// `(hour_0_23, minute)`.
fn parse_time(tok: &str) -> Option<(u8, u8)> {
    let lower = tok.to_ascii_lowercase();
    let (body, meridiem) = if let Some(b) = lower.strip_suffix("am") {
        (b, Some(false))
    } else if let Some(b) = lower.strip_suffix("pm") {
        (b, Some(true))
    } else {
        (lower.as_str(), None)
    };
    let (h_str, m_str) = match body.split_once(':') {
        Some((h, m)) => (h, m),
        None => (body, ""),
    };
    // Require SOME disambiguator: either a minute part (had a colon) or a meridiem.
    if m_str.is_empty() && meridiem.is_none() {
        return None;
    }
    let mut hour: i32 = h_str.parse().ok()?;
    let minute: u8 = if m_str.is_empty() {
        0
    } else {
        if m_str.len() != 2 {
            return None;
        }
        let m: u8 = m_str.parse().ok()?;
        if m > 59 {
            return None;
        }
        m
    };
    match meridiem {
        Some(is_pm) => {
            if !(1..=12).contains(&hour) {
                return None;
            }
            hour %= 12; // 12am -> 0, 12pm -> 12 (after +12 below)
            if is_pm {
                hour += 12;
            }
        }
        None => {
            if !(0..=23).contains(&hour) {
                return None;
            }
        }
    }
    Some((hour as u8, minute))
}

/// Resolve a [`DaySpec`] to a LOCAL-midnight epoch, given the owner's local-midnight "today".
pub fn resolve_local_midnight(day: DaySpec, today_local_midnight: i64) -> i64 {
    match day {
        DaySpec::Today => today_local_midnight,
        DaySpec::Tomorrow => today_local_midnight + DAY_MS,
        DaySpec::Weekday(target) => {
            let today_wd = match time::OffsetDateTime::from_unix_timestamp(
                today_local_midnight.div_euclid(1000),
            ) {
                Ok(dt) => dt.weekday(),
                Err(_) => Weekday::Monday,
            };
            let ahead = (target.number_days_from_monday() as i64
                - today_wd.number_days_from_monday() as i64)
                .rem_euclid(7);
            today_local_midnight + ahead * DAY_MS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::{fmt_date_input, parse_date};

    #[test]
    fn parses_title_day_and_meridiem_time() {
        let qa = parse_quick_add("Lunch tomorrow 12pm").unwrap();
        assert_eq!(qa.title, "Lunch");
        assert_eq!(qa.day, DaySpec::Tomorrow);
        assert_eq!((qa.hour, qa.minute), (12, 0));
    }

    #[test]
    fn parses_weekday_and_minutes() {
        let qa = parse_quick_add("Standup mon 9:30am").unwrap();
        assert_eq!(qa.title, "Standup");
        assert_eq!(qa.day, DaySpec::Weekday(Weekday::Monday));
        assert_eq!((qa.hour, qa.minute), (9, 30));
    }

    #[test]
    fn multiword_title_and_24h_time_default_today() {
        let qa = parse_quick_add("Design review 14:00").unwrap();
        assert_eq!(qa.title, "Design review");
        assert_eq!(qa.day, DaySpec::Today); // no day keyword
        assert_eq!((qa.hour, qa.minute), (14, 0));
    }

    #[test]
    fn drops_the_at_connector() {
        let qa = parse_quick_add("Coffee at 3pm").unwrap();
        assert_eq!(qa.title, "Coffee");
        assert_eq!((qa.hour, qa.minute), (15, 0));
    }

    #[test]
    fn midnight_and_noon_meridiem_edges() {
        assert_eq!(parse_quick_add("A 12am").unwrap().hour, 0);
        assert_eq!(parse_quick_add("B 12pm").unwrap().hour, 12);
        assert_eq!(parse_quick_add("C 1pm").unwrap().hour, 13);
    }

    #[test]
    fn unparseable_falls_back_to_none() {
        // No time token at all.
        assert!(parse_quick_add("Lunch tomorrow").is_none());
        // A bare number is not a time (avoids grabbing "12" from a title).
        assert!(parse_quick_add("Room 12").is_none());
        // Time but no title.
        assert!(parse_quick_add("3pm").is_none());
        assert!(parse_quick_add("tomorrow 9am").is_none());
        // Empty input.
        assert!(parse_quick_add("   ").is_none());
        // Out-of-range clock.
        assert!(parse_quick_add("X 25:00").is_none());
        assert!(parse_quick_add("X 13pm").is_none());
    }

    #[test]
    fn resolves_relative_days() {
        // 2026-06-15 is a Monday.
        let today = parse_date("2026-06-15").unwrap();
        assert_eq!(
            fmt_date_input(resolve_local_midnight(DaySpec::Today, today)),
            "2026-06-15"
        );
        assert_eq!(
            fmt_date_input(resolve_local_midnight(DaySpec::Tomorrow, today)),
            "2026-06-16"
        );
        // "wednesday" from a Monday => the same week's Wednesday (Jun 17).
        assert_eq!(
            fmt_date_input(resolve_local_midnight(
                DaySpec::Weekday(Weekday::Wednesday),
                today
            )),
            "2026-06-17"
        );
        // The current weekday resolves to today (Monday -> Jun 15).
        assert_eq!(
            fmt_date_input(resolve_local_midnight(
                DaySpec::Weekday(Weekday::Monday),
                today
            )),
            "2026-06-15"
        );
        // "sunday" from a Monday => the coming Sunday (Jun 21).
        assert_eq!(
            fmt_date_input(resolve_local_midnight(
                DaySpec::Weekday(Weekday::Sunday),
                today
            )),
            "2026-06-21"
        );
    }
}
