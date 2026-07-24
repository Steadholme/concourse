//! End-to-end HTTP flow for the Slack/Discord-parity additions (in-memory store, NO database).
//!
//! Drives the real `app` router via `tower::oneshot`, mirroring `chat_flow.rs`. Covers:
//! - room topic edit gate (mods only) + the topic rendered into the room header (escaped);
//! - pin / unpin gate (mods only) + the pinned panel + membership-gated pinned read;
//! - message search strictly scoped to the caller's member rooms (a non-member never sees content);
//! - @mention parsing + the per-room unread / mention markers + the global mentions view.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use murmur::store::{Message, Room};
use murmur::{app, build_dev_state, AppState};
use tower::ServiceExt;

const CSRF: &str = "tok_csrf_for_tests";

/// Authenticated GET, optionally carrying an `X-Auth-Groups` header (for the admin gate).
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

/// JSON POST with an always-present CSRF cookie; optional groups + `X-CSRF-Token` header.
fn post(
    path: &str,
    json: &str,
    sub: &str,
    email: &str,
    groups: Option<&str>,
    csrf_header: Option<&str>,
) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, format!("__Host-csrf={CSRF}"))
        .header("x-auth-subject", sub)
        .header("x-auth-email", email);
    if let Some(g) = groups {
        b = b.header("x-auth-groups", g);
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

#[tokio::test]
async fn tuple_cursor_http_paginates_same_second_messages() {
    let state = build_dev_state();
    let room = Room {
        id: "room_cursor_http".to_string(),
        name: "#cursor".to_string(),
        kind: "room".to_string(),
        created_by: "u_alice".to_string(),
        created_at: 1,
        archived: false,
        topic: String::new(),
    };
    state.store.ensure_room(&room).await.unwrap();
    state
        .store
        .ensure_membership(&room.id, "u_bob", "bob@hf", 1)
        .await
        .unwrap();
    for index in 0..51 {
        state
            .store
            .create_message(&Message {
                id: format!("msg_http_{index:03}"),
                room_id: room.id.clone(),
                sender_sub: "u_alice".to_string(),
                sender_email: "alice@hf".to_string(),
                body: "same second".to_string(),
                created_at: 100,
                edited_at: 0,
                deleted: false,
                reply_to_id: None,
            })
            .await
            .unwrap();
    }

    let (status, first_body) = call(
        &state,
        get_auth(
            "/api/rooms/room_cursor_http/messages",
            "u_bob",
            "bob@hf",
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let first: serde_json::Value = serde_json::from_str(&first_body).unwrap();
    assert_eq!(first["messages"].as_array().unwrap().len(), 50);
    let cursor = first["next_cursor"].as_str().expect("next cursor");

    let (status, second_body) = call(
        &state,
        get_auth(
            &format!("/api/rooms/room_cursor_http/messages?before={cursor}"),
            "u_bob",
            "bob@hf",
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let second: serde_json::Value = serde_json::from_str(&second_body).unwrap();
    assert_eq!(second["messages"].as_array().unwrap().len(), 1);
    assert_eq!(second["messages"][0]["id"], "msg_http_000");
    assert!(second["next_cursor"].is_null());
}

// ---------------------------------------------------------------------------
// Room topic
// ---------------------------------------------------------------------------

#[tokio::test]
async fn topic_edit_is_mod_gated_and_rendered() {
    let state = build_dev_state();
    // Alice touches the API so the lobby exists + she is a member.
    let _ = call(&state, get_auth("/api/rooms", "u_alice", "alice@hf", None)).await;

    // A signed-in non-admin cannot set a topic (valid CSRF, but no admin group -> 403).
    let (status, _) = call(
        &state,
        post(
            "/api/rooms/lobby/topic",
            r#"{"topic":"Daily standup"}"#,
            "u_alice",
            "alice@hf",
            None,
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-mod topic edit -> 403");

    // An admin with a bad CSRF token is rejected before any write.
    let (status, _) = call(
        &state,
        post(
            "/api/rooms/lobby/topic",
            r#"{"topic":"Daily standup"}"#,
            "u_adm",
            "adm@hf",
            Some("admins"),
            Some("WRONG"),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "mod topic edit bad CSRF -> 403"
    );

    // An admin sets the topic (note the embedded markup, to prove escaping on render).
    let (status, body) = call(
        &state,
        post(
            "/api/rooms/lobby/topic",
            r#"{"topic":"Daily <b>standup</b>"}"#,
            "u_adm",
            "adm@hf",
            Some("admins"),
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "mod topic edit -> 200");
    assert!(
        body.contains("Daily <b>standup</b>"),
        "topic echoed in JSON (client escapes)"
    );

    // The dashboard renders the topic in the room header — ESCAPED, never as live markup.
    let (status, page) = call(&state, get_auth("/", "u_alice", "alice@hf", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains(r#"id="pb-room-topic""#),
        "topic element present"
    );
    assert!(
        page.contains("Daily &lt;b&gt;standup&lt;/b&gt;"),
        "topic escaped in header"
    );
    assert!(
        !page.contains("Daily <b>standup</b>"),
        "raw topic markup must not survive"
    );
}

#[tokio::test]
async fn topic_on_missing_room_is_404() {
    let state = build_dev_state();
    let (status, _) = call(
        &state,
        post(
            "/api/rooms/nope/topic",
            r#"{"topic":"x"}"#,
            "u_adm",
            "adm@hf",
            Some("admins"),
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "topic on unknown room -> 404"
    );
}

// ---------------------------------------------------------------------------
// Pinned messages
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pin_unpin_gate_and_panel() {
    let state = build_dev_state();
    let _ = call(&state, get_auth("/api/rooms", "u_bob", "bob@hf", None)).await;

    // Bob posts a message we will pin.
    let (status, body) = call(
        &state,
        post(
            "/api/rooms/lobby/messages",
            r#"{"body":"PINME distinctive body"}"#,
            "u_bob",
            "bob@hf",
            None,
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let msg_id = extract_id(&body);

    // A non-admin cannot pin (valid CSRF, no admin group -> 403).
    let (status, _) = call(
        &state,
        post(
            &format!("/api/rooms/lobby/messages/{msg_id}/pin"),
            "{}",
            "u_bob",
            "bob@hf",
            None,
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-mod pin -> 403");

    // Admin pin with a bad CSRF token -> 403.
    let (status, _) = call(
        &state,
        post(
            &format!("/api/rooms/lobby/messages/{msg_id}/pin"),
            "{}",
            "u_adm",
            "adm@hf",
            Some("admins"),
            Some("WRONG"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "mod pin bad CSRF -> 403");

    // Admin pins it -> 200, the returned panel lists the message.
    let (status, body) = call(
        &state,
        post(
            &format!("/api/rooms/lobby/messages/{msg_id}/pin"),
            "{}",
            "u_adm",
            "adm@hf",
            Some("admins"),
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "mod pin -> 200");
    assert!(body.contains("\"pinned\":true"));
    assert!(body.contains(&msg_id), "pinned message id in panel");

    // A member reads the pinned panel and sees the message.
    let (status, panel) = call(
        &state,
        get_auth("/api/rooms/lobby/pinned", "u_bob", "bob@hf", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        panel.contains("PINME distinctive body"),
        "pinned body in panel read"
    );

    // The dashboard server-renders pins through the bounded Patch Ledger view.
    let (_, page) = call(
        &state,
        get_auth(
            "/?room=lobby&ledger=pins",
            "u_adm",
            "adm@hf",
            Some("admins"),
        ),
    )
    .await;
    assert!(
        page.contains("class=\"pb-ledger__item\""),
        "pinned ledger item rendered"
    );
    assert!(page.contains(&msg_id));

    // Admin unpins it -> the panel is empty again.
    let (status, body) = call(
        &state,
        post(
            &format!("/api/rooms/lobby/messages/{msg_id}/unpin"),
            "{}",
            "u_adm",
            "adm@hf",
            Some("admins"),
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"pinned\":false"));
    assert!(!body.contains(&msg_id), "unpinned message gone from panel");
}

#[tokio::test]
async fn pinned_read_is_membership_scoped() {
    let state = build_dev_state();
    // Bob creates a private room and pins a message in it.
    let (_, body) = call(
        &state,
        post(
            "/api/rooms",
            r#"{"name":"vault","kind":"room"}"#,
            "u_bob",
            "bob@hf",
            None,
            Some(CSRF),
        ),
    )
    .await;
    let room_id = extract_id(&body);
    let (_, body) = call(
        &state,
        post(
            &format!("/api/rooms/{room_id}/messages"),
            r#"{"body":"vault secret"}"#,
            "u_bob",
            "bob@hf",
            None,
            Some(CSRF),
        ),
    )
    .await;
    let msg_id = extract_id(&body);
    let (status, _) = call(
        &state,
        post(
            &format!("/api/rooms/{room_id}/messages/{msg_id}/pin"),
            "{}",
            "u_adm",
            "adm@hf",
            Some("admins"),
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A non-member cannot read the room's pinned panel.
    let (status, _) = call(
        &state,
        get_auth(
            &format!("/api/rooms/{room_id}/pinned"),
            "u_carol",
            "carol@hf",
            None,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "non-member pinned read -> generic 404"
    );
}

// ---------------------------------------------------------------------------
// Message formatting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn formatting_renders_and_escapes() {
    let state = build_dev_state();
    // Alice + Bob both auto-join the lobby so @bob is a normal in-room mention target.
    let _ = call(&state, get_auth("/api/rooms", "u_alice", "alice@hf", None)).await;
    let _ = call(&state, get_auth("/api/rooms", "u_bob", "bob@hf", None)).await;

    let body = "Here is `code`, *bold*, _italic_, and ~strike~\n\
> quote\n\
@bob please review\n\
```rust\n\
<script>alert(1)</script>\n\
https://example.com\n\
_snake_\n\
```\n\
`\"><img src=x onerror=alert(1)>`";
    let json = format!(r#"{{"body":{body:?}}}"#);
    let (status, resp) = call(
        &state,
        post(
            "/api/rooms/lobby/messages",
            &json,
            "u_alice",
            "alice@hf",
            None,
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "formatted send failed: {resp}");

    let (status, page) = call(&state, get_auth("/", "u_alice", "alice@hf", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains(r#"<pre><code class="lang-rust">"#),
        "fenced block rendered"
    );
    assert!(page.contains("<code>code</code>"), "inline code rendered");
    assert!(page.contains("<strong>bold</strong>"), "bold rendered");
    assert!(page.contains("<em>italic</em>"), "italic rendered");
    assert!(page.contains("<del>strike</del>"), "strike rendered");
    assert!(
        page.contains(r#"<span class="mention">@bob</span>"#),
        "mention chip rendered"
    );
    assert!(
        page.contains("<blockquote>quote</blockquote>"),
        "quote rendered"
    );
    assert!(
        page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "script payload escaped"
    );
    assert!(
        page.contains("&quot;&gt;&lt;img src=x onerror=alert(1)&gt;"),
        "img payload escaped"
    );
    assert!(!page.contains("<img"), "img payload must not become markup");
    assert!(
        !page.contains("<script>alert(1)</script>"),
        "script payload must not become markup"
    );

    // Render-only guard: the membership gate is unchanged for rooms Alice did not join.
    let (status, body) = call(
        &state,
        post(
            "/api/rooms",
            r#"{"name":"format-vault","kind":"room"}"#,
            "u_bob",
            "bob@hf",
            None,
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let room_id = extract_id(&body);
    let (status, _) = call(
        &state,
        get_auth(
            &format!("/api/rooms/{room_id}/messages"),
            "u_alice",
            "alice@hf",
            None,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "non-member room read -> generic 404"
    );
}

// ---------------------------------------------------------------------------
// Search — strictly membership-scoped
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_is_scoped_to_member_rooms() {
    let state = build_dev_state();
    // Alice + Bob both auto-join the lobby.
    let _ = call(&state, get_auth("/api/rooms", "u_alice", "alice@hf", None)).await;
    let _ = call(&state, get_auth("/api/rooms", "u_bob", "bob@hf", None)).await;

    // Bob creates a PRIVATE room Alice is not in, and posts a distinctive message there.
    let (_, body) = call(
        &state,
        post(
            "/api/rooms",
            r#"{"name":"vault","kind":"room"}"#,
            "u_bob",
            "bob@hf",
            None,
            Some(CSRF),
        ),
    )
    .await;
    let vault = extract_id(&body);
    let _ = call(
        &state,
        post(
            &format!("/api/rooms/{vault}/messages"),
            r#"{"body":"secret sauce recipe"}"#,
            "u_bob",
            "bob@hf",
            None,
            Some(CSRF),
        ),
    )
    .await;
    // Alice posts a public message in the lobby.
    let _ = call(
        &state,
        post(
            "/api/rooms/lobby/messages",
            r#"{"body":"public lobby note"}"#,
            "u_alice",
            "alice@hf",
            None,
            Some(CSRF),
        ),
    )
    .await;

    // Alice searching for the vault content finds NOTHING (she is not a member of the vault).
    let (status, results) = call(
        &state,
        get_auth("/api/search?q=secret", "u_alice", "alice@hf", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !results.contains("secret sauce recipe"),
        "non-member must not see vault content"
    );
    assert!(
        results.contains("\"results\":[]"),
        "no hits for a non-member"
    );

    // Bob (a member of the vault) DOES find it.
    let (status, results) = call(
        &state,
        get_auth("/api/search?q=SECRET", "u_bob", "bob@hf", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        results.contains("secret sauce recipe"),
        "member finds their room's message (case-insensitive)"
    );

    // Alice finds her own lobby message.
    let (_, results) = call(
        &state,
        get_auth("/api/search?q=public", "u_alice", "alice@hf", None),
    )
    .await;
    assert!(
        results.contains("public lobby note"),
        "member finds lobby content"
    );

    // A too-short query is rejected.
    let (status, _) = call(
        &state,
        get_auth("/api/search?q=a", "u_alice", "alice@hf", None),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "1-char query -> 400");
}

// ---------------------------------------------------------------------------
// @mentions + unread marker math
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mentions_and_unread_marker_math() {
    let state = build_dev_state();
    // Alice + Bob both auto-join the lobby (so Bob is a resolvable mention target).
    let _ = call(&state, get_auth("/api/rooms", "u_alice", "alice@hf", None)).await;
    let _ = call(&state, get_auth("/api/rooms", "u_bob", "bob@hf", None)).await;

    // Alice @mentions Bob (his email local-part is "bob").
    let (_, mention_send) = call(
        &state,
        post(
            "/api/rooms/lobby/messages",
            r#"{"body":"hey @bob please review"}"#,
            "u_alice",
            "alice@hf",
            None,
            Some(CSRF),
        ),
    )
    .await;
    let mut last_message_id = extract_id(&mention_send);

    // Bob's room list shows the lobby unread (1, from Alice) and the mention marker lit.
    let (status, rooms) = call(&state, get_auth("/api/rooms", "u_bob", "bob@hf", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(rooms.contains("\"unread\":1"), "one unread from Alice");
    assert!(rooms.contains("\"mentioned\":true"), "mention marker lit");

    // The global mentions view carries the mentioning message + its room name.
    let (status, mentions) = call(&state, get_auth("/api/mentions", "u_bob", "bob@hf", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        mentions.contains("hey @bob please review"),
        "mention message present"
    );
    assert!(
        mentions.contains("#lobby"),
        "mention tagged with its room name"
    );

    // Alice's OWN room list never counts her own posts as unread.
    let (_, alice_rooms) = call(&state, get_auth("/api/rooms", "u_alice", "alice@hf", None)).await;
    assert!(
        alice_rooms.contains("\"unread\":0"),
        "own posts do not mark unread"
    );

    // Alice posts two more plain messages -> Bob's unread climbs to 3.
    for _ in 0..2 {
        let (_, sent) = call(
            &state,
            post(
                "/api/rooms/lobby/messages",
                r#"{"body":"another update"}"#,
                "u_alice",
                "alice@hf",
                None,
                Some(CSRF),
            ),
        )
        .await;
        last_message_id = extract_id(&sent);
    }
    let (_, rooms) = call(&state, get_auth("/api/rooms", "u_bob", "bob@hf", None)).await;
    assert!(
        rooms.contains("\"unread\":3"),
        "unread reflects all of Alice's messages"
    );

    // Bob opens the room (advances his read cursor) -> unread + mention markers clear.
    let (status, _) = call(
        &state,
        post(
            "/api/rooms/lobby/read",
            &format!(r#"{{"message_id":"{last_message_id}"}}"#),
            "u_bob",
            "bob@hf",
            None,
            Some(CSRF),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, rooms) = call(&state, get_auth("/api/rooms", "u_bob", "bob@hf", None)).await;
    assert!(rooms.contains("\"unread\":0"), "read cursor clears unread");
    assert!(
        rooms.contains("\"mentioned\":false"),
        "read cursor clears mention marker"
    );
}
