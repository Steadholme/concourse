use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use murmur::store::{Message, MessageCursor, PgStore, Room, Store, StoreMutationError};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

struct PgFixture {
    store: Arc<PgStore>,
    pool: PgPool,
    admin_pool: PgPool,
    schema: String,
}

impl PgFixture {
    async fn new() -> Option<Self> {
        let database_url = std::env::var("MURMUR_TEST_DATABASE_URL").ok()?;
        let options = PgConnectOptions::from_str(&database_url)
            .expect("MURMUR_TEST_DATABASE_URL must be a PostgreSQL URL");
        let admin_pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .expect("connect PostgreSQL test authority");
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("murmur_security_{suffix}");
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin_pool)
            .await
            .expect("create isolated Murmur test schema");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await
            .expect("connect isolated Murmur test schema");
        let store = Arc::new(PgStore::from_pool(pool.clone()));
        store
            .migrate()
            .await
            .expect("migrate isolated Murmur schema");
        Some(Self {
            store,
            pool,
            admin_pool,
            schema,
        })
    }

    async fn cleanup(self) {
        drop(self.store);
        self.pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&self.admin_pool)
            .await
            .expect("drop isolated Murmur test schema");
        self.admin_pool.close().await;
    }
}

fn room(id: &str) -> Room {
    Room {
        id: id.to_string(),
        name: format!("#{id}"),
        kind: "room".to_string(),
        created_by: "u_alice".to_string(),
        created_at: 1,
        archived: false,
        topic: String::new(),
    }
}

fn message(id: &str, room_id: &str, body: &str) -> Message {
    Message {
        id: id.to_string(),
        room_id: room_id.to_string(),
        sender_sub: "u_alice".to_string(),
        sender_email: "alice@hf".to_string(),
        body: body.to_string(),
        created_at: 10,
        edited_at: 0,
        deleted: false,
        reply_to_id: None,
    }
}

#[tokio::test]
#[ignore = "requires MURMUR_TEST_DATABASE_URL; run this PostgreSQL gate explicitly with --ignored"]
async fn postgres_migrates_head_schema_with_conservative_tuple_cursor() {
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
        .unwrap()
        .as_nanos();
    let schema = format!("murmur_head_upgrade_{suffix}");
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin_pool)
        .await
        .expect("create isolated Murmur upgrade schema");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await
        .expect("connect isolated Murmur upgrade schema");

    // Reproduce the complete logical result of the previous release migration: all columns,
    // auxiliary tables, and indexes that existed at HEAD before this upgrade.
    sqlx::raw_sql(
        "CREATE TABLE rooms ( \
             id TEXT PRIMARY KEY, name TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'room', \
             created_by TEXT NOT NULL, created_at BIGINT NOT NULL, \
             archived BOOLEAN NOT NULL DEFAULT FALSE, topic TEXT NOT NULL DEFAULT '' \
         ); \
         CREATE TABLE memberships ( \
             room_id TEXT NOT NULL, user_sub TEXT NOT NULL, \
             user_email TEXT NOT NULL DEFAULT '', joined_at BIGINT NOT NULL, \
             last_read_at BIGINT NOT NULL DEFAULT 0, banned BOOLEAN NOT NULL DEFAULT FALSE, \
             PRIMARY KEY (room_id, user_sub) \
         ); \
         CREATE TABLE messages ( \
             id TEXT PRIMARY KEY, room_id TEXT NOT NULL, sender_sub TEXT NOT NULL, \
             sender_email TEXT NOT NULL DEFAULT '', body TEXT NOT NULL, created_at BIGINT NOT NULL, \
             edited_at BIGINT NOT NULL DEFAULT 0, deleted BOOLEAN NOT NULL DEFAULT FALSE, \
             reply_to_id TEXT \
         ); \
         CREATE TABLE message_reactions ( \
             message_id TEXT NOT NULL, user_sub TEXT NOT NULL, emoji TEXT NOT NULL, \
             created_at BIGINT NOT NULL DEFAULT 0, \
             PRIMARY KEY (message_id, user_sub, emoji) \
         ); \
         CREATE TABLE pinned_messages ( \
             room_id TEXT NOT NULL, message_id TEXT NOT NULL, pinned_by TEXT NOT NULL DEFAULT '', \
             pinned_at BIGINT NOT NULL DEFAULT 0, PRIMARY KEY (room_id, message_id) \
         ); \
         CREATE TABLE mentions ( \
             message_id TEXT NOT NULL, room_id TEXT NOT NULL, mentioned_sub TEXT NOT NULL, \
             created_at BIGINT NOT NULL DEFAULT 0, PRIMARY KEY (message_id, mentioned_sub) \
         ); \
         CREATE INDEX idx_messages_room_created ON messages (room_id, created_at); \
         CREATE INDEX idx_memberships_user ON memberships (user_sub); \
         CREATE INDEX idx_reactions_message ON message_reactions (message_id); \
         CREATE INDEX idx_messages_reply ON messages (reply_to_id); \
         CREATE INDEX idx_pinned_room ON pinned_messages (room_id); \
         CREATE INDEX idx_mentions_user ON mentions (mentioned_sub); \
         INSERT INTO rooms (id, name, kind, created_by, created_at) \
             VALUES ('room_upgrade', '#upgrade', 'room', 'u_alice', 1); \
         INSERT INTO memberships \
             (room_id, user_sub, user_email, joined_at, last_read_at) \
             VALUES ('room_upgrade', 'u_bob', 'bob@hf', 2, 100); \
         INSERT INTO messages \
             (id, room_id, sender_sub, sender_email, body, created_at) \
             VALUES ('msg_same_second', 'room_upgrade', 'u_alice', 'alice@hf', 'new?', 100)",
    )
    .execute(&pool)
    .await
    .expect("seed previous-release Murmur schema");

    let store = PgStore::from_pool(pool.clone());
    store
        .migrate()
        .await
        .expect("upgrade previous-release schema");
    store.migrate().await.expect("upgrade remains idempotent");

    let marker: String = sqlx::query_scalar(
        "SELECT last_read_message_id FROM memberships \
         WHERE room_id = 'room_upgrade' AND user_sub = 'u_bob'",
    )
    .fetch_one(&pool)
    .await
    .expect("read migrated tuple marker");
    assert_eq!(marker, "");
    let incarnation: String =
        sqlx::query_scalar("SELECT incarnation FROM rooms WHERE id = 'room_upgrade'")
            .fetch_one(&pool)
            .await
            .expect("read migrated room incarnation");
    assert!(!incarnation.is_empty());
    let outbox_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('room_delete_audit_outbox') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .expect("inspect migrated audit outbox");
    assert!(outbox_exists);
    let rooms = store.list_user_rooms("u_bob").await.unwrap();
    assert_eq!(rooms.len(), 1);
    assert_eq!(
        rooms[0].unread, 1,
        "the conservative empty-id backfill may repeat a same-second notification but must not \
         lose it"
    );

    drop(store);
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin_pool)
        .await
        .expect("drop isolated Murmur upgrade schema");
    admin_pool.close().await;
}

async fn seed_members(store: &PgStore, room_id: &str) {
    store.ensure_room(&room(room_id)).await.unwrap();
    store
        .ensure_membership(room_id, "u_alice", "alice@hf", 1)
        .await
        .unwrap();
    store
        .ensure_membership(room_id, "u_bob", "bob@hf", 2)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires MURMUR_TEST_DATABASE_URL; run this PostgreSQL gate explicitly with --ignored"]
async fn postgres_atomicity_membership_and_failure_matrix() {
    let fixture = PgFixture::new()
        .await
        .expect("MURMUR_TEST_DATABASE_URL must point to the disposable PostgreSQL gate");
    let store = fixture.store.clone();

    seed_members(&store, "room_mentions").await;
    sqlx::query(
        "CREATE FUNCTION fail_mention_insert() RETURNS trigger AS $fn$ \
         BEGIN RAISE EXCEPTION 'forced mention failure'; END; $fn$ LANGUAGE plpgsql",
    )
    .execute(&fixture.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_mention_insert_trigger BEFORE INSERT ON mentions \
         FOR EACH ROW EXECUTE FUNCTION fail_mention_insert()",
    )
    .execute(&fixture.pool)
    .await
    .unwrap();
    let failed = store
        .create_message_authorized(
            &message("msg_mention_failed", "room_mentions", "hello @bob"),
            &["bob".to_string()],
        )
        .await;
    assert!(matches!(failed, Err(StoreMutationError::Backend(_))));
    assert!(store
        .get_message("msg_mention_failed")
        .await
        .unwrap()
        .is_none());
    sqlx::query("DROP TRIGGER fail_mention_insert_trigger ON mentions")
        .execute(&fixture.pool)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION fail_mention_insert()")
        .execute(&fixture.pool)
        .await
        .unwrap();

    store
        .create_message_authorized(
            &message("msg_mention_ok", "room_mentions", "hello @bob"),
            &["bob".to_string()],
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .list_user_mentions("u_bob", None, 50)
            .await
            .unwrap()
            .len(),
        1
    );
    store.ban_member("room_mentions", "u_bob").await.unwrap();
    assert!(store
        .list_user_mentions("u_bob", None, 50)
        .await
        .unwrap()
        .is_empty());

    seed_members(&store, "room_delete").await;
    let delete_message = message("msg_delete", "room_delete", "delete @bob");
    store
        .create_message_authorized(&delete_message, &["bob".to_string()])
        .await
        .unwrap();
    store
        .toggle_reaction_authorized("room_delete", "msg_delete", "u_alice", "x")
        .await
        .unwrap();
    store
        .pin_message_authorized("room_delete", "msg_delete", "u_alice", 10)
        .await
        .unwrap();
    store
        .issue_room_delete_token(
            "room_delete_token",
            "room_delete",
            "u_admin",
            "csrf",
            0,
            i64::MAX,
        )
        .await
        .unwrap();
    sqlx::query(
        "CREATE FUNCTION fail_membership_delete() RETURNS trigger AS $fn$ \
         BEGIN RAISE EXCEPTION 'forced delete failure'; END; $fn$ LANGUAGE plpgsql",
    )
    .execute(&fixture.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_membership_delete_trigger BEFORE DELETE ON memberships \
         FOR EACH ROW EXECUTE FUNCTION fail_membership_delete()",
    )
    .execute(&fixture.pool)
    .await
    .unwrap();
    assert!(matches!(
        store
            .delete_room_with_token("room_delete", "room_delete_token", "u_admin", "csrf", 1,)
            .await,
        Err(StoreMutationError::Backend(_))
    ));
    assert!(store.get_room("room_delete").await.unwrap().is_some());
    assert!(store.get_message("msg_delete").await.unwrap().is_some());
    assert!(store.is_member("room_delete", "u_alice").await.unwrap());
    assert_eq!(store.list_pinned("room_delete").await.unwrap().len(), 1);
    assert_eq!(store.list_reactions("msg_delete").await.unwrap().len(), 1);
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM room_delete_audit_outbox WHERE room_id = 'room_delete'",
    )
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(
        audit_count, 0,
        "failed delete must roll back its consequence"
    );
    assert_eq!(
        store
            .list_user_mentions("u_bob", None, 50)
            .await
            .unwrap()
            .iter()
            .filter(|hit| hit.message.room_id == "room_delete")
            .count(),
        1
    );
    sqlx::query("DROP TRIGGER fail_membership_delete_trigger ON memberships")
        .execute(&fixture.pool)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION fail_membership_delete()")
        .execute(&fixture.pool)
        .await
        .unwrap();
    store
        .delete_room_with_token("room_delete", "room_delete_token", "u_admin", "csrf", 1)
        .await
        .unwrap();
    assert!(store.get_room("room_delete").await.unwrap().is_none());
    assert!(store.get_message("msg_delete").await.unwrap().is_none());
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM room_delete_audit_outbox WHERE room_id = 'room_delete'",
    )
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(audit_count, 1);

    let mut dm = room("dm_u_alice__u_bob");
    dm.kind = "dm".to_string();
    store
        .open_dm_authorized(&dm, "u_alice", "alice@hf", "u_bob", "bob@hf", 1)
        .await
        .unwrap();
    store.remove_member(&dm.id, "u_bob").await.unwrap();
    let mut dm_blocker = fixture.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM rooms WHERE id = $1 FOR UPDATE")
        .bind(&dm.id)
        .fetch_one(&mut *dm_blocker)
        .await
        .unwrap();
    let dm_store = store.clone();
    let proposed_dm = dm.clone();
    let pending_dm = tokio::spawn(async move {
        dm_store
            .open_dm_authorized(&proposed_dm, "u_alice", "alice@hf", "u_bob", "bob@hf", 2)
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    sqlx::query("UPDATE rooms SET archived = TRUE WHERE id = $1")
        .bind(&dm.id)
        .execute(&mut *dm_blocker)
        .await
        .unwrap();
    dm_blocker.commit().await.unwrap();
    let dm_outcome = tokio::time::timeout(Duration::from_secs(5), pending_dm)
        .await
        .expect("DM open must leave the room-lock wait")
        .unwrap();
    assert!(matches!(dm_outcome, Err(StoreMutationError::RoomArchived)));
    assert!(
        !store.is_member(&dm.id, "u_bob").await.unwrap(),
        "archiving during open must leave the removed peer absent"
    );

    seed_members(&store, "room_race").await;
    let mut blocker = fixture.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT user_sub FROM memberships \
         WHERE room_id = 'room_race' AND user_sub = 'u_alice' FOR UPDATE",
    )
    .fetch_one(&mut *blocker)
    .await
    .unwrap();
    let race_store = store.clone();
    let pending = tokio::spawn(async move {
        race_store
            .create_message_authorized(
                &message("msg_race_denied", "room_race", "must not persist"),
                &[],
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    sqlx::query(
        "UPDATE memberships SET banned = TRUE \
         WHERE room_id = 'room_race' AND user_sub = 'u_alice'",
    )
    .execute(&mut *blocker)
    .await
    .unwrap();
    blocker.commit().await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .expect("authorized send must leave the row-lock wait")
        .unwrap();
    assert!(matches!(
        outcome,
        Err(StoreMutationError::ResourceUnavailable)
    ));
    assert!(store
        .get_message("msg_race_denied")
        .await
        .unwrap()
        .is_none());

    // Tuple cursor and tuple read marker: 51 same-second rows must paginate completely and the
    // read marker must retain the id tie-breaker.
    seed_members(&store, "room_cursor").await;
    for index in 0..51 {
        let mut row = message(
            &format!("msg_cursor_{index:03}"),
            "room_cursor",
            "same second",
        );
        row.created_at = 100;
        store.create_message(&row).await.unwrap();
    }
    let first = store
        .list_messages_authorized("room_cursor", "u_bob", None, 50)
        .await
        .unwrap();
    assert_eq!(first.len(), 50);
    let second = store
        .list_messages_authorized(
            "room_cursor",
            "u_bob",
            Some(MessageCursor::from_message(first.last().unwrap())),
            50,
        )
        .await
        .unwrap();
    assert_eq!(
        second.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        vec!["msg_cursor_000"]
    );
    let search_first = store
        .search_user_messages("u_bob", "same second", None, 50)
        .await
        .unwrap();
    assert_eq!(search_first.len(), 50);
    let search_second = store
        .search_user_messages(
            "u_bob",
            "same second",
            Some(MessageCursor::from_message(
                &search_first.last().unwrap().message,
            )),
            50,
        )
        .await
        .unwrap();
    assert_eq!(search_second.len(), 1);
    assert_eq!(search_second[0].message.id, "msg_cursor_000");
    assert!(store
        .list_user_mentions(
            "u_bob",
            Some(MessageCursor {
                created_at: 100,
                message_id: "msg_cursor_025".to_string(),
            }),
            50,
        )
        .await
        .unwrap()
        .is_empty());
    store
        .update_last_read_authorized("room_cursor", "msg_cursor_025", "u_bob")
        .await
        .unwrap();
    let cursor_room = store
        .list_user_rooms("u_bob")
        .await
        .unwrap()
        .into_iter()
        .find(|room| room.room.id == "room_cursor")
        .unwrap();
    assert_eq!(cursor_room.last_read_at, 100);
    assert_eq!(cursor_room.last_read_message_id, "msg_cursor_025");
    assert_eq!(cursor_room.unread, 25);

    // A protected projection waiting behind the canonical room lock observes the committed ban
    // as a typed denial and returns no data.
    seed_members(&store, "room_projection").await;
    store
        .create_message(&message(
            "msg_projection_secret",
            "room_projection",
            "secret",
        ))
        .await
        .unwrap();
    let mut projection_blocker = fixture.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM rooms WHERE id = 'room_projection' FOR UPDATE")
        .fetch_one(&mut *projection_blocker)
        .await
        .unwrap();
    let projection_store = store.clone();
    let pending_projection = tokio::spawn(async move {
        projection_store
            .list_messages_authorized("room_projection", "u_bob", None, 50)
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    sqlx::query(
        "UPDATE memberships SET banned = TRUE \
         WHERE room_id = 'room_projection' AND user_sub = 'u_bob'",
    )
    .execute(&mut *projection_blocker)
    .await
    .unwrap();
    projection_blocker.commit().await.unwrap();
    let projection_outcome = tokio::time::timeout(Duration::from_secs(5), pending_projection)
        .await
        .expect("projection must leave the room-lock wait")
        .unwrap();
    assert!(matches!(
        projection_outcome,
        Err(StoreMutationError::MemberBanned)
    ));

    // Token consumption and the purge share one transaction: a forced child-delete failure rolls
    // both back, so the same grant can be retried once the backend recovers.
    seed_members(&store, "room_token_rollback").await;
    store
        .create_message(&message(
            "msg_token_rollback",
            "room_token_rollback",
            "delete me",
        ))
        .await
        .unwrap();
    store
        .issue_room_delete_token(
            "token_rollback",
            "room_token_rollback",
            "u_admin",
            "csrf",
            0,
            i64::MAX,
        )
        .await
        .unwrap();
    sqlx::query(
        "CREATE FUNCTION fail_token_membership_delete() RETURNS trigger AS $fn$ \
         BEGIN RAISE EXCEPTION 'forced token delete failure'; END; $fn$ LANGUAGE plpgsql",
    )
    .execute(&fixture.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_token_membership_delete_trigger BEFORE DELETE ON memberships \
         FOR EACH ROW EXECUTE FUNCTION fail_token_membership_delete()",
    )
    .execute(&fixture.pool)
    .await
    .unwrap();
    assert!(matches!(
        store
            .delete_room_with_token(
                "room_token_rollback",
                "token_rollback",
                "u_admin",
                "csrf",
                1,
            )
            .await,
        Err(StoreMutationError::Backend(_))
    ));
    assert!(store
        .get_room("room_token_rollback")
        .await
        .unwrap()
        .is_some());
    sqlx::query("DROP TRIGGER fail_token_membership_delete_trigger ON memberships")
        .execute(&fixture.pool)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION fail_token_membership_delete()")
        .execute(&fixture.pool)
        .await
        .unwrap();
    store
        .delete_room_with_token(
            "room_token_rollback",
            "token_rollback",
            "u_admin",
            "csrf",
            1,
        )
        .await
        .unwrap();
    assert!(matches!(
        store
            .delete_room_with_token(
                "room_token_rollback",
                "token_rollback",
                "u_admin",
                "csrf",
                1,
            )
            .await,
        Err(StoreMutationError::ConsequenceTokenInvalid)
    ));

    // The durable audit append is part of the delete transaction. If the outbox write fails, the
    // room and grant both remain retryable.
    seed_members(&store, "room_audit_rollback").await;
    store
        .issue_room_delete_token(
            "token_audit_rollback",
            "room_audit_rollback",
            "u_admin",
            "csrf",
            0,
            i64::MAX,
        )
        .await
        .unwrap();
    sqlx::query(
        "CREATE FUNCTION fail_room_delete_audit_insert() RETURNS trigger AS $fn$ \
         BEGIN RAISE EXCEPTION 'forced audit outbox failure'; END; $fn$ LANGUAGE plpgsql",
    )
    .execute(&fixture.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_room_delete_audit_insert_trigger \
         BEFORE INSERT ON room_delete_audit_outbox \
         FOR EACH ROW EXECUTE FUNCTION fail_room_delete_audit_insert()",
    )
    .execute(&fixture.pool)
    .await
    .unwrap();
    assert!(matches!(
        store
            .delete_room_with_token(
                "room_audit_rollback",
                "token_audit_rollback",
                "u_admin",
                "csrf",
                1,
            )
            .await,
        Err(StoreMutationError::Backend(_))
    ));
    assert!(store
        .get_room("room_audit_rollback")
        .await
        .unwrap()
        .is_some());
    let token_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM room_delete_tokens WHERE token_digest = 'token_audit_rollback'",
    )
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(token_count, 1);
    sqlx::query("DROP TRIGGER fail_room_delete_audit_insert_trigger ON room_delete_audit_outbox")
        .execute(&fixture.pool)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION fail_room_delete_audit_insert()")
        .execute(&fixture.pool)
        .await
        .unwrap();
    store
        .delete_room_with_token(
            "room_audit_rollback",
            "token_audit_rollback",
            "u_admin",
            "csrf",
            1,
        )
        .await
        .unwrap();

    // A stale grant cannot cross a deterministic room-id recreation, even if a legacy/out-of-band
    // delete failed to clean the token table.
    store.ensure_room(&room("room_incarnation")).await.unwrap();
    store
        .issue_room_delete_token(
            "token_old_incarnation",
            "room_incarnation",
            "u_admin",
            "csrf",
            0,
            i64::MAX,
        )
        .await
        .unwrap();
    let old_incarnation: String =
        sqlx::query_scalar("SELECT incarnation FROM rooms WHERE id = 'room_incarnation'")
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    sqlx::query("DELETE FROM rooms WHERE id = 'room_incarnation'")
        .execute(&fixture.pool)
        .await
        .unwrap();
    let mut recreated = room("room_incarnation");
    recreated.name = "#recreated".to_string();
    store.ensure_room(&recreated).await.unwrap();
    let new_incarnation: String =
        sqlx::query_scalar("SELECT incarnation FROM rooms WHERE id = 'room_incarnation'")
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_ne!(old_incarnation, new_incarnation);
    assert!(matches!(
        store
            .delete_room_with_token(
                "room_incarnation",
                "token_old_incarnation",
                "u_admin",
                "csrf",
                1,
            )
            .await,
        Err(StoreMutationError::ConsequenceTokenInvalid)
    ));
    assert!(store.get_room("room_incarnation").await.unwrap().is_some());

    // Concurrent duplicate submission yields exactly one committed consequence.
    seed_members(&store, "room_token_concurrent").await;
    store
        .issue_room_delete_token(
            "token_concurrent",
            "room_token_concurrent",
            "u_admin",
            "csrf",
            0,
            i64::MAX,
        )
        .await
        .unwrap();
    let left_store = store.clone();
    let left = tokio::spawn(async move {
        left_store
            .delete_room_with_token(
                "room_token_concurrent",
                "token_concurrent",
                "u_admin",
                "csrf",
                1,
            )
            .await
    });
    let right_store = store.clone();
    let right = tokio::spawn(async move {
        right_store
            .delete_room_with_token(
                "room_token_concurrent",
                "token_concurrent",
                "u_admin",
                "csrf",
                1,
            )
            .await
    });
    let outcomes = [left.await.unwrap(), right.await.unwrap()];
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| { matches!(result, Err(StoreMutationError::ConsequenceTokenInvalid)) })
            .count(),
        1
    );
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM room_delete_audit_outbox \
         WHERE room_id = 'room_token_concurrent'",
    )
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(
        audit_count, 1,
        "duplicate POSTs must append one consequence"
    );

    drop(store);
    fixture.cleanup().await;
}
