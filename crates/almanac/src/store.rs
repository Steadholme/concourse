//! Calendar + contacts storage: `events` and `contacts`, both scoped to an owner subject.
//!
//! `Store` is a small async trait with an in-memory and a PostgreSQL implementation, mirroring
//! the keystone/lattice/watchtower seam: handlers depend only on the trait, so a FusionDB-backed
//! store can drop in later. The PostgreSQL layer uses ONLY portable standard SQL
//! (TEXT/BIGINT/BOOLEAN, PRIMARY KEY/NOT NULL/DEFAULT, `INSERT .. ON CONFLICT .. DO UPDATE`,
//! plain indexes) and runtime queries (no compile-time macros), so the build needs NO database
//! and the same statements later run unchanged on FusionDB over pgwire.
//!
//! EVERY read/write takes `owner_sub` and filters by it, so one user can never see or mutate
//! another's rows. `owner_sub` always comes from the gateway-injected `X-Auth-Subject`, never
//! from a client field. The optional text columns are `NOT NULL DEFAULT ''` rather than nullable,
//! so the Rust types stay plain `String` with no `Option` edge cases.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use thiserror::Error;

use crate::config::{CONTACT_LIST_LIMIT, EVENT_LIST_LIMIT};

/// Storage failure surfaced to the handler layer (mapped to a 500).
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("store error: {0}")]
    Backend(String),
}

/// A calendar event. `id` is a random hex string; `owner_sub` scopes it to one user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub id: String,
    pub owner_sub: String,
    pub title: String,
    /// UTC epoch ms.
    pub starts_at: i64,
    /// UTC epoch ms (>= `starts_at`).
    pub ends_at: i64,
    pub all_day: bool,
    pub location: String,
    pub notes: String,
    pub created_at: i64,
}

/// An address-book contact, scoped to one owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contact {
    pub id: String,
    pub owner_sub: String,
    pub name: String,
    pub email: String,
    pub phone: String,
    pub notes: String,
    pub created_at: i64,
}

/// Pluggable store. Methods are `async`: the axum handlers `.await` them directly on the serving
/// runtime, and `PgStore` drives sqlx natively, so a worker thread is never blocked on a DB
/// round-trip (no `block_in_place`, no sync-over-async bridge).
#[async_trait]
pub trait Store: Send + Sync {
    /// All of the owner's events, ordered by `starts_at` ascending, capped for a single load.
    async fn list_events(&self, owner_sub: &str) -> Result<Vec<Event>, StoreError>;

    /// One event by id, only if it belongs to `owner_sub`.
    async fn get_event(&self, owner_sub: &str, id: &str) -> Result<Option<Event>, StoreError>;

    /// Create or update an event (insert, or update when the id already exists AND is owned by
    /// the same subject). Returns the stored event.
    async fn upsert_event(&self, event: Event) -> Result<Event, StoreError>;

    /// Delete an event; returns `true` if a row owned by `owner_sub` was removed.
    async fn delete_event(&self, owner_sub: &str, id: &str) -> Result<bool, StoreError>;

    /// All of the owner's contacts, ordered by `name` (case-insensitive), capped.
    async fn list_contacts(&self, owner_sub: &str) -> Result<Vec<Contact>, StoreError>;

    /// One contact by id, only if it belongs to `owner_sub`.
    async fn get_contact(&self, owner_sub: &str, id: &str) -> Result<Option<Contact>, StoreError>;

    /// Create or update a contact (same ownership guard as events). Returns the stored contact.
    async fn upsert_contact(&self, contact: Contact) -> Result<Contact, StoreError>;

    /// Delete a contact; returns `true` if a row owned by `owner_sub` was removed.
    async fn delete_contact(&self, owner_sub: &str, id: &str) -> Result<bool, StoreError>;
}

// --------------------------------------------------------------------------------------
// In-memory store (the default; keeps the whole service database-free for dev + tests).
// --------------------------------------------------------------------------------------

#[derive(Default)]
struct MemData {
    events: HashMap<String, Event>,
    contacts: HashMap<String, Contact>,
}

/// In-memory `Store`. A single `Mutex` guards both maps.
#[derive(Default)]
pub struct InMemoryStore {
    data: Mutex<MemData>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for InMemoryStore {
    async fn list_events(&self, owner_sub: &str) -> Result<Vec<Event>, StoreError> {
        let data = self.data.lock().expect("almanac store lock poisoned");
        let mut events: Vec<Event> = data
            .events
            .values()
            .filter(|e| e.owner_sub == owner_sub)
            .cloned()
            .collect();
        events.sort_by(|a, b| a.starts_at.cmp(&b.starts_at).then_with(|| a.id.cmp(&b.id)));
        events.truncate(EVENT_LIST_LIMIT);
        Ok(events)
    }

    async fn get_event(&self, owner_sub: &str, id: &str) -> Result<Option<Event>, StoreError> {
        let data = self.data.lock().expect("almanac store lock poisoned");
        Ok(data
            .events
            .get(id)
            .filter(|e| e.owner_sub == owner_sub)
            .cloned())
    }

    async fn upsert_event(&self, event: Event) -> Result<Event, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        // Ownership guard: never overwrite a row owned by a different subject.
        if let Some(existing) = data.events.get(&event.id) {
            if existing.owner_sub != event.owner_sub {
                return Ok(existing.clone());
            }
        }
        data.events.insert(event.id.clone(), event.clone());
        Ok(event)
    }

    async fn delete_event(&self, owner_sub: &str, id: &str) -> Result<bool, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        let owned = data
            .events
            .get(id)
            .map(|e| e.owner_sub == owner_sub)
            .unwrap_or(false);
        if owned {
            data.events.remove(id);
        }
        Ok(owned)
    }

    async fn list_contacts(&self, owner_sub: &str) -> Result<Vec<Contact>, StoreError> {
        let data = self.data.lock().expect("almanac store lock poisoned");
        let mut contacts: Vec<Contact> = data
            .contacts
            .values()
            .filter(|c| c.owner_sub == owner_sub)
            .cloned()
            .collect();
        contacts.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        contacts.truncate(CONTACT_LIST_LIMIT);
        Ok(contacts)
    }

    async fn get_contact(&self, owner_sub: &str, id: &str) -> Result<Option<Contact>, StoreError> {
        let data = self.data.lock().expect("almanac store lock poisoned");
        Ok(data
            .contacts
            .get(id)
            .filter(|c| c.owner_sub == owner_sub)
            .cloned())
    }

    async fn upsert_contact(&self, contact: Contact) -> Result<Contact, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        if let Some(existing) = data.contacts.get(&contact.id) {
            if existing.owner_sub != contact.owner_sub {
                return Ok(existing.clone());
            }
        }
        data.contacts.insert(contact.id.clone(), contact.clone());
        Ok(contact)
    }

    async fn delete_contact(&self, owner_sub: &str, id: &str) -> Result<bool, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        let owned = data
            .contacts
            .get(id)
            .map(|c| c.owner_sub == owner_sub)
            .unwrap_or(false);
        if owned {
            data.contacts.remove(id);
        }
        Ok(owned)
    }
}

// --------------------------------------------------------------------------------------
// PostgreSQL-backed store (portable: standard SQL, runtime queries, no macros).
// --------------------------------------------------------------------------------------
//
// Selected at runtime by `ALMANAC_STORE=postgres`. The `Store` trait is async, so each method
// uses sqlx natively and the handlers `.await` it on the serving runtime — there is NO
// `block_in_place` and NO sync-over-async. The upsert `ON CONFLICT (id) DO UPDATE ... WHERE
// <table>.owner_sub = EXCLUDED.owner_sub` clause is a second ownership guard at the SQL layer.

use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use sqlx::Row;

/// PostgreSQL-backed [`Store`].
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

    /// Idempotent, portable migration. Standard SQL only — safe to run on every startup.
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS events (\
                 id TEXT PRIMARY KEY, \
                 owner_sub TEXT NOT NULL, \
                 title TEXT NOT NULL, \
                 starts_at BIGINT NOT NULL, \
                 ends_at BIGINT NOT NULL, \
                 all_day BOOLEAN NOT NULL DEFAULT FALSE, \
                 location TEXT NOT NULL DEFAULT '', \
                 notes TEXT NOT NULL DEFAULT '', \
                 created_at BIGINT NOT NULL\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_events_owner_start ON events (owner_sub, starts_at)",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS contacts (\
                 id TEXT PRIMARY KEY, \
                 owner_sub TEXT NOT NULL, \
                 name TEXT NOT NULL, \
                 email TEXT NOT NULL DEFAULT '', \
                 phone TEXT NOT NULL DEFAULT '', \
                 notes TEXT NOT NULL DEFAULT '', \
                 created_at BIGINT NOT NULL\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_contacts_owner_name ON contacts (owner_sub, name)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    fn event_from_row(row: &PgRow) -> Result<Event, sqlx::Error> {
        Ok(Event {
            id: row.try_get("id")?,
            owner_sub: row.try_get("owner_sub")?,
            title: row.try_get("title")?,
            starts_at: row.try_get("starts_at")?,
            ends_at: row.try_get("ends_at")?,
            all_day: row.try_get("all_day")?,
            location: row.try_get("location")?,
            notes: row.try_get("notes")?,
            created_at: row.try_get("created_at")?,
        })
    }

    fn contact_from_row(row: &PgRow) -> Result<Contact, sqlx::Error> {
        Ok(Contact {
            id: row.try_get("id")?,
            owner_sub: row.try_get("owner_sub")?,
            name: row.try_get("name")?,
            email: row.try_get("email")?,
            phone: row.try_get("phone")?,
            notes: row.try_get("notes")?,
            created_at: row.try_get("created_at")?,
        })
    }

    async fn list_events_async(&self, owner_sub: &str) -> Result<Vec<Event>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, owner_sub, title, starts_at, ends_at, all_day, location, notes, created_at \
             FROM events WHERE owner_sub = $1 ORDER BY starts_at ASC, id ASC LIMIT $2",
        )
        .bind(owner_sub)
        .bind(EVENT_LIST_LIMIT as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::event_from_row).collect()
    }

    async fn get_event_async(
        &self,
        owner_sub: &str,
        id: &str,
    ) -> Result<Option<Event>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, owner_sub, title, starts_at, ends_at, all_day, location, notes, created_at \
             FROM events WHERE id = $1 AND owner_sub = $2",
        )
        .bind(id)
        .bind(owner_sub)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(Self::event_from_row).transpose()
    }

    async fn upsert_event_async(&self, e: &Event) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO events \
                 (id, owner_sub, title, starts_at, ends_at, all_day, location, notes, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (id) DO UPDATE SET \
                 title = EXCLUDED.title, \
                 starts_at = EXCLUDED.starts_at, \
                 ends_at = EXCLUDED.ends_at, \
                 all_day = EXCLUDED.all_day, \
                 location = EXCLUDED.location, \
                 notes = EXCLUDED.notes \
             WHERE events.owner_sub = EXCLUDED.owner_sub",
        )
        .bind(&e.id)
        .bind(&e.owner_sub)
        .bind(&e.title)
        .bind(e.starts_at)
        .bind(e.ends_at)
        .bind(e.all_day)
        .bind(&e.location)
        .bind(&e.notes)
        .bind(e.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_event_async(&self, owner_sub: &str, id: &str) -> Result<bool, sqlx::Error> {
        let res = sqlx::query("DELETE FROM events WHERE id = $1 AND owner_sub = $2")
            .bind(id)
            .bind(owner_sub)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn list_contacts_async(&self, owner_sub: &str) -> Result<Vec<Contact>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, owner_sub, name, email, phone, notes, created_at \
             FROM contacts WHERE owner_sub = $1 ORDER BY lower(name) ASC, id ASC LIMIT $2",
        )
        .bind(owner_sub)
        .bind(CONTACT_LIST_LIMIT as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::contact_from_row).collect()
    }

    async fn get_contact_async(
        &self,
        owner_sub: &str,
        id: &str,
    ) -> Result<Option<Contact>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, owner_sub, name, email, phone, notes, created_at \
             FROM contacts WHERE id = $1 AND owner_sub = $2",
        )
        .bind(id)
        .bind(owner_sub)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(Self::contact_from_row).transpose()
    }

    async fn upsert_contact_async(&self, c: &Contact) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO contacts (id, owner_sub, name, email, phone, notes, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (id) DO UPDATE SET \
                 name = EXCLUDED.name, \
                 email = EXCLUDED.email, \
                 phone = EXCLUDED.phone, \
                 notes = EXCLUDED.notes \
             WHERE contacts.owner_sub = EXCLUDED.owner_sub",
        )
        .bind(&c.id)
        .bind(&c.owner_sub)
        .bind(&c.name)
        .bind(&c.email)
        .bind(&c.phone)
        .bind(&c.notes)
        .bind(c.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_contact_async(&self, owner_sub: &str, id: &str) -> Result<bool, sqlx::Error> {
        let res = sqlx::query("DELETE FROM contacts WHERE id = $1 AND owner_sub = $2")
            .bind(id)
            .bind(owner_sub)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }
}

#[async_trait]
impl Store for PgStore {
    async fn list_events(&self, owner_sub: &str) -> Result<Vec<Event>, StoreError> {
        self.list_events_async(owner_sub)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn get_event(&self, owner_sub: &str, id: &str) -> Result<Option<Event>, StoreError> {
        self.get_event_async(owner_sub, id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn upsert_event(&self, event: Event) -> Result<Event, StoreError> {
        self.upsert_event_async(&event)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(event)
    }

    async fn delete_event(&self, owner_sub: &str, id: &str) -> Result<bool, StoreError> {
        self.delete_event_async(owner_sub, id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_contacts(&self, owner_sub: &str) -> Result<Vec<Contact>, StoreError> {
        self.list_contacts_async(owner_sub)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn get_contact(&self, owner_sub: &str, id: &str) -> Result<Option<Contact>, StoreError> {
        self.get_contact_async(owner_sub, id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn upsert_contact(&self, contact: Contact) -> Result<Contact, StoreError> {
        self.upsert_contact_async(&contact)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(contact)
    }

    async fn delete_contact(&self, owner_sub: &str, id: &str) -> Result<bool, StoreError> {
        self.delete_contact_async(owner_sub, id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, owner: &str, title: &str, starts: i64) -> Event {
        Event {
            id: id.to_string(),
            owner_sub: owner.to_string(),
            title: title.to_string(),
            starts_at: starts,
            ends_at: starts + 3_600_000,
            all_day: false,
            location: String::new(),
            notes: String::new(),
            created_at: starts,
        }
    }

    #[tokio::test]
    async fn events_are_scoped_per_owner() {
        let store = InMemoryStore::new();
        store.upsert_event(ev("e1", "alice", "Standup", 200)).await.unwrap();
        store.upsert_event(ev("e2", "alice", "Lunch", 100)).await.unwrap();
        store.upsert_event(ev("e3", "bob", "Bob only", 150)).await.unwrap();

        let alice = store.list_events("alice").await.unwrap();
        assert_eq!(alice.len(), 2, "only alice's events");
        assert_eq!(alice[0].id, "e2", "ordered by starts_at ascending");
        assert_eq!(alice[1].id, "e1");

        // Bob cannot read or delete alice's event.
        assert!(store.get_event("bob", "e1").await.unwrap().is_none());
        assert!(!store.delete_event("bob", "e1").await.unwrap());
        assert!(store.get_event("alice", "e1").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn upsert_updates_and_guards_owner() {
        let store = InMemoryStore::new();
        store.upsert_event(ev("e1", "alice", "v1", 100)).await.unwrap();
        let mut updated = ev("e1", "alice", "v2", 500);
        updated.created_at = 100;
        store.upsert_event(updated).await.unwrap();
        let got = store.get_event("alice", "e1").await.unwrap().unwrap();
        assert_eq!(got.title, "v2");
        assert_eq!(got.created_at, 100, "created_at preserved by caller");

        // Bob trying to hijack e1 by id is a no-op on the stored (alice) row.
        store.upsert_event(ev("e1", "bob", "stolen", 1)).await.unwrap();
        let still = store.get_event("alice", "e1").await.unwrap().unwrap();
        assert_eq!(still.title, "v2", "ownership guard prevented overwrite");
    }

    #[tokio::test]
    async fn contacts_sorted_and_scoped() {
        let store = InMemoryStore::new();
        let mk = |id: &str, owner: &str, name: &str| Contact {
            id: id.to_string(),
            owner_sub: owner.to_string(),
            name: name.to_string(),
            email: String::new(),
            phone: String::new(),
            notes: String::new(),
            created_at: 0,
        };
        store.upsert_contact(mk("c1", "alice", "Zoe")).await.unwrap();
        store.upsert_contact(mk("c2", "alice", "amy")).await.unwrap();
        store.upsert_contact(mk("c3", "bob", "Carol")).await.unwrap();
        let alice = store.list_contacts("alice").await.unwrap();
        assert_eq!(alice.len(), 2);
        assert_eq!(alice[0].name, "amy", "case-insensitive name order");
        assert_eq!(alice[1].name, "Zoe");
    }
}
