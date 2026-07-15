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

use almanac::calendar;
use almanac::store::default_calendar_id;
use almanac::{app, build_dev_state, now_ms, AppState};

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
    let (status, _h, body) = call(&state, get_as("/", "u_alice", "alice@steadholme.local")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Calendar"));
    assert!(body.contains("No upcoming events"));

    // Open the new-event form; capture the minted CSRF token (cookie == hidden field).
    let (status, headers, body) =
        call(&state, get_as("/new", "u_alice", "alice@steadholme.local")).await;
    assert_eq!(status, StatusCode::OK);
    let cookie = set_cookie(&headers).expect("form sets a CSRF cookie");
    let csrf = cookie_value(&cookie).expect("csrf cookie value");
    assert!(
        body.contains(&format!("value=\"{csrf}\"")),
        "hidden field matches cookie"
    );

    // Create a far-future event so it is always "upcoming" in the agenda.
    let save = post_form(
        "/new",
        &cookie,
        "u_alice",
        "alice@steadholme.local",
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
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "create redirects (POST->GET)"
    );

    // The event's month shows it in the grid AND the agenda.
    let (status, _h, month) = call(
        &state,
        get_as("/?y=2099&m=6", "u_alice", "alice@steadholme.local"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(month.contains("June 2099"));
    assert!(month.contains("Quarterly review"));
    assert!(month.contains("War room"), "location shown in agenda");
    let id = find_between(&month, "/edit/", "\"").expect("an event id link");

    // Bob sees none of alice's events and cannot open her event.
    let (_s, _h, bob_view) = call(
        &state,
        get_as("/?y=2099&m=6", "u_bob", "bob@steadholme.local"),
    )
    .await;
    assert!(
        !bob_view.contains("Quarterly review"),
        "events are owner-scoped"
    );
    let (status, _h, _b) = call(
        &state,
        get_as(&format!("/edit/{id}"), "u_bob", "bob@steadholme.local"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "bob cannot open alice's event"
    );

    // Alice edits the event (new CSRF from the edit form).
    let (_s, h2, _b) = call(
        &state,
        get_as(&format!("/edit/{id}"), "u_alice", "alice@steadholme.local"),
    )
    .await;
    let cookie2 = set_cookie(&h2).unwrap();
    let csrf2 = cookie_value(&cookie2).unwrap();
    let save2 = post_form(
        &format!("/edit/{id}"),
        &cookie2,
        "u_alice",
        "alice@steadholme.local",
        &[
            ("csrf_token", &csrf2),
            ("title", "Quarterly review (rescheduled)"),
            ("starts_at", "2099-06-16T14:00"),
            ("ends_at", "2099-06-16T15:00"),
        ],
    );
    let (status, _h, _b) = call(&state, save2).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_s, _h, month) = call(
        &state,
        get_as("/?y=2099&m=6", "u_alice", "alice@steadholme.local"),
    )
    .await;
    assert!(month.contains("Quarterly review (rescheduled)"));

    // Bob cannot delete alice's event (it survives); alice can.
    let bad_del = post_form(
        &format!("/delete/{id}"),
        &cookie2,
        "u_bob",
        "bob@steadholme.local",
        &[("csrf_token", &csrf2)],
    );
    let (status, _h, _b) = call(&state, bad_del).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "delete always redirects");
    let (_s, _h, still) = call(
        &state,
        get_as("/?y=2099&m=6", "u_alice", "alice@steadholme.local"),
    )
    .await;
    assert!(
        still.contains("Quarterly review"),
        "bob's delete affected nothing"
    );

    let del = post_form(
        &format!("/delete/{id}"),
        &cookie2,
        "u_alice",
        "alice@steadholme.local",
        &[("csrf_token", &csrf2)],
    );
    let (status, _h, _b) = call(&state, del).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_s, _h, gone) = call(
        &state,
        get_as("/?y=2099&m=6", "u_alice", "alice@steadholme.local"),
    )
    .await;
    assert!(!gone.contains("Quarterly review"), "event deleted");
}

#[tokio::test]
async fn search_by_title() {
    let state = build_dev_state();
    let (start, end) = future_slot(30, 10, 11);
    create_event_as(
        &state,
        "u_alice",
        "alice@steadholme.local",
        &[
            ("title", "Dentist appointment"),
            ("starts_at", &start),
            ("ends_at", &end),
        ],
    )
    .await;

    let (status, _h, body) = call(
        &state,
        get_as("/search?q=dentist", "u_alice", "alice@steadholme.local"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Dentist appointment"));
    assert!(body.contains("href=\"/edit/"), "result links to edit");
}

#[tokio::test]
async fn search_matches_location_and_notes() {
    let state = build_dev_state();
    let (start, end) = future_slot(31, 10, 11);
    create_event_as(
        &state,
        "u_alice",
        "a@x.co",
        &[
            ("title", "Launch prep"),
            ("starts_at", &start),
            ("ends_at", &end),
            ("location", "War room"),
            ("notes", "Bring the deck"),
        ],
    )
    .await;

    let (_s, _h, by_location) =
        call(&state, get_as("/search?q=war%20room", "u_alice", "a@x.co")).await;
    assert!(by_location.contains("Launch prep"));
    let (_s, _h, by_notes) = call(&state, get_as("/search?q=deck", "u_alice", "a@x.co")).await;
    assert!(by_notes.contains("Launch prep"));
}

#[tokio::test]
async fn search_case_insensitive() {
    let state = build_dev_state();
    let (start, end) = future_slot(32, 10, 11);
    create_event_as(
        &state,
        "u_alice",
        "a@x.co",
        &[
            ("title", "Dentist"),
            ("starts_at", &start),
            ("ends_at", &end),
        ],
    )
    .await;

    let (_s, _h, body) = call(&state, get_as("/search?q=DENTIST", "u_alice", "a@x.co")).await;
    assert!(body.contains("Dentist"));
}

#[tokio::test]
async fn search_is_owner_scoped() {
    let state = build_dev_state();
    let (start, end) = future_slot(33, 10, 11);
    create_event_as(
        &state,
        "u_alice",
        "alice@steadholme.local",
        &[
            ("title", "Alice secret sync"),
            ("starts_at", &start),
            ("ends_at", &end),
        ],
    )
    .await;

    let (status, _h, body) = call(
        &state,
        get_as("/search?q=secret", "u_bob", "bob@steadholme.local"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("Alice secret sync"));
}

#[tokio::test]
async fn search_recurring_occurrence() {
    let state = build_dev_state();
    create_event_as(
        &state,
        "u_alice",
        "a@x.co",
        &[
            ("title", "Weekly planning"),
            ("starts_at", "2099-06-03T09:00"),
            ("ends_at", "2099-06-03T09:30"),
            ("repeat", "weekly"),
            ("repeat_interval", "1"),
            ("repeat_count", "3"),
        ],
    )
    .await;

    let (_s, _h, body) = call(
        &state,
        get_as(
            "/search?q=weekly%20planning&from=2099-06-10&to=2099-06-17",
            "u_alice",
            "a@x.co",
        ),
    )
    .await;
    assert!(
        body.matches("Weekly planning").count() >= 2,
        "search expands recurring occurrences"
    );
}

#[tokio::test]
async fn search_date_range() {
    let state = build_dev_state();
    create_event_as(
        &state,
        "u_alice",
        "a@x.co",
        &[
            ("title", "Early dentist"),
            ("starts_at", "2099-01-05T10:00"),
            ("ends_at", "2099-01-05T11:00"),
        ],
    )
    .await;
    create_event_as(
        &state,
        "u_alice",
        "a@x.co",
        &[
            ("title", "Late dentist"),
            ("starts_at", "2099-03-05T10:00"),
            ("ends_at", "2099-03-05T11:00"),
        ],
    )
    .await;

    let (_s, _h, body) = call(
        &state,
        get_as(
            "/search?q=dentist&from=2099-03-01&to=2099-03-31",
            "u_alice",
            "a@x.co",
        ),
    )
    .await;
    assert!(body.contains("Late dentist"));
    assert!(!body.contains("Early dentist"));
}

#[tokio::test]
async fn search_query_is_escaped() {
    let state = build_dev_state();
    let (status, _h, body) = call(
        &state,
        get_as("/search?q=%3Cscript%3E", "u_alice", "a@x.co"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("<script>"));
    assert!(body.contains("&lt;script&gt;"));
}

#[tokio::test]
async fn search_empty_query() {
    let state = build_dev_state();
    let (start, end) = future_slot(34, 10, 11);
    create_event_as(
        &state,
        "u_alice",
        "a@x.co",
        &[
            ("title", "Invisible empty query"),
            ("starts_at", &start),
            ("ends_at", &end),
        ],
    )
    .await;

    let (status, _h, body) = call(&state, get_as("/search", "u_alice", "a@x.co")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Type a word to search your events."));
    assert!(!body.contains("Invisible empty query"));
    assert!(!body.contains("<div class=\"agenda__item\""));
}

#[tokio::test]
async fn search_delete_from_results() {
    let state = build_dev_state();
    let (start, end) = future_slot(35, 10, 11);
    create_event_as(
        &state,
        "u_alice",
        "a@x.co",
        &[
            ("title", "Deletable dentist"),
            ("starts_at", &start),
            ("ends_at", &end),
        ],
    )
    .await;

    let (status, headers, body) =
        call(&state, get_as("/search?q=deletable", "u_alice", "a@x.co")).await;
    assert_eq!(status, StatusCode::OK);
    let cookie = set_cookie(&headers).expect("search sets a CSRF cookie");
    assert!(cookie.starts_with("__Host-almanac_csrf="));
    let csrf = cookie_value(&cookie).expect("csrf cookie value");
    assert!(
        body.contains(&format!("value=\"{csrf}\"")),
        "search result delete form uses the CSRF cookie token"
    );
    let id = find_between(&body, "/edit/", "\"").expect("event id link");

    let del = post_form(
        &format!("/delete/{id}"),
        &cookie,
        "u_alice",
        "a@x.co",
        &[("csrf_token", &csrf)],
    );
    assert_eq!(call(&state, del).await.0, StatusCode::SEE_OTHER);

    let (_s, _h, gone) = call(&state, get_as("/search?q=deletable", "u_alice", "a@x.co")).await;
    assert!(!gone.contains("Deletable dentist"));
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
    assert!(
        month.contains("cal-event--allday"),
        "rendered as an all-day chip"
    );
    assert!(month.contains("All day"), "agenda labels it all-day");
}

#[tokio::test]
async fn multiple_calendars_filter_and_block_nonempty_delete() {
    let state = build_dev_state();
    let (_s, headers, body) = call(&state, get_as("/", "u_alice", "a@x.co")).await;
    assert!(
        body.contains("Default"),
        "default calendar is created lazily"
    );
    let cookie = set_cookie(&headers).unwrap();
    let csrf = cookie_value(&cookie).unwrap();

    let create_cal = post_form(
        "/calendars/new",
        &cookie,
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", &csrf),
            ("name", "Work"),
            ("color", "#22c55e"),
        ],
    );
    assert_eq!(call(&state, create_cal).await.0, StatusCode::SEE_OTHER);
    let (_s, headers, home) = call(&state, get_as("/", "u_alice", "a@x.co")).await;
    assert!(home.contains("Work"));
    let work_id = find_calendar_update_id_by_name(&home, "Work").expect("calendar update id");

    let cookie2 = set_cookie(&headers).unwrap();
    let csrf2 = cookie_value(&cookie2).unwrap();
    let save = post_form(
        "/new",
        &cookie2,
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", &csrf2),
            ("calendar_id", &work_id),
            ("title", "Work planning"),
            ("starts_at", "2099-08-05T10:00"),
            ("ends_at", "2099-08-05T11:00"),
        ],
    );
    assert_eq!(call(&state, save).await.0, StatusCode::SEE_OTHER);
    let (_s, _h, month) = call(&state, get_as("/?y=2099&m=8", "u_alice", "a@x.co")).await;
    assert!(month.contains("Work planning"));
    assert!(
        month.contains("--cal-color:#22c55e"),
        "event renders in calendar color"
    );
    let event_id = find_between(&month, "/edit/", "\"").expect("event id");

    let default_id = default_calendar_id("u_alice");
    let (_s, _h, default_only) = call(
        &state,
        get_as(
            &format!("/?y=2099&m=8&cal={default_id}"),
            "u_alice",
            "a@x.co",
        ),
    )
    .await;
    assert!(
        !default_only.contains("Work planning"),
        "calendar filter hides other calendars"
    );
    let (_s, _h, work_only) = call(
        &state,
        get_as(&format!("/?y=2099&m=8&cal={work_id}"), "u_alice", "a@x.co"),
    )
    .await;
    assert!(
        work_only.contains("Work planning"),
        "calendar filter shows selected calendar"
    );

    let blocked = post_form(
        &format!("/calendars/delete/{work_id}"),
        &cookie2,
        "u_alice",
        "a@x.co",
        &[("csrf_token", &csrf2)],
    );
    assert_eq!(call(&state, blocked).await.0, StatusCode::SEE_OTHER);
    let (_s, _h, still) = call(&state, get_as("/", "u_alice", "a@x.co")).await;
    assert!(
        still.contains("Work"),
        "non-empty calendar delete is blocked"
    );

    let del_event = post_form(
        &format!("/delete/{event_id}"),
        &cookie2,
        "u_alice",
        "a@x.co",
        &[("csrf_token", &csrf2)],
    );
    assert_eq!(call(&state, del_event).await.0, StatusCode::SEE_OTHER);
    let del_cal = post_form(
        &format!("/calendars/delete/{work_id}"),
        &cookie2,
        "u_alice",
        "a@x.co",
        &[("csrf_token", &csrf2)],
    );
    assert_eq!(call(&state, del_cal).await.0, StatusCode::SEE_OTHER);
    let (_s, _h, gone) = call(&state, get_as("/", "u_alice", "a@x.co")).await;
    assert!(
        !gone.contains("Work"),
        "empty non-default calendar can be deleted"
    );
}

#[tokio::test]
async fn agenda_view_groups_next_days() {
    let state = build_dev_state();
    let (_s, headers, _b) = call(&state, get_as("/new", "u_alice", "a@x.co")).await;
    let cookie = set_cookie(&headers).unwrap();
    let csrf = cookie_value(&cookie).unwrap();
    let tomorrow = calendar::start_of_day(almanac::now_ms()) + 86_400_000;
    let starts = calendar::fmt_datetime_local(tomorrow + 9 * 3_600_000);
    let ends = calendar::fmt_datetime_local(tomorrow + 10 * 3_600_000);
    let save = post_form(
        "/new",
        &cookie,
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", &csrf),
            ("title", "Agenda review"),
            ("starts_at", &starts),
            ("ends_at", &ends),
        ],
    );
    assert_eq!(call(&state, save).await.0, StatusCode::SEE_OTHER);
    let (status, _h, agenda) = call(&state, get_as("/?view=agenda", "u_alice", "a@x.co")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(agenda.contains("<h1>Agenda</h1>"));
    assert!(agenda.contains("agenda__day"), "events are grouped by day");
    assert!(agenda.contains("Agenda review"));
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
    assert!(
        occurrences >= 4,
        "each weekly occurrence renders (got {occurrences})"
    );
    assert!(
        month.contains("↻"),
        "recurring occurrences carry a repeat marker"
    );

    // The edit form pre-fills the recurrence controls from the stored RRULE.
    let id = find_between(&month, "/edit/", "\"").expect("event id");
    let (_s, _h, edit) = call(&state, get_as(&format!("/edit/{id}"), "u_alice", "a@x.co")).await;
    assert!(
        edit.contains("value=\"weekly\" selected"),
        "frequency pre-selected"
    );
    assert!(
        edit.contains("name=\"repeat_count\" min=\"1\" max=\"9999\" value=\"4\""),
        "count pre-filled"
    );

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
    assert!(
        month2.matches("Weekly sync (renamed)").count() >= 4,
        "rename hits the whole series"
    );

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
    assert!(
        !gone.contains("Weekly sync"),
        "deleting the series clears every occurrence"
    );
}

#[tokio::test]
async fn recurring_single_occurrence_delete_and_edit() {
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
            ("title", "Weekly sync"),
            ("starts_at", "2099-06-03T09:00"),
            ("ends_at", "2099-06-03T09:30"),
            ("repeat", "weekly"),
            ("repeat_interval", "1"),
            ("repeat_count", "3"),
        ],
    );
    assert_eq!(call(&state, save).await.0, StatusCode::SEE_OTHER);
    let (_s, h2, month) = call(&state, get_as("/?y=2099&m=6", "u_alice", "a@x.co")).await;
    assert!(month.matches("Weekly sync").count() >= 3);
    let id = find_between(&month, "/edit/", "\"").expect("series id");
    let cookie2 = set_cookie(&h2).unwrap();
    let csrf2 = cookie_value(&cookie2).unwrap();

    let occ2 = calendar::parse_datetime_local("2099-06-10T09:00").unwrap();
    let del_one = post_form(
        &format!("/delete/{id}?occurrence={occ2}"),
        &cookie2,
        "u_alice",
        "a@x.co",
        &[("csrf_token", &csrf2)],
    );
    assert_eq!(call(&state, del_one).await.0, StatusCode::SEE_OTHER);
    let (_s, _h, after_delete) = call(&state, get_as("/?y=2099&m=6", "u_alice", "a@x.co")).await;
    assert!(
        after_delete.matches("Weekly sync").count() >= 2,
        "the other occurrences remain"
    );

    let occ3 = calendar::parse_datetime_local("2099-06-17T09:00").unwrap();
    let (_s, h3, edit_occ) = call(
        &state,
        get_as(
            &format!("/edit/{id}?occurrence={occ3}"),
            "u_alice",
            "a@x.co",
        ),
    )
    .await;
    assert!(
        edit_occ.contains("Edit series"),
        "occurrence form links back to whole-series edit"
    );
    let cookie3 = set_cookie(&h3).unwrap();
    let csrf3 = cookie_value(&cookie3).unwrap();
    let save_occ = post_form(
        &format!("/edit/{id}?occurrence={occ3}"),
        &cookie3,
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", &csrf3),
            ("title", "Special sync"),
            ("starts_at", "2099-06-18T12:00"),
            ("ends_at", "2099-06-18T12:30"),
        ],
    );
    assert_eq!(call(&state, save_occ).await.0, StatusCode::SEE_OTHER);
    let (_s, _h, after_edit) = call(&state, get_as("/?y=2099&m=6", "u_alice", "a@x.co")).await;
    assert!(
        after_edit.contains("Special sync"),
        "override occurrence renders"
    );
    assert!(
        after_edit.contains("Weekly sync"),
        "whole series remains editable"
    );

    let (_s, _h, series_edit) =
        call(&state, get_as(&format!("/edit/{id}"), "u_alice", "a@x.co")).await;
    assert!(
        series_edit.contains("value=\"weekly\" selected"),
        "whole-series edit still works"
    );
}

#[tokio::test]
async fn contacts_crud_scoped_to_owner() {
    let state = build_dev_state();

    // Empty address book.
    let (status, headers, body) = call(
        &state,
        get_as("/contacts", "u_alice", "alice@steadholme.local"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No contacts yet"));
    let cookie = set_cookie(&headers).unwrap();
    let csrf = cookie_value(&cookie).unwrap();

    // Add a contact.
    let add = post_form(
        "/contacts/new",
        &cookie,
        "u_alice",
        "alice@steadholme.local",
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

    let (_s, _h, list) = call(
        &state,
        get_as("/contacts", "u_alice", "alice@steadholme.local"),
    )
    .await;
    assert!(list.contains("Grace Hopper"));
    assert!(list.contains("grace@navy.mil"));
    assert!(list.contains("1 contact"));
    let id = find_between(&list, "/contacts/edit/", "\"").expect("contact id link");

    // Bob's address book is empty + he can't open alice's contact.
    let (_s, _h, bob) = call(&state, get_as("/contacts", "u_bob", "bob@steadholme.local")).await;
    assert!(!bob.contains("Grace Hopper"));
    let (status, _h, _b) = call(
        &state,
        get_as(
            &format!("/contacts/edit/{id}"),
            "u_bob",
            "bob@steadholme.local",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Edit then delete.
    let (_s, h2, _b) = call(
        &state,
        get_as(
            &format!("/contacts/edit/{id}"),
            "u_alice",
            "alice@steadholme.local",
        ),
    )
    .await;
    let cookie2 = set_cookie(&h2).unwrap();
    let csrf2 = cookie_value(&cookie2).unwrap();
    let upd = post_form(
        &format!("/contacts/edit/{id}"),
        &cookie2,
        "u_alice",
        "alice@steadholme.local",
        &[
            ("csrf_token", &csrf2),
            ("name", "Rear Admiral Grace Hopper"),
            ("email", "grace@navy.mil"),
        ],
    );
    let (status, _h, _b) = call(&state, upd).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_s, _h, list) = call(
        &state,
        get_as("/contacts", "u_alice", "alice@steadholme.local"),
    )
    .await;
    assert!(list.contains("Rear Admiral Grace Hopper"));

    let del = post_form(
        &format!("/contacts/delete/{id}"),
        &cookie2,
        "u_alice",
        "alice@steadholme.local",
        &[("csrf_token", &csrf2)],
    );
    let (status, _h, _b) = call(&state, del).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (_s, _h, list) = call(
        &state,
        get_as("/contacts", "u_alice", "alice@steadholme.local"),
    )
    .await;
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
        "alice@steadholme.local",
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
    let (_s, _h, idx) = call(
        &state,
        get_as("/?y=2099&m=1", "u_alice", "alice@steadholme.local"),
    )
    .await;
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
    assert!(
        !month.contains("<script>alert(1)</script>"),
        "title must be escaped"
    );
    assert!(month.contains("&lt;script&gt;"));
}

#[tokio::test]
async fn settings_timezone_shifts_rendering_and_persists() {
    let state = build_dev_state();

    // Default settings page renders UTC + Sunday-first selected.
    let (status, headers, body) = call(&state, get_as("/settings", "u_alice", "a@x.co")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Settings"));
    assert!(
        body.contains("value=\"UTC\" selected"),
        "UTC is the default timezone"
    );
    assert!(
        body.contains("value=\"sunday\" selected"),
        "Sunday-first is the default"
    );
    let cookie = set_cookie(&headers).unwrap();
    let csrf = cookie_value(&cookie).unwrap();

    // A POST with no CSRF cookie is rejected (nothing saved).
    let forged = post_form(
        "/settings",
        "",
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", "x"),
            ("timezone", "UTC+08:00"),
            ("week_start", "monday"),
        ],
    );
    let (status, _h, _b) = call(&state, forged).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Save UTC+08:00 / Monday-first.
    let save = post_form(
        "/settings",
        &cookie,
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", &csrf),
            ("timezone", "UTC+08:00"),
            ("week_start", "monday"),
        ],
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
    assert!(
        month.contains("10:00 Tokyo sync"),
        "chip time shown in the owner's timezone"
    );
    assert!(
        month.contains("times shown in UTC+08:00"),
        "footer names the timezone"
    );
    // Monday-first grid puts Monday in the first column.
    let mon_idx = month.find(">Mon<").unwrap();
    let sun_idx = month.find(">Sun<").unwrap();
    assert!(mon_idx < sun_idx, "Monday column precedes Sunday");

    // The edit form pre-fills the LOCAL wall-clock value.
    let id = find_between(&month, "/edit/", "\"").expect("event id");
    let (_s, _h, edit) = call(&state, get_as(&format!("/edit/{id}"), "u_alice", "a@x.co")).await;
    assert!(
        edit.contains("value=\"2099-06-15T10:00\""),
        "edit shows local start time"
    );

    // Bob (still on UTC defaults) sees the same instant as 02:00, Sunday-first.
    let (_s, _h, bob) = call(&state, get_as("/?y=2099&m=6", "u_bob", "b@x.co")).await;
    assert!(bob.contains("times shown in UTC"));
    let bob_mon = bob.find(">Mon<").unwrap();
    let bob_sun = bob.find(">Sun<").unwrap();
    assert!(bob_sun < bob_mon, "bob keeps the Sunday-first default");
}

#[tokio::test]
async fn week_and_day_views_render_events_on_the_time_grid() {
    let state = build_dev_state();
    let (_s, headers, _b) = call(&state, get_as("/new", "u_alice", "a@x.co")).await;
    let cookie = set_cookie(&headers).unwrap();
    let csrf = cookie_value(&cookie).unwrap();

    // A timed event on Mon 2099-06-15 10:00–11:00, plus a daily recurring standup that week.
    let save = post_form(
        "/new",
        &cookie,
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", &csrf),
            ("title", "Design review"),
            ("starts_at", "2099-06-15T10:00"),
            ("ends_at", "2099-06-15T11:00"),
        ],
    );
    assert_eq!(call(&state, save).await.0, StatusCode::SEE_OTHER);
    let stand = post_form(
        "/new",
        &cookie,
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", &csrf),
            ("title", "Standup"),
            ("starts_at", "2099-06-15T09:00"),
            ("ends_at", "2099-06-15T09:15"),
            ("repeat", "daily"),
            ("repeat_interval", "1"),
            ("repeat_count", "5"),
        ],
    );
    assert_eq!(call(&state, stand).await.0, StatusCode::SEE_OTHER);

    // Week view anchored inside that week: the time-grid shows the one-off AND every daily
    // occurrence, plus the hour gutter and the Month/Week/Day switch.
    let (status, _h, week) = call(
        &state,
        get_as("/?view=week&date=2099-06-17", "u_alice", "a@x.co"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(week.contains("tgrid__body"), "renders the time-grid");
    assert!(week.contains("09:00"), "hour gutter present");
    assert!(week.contains("tgrid__event"), "timed events are placed");
    assert!(week.contains("Design review"));
    // Mon–Fri daily standup => 5 occurrences visible in the week.
    assert!(
        week.matches("Standup").count() >= 5,
        "recurring series expands in the week grid"
    );
    assert!(
        week.contains("href=\"/?view=day&amp;date=2099-06-15\""),
        "day headers link to the day view"
    );
    // Sunday-first week containing Jun 17 runs Jun 14 (Sun)..Jun 20 (Sat); nav steps a whole week.
    assert!(
        week.contains("href=\"/?view=week&amp;date=2099-06-07\""),
        "prev week"
    );
    assert!(
        week.contains("href=\"/?view=week&amp;date=2099-06-21\""),
        "next week"
    );

    // Day view for Mon Jun 15: the one-off + that day's standup occurrence, stepping day-by-day.
    let (status, _h, day) = call(
        &state,
        get_as("/?view=day&date=2099-06-15", "u_alice", "a@x.co"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(day.contains("Jun 15, 2099"));
    assert!(day.contains("Design review"));
    assert!(
        day.contains("Standup"),
        "the day's recurring occurrence shows"
    );
    assert!(
        day.contains("href=\"/?view=day&amp;date=2099-06-14\""),
        "prev day"
    );
    assert!(
        day.contains("href=\"/?view=day&amp;date=2099-06-16\""),
        "next day"
    );

    // Sat Jun 20 is outside the 5-occurrence (Mon–Fri) run and holds no one-off => empty grid.
    let (_s, _h, sat) = call(
        &state,
        get_as("/?view=day&date=2099-06-20", "u_alice", "a@x.co"),
    )
    .await;
    assert!(
        !sat.contains("Standup"),
        "day view is scoped to its own day"
    );
    assert!(!sat.contains("Design review"));
}

#[tokio::test]
async fn week_view_escapes_event_titles() {
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
            ("title", "<script>alert(2)</script>"),
            ("starts_at", "2099-07-06T10:00"),
            ("ends_at", "2099-07-06T11:00"),
        ],
    );
    assert_eq!(call(&state, save).await.0, StatusCode::SEE_OTHER);
    let (_s, _h, week) = call(
        &state,
        get_as("/?view=week&date=2099-07-06", "u_alice", "a@x.co"),
    )
    .await;
    assert!(
        !week.contains("<script>alert(2)</script>"),
        "title must be escaped in the grid"
    );
    assert!(week.contains("&lt;script&gt;"));
}

#[tokio::test]
async fn unknown_route_renders_404() {
    let state = build_dev_state();
    let (status, _h, body) = call(&state, get("/no/such/path")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("Not found"));
}

#[tokio::test]
async fn quick_add_json_creates_event_optimistically() {
    let state = build_dev_state();
    // GET the calendar to mint the CSRF cookie the quick-add box carries.
    let (_s, headers, _b) = call(&state, get_as("/", "u_alice", "a@x.co")).await;
    let cookie = set_cookie(&headers).unwrap();
    let csrf = cookie_value(&cookie).unwrap();

    // Bad CSRF -> 403 JSON, nothing written.
    let bad = json_post(
        "/quick-add.json",
        &cookie,
        "u_alice",
        "a@x.co",
        r#"{"text":"Dentist tomorrow 12pm","csrf_token":"wrong"}"#.to_string(),
    );
    assert_eq!(call(&state, bad).await.0, StatusCode::FORBIDDEN);

    // Valid parse -> 200 {ok:true} with the echoed title.
    let ok = json_post(
        "/quick-add.json",
        &cookie,
        "u_alice",
        "a@x.co",
        format!(r#"{{"text":"Dentist tomorrow 12pm","csrf_token":"{csrf}"}}"#),
    );
    let (status, _h, body) = call(&state, ok).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"ok\":true"), "created: {body}");
    assert!(body.contains("Dentist"), "title echoed in JSON: {body}");

    // The created event shows in the agenda (owner-scoped, no reload needed by the client).
    let (_s, _h, agenda) = call(&state, get_as("/?view=agenda", "u_alice", "a@x.co")).await;
    assert!(agenda.contains("Dentist"), "quick-added event is listed");

    // An unparseable phrase -> 200 {ok:false} (the client then falls back to the form editor).
    let vague = json_post(
        "/quick-add.json",
        &cookie,
        "u_alice",
        "a@x.co",
        format!(r#"{{"text":"just some notes","csrf_token":"{csrf}"}}"#),
    );
    let (status, _h, body) = call(&state, vague).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("\"ok\":false"),
        "unparseable is a soft no: {body}"
    );

    // Owner scoping: Bob never sees Alice's quick-added event.
    let (_s, _h, bob) = call(&state, get_as("/?view=agenda", "u_bob", "b@x.co")).await;
    assert!(!bob.contains("Dentist"), "quick-add is owner-scoped");
}

// --- helpers ---------------------------------------------------------------------------

/// POST a JSON body to `uri` as `subject`, carrying the double-submit CSRF `cookie`.
fn json_post(uri: &str, cookie: &str, subject: &str, email: &str, body: String) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-auth-subject", subject)
        .header("x-auth-email", email);
    if !cookie.is_empty() {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::from(body)).unwrap()
}

async fn call(state: &AppState, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
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

fn future_slot(days_from_now: i64, start_hour: i64, end_hour: i64) -> (String, String) {
    let day = calendar::start_of_day(now_ms() + days_from_now * 86_400_000);
    (
        calendar::fmt_datetime_local(day + start_hour * 3_600_000),
        calendar::fmt_datetime_local(day + end_hour * 3_600_000),
    )
}

async fn create_event_as(state: &AppState, subject: &str, email: &str, fields: &[(&str, &str)]) {
    let (_s, headers, _b) = call(state, get_as("/new", subject, email)).await;
    let cookie = set_cookie(&headers).expect("new event form sets a CSRF cookie");
    let csrf = cookie_value(&cookie).expect("csrf cookie value");
    let mut pairs = Vec::with_capacity(fields.len() + 1);
    pairs.push(("csrf_token", csrf.as_str()));
    pairs.extend_from_slice(fields);
    let req = post_form("/new", &cookie, subject, email, &pairs);
    assert_eq!(call(state, req).await.0, StatusCode::SEE_OTHER);
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

fn find_calendar_update_id_by_name(s: &str, name: &str) -> Option<String> {
    let marker = format!("value=\"{name}\"");
    let pos = s.find(&marker)?;
    let before = &s[..pos];
    let start_token = "/calendars/update/";
    let i = before.rfind(start_token)? + start_token.len();
    let rest = &before[i..];
    let j = rest.find('"')?;
    Some(rest[..j].to_string())
}
