//! End-to-end HTTP flow over the in-memory store (NO database).
//!
//! Drives the real `app` router via `tower::oneshot`, exactly like the rest of the estate.
//! Covers: health, dashboard render + CSRF cookie, the SSO/CSRF/membership guards on the API,
//! the auto-joined lobby, create-room / send / list-messages, and body XSS sanitization in the
//! server-rendered timeline.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use murmur::{app, build_dev_state, AppState};
use tower::ServiceExt;

const CSRF: &str = "tok_csrf_for_tests";

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

/// Authenticated GET (gateway-injected identity).
fn get_auth(path: &str, sub: &str, email: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .header("x-auth-subject", sub)
        .header("x-auth-email", email)
        .body(Body::empty())
        .unwrap()
}

/// JSON POST with optional identity + a CSRF cookie/header pair.
fn post_json(
    path: &str,
    json: &str,
    auth: Option<(&str, &str)>,
    csrf_header: Option<&str>,
) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, format!("__Host-csrf={CSRF}"));
    if let Some((sub, email)) = auth {
        b = b.header("x-auth-subject", sub).header("x-auth-email", email);
    }
    if let Some(tok) = csrf_header {
        b = b.header("x-csrf-token", tok);
    }
    b.body(Body::from(json.to_string())).unwrap()
}

async fn call(state: &AppState, req: Request<Body>) -> (StatusCode, String) {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn full_chat_flow_in_memory() {
    let state = build_dev_state();
    let alice = Some(("u_alice", "alice@hf"));

    // --- health ------------------------------------------------------------
    let (status, body) = call(&state, get("/healthz")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");

    // --- dashboard mints a CSRF cookie + auto-creates the lobby ------------
    let resp = app(state.clone())
        .oneshot(get_auth("/", "u_alice", "alice@hf"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(set_cookie.contains("__Host-csrf="), "dashboard mints CSRF cookie");

    // --- GET /api/rooms: lobby auto-joined --------------------------------
    let (status, body) = call(&state, get_auth("/api/rooms", "u_alice", "alice@hf")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"id\":\"lobby\""), "lobby present");
    assert!(body.contains("#lobby"), "lobby name present");

    // --- unauthenticated API -> 401 ---------------------------------------
    let (status, _) = call(&state, get("/api/rooms")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // --- send with bad CSRF -> 401 ----------------------------------------
    let (status, _) = call(
        &state,
        post_json("/api/rooms/lobby/messages", r#"{"body":"hi"}"#, alice, Some("WRONG")),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "CSRF mismatch -> 401");

    // --- send to lobby (member via auto-join) -----------------------------
    let (status, body) = call(
        &state,
        post_json(
            "/api/rooms/lobby/messages",
            r#"{"body":"hello <b>world</b> https://example.com"}"#,
            alice,
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(body.contains("\"room_id\":\"lobby\""));

    // --- list messages -----------------------------------------------------
    let (status, body) = call(
        &state,
        get_auth("/api/rooms/lobby/messages", "u_alice", "alice@hf"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("hello <b>world</b>"), "raw body in JSON (client escapes)");

    // --- non-member cannot read another room ------------------------------
    // Bob creates a private room; Alice is not a member.
    let bob = Some(("u_bob", "bob@hf"));
    let (status, body) = call(
        &state,
        post_json("/api/rooms", r#"{"name":"secret","kind":"room"}"#, bob, Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let room_id = extract_room_id(&body);
    let (status, _) = call(
        &state,
        get_auth(
            &format!("/api/rooms/{room_id}/messages"),
            "u_alice",
            "alice@hf",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-member -> 403");

    // --- dashboard timeline sanitizes the body ----------------------------
    let (status, page) = call(&state, get_auth("/", "u_alice", "alice@hf")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!page.contains("hello <b>world</b>"), "raw tag must not survive in HTML");
    assert!(page.contains("hello &lt;b&gt;world&lt;/b&gt;"), "body escaped in timeline");
    assert!(
        page.contains("<a href=\"https://example.com\""),
        "url autolinked in timeline"
    );
}

#[tokio::test]
async fn join_unknown_room_is_404() {
    let state = build_dev_state();
    let (status, _) = call(
        &state,
        post_json("/api/rooms/nope/join", "{}", Some(("u_x", "x@hf")), Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn empty_body_rejected() {
    let state = build_dev_state();
    // Ensure the lobby/membership exists first.
    let _ = call(&state, get_auth("/api/rooms", "u_a", "a@hf")).await;
    let (status, _) = call(
        &state,
        post_json("/api/rooms/lobby/messages", r#"{"body":"   "}"#, Some(("u_a", "a@hf")), Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Pull `room_xxx` out of a `{"room":{"id":"room_..."}}` JSON response (test-only, no serde).
fn extract_room_id(body: &str) -> String {
    let key = "\"id\":\"";
    let start = body.find(key).expect("id in body") + key.len();
    let end = body[start..].find('"').unwrap() + start;
    body[start..end].to_string()
}

/// Pull `msg_xxx` out of a `{"message":{"id":"msg_..."}}` JSON response (test-only, no serde).
fn extract_message_id(body: &str) -> String {
    let key = "\"id\":\"";
    let start = body.find(key).expect("id in body") + key.len();
    let end = body[start..].find('"').unwrap() + start;
    body[start..end].to_string()
}

#[tokio::test]
async fn edit_and_delete_flow() {
    let state = build_dev_state();
    let alice = Some(("u_alice", "alice@hf"));
    let bob = Some(("u_bob", "bob@hf"));

    // Both users auto-join the lobby on first touch.
    let _ = call(&state, get_auth("/api/rooms", "u_alice", "alice@hf")).await;
    let _ = call(&state, get_auth("/api/rooms", "u_bob", "bob@hf")).await;

    // Alice posts a message and we capture its id.
    let (status, body) = call(
        &state,
        post_json("/api/rooms/lobby/messages", r#"{"body":"first draft"}"#, alice, Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let msg_id = extract_message_id(&body);

    // --- bad CSRF -> 401 ---------------------------------------------------
    let (status, _) = call(
        &state,
        post_json(
            &format!("/api/rooms/lobby/messages/{msg_id}/edit"),
            r#"{"body":"nope"}"#,
            alice,
            Some("WRONG"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "edit CSRF mismatch -> 401");

    // --- non-author cannot edit -> 403 ------------------------------------
    let (status, _) = call(
        &state,
        post_json(
            &format!("/api/rooms/lobby/messages/{msg_id}/edit"),
            r#"{"body":"hijack"}"#,
            bob,
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-author edit -> 403");

    // --- author edits own message -----------------------------------------
    let (status, body) = call(
        &state,
        post_json(
            &format!("/api/rooms/lobby/messages/{msg_id}/edit"),
            r#"{"body":"edited body"}"#,
            alice,
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("edited body"), "new body echoed");
    assert!(!body.contains("\"edited_at\":0"), "edited_at stamped");

    // The edit is visible on the timeline (server render) with an (edited) marker.
    let (_, page) = call(&state, get_auth("/", "u_alice", "alice@hf")).await;
    assert!(page.contains("edited body"), "edited body rendered");
    assert!(page.contains("(edited)"), "edited marker rendered");

    // --- non-author cannot delete -> 403 ----------------------------------
    let (status, _) = call(
        &state,
        post_json(&format!("/api/rooms/lobby/messages/{msg_id}/delete"), "{}", bob, Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-author delete -> 403");

    // --- author soft-deletes own message ----------------------------------
    let (status, body) = call(
        &state,
        post_json(&format!("/api/rooms/lobby/messages/{msg_id}/delete"), "{}", alice, Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"deleted\":true"), "message flagged deleted");
    assert!(!body.contains("edited body"), "deleted body cleared in response");

    // The API list no longer carries the original content.
    let (_, list) = call(
        &state,
        get_auth("/api/rooms/lobby/messages", "u_alice", "alice@hf"),
    )
    .await;
    assert!(list.contains("\"deleted\":true"));
    assert!(!list.contains("edited body"), "deleted body absent from list");

    // The timeline renders the [deleted] tombstone, not the old text.
    let (_, page) = call(&state, get_auth("/", "u_alice", "alice@hf")).await;
    assert!(page.contains("[deleted]"), "deleted tombstone rendered");
    assert!(!page.contains("edited body"), "old body gone from timeline");

    // --- edit after delete is rejected ------------------------------------
    let (status, _) = call(
        &state,
        post_json(
            &format!("/api/rooms/lobby/messages/{msg_id}/edit"),
            r#"{"body":"resurrect"}"#,
            alice,
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "cannot edit a deleted message");
}

#[tokio::test]
async fn edit_unknown_message_is_404() {
    let state = build_dev_state();
    let _ = call(&state, get_auth("/api/rooms", "u_a", "a@hf")).await;
    let (status, _) = call(
        &state,
        post_json(
            "/api/rooms/lobby/messages/msg_missing/edit",
            r#"{"body":"x"}"#,
            Some(("u_a", "a@hf")),
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
