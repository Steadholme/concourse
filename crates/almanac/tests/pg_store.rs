//! PostgreSQL `Store` integration test.
//!
//! Runs ONLY when `TEST_DATABASE_URL` is set (it needs an external Postgres). When unset the
//! test prints a note and returns early — it never fails the default `cargo test` run, which
//! stays database-free. Spin up a throwaway Postgres and run:
//!
//! ```text
//! docker run --rm -d --name almanac-testpg -e POSTGRES_PASSWORD=pw -e POSTGRES_DB=almanac \
//!   -p 127.0.0.1:55480:5432 postgres:18-alpine
//! TEST_DATABASE_URL=postgres://postgres:pw@127.0.0.1:55480/almanac \
//!   cargo test --test pg_store -- --nocapture
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

use almanac::store::PgStore;
use almanac::{app, build_dev_state, AppState};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pg_store_full_integration() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!(
            "NOTE: TEST_DATABASE_URL not set — skipping Postgres integration test (needs external \
             Postgres). This is expected for the default test run."
        );
        return;
    };

    // --- connect / migrate (idempotent: run twice) -------------------------
    let pg = PgStore::connect(&url).await.expect("connect to TEST_DATABASE_URL");
    pg.migrate().await.expect("migrate");
    pg.migrate().await.expect("migrate is idempotent");

    let mut state = build_dev_state();
    state.store = Arc::new(pg);

    // Raw pool for clean-slate setup, out-of-band asserts, and teardown.
    let raw = PgPoolOptions::new().max_connections(2).connect(&url).await.unwrap();
    sqlx::query("DELETE FROM events").execute(&raw).await.unwrap();
    sqlx::query("DELETE FROM contacts").execute(&raw).await.unwrap();

    // --- create an event through the real HTTP flow ------------------------
    let (_s, headers, _b) = call(&state, get_as("/new", "u_alice", "alice@holdfast.local")).await;
    let cookie = set_cookie(&headers).expect("form sets CSRF cookie");
    let csrf = cookie_value(&cookie).expect("csrf value");

    let save = post_form(
        &state,
        "/new",
        &cookie,
        "u_alice",
        "alice@holdfast.local",
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
    let (status, _h, month) = call(&state, get_as("/?y=2099&m=7", "u_alice", "alice@holdfast.local")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(month.contains("Launch"));
    assert!(month.contains("Pad 39A"));
    let id = find_between(&month, "/edit/", "\"").expect("event id");

    // The row landed with the gateway owner + a created_at.
    let row = sqlx::query("SELECT owner_sub, title, starts_at, created_at FROM events WHERE id = $1")
        .bind(&id)
        .fetch_one(&raw)
        .await
        .unwrap();
    let owner: String = row.try_get("owner_sub").unwrap();
    let created_at: i64 = row.try_get("created_at").unwrap();
    assert_eq!(owner, "u_alice", "owner comes from X-Auth-Subject");

    // --- another owner cannot read/edit/delete it (DB-scoped) --------------
    let (status, _h, _b) = call(&state, get_as(&format!("/edit/{id}"), "u_mallory", "m@x.co")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // --- edit: upsert preserves created_at, updates the fields -------------
    let (_s, h2, _b) = call(&state, get_as(&format!("/edit/{id}"), "u_alice", "alice@holdfast.local")).await;
    let cookie2 = set_cookie(&h2).unwrap();
    let csrf2 = cookie_value(&cookie2).unwrap();
    let save2 = post_form(
        &state,
        &format!("/edit/{id}"),
        &cookie2,
        "u_alice",
        "alice@holdfast.local",
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
    assert_eq!(created_after, created_at, "created_at preserved across the upsert");

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
        "alice@holdfast.local",
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
        "alice@holdfast.local",
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
    let (_s, ch, _b) = call(&state, get_as("/contacts", "u_alice", "alice@holdfast.local")).await;
    let ccookie = set_cookie(&ch).unwrap();
    let ccsrf = cookie_value(&ccookie).unwrap();
    let add = post_form(
        &state,
        "/contacts/new",
        &ccookie,
        "u_alice",
        "alice@holdfast.local",
        &[("csrf_token", &ccsrf), ("name", "Ada Lovelace"), ("email", "ada@analytical.engine")],
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
    let (_s, _h, list) = call(&state, get_as("/contacts", "u_alice", "alice@holdfast.local")).await;
    assert!(list.contains("Ada Lovelace"));

    // Teardown.
    sqlx::query("DELETE FROM events").execute(&raw).await.unwrap();
    sqlx::query("DELETE FROM contacts").execute(&raw).await.unwrap();
    println!(
        "PG STORE INTEGRATION OK: migrate (idempotent) + event create/edit upsert + created_at \
         preserved + owner isolation + CSRF enforced + delete + contacts — all through Postgres."
    );
}

// --- helpers ---------------------------------------------------------------------------

type Resp = (StatusCode, axum::http::HeaderMap, String);

async fn call(state: &AppState, req: Request<Body>) -> Resp {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
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
    headers.get(header::SET_COOKIE).and_then(|v| v.to_str().ok()).map(str::to_string)
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
