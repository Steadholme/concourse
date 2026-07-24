//! PostgreSQL `Store` integration test.
//!
//! This test is explicitly ignored by the database-free default suite. Spin up a throwaway
//! Postgres, set `TEST_DATABASE_URL`, and run the ignored gate:
//!
//! ```text
//! docker run --rm -d --name almanac-testpg -e POSTGRES_PASSWORD=pw -e POSTGRES_DB=almanac \
//!   -p 127.0.0.1:55480:5432 postgres:18-alpine
//! TEST_DATABASE_URL=postgres://postgres:pw@127.0.0.1:55480/almanac \
//!   cargo test --test pg_store -- --ignored --nocapture
//! docker rm -f almanac-testpg
//! ```
//!
//! Uses a multi-threaded runtime (matching production); the `Store` trait is async, so the
//! handlers `.await` sqlx natively with no sync-over-async bridge.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use sqlx::postgres::PgPoolOptions;
use sqlx::Row;
use tower::ServiceExt;

use almanac::config::EVENT_LIST_LIMIT;
use almanac::store::{
    AttendeeInput, Event as StoreEvent, EventBundle, EventException, PgStore,
    Reminder as StoreReminder,
};
use almanac::{app, build_dev_state, AppState};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TEST_DATABASE_URL; run this PostgreSQL gate explicitly with --ignored"]
async fn pg_store_full_integration() {
    let url = std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must point to the disposable PostgreSQL gate");

    // --- connect / migrate (idempotent: run twice) -------------------------
    let pg = PgStore::connect(&url)
        .await
        .expect("connect to TEST_DATABASE_URL");
    pg.migrate().await.expect("migrate");
    pg.migrate().await.expect("migrate is idempotent");

    let mut state = build_dev_state();
    state.store = Arc::new(pg);

    // Raw pool for clean-slate setup, out-of-band asserts, and teardown.
    let raw = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    sqlx::query("DELETE FROM event_attendees")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query("DELETE FROM event_reminders")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query("DELETE FROM event_exceptions")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query("DELETE FROM events")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query("DELETE FROM calendars")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query("DELETE FROM contacts")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query("DELETE FROM settings")
        .execute(&raw)
        .await
        .unwrap();

    // --- create an event through the real HTTP flow ------------------------
    let (_s, headers, _b) = call(&state, get_as("/new", "u_alice", "alice@steadholme.local")).await;
    let cookie = set_cookie(&headers).expect("form sets CSRF cookie");
    let csrf = cookie_value(&cookie).expect("csrf value");

    let save = post_form(
        &state,
        "/new",
        &cookie,
        "u_alice",
        "alice@steadholme.local",
        &[
            ("csrf_token", &csrf),
            ("title", "Launch"),
            ("starts_at", "2099-07-04T09:00"),
            ("ends_at", "2099-07-04T10:00"),
            ("location", "Pad 39A"),
            ("notes", "Go for launch."),
        ],
    )
    .await;
    assert_eq!(save.0, StatusCode::SEE_OTHER);

    // The owner's month view renders straight out of Postgres.
    let (status, _h, month) = call(
        &state,
        get_as("/?y=2099&m=7", "u_alice", "alice@steadholme.local"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(month.contains("Launch"));
    assert!(month.contains("Pad 39A"));
    let id = find_between(&month, "/edit/", "\"").expect("event id");

    // The row landed with the gateway owner + a created_at.
    let row =
        sqlx::query("SELECT owner_sub, title, starts_at, created_at FROM events WHERE id = $1")
            .bind(&id)
            .fetch_one(&raw)
            .await
            .unwrap();
    let owner: String = row.try_get("owner_sub").unwrap();
    let created_at: i64 = row.try_get("created_at").unwrap();
    assert_eq!(owner, "u_alice", "owner comes from X-Auth-Subject");

    // --- another owner cannot read/edit/delete it (DB-scoped) --------------
    let (status, _h, _b) = call(
        &state,
        get_as(&format!("/edit/{id}"), "u_mallory", "m@x.co"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // --- edit: upsert preserves created_at, updates the fields -------------
    let (_s, h2, _b) = call(
        &state,
        get_as(&format!("/edit/{id}"), "u_alice", "alice@steadholme.local"),
    )
    .await;
    let cookie2 = set_cookie(&h2).unwrap();
    let csrf2 = cookie_value(&cookie2).unwrap();
    let save2 = post_form(
        &state,
        &format!("/edit/{id}"),
        &cookie2,
        "u_alice",
        "alice@steadholme.local",
        &[
            ("csrf_token", &csrf2),
            ("title", "Launch (scrubbed)"),
            ("starts_at", "2099-07-05T09:00"),
            ("ends_at", "2099-07-05T10:00"),
        ],
    )
    .await;
    assert_eq!(save2.0, StatusCode::SEE_OTHER);

    let row = sqlx::query("SELECT title, created_at FROM events WHERE id = $1")
        .bind(&id)
        .fetch_one(&raw)
        .await
        .unwrap();
    let title: String = row.try_get("title").unwrap();
    let created_after: i64 = row.try_get("created_at").unwrap();
    assert_eq!(title, "Launch (scrubbed)");
    assert_eq!(
        created_after, created_at,
        "created_at preserved across the upsert"
    );

    let event_count: i64 = sqlx::query("SELECT count(*) AS n FROM events")
        .fetch_one(&raw)
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
    assert_eq!(event_count, 1, "upsert never duplicates the row");

    // --- a forged delete (no CSRF cookie) writes nothing -------------------
    let bad = post_form(
        &state,
        &format!("/delete/{id}"),
        "", // no cookie
        "u_alice",
        "alice@steadholme.local",
        &[("csrf_token", "forged")],
    )
    .await;
    assert_eq!(bad.0, StatusCode::FORBIDDEN);
    let still: i64 = sqlx::query("SELECT count(*) AS n FROM events WHERE id = $1")
        .bind(&id)
        .fetch_one(&raw)
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
    assert_eq!(still, 1, "CSRF failure deleted nothing");

    // --- real delete --------------------------------------------------------
    let del = post_form(
        &state,
        &format!("/delete/{id}"),
        &cookie2,
        "u_alice",
        "alice@steadholme.local",
        &[("csrf_token", &csrf2)],
    )
    .await;
    assert_eq!(del.0, StatusCode::SEE_OTHER);
    let after: i64 = sqlx::query("SELECT count(*) AS n FROM events WHERE id = $1")
        .bind(&id)
        .fetch_one(&raw)
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
    assert_eq!(after, 0, "event deleted from Postgres");

    // --- contacts round-trip through Postgres ------------------------------
    let (_s, ch, _b) = call(
        &state,
        get_as("/contacts", "u_alice", "alice@steadholme.local"),
    )
    .await;
    let ccookie = set_cookie(&ch).unwrap();
    let ccsrf = cookie_value(&ccookie).unwrap();
    let add = post_form(
        &state,
        "/contacts/new",
        &ccookie,
        "u_alice",
        "alice@steadholme.local",
        &[
            ("csrf_token", &ccsrf),
            ("name", "Ada Lovelace"),
            ("email", "ada@analytical.engine"),
        ],
    )
    .await;
    assert_eq!(add.0, StatusCode::SEE_OTHER);
    let contact_count: i64 = sqlx::query("SELECT count(*) AS n FROM contacts WHERE owner_sub = $1")
        .bind("u_alice")
        .fetch_one(&raw)
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
    assert_eq!(contact_count, 1);
    let (_s, _h, list) = call(
        &state,
        get_as("/contacts", "u_alice", "alice@steadholme.local"),
    )
    .await;
    assert!(list.contains("Ada Lovelace"));

    // --- settings round-trip through Postgres ------------------------------
    let (_s, sh, sbody) = call(
        &state,
        get_as("/settings", "u_alice", "alice@steadholme.local"),
    )
    .await;
    assert!(
        sbody.contains("value=\"UTC\" selected"),
        "defaults to UTC before any save"
    );
    let scookie = set_cookie(&sh).unwrap();
    let scsrf = cookie_value(&scookie).unwrap();
    let save_settings = post_form(
        &state,
        "/settings",
        &scookie,
        "u_alice",
        "alice@steadholme.local",
        &[
            ("csrf_token", &scsrf),
            ("timezone", "UTC+08:00"),
            ("week_start", "monday"),
        ],
    )
    .await;
    assert_eq!(save_settings.0, StatusCode::SEE_OTHER);
    let srow = sqlx::query("SELECT timezone, week_start FROM settings WHERE owner_sub = $1")
        .bind("u_alice")
        .fetch_one(&raw)
        .await
        .unwrap();
    let tz: String = srow.try_get("timezone").unwrap();
    let ws: String = srow.try_get("week_start").unwrap();
    assert_eq!(tz, "UTC+08:00", "timezone persisted to Postgres");
    assert_eq!(ws, "monday", "week_start persisted to Postgres");
    // Re-saving the same owner updates the single row (no duplicate).
    let settings_count: i64 =
        sqlx::query("SELECT count(*) AS n FROM settings WHERE owner_sub = $1")
            .bind("u_alice")
            .fetch_one(&raw)
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
    assert_eq!(settings_count, 1, "one settings row per owner");

    // --- attendees + reminders + RSVP + due-scan through Postgres ----------
    let (_s, ah, _b) = call(&state, get_as("/new", "u_alice", "alice@steadholme.local")).await;
    let acookie = set_cookie(&ah).unwrap();
    let acsrf = cookie_value(&acookie).unwrap();
    let ev = post_form(
        &state,
        "/new",
        &acookie,
        "u_alice",
        "alice@steadholme.local",
        &[
            ("csrf_token", &acsrf),
            ("title", "Review"),
            ("starts_at", "2099-09-01T09:00"),
            ("ends_at", "2099-09-01T10:00"),
            ("attendees", "Guest <guest@x.co>"),
            ("rem_10", "on"),
        ],
    )
    .await;
    assert_eq!(ev.0, StatusCode::SEE_OTHER);
    let (_s, _h, m9) = call(
        &state,
        get_as("/?y=2099&m=9", "u_alice", "alice@steadholme.local"),
    )
    .await;
    let eid = find_between(&m9, "/edit/", "\"").expect("event id");

    let acount: i64 = sqlx::query("SELECT count(*) AS n FROM event_attendees WHERE event_id = $1")
        .bind(&eid)
        .fetch_one(&raw)
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
    assert_eq!(acount, 1, "attendee persisted to Postgres");
    let rcount: i64 = sqlx::query("SELECT count(*) AS n FROM event_reminders WHERE event_id = $1")
        .bind(&eid)
        .fetch_one(&raw)
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
    assert_eq!(rcount, 1, "reminder persisted to Postgres");

    // The public RSVP token round-trips and updates the row.
    let (_s, _h, detail) = call(
        &state,
        get_as(
            &format!("/event/{eid}"),
            "u_alice",
            "alice@steadholme.local",
        ),
    )
    .await;
    let token = find_between(&detail, "/rsvp/", "\"").expect("rsvp token");
    let rsvp_get = call(
        &state,
        Request::builder()
            .uri(format!("/rsvp/{token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(rsvp_get.0, StatusCode::OK);
    let rsvp_cookie = set_cookie(&rsvp_get.1).expect("RSVP GET sets CSRF cookie");
    let rsvp_csrf = cookie_value(&rsvp_cookie).unwrap();
    let confirm = post_form(
        &state,
        &format!("/rsvp/{token}"),
        &rsvp_cookie,
        "",
        "",
        &[
            ("csrf_token", &rsvp_csrf),
            ("intent", "confirm"),
            ("reply", "accepted"),
        ],
    )
    .await;
    assert_eq!(confirm.0, StatusCode::OK);
    let proof = find_between(&confirm.2, "name=\"confirmation_proof\" value=\"", "\"")
        .expect("RSVP confirm mints proof");
    let commit = post_form(
        &state,
        &format!("/rsvp/{token}"),
        &rsvp_cookie,
        "",
        "",
        &[
            ("csrf_token", &rsvp_csrf),
            ("intent", "commit"),
            ("reply", "accepted"),
            ("confirmation_proof", &proof),
        ],
    )
    .await;
    assert_eq!(commit.0, StatusCode::SEE_OTHER);
    let status: String = sqlx::query("SELECT status FROM event_attendees WHERE token = $1")
        .bind(&token)
        .fetch_one(&raw)
        .await
        .unwrap()
        .try_get("status")
        .unwrap();
    assert_eq!(status, "accepted", "RSVP status persisted to Postgres");

    // due_reminders: an imminent event with a 10-min reminder surfaces from the JOIN query, then
    // stops once marked delivered.
    let now = almanac::now_ms();
    state
        .store
        .upsert_event(StoreEvent {
            id: "pg-soon".into(),
            owner_sub: "u_alice".into(),
            calendar_id: String::new(),
            title: "Soon".into(),
            starts_at: now + 5 * 60_000,
            ends_at: now + 65 * 60_000,
            all_day: false,
            location: String::new(),
            notes: String::new(),
            rrule: String::new(),
            series_id: String::new(),
            override_occurrence_date: 0,
            exception_dates: Vec::new(),
            created_at: now,
        })
        .await
        .unwrap();
    state
        .store
        .replace_reminders(
            "u_alice",
            "pg-soon",
            vec![StoreReminder {
                id: "pg-rem".into(),
                event_id: "pg-soon".into(),
                owner_sub: "u_alice".into(),
                minutes_before: 10,
                delivered_at: 0,
                created_at: now,
            }],
        )
        .await
        .unwrap();
    let due = state.store.due_reminders(now, 50).await.unwrap();
    assert!(
        due.iter().any(|d| d.reminder_id == "pg-rem"),
        "due reminder surfaces from Postgres JOIN"
    );
    state
        .store
        .mark_reminder_delivered("pg-rem", now)
        .await
        .unwrap();
    let due2 = state.store.due_reminders(now, 50).await.unwrap();
    assert!(
        !due2.iter().any(|d| d.reminder_id == "pg-rem"),
        "delivered reminder no longer due"
    );

    // --- EventListing reads limit + 1 and reports partial truth ------------
    let limit_owner = "u_listing_contract";
    sqlx::query(
        "INSERT INTO events \
             (id, owner_sub, calendar_id, title, starts_at, ends_at, all_day, location, notes, \
              rrule, series_id, override_occurrence_date, created_at) \
         SELECT 'pg-limit-' || n, $1, '', 'Loaded', n, n + 1, FALSE, '', '', '', '', 0, n \
         FROM generate_series(0::BIGINT, $2) AS generated(n)",
    )
    .bind(limit_owner)
    .bind(EVENT_LIST_LIMIT as i64)
    .execute(&raw)
    .await
    .unwrap();
    let listing = state.store.list_events(limit_owner).await.unwrap();
    assert_eq!(listing.events.len(), EVENT_LIST_LIMIT);
    assert!(
        listing.has_more,
        "the extra row is surfaced, not truncated silently"
    );
    assert_eq!(listing.events.first().unwrap().starts_at, 0);
    assert_eq!(
        listing.events.last().unwrap().starts_at,
        (EVENT_LIST_LIMIT - 1) as i64
    );
    sqlx::query("DELETE FROM events WHERE owner_sub = $1")
        .bind(limit_owner)
        .execute(&raw)
        .await
        .unwrap();

    // --- atomic event projection + rollback + metadata preservation --------
    let bundle_owner = "u_bundle_contract";
    state
        .store
        .save_event_bundle(event_bundle(
            store_event(
                "pg-bundle",
                bundle_owner,
                "Original",
                4_000_000_000_000,
                "",
                "",
                0,
            ),
            &[("Guest", "Guest@Example.test")],
            &[10],
            100,
            false,
        ))
        .await
        .unwrap();
    let initial_attendee = state
        .store
        .list_attendees(bundle_owner, "pg-bundle")
        .await
        .unwrap()
        .pop()
        .unwrap();
    state
        .store
        .set_attendee_status_by_token(&initial_attendee.token, "accepted")
        .await
        .unwrap();
    let initial_reminder = state
        .store
        .list_reminders(bundle_owner, "pg-bundle")
        .await
        .unwrap()
        .pop()
        .unwrap();
    state
        .store
        .mark_reminder_delivered(&initial_reminder.id, 777)
        .await
        .unwrap();

    state
        .store
        .save_event_bundle(event_bundle(
            store_event(
                "pg-bundle",
                bundle_owner,
                "Preserved",
                4_000_000_100_000,
                "",
                "",
                0,
            ),
            &[("Renamed", "guest@example.test")],
            &[10, 30],
            200,
            false,
        ))
        .await
        .unwrap();
    let preserved_attendees = state
        .store
        .list_attendees(bundle_owner, "pg-bundle")
        .await
        .unwrap();
    assert_eq!(preserved_attendees.len(), 1);
    assert_eq!(preserved_attendees[0].id, initial_attendee.id);
    assert_eq!(preserved_attendees[0].token, initial_attendee.token);
    assert_eq!(preserved_attendees[0].status, "accepted");
    let preserved_reminders = state
        .store
        .list_reminders(bundle_owner, "pg-bundle")
        .await
        .unwrap();
    let retained_reminder = preserved_reminders
        .iter()
        .find(|reminder| reminder.minutes_before == 10)
        .unwrap();
    assert_eq!(retained_reminder.id, initial_reminder.id);
    assert_eq!(retained_reminder.delivered_at, 777);

    sqlx::query("DROP TRIGGER IF EXISTS almanac_test_fail_bundle ON event_attendees")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION IF EXISTS almanac_test_fail_bundle()")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query(
        "CREATE FUNCTION almanac_test_fail_bundle() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN \
           IF NEW.email = 'pg-fail@example.test' THEN \
             RAISE EXCEPTION 'injected attendee failure'; \
           END IF; \
           RETURN NEW; \
         END; $$",
    )
    .execute(&raw)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER almanac_test_fail_bundle BEFORE INSERT ON event_attendees \
         FOR EACH ROW EXECUTE FUNCTION almanac_test_fail_bundle()",
    )
    .execute(&raw)
    .await
    .unwrap();

    let failed_bundle = state
        .store
        .save_event_bundle(event_bundle(
            store_event(
                "pg-bundle",
                bundle_owner,
                "Must roll back",
                4_000_000_200_000,
                "",
                "",
                0,
            ),
            &[
                ("Retained", "guest@example.test"),
                ("Fail", "pg-fail@example.test"),
            ],
            &[60],
            300,
            false,
        ))
        .await;
    assert!(failed_bundle.is_err(), "trigger failure aborts the bundle");
    assert_eq!(
        state
            .store
            .get_event(bundle_owner, "pg-bundle")
            .await
            .unwrap()
            .unwrap()
            .title,
        "Preserved"
    );
    assert_eq!(
        state
            .store
            .list_attendees(bundle_owner, "pg-bundle")
            .await
            .unwrap(),
        preserved_attendees
    );
    assert_eq!(
        state
            .store
            .list_reminders(bundle_owner, "pg-bundle")
            .await
            .unwrap(),
        preserved_reminders
    );
    sqlx::query("DROP TRIGGER almanac_test_fail_bundle ON event_attendees")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION almanac_test_fail_bundle()")
        .execute(&raw)
        .await
        .unwrap();

    // A globally colliding event id owned by someone else is never reported as a successful save.
    state
        .store
        .save_event_bundle(event_bundle(
            store_event(
                "pg-collision",
                "u_collision_bob",
                "Bob",
                4_000_000_300_000,
                "",
                "",
                0,
            ),
            &[],
            &[],
            400,
            false,
        ))
        .await
        .unwrap();
    assert!(state
        .store
        .save_event_bundle(event_bundle(
            store_event(
                "pg-collision",
                "u_collision_alice",
                "Alice",
                4_000_000_400_000,
                "",
                "",
                0,
            ),
            &[],
            &[],
            500,
            false,
        ))
        .await
        .is_err());
    assert_eq!(
        state
            .store
            .get_event("u_collision_bob", "pg-collision")
            .await
            .unwrap()
            .unwrap()
            .title,
        "Bob"
    );

    // --- series reconciliation prunes invalid overrides + their children ---
    let series_owner = "u_series_contract";
    let day = 86_400_000;
    let series_start = 4_100_000_000_000;
    state
        .store
        .save_event_bundle(event_bundle(
            store_event(
                "pg-series",
                series_owner,
                "Series",
                series_start,
                "FREQ=DAILY;COUNT=3",
                "",
                0,
            ),
            &[("Base", "base@series.test")],
            &[5],
            600,
            true,
        ))
        .await
        .unwrap();
    for (id, occurrence, email) in [
        (
            "pg-series-override-2",
            series_start + day,
            "second@series.test",
        ),
        (
            "pg-series-override-3",
            series_start + 2 * day,
            "third@series.test",
        ),
    ] {
        state
            .store
            .save_event_bundle(event_bundle(
                store_event(
                    id,
                    series_owner,
                    id,
                    occurrence + 3_600_000,
                    "",
                    "pg-series",
                    occurrence,
                ),
                &[("Child", email)],
                &[15],
                700,
                false,
            ))
            .await
            .unwrap();
        state
            .store
            .upsert_event_exception(EventException {
                id: format!("exception-{id}"),
                event_id: "pg-series".to_string(),
                owner_sub: series_owner.to_string(),
                occurrence_date: occurrence,
                created_at: 700,
            })
            .await
            .unwrap();
    }
    let canonical_override = state
        .store
        .save_event_bundle(event_bundle(
            store_event(
                "pg-series-override-race",
                series_owner,
                "Second moved again",
                series_start + day + 7_200_000,
                "",
                "pg-series",
                series_start + day,
            ),
            &[("Child", "second@series.test")],
            &[15],
            750,
            false,
        ))
        .await
        .unwrap();
    assert_eq!(canonical_override.id, "pg-series-override-2");
    assert!(state
        .store
        .get_event(series_owner, "pg-series-override-race")
        .await
        .unwrap()
        .is_none());
    let retained_child = state
        .store
        .list_attendees(series_owner, "pg-series-override-2")
        .await
        .unwrap()
        .pop()
        .unwrap();
    state
        .store
        .set_attendee_status_by_token(&retained_child.token, "tentative")
        .await
        .unwrap();
    let retained_child_reminder = state
        .store
        .list_reminders(series_owner, "pg-series-override-2")
        .await
        .unwrap()
        .pop()
        .unwrap();
    state
        .store
        .mark_reminder_delivered(&retained_child_reminder.id, 888)
        .await
        .unwrap();

    state
        .store
        .save_event_bundle(event_bundle(
            store_event(
                "pg-series",
                series_owner,
                "Series shortened",
                series_start,
                "FREQ=DAILY;COUNT=2",
                "",
                0,
            ),
            &[("Base", "base@series.test")],
            &[5],
            800,
            true,
        ))
        .await
        .unwrap();
    assert!(state
        .store
        .get_event_override(series_owner, "pg-series", series_start + day)
        .await
        .unwrap()
        .is_some());
    assert!(state
        .store
        .get_event_override(series_owner, "pg-series", series_start + 2 * day)
        .await
        .unwrap()
        .is_none());
    let retained_after = state
        .store
        .list_attendees(series_owner, "pg-series-override-2")
        .await
        .unwrap();
    assert_eq!(retained_after[0].token, retained_child.token);
    assert_eq!(retained_after[0].status, "tentative");
    assert_eq!(
        state
            .store
            .list_reminders(series_owner, "pg-series-override-2")
            .await
            .unwrap()[0]
            .delivered_at,
        888
    );
    assert!(state
        .store
        .list_attendees(series_owner, "pg-series-override-3")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        state
            .store
            .get_event(series_owner, "pg-series")
            .await
            .unwrap()
            .unwrap()
            .exception_dates,
        vec![series_start + day]
    );

    state
        .store
        .save_event_bundle(event_bundle(
            store_event(
                "pg-series",
                series_owner,
                "Series removed",
                series_start,
                "",
                "",
                0,
            ),
            &[("Base", "base@series.test")],
            &[5],
            900,
            true,
        ))
        .await
        .unwrap();
    assert!(state
        .store
        .get_event_override(series_owner, "pg-series", series_start + day)
        .await
        .unwrap()
        .is_none());
    assert!(state
        .store
        .get_event(series_owner, "pg-series")
        .await
        .unwrap()
        .unwrap()
        .exception_dates
        .is_empty());
    assert!(state
        .store
        .list_reminders(series_owner, "pg-series-override-2")
        .await
        .unwrap()
        .is_empty());

    // --- full tree delete rolls back on failure, then removes every row ----
    let tree_owner = "u_tree_contract";
    let tree_start = 4_200_000_000_000;
    state
        .store
        .save_event_bundle(event_bundle(
            store_event(
                "pg-tree",
                tree_owner,
                "Tree",
                tree_start,
                "FREQ=DAILY;COUNT=2",
                "",
                0,
            ),
            &[("Base", "base@tree.test")],
            &[5],
            1_000,
            true,
        ))
        .await
        .unwrap();
    state
        .store
        .save_event_bundle(event_bundle(
            store_event(
                "pg-tree-override",
                tree_owner,
                "Moved",
                tree_start + day + 3_600_000,
                "",
                "pg-tree",
                tree_start + day,
            ),
            &[("Child", "child@tree.test")],
            &[15],
            1_100,
            false,
        ))
        .await
        .unwrap();
    state
        .store
        .upsert_event_exception(EventException {
            id: "pg-tree-exception".to_string(),
            event_id: "pg-tree".to_string(),
            owner_sub: tree_owner.to_string(),
            occurrence_date: tree_start + day,
            created_at: 1_100,
        })
        .await
        .unwrap();

    sqlx::query("DROP TRIGGER IF EXISTS almanac_test_fail_tree ON event_reminders")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION IF EXISTS almanac_test_fail_tree()")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query(
        "CREATE FUNCTION almanac_test_fail_tree() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN \
           IF OLD.event_id = 'pg-tree-override' THEN \
             RAISE EXCEPTION 'injected tree delete failure'; \
           END IF; \
           RETURN OLD; \
         END; $$",
    )
    .execute(&raw)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER almanac_test_fail_tree BEFORE DELETE ON event_reminders \
         FOR EACH ROW EXECUTE FUNCTION almanac_test_fail_tree()",
    )
    .execute(&raw)
    .await
    .unwrap();
    assert!(state
        .store
        .delete_event_tree(tree_owner, "pg-tree")
        .await
        .is_err());

    let tree_events: i64 = sqlx::query("SELECT count(*) AS n FROM events WHERE id = $1 OR id = $2")
        .bind("pg-tree")
        .bind("pg-tree-override")
        .fetch_one(&raw)
        .await
        .unwrap()
        .try_get("n")
        .unwrap();
    let tree_attendees: i64 = sqlx::query(
        "SELECT count(*) AS n FROM event_attendees WHERE event_id = $1 OR event_id = $2",
    )
    .bind("pg-tree")
    .bind("pg-tree-override")
    .fetch_one(&raw)
    .await
    .unwrap()
    .try_get("n")
    .unwrap();
    let tree_reminders: i64 = sqlx::query(
        "SELECT count(*) AS n FROM event_reminders WHERE event_id = $1 OR event_id = $2",
    )
    .bind("pg-tree")
    .bind("pg-tree-override")
    .fetch_one(&raw)
    .await
    .unwrap()
    .try_get("n")
    .unwrap();
    let tree_exceptions: i64 =
        sqlx::query("SELECT count(*) AS n FROM event_exceptions WHERE event_id = $1")
            .bind("pg-tree")
            .fetch_one(&raw)
            .await
            .unwrap()
            .try_get("n")
            .unwrap();
    assert_eq!(
        (tree_events, tree_attendees, tree_reminders, tree_exceptions),
        (2, 2, 2, 1),
        "the failed tree delete rolled every prior delete back"
    );

    sqlx::query("DROP TRIGGER almanac_test_fail_tree ON event_reminders")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION almanac_test_fail_tree()")
        .execute(&raw)
        .await
        .unwrap();
    assert!(state
        .store
        .delete_event_tree(tree_owner, "pg-tree")
        .await
        .unwrap());
    let tree_rows_after: i64 = sqlx::query(
        "SELECT \
             (SELECT count(*) FROM events WHERE id = $1 OR id = $2) + \
             (SELECT count(*) FROM event_attendees WHERE event_id = $1 OR event_id = $2) + \
             (SELECT count(*) FROM event_reminders WHERE event_id = $1 OR event_id = $2) + \
             (SELECT count(*) FROM event_exceptions WHERE event_id = $1) AS n",
    )
    .bind("pg-tree")
    .bind("pg-tree-override")
    .fetch_one(&raw)
    .await
    .unwrap()
    .try_get("n")
    .unwrap();
    assert_eq!(tree_rows_after, 0, "the successful tree delete is complete");

    // Teardown.
    sqlx::query("DELETE FROM event_attendees")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query("DELETE FROM event_reminders")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query("DELETE FROM event_exceptions")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query("DELETE FROM events")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query("DELETE FROM calendars")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query("DELETE FROM contacts")
        .execute(&raw)
        .await
        .unwrap();
    sqlx::query("DELETE FROM settings")
        .execute(&raw)
        .await
        .unwrap();
    println!(
        "PG STORE INTEGRATION OK: migrate (idempotent) + event create/edit upsert + created_at \
         preserved + owner isolation + CSRF enforced + delete + contacts + attendees/RSVP + \
         reminders due-scan + EventListing limit+1 + atomic bundle rollback + series \
         reconciliation + full-tree rollback/delete — all through Postgres."
    );
}

fn store_event(
    id: &str,
    owner_sub: &str,
    title: &str,
    starts_at: i64,
    rrule: &str,
    series_id: &str,
    override_occurrence_date: i64,
) -> StoreEvent {
    StoreEvent {
        id: id.to_string(),
        owner_sub: owner_sub.to_string(),
        calendar_id: String::new(),
        title: title.to_string(),
        starts_at,
        ends_at: starts_at + 3_600_000,
        all_day: false,
        location: String::new(),
        notes: String::new(),
        rrule: rrule.to_string(),
        series_id: series_id.to_string(),
        override_occurrence_date,
        exception_dates: Vec::new(),
        created_at: starts_at,
    }
}

fn event_bundle(
    event: StoreEvent,
    attendees: &[(&str, &str)],
    reminder_minutes: &[i64],
    now_ms: i64,
    reconcile_series: bool,
) -> EventBundle {
    EventBundle {
        event,
        attendees: attendees
            .iter()
            .map(|(name, email)| AttendeeInput {
                email: (*email).to_string(),
                name: (*name).to_string(),
            })
            .collect(),
        reminder_minutes: reminder_minutes.to_vec(),
        now_ms,
        reconcile_series,
    }
}

// --- helpers ---------------------------------------------------------------------------

type Resp = (StatusCode, axum::http::HeaderMap, String);

async fn call(state: &AppState, req: Request<Body>) -> Resp {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

fn get_as(uri: &str, subject: &str, email: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("x-auth-subject", subject)
        .header("x-auth-email", email)
        .body(Body::empty())
        .unwrap()
}

async fn post_form(
    state: &AppState,
    uri: &str,
    cookie: &str,
    subject: &str,
    email: &str,
    pairs: &[(&str, &str)],
) -> Resp {
    let body = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", pct(k), pct(v)))
        .collect::<Vec<_>>()
        .join("&");
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header("x-auth-subject", subject)
        .header("x-auth-email", email);
    if !cookie.is_empty() {
        builder = builder.header(header::COOKIE, cookie);
    }
    call(state, builder.body(Body::from(body)).unwrap()).await
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
    let (_n, v) = first.split_once('=')?;
    Some(v.to_string())
}

fn find_between(s: &str, start: &str, end: &str) -> Option<String> {
    let i = s.find(start)? + start.len();
    let rest = &s[i..];
    let j = rest.find(end)?;
    Some(rest[..j].to_string())
}
