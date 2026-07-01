//! Direct-message + people-directory end-to-end over the in-memory store (NO database).
//!
//! Drives the real `app` router via `tower::oneshot`. Covers: the SSO/CSRF guards on the DM
//! write, the people directory (distinct known people, excludes self), deterministic 1:1 DM room
//! keying (same room from either side), both members reading/sending on the reused message API,
//! a third party being locked out, and the DM appearing in both users' room lists.

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

/// Pull the room id out of a `{"room":{"id":"dm_..."}}` JSON response (test-only, no serde).
fn extract_room_id(body: &str) -> String {
    let key = "\"id\":\"";
    let start = body.find(key).expect("id in body") + key.len();
    let end = body[start..].find('"').unwrap() + start;
    body[start..end].to_string()
}

#[tokio::test]
async fn directory_lists_known_people_excluding_self() {
    let state = build_dev_state();
    // Alice and Bob each touch the lobby (auto-join => they become known people).
    let _ = call(&state, get_auth("/api/rooms", "u_alice", "alice@hf")).await;
    let _ = call(&state, get_auth("/api/rooms", "u_bob", "bob@hf")).await;

    // Alice's directory shows Bob, not herself.
    let (status, body) = call(&state, get_auth("/api/directory", "u_alice", "alice@hf")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("bob@hf"), "peer present in directory");
    assert!(!body.contains("alice@hf"), "self excluded from directory");
    assert!(!body.contains("\"user_sub\":\"u_alice\""), "self subject excluded");
}

#[tokio::test]
async fn directory_requires_auth() {
    let state = build_dev_state();
    let req = Request::builder()
        .method("GET")
        .uri("/api/directory")
        .body(Body::empty())
        .unwrap();
    let (status, _) = call(&state, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn open_dm_requires_csrf() {
    let state = build_dev_state();
    let (status, _) = call(
        &state,
        post_json(
            "/api/dms",
            r#"{"subject":"u_bob","email":"bob@hf"}"#,
            Some(("u_alice", "alice@hf")),
            Some("WRONG"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "CSRF mismatch -> 401");
}

#[tokio::test]
async fn cannot_dm_self() {
    let state = build_dev_state();
    let (status, _) = call(
        &state,
        post_json(
            "/api/dms",
            r#"{"subject":"u_alice","email":"alice@hf"}"#,
            Some(("u_alice", "alice@hf")),
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "self-DM rejected");
}

#[tokio::test]
async fn open_dm_is_deterministic_and_bidirectional() {
    let state = build_dev_state();
    let alice = Some(("u_alice", "alice@hf"));
    let bob = Some(("u_bob", "bob@hf"));

    // Alice opens a DM with Bob.
    let (status, body) = call(
        &state,
        post_json("/api/dms", r#"{"subject":"u_bob","email":"bob@hf"}"#, alice, Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let room_id = extract_room_id(&body);
    assert!(body.contains("\"kind\":\"dm\""), "room is a DM");
    assert!(room_id.starts_with("dm_"), "deterministic dm id: {room_id}");

    // Bob opens the DM with Alice from the other side -> SAME room id.
    let (status, body2) = call(
        &state,
        post_json("/api/dms", r#"{"subject":"u_alice","email":"alice@hf"}"#, bob, Some(CSRF)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(extract_room_id(&body2), room_id, "same DM room from either side");

    // The DM shows up in BOTH users' room lists.
    let (_, alice_rooms) = call(&state, get_auth("/api/rooms", "u_alice", "alice@hf")).await;
    assert!(alice_rooms.contains(&room_id), "DM in Alice's rooms");
    let (_, bob_rooms) = call(&state, get_auth("/api/rooms", "u_bob", "bob@hf")).await;
    assert!(bob_rooms.contains(&room_id), "DM in Bob's rooms");
}

#[tokio::test]
async fn dm_messages_flow_and_third_party_locked_out() {
    let state = build_dev_state();
    let alice = Some(("u_alice", "alice@hf"));
    let bob = Some(("u_bob", "bob@hf"));

    let (_, body) = call(
        &state,
        post_json("/api/dms", r#"{"subject":"u_bob","email":"bob@hf"}"#, alice, Some(CSRF)),
    )
    .await;
    let room_id = extract_room_id(&body);

    // Alice sends into the DM (reuses the ordinary message API + membership check).
    let (status, _) = call(
        &state,
        post_json(
            &format!("/api/rooms/{room_id}/messages"),
            r#"{"body":"hey bob"}"#,
            alice,
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Bob (the other member) can read it.
    let (status, list) = call(
        &state,
        get_auth(&format!("/api/rooms/{room_id}/messages"), "u_bob", "bob@hf"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(list.contains("hey bob"), "peer reads the DM message");

    // A third party is NOT a member and is locked out.
    let (status, _) = call(
        &state,
        get_auth(&format!("/api/rooms/{room_id}/messages"), "u_carol", "carol@hf"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-member -> 403");

    // And cannot send, either.
    let (status, _) = call(
        &state,
        post_json(
            &format!("/api/rooms/{room_id}/messages"),
            r#"{"body":"intrusion"}"#,
            Some(("u_carol", "carol@hf")),
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-member cannot post");
}
