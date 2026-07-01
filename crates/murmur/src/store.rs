//! Chat storage: rooms, memberships, messages.
//!
//! `Store` is a small async trait with an in-memory and a PostgreSQL implementation, mirroring
//! the inkwell/sanctum seam: handlers depend only on the trait, so a FusionDB-backed store can
//! drop in later. The PostgreSQL layer uses ONLY portable standard SQL (TEXT/BIGINT,
//! PK/UNIQUE/NOT NULL/DEFAULT, `INSERT … ON CONFLICT DO NOTHING`, parameterized queries, a
//! `CREATE INDEX`) and runtime queries (no compile-time macros), so the build needs NO database
//! and the same statements later run unchanged on FusionDB over pgwire.
//!
//! The methods are `async`: the axum handlers `.await` them directly on the serving runtime, and
//! `PgStore` drives sqlx natively — there is NO `block_in_place` and NO sync-over-async bridge,
//! so a DB round-trip never blocks a worker thread. The pinned schema (read read-only by Atrium)
//! is reproduced EXACTLY in [`PgStore::migrate`].

use std::sync::Mutex;

use async_trait::async_trait;
use serde::Serialize;
use thiserror::Error;

/// A chat room (maps 1:1 to a `rooms` row).
#[derive(Clone, Debug, Serialize)]
pub struct Room {
    pub id: String,
    pub name: String,
    /// `room` (default) or `dm`.
    pub kind: String,
    pub created_by: String,
    pub created_at: i64,
    /// Admin soft-archive flag. An archived room drops out of users' room lists (it is inert),
    /// but its row/history is retained. Distinct from a hard `delete_room`.
    #[serde(default)]
    pub archived: bool,
}

/// A room member as surfaced to the admin membership panel (one `memberships` row).
#[derive(Clone, Debug, Serialize)]
pub struct Member {
    pub user_sub: String,
    pub user_email: String,
    /// Admin ban flag: a banned member keeps their row but is treated as a non-member (cannot
    /// read/post), and re-joining does not clear it.
    pub banned: bool,
    pub joined_at: i64,
}

/// A room the requesting user is a member of, with that membership's read cursor.
#[derive(Clone, Debug, Serialize)]
pub struct UserRoom {
    #[serde(flatten)]
    pub room: Room,
    pub last_read_at: i64,
}

/// A chat message (maps 1:1 to a `messages` row).
#[derive(Clone, Debug, Serialize)]
pub struct Message {
    pub id: String,
    pub room_id: String,
    pub sender_sub: String,
    pub sender_email: String,
    pub body: String,
    pub created_at: i64,
    /// When the author last edited the body (epoch seconds); `0` means never edited.
    pub edited_at: i64,
    /// Soft-delete flag. A deleted message keeps its row (id/author/timestamps) but its `body`
    /// is cleared and the timeline renders `[deleted]`.
    pub deleted: bool,
}

/// Storage failure surfaced to the handler layer (always maps to a 500 — there are no
/// user-facing conflicts: room create + join are idempotent).
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("store error: {0}")]
    Backend(String),
}

/// Pluggable chat store.
#[async_trait]
pub trait Store: Send + Sync {
    /// Create a room if one with this id does not already exist (idempotent — used for the
    /// auto-provisioned lobby and for user-created rooms).
    async fn ensure_room(&self, room: &Room) -> Result<(), StoreError>;
    /// One room by id.
    async fn get_room(&self, id: &str) -> Option<Room>;
    /// Rooms the user is a member of, oldest-created first (so the lobby leads), with each
    /// membership's `last_read_at`. Capped at [`crate::config::ROOM_LIST_LIMIT`].
    async fn list_user_rooms(&self, user_sub: &str) -> Vec<UserRoom>;
    /// Add a membership if one does not already exist (idempotent join).
    async fn ensure_membership(
        &self,
        room_id: &str,
        user_sub: &str,
        user_email: &str,
        joined_at: i64,
    ) -> Result<(), StoreError>;
    /// Whether `user_sub` is a member of `room_id`.
    async fn is_member(&self, room_id: &str, user_sub: &str) -> bool;
    /// Advance a membership's read cursor (no-op if not a member).
    async fn update_last_read(
        &self,
        room_id: &str,
        user_sub: &str,
        last_read_at: i64,
    ) -> Result<(), StoreError>;
    /// Recent messages in a room, NEWEST-first. When `before` is `Some`, only messages strictly
    /// older than that `created_at` are returned (keyset pagination). Capped at `limit`.
    async fn list_messages(&self, room_id: &str, before: Option<i64>, limit: i64) -> Vec<Message>;
    /// Insert a new message.
    async fn create_message(&self, message: &Message) -> Result<(), StoreError>;
    /// One message by id (used to authorize edit/delete against `sender_sub`).
    async fn get_message(&self, id: &str) -> Option<Message>;
    /// Replace a message's body and stamp `edited_at` (author edit). No-op on a missing or
    /// already-deleted message.
    async fn edit_message(&self, id: &str, body: &str, edited_at: i64) -> Result<(), StoreError>;
    /// Soft-delete a message: mark it deleted and clear the stored body. No-op on a missing
    /// message. Idempotent.
    async fn delete_message(&self, id: &str) -> Result<(), StoreError>;

    // --- admin panel -------------------------------------------------------
    /// ALL rooms in the system, newest-created first (admin room list; includes archived).
    async fn list_all_rooms(&self) -> Vec<Room>;
    /// Set a room's archived flag (idempotent). Archived rooms drop out of users' room lists.
    async fn set_room_archived(&self, room_id: &str, archived: bool) -> Result<(), StoreError>;
    /// Hard-delete a room and ALL of its memberships + messages (admin, irreversible). Idempotent.
    async fn delete_room(&self, room_id: &str) -> Result<(), StoreError>;
    /// Every membership of a room (admin membership control), oldest-joined first.
    async fn list_room_members(&self, room_id: &str) -> Vec<Member>;
    /// Remove a membership outright (admin kick). No-op when absent. Idempotent.
    async fn remove_member(&self, room_id: &str, user_sub: &str) -> Result<(), StoreError>;
    /// Ban a member: keep the row but set `banned`, so they can no longer read/post and a
    /// re-join cannot clear it. No-op when absent. Idempotent.
    async fn ban_member(&self, room_id: &str, user_sub: &str) -> Result<(), StoreError>;
    /// Redact ANY message (admin overrides ownership): soft-delete it and replace the body with
    /// the fixed [`crate::config::REDACTED_BODY`] tombstone. No-op on a missing message. Idempotent.
    async fn redact_message(&self, id: &str) -> Result<(), StoreError>;
}

// --------------------------------------------------------------------------------------
// In-memory store (the default; keeps the whole service database-free for dev + tests).
// --------------------------------------------------------------------------------------

#[derive(Default)]
pub struct InMemoryStore {
    rooms: Mutex<Vec<Room>>,
    memberships: Mutex<Vec<MembershipRow>>,
    messages: Mutex<Vec<Message>>,
}

/// In-memory membership row (the PK is `(room_id, user_sub)`).
#[derive(Clone, Debug)]
struct MembershipRow {
    room_id: String,
    user_sub: String,
    user_email: String,
    joined_at: i64,
    last_read_at: i64,
    /// Admin ban flag (see [`Member::banned`]).
    banned: bool,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for InMemoryStore {
    // The std `Mutex` is fine throughout: each critical section is fully synchronous (no `.await`
    // inside), so a guard is never held across a yield point.
    async fn ensure_room(&self, room: &Room) -> Result<(), StoreError> {
        let mut rooms = self.rooms.lock().expect("rooms lock poisoned");
        if !rooms.iter().any(|r| r.id == room.id) {
            rooms.push(room.clone());
        }
        Ok(())
    }

    async fn get_room(&self, id: &str) -> Option<Room> {
        self.rooms
            .lock()
            .expect("rooms lock poisoned")
            .iter()
            .find(|r| r.id == id)
            .cloned()
    }

    async fn list_user_rooms(&self, user_sub: &str) -> Vec<UserRoom> {
        let memberships = self.memberships.lock().expect("memberships lock poisoned");
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let mut out: Vec<UserRoom> = memberships
            .iter()
            // A banned membership is inert — the room disappears from that user's list.
            .filter(|m| m.user_sub == user_sub && !m.banned)
            .filter_map(|m| {
                // An archived room drops out of the active room list.
                rooms
                    .iter()
                    .find(|r| r.id == m.room_id && !r.archived)
                    .map(|r| UserRoom {
                        room: r.clone(),
                        last_read_at: m.last_read_at,
                    })
            })
            .collect();
        // Oldest-created room first (the lobby is created first), ties broken by id for stability.
        out.sort_by(|a, b| {
            a.room
                .created_at
                .cmp(&b.room.created_at)
                .then_with(|| a.room.id.cmp(&b.room.id))
        });
        out.truncate(crate::config::ROOM_LIST_LIMIT as usize);
        out
    }

    async fn ensure_membership(
        &self,
        room_id: &str,
        user_sub: &str,
        user_email: &str,
        joined_at: i64,
    ) -> Result<(), StoreError> {
        let mut memberships = self.memberships.lock().expect("memberships lock poisoned");
        if !memberships
            .iter()
            .any(|m| m.room_id == room_id && m.user_sub == user_sub)
        {
            memberships.push(MembershipRow {
                room_id: room_id.to_string(),
                user_sub: user_sub.to_string(),
                user_email: user_email.to_string(),
                joined_at,
                last_read_at: 0,
                banned: false,
            });
        }
        Ok(())
    }

    async fn is_member(&self, room_id: &str, user_sub: &str) -> bool {
        // A banned membership does NOT authorize: the user is treated as a non-member.
        self.memberships
            .lock()
            .expect("memberships lock poisoned")
            .iter()
            .any(|m| m.room_id == room_id && m.user_sub == user_sub && !m.banned)
    }

    async fn update_last_read(
        &self,
        room_id: &str,
        user_sub: &str,
        last_read_at: i64,
    ) -> Result<(), StoreError> {
        let mut memberships = self.memberships.lock().expect("memberships lock poisoned");
        if let Some(m) = memberships
            .iter_mut()
            .find(|m| m.room_id == room_id && m.user_sub == user_sub)
        {
            // Read cursors only move forward.
            if last_read_at > m.last_read_at {
                m.last_read_at = last_read_at;
            }
        }
        Ok(())
    }

    async fn list_messages(&self, room_id: &str, before: Option<i64>, limit: i64) -> Vec<Message> {
        let messages = self.messages.lock().expect("messages lock poisoned");
        let mut v: Vec<Message> = messages
            .iter()
            .filter(|m| m.room_id == room_id)
            .filter(|m| before.is_none_or(|b| m.created_at < b))
            .cloned()
            .collect();
        // Newest-first; ties broken by id so output is stable + keyset-safe.
        v.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        v.truncate(limit.max(0) as usize);
        v
    }

    async fn create_message(&self, message: &Message) -> Result<(), StoreError> {
        self.messages
            .lock()
            .expect("messages lock poisoned")
            .push(message.clone());
        Ok(())
    }

    async fn get_message(&self, id: &str) -> Option<Message> {
        self.messages
            .lock()
            .expect("messages lock poisoned")
            .iter()
            .find(|m| m.id == id)
            .cloned()
    }

    async fn edit_message(&self, id: &str, body: &str, edited_at: i64) -> Result<(), StoreError> {
        let mut messages = self.messages.lock().expect("messages lock poisoned");
        if let Some(m) = messages.iter_mut().find(|m| m.id == id) {
            // A deleted message is inert — editing it would resurrect content.
            if !m.deleted {
                m.body = body.to_string();
                m.edited_at = edited_at;
            }
        }
        Ok(())
    }

    async fn delete_message(&self, id: &str) -> Result<(), StoreError> {
        let mut messages = self.messages.lock().expect("messages lock poisoned");
        if let Some(m) = messages.iter_mut().find(|m| m.id == id) {
            m.deleted = true;
            m.body = String::new();
        }
        Ok(())
    }

    async fn list_all_rooms(&self) -> Vec<Room> {
        let mut out: Vec<Room> = self
            .rooms
            .lock()
            .expect("rooms lock poisoned")
            .iter()
            .cloned()
            .collect();
        // Newest-created first, ties broken by id for a stable admin ordering.
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id)));
        out
    }

    async fn set_room_archived(&self, room_id: &str, archived: bool) -> Result<(), StoreError> {
        let mut rooms = self.rooms.lock().expect("rooms lock poisoned");
        if let Some(r) = rooms.iter_mut().find(|r| r.id == room_id) {
            r.archived = archived;
        }
        Ok(())
    }

    async fn delete_room(&self, room_id: &str) -> Result<(), StoreError> {
        self.rooms
            .lock()
            .expect("rooms lock poisoned")
            .retain(|r| r.id != room_id);
        self.memberships
            .lock()
            .expect("memberships lock poisoned")
            .retain(|m| m.room_id != room_id);
        self.messages
            .lock()
            .expect("messages lock poisoned")
            .retain(|m| m.room_id != room_id);
        Ok(())
    }

    async fn list_room_members(&self, room_id: &str) -> Vec<Member> {
        let mut out: Vec<Member> = self
            .memberships
            .lock()
            .expect("memberships lock poisoned")
            .iter()
            .filter(|m| m.room_id == room_id)
            .map(|m| Member {
                user_sub: m.user_sub.clone(),
                user_email: m.user_email.clone(),
                banned: m.banned,
                joined_at: m.joined_at,
            })
            .collect();
        out.sort_by(|a, b| {
            a.joined_at
                .cmp(&b.joined_at)
                .then_with(|| a.user_sub.cmp(&b.user_sub))
        });
        out
    }

    async fn remove_member(&self, room_id: &str, user_sub: &str) -> Result<(), StoreError> {
        self.memberships
            .lock()
            .expect("memberships lock poisoned")
            .retain(|m| !(m.room_id == room_id && m.user_sub == user_sub));
        Ok(())
    }

    async fn ban_member(&self, room_id: &str, user_sub: &str) -> Result<(), StoreError> {
        let mut memberships = self.memberships.lock().expect("memberships lock poisoned");
        if let Some(m) = memberships
            .iter_mut()
            .find(|m| m.room_id == room_id && m.user_sub == user_sub)
        {
            m.banned = true;
        }
        Ok(())
    }

    async fn redact_message(&self, id: &str) -> Result<(), StoreError> {
        let mut messages = self.messages.lock().expect("messages lock poisoned");
        if let Some(m) = messages.iter_mut().find(|m| m.id == id) {
            m.deleted = true;
            m.body = crate::config::REDACTED_BODY.to_string();
        }
        Ok(())
    }
}

// --------------------------------------------------------------------------------------
// PostgreSQL-backed store (portable: standard SQL, runtime queries, no macros).
// --------------------------------------------------------------------------------------
//
// Selected at runtime by `MURMUR_STORE=postgres`. Each method drives sqlx natively and the
// handlers `.await` it on the serving runtime — NO `block_in_place`, NO sync-over-async. Joins
// and room creates are idempotent via `ON CONFLICT DO NOTHING`, so no in-process serializer is
// needed.

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

    /// Construct from an existing pool.
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Idempotent, portable migration. Standard SQL only — safe to run on every startup. The
    /// table/column names match the PINNED schema exactly (Atrium reads them read-only).
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS rooms (\
                 id TEXT PRIMARY KEY, \
                 name TEXT NOT NULL, \
                 kind TEXT NOT NULL DEFAULT 'room', \
                 created_by TEXT NOT NULL, \
                 created_at BIGINT NOT NULL\
             )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS memberships (\
                 room_id TEXT NOT NULL, \
                 user_sub TEXT NOT NULL, \
                 user_email TEXT NOT NULL DEFAULT '', \
                 joined_at BIGINT NOT NULL, \
                 last_read_at BIGINT NOT NULL DEFAULT 0, \
                 PRIMARY KEY (room_id, user_sub)\
             )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS messages (\
                 id TEXT PRIMARY KEY, \
                 room_id TEXT NOT NULL, \
                 sender_sub TEXT NOT NULL, \
                 sender_email TEXT NOT NULL DEFAULT '', \
                 body TEXT NOT NULL, \
                 created_at BIGINT NOT NULL\
             )",
        )
        .execute(&self.pool)
        .await?;

        // Author edit/soft-delete columns. Added idempotently so an existing `messages` table
        // upgrades in place (portable ALTER — no data migration, defaults backfill old rows).
        sqlx::query(
            "ALTER TABLE messages ADD COLUMN IF NOT EXISTS edited_at BIGINT NOT NULL DEFAULT 0",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE messages ADD COLUMN IF NOT EXISTS deleted BOOLEAN NOT NULL DEFAULT FALSE",
        )
        .execute(&self.pool)
        .await?;

        // Admin panel columns. Added idempotently so an existing schema upgrades in place
        // (portable ALTER — defaults backfill old rows, no data migration).
        sqlx::query(
            "ALTER TABLE rooms ADD COLUMN IF NOT EXISTS archived BOOLEAN NOT NULL DEFAULT FALSE",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE memberships ADD COLUMN IF NOT EXISTS banned BOOLEAN NOT NULL DEFAULT FALSE",
        )
        .execute(&self.pool)
        .await?;

        // Backs the per-room newest-first timeline scan + keyset pagination.
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_messages_room_created \
             ON messages (room_id, created_at)",
        )
        .execute(&self.pool)
        .await?;
        // Backs the "rooms this user belongs to" lookup.
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memberships_user ON memberships (user_sub)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    fn room_from_row(row: &sqlx::postgres::PgRow) -> Result<Room, sqlx::Error> {
        Ok(Room {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            kind: row.try_get("kind")?,
            created_by: row.try_get("created_by")?,
            created_at: row.try_get("created_at")?,
            archived: row.try_get("archived")?,
        })
    }

    fn member_from_row(row: &sqlx::postgres::PgRow) -> Result<Member, sqlx::Error> {
        Ok(Member {
            user_sub: row.try_get("user_sub")?,
            user_email: row.try_get("user_email")?,
            banned: row.try_get("banned")?,
            joined_at: row.try_get("joined_at")?,
        })
    }

    fn message_from_row(row: &sqlx::postgres::PgRow) -> Result<Message, sqlx::Error> {
        Ok(Message {
            id: row.try_get("id")?,
            room_id: row.try_get("room_id")?,
            sender_sub: row.try_get("sender_sub")?,
            sender_email: row.try_get("sender_email")?,
            body: row.try_get("body")?,
            created_at: row.try_get("created_at")?,
            edited_at: row.try_get("edited_at")?,
            deleted: row.try_get("deleted")?,
        })
    }

    async fn ensure_room_async(&self, room: &Room) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO rooms (id, name, kind, created_by, created_at, archived) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO NOTHING",
        )
        .bind(&room.id)
        .bind(&room.name)
        .bind(&room.kind)
        .bind(&room.created_by)
        .bind(room.created_at)
        .bind(room.archived)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_room_async(&self, id: &str) -> Result<Option<Room>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, name, kind, created_by, created_at, archived FROM rooms WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => Ok(Some(Self::room_from_row(&r)?)),
            None => Ok(None),
        }
    }

    async fn list_user_rooms_async(&self, user_sub: &str) -> Result<Vec<UserRoom>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT r.id, r.name, r.kind, r.created_by, r.created_at, r.archived, m.last_read_at \
             FROM rooms r JOIN memberships m ON m.room_id = r.id \
             WHERE m.user_sub = $1 AND m.banned = FALSE AND r.archived = FALSE \
             ORDER BY r.created_at ASC, r.id ASC LIMIT $2",
        )
        .bind(user_sub)
        .bind(crate::config::ROOM_LIST_LIMIT)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(UserRoom {
                    room: Self::room_from_row(r)?,
                    last_read_at: r.try_get("last_read_at")?,
                })
            })
            .collect()
    }

    async fn ensure_membership_async(
        &self,
        room_id: &str,
        user_sub: &str,
        user_email: &str,
        joined_at: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO memberships (room_id, user_sub, user_email, joined_at, last_read_at) \
             VALUES ($1, $2, $3, $4, 0) ON CONFLICT (room_id, user_sub) DO NOTHING",
        )
        .bind(room_id)
        .bind(user_sub)
        .bind(user_email)
        .bind(joined_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn is_member_async(&self, room_id: &str, user_sub: &str) -> Result<bool, sqlx::Error> {
        let row = sqlx::query(
            "SELECT 1 AS one FROM memberships \
             WHERE room_id = $1 AND user_sub = $2 AND banned = FALSE",
        )
        .bind(room_id)
        .bind(user_sub)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    async fn update_last_read_async(
        &self,
        room_id: &str,
        user_sub: &str,
        last_read_at: i64,
    ) -> Result<(), sqlx::Error> {
        // Read cursors only move forward (GREATEST keeps an out-of-order update harmless).
        sqlx::query(
            "UPDATE memberships SET last_read_at = $1 \
             WHERE room_id = $2 AND user_sub = $3 AND $1 > last_read_at",
        )
        .bind(last_read_at)
        .bind(room_id)
        .bind(user_sub)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_messages_async(
        &self,
        room_id: &str,
        before: Option<i64>,
        limit: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        // `before` is folded into one statement: a sentinel of i64::MAX means "no cursor", so the
        // predicate `created_at < $2` always passes for the first page. Keeps the SQL portable
        // (no dynamic string building).
        let cursor = before.unwrap_or(i64::MAX);
        let rows = sqlx::query(
            "SELECT id, room_id, sender_sub, sender_email, body, created_at, edited_at, deleted \
             FROM messages WHERE room_id = $1 AND created_at < $2 \
             ORDER BY created_at DESC, id DESC LIMIT $3",
        )
        .bind(room_id)
        .bind(cursor)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::message_from_row).collect()
    }

    async fn create_message_async(&self, m: &Message) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO messages \
             (id, room_id, sender_sub, sender_email, body, created_at, edited_at, deleted) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&m.id)
        .bind(&m.room_id)
        .bind(&m.sender_sub)
        .bind(&m.sender_email)
        .bind(&m.body)
        .bind(m.created_at)
        .bind(m.edited_at)
        .bind(m.deleted)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_message_async(&self, id: &str) -> Result<Option<Message>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, room_id, sender_sub, sender_email, body, created_at, edited_at, deleted \
             FROM messages WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => Ok(Some(Self::message_from_row(&r)?)),
            None => Ok(None),
        }
    }

    async fn edit_message_async(
        &self,
        id: &str,
        body: &str,
        edited_at: i64,
    ) -> Result<(), sqlx::Error> {
        // A deleted message stays inert (`AND deleted = FALSE`): an edit must never resurrect it.
        sqlx::query(
            "UPDATE messages SET body = $1, edited_at = $2 WHERE id = $3 AND deleted = FALSE",
        )
        .bind(body)
        .bind(edited_at)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_message_async(&self, id: &str) -> Result<(), sqlx::Error> {
        // Soft delete: keep the row (id/author/timestamps) but clear the body. Idempotent.
        sqlx::query("UPDATE messages SET deleted = TRUE, body = '' WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_all_rooms_async(&self) -> Result<Vec<Room>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, name, kind, created_by, created_at, archived FROM rooms \
             ORDER BY created_at DESC, id DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::room_from_row).collect()
    }

    async fn set_room_archived_async(
        &self,
        room_id: &str,
        archived: bool,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE rooms SET archived = $1 WHERE id = $2")
            .bind(archived)
            .bind(room_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_room_async(&self, room_id: &str) -> Result<(), sqlx::Error> {
        // Hard delete: messages + memberships first, then the room row. Portable, no FK cascade.
        sqlx::query("DELETE FROM messages WHERE room_id = $1")
            .bind(room_id)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM memberships WHERE room_id = $1")
            .bind(room_id)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_room_members_async(&self, room_id: &str) -> Result<Vec<Member>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT user_sub, user_email, banned, joined_at FROM memberships \
             WHERE room_id = $1 ORDER BY joined_at ASC, user_sub ASC",
        )
        .bind(room_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::member_from_row).collect()
    }

    async fn remove_member_async(
        &self,
        room_id: &str,
        user_sub: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM memberships WHERE room_id = $1 AND user_sub = $2")
            .bind(room_id)
            .bind(user_sub)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn ban_member_async(&self, room_id: &str, user_sub: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE memberships SET banned = TRUE WHERE room_id = $1 AND user_sub = $2")
            .bind(room_id)
            .bind(user_sub)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn redact_message_async(&self, id: &str) -> Result<(), sqlx::Error> {
        // Redact ANY message: soft-delete + replace the body with the moderator tombstone.
        sqlx::query("UPDATE messages SET deleted = TRUE, body = $1 WHERE id = $2")
            .bind(crate::config::REDACTED_BODY)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl Store for PgStore {
    async fn ensure_room(&self, room: &Room) -> Result<(), StoreError> {
        self.ensure_room_async(room)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn get_room(&self, id: &str) -> Option<Room> {
        self.get_room_async(id).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg get_room failed");
            None
        })
    }

    async fn list_user_rooms(&self, user_sub: &str) -> Vec<UserRoom> {
        self.list_user_rooms_async(user_sub)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "pg list_user_rooms failed");
                Vec::new()
            })
    }

    async fn ensure_membership(
        &self,
        room_id: &str,
        user_sub: &str,
        user_email: &str,
        joined_at: i64,
    ) -> Result<(), StoreError> {
        self.ensure_membership_async(room_id, user_sub, user_email, joined_at)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn is_member(&self, room_id: &str, user_sub: &str) -> bool {
        self.is_member_async(room_id, user_sub)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "pg is_member failed");
                false
            })
    }

    async fn update_last_read(
        &self,
        room_id: &str,
        user_sub: &str,
        last_read_at: i64,
    ) -> Result<(), StoreError> {
        self.update_last_read_async(room_id, user_sub, last_read_at)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_messages(&self, room_id: &str, before: Option<i64>, limit: i64) -> Vec<Message> {
        self.list_messages_async(room_id, before, limit)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "pg list_messages failed");
                Vec::new()
            })
    }

    async fn create_message(&self, message: &Message) -> Result<(), StoreError> {
        self.create_message_async(message)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn get_message(&self, id: &str) -> Option<Message> {
        self.get_message_async(id).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg get_message failed");
            None
        })
    }

    async fn edit_message(&self, id: &str, body: &str, edited_at: i64) -> Result<(), StoreError> {
        self.edit_message_async(id, body, edited_at)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn delete_message(&self, id: &str) -> Result<(), StoreError> {
        self.delete_message_async(id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_all_rooms(&self) -> Vec<Room> {
        self.list_all_rooms_async().await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg list_all_rooms failed");
            Vec::new()
        })
    }

    async fn set_room_archived(&self, room_id: &str, archived: bool) -> Result<(), StoreError> {
        self.set_room_archived_async(room_id, archived)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn delete_room(&self, room_id: &str) -> Result<(), StoreError> {
        self.delete_room_async(room_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_room_members(&self, room_id: &str) -> Vec<Member> {
        self.list_room_members_async(room_id)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "pg list_room_members failed");
                Vec::new()
            })
    }

    async fn remove_member(&self, room_id: &str, user_sub: &str) -> Result<(), StoreError> {
        self.remove_member_async(room_id, user_sub)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn ban_member(&self, room_id: &str, user_sub: &str) -> Result<(), StoreError> {
        self.ban_member_async(room_id, user_sub)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn redact_message(&self, id: &str) -> Result<(), StoreError> {
        self.redact_message_async(id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }
}
