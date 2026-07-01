//! End-to-end calendar + contacts flow against the in-memory store (NO database).
//!
//! Drives the real `Router` in-process via `tower::oneshot`, exactly like the rest of the
//! estate. Covers: liveness, the empty calendar, the CSRF-protected event create/edit/delete
//! cycle (owner taken from the gateway `X-Auth-Subject`), the calendar grid + upcoming agenda,
//! all-day normalization, per-owner isolation, the contacts CRUD, CSRF enforcement, stored-text
//! escaping, and the 404 fallback.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

use almanac::{app, build_dev_state, AppState};

#[tokio::test]
async fn healthz_ok() {
    let state = build_dev_state();
    let (status, _h, body) = call(&state, get("/healthz")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");
}

#[tokio::test]
async fn full_event_lifecycle_scoped_to_owner() {
    let state = build_dev_state();

    // Empty calendar for alice.
    let (status, _h, body) = call(&state, get_as("/", "u_alice", "alice@holdfast.local")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Calendar"));
    assert!(body.contains("No upcoming events"));

    // Open the new-event form; capture the minted CSRF token (cookie == hidden field).
    let (status, headers, body) = call(&state, get_as("/new", "u_alice", "alice@holdfast.local")).await;
    assert_eq!(status, StatusCode::OK);
    let cookie = set_cookie(&headers).expect("form sets a CSRF cookie");
    let csrf = cookie_value(&cookie).expect("csrf cookie value");
    assert!(body.contains(&format!("value=\"{csrf}\"")), "hidden field matches cookie");

    // Create a far-future event so it is always "upcoming" in the agenda.
    let save = post_form(
        "/new",
        &cookie,
        "u_alice",
        "alice@holdfast.local",
        &[
            ("csrf_token", &csrf),
            ("title", "Quarterly review"),
            ("starts_at", "2099-06-15T10:00"),
            ("ends_at", "2099-06-15T11:30"),
            ("location", "War room"),
            ("notes", "Bring the deck."),
        ],
    );
    let (status, _h, _b) = call(&state, save).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "create redirects (POST->GET)");

    // The event's month shows it in the grid AND the agenda.
    let (status, _h, month) = call(&state, get_as("/?y=2099&m=6", "u_alice", "alice@holdfast.local")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(month.contains("June 2099"));
    assert!(month.contains("Quarterly review"));
    assert!(month.contains("War room"), "location shown in agenda");
    let id = find_between(&month, "/edit/", "\"").expect("an event id link");

    // Bob sees none of alice's events and cannot open her event.
    let (_s, _h, bob_view) = call(&state, get_as("/?y=2099&m=6", "u_bob", "bob@holdfast.local")).await;
    assert!(!bob_view.contains("Quarterly review"), "events are owner-scoped");
    let (status, _h, _b) = call(&state, get_as(&format!("/edit/{id}"), "u_bob", "bob@holdfast.local")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "bob cannot open alice's event");

    // Alice edits the event (new CSRF from the edit form).
    let (_s, h2, _b) = call(&state, get_as(&format!("/edit/{id}"), "u_alice", "alice@holdfast.local")).await;
    let cookie2 = set_cookie(&h2).unwrap();
    let csrf2 = cookie_value(&cookie2).unwrap();
    let save2 = post_form(
        &format!("/edit/{id}"),
        &cookie2,
        "u_alice",
        "alice@holdfast.local",
        &[
            ("csrf_token", &csrf2),
            ("title", "Quarterly review (rescheduled)"),
            ("starts_at", "2099-06-16T14:00"),
            ("ends_at", "2099-06-16T15:00"),
        ],
    );
    let (status, _h, _b) = call(&state, save2).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_s, _h, month) = call(&state, get_as("/?y=2099&m=6", "u_alice", "alice@holdfast.local")).await;
    assert!(month.contains("Quarterly review (rescheduled)"));

    // Bob cannot delete alice's event (it survives); alice can.
    let bad_del = post_form(
        &format!("/delete/{id}"),
        &cookie2,
        "u_bob",
        "bob@holdfast.local",
        &[("csrf_token", &csrf2)],
    );
    let (status, _h, _b) = call(&state, bad_del).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "delete always redirects");
    let (_s, _h, still) = call(&state, get_as("/?y=2099&m=6", "u_alice", "alice@holdfast.local")).await;
    assert!(still.contains("Quarterly review"), "bob's delete affected nothing");

    let del = post_form(
        &format!("/delete/{id}"),
        &cookie2,
        "u_alice",
        "alice@holdfast.local",
        &[("csrf_token", &csrf2)],
    );
    let (status, _h, _b) = call(&state, del).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_s, _h, gone) = call(&state, get_as("/?y=2099&m=6", "u_alice", "alice@holdfast.local")).await;
    assert!(!gone.contains("Quarterly review"), "event deleted");
}

#[tokio::test]
async fn all_day_event_spans_the_day() {
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
            ("title", "Conference"),
            ("starts_at", "2099-03-10T13:00"),
            ("ends_at", "2099-03-10T13:00"),
            ("all_day", "on"),
        ],
    );
    let (status, _h, _b) = call(&state, save).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_s, _h, month) = call(&state, get_as("/?y=2099&m=3", "u_alice", "a@x.co")).await;
    assert!(month.contains("Conference"));
    assert!(month.contains("cal-event--allday"), "rendered as an all-day chip");
    assert!(month.contains("All day"), "agenda labels it all-day");
}

#[tokio::test]
async fn recurring_event_expands_in_month_and_agenda() {
    let state = build_dev_state();
    let (_s, headers, _b) = call(&state, get_as("/new", "u_alice", "a@x.co")).await;
    let cookie = set_cookie(&headers).unwrap();
    let csrf = cookie_value(&cookie).unwrap();

    // A weekly series on a Wednesday (2099-06-03), 4 occurrences.
    let save = post_form(
        "/new",
        &cookie,
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", &csrf),
            ("title", "Weekly sync"),
            ("starts_at", "2099-06-03T09:00"),
            ("ends_at", "2099-06-03T09:30"),
            ("repeat", "weekly"),
            ("repeat_interval", "1"),
            ("repeat_count", "4"),
        ],
    );
    let (status, _h, _b) = call(&state, save).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // June 2099: the four Wednesdays (3, 10, 17, 24) each render an occurrence chip.
    let (_s, _h, month) = call(&state, get_as("/?y=2099&m=6", "u_alice", "a@x.co")).await;
    let occurrences = month.matches("Weekly sync").count();
    assert!(occurrences >= 4, "each weekly occurrence renders (got {occurrences})");
    assert!(month.contains("↻"), "recurring occurrences carry a repeat marker");

    // The edit form pre-fills the recurrence controls from the stored RRULE.
    let id = find_between(&month, "/edit/", "\"").expect("event id");
    let (_s, _h, edit) = call(&state, get_as(&format!("/edit/{id}"), "u_alice", "a@x.co")).await;
    assert!(edit.contains("value=\"weekly\" selected"), "frequency pre-selected");
    assert!(edit.contains("name=\"repeat_count\" min=\"1\" max=\"9999\" value=\"4\""), "count pre-filled");

    // Editing the series (change the title) applies to every occurrence (v1 series semantics).
    let (_s, h2, _b) = call(&state, get_as(&format!("/edit/{id}"), "u_alice", "a@x.co")).await;
    let cookie2 = set_cookie(&h2).unwrap();
    let csrf2 = cookie_value(&cookie2).unwrap();
    let save2 = post_form(
        &format!("/edit/{id}"),
        &cookie2,
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", &csrf2),
            ("title", "Weekly sync (renamed)"),
            ("starts_at", "2099-06-03T09:00"),
            ("ends_at", "2099-06-03T09:30"),
            ("repeat", "weekly"),
            ("repeat_interval", "1"),
            ("repeat_count", "4"),
        ],
    );
    let (status, _h, _b) = call(&state, save2).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_s, _h, month2) = call(&state, get_as("/?y=2099&m=6", "u_alice", "a@x.co")).await;
    assert!(month2.matches("Weekly sync (renamed)").count() >= 4, "rename hits the whole series");

    // Deleting the series removes ALL occurrences.
    let del = post_form(
        &format!("/delete/{id}"),
        &cookie2,
        "u_alice",
        "a@x.co",
        &[("csrf_token", &csrf2)],
    );
    let (status, _h, _b) = call(&state, del).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_s, _h, gone) = call(&state, get_as("/?y=2099&m=6", "u_alice", "a@x.co")).await;
    assert!(!gone.contains("Weekly sync"), "deleting the series clears every occurrence");
}

#[tokio::test]
async fn contacts_crud_scoped_to_owner() {
    let state = build_dev_state();

    // Empty address book.
    let (status, headers, body) = call(&state, get_as("/contacts", "u_alice", "alice@holdfast.local")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No contacts yet"));
    let cookie = set_cookie(&headers).unwrap();
    let csrf = cookie_value(&cookie).unwrap();

    // Add a contact.
    let add = post_form(
        "/contacts/new",
        &cookie,
        "u_alice",
        "alice@holdfast.local",
        &[
            ("csrf_token", &csrf),
            ("name", "Grace Hopper"),
            ("email", "grace@navy.mil"),
            ("phone", "+1-202-555-0143"),
            ("notes", "Compiler pioneer."),
        ],
    );
    let (status, _h, _b) = call(&state, add).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (_s, _h, list) = call(&state, get_as("/contacts", "u_alice", "alice@holdfast.local")).await;
    assert!(list.contains("Grace Hopper"));
    assert!(list.contains("grace@navy.mil"));
    assert!(list.contains("1 contact"));
    let id = find_between(&list, "/contacts/edit/", "\"").expect("contact id link");

    // Bob's address book is empty + he can't open alice's contact.
    let (_s, _h, bob) = call(&state, get_as("/contacts", "u_bob", "bob@holdfast.local")).await;
    assert!(!bob.contains("Grace Hopper"));
    let (status, _h, _b) = call(&state, get_as(&format!("/contacts/edit/{id}"), "u_bob", "bob@holdfast.local")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Edit then delete.
    let (_s, h2, _b) = call(&state, get_as(&format!("/contacts/edit/{id}"), "u_alice", "alice@holdfast.local")).await;
    let cookie2 = set_cookie(&h2).unwrap();
    let csrf2 = cookie_value(&cookie2).unwrap();
    let upd = post_form(
        &format!("/contacts/edit/{id}"),
        &cookie2,
        "u_alice",
        "alice@holdfast.local",
        &[("csrf_token", &csrf2), ("name", "Rear Admiral Grace Hopper"), ("email", "grace@navy.mil")],
    );
    let (status, _h, _b) = call(&state, upd).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_s, _h, list) = call(&state, get_as("/contacts", "u_alice", "alice@holdfast.local")).await;
    assert!(list.contains("Rear Admiral Grace Hopper"));

    let del = post_form(
        &format!("/contacts/delete/{id}"),
        &cookie2,
        "u_alice",
        "alice@holdfast.local",
        &[("csrf_token", &csrf2)],
    );
    let (status, _h, _b) = call(&state, del).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_s, _h, list) = call(&state, get_as("/contacts", "u_alice", "alice@holdfast.local")).await;
    assert!(list.contains("No contacts yet"), "contact deleted");
}

#[tokio::test]
async fn csrf_required_on_post() {
    let state = build_dev_state();
    // POST with a token but NO cookie -> double-submit fails -> 403.
    let req = post_form(
        "/new",
        "", // no cookie
        "u_alice",
        "alice@holdfast.local",
        &[
            ("csrf_token", "anything"),
            ("title", "X"),
            ("starts_at", "2099-01-01T10:00"),
        ],
    );
    let (status, _h, body) = call(&state, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("CSRF"));
    // Nothing was written.
    let (_s, _h, idx) = call(&state, get_as("/?y=2099&m=1", "u_alice", "alice@holdfast.local")).await;
    assert!(idx.contains("No upcoming events"));
}

#[tokio::test]
async fn stored_text_is_escaped() {
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
            ("title", "<script>alert(1)</script>"),
            ("starts_at", "2099-05-05T10:00"),
            ("ends_at", "2099-05-05T11:00"),
        ],
    );
    let (status, _h, _b) = call(&state, save).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_s, _h, month) = call(&state, get_as("/?y=2099&m=5", "u_alice", "a@x.co")).await;
    assert!(!month.contains("<script>alert(1)</script>"), "title must be escaped");
    assert!(month.contains("&lt;script&gt;"));
}

#[tokio::test]
async fn settings_timezone_shifts_rendering_and_persists() {
    let state = build_dev_state();

    // Default settings page renders UTC + Sunday-first selected.
    let (status, headers, body) = call(&state, get_as("/settings", "u_alice", "a@x.co")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Settings"));
    assert!(body.contains("value=\"UTC\" selected"), "UTC is the default timezone");
    assert!(body.contains("value=\"sunday\" selected"), "Sunday-first is the default");
    let cookie = set_cookie(&headers).unwrap();
    let csrf = cookie_value(&cookie).unwrap();

    // A POST with no CSRF cookie is rejected (nothing saved).
    let forged = post_form(
        "/settings",
        "",
        "u_alice",
        "a@x.co",
        &[("csrf_token", "x"), ("timezone", "UTC+08:00"), ("week_start", "monday")],
    );
    let (status, _h, _b) = call(&state, forged).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Save UTC+08:00 / Monday-first.
    let save = post_form(
        "/settings",
        &cookie,
        "u_alice",
        "a@x.co",
        &[("csrf_token", &csrf), ("timezone", "UTC+08:00"), ("week_start", "monday")],
    );
    let (status, _h, _b) = call(&state, save).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // The form now reflects the saved choice.
    let (_s, _h, body) = call(&state, get_as("/settings", "u_alice", "a@x.co")).await;
    assert!(body.contains("value=\"UTC+08:00\" selected"));
    assert!(body.contains("value=\"monday\" selected"));

    // Create an event: 10:00 typed under UTC+8 stores 02:00 UTC and renders back as 10:00 local.
    let (_s, ch, _b) = call(&state, get_as("/new", "u_alice", "a@x.co")).await;
    let ccookie = set_cookie(&ch).unwrap();
    let ccsrf = cookie_value(&ccookie).unwrap();
    let create = post_form(
        "/new",
        &ccookie,
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", &ccsrf),
            ("title", "Tokyo sync"),
            ("starts_at", "2099-06-15T10:00"),
            ("ends_at", "2099-06-15T11:00"),
        ],
    );
    let (status, _h, _b) = call(&state, create).await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let (_s, _h, month) = call(&state, get_as("/?y=2099&m=6", "u_alice", "a@x.co")).await;
    assert!(month.contains("Tokyo sync"));
    assert!(month.contains("10:00 Tokyo sync"), "chip time shown in the owner's timezone");
    assert!(month.contains("times shown in UTC+08:00"), "footer names the timezone");
    // Monday-first grid puts Monday in the first column.
    let mon_idx = month.find(">Mon<").unwrap();
    let sun_idx = month.find(">Sun<").unwrap();
    assert!(mon_idx < sun_idx, "Monday column precedes Sunday");

    // The edit form pre-fills the LOCAL wall-clock value.
    let id = find_between(&month, "/edit/", "\"").expect("event id");
    let (_s, _h, edit) = call(&state, get_as(&format!("/edit/{id}"), "u_alice", "a@x.co")).await;
    assert!(edit.contains("value=\"2099-06-15T10:00\""), "edit shows local start time");

    // Bob (still on UTC defaults) sees the same instant as 02:00, Sunday-first.
    let (_s, _h, bob) = call(&state, get_as("/?y=2099&m=6", "u_bob", "b@x.co")).await;
    assert!(bob.contains("times shown in UTC"));
    let bob_mon = bob.find(">Mon<").unwrap();
    let bob_sun = bob.find(">Sun<").unwrap();
    assert!(bob_sun < bob_mon, "bob keeps the Sunday-first default");
}

#[tokio::test]
async fn unknown_route_renders_404() {
    let state = build_dev_state();
    let (status, _h, body) = call(&state, get("/no/such/path")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("Not found"));
}

// --- helpers ---------------------------------------------------------------------------

async fn call(state: &AppState, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
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

fn post_form(
    uri: &str,
    cookie: &str,
    subject: &str,
    email: &str,
    pairs: &[(&str, &str)],
) -> Request<Body> {
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

/// Extract the `name=value` (first segment) from a Set-Cookie header.
fn cookie_value(set_cookie: &str) -> Option<String> {
    let first = set_cookie.split(';').next()?;
    let (_name, value) = first.split_once('=')?;
    Some(value.to_string())
}

/// First substring strictly between `start` and the next `end`.
fn find_between(s: &str, start: &str, end: &str) -> Option<String> {
    let i = s.find(start)? + start.len();
    let rest = &s[i..];
    let j = rest.find(end)?;
    Some(rest[..j].to_string())
}
