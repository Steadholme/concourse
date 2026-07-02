//! End-to-end flow for the Google-Calendar-parity additions: attendees + RSVP tokens, the ICS
//! subscription feed + per-event download, reminders wiring, and natural-language quick-add.
//!
//! Drives the real `Router` in-process via `tower::oneshot` against the in-memory store (NO
//! database), exactly like `calendar_flow.rs`.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

use almanac::{app, build_dev_state, AppState};

#[tokio::test]
async fn attendee_rsvp_token_flow_updates_status() {
    let state = build_dev_state();

    // Create an event WITH two attendees + reminders through the real form flow.
    let (_s, headers, _b) = call(&state, get_as("/new", "u_alice", "a@x.co")).await;
    let cookie = set_cookie(&headers).unwrap();
    let csrf = cookie_value(&cookie).unwrap();
    let save = post_form(
        "/new",
        &cookie,
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", &csrf),
            ("title", "Launch review"),
            ("starts_at", "2099-06-15T10:00"),
            ("ends_at", "2099-06-15T11:00"),
            ("attendees", "Grace Hopper <grace@navy.mil>\nbob@x.co"),
            ("rem_10", "on"),
            ("rem_60", "on"),
        ],
    );
    assert_eq!(call(&state, save).await.0, StatusCode::SEE_OTHER);

    // The event id, from the month grid.
    let (_s, _h, month) = call(&state, get_as("/?y=2099&m=6", "u_alice", "a@x.co")).await;
    let id = find_between(&month, "/edit/", "\"").expect("event id");

    // The detail page lists both attendees (escaped) + their RSVP links + reminders.
    let (status, _h, detail) = call(&state, get_as(&format!("/event/{id}"), "u_alice", "a@x.co")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.contains("Grace Hopper"));
    assert!(detail.contains("grace@navy.mil"));
    assert!(detail.contains("No response"), "initial status is needs-action");
    assert!(detail.contains("10 minutes before"));
    assert!(detail.contains("1 hour before"));
    // Grab one attendee's RSVP token from a /rsvp/{token} link.
    let token = find_between(&detail, "/rsvp/", "\"").expect("rsvp token");
    assert_eq!(token.len(), 64, "token is an unguessable 64-hex capability");

    // The RSVP page is PUBLIC — no gateway identity headers at all.
    let (status, _h, page) = call(&state, get(&format!("/rsvp/{token}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("You're invited"));
    assert!(page.contains("Launch review"));

    // Accept via the link; the confirmation reflects it.
    let (status, _h, confirmed) = call(&state, get(&format!("/rsvp/{token}?reply=accepted"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(confirmed.contains("recorded as"));
    assert!(confirmed.contains("Accepted"));

    // The organizer's detail page now shows the accepted status.
    let (_s, _h, detail2) = call(&state, get_as(&format!("/event/{id}"), "u_alice", "a@x.co")).await;
    assert!(detail2.contains("Accepted"));

    // Change the reply to declined; status flips.
    let (_s, _h, declined) = call(&state, get(&format!("/rsvp/{token}?reply=declined"))).await;
    assert!(declined.contains("Declined"));
    let (_s, _h, detail3) = call(&state, get_as(&format!("/event/{id}"), "u_alice", "a@x.co")).await;
    assert!(detail3.contains("Declined"));

    // An unknown token is a public 404.
    let (status, _h, _b) = call(&state, get("/rsvp/deadbeef")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rsvp_page_escapes_attendee_name() {
    let state = build_dev_state();
    let (_s, headers, _b) = call(&state, get_as("/new", "u_alice", "a@x.co")).await;
    let cookie = set_cookie(&headers).unwrap();
    let csrf = cookie_value(&cookie).unwrap();
    let save = post_form(
        "/new",
        &cookie,
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", &csrf),
            ("title", "Party"),
            ("starts_at", "2099-08-01T18:00"),
            ("ends_at", "2099-08-01T20:00"),
            ("attendees", "<script>alert(9)</script> <x@y.co>"),
        ],
    );
    assert_eq!(call(&state, save).await.0, StatusCode::SEE_OTHER);
    let (_s, _h, month) = call(&state, get_as("/?y=2099&m=8", "u_alice", "a@x.co")).await;
    let id = find_between(&month, "/edit/", "\"").unwrap();
    let (_s, _h, detail) = call(&state, get_as(&format!("/event/{id}"), "u_alice", "a@x.co")).await;
    let token = find_between(&detail, "/rsvp/", "\"").unwrap();
    let (_s, _h, page) = call(&state, get(&format!("/rsvp/{token}"))).await;
    assert!(!page.contains("<script>alert(9)</script>"), "attendee name must be escaped");
    assert!(page.contains("&lt;script&gt;"));
}

#[tokio::test]
async fn ics_feed_and_event_download() {
    let state = build_dev_state();

    // Owner on UTC+08:00 so the feed exercises VTIMEZONE + TZID + local times.
    let (_s, sh, _b) = call(&state, get_as("/settings", "u_alice", "a@x.co")).await;
    let scookie = set_cookie(&sh).unwrap();
    let scsrf = cookie_value(&scookie).unwrap();
    let save_tz = post_form(
        "/settings",
        &scookie,
        "u_alice",
        "a@x.co",
        &[("csrf_token", &scsrf), ("timezone", "UTC+08:00"), ("week_start", "monday")],
    );
    assert_eq!(call(&state, save_tz).await.0, StatusCode::SEE_OTHER);

    // A weekly recurring event with a comma + semicolon in text to check escaping.
    let (_s, ch, _b) = call(&state, get_as("/new", "u_alice", "a@x.co")).await;
    let cookie = set_cookie(&ch).unwrap();
    let csrf = cookie_value(&cookie).unwrap();
    let save = post_form(
        "/new",
        &cookie,
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", &csrf),
            ("title", "Sync, weekly; team"),
            ("starts_at", "2099-06-15T10:00"),
            ("ends_at", "2099-06-15T10:30"),
            ("location", "Room A, B"),
            ("repeat", "weekly"),
            ("repeat_interval", "1"),
            ("repeat_count", "5"),
        ],
    );
    assert_eq!(call(&state, save).await.0, StatusCode::SEE_OTHER);

    // The subscription feed.
    let (status, h, ics) = call(&state, get_as("/calendar.ics", "u_alice", "a@x.co")).await;
    assert_eq!(status, StatusCode::OK);
    let ctype = h.get(header::CONTENT_TYPE).unwrap().to_str().unwrap();
    assert!(ctype.starts_with("text/calendar"), "content-type is text/calendar");
    assert!(ics.starts_with("BEGIN:VCALENDAR\r\n"));
    assert!(ics.contains("BEGIN:VTIMEZONE"));
    assert!(ics.contains("TZID:UTC+0800"));
    assert!(ics.contains("BEGIN:VEVENT"));
    // 10:00 UTC+8 local wall clock via TZID.
    assert!(ics.contains("DTSTART;TZID=UTC+0800:20990615T100000"), "{ics}");
    assert!(ics.contains("RRULE:FREQ=WEEKLY;COUNT=5"));
    // TEXT escaping of comma + semicolon.
    assert!(ics.contains("SUMMARY:Sync\\, weekly\\; team"));
    assert!(ics.contains("LOCATION:Room A\\, B"));
    assert!(ics.trim_end().ends_with("END:VCALENDAR"));

    // Per-event download.
    let id = {
        let (_s, _h, month) = call(&state, get_as("/?y=2099&m=6", "u_alice", "a@x.co")).await;
        find_between(&month, "/edit/", "\"").unwrap()
    };
    let (status, h, one) = call(&state, get_as(&format!("/event/{id}/ics"), "u_alice", "a@x.co")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(h
        .get(header::CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap()
        .contains("event.ics"));
    assert_eq!(one.matches("BEGIN:VEVENT").count(), 1, "single VEVENT");

    // Another owner's feed does not leak alice's event.
    let (_s, _h, bob) = call(&state, get_as("/calendar.ics", "u_bob", "b@x.co")).await;
    assert!(!bob.contains("Sync"), "ICS feed is owner-scoped");
}

#[tokio::test]
async fn quick_add_creates_event_or_falls_back() {
    let state = build_dev_state();

    // Mint a CSRF cookie from the index (which renders the quick-add box).
    let (_s, headers, home) = call(&state, get_as("/", "u_alice", "a@x.co")).await;
    assert!(home.contains("Quick add"), "index shows the quick-add box");
    let cookie = set_cookie(&headers).unwrap();
    let csrf = cookie_value(&cookie).unwrap();

    // A parseable phrase creates an event (redirects to its month).
    let qa = post_form(
        "/quick-add",
        &cookie,
        "u_alice",
        "a@x.co",
        &[("csrf_token", &csrf), ("text", "Lunch tomorrow 12pm")],
    );
    let (status, loc_headers, _b) = call(&state, qa).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "quick-add creates + redirects");
    let location = loc_headers.get(header::LOCATION).unwrap().to_str().unwrap().to_string();
    let (_s, _h, month) = call(&state, get_as(&location, "u_alice", "a@x.co")).await;
    assert!(month.contains("Lunch"), "the quick-added event appears");

    // An unparseable phrase falls back to the editor, prefilled with the raw text as the title.
    let qa2 = post_form(
        "/quick-add",
        &cookie,
        "u_alice",
        "a@x.co",
        &[("csrf_token", &csrf), ("text", "buy milk")],
    );
    let (status, _h, editor) = call(&state, qa2).await;
    assert_eq!(status, StatusCode::OK, "fallback re-renders the editor (200)");
    assert!(editor.contains("New event"));
    assert!(editor.contains("value=\"buy milk\""), "raw text prefilled as the title");
}

// --- helpers (mirrors calendar_flow.rs) -----------------------------------------------

async fn call(state: &AppState, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, headers, String::from_utf8_lossy(&bytes).to_string())
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn get_as(uri: &str, subject: &str, email: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("x-auth-subject", subject)
        .header("x-auth-email", email)
        .body(Body::empty())
        .unwrap()
}

fn post_form(uri: &str, cookie: &str, subject: &str, email: &str, pairs: &[(&str, &str)]) -> Request<Body> {
    let body = form_encode(pairs);
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header("x-auth-subject", subject)
        .header("x-auth-email", email);
    if !cookie.is_empty() {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::from(body)).unwrap()
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", pct(k), pct(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn set_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn cookie_value(set_cookie: &str) -> Option<String> {
    let first = set_cookie.split(';').next()?;
    let (_name, value) = first.split_once('=')?;
    Some(value.to_string())
}

fn find_between(s: &str, start: &str, end: &str) -> Option<String> {
    let i = s.find(start)? + start.len();
    let rest = &s[i..];
    let j = rest.find(end)?;
    Some(rest[..j].to_string())
}
