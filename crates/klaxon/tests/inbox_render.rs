//! Signal Muster render-contract tests. These drive the real Router and both success/failure Store
//! seams without a database or browser.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use klaxon::audit::AuditSink;
use klaxon::config::Config;
use klaxon::store::{
    InMemoryStore, InboxListSnapshot, Notification, NotifyPrefs, PgStore, PushSubscription, Store,
    StoreError, Webhook,
};
use klaxon::{app, AppState};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

const SUBJECT: &str = "u_signal_muster";
const EMAIL: &str = "muster@w33d.xyz";

struct ControlledStore {
    inner: InMemoryStore,
    fail_list: bool,
    fail_prefs: bool,
}

impl ControlledStore {
    fn new(fail_list: bool, fail_prefs: bool) -> Self {
        Self {
            inner: InMemoryStore::new(),
            fail_list,
            fail_prefs,
        }
    }
}

#[async_trait]
impl Store for ControlledStore {
    async fn create_notification(&self, n: &Notification) -> Result<(), StoreError> {
        self.inner.create_notification(n).await
    }

    async fn list_notifications(&self, keys: &[String]) -> Vec<Notification> {
        self.inner.list_notifications(keys).await
    }

    async fn list_notifications_checked(
        &self,
        keys: &[String],
    ) -> Result<InboxListSnapshot, StoreError> {
        if self.fail_list {
            Err(StoreError::Backend(
                "postgres://secret@db.internal/raw-list-failure".to_string(),
            ))
        } else {
            self.inner.list_notifications_checked(keys).await
        }
    }

    async fn list_since(&self, keys: &[String], since: i64) -> Vec<Notification> {
        self.inner.list_since(keys, since).await
    }

    async fn mark_read(
        &self,
        keys: &[String],
        id: Option<&str>,
        now: i64,
    ) -> Result<u64, StoreError> {
        self.inner.mark_read(keys, id, now).await
    }

    async fn delete_notification(&self, keys: &[String], id: &str) -> Result<u64, StoreError> {
        self.inner.delete_notification(keys, id).await
    }

    async fn unread_count(&self, keys: &[String]) -> i64 {
        self.inner.unread_count(keys).await
    }

    async fn upsert_subscription(&self, sub: &PushSubscription) -> Result<(), StoreError> {
        self.inner.upsert_subscription(sub).await
    }

    async fn list_subscriptions(&self, key: &str) -> Vec<PushSubscription> {
        self.inner.list_subscriptions(key).await
    }

    async fn delete_subscription(&self, key: &str, endpoint: &str) -> Result<u64, StoreError> {
        self.inner.delete_subscription(key, endpoint).await
    }

    async fn create_webhook(&self, hook: &Webhook) -> Result<(), StoreError> {
        self.inner.create_webhook(hook).await
    }

    async fn list_webhooks(&self, key: &str) -> Vec<Webhook> {
        self.inner.list_webhooks(key).await
    }

    async fn delete_webhook(&self, key: &str, id: &str) -> Result<u64, StoreError> {
        self.inner.delete_webhook(key, id).await
    }

    async fn get_prefs(&self, key: &str) -> NotifyPrefs {
        self.inner.get_prefs(key).await
    }

    async fn get_prefs_checked(&self, key: &str) -> Result<NotifyPrefs, StoreError> {
        if self.fail_prefs {
            Err(StoreError::Backend(
                "postgres://secret@db.internal/raw-prefs-failure".to_string(),
            ))
        } else {
            self.inner.get_prefs_checked(key).await
        }
    }

    async fn set_prefs(&self, prefs: &NotifyPrefs) -> Result<(), StoreError> {
        self.inner.set_prefs(prefs).await
    }
}

fn state(store: Arc<dyn Store>) -> AppState {
    AppState {
        config: Arc::new(Config::dev()),
        store,
        audit: AuditSink::disabled(),
    }
}

fn notification(
    id: &str,
    source: &str,
    severity: &str,
    title: &str,
    created_at: i64,
) -> Notification {
    Notification {
        id: id.to_string(),
        user_sub: SUBJECT.to_string(),
        source: source.to_string(),
        severity: severity.to_string(),
        title: title.to_string(),
        body: String::new(),
        url: String::new(),
        created_at,
        read_at: 0,
    }
}

async fn get(st: &AppState, uri: &str) -> (StatusCode, String) {
    let response = app(st.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("x-auth-subject", SUBJECT)
                .header("x-auth-email", EMAIL)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 8 << 20)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn occurrences(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

fn rendered_href_containing(body: &str, needle: &str) -> String {
    let encoded = body
        .split(r#"href=""#)
        .skip(1)
        .filter_map(|tail| tail.split('"').next())
        .find(|href| href.contains(needle))
        .unwrap_or_else(|| panic!("no rendered href contains {needle:?}"));
    encoded.replace("&amp;", "&")
}

fn fragment_between<'a>(body: &'a str, start: &str, end: &str) -> &'a str {
    let start_index = body
        .find(start)
        .unwrap_or_else(|| panic!("missing fragment start {start:?}"));
    let tail = &body[start_index..];
    let end_index = tail
        .find(end)
        .unwrap_or_else(|| panic!("missing fragment end {end:?}"));
    &tail[..end_index + end.len()]
}

#[tokio::test]
async fn checked_snapshot_uses_exact_201st_row_evidence_and_stable_order() {
    let store = Arc::new(ControlledStore::new(false, false));
    for index in 0..201 {
        store
            .create_notification(&notification(
                &format!("n-{index:03}"),
                "loom",
                "info",
                &format!("TITLE-{index:03}"),
                index,
            ))
            .await
            .unwrap();
    }
    let st = state(store);
    let (status, body) = get(&st, "/").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(occurrences(&body, r#"<li class="signal-entry"#), 200);
    assert!(
        body.contains(r#"<ol class="muster-list" aria-label="Signals"><li class="signal-entry"#)
    );
    assert!(body.contains("Showing up to 200 signals from this bounded view"));
    assert!(body.contains(r#"<span class="count" data-zero="0">200</span>"#));
    assert!(body.contains(
        r#"<li><span class="muster-summary__value">200</span><span>visible signals</span></li>"#
    ));
    assert!(body.contains(
        r#"<li><span class="muster-summary__value">200</span><span>unread here</span></li>"#
    ));
    assert!(body
        .contains(r#"<li><span class="muster-summary__value">0</span><span>warning</span></li>"#));
    assert!(body
        .contains(r#"<li><span class="muster-summary__value">0</span><span>critical</span></li>"#));
    assert!(body.contains(
        r#"<span>INFO</span><span class="muster-filter__count" aria-hidden="true">200</span>"#
    ));
    assert!(body.contains(
        r#"<span>WARNING</span><span class="muster-filter__count" aria-hidden="true">0</span>"#
    ));
    assert!(body.contains(
        r#"<span>UNREAD</span><span class="muster-filter__count" aria-hidden="true">200</span>"#
    ));
    assert!(body.contains(
        r#"<span>loom</span><span class="muster-filter__count" aria-hidden="true">200</span>"#
    ));
    let filter_nav = fragment_between(&body, r#"<nav class="muster-filters""#, "</nav>");
    assert_eq!(occurrences(filter_nav, r#"aria-current="true""#), 3);
    assert_eq!(
        occurrences(filter_nav, r#"<div class="muster-filter-group">"#),
        3
    );
    assert_eq!(
        occurrences(&body, r#"<span class="signal-entry__ordinal""#),
        200
    );
    assert!(body.contains(r#"<span class="signal-entry__ordinal" aria-hidden="true">001</span>"#));
    assert!(body.contains(r#"<span class="signal-entry__ordinal" aria-hidden="true">200</span>"#));
    assert_eq!(occurrences(&body, r#"action="/api/read""#), 201);
    assert_eq!(occurrences(&body, r#"action="/api/dismiss""#), 200);
    assert_eq!(occurrences(&body, r#"name="id""#), 400);
    let main = fragment_between(&body, r#"<main id="main""#, "</main>");
    assert_eq!(occurrences(main, r#"name="csrf_token""#), 402);
    assert!(body.find("TITLE-200").unwrap() < body.find("TITLE-199").unwrap());
    assert!(body.contains("TITLE-001"));
    assert!(!body.contains("TITLE-000"));

    let exact = st
        .store
        .list_notifications_checked(&[SUBJECT.to_string()])
        .await
        .unwrap();
    assert!(exact.truncated);
    assert_eq!(exact.notifications.len(), 200);
}

#[tokio::test]
async fn exactly_200_rows_are_not_marked_truncated() {
    let store = ControlledStore::new(false, false);
    for index in 0..200 {
        store
            .create_notification(&notification(
                &format!("n-{index:03}"),
                "loom",
                "info",
                "bounded",
                index,
            ))
            .await
            .unwrap();
    }
    let snapshot = store
        .list_notifications_checked(&[SUBJECT.to_string()])
        .await
        .unwrap();
    assert_eq!(snapshot.notifications.len(), 200);
    assert!(!snapshot.truncated);
}

#[tokio::test]
async fn checked_snapshot_orders_unread_then_newest_then_id() {
    let store = ControlledStore::new(false, false);
    let mut read = notification("r-z", "loom", "info", "read but newest", 99);
    read.read_at = 100;
    for row in [
        read,
        notification("u-a", "loom", "info", "tie a", 10),
        notification("u-b", "loom", "info", "tie b", 10),
        notification("u-old", "loom", "info", "old", 9),
    ] {
        store.create_notification(&row).await.unwrap();
    }
    let snapshot = store
        .list_notifications_checked(&[SUBJECT.to_string()])
        .await
        .unwrap();
    let ids = snapshot
        .notifications
        .iter()
        .map(|row| row.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["u-b", "u-a", "u-old", "r-z"]);
}

#[tokio::test]
async fn exact_source_filters_and_unknown_grade_round_trip_without_sentinels() {
    let store = Arc::new(ControlledStore::new(false, false));
    let long_source = format!("producer-{}", "x".repeat(240));
    for row in [
        notification("n-all", "all", "info", "LITERAL-ALL", 4),
        notification("n-empty", "", "warning", "EMPTY-SOURCE", 3),
        notification(
            "n-hostile",
            "A & = % \"<x>",
            "emergency",
            "HOSTILE-SOURCE",
            2,
        ),
        notification("n-other", "loom", "critical", "LOOM-SOURCE", 1),
        notification("n-long", &long_source, "info", "LONG-SOURCE", 0),
    ] {
        store.create_notification(&row).await.unwrap();
    }
    let st = state(store);

    let (_, literal_all) = get(&st, "/?source=all").await;
    assert!(literal_all.contains("LITERAL-ALL"));
    assert!(!literal_all.contains("EMPTY-SOURCE"));
    assert!(literal_all.contains(r#"aria-current="true""#));

    let (_, empty) = get(&st, "/?source=").await;
    assert!(empty.contains("EMPTY-SOURCE"));
    assert!(empty.contains("EMPTY SOURCE"));
    assert!(empty.contains(r#"<span class="badge-source">EMPTY SOURCE</span>"#));
    assert!(!empty.contains("LITERAL-ALL"));

    let encoded = "A%20%26%20%3D%20%25%20%22%3Cx%3E";
    let (_, hostile) = get(&st, &format!("/?grade=other&state=unread&source={encoded}")).await;
    assert!(hostile.contains("HOSTILE-SOURCE"));
    assert!(hostile.contains(
        r#"<span class="signal-grade signal-grade--other"><span aria-hidden="true">◇</span>OTHER</span>"#
    ));
    assert!(
        hostile.contains(r#"<span class="signal-entry__ordinal" aria-hidden="true">001</span>"#)
    );
    let round_trip_href = rendered_href_containing(
        &hostile,
        &format!("grade=other&amp;state=unread&amp;source={encoded}"),
    );
    let (_, round_tripped) = get(&st, &round_trip_href).await;
    assert!(round_tripped.contains("HOSTILE-SOURCE"));
    assert!(round_tripped.contains(r#"aria-current="true""#));
    assert!(hostile.contains("A &amp; = % &quot;&lt;x&gt;"));

    let (_, long) = get(&st, &format!("/?source={long_source}")).await;
    assert!(long.contains("LONG-SOURCE"));
    assert!(long.contains(&long_source));
    assert!(
        rendered_href_containing(&long, &format!("source={long_source}"))
            .contains(&format!("source={long_source}"))
    );

    let (_, normalized) = get(&st, "/?grade=INFO&state=READ&source=all").await;
    assert!(normalized.contains("LITERAL-ALL"));
    assert!(normalized.contains("grade=warning&amp;source=all"));
    assert!(!normalized.contains("grade=INFO"));
    assert!(!normalized.contains("state=READ"));
}

#[tokio::test]
async fn hostile_placeholders_are_literal_and_unsafe_url_is_sanitized() {
    let store = Arc::new(ControlledStore::new(false, false));
    let mut row = notification("n-hostile", "{{ITEMS}}", "not-a-grade", "{{CSS}}", 1);
    row.body = "{{TOPBAR}} {{GOVERNANCE_RAIL}}".to_string();
    row.url = "javascript:alert(1)".to_string();
    store.create_notification(&row).await.unwrap();
    let st = state(store);
    let (_, body) = get(&st, "/").await;

    assert!(body.contains("{{CSS}}"));
    assert!(body.contains("{{TOPBAR}} {{GOVERNANCE_RAIL}}"));
    assert!(body.contains("{{ITEMS}}"));
    assert!(body.contains("OTHER"));
    assert!(body.contains(r##"href="#" rel="noopener noreferrer""##));
    assert!(!body.contains("javascript:alert"));
    assert_eq!(occurrences(&body, "<main"), 1);
}

#[tokio::test]
async fn list_and_preferences_failures_are_independent_and_never_fake_zero() {
    let list_failure = Arc::new(ControlledStore::new(true, false));
    let (_, body) = get(&state(list_failure), "/").await;
    assert!(body.contains("Muster unavailable"));
    assert!(body.contains("No active delivery rule"));
    assert!(!body.contains("raw-list-failure"));
    assert!(!body.contains(r#"class="count""#));
    assert!(!body.contains(r#"<nav class="muster-filters"#));
    assert!(!body.contains("delivery state"));

    let prefs_failure = Arc::new(ControlledStore::new(false, true));
    prefs_failure
        .create_notification(&notification("n-1", "loom", "info", "LIST-STILL-WORKS", 1))
        .await
        .unwrap();
    let (_, body) = get(&state(prefs_failure), "/").await;
    assert!(body.contains("LIST-STILL-WORKS"));
    assert!(body.contains("Delivery settings unavailable"));
    assert!(!body.contains("raw-prefs-failure"));
    assert!(!body.contains("signal list remains"));
    assert!(body.contains(r#"<span class="count" data-zero="0">1</span>"#));

    let both_fail = Arc::new(ControlledStore::new(true, true));
    let (_, body) = get(&state(both_fail), "/").await;
    assert!(body.contains("Muster unavailable"));
    assert!(body.contains("Delivery settings unavailable"));
    assert!(body.contains("unread counts or register contents"));
    assert!(body.contains("Retry before changing delivery assumptions"));
    assert!(!body.contains("signal list remains"));
    assert!(!body.contains("delivery state"));
    assert!(!body.contains(r#"class="count""#));
}

#[tokio::test]
#[ignore = "requires an isolated local KLAXON_TEST_DATABASE_URL"]
async fn postgres_checked_preferences_never_mix_concurrent_snapshots() {
    let database_url =
        std::env::var("KLAXON_TEST_DATABASE_URL").expect("KLAXON_TEST_DATABASE_URL is required");
    assert!(
        database_url.contains("127.0.0.1") || database_url.contains("localhost"),
        "the concurrency fixture is restricted to an isolated local PostgreSQL"
    );
    assert!(
        database_url.contains("/klaxon_test"),
        "the database name must start with klaxon_test"
    );
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await
        .expect("connect isolated PostgreSQL");
    let store = Arc::new(PgStore::from_pool(pool.clone()));
    store.migrate().await.expect("migrate isolated PostgreSQL");
    let owner = format!("u_pg_snapshot_{}", std::process::id());
    sqlx::query("DELETE FROM notify_mutes WHERE user_sub = $1")
        .bind(&owner)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM notify_prefs WHERE user_sub = $1")
        .bind(&owner)
        .execute(&pool)
        .await
        .unwrap();

    let state_a = NotifyPrefs {
        user_sub: owner.clone(),
        mute_all: false,
        quiet_start: -1,
        quiet_end: -1,
        digest: false,
        updated_at: 11,
        muted_sources: vec!["alpha".to_string()],
        muted_severities: Vec::new(),
    };
    let state_b = NotifyPrefs {
        user_sub: owner.clone(),
        mute_all: true,
        quiet_start: -1,
        quiet_end: -1,
        digest: false,
        updated_at: 22,
        muted_sources: Vec::new(),
        muted_severities: vec!["critical".to_string()],
    };
    store.set_prefs(&state_a).await.unwrap();

    let writer_store = store.clone();
    let writer_a = state_a.clone();
    let writer_b = state_b.clone();
    let writer = tokio::spawn(async move {
        for index in 0..300 {
            let next = if index % 2 == 0 { &writer_b } else { &writer_a };
            writer_store.set_prefs(next).await.unwrap();
            tokio::task::yield_now().await;
        }
    });
    let reader_store = store.clone();
    let reader_owner = owner.clone();
    let reader = tokio::spawn(async move {
        for _ in 0..900 {
            let value = reader_store.get_prefs_checked(&reader_owner).await.unwrap();
            let is_a = value.updated_at == 11
                && !value.mute_all
                && value.muted_sources == ["alpha"]
                && value.muted_severities.is_empty();
            let is_b = value.updated_at == 22
                && value.mute_all
                && value.muted_sources.is_empty()
                && value.muted_severities == ["critical"];
            assert!(
                is_a || is_b,
                "checked preferences mixed two committed snapshots: {value:?}"
            );
            tokio::task::yield_now().await;
        }
    });
    let (writer_result, reader_result) = tokio::join!(writer, reader);
    writer_result.unwrap();
    reader_result.unwrap();

    sqlx::query("DELETE FROM notify_mutes WHERE user_sub = $1")
        .bind(&owner)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM notify_prefs WHERE user_sub = $1")
        .bind(&owner)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn true_empty_and_filtered_empty_are_distinct() {
    let empty_store = Arc::new(ControlledStore::new(false, false));
    let (_, clear) = get(&state(empty_store), "/").await;
    assert!(clear.contains("Muster clear"));
    assert!(!clear.contains("No signals match this register"));
    assert!(!clear.contains(r#"class="count""#));

    let filtered_store = Arc::new(ControlledStore::new(false, false));
    filtered_store
        .create_notification(&notification("n-1", "loom", "info", "INFO-ONLY", 1))
        .await
        .unwrap();
    let (_, filtered) = get(&state(filtered_store), "/?grade=critical&source=unknown").await;
    assert!(filtered.contains("No signals match this register"));
    assert!(filtered.contains("unknown"));
    assert!(!filtered.contains("Muster clear"));
    assert!(!filtered.contains(r#"class="count""#));
    assert!(filtered.contains(r#"href="/""#));
}

#[tokio::test]
async fn governance_precedence_and_inactive_quiet_are_truthful() {
    let global_store = Arc::new(ControlledStore::new(false, false));
    let mut prefs = NotifyPrefs::defaults(SUBJECT);
    prefs.mute_all = true;
    prefs.muted_sources = vec!["secret-source".to_string()];
    global_store.set_prefs(&prefs).await.unwrap();
    let (_, global) = get(&state(global_store), "/").await;
    assert!(global.contains("Global pause"));
    assert!(global.contains("Source rules</dt><dd>1"));
    assert!(!global.contains("secret-source"));
    assert!(global.contains("Inbox copies remain retained"));

    let selective_store = Arc::new(ControlledStore::new(false, false));
    let mut prefs = NotifyPrefs::defaults(SUBJECT);
    prefs.muted_severities = vec!["warning".to_string()];
    selective_store.set_prefs(&prefs).await.unwrap();
    let (_, selective) = get(&state(selective_store), "/").await;
    assert!(selective.contains("Selective rules"));
    assert!(!selective.contains(">warning<"));

    let quiet_store = Arc::new(ControlledStore::new(false, false));
    let minute = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        / 60
        % 1440) as i64;
    let mut prefs = NotifyPrefs::defaults(SUBJECT);
    prefs.quiet_start = (minute + 720) % 1440;
    prefs.quiet_end = (prefs.quiet_start + 60) % 1440;
    quiet_store.set_prefs(&prefs).await.unwrap();
    let (_, quiet) = get(&state(quiet_store), "/").await;
    assert!(quiet.contains("No active delivery rule"));
    assert!(quiet.contains("quiet interval scheduled / inactive"));

    let bounded_store = Arc::new(ControlledStore::new(false, false));
    let mut prefs = NotifyPrefs::defaults(SUBJECT);
    prefs.muted_sources = (0..120).map(|index| format!("source-{index}")).collect();
    prefs.muted_severities = (0..130).map(|index| format!("severity-{index}")).collect();
    bounded_store.set_prefs(&prefs).await.unwrap();
    let (_, bounded) = get(&state(bounded_store), "/").await;
    assert!(bounded.contains("Selective rules"));
    assert!(bounded.contains("Source rules</dt><dd>99+"));
    assert!(bounded.contains("Grade rules</dt><dd>99+"));
    assert!(!bounded.contains("source-0"));
    assert!(!bounded.contains("severity-0"));
}

#[test]
fn all_three_templates_keep_landmarks_skip_links_and_native_contracts() {
    let dashboard = include_str!("../templates/dashboard.html");
    let prefs = include_str!("../templates/prefs.html");
    let webhooks = include_str!("../templates/webhooks.html");

    for template in [dashboard, prefs, webhooks] {
        assert_eq!(occurrences(template, "<h1"), 1);
        assert_eq!(occurrences(template, r#"id="main""#), 1);
        assert!(template.find("href=\"#main\"").unwrap() < template.find("{{TOPBAR}}").unwrap());
    }
    for token in [
        "CSS",
        "TOPBAR",
        "UNREAD",
        "MARK_ALL",
        "PUSH_STATE",
        "CSRF",
        "VAPID_PUBLIC",
        "ITEMS",
        "GOVERNANCE_RAIL",
        "SEVERITY_LEGEND",
        "FILTER_RAIL",
    ] {
        assert_eq!(occurrences(dashboard, &format!("{{{{{token}}}}}")), 1);
    }
    assert!(prefs.contains(r#"action="/settings/prefs""#));
    for name in [
        "csrf_token",
        "mute_all",
        "digest",
        "quiet_start",
        "quiet_end",
        "muted_sources",
        "muted_severities",
    ] {
        assert!(prefs.contains(&format!(r#"name="{name}""#)));
    }
    assert!(webhooks.contains(r#"action="/settings/webhooks""#));
    assert!(webhooks.contains(r#"name="csrf_token""#));
    assert!(webhooks.contains(r#"name="url""#));
    assert!(webhooks.contains(r#"name="secret""#));
}
