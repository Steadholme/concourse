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
    /// RFC 5545 RRULE for a recurring series (e.g. `FREQ=WEEKLY;BYDAY=MO,WE`); empty = a one-off.
    /// The row IS the series (its `starts_at`/`ends_at` are DTSTART); occurrences are expanded
    /// virtually at render time by [`crate::rrule`].
    pub rrule: String,
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

/// Per-owner display preferences. There is at most ONE row per subject (`owner_sub` is the key),
/// so it is fetched with a plain default when the user has never saved: the whole app keeps
/// working (renders in UTC, Sunday-first) with no settings row present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub owner_sub: String,
    /// Timezone string, e.g. `UTC`, `UTC+08:00`, `UTC-05:00`. Interpreted as a fixed UTC offset.
    pub timezone: String,
    /// First column of the month grid: `sunday` (default) or `monday`.
    pub week_start: String,
    /// Epoch ms of the last save (0 for the synthesized default).
    pub updated_at: i64,
}

impl Settings {
    /// The defaults used until the owner saves anything: UTC, Sunday-first.
    pub fn default_for(owner_sub: &str) -> Self {
        Settings {
            owner_sub: owner_sub.to_string(),
            timezone: "UTC".to_string(),
            week_start: "sunday".to_string(),
            updated_at: 0,
        }
    }
}

/// An invited attendee on one event. Owned (like everything) by the event's `owner_sub`; the
/// per-attendee `token` is an unguessable RSVP capability handed out in a public link, so its
/// lookup is the ONLY store read that is NOT owner-scoped. `status` is one of the four RFC 5545
/// PARTSTAT keywords (see [`ATTENDEE_STATUSES`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attendee {
    pub id: String,
    pub event_id: String,
    pub owner_sub: String,
    pub email: String,
    pub name: String,
    /// `needs-action` (default) | `accepted` | `declined` | `tentative`.
    pub status: String,
    /// Unguessable RSVP capability (64 hex chars); the public `/rsvp/{token}` link carries it.
    pub token: String,
    pub created_at: i64,
}

/// The RSVP states an attendee row may hold. `needs-action` is the initial state; the public RSVP
/// link only ever sets one of the latter three.
pub const ATTENDEE_STATUSES: [&str; 4] = ["needs-action", "accepted", "declined", "tentative"];

/// A pre-event reminder: fire `minutes_before` the owning event's start by delivering an in-app
/// notification. `delivered_at` is `0` until a due-scan hands it off (so a reminder fires at most
/// once); the scan filters on it with a bounded query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reminder {
    pub id: String,
    pub event_id: String,
    pub owner_sub: String,
    pub minutes_before: i64,
    /// Epoch ms of successful hand-off, or `0` while still pending.
    pub delivered_at: i64,
    pub created_at: i64,
}

/// A reminder joined to its (still-upcoming) event, produced by [`Store::due_reminders`]. Carries
/// everything the delivery hook needs to build the notification without a second lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DueReminder {
    pub reminder_id: String,
    pub owner_sub: String,
    pub minutes_before: i64,
    pub event_id: String,
    pub event_title: String,
    pub event_starts_at: i64,
    pub event_location: String,
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

    /// The owner's display settings, or [`Settings::default_for`] when no row exists yet.
    async fn get_settings(&self, owner_sub: &str) -> Result<Settings, StoreError>;

    /// Insert or replace the owner's settings row. Returns the stored value.
    async fn upsert_settings(&self, settings: Settings) -> Result<Settings, StoreError>;

    // --- Attendees / invites ----------------------------------------------------------------

    /// The event's attendees, ordered by name (case-insensitive), only if the event belongs to
    /// `owner_sub`.
    async fn list_attendees(&self, owner_sub: &str, event_id: &str)
        -> Result<Vec<Attendee>, StoreError>;

    /// Replace the whole attendee set for `(owner_sub, event_id)` with `attendees` (delete-then-
    /// insert). The caller reconciles tokens/status beforehand so retained emails keep their RSVP
    /// links; this call just persists the final set.
    async fn replace_attendees(
        &self,
        owner_sub: &str,
        event_id: &str,
        attendees: Vec<Attendee>,
    ) -> Result<(), StoreError>;

    /// Remove all attendees of an event (used when the event itself is deleted).
    async fn delete_event_attendees(&self, owner_sub: &str, event_id: &str)
        -> Result<(), StoreError>;

    /// Look up one attendee by its RSVP `token`. NOT owner-scoped: the token IS the capability.
    async fn get_attendee_by_token(&self, token: &str) -> Result<Option<Attendee>, StoreError>;

    /// Set an attendee's RSVP status by `token`; returns the updated row (or `None` if unknown).
    async fn set_attendee_status_by_token(
        &self,
        token: &str,
        status: &str,
    ) -> Result<Option<Attendee>, StoreError>;

    // --- Reminders --------------------------------------------------------------------------

    /// The event's reminders, ordered by `minutes_before` ascending, only if the event belongs to
    /// `owner_sub`.
    async fn list_reminders(&self, owner_sub: &str, event_id: &str)
        -> Result<Vec<Reminder>, StoreError>;

    /// Replace the whole reminder set for `(owner_sub, event_id)` with `reminders`.
    async fn replace_reminders(
        &self,
        owner_sub: &str,
        event_id: &str,
        reminders: Vec<Reminder>,
    ) -> Result<(), StoreError>;

    /// Remove all reminders of an event (used when the event itself is deleted).
    async fn delete_event_reminders(&self, owner_sub: &str, event_id: &str)
        -> Result<(), StoreError>;

    /// Pending reminders whose fire time (`event.starts_at - minutes_before`) has arrived while the
    /// event is still upcoming, joined to their event. A SINGLE bounded query (`LIMIT limit`) — the
    /// delivery hook scans on a timer, never a busy loop. Ordered by event start ascending.
    async fn due_reminders(&self, now_ms: i64, limit: usize)
        -> Result<Vec<DueReminder>, StoreError>;

    /// Mark a reminder delivered (sets `delivered_at`), so it never fires twice.
    async fn mark_reminder_delivered(&self, reminder_id: &str, when_ms: i64)
        -> Result<(), StoreError>;
}

// --------------------------------------------------------------------------------------
// In-memory store (the default; keeps the whole service database-free for dev + tests).
// --------------------------------------------------------------------------------------

#[derive(Default)]
struct MemData {
    events: HashMap<String, Event>,
    contacts: HashMap<String, Contact>,
    settings: HashMap<String, Settings>,
    /// Attendees keyed by attendee id.
    attendees: HashMap<String, Attendee>,
    /// Reminders keyed by reminder id.
    reminders: HashMap<String, Reminder>,
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

    async fn get_settings(&self, owner_sub: &str) -> Result<Settings, StoreError> {
        let data = self.data.lock().expect("almanac store lock poisoned");
        Ok(data
            .settings
            .get(owner_sub)
            .cloned()
            .unwrap_or_else(|| Settings::default_for(owner_sub)))
    }

    async fn upsert_settings(&self, settings: Settings) -> Result<Settings, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        data.settings.insert(settings.owner_sub.clone(), settings.clone());
        Ok(settings)
    }

    async fn list_attendees(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<Vec<Attendee>, StoreError> {
        let data = self.data.lock().expect("almanac store lock poisoned");
        let mut out: Vec<Attendee> = data
            .attendees
            .values()
            .filter(|a| a.owner_sub == owner_sub && a.event_id == event_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.email.to_lowercase().cmp(&b.email.to_lowercase()))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }

    async fn replace_attendees(
        &self,
        owner_sub: &str,
        event_id: &str,
        attendees: Vec<Attendee>,
    ) -> Result<(), StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        data.attendees
            .retain(|_, a| !(a.owner_sub == owner_sub && a.event_id == event_id));
        for a in attendees {
            if a.owner_sub == owner_sub && a.event_id == event_id {
                data.attendees.insert(a.id.clone(), a);
            }
        }
        Ok(())
    }

    async fn delete_event_attendees(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        data.attendees
            .retain(|_, a| !(a.owner_sub == owner_sub && a.event_id == event_id));
        Ok(())
    }

    async fn get_attendee_by_token(&self, token: &str) -> Result<Option<Attendee>, StoreError> {
        let data = self.data.lock().expect("almanac store lock poisoned");
        Ok(data
            .attendees
            .values()
            .find(|a| a.token == token)
            .cloned())
    }

    async fn set_attendee_status_by_token(
        &self,
        token: &str,
        status: &str,
    ) -> Result<Option<Attendee>, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        let id = data
            .attendees
            .values()
            .find(|a| a.token == token)
            .map(|a| a.id.clone());
        match id {
            Some(id) => {
                let a = data.attendees.get_mut(&id).expect("just found");
                a.status = status.to_string();
                Ok(Some(a.clone()))
            }
            None => Ok(None),
        }
    }

    async fn list_reminders(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<Vec<Reminder>, StoreError> {
        let data = self.data.lock().expect("almanac store lock poisoned");
        let mut out: Vec<Reminder> = data
            .reminders
            .values()
            .filter(|r| r.owner_sub == owner_sub && r.event_id == event_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            a.minutes_before
                .cmp(&b.minutes_before)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }

    async fn replace_reminders(
        &self,
        owner_sub: &str,
        event_id: &str,
        reminders: Vec<Reminder>,
    ) -> Result<(), StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        data.reminders
            .retain(|_, r| !(r.owner_sub == owner_sub && r.event_id == event_id));
        for r in reminders {
            if r.owner_sub == owner_sub && r.event_id == event_id {
                data.reminders.insert(r.id.clone(), r);
            }
        }
        Ok(())
    }

    async fn delete_event_reminders(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        data.reminders
            .retain(|_, r| !(r.owner_sub == owner_sub && r.event_id == event_id));
        Ok(())
    }

    async fn due_reminders(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<DueReminder>, StoreError> {
        let data = self.data.lock().expect("almanac store lock poisoned");
        let mut out: Vec<DueReminder> = data
            .reminders
            .values()
            .filter(|r| r.delivered_at == 0)
            .filter_map(|r| {
                let event = data.events.get(&r.event_id)?;
                if event.owner_sub != r.owner_sub {
                    return None;
                }
                let fire_at = event.starts_at - r.minutes_before * 60_000;
                // Due once the fire time has arrived AND the event has not yet started.
                if fire_at <= now_ms && event.starts_at >= now_ms {
                    Some(DueReminder {
                        reminder_id: r.id.clone(),
                        owner_sub: r.owner_sub.clone(),
                        minutes_before: r.minutes_before,
                        event_id: event.id.clone(),
                        event_title: event.title.clone(),
                        event_starts_at: event.starts_at,
                        event_location: event.location.clone(),
                    })
                } else {
                    None
                }
            })
            .collect();
        out.sort_by(|a, b| {
            a.event_starts_at
                .cmp(&b.event_starts_at)
                .then_with(|| a.reminder_id.cmp(&b.reminder_id))
        });
        out.truncate(limit);
        Ok(out)
    }

    async fn mark_reminder_delivered(
        &self,
        reminder_id: &str,
        when_ms: i64,
    ) -> Result<(), StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        if let Some(r) = data.reminders.get_mut(reminder_id) {
            r.delivered_at = when_ms;
        }
        Ok(())
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
        // Recurrence: an optional RRULE column added idempotently so an existing `events` table
        // migrates forward with no data loss. Portable standard SQL (TEXT NOT NULL DEFAULT '').
        sqlx::query("ALTER TABLE events ADD COLUMN IF NOT EXISTS rrule TEXT NOT NULL DEFAULT ''")
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

        // Per-owner settings: one row per subject. Created as a new table; the ADD COLUMN IF NOT
        // EXISTS statements make the migration forward-safe (idempotent) if an older, narrower
        // `settings` table already exists. Portable standard SQL only.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS settings (\
                 owner_sub TEXT PRIMARY KEY\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE settings ADD COLUMN IF NOT EXISTS timezone TEXT NOT NULL DEFAULT 'UTC'",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE settings ADD COLUMN IF NOT EXISTS week_start TEXT NOT NULL DEFAULT 'sunday'",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE settings ADD COLUMN IF NOT EXISTS updated_at BIGINT NOT NULL DEFAULT 0",
        )
        .execute(&self.pool)
        .await?;

        // Invites / attendees: one row per invited attendee of an event. `token` is the unguessable
        // public RSVP capability (unique). Portable standard SQL only.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS event_attendees (\
                 id TEXT PRIMARY KEY, \
                 event_id TEXT NOT NULL, \
                 owner_sub TEXT NOT NULL, \
                 email TEXT NOT NULL DEFAULT '', \
                 name TEXT NOT NULL DEFAULT '', \
                 status TEXT NOT NULL DEFAULT 'needs-action', \
                 token TEXT NOT NULL, \
                 created_at BIGINT NOT NULL\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_attendees_owner_event ON event_attendees (owner_sub, event_id)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_attendees_token ON event_attendees (token)",
        )
        .execute(&self.pool)
        .await?;

        // Reminders: one row per (event, minutes_before). `delivered_at = 0` means pending; the
        // bounded due-scan filters on it. Portable standard SQL only.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS event_reminders (\
                 id TEXT PRIMARY KEY, \
                 event_id TEXT NOT NULL, \
                 owner_sub TEXT NOT NULL, \
                 minutes_before BIGINT NOT NULL, \
                 delivered_at BIGINT NOT NULL DEFAULT 0, \
                 created_at BIGINT NOT NULL\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_reminders_owner_event ON event_reminders (owner_sub, event_id)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_reminders_due ON event_reminders (delivered_at)",
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
            rrule: row.try_get("rrule")?,
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

    fn settings_from_row(row: &PgRow) -> Result<Settings, sqlx::Error> {
        Ok(Settings {
            owner_sub: row.try_get("owner_sub")?,
            timezone: row.try_get("timezone")?,
            week_start: row.try_get("week_start")?,
            updated_at: row.try_get("updated_at")?,
        })
    }

    fn attendee_from_row(row: &PgRow) -> Result<Attendee, sqlx::Error> {
        Ok(Attendee {
            id: row.try_get("id")?,
            event_id: row.try_get("event_id")?,
            owner_sub: row.try_get("owner_sub")?,
            email: row.try_get("email")?,
            name: row.try_get("name")?,
            status: row.try_get("status")?,
            token: row.try_get("token")?,
            created_at: row.try_get("created_at")?,
        })
    }

    fn reminder_from_row(row: &PgRow) -> Result<Reminder, sqlx::Error> {
        Ok(Reminder {
            id: row.try_get("id")?,
            event_id: row.try_get("event_id")?,
            owner_sub: row.try_get("owner_sub")?,
            minutes_before: row.try_get("minutes_before")?,
            delivered_at: row.try_get("delivered_at")?,
            created_at: row.try_get("created_at")?,
        })
    }

    fn due_reminder_from_row(row: &PgRow) -> Result<DueReminder, sqlx::Error> {
        Ok(DueReminder {
            reminder_id: row.try_get("reminder_id")?,
            owner_sub: row.try_get("owner_sub")?,
            minutes_before: row.try_get("minutes_before")?,
            event_id: row.try_get("event_id")?,
            event_title: row.try_get("event_title")?,
            event_starts_at: row.try_get("event_starts_at")?,
            event_location: row.try_get("event_location")?,
        })
    }

    async fn list_events_async(&self, owner_sub: &str) -> Result<Vec<Event>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, owner_sub, title, starts_at, ends_at, all_day, location, notes, rrule, created_at \
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
            "SELECT id, owner_sub, title, starts_at, ends_at, all_day, location, notes, rrule, created_at \
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
                 (id, owner_sub, title, starts_at, ends_at, all_day, location, notes, rrule, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (id) DO UPDATE SET \
                 title = EXCLUDED.title, \
                 starts_at = EXCLUDED.starts_at, \
                 ends_at = EXCLUDED.ends_at, \
                 all_day = EXCLUDED.all_day, \
                 location = EXCLUDED.location, \
                 notes = EXCLUDED.notes, \
                 rrule = EXCLUDED.rrule \
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
        .bind(&e.rrule)
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

    async fn get_settings_async(&self, owner_sub: &str) -> Result<Settings, sqlx::Error> {
        let row = sqlx::query(
            "SELECT owner_sub, timezone, week_start, updated_at FROM settings WHERE owner_sub = $1",
        )
        .bind(owner_sub)
        .fetch_optional(&self.pool)
        .await?;
        match row.as_ref() {
            Some(r) => Self::settings_from_row(r),
            None => Ok(Settings::default_for(owner_sub)),
        }
    }

    async fn upsert_settings_async(&self, s: &Settings) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO settings (owner_sub, timezone, week_start, updated_at) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (owner_sub) DO UPDATE SET \
                 timezone = EXCLUDED.timezone, \
                 week_start = EXCLUDED.week_start, \
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(&s.owner_sub)
        .bind(&s.timezone)
        .bind(&s.week_start)
        .bind(s.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_attendees_async(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<Vec<Attendee>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, event_id, owner_sub, email, name, status, token, created_at \
             FROM event_attendees WHERE owner_sub = $1 AND event_id = $2 \
             ORDER BY lower(name) ASC, lower(email) ASC, id ASC",
        )
        .bind(owner_sub)
        .bind(event_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::attendee_from_row).collect()
    }

    async fn replace_attendees_async(
        &self,
        owner_sub: &str,
        event_id: &str,
        attendees: &[Attendee],
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM event_attendees WHERE owner_sub = $1 AND event_id = $2")
            .bind(owner_sub)
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        for a in attendees {
            if a.owner_sub != owner_sub || a.event_id != event_id {
                continue;
            }
            sqlx::query(
                "INSERT INTO event_attendees \
                     (id, event_id, owner_sub, email, name, status, token, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(&a.id)
            .bind(&a.event_id)
            .bind(&a.owner_sub)
            .bind(&a.email)
            .bind(&a.name)
            .bind(&a.status)
            .bind(&a.token)
            .bind(a.created_at)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    async fn delete_event_attendees_async(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM event_attendees WHERE owner_sub = $1 AND event_id = $2")
            .bind(owner_sub)
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn get_attendee_by_token_async(
        &self,
        token: &str,
    ) -> Result<Option<Attendee>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, event_id, owner_sub, email, name, status, token, created_at \
             FROM event_attendees WHERE token = $1",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(Self::attendee_from_row).transpose()
    }

    async fn set_attendee_status_by_token_async(
        &self,
        token: &str,
        status: &str,
    ) -> Result<Option<Attendee>, sqlx::Error> {
        sqlx::query("UPDATE event_attendees SET status = $1 WHERE token = $2")
            .bind(status)
            .bind(token)
            .execute(&self.pool)
            .await?;
        self.get_attendee_by_token_async(token).await
    }

    async fn list_reminders_async(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<Vec<Reminder>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, event_id, owner_sub, minutes_before, delivered_at, created_at \
             FROM event_reminders WHERE owner_sub = $1 AND event_id = $2 \
             ORDER BY minutes_before ASC, id ASC",
        )
        .bind(owner_sub)
        .bind(event_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::reminder_from_row).collect()
    }

    async fn replace_reminders_async(
        &self,
        owner_sub: &str,
        event_id: &str,
        reminders: &[Reminder],
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM event_reminders WHERE owner_sub = $1 AND event_id = $2")
            .bind(owner_sub)
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        for r in reminders {
            if r.owner_sub != owner_sub || r.event_id != event_id {
                continue;
            }
            sqlx::query(
                "INSERT INTO event_reminders \
                     (id, event_id, owner_sub, minutes_before, delivered_at, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(&r.id)
            .bind(&r.event_id)
            .bind(&r.owner_sub)
            .bind(r.minutes_before)
            .bind(r.delivered_at)
            .bind(r.created_at)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    async fn delete_event_reminders_async(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM event_reminders WHERE owner_sub = $1 AND event_id = $2")
            .bind(owner_sub)
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn due_reminders_async(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<DueReminder>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT r.id AS reminder_id, r.owner_sub AS owner_sub, \
                    r.minutes_before AS minutes_before, e.id AS event_id, \
                    e.title AS event_title, e.starts_at AS event_starts_at, \
                    e.location AS event_location \
             FROM event_reminders r \
             JOIN events e ON e.id = r.event_id AND e.owner_sub = r.owner_sub \
             WHERE r.delivered_at = 0 \
               AND (e.starts_at - r.minutes_before * 60000) <= $1 \
               AND e.starts_at >= $1 \
             ORDER BY e.starts_at ASC, r.id ASC LIMIT $2",
        )
        .bind(now_ms)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::due_reminder_from_row).collect()
    }

    async fn mark_reminder_delivered_async(
        &self,
        reminder_id: &str,
        when_ms: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE event_reminders SET delivered_at = $1 WHERE id = $2")
            .bind(when_ms)
            .bind(reminder_id)
            .execute(&self.pool)
            .await?;
        Ok(())
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

    async fn get_settings(&self, owner_sub: &str) -> Result<Settings, StoreError> {
        self.get_settings_async(owner_sub)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn upsert_settings(&self, settings: Settings) -> Result<Settings, StoreError> {
        self.upsert_settings_async(&settings)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(settings)
    }

    async fn list_attendees(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<Vec<Attendee>, StoreError> {
        self.list_attendees_async(owner_sub, event_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn replace_attendees(
        &self,
        owner_sub: &str,
        event_id: &str,
        attendees: Vec<Attendee>,
    ) -> Result<(), StoreError> {
        self.replace_attendees_async(owner_sub, event_id, &attendees)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn delete_event_attendees(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), StoreError> {
        self.delete_event_attendees_async(owner_sub, event_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn get_attendee_by_token(&self, token: &str) -> Result<Option<Attendee>, StoreError> {
        self.get_attendee_by_token_async(token)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn set_attendee_status_by_token(
        &self,
        token: &str,
        status: &str,
    ) -> Result<Option<Attendee>, StoreError> {
        self.set_attendee_status_by_token_async(token, status)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_reminders(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<Vec<Reminder>, StoreError> {
        self.list_reminders_async(owner_sub, event_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn replace_reminders(
        &self,
        owner_sub: &str,
        event_id: &str,
        reminders: Vec<Reminder>,
    ) -> Result<(), StoreError> {
        self.replace_reminders_async(owner_sub, event_id, &reminders)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn delete_event_reminders(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), StoreError> {
        self.delete_event_reminders_async(owner_sub, event_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn due_reminders(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<DueReminder>, StoreError> {
        self.due_reminders_async(now_ms, limit)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn mark_reminder_delivered(
        &self,
        reminder_id: &str,
        when_ms: i64,
    ) -> Result<(), StoreError> {
        self.mark_reminder_delivered_async(reminder_id, when_ms)
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
            rrule: String::new(),
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

    #[tokio::test]
    async fn settings_default_then_persist_and_scope() {
        let store = InMemoryStore::new();
        // No row yet => the UTC/Sunday default, echoing the requested owner.
        let def = store.get_settings("alice").await.unwrap();
        assert_eq!(def, Settings::default_for("alice"));
        assert_eq!(def.timezone, "UTC");
        assert_eq!(def.week_start, "sunday");

        store
            .upsert_settings(Settings {
                owner_sub: "alice".to_string(),
                timezone: "UTC+08:00".to_string(),
                week_start: "monday".to_string(),
                updated_at: 42,
            })
            .await
            .unwrap();

        let got = store.get_settings("alice").await.unwrap();
        assert_eq!(got.timezone, "UTC+08:00");
        assert_eq!(got.week_start, "monday");
        assert_eq!(got.updated_at, 42);

        // Another owner is unaffected — still the default.
        assert_eq!(store.get_settings("bob").await.unwrap(), Settings::default_for("bob"));
    }

    fn attendee(id: &str, event: &str, owner: &str, email: &str, token: &str) -> Attendee {
        Attendee {
            id: id.to_string(),
            event_id: event.to_string(),
            owner_sub: owner.to_string(),
            email: email.to_string(),
            name: String::new(),
            status: "needs-action".to_string(),
            token: token.to_string(),
            created_at: 0,
        }
    }

    #[tokio::test]
    async fn attendees_replace_and_token_rsvp() {
        let store = InMemoryStore::new();
        store
            .replace_attendees(
                "alice",
                "e1",
                vec![
                    attendee("a1", "e1", "alice", "guest@x.co", "tok-guest"),
                    attendee("a2", "e1", "alice", "team@x.co", "tok-team"),
                ],
            )
            .await
            .unwrap();
        assert_eq!(store.list_attendees("alice", "e1").await.unwrap().len(), 2);
        // Another owner sees nothing for the same event id.
        assert!(store.list_attendees("bob", "e1").await.unwrap().is_empty());

        // Public token lookup is NOT owner-scoped; RSVP updates status.
        let got = store.get_attendee_by_token("tok-guest").await.unwrap().unwrap();
        assert_eq!(got.email, "guest@x.co");
        let updated = store
            .set_attendee_status_by_token("tok-guest", "accepted")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, "accepted");
        // An unknown token yields None (no panic, no mutation).
        assert!(store.set_attendee_status_by_token("nope", "declined").await.unwrap().is_none());

        // Replace preserves only the final set; old rows are gone.
        store
            .replace_attendees("alice", "e1", vec![attendee("a3", "e1", "alice", "solo@x.co", "tok-solo")])
            .await
            .unwrap();
        assert_eq!(store.list_attendees("alice", "e1").await.unwrap().len(), 1);
        assert!(store.get_attendee_by_token("tok-guest").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reminders_due_scan_and_mark_delivered() {
        let store = InMemoryStore::new();
        // Event starts at t=1_000_000 ms.
        let start = 1_000_000i64;
        store.upsert_event(ev("e1", "alice", "Standup", start)).await.unwrap();
        // Two reminders: 10 min before (fires at start-600_000) and at-time (fires at start).
        store
            .replace_reminders(
                "alice",
                "e1",
                vec![
                    Reminder { id: "r10".into(), event_id: "e1".into(), owner_sub: "alice".into(), minutes_before: 10, delivered_at: 0, created_at: 0 },
                    Reminder { id: "r0".into(), event_id: "e1".into(), owner_sub: "alice".into(), minutes_before: 0, delivered_at: 0, created_at: 0 },
                ],
            )
            .await
            .unwrap();
        assert_eq!(store.list_reminders("alice", "e1").await.unwrap().len(), 2);

        // Well before either fire time: nothing due.
        assert!(store.due_reminders(start - 700_000, 100).await.unwrap().is_empty());

        // After the 10-min fire time but before start: only r10 is due.
        let due = store.due_reminders(start - 300_000, 100).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].reminder_id, "r10");
        assert_eq!(due[0].event_title, "Standup");

        // Deliver r10; it no longer appears. At start, r0 fires.
        store.mark_reminder_delivered("r10", 42).await.unwrap();
        let due = store.due_reminders(start, 100).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].reminder_id, "r0");

        // Past the start instant: the upcoming-only guard drops everything.
        store.mark_reminder_delivered("r0", 43).await.unwrap();
        assert!(store.due_reminders(start + 1, 100).await.unwrap().is_empty());

        // Limit bounds the result set.
        store
            .replace_reminders(
                "alice",
                "e1",
                vec![
                    Reminder { id: "x1".into(), event_id: "e1".into(), owner_sub: "alice".into(), minutes_before: 30, delivered_at: 0, created_at: 0 },
                    Reminder { id: "x2".into(), event_id: "e1".into(), owner_sub: "alice".into(), minutes_before: 20, delivered_at: 0, created_at: 0 },
                ],
            )
            .await
            .unwrap();
        assert_eq!(store.due_reminders(start - 100_000, 1).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deleting_event_cascade_helpers_scope_to_owner() {
        let store = InMemoryStore::new();
        store
            .replace_attendees("alice", "e1", vec![attendee("a1", "e1", "alice", "g@x.co", "t1")])
            .await
            .unwrap();
        store
            .replace_reminders("alice", "e1", vec![Reminder { id: "r1".into(), event_id: "e1".into(), owner_sub: "alice".into(), minutes_before: 5, delivered_at: 0, created_at: 0 }])
            .await
            .unwrap();
        // A different owner's delete does not touch alice's rows.
        store.delete_event_attendees("bob", "e1").await.unwrap();
        store.delete_event_reminders("bob", "e1").await.unwrap();
        assert_eq!(store.list_attendees("alice", "e1").await.unwrap().len(), 1);
        assert_eq!(store.list_reminders("alice", "e1").await.unwrap().len(), 1);
        // The owner's cascade clears them.
        store.delete_event_attendees("alice", "e1").await.unwrap();
        store.delete_event_reminders("alice", "e1").await.unwrap();
        assert!(store.list_attendees("alice", "e1").await.unwrap().is_empty());
        assert!(store.list_reminders("alice", "e1").await.unwrap().is_empty());
    }
}
