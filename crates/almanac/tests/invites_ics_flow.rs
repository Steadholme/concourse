//! End-to-end flow for the Google-Calendar-parity additions: attendees + RSVP tokens, the ICS
//! subscription feed + per-event download, reminders wiring, and natural-language quick-add.
//!
//! Drives the real `Router` in-process via `tower::oneshot` against the in-memory store (NO
//! database), exactly like `calendar_flow.rs`.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

use almanac::auth;
use almanac::config::Config;
use almanac::store::PgStore;
use almanac::{app, build_dev_state, AppState};

#[tokio::test]
async fn attendee_rsvp_token_flow_is_safe_two_stage_and_private() {
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
    let (status, _h, detail) =
        call(&state, get_as(&format!("/event/{id}"), "u_alice", "a@x.co")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.contains("Grace Hopper"));
    assert!(detail.contains("grace@navy.mil"));
    assert!(
        detail.contains("No response"),
        "initial status is needs-action"
    );
    assert!(
        detail.contains("0 of 2 responses"),
        "complete attendee projection prints an exact response bound"
    );
    assert!(detail.contains("10 minutes before"));
    assert!(detail.contains("1 hour before"));
    // Grab one attendee's RSVP token from a /rsvp/{token} link.
    let token = find_between(&detail, "/rsvp/", "\"").expect("rsvp token");
    assert_eq!(token.len(), 64, "token is an unguessable 64-hex capability");

    // The RSVP page is PUBLIC — no gateway identity headers at all.
    let (status, rsvp_headers, page) = call(&state, get(&format!("/rsvp/{token}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_privacy(&rsvp_headers);
    let rsvp_cookie = set_cookie(&rsvp_headers).expect("RSVP GET mints public CSRF");
    let rsvp_csrf = cookie_value(&rsvp_cookie).unwrap();
    assert!(page.contains("gate--choice"));
    assert!(page.contains("Launch review"));
    assert!(
        !page.contains(&token),
        "capability never enters response copy"
    );

    // Legacy query input is read-only and cannot change the attendee.
    let (status, legacy_headers, legacy) =
        call(&state, get(&format!("/rsvp/{token}?reply=accepted"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_privacy(&legacy_headers);
    assert!(legacy.contains("No response"));
    let (_s, _h, unchanged) =
        call(&state, get_as(&format!("/event/{id}"), "u_alice", "a@x.co")).await;
    assert!(
        unchanged.contains("No response"),
        "GET performed zero writes"
    );

    // Stage one confirms consequence and still performs zero writes.
    let confirm = post_public_form(
        &format!("/rsvp/{token}"),
        &rsvp_cookie,
        &[
            ("csrf_token", &rsvp_csrf),
            ("intent", "confirm"),
            ("reply", "accepted"),
        ],
    );
    let (status, confirm_headers, confirmed) = call(&state, confirm).await;
    assert_eq!(status, StatusCode::OK);
    assert_privacy(&confirm_headers);
    assert!(confirmed.contains("Confirm: Accepted"));
    let accepted_proof = find_between(&confirmed, "name=\"confirmation_proof\" value=\"", "\"")
        .expect("Confirm mints a server proof");
    let (_s, _h, still_unchanged) =
        call(&state, get_as(&format!("/event/{id}"), "u_alice", "a@x.co")).await;
    assert!(still_unchanged.contains("No response"));

    // A direct Commit cannot skip the reviewed consequence.
    let direct = post_public_form(
        &format!("/rsvp/{token}"),
        &rsvp_cookie,
        &[
            ("csrf_token", &rsvp_csrf),
            ("intent", "commit"),
            ("reply", "accepted"),
        ],
    );
    let (status, direct_headers, _body) = call(&state, direct).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_privacy(&direct_headers);
    let (_s, _h, still_direct_unchanged) =
        call(&state, get_as(&format!("/event/{id}"), "u_alice", "a@x.co")).await;
    assert!(still_direct_unchanged.contains("No response"));

    let expired_proof = auth::mint_rsvp_confirmation(&token, "accepted", &rsvp_csrf, 0);
    let expired = post_public_form(
        &format!("/rsvp/{token}"),
        &rsvp_cookie,
        &[
            ("csrf_token", &rsvp_csrf),
            ("intent", "commit"),
            ("reply", "accepted"),
            ("confirmation_proof", &expired_proof),
        ],
    );
    let (status, expired_headers, _body) = call(&state, expired).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_privacy(&expired_headers);

    // Stage two commits with the proof and redirects privately back to the same capability GET.
    let commit = post_public_form(
        &format!("/rsvp/{token}"),
        &rsvp_cookie,
        &[
            ("csrf_token", &rsvp_csrf),
            ("intent", "commit"),
            ("reply", "accepted"),
            ("confirmation_proof", &accepted_proof),
        ],
    );
    let (status, commit_headers, committed) = call(&state, commit).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_privacy(&commit_headers);
    assert_eq!(
        commit_headers
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok()),
        Some(format!("/rsvp/{token}").as_str())
    );
    assert!(committed.is_empty());
    let (_s, accepted_headers, accepted_page) = call(&state, get(&format!("/rsvp/{token}"))).await;
    assert_privacy(&accepted_headers);
    assert!(accepted_page.contains("Accepted"));
    let (_s, _h, detail2) =
        call(&state, get_as(&format!("/event/{id}"), "u_alice", "a@x.co")).await;
    assert!(detail2.contains("Accepted"));
    assert!(detail2.contains("1 of 2 responses"));

    // Same reviewed reply is idempotent.
    let replay = post_public_form(
        &format!("/rsvp/{token}"),
        &rsvp_cookie,
        &[
            ("csrf_token", &rsvp_csrf),
            ("intent", "commit"),
            ("reply", "accepted"),
            ("confirmation_proof", &accepted_proof),
        ],
    );
    assert_eq!(call(&state, replay).await.0, StatusCode::SEE_OTHER);

    // A proof cannot be rebound to a different reply.
    let tampered_reply = post_public_form(
        &format!("/rsvp/{token}"),
        &rsvp_cookie,
        &[
            ("csrf_token", &rsvp_csrf),
            ("intent", "commit"),
            ("reply", "declined"),
            ("confirmation_proof", &accepted_proof),
        ],
    );
    let (status, tampered_headers, _body) = call(&state, tampered_reply).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_privacy(&tampered_headers);
    let tampered_capability = post_public_form(
        "/rsvp/deadbeef",
        &rsvp_cookie,
        &[
            ("csrf_token", &rsvp_csrf),
            ("intent", "commit"),
            ("reply", "accepted"),
            ("confirmation_proof", &accepted_proof),
        ],
    );
    let (status, tampered_headers, _body) = call(&state, tampered_capability).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_privacy(&tampered_headers);
    let (_s, _h, after_tamper) =
        call(&state, get_as(&format!("/event/{id}"), "u_alice", "a@x.co")).await;
    assert!(after_tamper.contains("Accepted"));

    // A later different response needs its own Confirm proof.
    let decline_confirm = post_public_form(
        &format!("/rsvp/{token}"),
        &rsvp_cookie,
        &[
            ("csrf_token", &rsvp_csrf),
            ("intent", "confirm"),
            ("reply", "declined"),
        ],
    );
    let (_s, _h, decline_confirmation) = call(&state, decline_confirm).await;
    let decline_proof = find_between(
        &decline_confirmation,
        "name=\"confirmation_proof\" value=\"",
        "\"",
    )
    .expect("Decline Confirm mints a bound proof");
    let decline = post_public_form(
        &format!("/rsvp/{token}"),
        &rsvp_cookie,
        &[
            ("csrf_token", &rsvp_csrf),
            ("intent", "commit"),
            ("reply", "declined"),
            ("confirmation_proof", &decline_proof),
        ],
    );
    let (status, decline_headers, declined) = call(&state, decline).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_privacy(&decline_headers);
    assert!(declined.is_empty());
    let (_s, _h, detail3) =
        call(&state, get_as(&format!("/event/{id}"), "u_alice", "a@x.co")).await;
    assert!(detail3.contains("Declined"));

    // Malformed form and CSRF are product-owned safe responses, never Axum's bare 422.
    let malformed = post_public_form(
        &format!("/rsvp/{token}"),
        &rsvp_cookie,
        &[("csrf_token", &rsvp_csrf), ("intent", "bogus")],
    );
    let (status, malformed_headers, _body) = call(&state, malformed).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_privacy(&malformed_headers);
    let bad_csrf = post_public_form(
        &format!("/rsvp/{token}"),
        &rsvp_cookie,
        &[
            ("csrf_token", "wrong"),
            ("intent", "commit"),
            ("reply", "accepted"),
        ],
    );
    let (status, csrf_headers, _body) = call(&state, bad_csrf).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_privacy(&csrf_headers);
    let unknown_bad_csrf = post_public_form(
        "/rsvp/deadbeef",
        &rsvp_cookie,
        &[
            ("csrf_token", "wrong"),
            ("intent", "commit"),
            ("reply", "accepted"),
        ],
    );
    let (status, unknown_csrf_headers, _body) = call(&state, unknown_bad_csrf).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "CSRF is checked before capability lookup"
    );
    assert_privacy(&unknown_csrf_headers);

    // Unknown GET/POST share byte-identical non-enumerating 404 copy.
    let (status, missing_get_headers, missing_get) = call(&state, get("/rsvp/deadbeef")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_privacy(&missing_get_headers);
    let missing_post = post_public_form(
        "/rsvp/deadbeef",
        &rsvp_cookie,
        &[
            ("csrf_token", &rsvp_csrf),
            ("intent", "confirm"),
            ("reply", "accepted"),
        ],
    );
    let (status, missing_post_headers, missing_post) = call(&state, missing_post).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_privacy(&missing_post_headers);
    assert_eq!(missing_get, missing_post);

    // An attendee whose event disappeared and a deleted attendee use the identical 404 body.
    state.store.delete_event("u_alice", &id).await.unwrap();
    let (status, event_missing_headers, event_missing) =
        call(&state, get(&format!("/rsvp/{token}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_privacy(&event_missing_headers);
    assert_eq!(event_missing, missing_get);
    state
        .store
        .delete_event_attendees("u_alice", &id)
        .await
        .unwrap();
    let (status, attendee_deleted_headers, attendee_deleted) =
        call(&state, get(&format!("/rsvp/{token}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_privacy(&attendee_deleted_headers);
    assert_eq!(attendee_deleted, missing_get);
}

#[tokio::test]
async fn rsvp_store_failure_is_private_generic_503() {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_millis(100))
        .connect_lazy("postgres://127.0.0.1:1/almanac")
        .unwrap();
    let state = AppState {
        config: Arc::new(Config::dev()),
        store: Arc::new(PgStore::from_pool(pool)),
    };
    let token = "RAW-CAPABILITY-MUST-NOT-APPEAR";
    let (status, headers, body) = call(&state, get(&format!("/rsvp/{token}"))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_privacy(&headers);
    assert!(body.contains("Please try the link again shortly."));
    assert!(!body.contains(token));
    assert!(!body.to_ascii_lowercase().contains("database"));
}

#[tokio::test]
async fn quick_add_settings_failure_never_falls_back_to_zero_offset() {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_millis(100))
        .connect_lazy("postgres://127.0.0.1:1/almanac")
        .unwrap();
    let state = AppState {
        config: Arc::new(Config::dev()),
        store: Arc::new(PgStore::from_pool(pool)),
    };
    let csrf = almanac::auth::new_csrf_token();
    let cookie = format!("{}={csrf}", almanac::auth::CSRF_COOKIE);
    let request = post_json(
        "/quick-add.json",
        &cookie,
        "u_alice",
        "a@x.co",
        format!(
            r#"{{"csrf_token":"{csrf}","text":"Lunch tomorrow 12pm","calendar_id":"default:u_alice","intent":"review"}}"#
        ),
    );
    let (status, _headers, body) = call(&state, request).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("\"kind\":\"unavailable\""), "{body}");
    assert!(
        !body.contains("\"starts_at\""),
        "no UTC-zero interpretation: {body}"
    );
}

#[tokio::test]
async fn rsvp_page_excludes_attendee_identity() {
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
    assert!(
        !page.contains("<script>alert(9)</script>"),
        "attendee name must not be rendered"
    );
    assert!(
        !page.contains("&lt;script&gt;"),
        "even escaped attendee identity is outside the safe projection"
    );
    assert!(!page.contains("x@y.co"));
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
        &[
            ("csrf_token", &scsrf),
            ("timezone", "UTC+08:00"),
            ("week_start", "monday"),
        ],
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
    assert!(
        ctype.starts_with("text/calendar"),
        "content-type is text/calendar"
    );
    assert!(ics.starts_with("BEGIN:VCALENDAR\r\n"));
    assert!(ics.contains("BEGIN:VTIMEZONE"));
    assert!(ics.contains("TZID:UTC+0800"));
    assert!(ics.contains("BEGIN:VEVENT"));
    // 10:00 UTC+8 local wall clock via TZID.
    assert!(
        ics.contains("DTSTART;TZID=UTC+0800:20990615T100000"),
        "{ics}"
    );
    assert!(ics.contains("RRULE:FREQ=WEEKLY;COUNT=5"));
    // TEXT escaping of comma + semicolon.
    assert!(ics.contains("SUMMARY:Sync\\, weekly\\; team"));
    assert!(ics.contains("LOCATION:Room A\\, B"));
    assert!(ics.trim_end().ends_with("END:VCALENDAR"));

    // Per-event download.
    let id = {
        let (_s, _h, month) = call(&state, get_as("/?y=2099&m=6", "u_alice", "a@x.co")).await;
        let target = find_between(&month, "/edit/", "\"").unwrap();
        target.split('?').next().unwrap().to_string()
    };
    let (status, h, one) = call(
        &state,
        get_as(&format!("/event/{id}/ics"), "u_alice", "a@x.co"),
    )
    .await;
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

    let calendar_id = almanac::store::default_calendar_id("u_alice");

    // Stage one reviews and performs zero writes.
    let review = post_form(
        "/quick-add",
        &cookie,
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", &csrf),
            ("text", "Lunch tomorrow 12pm"),
            ("calendar_id", &calendar_id),
            ("intent", "review"),
        ],
    );
    let (status, _headers, review_body) = call(&state, review).await;
    assert_eq!(status, StatusCode::OK);
    assert!(review_body.contains("Review quick add"));
    assert!(review_body.contains("fixed offset"));
    assert!(
        state
            .store
            .list_events("u_alice")
            .await
            .unwrap()
            .events
            .is_empty(),
        "review performs zero writes"
    );
    let reviewed_start =
        find_between(&review_body, "name=\"reviewed_starts_at\" value=\"", "\"").unwrap();
    let json_review = post_json(
        "/quick-add.json",
        &cookie,
        "u_alice",
        "a@x.co",
        format!(
            r#"{{"csrf_token":"{csrf}","text":"Lunch tomorrow 12pm","calendar_id":"{calendar_id}","intent":"review"}}"#
        ),
    );
    let (status, _headers, json_review_body) = call(&state, json_review).await;
    assert_eq!(status, StatusCode::OK);
    assert!(json_review_body.contains("\"kind\":\"review\""));
    assert_eq!(
        find_between(&json_review_body, "\"starts_at\":", ",").unwrap(),
        reviewed_start,
        "HTML and JSON share one evaluator"
    );

    // Stage two reparses and commits only when the reviewed interpretation still matches.
    let qa = post_form(
        "/quick-add",
        &cookie,
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", &csrf),
            ("text", "Lunch tomorrow 12pm"),
            ("calendar_id", &calendar_id),
            ("intent", "commit"),
            ("reviewed_title", "Lunch"),
            ("reviewed_starts_at", &reviewed_start),
        ],
    );
    let (status, loc_headers, _b) = call(&state, qa).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "quick-add creates + redirects"
    );
    let location = loc_headers
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let (_s, _h, month) = call(&state, get_as(&location, "u_alice", "a@x.co")).await;
    assert!(month.contains("Lunch"), "the quick-added event appears");

    // Invalid input is typed; opening the editor is a separate explicit intent.
    let qa2 = post_form(
        "/quick-add",
        &cookie,
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", &csrf),
            ("text", "buy milk"),
            ("calendar_id", &calendar_id),
            ("intent", "review"),
        ],
    );
    let (status, _h, invalid) = call(&state, qa2).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(invalid.contains("We couldn&#39;t interpret that phrase."));

    let open_editor = post_form(
        "/quick-add",
        &cookie,
        "u_alice",
        "a@x.co",
        &[
            ("csrf_token", &csrf),
            ("text", "buy milk"),
            ("calendar_id", &calendar_id),
            ("intent", "open_editor"),
        ],
    );
    let (status, _h, editor) = call(&state, open_editor).await;
    assert_eq!(status, StatusCode::OK);
    assert!(editor.contains("New event"));
    assert!(
        editor.contains("value=\"buy milk\""),
        "raw text prefilled as the title"
    );
}

// --- helpers (mirrors calendar_flow.rs) -----------------------------------------------

async fn call(state: &AppState, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
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

fn post_public_form(uri: &str, cookie: &str, pairs: &[(&str, &str)]) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::COOKIE, cookie)
        .body(Body::from(form_encode(pairs)))
        .unwrap()
}

fn post_json(uri: &str, cookie: &str, subject: &str, email: &str, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, cookie)
        .header("x-auth-subject", subject)
        .header("x-auth-email", email)
        .body(Body::from(body))
        .unwrap()
}

fn assert_privacy(headers: &axum::http::HeaderMap) {
    assert_eq!(
        headers
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("private, no-store")
    );
    assert_eq!(
        headers
            .get("referrer-policy")
            .and_then(|value| value.to_str().ok()),
        Some("no-referrer")
    );
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
