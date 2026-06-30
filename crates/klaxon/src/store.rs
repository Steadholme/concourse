//! Notification / push-subscription / webhook storage.
//!
//! `Store` is a small async trait with an in-memory and a PostgreSQL implementation, mirroring
//! the inkwell/sanctum seam: handlers depend only on the trait, so a FusionDB-backed store can
//! drop in later. The PostgreSQL layer uses ONLY portable standard SQL
//! (TEXT/BIGINT, PRIMARY KEY/UNIQUE/NOT NULL/DEFAULT, INSERT..ON CONFLICT, CREATE INDEX) and
//! runtime queries (no compile-time macros), so the build needs NO database and the same
//! statements later run unchanged on FusionDB over pgwire.
//!
//! The methods are `async`: the axum handlers `.await` them directly on the serving runtime, and
//! `PgStore` drives sqlx natively — there is NO `block_in_place` and NO sync-over-async bridge,
//! so a DB round-trip never blocks a worker thread.

use std::sync::Mutex;

use async_trait::async_trait;
use serde::Serialize;
use thiserror::Error;

use crate::config::LIST_LIMIT;

/// A delivered notification (maps 1:1 to a `notifications` row). `read_at == 0` means unread.
#[derive(Clone, Debug, Serialize)]
pub struct Notification {
    pub id: String,
    pub user_sub: String,
    pub source: String,
    pub title: String,
    pub body: String,
    pub url: String,
    pub created_at: i64,
    pub read_at: i64,
}

/// A Web Push subscription (maps 1:1 to a `push_subscriptions` row).
#[derive(Clone, Debug, Serialize)]
pub struct PushSubscription {
    pub id: String,
    pub user_sub: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub created_at: i64,
}

/// A registered outbound webhook (maps 1:1 to a `webhooks` row).
#[derive(Clone, Debug, Serialize)]
pub struct Webhook {
    pub id: String,
    pub user_sub: String,
    pub url: String,
    pub secret: String,
    pub created_at: i64,
}

/// Storage failure surfaced to the handler layer.
#[derive(Debug, Error)]
pub enum StoreError {
    /// Backend I/O failure (mapped to a 500).
    #[error("store error: {0}")]
    Backend(String),
}

/// Pluggable notification store.
#[async_trait]
pub trait Store: Send + Sync {
    /// Insert a delivered notification.
    async fn create_notification(&self, n: &Notification) -> Result<(), StoreError>;
    /// A user's notifications, UNREAD first then newest-first, capped at [`LIST_LIMIT`]. The user
    /// is matched by ANY of `keys` (the subject and/or the email, since a producer may address by
    /// either).
    async fn list_notifications(&self, keys: &[String]) -> Vec<Notification>;
    /// A user's notifications created at-or-after `since` epoch seconds (the SSE poll tail).
    async fn list_since(&self, keys: &[String], since: i64) -> Vec<Notification>;
    /// Mark one notification read (when `id` is `Some`) or all of the user's unread (when `None`).
    /// Returns the number of rows updated.
    async fn mark_read(&self, keys: &[String], id: Option<&str>, now: i64) -> Result<u64, StoreError>;
    /// Count a user's unread notifications.
    async fn unread_count(&self, keys: &[String]) -> i64;
    /// Store (or refresh) a Web Push subscription, keyed by its unique `endpoint`.
    async fn upsert_subscription(&self, sub: &PushSubscription) -> Result<(), StoreError>;
    /// A user's Web Push subscriptions (fan-out targets).
    async fn list_subscriptions(&self, key: &str) -> Vec<PushSubscription>;
    /// Register an outbound webhook for a user.
    async fn create_webhook(&self, hook: &Webhook) -> Result<(), StoreError>;
    /// A user's registered webhooks (fan-out targets).
    async fn list_webhooks(&self, key: &str) -> Vec<Webhook>;
}

// --------------------------------------------------------------------------------------
// In-memory store (the default; keeps the whole service database-free for dev + tests).
// --------------------------------------------------------------------------------------

#[derive(Default)]
pub struct InMemoryStore {
    notifications: Mutex<Vec<Notification>>,
    subscriptions: Mutex<Vec<PushSubscription>>,
    webhooks: Mutex<Vec<Webhook>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

/// True when `sub` is any of the addressing `keys` (subject and/or email).
fn matches(sub: &str, keys: &[String]) -> bool {
    keys.iter().any(|k| k == sub)
}

#[async_trait]
impl Store for InMemoryStore {
    // The std `Mutex` is fine throughout: each critical section is fully synchronous (no `.await`
    // inside), so a guard is never held across a yield point.
    async fn create_notification(&self, n: &Notification) -> Result<(), StoreError> {
        self.notifications
            .lock()
            .expect("notifications lock poisoned")
            .push(n.clone());
        Ok(())
    }

    async fn list_notifications(&self, keys: &[String]) -> Vec<Notification> {
        let all = self.notifications.lock().expect("notifications lock poisoned");
        let mut v: Vec<Notification> = all.iter().filter(|n| matches(&n.user_sub, keys)).cloned().collect();
        // Unread (read_at == 0) first; within each group newest-first, ties broken by id.
        v.sort_by(|a, b| {
            let ar = (a.read_at != 0) as u8;
            let br = (b.read_at != 0) as u8;
            ar.cmp(&br)
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| b.id.cmp(&a.id))
        });
        v.truncate(LIST_LIMIT);
        v
    }

    async fn list_since(&self, keys: &[String], since: i64) -> Vec<Notification> {
        let all = self.notifications.lock().expect("notifications lock poisoned");
        let mut v: Vec<Notification> = all
            .iter()
            .filter(|n| matches(&n.user_sub, keys) && n.created_at >= since)
            .cloned()
            .collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        v
    }

    async fn mark_read(&self, keys: &[String], id: Option<&str>, now: i64) -> Result<u64, StoreError> {
        let mut all = self.notifications.lock().expect("notifications lock poisoned");
        let mut updated = 0u64;
        for n in all.iter_mut() {
            if !matches(&n.user_sub, keys) || n.read_at != 0 {
                continue;
            }
            if let Some(target) = id {
                if n.id != target {
                    continue;
                }
            }
            n.read_at = now;
            updated += 1;
        }
        Ok(updated)
    }

    async fn unread_count(&self, keys: &[String]) -> i64 {
        self.notifications
            .lock()
            .expect("notifications lock poisoned")
            .iter()
            .filter(|n| matches(&n.user_sub, keys) && n.read_at == 0)
            .count() as i64
    }

    async fn upsert_subscription(&self, sub: &PushSubscription) -> Result<(), StoreError> {
        let mut subs = self.subscriptions.lock().expect("subscriptions lock poisoned");
        match subs.iter_mut().find(|s| s.endpoint == sub.endpoint) {
            Some(existing) => {
                existing.user_sub = sub.user_sub.clone();
                existing.p256dh = sub.p256dh.clone();
                existing.auth = sub.auth.clone();
            }
            None => subs.push(sub.clone()),
        }
        Ok(())
    }

    async fn list_subscriptions(&self, key: &str) -> Vec<PushSubscription> {
        self.subscriptions
            .lock()
            .expect("subscriptions lock poisoned")
            .iter()
            .filter(|s| s.user_sub == key)
            .cloned()
            .collect()
    }

    async fn create_webhook(&self, hook: &Webhook) -> Result<(), StoreError> {
        self.webhooks
            .lock()
            .expect("webhooks lock poisoned")
            .push(hook.clone());
        Ok(())
    }

    async fn list_webhooks(&self, key: &str) -> Vec<Webhook> {
        self.webhooks
            .lock()
            .expect("webhooks lock poisoned")
            .iter()
            .filter(|h| h.user_sub == key)
            .cloned()
            .collect()
    }
}

// --------------------------------------------------------------------------------------
// PostgreSQL-backed store (portable: standard SQL, runtime queries, no macros).
// --------------------------------------------------------------------------------------
//
// Selected at runtime by `KLAXON_STORE=postgres`. Each method drives sqlx natively and the
// handlers `.await` it on the serving runtime — NO `block_in_place`, NO sync-over-async. The DB
// enforces the `endpoint` UNIQUE constraint via INSERT..ON CONFLICT, so no in-process serializer
// is needed.

use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;

/// PostgreSQL-backed [`Store`]. Holds just a `PgPool`.
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    /// Open a pooled connection. Async; call from within a Tokio runtime.
    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(database_url)
            .await?;
        Ok(Self::from_pool(pool))
    }

    /// Construct from an existing pool (used by tests that share a pool).
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Idempotent, portable migration. Standard SQL only — safe to run on every startup. The
    /// table/column names are PINNED: other services in the estate depend on them.
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS notifications (\
                 id TEXT PRIMARY KEY, \
                 user_sub TEXT NOT NULL, \
                 source TEXT NOT NULL, \
                 title TEXT NOT NULL, \
                 body TEXT NOT NULL DEFAULT '', \
                 url TEXT NOT NULL DEFAULT '', \
                 created_at BIGINT NOT NULL, \
                 read_at BIGINT NOT NULL DEFAULT 0\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_notifications_user_created \
             ON notifications (user_sub, created_at)",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS push_subscriptions (\
                 id TEXT PRIMARY KEY, \
                 user_sub TEXT NOT NULL, \
                 endpoint TEXT NOT NULL UNIQUE, \
                 p256dh TEXT NOT NULL, \
                 auth TEXT NOT NULL, \
                 created_at BIGINT NOT NULL\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_push_subscriptions_user \
             ON push_subscriptions (user_sub)",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS webhooks (\
                 id TEXT PRIMARY KEY, \
                 user_sub TEXT NOT NULL, \
                 url TEXT NOT NULL, \
                 secret TEXT NOT NULL DEFAULT '', \
                 created_at BIGINT NOT NULL\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_webhooks_user ON webhooks (user_sub)")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    fn notification_from_row(row: &sqlx::postgres::PgRow) -> Result<Notification, sqlx::Error> {
        Ok(Notification {
            id: row.try_get("id")?,
            user_sub: row.try_get("user_sub")?,
            source: row.try_get("source")?,
            title: row.try_get("title")?,
            body: row.try_get("body")?,
            url: row.try_get("url")?,
            created_at: row.try_get("created_at")?,
            read_at: row.try_get("read_at")?,
        })
    }

    fn subscription_from_row(row: &sqlx::postgres::PgRow) -> Result<PushSubscription, sqlx::Error> {
        Ok(PushSubscription {
            id: row.try_get("id")?,
            user_sub: row.try_get("user_sub")?,
            endpoint: row.try_get("endpoint")?,
            p256dh: row.try_get("p256dh")?,
            auth: row.try_get("auth")?,
            created_at: row.try_get("created_at")?,
        })
    }

    fn webhook_from_row(row: &sqlx::postgres::PgRow) -> Result<Webhook, sqlx::Error> {
        Ok(Webhook {
            id: row.try_get("id")?,
            user_sub: row.try_get("user_sub")?,
            url: row.try_get("url")?,
            secret: row.try_get("secret")?,
            created_at: row.try_get("created_at")?,
        })
    }

    /// The two addressing keys (subject, email) as a fixed pair: when only one key is present we
    /// pass it twice so the `= $1 OR = $2` predicate stays a single, parameterized shape.
    fn key_pair(keys: &[String]) -> (String, String) {
        let k1 = keys.first().cloned().unwrap_or_default();
        let k2 = keys.get(1).cloned().unwrap_or_else(|| k1.clone());
        (k1, k2)
    }

    async fn create_notification_async(&self, n: &Notification) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO notifications \
                 (id, user_sub, source, title, body, url, created_at, read_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&n.id)
        .bind(&n.user_sub)
        .bind(&n.source)
        .bind(&n.title)
        .bind(&n.body)
        .bind(&n.url)
        .bind(n.created_at)
        .bind(n.read_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_notifications_async(&self, keys: &[String]) -> Result<Vec<Notification>, sqlx::Error> {
        let (k1, k2) = Self::key_pair(keys);
        let rows = sqlx::query(
            "SELECT id, user_sub, source, title, body, url, created_at, read_at \
             FROM notifications WHERE user_sub = $1 OR user_sub = $2 \
             ORDER BY CASE WHEN read_at = 0 THEN 0 ELSE 1 END, created_at DESC, id DESC LIMIT $3",
        )
        .bind(&k1)
        .bind(&k2)
        .bind(LIST_LIMIT as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::notification_from_row).collect()
    }

    async fn list_since_async(&self, keys: &[String], since: i64) -> Result<Vec<Notification>, sqlx::Error> {
        let (k1, k2) = Self::key_pair(keys);
        let rows = sqlx::query(
            "SELECT id, user_sub, source, title, body, url, created_at, read_at \
             FROM notifications WHERE (user_sub = $1 OR user_sub = $2) AND created_at >= $3 \
             ORDER BY created_at ASC, id ASC LIMIT $4",
        )
        .bind(&k1)
        .bind(&k2)
        .bind(since)
        .bind(LIST_LIMIT as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::notification_from_row).collect()
    }

    async fn mark_read_async(&self, keys: &[String], id: Option<&str>, now: i64) -> Result<u64, sqlx::Error> {
        let (k1, k2) = Self::key_pair(keys);
        let result = match id {
            Some(target) => {
                sqlx::query(
                    "UPDATE notifications SET read_at = $1 \
                     WHERE id = $2 AND read_at = 0 AND (user_sub = $3 OR user_sub = $4)",
                )
                .bind(now)
                .bind(target)
                .bind(&k1)
                .bind(&k2)
                .execute(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "UPDATE notifications SET read_at = $1 \
                     WHERE read_at = 0 AND (user_sub = $2 OR user_sub = $3)",
                )
                .bind(now)
                .bind(&k1)
                .bind(&k2)
                .execute(&self.pool)
                .await?
            }
        };
        Ok(result.rows_affected())
    }

    async fn unread_count_async(&self, keys: &[String]) -> Result<i64, sqlx::Error> {
        let (k1, k2) = Self::key_pair(keys);
        let row = sqlx::query(
            "SELECT COUNT(*) AS c FROM notifications \
             WHERE read_at = 0 AND (user_sub = $1 OR user_sub = $2)",
        )
        .bind(&k1)
        .bind(&k2)
        .fetch_one(&self.pool)
        .await?;
        row.try_get::<i64, _>("c")
    }

    async fn upsert_subscription_async(&self, s: &PushSubscription) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO push_subscriptions (id, user_sub, endpoint, p256dh, auth, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (endpoint) DO UPDATE SET \
                 user_sub = EXCLUDED.user_sub, p256dh = EXCLUDED.p256dh, auth = EXCLUDED.auth",
        )
        .bind(&s.id)
        .bind(&s.user_sub)
        .bind(&s.endpoint)
        .bind(&s.p256dh)
        .bind(&s.auth)
        .bind(s.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_subscriptions_async(&self, key: &str) -> Result<Vec<PushSubscription>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, user_sub, endpoint, p256dh, auth, created_at \
             FROM push_subscriptions WHERE user_sub = $1",
        )
        .bind(key)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::subscription_from_row).collect()
    }

    async fn create_webhook_async(&self, h: &Webhook) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO webhooks (id, user_sub, url, secret, created_at) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&h.id)
        .bind(&h.user_sub)
        .bind(&h.url)
        .bind(&h.secret)
        .bind(h.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_webhooks_async(&self, key: &str) -> Result<Vec<Webhook>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, user_sub, url, secret, created_at FROM webhooks WHERE user_sub = $1",
        )
        .bind(key)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::webhook_from_row).collect()
    }
}

#[async_trait]
impl Store for PgStore {
    async fn create_notification(&self, n: &Notification) -> Result<(), StoreError> {
        self.create_notification_async(n)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_notifications(&self, keys: &[String]) -> Vec<Notification> {
        self.list_notifications_async(keys).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg list_notifications failed");
            Vec::new()
        })
    }

    async fn list_since(&self, keys: &[String], since: i64) -> Vec<Notification> {
        self.list_since_async(keys, since).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg list_since failed");
            Vec::new()
        })
    }

    async fn mark_read(&self, keys: &[String], id: Option<&str>, now: i64) -> Result<u64, StoreError> {
        self.mark_read_async(keys, id, now)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn unread_count(&self, keys: &[String]) -> i64 {
        self.unread_count_async(keys).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg unread_count failed");
            0
        })
    }

    async fn upsert_subscription(&self, sub: &PushSubscription) -> Result<(), StoreError> {
        self.upsert_subscription_async(sub)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_subscriptions(&self, key: &str) -> Vec<PushSubscription> {
        self.list_subscriptions_async(key).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg list_subscriptions failed");
            Vec::new()
        })
    }

    async fn create_webhook(&self, hook: &Webhook) -> Result<(), StoreError> {
        self.create_webhook_async(hook)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_webhooks(&self, key: &str) -> Vec<Webhook> {
        self.list_webhooks_async(key).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg list_webhooks failed");
            Vec::new()
        })
    }
}
