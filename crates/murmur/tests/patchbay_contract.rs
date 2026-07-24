use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use hmac::{Hmac, Mac};
use murmur::config::{Config, CHAT_ORIGIN, DEFAULT_BIND_ADDR};
use murmur::handlers::dashboard::{
    render_boot_json, ComposerViewV1, DashboardViewV1, IdentityViewV1, LedgerOutcomeV1,
    LedgerViewV1, RoomAuthorityV1, RoomBanksViewV1, SelectedRoomViewV1, TapeOutcomeV1, TapeViewV1,
};
use murmur::handlers::{APP_JS, SERVICE_CSS};
use murmur::store::{Message, PgStore};
use murmur::{app, build_dev_state, AppState, ROUTE_SNAPSHOT_V1};
use serde_json::Value;
use sha2::Sha256;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tower::ServiceExt;

const CSRF: &str = "patchbay_contract_csrf";

fn dev_request(method: Method, uri: &str, sub: &str, groups: &str, body: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-auth-subject", sub)
        .header("x-auth-email", format!("{sub}@hf"))
        .header(header::COOKIE, format!("__Host-csrf={CSRF}"));
    if !groups.is_empty() {
        builder = builder.header("x-auth-groups", groups);
    }
    if !body.is_empty() {
        builder = builder
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-csrf-token", CSRF);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

fn form_request(uri: &str, sub: &str, groups: &str, body: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("x-auth-subject", sub)
        .header("x-auth-email", format!("{sub}@hf"))
        .header(header::COOKIE, format!("__Host-csrf={CSRF}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if !groups.is_empty() {
        builder = builder.header("x-auth-groups", groups);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

fn typed_mutation_request(
    uri: &str,
    sub: &str,
    groups: &str,
    content_type: &str,
    body: &str,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("x-auth-subject", sub)
        .header("x-auth-email", format!("{sub}@hf"))
        .header(header::COOKIE, format!("__Host-csrf={CSRF}"))
        .header("x-csrf-token", CSRF)
        .header(header::CONTENT_TYPE, content_type);
    if !groups.is_empty() {
        builder = builder.header("x-auth-groups", groups);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

async fn call(state: &AppState, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

fn assert_private(headers: &HeaderMap) {
    assert_eq!(headers[header::CACHE_CONTROL], "private, no-store");
    assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
}

fn json_id(body: &str, object: &str) -> String {
    serde_json::from_str::<Value>(body).unwrap()[object]["id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn html_input_value(body: &str, name: &str) -> String {
    let marker = format!(r#"name="{name}" value=""#);
    let start = body.find(&marker).expect("input in page") + marker.len();
    let end = body[start..].find('"').expect("input value terminator") + start;
    body[start..end].to_string()
}

fn production_state(key: &str) -> AppState {
    let mut state = build_dev_state();
    state.config = Arc::new(Config {
        bind_addr: DEFAULT_BIND_ADDR.to_string(),
        gateway_hmac_key: Some(key.to_string()),
        websocket_origin: Some(CHAT_ORIGIN.to_string()),
    });
    state
}

fn unavailable_pg_state() -> AppState {
    let options = "postgres://murmur:murmur@127.0.0.1:1/murmur"
        .parse::<PgConnectOptions>()
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_millis(100))
        .connect_lazy_with(options);
    let mut state = build_dev_state();
    state.store = Arc::new(PgStore::from_pool(pool));
    state
}

fn signature(key: &str, subject: &str, groups: &str, minute: i64) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).unwrap();
    mac.update(subject.as_bytes());
    mac.update(b"\n");
    mac.update(groups.as_bytes());
    mac.update(b"\n");
    mac.update(minute.to_string().as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn signed_get(uri: &str, key: &str, groups: &str, minute: i64) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("x-auth-subject", "u_signed")
        .header("x-auth-email", "signed@hf")
        .header("x-auth-groups", groups)
        .header("x-auth-sig", signature(key, "u_signed", groups, minute))
        .body(Body::empty())
        .unwrap()
}

fn signed_ws(key: &str, minute: i64, origin: Option<&str>, groups: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::GET)
        .uri("/ws")
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("x-auth-subject", "u_signed")
        .header("x-auth-email", "signed@hf")
        .header("x-auth-groups", groups)
        .header("x-auth-sig", signature(key, "u_signed", groups, minute));
    if let Some(origin) = origin {
        builder = builder.header(header::ORIGIN, origin);
    }
    builder.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn identity_privacy_and_websocket_origin_are_fail_closed() {
    let key = "patchbay-gateway-key";
    let state = production_state(key);
    let minute = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        / 60;

    let (status, headers, _) = call(
        &state,
        Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get(header::CACHE_CONTROL).is_none());

    let (status, headers, _) = call(
        &state,
        Request::builder()
            .method(Method::POST)
            .uri("/healthz")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_private(&headers);

    let (status, headers, _) = call(
        &state,
        Request::builder().uri("/").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_private(&headers);

    let (status, headers, _) = call(
        &state,
        Request::builder()
            .uri("/")
            .header("x-auth-subject", "u_signed")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_private(&headers);

    let (status, headers, _) = call(&state, signed_get("/", key, "", minute)).await;
    assert_eq!(status, StatusCode::OK);
    assert_private(&headers);
    let (status, headers, _) = call(&state, signed_get("/no-such-route", key, "", minute)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_private(&headers);
    let (status, _, _) = call(&state, signed_get("/", key, "", minute - 1)).await;
    assert_eq!(status, StatusCode::OK);

    let mut tampered = signed_get("/", key, "", minute);
    tampered
        .headers_mut()
        .insert("x-auth-groups", "admins".parse().unwrap());
    let (status, _, _) = call(&state, tampered).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    for origin in [
        None,
        Some("null"),
        Some("https://evil.example"),
        Some("https://chat.w33d.xyz/"),
        Some("not-an-origin"),
    ] {
        let (status, headers, _) = call(&state, signed_ws(key, minute, origin, "")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "origin={origin:?}");
        assert_private(&headers);
    }
    let (status, headers, _) = call(&state, signed_ws(key, minute, Some(CHAT_ORIGIN), "")).await;
    // `tower::oneshot` has no hyper OnUpgrade extension, so a valid-origin request reaches the
    // WebSocket extractor and stops at its harness-only 426 instead of completing a 101.
    assert_eq!(status, StatusCode::UPGRADE_REQUIRED);
    assert_private(&headers);
}

#[tokio::test]
async fn postgres_read_failures_are_unavailable_never_empty_success() {
    let state = unavailable_pg_state();

    let (status, headers, body) = call(
        &state,
        dev_request(Method::GET, "/api/search?q=needle", "u_reader", "", ""),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_private(&headers);
    assert!(body.contains(r#""error":"unavailable""#));
    assert!(!body.contains(r#""results":[]"#));
    assert!(!body.contains("Connection refused"));

    let (status, headers, dashboard) = call(
        &state,
        dev_request(Method::GET, "/?ledger=search&q=needle", "u_reader", "", ""),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_private(&headers);
    assert!(dashboard
        .contains(r#"id="pb-tape-status" role="separator">Murmur is temporarily unavailable"#));
    assert!(dashboard.contains(r#"<p class="pb-ledger__status">Murmur is temporarily unavailable"#));
    assert!(dashboard.contains(r#"class="deck deck--unavailable""#));
    assert!(!dashboard.contains("Connection refused"));
}

#[tokio::test]
async fn json_rejections_are_fixed_safe_400_envelopes() {
    let state = build_dev_state();
    let endpoints = [
        ("/api/rooms", "u_alice", ""),
        ("/api/dms", "u_alice", ""),
        ("/api/rooms/missing/messages/missing/edit", "u_alice", ""),
        ("/api/rooms/missing/messages/missing/react", "u_alice", ""),
        ("/api/rooms/missing/topic", "u_admin", "admins"),
    ];
    for (uri, sub, groups) in endpoints {
        for (content_type, body) in [("application/json", r#"{"broken":"#), ("text/plain", "{}")] {
            let (status, headers, response) = call(
                &state,
                typed_mutation_request(uri, sub, groups, content_type, body),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {response}");
            assert_private(&headers);
            assert!(headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/json"));
            assert_eq!(
                serde_json::from_str::<Value>(&response).unwrap(),
                serde_json::json!({
                    "error": "invalid_request",
                    "message": "invalid JSON body",
                }),
                "{uri} {content_type}"
            );
            assert!(!response.contains("Failed to parse"));
            assert!(!response.contains("Unsupported Media Type"));
        }
    }
}

#[tokio::test]
async fn query_form_and_path_rejections_are_fixed_safe_400_envelopes() {
    let state = build_dev_state();
    let cases = [
        (
            dev_request(
                Method::GET,
                "/api/search?q=ok&before=not-a-number",
                "u_alice",
                "",
                "",
            ),
            "invalid query parameters",
        ),
        (
            dev_request(Method::GET, "/?ledger=not-a-tab", "u_alice", "", ""),
            "invalid query parameters",
        ),
        (
            typed_mutation_request(
                "/admin/rooms/missing/archive",
                "u_admin",
                "admins",
                "application/json",
                "{}",
            ),
            "invalid form body",
        ),
        (
            dev_request(Method::GET, "/api/rooms/%FF/messages", "u_alice", "", ""),
            "invalid path parameters",
        ),
    ];

    for (request, message) in cases {
        let (status, headers, response) = call(&state, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
        assert_private(&headers);
        assert!(headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/json"));
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap(),
            serde_json::json!({
                "error": "invalid_request",
                "message": message,
            })
        );
        assert!(!response.contains("Failed to deserialize"));
        assert!(!response.contains("invalid digit"));
        assert!(!response.contains("Unsupported Media Type"));
    }
}

#[tokio::test]
async fn dashboard_quotes_stay_same_room_and_preview_escapes_once() {
    let state = build_dev_state();
    let (_, _, room_a_body) = call(
        &state,
        dev_request(
            Method::POST,
            "/api/rooms",
            "u_alice",
            "",
            r#"{"name":"room a"}"#,
        ),
    )
    .await;
    let room_a = json_id(&room_a_body, "room");
    let (_, _, room_b_body) = call(
        &state,
        dev_request(
            Method::POST,
            "/api/rooms",
            "u_alice",
            "",
            r#"{"name":"room b"}"#,
        ),
    )
    .await;
    let room_b = json_id(&room_b_body, "room");
    let foreign = Message {
        id: "msg_foreign_parent".to_string(),
        room_id: room_b.clone(),
        sender_sub: "u_alice".to_string(),
        sender_email: "alice@hf".to_string(),
        body: "foreign parent".to_string(),
        created_at: 10,
        edited_at: 0,
        deleted: false,
        reply_to_id: None,
    };
    state.store.create_message(&foreign).await.unwrap();
    let hostile = Message {
        id: "msg_hostile_reply".to_string(),
        room_id: room_a.clone(),
        sender_sub: "u_alice".to_string(),
        sender_email: "alice@hf".to_string(),
        body: "needle <b>x</b> & \"quoted\"".to_string(),
        created_at: 11,
        edited_at: 0,
        deleted: false,
        // Deliberately simulate a legacy/corrupt cross-room parent pointer.
        reply_to_id: Some(foreign.id.clone()),
    };
    state.store.create_message(&hostile).await.unwrap();

    let (status, _, dashboard) = call(
        &state,
        dev_request(
            Method::GET,
            &format!(
                "/?room={room_a}&reply_to={}&ledger=search&q=needle",
                hostile.id
            ),
            "u_alice",
            "",
            "",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!dashboard.contains(r#"data-parent-id="msg_foreign_parent""#));
    assert!(dashboard.contains("needle &lt;b&gt;x&lt;/b&gt; &amp; &quot;quoted&quot;"));
    assert!(!dashboard.contains("&amp;lt;b&amp;gt;"));
    assert!(!dashboard.contains("<b>x</b>"));

    let (status, _, foreign_room_dashboard) = call(
        &state,
        dev_request(Method::GET, &format!("/?room={room_b}"), "u_alice", "", ""),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!foreign_room_dashboard.contains("1 reply"));
}

#[tokio::test]
async fn send_receipts_forms_and_read_marker_use_server_authority() {
    let state = build_dev_state();
    let _ = call(
        &state,
        dev_request(Method::GET, "/api/rooms", "u_alice", "", ""),
    )
    .await;
    let (status, headers, body) = call(
        &state,
        dev_request(
            Method::POST,
            "/api/rooms/lobby/messages",
            "u_alice",
            "",
            r#"{"body":"canonical JSON"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_private(&headers);
    assert!(body.contains(r#""state":"persisted""#));
    let message_id = json_id(&body, "message");
    let (status, _, dashboard) = call(
        &state,
        dev_request(
            Method::GET,
            &format!(
                "/?room=lobby&message={message_id}&reply_to={message_id}&receipt_message={message_id}&ledger=search&q=canonical"
            ),
            "u_alice",
            "",
            "",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(dashboard.contains(&format!(r#"id="msg-{message_id}""#)));
    assert!(dashboard.contains(r#"data-lifecycle="live""#));
    assert!(dashboard.contains(r#"data-own="true""#));
    assert!(!dashboard.contains("data-epoch"));
    assert!(!dashboard.contains("data-position"));

    let (_, _, unavailable) = call(
        &state,
        dev_request(
            Method::GET,
            "/?room=protected-room-sentinel",
            "u_alice",
            "",
            "",
        ),
    )
    .await;
    assert!(unavailable.contains("Room unavailable"));
    assert!(!unavailable.contains("protected-room-sentinel"));

    let (status, headers, _) = call(
        &state,
        form_request(
            "/api/rooms/lobby/messages",
            "u_alice",
            "",
            &format!("body=form+message&csrf={CSRF}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_private(&headers);
    assert!(headers[header::LOCATION]
        .to_str()
        .unwrap()
        .starts_with("/?room=lobby&receipt_message=msg_"));

    let (status, _, body) = call(
        &state,
        form_request(
            "/api/rooms/lobby/messages",
            "u_alice",
            "",
            &format!("body=preserved+draft&reply_to_id=missing&csrf={CSRF}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("preserved draft"));
    assert!(body.contains("Correct and send"));

    let (status, _, body) = call(
        &state,
        form_request(
            "/api/rooms/lobby/messages",
            "u_alice",
            "",
            "body=csrf+draft&csrf=invalid-submitted-token",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("csrf draft"));
    assert!(!body.contains("invalid-submitted-token"));
    assert!(!body.contains(r#"<button type="submit">"#));

    let _ = call(
        &state,
        dev_request(Method::GET, "/api/rooms", "u_bob", "", ""),
    )
    .await;
    let (_, _, dashboard) = call(&state, dev_request(Method::GET, "/", "u_bob", "", "")).await;
    assert!(dashboard.contains(r#"class="jack__mark--unread""#));
    assert!(dashboard.contains(">Unread</span>"));
    assert!(!dashboard.contains(r#"class="jack__mark--unread">1</span>"#));
    let (status, _, body) = call(
        &state,
        dev_request(
            Method::POST,
            "/api/rooms/lobby/read?at=9999999999",
            "u_bob",
            "",
            &format!(r#"{{"message_id":"{message_id}"}}"#),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&message_id));
    assert!(!body.contains("9999999999"));

    let (status, _, _) = call(
        &state,
        dev_request(
            Method::POST,
            "/api/rooms/lobby/read?at=9999999999",
            "u_bob",
            "",
            "{}",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (_, _, room_body) = call(
        &state,
        dev_request(
            Method::POST,
            "/api/rooms",
            "u_alice",
            "",
            r#"{"name":"private"}"#,
        ),
    )
    .await;
    let room_id = json_id(&room_body, "room");
    let (_, _, private_message) = call(
        &state,
        dev_request(
            Method::POST,
            &format!("/api/rooms/{room_id}/messages"),
            "u_alice",
            "",
            r#"{"body":"private"}"#,
        ),
    )
    .await;
    let private_message_id = json_id(&private_message, "message");
    let (status, _, _) = call(
        &state,
        dev_request(
            Method::POST,
            "/api/rooms/lobby/read",
            "u_bob",
            "",
            &format!(r#"{{"message_id":"{private_message_id}"}}"#),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn archived_room_matrix_and_hub_generation_are_enforced() {
    let state = build_dev_state();
    let _ = call(
        &state,
        dev_request(Method::GET, "/api/rooms", "u_admin", "admins", ""),
    )
    .await;
    let (_, _, room_body) = call(
        &state,
        dev_request(
            Method::POST,
            "/api/rooms",
            "u_admin",
            "admins",
            r#"{"name":"archive matrix"}"#,
        ),
    )
    .await;
    let room_id = json_id(&room_body, "room");
    for user in ["u_bob", "u_carol"] {
        let (status, _, _) = call(
            &state,
            dev_request(
                Method::POST,
                &format!("/api/rooms/{room_id}/join"),
                user,
                "",
                "{}",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (_, _, message_body) = call(
        &state,
        dev_request(
            Method::POST,
            &format!("/api/rooms/{room_id}/messages"),
            "u_admin",
            "admins",
            r#"{"body":"archive searchable marker"}"#,
        ),
    )
    .await;
    let message_id = json_id(&message_body, "message");
    let (status, _, _) = call(
        &state,
        dev_request(
            Method::POST,
            &format!("/api/rooms/{room_id}/messages/{message_id}/pin"),
            "u_admin",
            "admins",
            "{}",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let generation = state.hub.generation(&room_id);
    let (status, headers, _) = call(
        &state,
        form_request(
            &format!("/admin/rooms/{room_id}/archive"),
            "u_admin",
            "admins",
            &format!("csrf={CSRF}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_private(&headers);
    assert_eq!(state.hub.generation(&room_id), generation + 1);

    for uri in [
        format!("/api/rooms/{room_id}/messages"),
        format!("/api/rooms/{room_id}/pinned"),
        "/api/search?q=archive".to_string(),
        "/api/mentions".to_string(),
    ] {
        let (status, _, _) = call(
            &state,
            dev_request(Method::GET, &uri, "u_admin", "admins", ""),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
    let (status, _, _) = call(
        &state,
        dev_request(
            Method::POST,
            &format!("/api/rooms/{room_id}/read"),
            "u_admin",
            "admins",
            &format!(r#"{{"message_id":"{message_id}"}}"#),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let mutations = [
        (
            format!("/api/rooms/{room_id}/messages"),
            r#"{"body":"blocked"}"#,
            "u_admin",
            "admins",
        ),
        (
            format!("/api/rooms/{room_id}/messages/{message_id}/edit"),
            r#"{"body":"blocked"}"#,
            "u_admin",
            "admins",
        ),
        (
            format!("/api/rooms/{room_id}/messages/{message_id}/delete"),
            "{}",
            "u_admin",
            "admins",
        ),
        (
            format!("/api/rooms/{room_id}/messages/{message_id}/react"),
            r#"{"emoji":"👍"}"#,
            "u_admin",
            "admins",
        ),
        (
            format!("/api/rooms/{room_id}/messages/{message_id}/pin"),
            "{}",
            "u_admin",
            "admins",
        ),
        (
            format!("/api/rooms/{room_id}/messages/{message_id}/unpin"),
            "{}",
            "u_admin",
            "admins",
        ),
        (
            format!("/api/rooms/{room_id}/topic"),
            r#"{"topic":"blocked"}"#,
            "u_admin",
            "admins",
        ),
        (format!("/api/rooms/{room_id}/join"), "{}", "u_new", ""),
    ];
    for (uri, body, sub, groups) in mutations {
        let (status, _, response) =
            call(&state, dev_request(Method::POST, &uri, sub, groups, body)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{uri}: {response}");
        assert!(response.contains(r#""error":"room_archived""#));
    }

    let before_remove = state.hub.generation(&room_id);
    let (status, _, _) = call(
        &state,
        form_request(
            &format!("/admin/rooms/{room_id}/members/u_bob/remove"),
            "u_admin",
            "admins",
            &format!("csrf={CSRF}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(state.hub.generation(&room_id), before_remove + 1);

    let before_ban = state.hub.generation(&room_id);
    let (status, _, _) = call(
        &state,
        form_request(
            &format!("/admin/rooms/{room_id}/members/u_carol/ban"),
            "u_admin",
            "admins",
            &format!("csrf={CSRF}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(state.hub.generation(&room_id), before_ban + 1);

    let (status, _, detail) = call(
        &state,
        dev_request(
            Method::GET,
            &format!("/admin/rooms/{room_id}"),
            "u_admin",
            "admins",
            "",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let consequence_token = html_input_value(&detail, "consequence_token");

    let before_delete = state.hub.generation(&room_id);
    let (status, _, _) = call(
        &state,
        form_request(
            &format!("/admin/rooms/{room_id}/delete"),
            "u_admin",
            "admins",
            &format!("csrf={CSRF}&confirm=delete&consequence_token={consequence_token}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(state.hub.generation(&room_id), before_delete + 1);

    for uri in ["/admin/rooms/lobby/archive", "/admin/rooms/lobby/delete"] {
        let body = if uri.ends_with("/delete") {
            format!("csrf={CSRF}&confirm=delete")
        } else {
            format!("csrf={CSRF}")
        };
        let (status, _, _) = call(&state, form_request(uri, "u_admin", "admins", &body)).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }
}

#[tokio::test]
async fn archived_room_frontend_enforces_read_only_parity() {
    let state = build_dev_state();
    let _ = call(
        &state,
        dev_request(Method::GET, "/api/rooms", "u_admin", "admins", ""),
    )
    .await;
    let (_, _, room_body) = call(
        &state,
        dev_request(
            Method::POST,
            "/api/rooms",
            "u_admin",
            "admins",
            r#"{"name":"parity room"}"#,
        ),
    )
    .await;
    let room_id = json_id(&room_body, "room");
    let (_, _, message_body) = call(
        &state,
        dev_request(
            Method::POST,
            &format!("/api/rooms/{room_id}/messages"),
            "u_admin",
            "admins",
            r#"{"body":"parity marker"}"#,
        ),
    )
    .await;
    let message_id = json_id(&message_body, "message");
    let (status, _, _) = call(
        &state,
        dev_request(
            Method::POST,
            &format!("/api/rooms/{room_id}/messages/{message_id}/react"),
            "u_admin",
            "admins",
            r#"{"emoji":"👍"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, _) = call(
        &state,
        form_request(
            &format!("/admin/rooms/{room_id}/archive"),
            "u_admin",
            "admins",
            &format!("csrf={CSRF}"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // SSR keeps the frozen row shape (tools + chips stay in the markup); the read-only rule
    // is enforced by the deck state, the disabled composer, and the frontend layers pinned
    // below — never by server rejection alone.
    let (status, headers, dashboard) = call(
        &state,
        dev_request(
            Method::GET,
            &format!("/?room={room_id}"),
            "u_admin",
            "admins",
            "",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_private(&headers);
    assert!(dashboard.contains(r#"class="deck deck--archived""#));
    assert!(dashboard.contains(r#""authority":"archived_read_only""#));
    assert!(dashboard.contains(r#"data-room-state="archived""#));
    assert!(!dashboard.contains(r#"data-room-state="archived_read_only""#));
    assert!(dashboard.contains(r#"aria-describedby="pb-cue-budget" disabled>"#));
    assert!(dashboard.contains(r#"id="pb-cue-send" type="submit" disabled>"#));
    assert!(dashboard.contains(&format!(r#"id="msg-{message_id}""#)));
    // SSR-exact markup: the inlined app.js never contains these HTML strings.
    assert!(dashboard
        .contains(r#"<button type="button" class="msg__tool" data-act="react">React</button>"#));
    assert!(
        dashboard.contains(r#"<button type="button" class="reaction is-mine" data-emoji="👍">"#)
    );

    // Removing any of these guards regresses the UI to server-only rejection (live write
    // controls in an archived room), so the exact guard expressions are pinned against the
    // embedded production assets: the JS tool builder emits nothing unless active, the chip
    // delegation returns before any mutation, SSR hydration adds no Edit/Delete, and the
    // archived-deck CSS backstop hides SSR tools and chips.
    assert!(APP_JS.contains(r#"if (selectedAuthority !== "active") return tools;"#));
    assert!(APP_JS.contains(r#"if (selectedAuthority !== "active") return;"#));
    assert!(APP_JS.contains(r#"lifecycle === "live" && selectedAuthority === "active""#));
    assert!(APP_JS.contains(r#"var authorityToken = rows[i].getAttribute("data-room-state");"#));
    assert!(APP_JS.contains(r#"authorityToken === "archived" ? "archived" :"#));
    assert!(APP_JS.contains(r#"authorityToken === "active" ? "active" : "unavailable""#));
    assert!(APP_JS.contains(r#"function markSelectedUnavailable()"#));
    assert!(SERVICE_CSS.contains(".deck--archived .msg__tools"));
    assert!(SERVICE_CSS.contains(".deck--archived .reaction"));

    // Active rooms are untouched: no archived deck state and the composer stays enabled.
    let (status, _, active) = call(
        &state,
        dev_request(Method::GET, "/", "u_admin", "admins", ""),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(active.contains(r#"class="deck" id="pb-deck""#));
    assert!(active.contains(r#"aria-describedby="pb-cue-budget">"#));
    assert!(active.contains(r#"id="pb-cue-send" type="submit">"#));
}

#[test]
fn ledger_responses_are_bound_to_the_latest_tab_room_and_query() {
    assert!(APP_JS.contains("var ledgerEpoch = 0;"));
    assert!(APP_JS.contains("var requestLedgerEpoch = ledgerEpoch;"));
    assert!(APP_JS.contains("requestLedgerEpoch !== ledgerEpoch || ledgerOpenTab !== requestTab"));
    assert!(APP_JS.contains(r#"requestTab === "pins" && selected !== requestRoom"#));
    assert!(APP_JS.contains(
        r#"requestTab === "search" && ledgerQ && ledgerQ.value.trim() !== requestQuery"#
    ));
    assert!(APP_JS.contains("if (ledgerRequestIsStale()) return;"));
}

#[test]
fn read_badges_and_local_locators_obey_selection_epochs() {
    assert!(APP_JS.contains("var roomActivityGeneration = Object.create(null);"));
    assert!(APP_JS.contains("var roomReadSequence = Object.create(null);"));
    assert!(APP_JS.contains("roomReadSequence[roomId] === readSequence"));
    assert!(APP_JS.contains("currentRoomActivity(roomId) === activity"));
    assert!(APP_JS.contains("var roomsSnapshotActivity = roomActivityClock;"));
    assert!(APP_JS.contains("currentRoomActivity(room.id) <= roomsSnapshotActivity"));
    assert!(APP_JS.contains("if (frame.room_id) noteRoomActivity(frame.room_id);"));
    assert!(APP_JS.contains(
        "function bumpUnread(roomId, mention) {\n    var li = jackRow(roomId);\n    if (!li) return;\n    var unread = li.querySelector(\".room__unread, .jack__mark--unread\");\n    if (unread) {\n      unread.hidden = false;"
    ));
    assert!(APP_JS.contains(
        "function locateRow(msgId) {\n    var row = rowFor(msgId);\n    if (!row) return false;\n    selectionEpoch++;"
    ));
}

#[test]
fn route_snapshot_is_exactly_frozen() {
    assert_eq!(ROUTE_SNAPSHOT_V1.len(), 28);
    assert_eq!(ROUTE_SNAPSHOT_V1[0], ("GET", "/healthz"));
    assert_eq!(
        ROUTE_SNAPSHOT_V1[27],
        ("POST", "/admin/messages/{msg_id}/redact")
    );
    assert!(!ROUTE_SNAPSHOT_V1
        .iter()
        .any(|(_, path)| path.contains("cursor") || path.contains("reconcile")));
}

#[test]
fn boot_json_is_exact_bounded_and_script_safe() {
    let view = DashboardViewV1 {
        identity: IdentityViewV1 {
            display_email: "</script><img src=x onerror=1>".to_string(),
        },
        room_banks: RoomBanksViewV1 {
            active: Vec::new(),
            archived: Vec::new(),
        },
        selected: SelectedRoomViewV1 {
            room_id: "lobby".to_string(),
            name: "#lobby".to_string(),
            topic: String::new(),
            authority: RoomAuthorityV1::Active,
            kind: "room".to_string(),
            available: true,
        },
        tape: TapeViewV1 {
            outcome: TapeOutcomeV1::Empty,
            messages: Vec::new(),
            located_message_id: None,
            locator_copy: None,
        },
        composer: ComposerViewV1 {
            enabled: true,
            reply_to_id: None,
            reply_context: None,
        },
        ledger: LedgerViewV1 {
            tab: None,
            outcome: LedgerOutcomeV1::Empty,
            items: Vec::new(),
            query: None,
        },
        receipt: None,
    };
    let boot = render_boot_json(&view, "</script>", false);
    assert!(!boot.contains("</script>"));
    assert!(boot.contains(r#"\u003c/script\u003e"#));
    assert!(boot.contains(r#""messagePage":50"#));
    assert!(boot.contains(r#""transport":{"initial":"unknown"}"#));
    assert!(!boot.contains("epoch"));
    assert!(!boot.contains("position"));
    assert!(!boot.contains("cursor"));
}
