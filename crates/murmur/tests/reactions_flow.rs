//! End-to-end HTTP flow for message reactions + threaded replies (in-memory store, NO database).
//!
//! Drives the real `app` router via `tower::oneshot`, mirroring `chat_flow.rs`. Covers: reaction
//! toggle idempotency + per-message counts, the SSO/CSRF/membership guards on `/react`, cross-room
//! reaction rejection, the `GET /reactions` tally, threaded-reply validation (`reply_to_id`), and
//! the server-rendered timeline (quoted parent + reply count + reaction chip).

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use murmur::{app, build_dev_state, AppState};
use tower::ServiceExt;

const CSRF: &str = "tok_csrf_for_tests";

fn get_auth(path: &str, sub: &str, email: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .header("x-auth-subject", sub)
        .header("x-auth-email", email)
        .body(Body::empty())
        .unwrap()
}

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

/// Pull the first `"id":"..."` value out of a JSON response (test-only, no serde).
fn extract_id(body: &str) -> String {
    let key = "\"id\":\"";
    let start = body.find(key).expect("id in body") + key.len();
    let end = body[start..].find('"').unwrap() + start;
    body[start..end].to_string()
}

/// Post a message to a room and return its id.
async fn send_message(state: &AppState, room: &str, auth: (&str, &str), body: &str) -> String {
    let (status, resp) = call(
        state,
        post_json(
            &format!("/api/rooms/{room}/messages"),
            &format!(r#"{{"body":{body:?}}}"#),
            Some(auth),
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "send failed: {resp}");
    extract_id(&resp)
}

#[tokio::test]
async fn reaction_toggle_and_counts() {
    let state = build_dev_state();
    let alice = ("u_alice", "alice@hf");
    let bob = ("u_bob", "bob@hf");

    // Both join the lobby (first touch) and Alice posts a message.
    let _ = call(&state, get_auth("/api/rooms", alice.0, alice.1)).await;
    let _ = call(&state, get_auth("/api/rooms", bob.0, bob.1)).await;
    let msg = send_message(&state, "lobby", alice, "react to me").await;

    let react_path = format!("/api/rooms/lobby/messages/{msg}/react");

    // --- guards ------------------------------------------------------------
    // Unauthenticated -> 401.
    let (status, _) = call(&state, post_json(&react_path, r#"{"emoji":"👍"}"#, None, Some(CSRF))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no identity -> 401");
    // Bad CSRF -> 401.
    let (status, _) = call(
        &state,
        post_json(&react_path, r#"{"emoji":"👍"}"#, Some(alice), Some("WRONG")),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "CSRF mismatch -> 401");
    // Empty emoji -> 400.
    let (status, _) = call(
        &state,
        post_json(&react_path, r#"{"emoji":"   "}"#, Some(alice), Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "empty emoji -> 400");
    // Missing message -> 404.
    let (status, _) = call(
        &state,
        post_json(
            "/api/rooms/lobby/messages/msg_missing/react",
            r#"{"emoji":"👍"}"#,
            Some(alice),
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "missing message -> 404");

    // --- toggle on ---------------------------------------------------------
    let (status, body) = call(
        &state,
        post_json(&react_path, r#"{"emoji":"👍"}"#, Some(alice), Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"added\":true"), "first react adds: {body}");
    assert!(body.contains("\"emoji\":\"👍\""), "emoji tallied");
    assert!(body.contains("\"count\":1"), "count is 1");
    assert!(body.contains("\"mine\":[\"👍\"]"), "caller's reaction echoed");

    // Bob adds the SAME emoji -> distinct-user count becomes 2.
    let (status, body) = call(
        &state,
        post_json(&react_path, r#"{"emoji":"👍"}"#, Some(bob), Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"count\":2"), "two distinct users -> count 2: {body}");

    // Alice toggles the same emoji again -> removed, count back to 1.
    let (status, body) = call(
        &state,
        post_json(&react_path, r#"{"emoji":"👍"}"#, Some(alice), Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"added\":false"), "second identical react removes: {body}");
    assert!(body.contains("\"count\":1"), "count back to 1");
    assert!(body.contains("\"mine\":[]"), "alice no longer reacted");

    // --- GET reactions tally ----------------------------------------------
    let (status, body) = call(
        &state,
        get_auth(&format!("/api/rooms/lobby/messages/{msg}/reactions"), alice.0, alice.1),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"count\":1"), "bob's reaction remains: {body}");
    assert!(body.contains("\"mine\":[]"), "alice's own list empty after toggle-off");
}

#[tokio::test]
async fn reaction_requires_membership() {
    let state = build_dev_state();
    let alice = ("u_alice", "alice@hf");
    let bob = ("u_bob", "bob@hf");

    // Bob creates a private room and posts; Alice is NOT a member.
    let _ = call(&state, get_auth("/api/rooms", bob.0, bob.1)).await;
    let (status, body) = call(
        &state,
        post_json("/api/rooms", r#"{"name":"secret","kind":"room"}"#, Some(bob), Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let room = extract_id(&body);
    let msg = send_message(&state, &room, bob, "members only").await;

    let (status, _) = call(
        &state,
        post_json(
            &format!("/api/rooms/{room}/messages/{msg}/react"),
            r#"{"emoji":"🔥"}"#,
            Some(alice),
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-member cannot react -> 403");
}

#[tokio::test]
async fn threaded_reply_flow() {
    let state = build_dev_state();
    let alice = ("u_alice", "alice@hf");
    let bob = ("u_bob", "bob@hf");
    let _ = call(&state, get_auth("/api/rooms", alice.0, alice.1)).await;
    let _ = call(&state, get_auth("/api/rooms", bob.0, bob.1)).await;

    // Alice posts a parent; Bob replies to it.
    let parent = send_message(&state, "lobby", alice, "parent post").await;
    let (status, body) = call(
        &state,
        post_json(
            "/api/rooms/lobby/messages",
            &format!(r#"{{"body":"a reply","reply_to_id":"{parent}"}}"#),
            Some(bob),
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(
        body.contains(&format!("\"reply_to_id\":\"{parent}\"")),
        "reply carries its parent id: {body}"
    );

    // A reply to a NON-EXISTENT parent is rejected.
    let (status, _) = call(
        &state,
        post_json(
            "/api/rooms/lobby/messages",
            r#"{"body":"orphan","reply_to_id":"msg_nope"}"#,
            Some(alice),
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "reply to missing parent -> 400");

    // A reply whose parent lives in ANOTHER room is rejected (no cross-room threading).
    let (status, body) = call(
        &state,
        post_json("/api/rooms", r#"{"name":"other"}"#, Some(alice), Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let other = extract_id(&body);
    let (status, _) = call(
        &state,
        post_json(
            &format!("/api/rooms/{other}/messages"),
            &format!(r#"{{"body":"cross","reply_to_id":"{parent}"}}"#),
            Some(alice),
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "cross-room reply -> 400");
}

#[tokio::test]
async fn timeline_renders_reply_and_reaction() {
    let state = build_dev_state();
    let alice = ("u_alice", "alice@hf");
    let _ = call(&state, get_auth("/api/rooms", alice.0, alice.1)).await;

    // Parent + a reply to it, then a reaction on the parent.
    let parent = send_message(&state, "lobby", alice, "hello parent").await;
    let _reply = call(
        &state,
        post_json(
            "/api/rooms/lobby/messages",
            &format!(r#"{{"body":"child reply","reply_to_id":"{parent}"}}"#),
            Some(alice),
            Some(CSRF),
        ),
    )
    .await;
    let (status, _) = call(
        &state,
        post_json(
            &format!("/api/rooms/lobby/messages/{parent}/react"),
            r#"{"emoji":"🎉"}"#,
            Some(alice),
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The server-rendered dashboard shows the quoted parent, the reply count, and the chip.
    let (status, page) = call(&state, get_auth("/", alice.0, alice.1)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("msg__quote"), "quoted parent block rendered");
    assert!(page.contains("hello parent"), "parent snippet present in quote");
    assert!(page.contains("msg__replies"), "reply count marker rendered");
    assert!(page.contains("1 reply"), "singular reply count");
    assert!(page.contains("msg__reactions"), "reaction chip row rendered");
    assert!(page.contains("🎉"), "reaction emoji rendered");
}
