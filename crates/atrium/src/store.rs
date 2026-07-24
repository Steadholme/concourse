//! The per-viewer row-action overlay.
//!
//! Atrium federates three sibling databases **READ-ONLY** and owns NONE of that data (see
//! [`crate::source`]). Yet the inbox has to be actionable: a viewer must be able to mark-read or
//! dismiss an aggregated row without a full reload. The pragmatic, non-invasive design that keeps
//! the source databases untouched is a small overlay Atrium DOES own: one table recording that a
//! given viewer acted on a given `(source, row_key)`. On every aggregate those keys are hidden
//! from the unread view. Marking-read and dismissing are the same operation to the OVERLAY (both
//! remove the row from "what's new"); only the audit label and the recorded `action` differ.
//!
//! The seam mirrors the [`crate::source::Source`] one: the handlers depend only on the async
//! [`ActionStore`] trait, so an in-memory fake ([`InMemoryActionStore`], the dev/test default) and
//! the Postgres-backed [`PgActionStore`] are interchangeable behind `Arc<dyn ActionStore>`.
//!
//! PORTABLE SQL ONLY: `TEXT` / `BIGINT` columns, an idempotent `CREATE TABLE IF NOT EXISTS`, and
//! runtime (non-macro) queries — the same statements later run unchanged on FusionDB over pgwire.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use sqlx::postgres::PgPool;
use sqlx::Row;

/// The per-viewer overlay of acted-on rows. `record` persists one mark-read / dismiss; `hidden`
/// returns, per source slug, the set of row keys the viewer has acted on (to filter the unread
/// view). `hidden` failing is non-fatal to the page: callers treat an error as "nothing hidden"
/// (fail-open) so a down overlay DB degrades to showing everything, never an error.
#[async_trait]
pub trait ActionStore: Send + Sync {
    /// Record that `user_sub` performed `action` (`"read"` | `"dismiss"`) on `row_key` in `source`.
    /// Idempotent: re-acting on the same row just updates the stored action/time.
    async fn record(
        &self,
        user_sub: &str,
        source: &str,
        row_key: &str,
        action: &str,
    ) -> Result<(), String>;

    /// Every `(source slug -> set of hidden row keys)` the viewer has acted on.
    async fn hidden(&self, user_sub: &str) -> Result<HashMap<String, HashSet<String>>, String>;
}

// ---------------------------------------------------------------------------
// In-memory store (dev/tests): a process-local map, no database.
// ---------------------------------------------------------------------------

/// An in-process [`ActionStore`] for dev and tests. Actions live only for the process lifetime.
#[derive(Default)]
pub struct InMemoryActionStore {
    /// `user_sub -> source slug -> {row_key}`.
    inner: Mutex<HashMap<String, HashMap<String, HashSet<String>>>>,
}

impl InMemoryActionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ActionStore for InMemoryActionStore {
    async fn record(
        &self,
        user_sub: &str,
        source: &str,
        row_key: &str,
        _action: &str,
    ) -> Result<(), String> {
        self.inner
            .lock()
            .expect("action store poisoned")
            .entry(user_sub.to_string())
            .or_default()
            .entry(source.to_string())
            .or_default()
            .insert(row_key.to_string());
        Ok(())
    }

    async fn hidden(&self, user_sub: &str) -> Result<HashMap<String, HashSet<String>>, String> {
        Ok(self
            .inner
            .lock()
            .expect("action store poisoned")
            .get(user_sub)
            .cloned()
            .unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// Postgres store: Atrium's OWN overlay table (portable, runtime queries).
// ---------------------------------------------------------------------------

/// A Postgres-backed [`ActionStore`] over Atrium's own `atrium_row_actions` table.
pub struct PgActionStore {
    pool: PgPool,
}

impl PgActionStore {
    /// Connect (lazily) and ensure the overlay schema exists. The `CREATE TABLE IF NOT EXISTS` is
    /// idempotent and portable (`TEXT` / `BIGINT` only). Returns an error string only if the schema
    /// bootstrap query fails (a down DB at startup) — the caller then falls back to in-memory.
    pub async fn connect(dsn: &str) -> Result<Self, String> {
        let pool = crate::source::lazy_pool(dsn).map_err(|e| e.to_string())?;
        let store = Self { pool };
        store.ensure_schema().await?;
        Ok(store)
    }

    async fn ensure_schema(&self) -> Result<(), String> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS atrium_row_actions ( \
                 user_sub   TEXT   NOT NULL, \
                 source     TEXT   NOT NULL, \
                 row_key    TEXT   NOT NULL, \
                 action     TEXT   NOT NULL, \
                 created_at BIGINT NOT NULL, \
                 PRIMARY KEY (user_sub, source, row_key) \
             )",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[async_trait]
impl ActionStore for PgActionStore {
    async fn record(
        &self,
        user_sub: &str,
        source: &str,
        row_key: &str,
        action: &str,
    ) -> Result<(), String> {
        // Upsert: re-acting on the same row keeps the row hidden and records the latest action/time.
        sqlx::query(
            "INSERT INTO atrium_row_actions (user_sub, source, row_key, action, created_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (user_sub, source, row_key) \
             DO UPDATE SET action = EXCLUDED.action, created_at = EXCLUDED.created_at",
        )
        .bind(user_sub)
        .bind(source)
        .bind(row_key)
        .bind(action)
        .bind(crate::now_secs())
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn hidden(&self, user_sub: &str) -> Result<HashMap<String, HashSet<String>>, String> {
        let rows =
            sqlx::query("SELECT source, row_key FROM atrium_row_actions WHERE user_sub = $1")
                .bind(user_sub)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| e.to_string())?;

        let mut out: HashMap<String, HashSet<String>> = HashMap::new();
        for r in &rows {
            let source: String = r.try_get("source").map_err(|e| e.to_string())?;
            let key: String = r.try_get("row_key").map_err(|e| e.to_string())?;
            out.entry(source).or_default().insert(key);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_records_and_hides_per_source() {
        let store = InMemoryActionStore::new();
        // A brand-new viewer hides nothing.
        assert!(store.hidden("u").await.unwrap().is_empty());

        store
            .record("u", "feed", "item-1", "dismiss")
            .await
            .unwrap();
        store.record("u", "feed", "item-2", "read").await.unwrap();
        store.record("u", "chat", "room-9", "read").await.unwrap();
        // A different viewer is isolated.
        store
            .record("other", "feed", "item-1", "read")
            .await
            .unwrap();

        let hidden = store.hidden("u").await.unwrap();
        assert_eq!(hidden.get("feed").unwrap().len(), 2);
        assert!(hidden.get("feed").unwrap().contains("item-1"));
        assert!(hidden.get("chat").unwrap().contains("room-9"));
        assert!(!hidden.contains_key("notifications"));

        // Re-acting is idempotent (still one key, no duplicate).
        store
            .record("u", "chat", "room-9", "dismiss")
            .await
            .unwrap();
        assert_eq!(
            store.hidden("u").await.unwrap().get("chat").unwrap().len(),
            1
        );

        let other = store.hidden("other").await.unwrap();
        assert_eq!(other.get("feed").unwrap().len(), 1);
    }
}
