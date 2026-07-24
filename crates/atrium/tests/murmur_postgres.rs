//! PostgreSQL contract test for Atrium's read-only Murmur projection.
//!
//! The default test run skips cleanly when `MURMUR_TEST_DATABASE_URL` is unset. The explicit
//! PostgreSQL gate shares the same disposable database used by Murmur's integration tests.

use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use atrium::source::{MurmurSource, Source};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

#[tokio::test]
#[ignore = "requires MURMUR_TEST_DATABASE_URL; run this PostgreSQL gate explicitly with --ignored"]
async fn murmur_source_uses_tuple_read_cursor() {
    let database_url = std::env::var("MURMUR_TEST_DATABASE_URL")
        .expect("MURMUR_TEST_DATABASE_URL must point to the disposable PostgreSQL gate");

    let options = PgConnectOptions::from_str(&database_url)
        .expect("MURMUR_TEST_DATABASE_URL must be a PostgreSQL URL");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .expect("connect PostgreSQL test authority");
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let schema = format!("atrium_murmur_{suffix}");
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin_pool)
        .await
        .expect("create isolated Atrium test schema");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await
        .expect("connect isolated Atrium test schema");

    sqlx::raw_sql(
        "CREATE TABLE rooms (id TEXT PRIMARY KEY, name TEXT NOT NULL); \
         CREATE TABLE memberships ( \
             room_id TEXT NOT NULL, user_sub TEXT NOT NULL, last_read_at BIGINT NOT NULL, \
             last_read_message_id TEXT NOT NULL DEFAULT '', banned BOOLEAN NOT NULL DEFAULT FALSE \
         ); \
         CREATE TABLE messages ( \
             id TEXT PRIMARY KEY, room_id TEXT NOT NULL, sender_sub TEXT NOT NULL, \
             body TEXT NOT NULL, created_at BIGINT NOT NULL, deleted BOOLEAN NOT NULL DEFAULT FALSE \
         )",
    )
    .execute(&pool)
    .await
    .expect("create minimal Murmur projection schema");
    sqlx::raw_sql(
        "INSERT INTO rooms (id, name) VALUES \
             ('room_tie', '#tie'), ('room_banned', '#banned'); \
         INSERT INTO memberships \
             (room_id, user_sub, last_read_at, last_read_message_id, banned) VALUES \
             ('room_tie', 'u_bob', 100, 'msg_a', FALSE), \
             ('room_banned', 'u_bob', 0, '', TRUE); \
         INSERT INTO messages \
             (id, room_id, sender_sub, body, created_at, deleted) VALUES \
             ('msg_a', 'room_tie', 'u_alice', 'already read', 100, FALSE), \
             ('msg_b', 'room_tie', 'u_alice', 'same-second unread', 100, FALSE), \
             ('msg_banned', 'room_banned', 'u_alice', 'must stay hidden', 101, FALSE)",
    )
    .execute(&pool)
    .await
    .expect("seed Murmur tuple-cursor projection");

    let section = MurmurSource::new(pool.clone())
        .fetch("u_bob", 10)
        .await
        .expect("fetch Atrium Murmur projection");
    assert_eq!(section.total, 1);
    assert_eq!(section.rows.len(), 1);
    assert_eq!(section.rows[0].key, "room_tie");
    assert_eq!(section.rows[0].count, Some(1));
    assert_eq!(section.rows[0].snippet, "same-second unread");
    assert_eq!(section.rows[0].at, Some(100));

    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin_pool)
        .await
        .expect("drop isolated Atrium test schema");
    admin_pool.close().await;
}
