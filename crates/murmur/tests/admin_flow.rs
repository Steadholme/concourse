//! Admin panel end-to-end over the in-memory store (NO database).
//!
//! Drives the real `app` router via `tower::oneshot`. Covers the admin gate (non-admin -> 403,
//! admin -> 200 for BOTH globals AND the delegated `chat-admins` product group), CSRF on every
//! mutating POST, and each admin mutation:
//! room archive + delete, member remove + ban, and message redaction.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use murmur::{app, build_dev_state, AppState};
use tower::ServiceExt;

const CSRF: &str = "tok_csrf_for_tests";

/// Authenticated GET, optionally carrying an `X-Auth-Groups` header.
fn get_auth(path: &str, sub: &str, email: &str, groups: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("GET")
        .uri(path)
        .header("x-auth-subject", sub)
        .header("x-auth-email", email);
    if let Some(g) = groups {
        b = b.header("x-auth-groups", g);
    }
    b.body(Body::empty()).unwrap()
}

/// JSON POST (the dashboard API path: header-based CSRF).
fn post_json(path: &str, json: &str, auth: (&str, &str), csrf_header: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, format!("__Host-csrf={CSRF}"))
        .header("x-auth-subject", auth.0)
        .header("x-auth-email", auth.1);
    if let Some(tok) = csrf_header {
        b = b.header("x-csrf-token", tok);
    }
    b.body(Body::from(json.to_string())).unwrap()
}

/// Server-rendered admin form POST (urlencoded body + hidden `csrf`, double-submit vs. cookie).
fn post_form(
    path: &str,
    auth: (&str, &str),
    groups: Option<&str>,
    csrf_cookie: bool,
    csrf_field: Option<&str>,
) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header("x-auth-subject", auth.0)
        .header("x-auth-email", auth.1);
    if csrf_cookie {
        b = b.header(header::COOKIE, format!("__Host-csrf={CSRF}"));
    }
    if let Some(g) = groups {
        b = b.header("x-auth-groups", g);
    }
    let body = match csrf_field {
        Some(tok) => format!("csrf={tok}"),
        None => String::new(),
    };
    b.body(Body::from(body)).unwrap()
}

async fn call(state: &AppState, req: Request<Body>) -> (StatusCode, String) {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// Pull the first `"id":"..."` value out of a JSON response (test-only, no serde).
fn extract_id(body: &str) -> String {
    let key = "\"id\":\"";
    let start = body.find(key).expect("id in body") + key.len();
    let end = body[start..].find('"').unwrap() + start;
    body[start..end].to_string()
}

// ---------------------------------------------------------------------------
// Gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_gate_allows_only_admins() {
    let state = build_dev_state();

    // Signed-in but no groups -> 403.
    let (status, _) = call(&state, get_auth("/admin", "u_x", "x@hf", None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no groups -> 403");

    // Signed-in with an unrelated group -> 403.
    let (status, _) = call(&state, get_auth("/admin", "u_x", "x@hf", Some("readers,writers"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-admin group -> 403");

    // `admins` -> 200.
    let (status, body) = call(&state, get_auth("/admin", "u_a", "a@hf", Some("admins"))).await;
    assert_eq!(status, StatusCode::OK, "admins -> 200");
    assert!(body.contains("Rooms"), "admin panel renders");

    // `infra-admins` (with whitespace) -> 200.
    let (status, _) = call(&state, get_auth("/admin", "u_a", "a@hf", Some("dev, infra-admins"))).await;
    assert_eq!(status, StatusCode::OK, "infra-admins -> 200");

    // Delegated admin: the product group `chat-admins` also unlocks the panel (a scoped operator
    // gets chat moderation without global admin). Default group; overridable via MURMUR_ADMIN_GROUP.
    let (status, body) = call(&state, get_auth("/admin", "u_d", "d@hf", Some("chat-admins"))).await;
    assert_eq!(status, StatusCode::OK, "chat-admins -> 200");
    assert!(body.contains("Rooms"), "delegated admin sees the panel");
}

#[tokio::test]
async fn admin_post_requires_admin_and_csrf() {
    let state = build_dev_state();

    // Non-admin POST -> 403 (gate runs before CSRF).
    let (status, _) = call(
        &state,
        post_form("/admin/rooms/lobby/archive", ("u_x", "x@hf"), Some("readers"), true, Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-admin POST -> 403");

    // Admin POST but missing CSRF field -> 401.
    let (status, _) = call(
        &state,
        post_form("/admin/rooms/lobby/archive", ("u_a", "a@hf"), Some("admins"), true, None),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "missing CSRF -> 401");

    // Admin POST, CSRF field present but no cookie -> 401.
    let (status, _) = call(
        &state,
        post_form("/admin/rooms/lobby/archive", ("u_a", "a@hf"), Some("admins"), false, Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no CSRF cookie -> 401");
}

// ---------------------------------------------------------------------------
// Room lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_archive_then_delete_room() {
    let state = build_dev_state();
    let bob = ("u_bob", "bob@hf");

    // Bob creates a room (and is auto-joined).
    let (status, body) = call(
        &state,
        post_json("/api/rooms", r#"{"name":"proj","kind":"room"}"#, bob, Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let room_id = extract_id(&body);

    // It appears in Bob's room list and in the admin panel.
    let (_, list) = call(&state, get_auth("/api/rooms", bob.0, bob.1, None)).await;
    assert!(list.contains("proj"), "room in Bob's list");
    let (_, panel) = call(&state, get_auth("/admin", "u_a", "a@hf", Some("admins"))).await;
    assert!(panel.contains(&room_id), "room in admin panel");

    // Admin archives it -> 303, and it drops out of Bob's active list.
    let (status, _) = call(
        &state,
        post_form(&format!("/admin/rooms/{room_id}/archive"), ("u_a", "a@hf"), Some("admins"), true, Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "archive redirects");
    let (_, list) = call(&state, get_auth("/api/rooms", bob.0, bob.1, None)).await;
    assert!(!list.contains("\"name\":\"proj\""), "archived room hidden from user list");

    // Admin hard-deletes it -> 303, and it is gone from the panel entirely.
    let (status, _) = call(
        &state,
        post_form(&format!("/admin/rooms/{room_id}/delete"), ("u_a", "a@hf"), Some("infra-admins"), true, Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "delete redirects");
    let (_, panel) = call(&state, get_auth("/admin", "u_a", "a@hf", Some("admins"))).await;
    assert!(!panel.contains(&room_id), "deleted room gone from panel");
    // The room detail 404s after deletion.
    let (status, _) = call(
        &state,
        get_auth(&format!("/admin/rooms/{room_id}"), "u_a", "a@hf", Some("admins")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Membership control
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_ban_and_remove_member() {
    let state = build_dev_state();
    let bob = ("u_bob", "bob@hf");
    let carol = ("u_carol", "carol@hf");

    // Both auto-join the lobby; each can read it.
    let _ = call(&state, get_auth("/api/rooms", bob.0, bob.1, None)).await;
    let _ = call(&state, get_auth("/api/rooms", carol.0, carol.1, None)).await;
    let (status, _) = call(&state, get_auth("/api/rooms/lobby/messages", carol.0, carol.1, None)).await;
    assert_eq!(status, StatusCode::OK, "carol is a member before the ban");

    // The room detail lists both members.
    let (status, detail) = call(
        &state,
        get_auth("/admin/rooms/lobby", "u_a", "a@hf", Some("admins")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.contains("carol@hf") && detail.contains("bob@hf"), "members listed");

    // Admin bans carol -> she can no longer read the room (treated as a non-member).
    let (status, _) = call(
        &state,
        post_form("/admin/rooms/lobby/members/u_carol/ban", ("u_a", "a@hf"), Some("admins"), true, Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _) = call(&state, get_auth("/api/rooms/lobby/messages", carol.0, carol.1, None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "banned member -> 403");

    // A banned member cannot re-join their way back in.
    let (status, _) = call(
        &state,
        post_json("/api/rooms/lobby/join", "{}", carol, Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "join is idempotent");
    let (status, _) = call(&state, get_auth("/api/rooms/lobby/messages", carol.0, carol.1, None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "ban sticks across re-join");

    // Admin removes bob outright -> he too is no longer a member.
    let (status, _) = call(
        &state,
        post_form("/admin/rooms/lobby/members/u_bob/remove", ("u_a", "a@hf"), Some("admins"), true, Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _) = call(&state, get_auth("/api/rooms/lobby/messages", bob.0, bob.1, None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "removed member -> 403");
}

// ---------------------------------------------------------------------------
// Message redaction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_redacts_any_message() {
    let state = build_dev_state();
    let bob = ("u_bob", "bob@hf");

    // Bob joins the lobby and posts a message.
    let _ = call(&state, get_auth("/api/rooms", bob.0, bob.1, None)).await;
    let (status, body) = call(
        &state,
        post_json("/api/rooms/lobby/messages", r#"{"body":"secret plans"}"#, bob, Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let msg_id = extract_id(&body);

    // Admin redacts it (admin overrides ownership — Bob is the author, admin is not).
    let (status, _) = call(
        &state,
        post_form(&format!("/admin/messages/{msg_id}/redact"), ("u_a", "a@hf"), Some("admins"), true, Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "redact redirects");

    // The API now flags it deleted and carries the moderator tombstone, not the original text.
    let (_, list) = call(&state, get_auth("/api/rooms/lobby/messages", bob.0, bob.1, None)).await;
    assert!(list.contains("\"deleted\":true"), "message flagged deleted");
    assert!(list.contains("[removed by moderator]"), "tombstone body present");
    assert!(!list.contains("secret plans"), "original body gone");

    // The rendered timeline shows the tombstone, not the old text.
    let (_, page) = call(&state, get_auth("/", bob.0, bob.1, None)).await;
    assert!(page.contains("[removed by moderator]"), "tombstone rendered");
    assert!(!page.contains("secret plans"), "old body gone from timeline");
}
