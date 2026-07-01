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
    /// Producer-supplied classification (`info` | `warning` | `critical` | …), lower-cased. Used
    /// only for per-severity delivery muting; defaults to `info` when the producer omits it.
    pub severity: String,
    pub title: String,
    pub body: String,
    pub url: String,
    pub created_at: i64,
    pub read_at: i64,
}

/// Per-owner delivery preferences (maps to a `notify_prefs` row plus its `notify_mutes` children).
///
/// Read is idempotent: [`Store::get_prefs`] returns [`NotifyPrefs::defaults`] (everything off) when
/// the owner has never saved any, so the request path never special-cases "no row". Quiet-hours are
/// stored as a minute-of-day (`0..=1439`, UTC); `-1` means unset. The window may wrap past midnight.
#[derive(Clone, Debug, Serialize)]
pub struct NotifyPrefs {
    pub user_sub: String,
    /// Suppress ALL real-time delivery (push + webhook + email); notifications still land in-inbox.
    pub mute_all: bool,
    /// Quiet-hours start, minute-of-day UTC (`0..=1439`), or `-1` when unset.
    pub quiet_start: i64,
    /// Quiet-hours end, minute-of-day UTC (`0..=1439`), or `-1` when unset.
    pub quiet_end: i64,
    /// Digest mode: collect for a later digest instead of pinging in real time (suppresses the
    /// real-time channels, exactly like quiet-hours; the in-inbox copy is always kept).
    pub digest: bool,
    /// Wall-clock (epoch secs) of the last save; `0` for the synthetic defaults.
    pub updated_at: i64,
    /// Sources whose notifications skip real-time delivery (matched case-insensitively).
    pub muted_sources: Vec<String>,
    /// Severities whose notifications skip real-time delivery (matched case-insensitively).
    pub muted_severities: Vec<String>,
}

impl NotifyPrefs {
    /// The synthetic "nothing muted" preferences returned for an owner with no saved row.
    pub fn defaults(user_sub: &str) -> Self {
        NotifyPrefs {
            user_sub: user_sub.to_string(),
            mute_all: false,
            quiet_start: -1,
            quiet_end: -1,
            digest: false,
            updated_at: 0,
            muted_sources: Vec::new(),
            muted_severities: Vec::new(),
        }
    }

    /// True when a notification of `source`/`severity` at wall-`minute` (minute-of-day UTC) must NOT
    /// be delivered in real time (push/webhook/email). The in-inbox copy is ALWAYS kept regardless.
    pub fn suppresses_realtime(&self, source: &str, severity: &str, minute: i64) -> bool {
        self.mute_all
            || self.digest
            || self.in_quiet_hours(minute)
            || self.muted_sources.iter().any(|s| s.eq_ignore_ascii_case(source))
            || self.muted_severities.iter().any(|s| s.eq_ignore_ascii_case(severity))
    }

    /// True when `minute` (minute-of-day UTC) falls inside the configured quiet-hours window. An
    /// unset or zero-width window is never quiet; a window whose start > end wraps past midnight.
    pub fn in_quiet_hours(&self, minute: i64) -> bool {
        if self.quiet_start < 0 || self.quiet_end < 0 || self.quiet_start == self.quiet_end {
            return false;
        }
        if self.quiet_start < self.quiet_end {
            minute >= self.quiet_start && minute < self.quiet_end
        } else {
            minute >= self.quiet_start || minute < self.quiet_end
        }
    }
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
    /// A user's registered webhooks (fan-out targets), newest-first.
    async fn list_webhooks(&self, key: &str) -> Vec<Webhook>;
    /// Delete one of a user's webhooks by id. Owner-scoped: only a row whose `user_sub` matches
    /// `key` AND whose id matches is removed. Returns the number of rows deleted (0 or 1).
    async fn delete_webhook(&self, key: &str, id: &str) -> Result<u64, StoreError>;
    /// An owner's delivery preferences. Idempotent: returns [`NotifyPrefs::defaults`] (everything
    /// off) when the owner has never saved any.
    async fn get_prefs(&self, key: &str) -> NotifyPrefs;
    /// Store (replace) an owner's delivery preferences, keyed by `prefs.user_sub`. Idempotent: the
    /// prefs row is upserted and the mute set is replaced wholesale.
    async fn set_prefs(&self, prefs: &NotifyPrefs) -> Result<(), StoreError>;
}

// --------------------------------------------------------------------------------------
// In-memory store (the default; keeps the whole service database-free for dev + tests).
// --------------------------------------------------------------------------------------

#[derive(Default)]
pub struct InMemoryStore {
    notifications: Mutex<Vec<Notification>>,
    subscriptions: Mutex<Vec<PushSubscription>>,
    webhooks: Mutex<Vec<Webhook>>,
    prefs: Mutex<Vec<NotifyPrefs>>,
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
        let mut v: Vec<Webhook> = self
            .webhooks
            .lock()
            .expect("webhooks lock poisoned")
            .iter()
            .filter(|h| h.user_sub == key)
            .cloned()
            .collect();
        // Newest-first for stable UI rendering, ties broken by id.
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id)));
        v
    }

    async fn delete_webhook(&self, key: &str, id: &str) -> Result<u64, StoreError> {
        let mut hooks = self.webhooks.lock().expect("webhooks lock poisoned");
        let before = hooks.len();
        hooks.retain(|h| !(h.user_sub == key && h.id == id));
        Ok((before - hooks.len()) as u64)
    }

    async fn get_prefs(&self, key: &str) -> NotifyPrefs {
        self.prefs
            .lock()
            .expect("prefs lock poisoned")
            .iter()
            .find(|p| p.user_sub == key)
            .cloned()
            .unwrap_or_else(|| NotifyPrefs::defaults(key))
    }

    async fn set_prefs(&self, prefs: &NotifyPrefs) -> Result<(), StoreError> {
        let mut all = self.prefs.lock().expect("prefs lock poisoned");
        match all.iter_mut().find(|p| p.user_sub == prefs.user_sub) {
            Some(existing) => *existing = prefs.clone(),
            None => all.push(prefs.clone()),
        }
        Ok(())
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
        // Additive, backward-compatible column for per-severity muting. Older rows / producers that
        // never set it default to 'info'. `ADD COLUMN IF NOT EXISTS` is idempotent + portable.
        sqlx::query(
            "ALTER TABLE notifications ADD COLUMN IF NOT EXISTS severity TEXT NOT NULL DEFAULT 'info'",
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

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS notify_prefs (\
                 user_sub TEXT PRIMARY KEY, \
                 mute_all BOOLEAN NOT NULL DEFAULT FALSE, \
                 quiet_start BIGINT NOT NULL DEFAULT -1, \
                 quiet_end BIGINT NOT NULL DEFAULT -1, \
                 digest BOOLEAN NOT NULL DEFAULT FALSE, \
                 updated_at BIGINT NOT NULL DEFAULT 0\
             )",
        )
        .execute(&self.pool)
        .await?;

        // One row per muted (kind, value); `kind` is 'source' or 'severity'. The composite PRIMARY
        // KEY keeps a re-save idempotent (no duplicate mutes for the same owner).
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS notify_mutes (\
                 user_sub TEXT NOT NULL, \
                 kind TEXT NOT NULL, \
                 value TEXT NOT NULL, \
                 PRIMARY KEY (user_sub, kind, value)\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_notify_mutes_user ON notify_mutes (user_sub)")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    fn notification_from_row(row: &sqlx::postgres::PgRow) -> Result<Notification, sqlx::Error> {
        Ok(Notification {
            id: row.try_get("id")?,
            user_sub: row.try_get("user_sub")?,
            source: row.try_get("source")?,
            severity: row.try_get("severity")?,
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
                 (id, user_sub, source, severity, title, body, url, created_at, read_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&n.id)
        .bind(&n.user_sub)
        .bind(&n.source)
        .bind(&n.severity)
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
            "SELECT id, user_sub, source, severity, title, body, url, created_at, read_at \
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
            "SELECT id, user_sub, source, severity, title, body, url, created_at, read_at \
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
            "SELECT id, user_sub, url, secret, created_at FROM webhooks WHERE user_sub = $1 \
             ORDER BY created_at DESC, id DESC",
        )
        .bind(key)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::webhook_from_row).collect()
    }

    async fn delete_webhook_async(&self, key: &str, id: &str) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM webhooks WHERE id = $1 AND user_sub = $2")
            .bind(id)
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    async fn get_prefs_async(&self, key: &str) -> Result<NotifyPrefs, sqlx::Error> {
        let mut prefs = match sqlx::query(
            "SELECT user_sub, mute_all, quiet_start, quiet_end, digest, updated_at \
             FROM notify_prefs WHERE user_sub = $1",
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await?
        {
            Some(row) => NotifyPrefs {
                user_sub: row.try_get("user_sub")?,
                mute_all: row.try_get("mute_all")?,
                quiet_start: row.try_get("quiet_start")?,
                quiet_end: row.try_get("quiet_end")?,
                digest: row.try_get("digest")?,
                updated_at: row.try_get("updated_at")?,
                muted_sources: Vec::new(),
                muted_severities: Vec::new(),
            },
            None => return Ok(NotifyPrefs::defaults(key)),
        };
        let rows = sqlx::query("SELECT kind, value FROM notify_mutes WHERE user_sub = $1")
            .bind(key)
            .fetch_all(&self.pool)
            .await?;
        for row in &rows {
            let kind: String = row.try_get("kind")?;
            let value: String = row.try_get("value")?;
            match kind.as_str() {
                "severity" => prefs.muted_severities.push(value),
                _ => prefs.muted_sources.push(value),
            }
        }
        Ok(prefs)
    }

    async fn set_prefs_async(&self, p: &NotifyPrefs) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO notify_prefs \
                 (user_sub, mute_all, quiet_start, quiet_end, digest, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (user_sub) DO UPDATE SET \
                 mute_all = EXCLUDED.mute_all, quiet_start = EXCLUDED.quiet_start, \
                 quiet_end = EXCLUDED.quiet_end, digest = EXCLUDED.digest, \
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(&p.user_sub)
        .bind(p.mute_all)
        .bind(p.quiet_start)
        .bind(p.quiet_end)
        .bind(p.digest)
        .bind(p.updated_at)
        .execute(&mut *tx)
        .await?;
        // Replace the mute set wholesale so a save is authoritative and idempotent.
        sqlx::query("DELETE FROM notify_mutes WHERE user_sub = $1")
            .bind(&p.user_sub)
            .execute(&mut *tx)
            .await?;
        for value in &p.muted_sources {
            sqlx::query(
                "INSERT INTO notify_mutes (user_sub, kind, value) VALUES ($1, 'source', $2) \
                 ON CONFLICT (user_sub, kind, value) DO NOTHING",
            )
            .bind(&p.user_sub)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        }
        for value in &p.muted_severities {
            sqlx::query(
                "INSERT INTO notify_mutes (user_sub, kind, value) VALUES ($1, 'severity', $2) \
                 ON CONFLICT (user_sub, kind, value) DO NOTHING",
            )
            .bind(&p.user_sub)
            .bind(value)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
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

    async fn delete_webhook(&self, key: &str, id: &str) -> Result<u64, StoreError> {
        self.delete_webhook_async(key, id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn get_prefs(&self, key: &str) -> NotifyPrefs {
        self.get_prefs_async(key).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg get_prefs failed");
            NotifyPrefs::defaults(key)
        })
    }

    async fn set_prefs(&self, prefs: &NotifyPrefs) -> Result<(), StoreError> {
        self.set_prefs_async(prefs)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_never_suppress() {
        let p = NotifyPrefs::defaults("u_1");
        assert!(!p.suppresses_realtime("loom", "info", 600));
        assert!(!p.in_quiet_hours(0));
        assert!(!p.in_quiet_hours(1439));
    }

    #[test]
    fn mute_all_and_digest_suppress_everything() {
        let mut p = NotifyPrefs::defaults("u_1");
        p.mute_all = true;
        assert!(p.suppresses_realtime("anything", "info", 720));
        p.mute_all = false;
        p.digest = true;
        assert!(p.suppresses_realtime("anything", "info", 720));
    }

    #[test]
    fn per_source_and_severity_mute_is_case_insensitive() {
        let mut p = NotifyPrefs::defaults("u_1");
        p.muted_sources = vec!["Beacon".to_string()];
        p.muted_severities = vec!["critical".to_string()];
        assert!(p.suppresses_realtime("beacon", "info", 720));
        assert!(!p.suppresses_realtime("loom", "info", 720));
        assert!(p.suppresses_realtime("loom", "CRITICAL", 720));
    }

    #[test]
    fn quiet_hours_same_day_window() {
        let mut p = NotifyPrefs::defaults("u_1");
        p.quiet_start = 9 * 60; // 09:00
        p.quiet_end = 17 * 60; // 17:00
        assert!(!p.in_quiet_hours(8 * 60 + 59));
        assert!(p.in_quiet_hours(9 * 60));
        assert!(p.in_quiet_hours(12 * 60));
        assert!(!p.in_quiet_hours(17 * 60)); // end is exclusive
    }

    #[test]
    fn quiet_hours_wrap_past_midnight() {
        let mut p = NotifyPrefs::defaults("u_1");
        p.quiet_start = 22 * 60; // 22:00
        p.quiet_end = 7 * 60; // 07:00 next day
        assert!(p.in_quiet_hours(23 * 60));
        assert!(p.in_quiet_hours(2 * 60));
        assert!(p.in_quiet_hours(6 * 60 + 59));
        assert!(!p.in_quiet_hours(7 * 60)); // end is exclusive
        assert!(!p.in_quiet_hours(12 * 60));
    }

    #[test]
    fn zero_width_quiet_window_is_never_quiet() {
        let mut p = NotifyPrefs::defaults("u_1");
        p.quiet_start = 8 * 60;
        p.quiet_end = 8 * 60;
        assert!(!p.in_quiet_hours(8 * 60));
    }

    #[tokio::test]
    async fn in_memory_prefs_roundtrip_is_idempotent() {
        let store = InMemoryStore::new();
        // Unset owner reads the synthetic defaults.
        let got = store.get_prefs("u_1").await;
        assert!(!got.mute_all);
        assert!(got.muted_sources.is_empty());

        let mut prefs = NotifyPrefs::defaults("u_1");
        prefs.mute_all = true;
        prefs.muted_sources = vec!["beacon".to_string()];
        store.set_prefs(&prefs).await.unwrap();
        // A second save replaces (not appends) the mute set.
        prefs.muted_sources = vec!["loom".to_string(), "sanctum".to_string()];
        store.set_prefs(&prefs).await.unwrap();

        let got = store.get_prefs("u_1").await;
        assert!(got.mute_all);
        assert_eq!(got.muted_sources, vec!["loom".to_string(), "sanctum".to_string()]);
    }
}
