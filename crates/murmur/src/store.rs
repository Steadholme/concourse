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

/// Stable tuple key used by every newest-first message projection.
///
/// The wire representation is an opaque, versioned, URL-safe string. Keeping the timestamp and
/// id together prevents same-second messages from being skipped by keyset pagination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageCursor {
    pub created_at: i64,
    pub message_id: String,
}

impl MessageCursor {
    const PREFIX: &'static str = "v1.";
    const MAX_WIRE_LEN: usize = 512;
    const MAX_ID_LEN: usize = 192;

    pub fn from_message(message: &Message) -> Self {
        Self {
            created_at: message.created_at,
            message_id: message.id.clone(),
        }
    }

    pub fn encode(&self) -> String {
        format!(
            "{}{}.{}",
            Self::PREFIX,
            self.created_at,
            hex::encode(self.message_id.as_bytes())
        )
    }

    pub fn decode(value: &str) -> Option<Self> {
        if value.is_empty() || value.len() > Self::MAX_WIRE_LEN {
            return None;
        }
        let body = value.strip_prefix(Self::PREFIX)?;
        let (created_at, encoded_id) = body.split_once('.')?;
        if created_at.is_empty()
            || encoded_id.is_empty()
            || encoded_id.len() % 2 != 0
            || encoded_id
                .bytes()
                .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte))
        {
            return None;
        }
        let created_at = created_at.parse::<i64>().ok()?;
        if created_at < 0 {
            return None;
        }
        let decoded = hex::decode(encoded_id).ok()?;
        if decoded.is_empty() || decoded.len() > Self::MAX_ID_LEN {
            return None;
        }
        let message_id = String::from_utf8(decoded).ok()?;
        let cursor = Self {
            created_at,
            message_id,
        };
        (cursor.encode() == value).then_some(cursor)
    }
}

/// A chat room (maps 1:1 to a `rooms` row).
#[derive(Clone, Debug, Serialize)]
pub struct Room {
    pub id: String,
    pub name: String,
    /// `room` (default) or `dm`.
    pub kind: String,
    pub created_by: String,
    pub created_at: i64,
    /// Admin soft-archive flag. Archived rooms remain visible to authorized readers but are
    /// read-only and excluded from live subscriptions. Distinct from a hard `delete_room`.
    #[serde(default)]
    pub archived: bool,
    /// Free-text room topic / description / purpose shown in the room header. Empty by default;
    /// editable by room admins/mods. Stored as PLAIN TEXT and escaped by every render layer.
    #[serde(default)]
    pub topic: String,
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

/// A person surfaced in the "New DM" people directory (a distinct known subject/email the
/// requesting user can direct-message). Derived from the union of all memberships.
#[derive(Clone, Debug, Serialize)]
pub struct Person {
    pub user_sub: String,
    pub user_email: String,
}

/// A room the requesting user is a member of, with that membership's read cursor plus the derived
/// unread state that drives the room-list badge.
#[derive(Clone, Debug, Serialize)]
pub struct UserRoom {
    #[serde(flatten)]
    pub room: Room,
    pub last_read_at: i64,
    /// Tuple tie-breaker paired with `last_read_at`. Old rows use the empty-string sentinel.
    pub last_read_message_id: String,
    /// Count of messages in the room newer than `last_read_at` and NOT authored by the requesting
    /// user. The current Patchbay DOM consumes only whether this is positive, never the exact value.
    #[serde(default)]
    pub unread: i64,
    /// Whether the requesting user has an unread @mention in this room (a mention on a message
    /// newer than `last_read_at`). Drives the mention dot on the room-list badge.
    #[serde(default)]
    pub mentioned: bool,
}

/// A message matched by search / carrying an @mention, tagged with its room's display name so the
/// result can link back to the room. Flattens the full [`Message`] (which already carries `room_id`).
#[derive(Clone, Debug, Serialize)]
pub struct MessageHit {
    /// The display name of the room the matched message lives in (escaped by the render layer).
    pub room_name: String,
    #[serde(flatten)]
    pub message: Message,
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
    /// Optional parent message this is a threaded reply to (`NULL`/`None` for a top-level post).
    /// The timeline renders a quoted parent above a reply and a reply count on the parent.
    #[serde(default)]
    pub reply_to_id: Option<String>,
}

/// A reaction tally on one message: an emoji with how many DISTINCT users applied it. Derived by
/// grouping `message_reactions` (never stored directly).
#[derive(Clone, Debug, Serialize)]
pub struct ReactionCount {
    pub emoji: String,
    pub count: i64,
}

/// The complete result of one authorized reaction toggle.
///
/// The toggle and both projections are produced under one Store atomicity boundary so a
/// concurrent membership revocation, archive, or second toggle cannot split the response.
#[derive(Clone, Debug)]
pub struct ReactionMutation {
    pub added: bool,
    pub reactions: Vec<ReactionCount>,
    pub mine: Vec<String>,
}

/// Authorized read projection for one message's reactions.
#[derive(Clone, Debug)]
pub struct ReactionProjection {
    pub reactions: Vec<ReactionCount>,
    pub mine: Vec<String>,
}

/// One durable hard-delete audit consequence.
///
/// PostgreSQL persists this row in the same transaction as token consumption and the room purge;
/// the in-memory Store appends it while holding the complete delete lock set. `id` is an opaque
/// CSPRNG identifier used to make downstream retry delivery idempotency-visible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomDeleteAuditConsequence {
    pub id: String,
    pub room_id: String,
    pub actor_sub: String,
    pub occurred_at: i64,
}

/// Storage failure surfaced to the handler layer (always maps to a safe `503`; raw backend detail
/// is logged server-side and never serialized to the client).
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("store error: {0}")]
    Backend(String),
}

/// Stable outcomes for Store-authorized mutations.
///
/// Authorization and the write happen in the same Memory lock set or PostgreSQL transaction.
/// Handlers map these variants to the frozen public error envelope; backend detail remains
/// server-side only.
#[derive(Debug, Error)]
pub enum StoreMutationError {
    #[error("store error: {0}")]
    Backend(String),
    #[error("resource unavailable")]
    ResourceUnavailable,
    #[error("room not found")]
    RoomNotFound,
    #[error("membership missing")]
    NotMember,
    #[error("membership banned")]
    MemberBanned,
    #[error("room archived")]
    RoomArchived,
    #[error("action forbidden")]
    Forbidden,
    #[error("message deleted")]
    MessageDeleted,
    #[error("reply target unavailable")]
    ReplyUnavailable,
    #[error("direct messages cannot be joined")]
    DirectMessageJoin,
    #[error("consequence token invalid")]
    ConsequenceTokenInvalid,
}

impl From<sqlx::Error> for StoreMutationError {
    fn from(error: sqlx::Error) -> Self {
        Self::Backend(error.to_string())
    }
}

/// Pluggable chat store.
#[async_trait]
pub trait Store: Send + Sync {
    /// Create a room if one with this id does not already exist (idempotent — used for the
    /// auto-provisioned lobby and for user-created rooms).
    async fn ensure_room(&self, room: &Room) -> Result<(), StoreError>;
    /// One room by id.
    async fn get_room(&self, id: &str) -> Result<Option<Room>, StoreError>;
    /// Active and archived-readable rooms the user is a non-banned member of, oldest-created first
    /// (so the lobby leads), with each membership's `last_read_at`.
    async fn list_user_rooms(&self, user_sub: &str) -> Result<Vec<UserRoom>, StoreError>;
    /// The people directory backing "New DM": every DISTINCT known person (one row per
    /// `user_sub`, with an email) drawn from the union of all memberships, EXCLUDING the
    /// requesting user. Ordered by email then subject. Capped at [`crate::config::DIRECTORY_LIMIT`].
    async fn list_directory(&self, exclude_sub: &str) -> Result<Vec<Person>, StoreError>;
    /// Add a membership if one does not already exist (idempotent join).
    async fn ensure_membership(
        &self,
        room_id: &str,
        user_sub: &str,
        user_email: &str,
        joined_at: i64,
    ) -> Result<(), StoreError>;
    /// Whether `user_sub` is a member of `room_id`.
    async fn is_member(&self, room_id: &str, user_sub: &str) -> Result<bool, StoreError>;
    /// Recent messages in a room, NEWEST-first. When `before` is `Some`, only messages strictly
    /// older than that `(created_at, id)` tuple are returned (keyset pagination). Capped at
    /// `limit`.
    async fn list_messages(
        &self,
        room_id: &str,
        before: Option<MessageCursor>,
        limit: i64,
    ) -> Result<Vec<Message>, StoreError>;
    /// Insert a new message.
    async fn create_message(&self, message: &Message) -> Result<(), StoreError>;
    /// One message by id (used to authorize edit/delete against `sender_sub`).
    async fn get_message(&self, id: &str) -> Result<Option<Message>, StoreError>;
    /// Replace a message's body and stamp `edited_at` (author edit). No-op on a missing or
    /// already-deleted message.
    async fn edit_message(&self, id: &str, body: &str, edited_at: i64) -> Result<(), StoreError>;
    /// Soft-delete a message: mark it deleted and clear the stored body. No-op on a missing
    /// message. Idempotent.
    async fn delete_message(&self, id: &str) -> Result<(), StoreError>;

    // --- reactions + threaded replies --------------------------------------
    /// Toggle a `(message_id, user_sub, emoji)` reaction: remove it when it already exists, else
    /// add it. Idempotent per unique triple. Returns `true` when the reaction is now PRESENT
    /// (added), `false` when it was removed.
    async fn toggle_reaction(
        &self,
        message_id: &str,
        user_sub: &str,
        emoji: &str,
    ) -> Result<bool, StoreError>;
    /// Per-emoji reaction tallies for a message (distinct users per emoji), most-used first then
    /// emoji for a stable order. Empty when the message has no reactions.
    async fn list_reactions(&self, message_id: &str) -> Result<Vec<ReactionCount>, StoreError>;
    /// The emojis `user_sub` has personally applied to `message_id` (used to highlight the
    /// caller's own reactions), sorted for stability.
    async fn list_user_reactions(
        &self,
        message_id: &str,
        user_sub: &str,
    ) -> Result<Vec<String>, StoreError>;
    /// How many messages are threaded replies to `message_id` (their `reply_to_id` equals it).
    async fn count_replies(&self, room_id: &str, message_id: &str) -> Result<i64, StoreError>;
    /// Current non-banned room members who have participated in a thread (the parent message or one
    /// of its direct replies), oldest participation first. Read-only helper for best-effort notify.
    async fn list_thread_participants(
        &self,
        room_id: &str,
        parent_message_id: &str,
    ) -> Result<Vec<Person>, StoreError>;

    // --- admin panel -------------------------------------------------------
    /// ALL rooms in the system, newest-created first (admin room list; includes archived).
    async fn list_all_rooms(&self) -> Result<Vec<Room>, StoreError>;
    /// Set a room's archived flag (idempotent). Archived rooms become read-only.
    async fn set_room_archived(&self, room_id: &str, archived: bool) -> Result<(), StoreError>;
    /// Every membership of a room (admin membership control), oldest-joined first.
    async fn list_room_members(&self, room_id: &str) -> Result<Vec<Member>, StoreError>;
    /// Remove a membership outright (admin kick). No-op when absent. Idempotent.
    async fn remove_member(&self, room_id: &str, user_sub: &str) -> Result<(), StoreError>;
    /// Ban a member: keep the row but set `banned`, so they can no longer read/post and a
    /// re-join cannot clear it. No-op when absent. Idempotent.
    async fn ban_member(&self, room_id: &str, user_sub: &str) -> Result<(), StoreError>;
    /// Redact ANY message (admin overrides ownership): soft-delete it and replace the body with
    /// the fixed [`crate::config::REDACTED_BODY`] tombstone. No-op on a missing message. Idempotent.
    async fn redact_message(&self, id: &str) -> Result<(), StoreError>;

    // --- room topic --------------------------------------------------------
    /// Set a room's free-text topic (idempotent overwrite). No-op on a missing room.
    async fn set_room_topic(&self, room_id: &str, topic: &str) -> Result<(), StoreError>;

    // --- pinned messages ---------------------------------------------------
    /// Pin `message_id` in `room_id` (idempotent per `(room, message)`). Records who pinned it and
    /// when.
    async fn pin_message(
        &self,
        room_id: &str,
        message_id: &str,
        pinned_by: &str,
        pinned_at: i64,
    ) -> Result<(), StoreError>;
    /// Unpin `message_id` from `room_id`. No-op when it was not pinned. Idempotent.
    async fn unpin_message(&self, room_id: &str, message_id: &str) -> Result<(), StoreError>;
    /// The pinned messages of a room, most-recently-pinned first. Capped at
    /// [`crate::config::PINNED_LIMIT`].
    async fn list_pinned(&self, room_id: &str) -> Result<Vec<Message>, StoreError>;
    /// Whether `message_id` is currently pinned in `room_id`.
    async fn is_pinned(&self, room_id: &str, message_id: &str) -> Result<bool, StoreError>;

    // --- search ------------------------------------------------------------
    /// Full-body substring search (case-insensitive) across ONLY the rooms `user_sub` is a
    /// non-banned member of — never leaks a non-member room's content. `query_lower` is the
    /// already-lowercased needle; deleted messages are excluded. NEWEST-first, keyset-paginated by
    /// `before` (messages strictly older than that `(created_at,id)` tuple). Capped at `limit`.
    async fn search_user_messages(
        &self,
        user_sub: &str,
        query_lower: &str,
        before: Option<MessageCursor>,
        limit: i64,
    ) -> Result<Vec<MessageHit>, StoreError>;

    // --- @mentions ---------------------------------------------------------
    /// Record that `mentioned_sub` was @mentioned by `message_id` in `room_id`. Idempotent per
    /// `(message, mentioned_sub)`.
    async fn add_mention(
        &self,
        message_id: &str,
        room_id: &str,
        mentioned_sub: &str,
        created_at: i64,
    ) -> Result<(), StoreError>;
    /// The messages that @mention `user_sub`, across every room (the global "mentions" view).
    /// Deleted messages are excluded. NEWEST-first, keyset-paginated by `before`. Capped at `limit`.
    async fn list_user_mentions(
        &self,
        user_sub: &str,
        before: Option<MessageCursor>,
        limit: i64,
    ) -> Result<Vec<MessageHit>, StoreError>;

    // --- atomic authorization + mutation ---------------------------------
    /// Join a current non-DM, active room. The room-state check and membership insertion are one
    /// atomic operation.
    async fn join_active_room(
        &self,
        room_id: &str,
        user_sub: &str,
        user_email: &str,
        joined_at: i64,
    ) -> Result<Room, StoreMutationError>;
    /// Create or reopen one deterministic DM and ensure both participants under the same
    /// active-room authority boundary. A concurrent archive either happens entirely before this
    /// operation (which then returns `RoomArchived`) or entirely after both memberships commit.
    async fn open_dm_authorized(
        &self,
        room: &Room,
        user_sub: &str,
        user_email: &str,
        peer_sub: &str,
        peer_email: &str,
        joined_at: i64,
    ) -> Result<Room, StoreMutationError>;
    /// Authorize the sender against current non-banned membership and an active room, validate an
    /// optional same-room reply parent, then persist the message and every resolved mention
    /// atomically. Returns the current mentioned members for best-effort notifications.
    async fn create_message_authorized(
        &self,
        message: &Message,
        mention_tokens: &[String],
    ) -> Result<Vec<Member>, StoreMutationError>;
    /// Edit an own, live message under current membership and active-room authority.
    async fn edit_message_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
        body: &str,
        edited_at: i64,
    ) -> Result<Message, StoreMutationError>;
    /// Soft-delete an own message under current membership and active-room authority.
    async fn delete_message_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
    ) -> Result<Message, StoreMutationError>;
    /// Toggle and project a reaction under current membership and active-room authority.
    async fn toggle_reaction_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
        emoji: &str,
    ) -> Result<ReactionMutation, StoreMutationError>;
    /// Advance the read cursor from a server-owned message timestamp under current membership.
    /// Archived rooms remain readable, matching the frozen archive matrix.
    async fn update_last_read_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
    ) -> Result<Message, StoreMutationError>;
    /// Admin-authorized topic mutation with an atomic active-room fence.
    async fn set_room_topic_authorized(
        &self,
        room_id: &str,
        topic: &str,
    ) -> Result<Room, StoreMutationError>;
    /// Admin-authorized pin mutation with an atomic active-room and same-room-message fence.
    async fn pin_message_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        pinned_by: &str,
        pinned_at: i64,
    ) -> Result<Vec<Message>, StoreMutationError>;
    /// Admin-authorized unpin mutation with an atomic active-room fence.
    async fn unpin_message_authorized(
        &self,
        room_id: &str,
        message_id: &str,
    ) -> Result<Vec<Message>, StoreMutationError>;

    // --- atomic authorization + protected read projections ----------------
    /// Revalidate one room subscription against canonical Store state. `require_active` is true
    /// for live WS subscriptions and false for archived-readable HTTP projections.
    async fn authorize_room_read(
        &self,
        room_id: &str,
        user_sub: &str,
        require_active: bool,
    ) -> Result<Room, StoreMutationError>;
    /// Membership-authorized room history under one Store authority boundary.
    async fn list_messages_authorized(
        &self,
        room_id: &str,
        user_sub: &str,
        before: Option<MessageCursor>,
        limit: i64,
    ) -> Result<Vec<Message>, StoreMutationError>;
    /// Membership-authorized optional message lookup. `None` means the message is absent after
    /// room authorization; it never represents a denied or failed Store read.
    async fn get_message_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
    ) -> Result<Option<Message>, StoreMutationError>;
    /// Membership-authorized reaction projection for one same-room message.
    async fn reaction_projection_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
    ) -> Result<ReactionProjection, StoreMutationError>;
    /// Membership-authorized pinned projection.
    async fn list_pinned_authorized(
        &self,
        room_id: &str,
        user_sub: &str,
    ) -> Result<Vec<Message>, StoreMutationError>;
    /// Membership-authorized reply count.
    async fn count_replies_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
    ) -> Result<i64, StoreMutationError>;

    // --- one-time hard-delete consequence authority -----------------------
    /// Persist one opaque, digest-only delete grant issued by the SSR admin detail page.
    async fn issue_room_delete_token(
        &self,
        token_digest: &str,
        room_id: &str,
        actor_sub: &str,
        csrf_digest: &str,
        issued_at: i64,
        expires_at: i64,
    ) -> Result<Room, StoreMutationError>;
    /// Validate and consume the one-time grant in the same atomic boundary as the room purge and
    /// one durable audit-outbox append.
    async fn delete_room_with_token(
        &self,
        room_id: &str,
        token_digest: &str,
        actor_sub: &str,
        csrf_digest: &str,
        now: i64,
    ) -> Result<RoomDeleteAuditConsequence, StoreMutationError>;
    /// Oldest undelivered hard-delete consequences for at-least-once Watchtower delivery.
    async fn pending_room_delete_audits(
        &self,
        limit: i64,
    ) -> Result<Vec<RoomDeleteAuditConsequence>, StoreError>;
    /// Idempotently mark one durable consequence delivered.
    async fn mark_room_delete_audit_delivered(
        &self,
        consequence_id: &str,
        delivered_at: i64,
    ) -> Result<(), StoreError>;
}

// --------------------------------------------------------------------------------------
// In-memory store (the default; keeps the whole service database-free for dev + tests).
// --------------------------------------------------------------------------------------

#[derive(Default)]
pub struct InMemoryStore {
    rooms: Mutex<Vec<Room>>,
    room_incarnations: Mutex<Vec<RoomIncarnationRow>>,
    memberships: Mutex<Vec<MembershipRow>>,
    messages: Mutex<Vec<Message>>,
    reactions: Mutex<Vec<ReactionRow>>,
    pinned: Mutex<Vec<PinnedRow>>,
    mentions: Mutex<Vec<MentionRow>>,
    room_delete_tokens: Mutex<Vec<RoomDeleteTokenRow>>,
    room_delete_audits: Mutex<Vec<RoomDeleteAuditRow>>,
}

/// In-memory reaction row (the PK is the whole `(message_id, user_sub, emoji)` triple).
#[derive(Clone, Debug)]
struct ReactionRow {
    message_id: String,
    user_sub: String,
    emoji: String,
}

/// In-memory pinned-message row (the PK is `(room_id, message_id)`).
#[derive(Clone, Debug)]
struct PinnedRow {
    room_id: String,
    message_id: String,
    #[allow(dead_code)]
    pinned_by: String,
    pinned_at: i64,
}

/// In-memory @mention row (the PK is `(message_id, mentioned_sub)`).
#[derive(Clone, Debug)]
struct MentionRow {
    message_id: String,
    room_id: String,
    mentioned_sub: String,
    created_at: i64,
}

/// In-memory membership row (the PK is `(room_id, user_sub)`).
#[derive(Clone, Debug)]
struct MembershipRow {
    room_id: String,
    user_sub: String,
    user_email: String,
    joined_at: i64,
    last_read_at: i64,
    last_read_message_id: String,
    /// Admin ban flag (see [`Member::banned`]).
    banned: bool,
}

#[derive(Clone, Debug)]
struct RoomDeleteTokenRow {
    token_digest: String,
    room_id: String,
    room_incarnation: String,
    actor_sub: String,
    csrf_digest: String,
    purpose: String,
    issued_at: i64,
    expires_at: i64,
}

#[derive(Clone, Debug)]
struct RoomIncarnationRow {
    room_id: String,
    incarnation: String,
}

#[derive(Clone, Debug)]
struct RoomDeleteAuditRow {
    consequence: RoomDeleteAuditConsequence,
    delivered_at: i64,
}

const ROOM_DELETE_PURPOSE: &str = "room-hard-delete";

fn new_opaque_id() -> String {
    let mut bytes = [0_u8; 32];
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG unavailable");
    hex::encode(bytes)
}

fn membership_matches_mention(member: &MembershipRow, sender_sub: &str, tokens: &[String]) -> bool {
    mention_identity_matches(
        &member.user_sub,
        &member.user_email,
        member.banned,
        sender_sub,
        tokens,
    )
}

fn mention_identity_matches(
    user_sub: &str,
    user_email: &str,
    banned: bool,
    sender_sub: &str,
    tokens: &[String],
) -> bool {
    if banned || user_sub == sender_sub {
        return false;
    }
    let email = user_email.to_lowercase();
    let local = email.split('@').next().unwrap_or("");
    tokens
        .iter()
        .any(|token| token == &email || (!local.is_empty() && token == local))
}

fn reaction_projection(
    rows: &[ReactionRow],
    message_id: &str,
    user_sub: &str,
    added: bool,
) -> ReactionMutation {
    let mut reactions: Vec<ReactionCount> = Vec::new();
    let mut mine = Vec::new();
    for row in rows.iter().filter(|row| row.message_id == message_id) {
        if let Some(count) = reactions.iter_mut().find(|count| count.emoji == row.emoji) {
            count.count += 1;
        } else {
            reactions.push(ReactionCount {
                emoji: row.emoji.clone(),
                count: 1,
            });
        }
        if row.user_sub == user_sub {
            mine.push(row.emoji.clone());
        }
    }
    reactions.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.emoji.cmp(&right.emoji))
    });
    mine.sort();
    ReactionMutation {
        added,
        reactions,
        mine,
    }
}

fn pinned_projection(pinned: &[PinnedRow], messages: &[Message], room_id: &str) -> Vec<Message> {
    let mut rows: Vec<&PinnedRow> = pinned.iter().filter(|row| row.room_id == room_id).collect();
    rows.sort_by(|left, right| {
        right
            .pinned_at
            .cmp(&left.pinned_at)
            .then_with(|| right.message_id.cmp(&left.message_id))
    });
    rows.into_iter()
        .filter_map(|row| {
            messages
                .iter()
                .find(|message| message.id == row.message_id)
                .cloned()
        })
        .take(crate::config::PINNED_LIMIT as usize)
        .collect()
}

fn tuple_is_after(created_at: i64, message_id: &str, marker_at: i64, marker_id: &str) -> bool {
    created_at > marker_at || (created_at == marker_at && message_id > marker_id)
}

fn tuple_is_before(message: &Message, cursor: &MessageCursor) -> bool {
    message.created_at < cursor.created_at
        || (message.created_at == cursor.created_at && message.id < cursor.message_id)
}

fn authorize_memory_read(
    memberships: &[MembershipRow],
    rooms: &[Room],
    room_id: &str,
    user_sub: &str,
    require_active: bool,
) -> Result<Room, StoreMutationError> {
    let room = rooms
        .iter()
        .find(|room| room.id == room_id)
        .cloned()
        .ok_or(StoreMutationError::RoomNotFound)?;
    let membership = memberships
        .iter()
        .find(|membership| membership.room_id == room_id && membership.user_sub == user_sub)
        .ok_or(StoreMutationError::NotMember)?;
    if membership.banned {
        return Err(StoreMutationError::MemberBanned);
    }
    if require_active && room.archived {
        return Err(StoreMutationError::RoomArchived);
    }
    Ok(room)
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
        let mut incarnations = self
            .room_incarnations
            .lock()
            .expect("room incarnations lock poisoned");
        if !rooms.iter().any(|r| r.id == room.id) {
            rooms.push(room.clone());
            incarnations.push(RoomIncarnationRow {
                room_id: room.id.clone(),
                incarnation: new_opaque_id(),
            });
        }
        Ok(())
    }

    async fn get_room(&self, id: &str) -> Result<Option<Room>, StoreError> {
        Ok(self
            .rooms
            .lock()
            .expect("rooms lock poisoned")
            .iter()
            .find(|r| r.id == id)
            .cloned())
    }

    async fn list_user_rooms(&self, user_sub: &str) -> Result<Vec<UserRoom>, StoreError> {
        let memberships = self.memberships.lock().expect("memberships lock poisoned");
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let messages = self.messages.lock().expect("messages lock poisoned");
        let mentions = self.mentions.lock().expect("mentions lock poisoned");
        let mut out: Vec<UserRoom> = memberships
            .iter()
            // A banned membership is inert — the room disappears from that user's list.
            .filter(|m| m.user_sub == user_sub && !m.banned)
            .filter_map(|m| {
                rooms.iter().find(|r| r.id == m.room_id).map(|r| {
                    // Unread = messages newer than the read cursor NOT authored by this user.
                    let unread = messages
                        .iter()
                        .filter(|msg| {
                            msg.room_id == r.id
                                && tuple_is_after(
                                    msg.created_at,
                                    &msg.id,
                                    m.last_read_at,
                                    &m.last_read_message_id,
                                )
                                && msg.sender_sub != user_sub
                                && !msg.deleted
                        })
                        .count() as i64;
                    // Mentioned = an @mention of this user on a message newer than the cursor.
                    let mentioned = mentions.iter().any(|mn| {
                        mn.room_id == r.id
                            && mn.mentioned_sub == user_sub
                            && tuple_is_after(
                                mn.created_at,
                                &mn.message_id,
                                m.last_read_at,
                                &m.last_read_message_id,
                            )
                    });
                    UserRoom {
                        room: r.clone(),
                        last_read_at: m.last_read_at,
                        last_read_message_id: m.last_read_message_id.clone(),
                        unread,
                        mentioned,
                    }
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
        Ok(out)
    }

    async fn list_directory(&self, exclude_sub: &str) -> Result<Vec<Person>, StoreError> {
        let memberships = self.memberships.lock().expect("memberships lock poisoned");
        // One entry per distinct subject (first email seen wins), excluding the caller.
        let mut out: Vec<Person> = Vec::new();
        for m in memberships.iter() {
            if m.user_sub == exclude_sub {
                continue;
            }
            if out.iter().any(|p| p.user_sub == m.user_sub) {
                continue;
            }
            out.push(Person {
                user_sub: m.user_sub.clone(),
                user_email: m.user_email.clone(),
            });
        }
        // Email-first ordering, ties broken by subject for a stable directory.
        out.sort_by(|a, b| {
            a.user_email
                .cmp(&b.user_email)
                .then_with(|| a.user_sub.cmp(&b.user_sub))
        });
        out.truncate(crate::config::DIRECTORY_LIMIT as usize);
        Ok(out)
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
                last_read_message_id: String::new(),
                banned: false,
            });
        }
        Ok(())
    }

    async fn is_member(&self, room_id: &str, user_sub: &str) -> Result<bool, StoreError> {
        // A banned membership does NOT authorize: the user is treated as a non-member.
        Ok(self
            .memberships
            .lock()
            .expect("memberships lock poisoned")
            .iter()
            .any(|m| m.room_id == room_id && m.user_sub == user_sub && !m.banned))
    }

    async fn list_messages(
        &self,
        room_id: &str,
        before: Option<MessageCursor>,
        limit: i64,
    ) -> Result<Vec<Message>, StoreError> {
        let messages = self.messages.lock().expect("messages lock poisoned");
        let mut v: Vec<Message> = messages
            .iter()
            .filter(|m| m.room_id == room_id)
            .filter(|m| {
                before
                    .as_ref()
                    .is_none_or(|cursor| tuple_is_before(m, cursor))
            })
            .cloned()
            .collect();
        // Newest-first; ties broken by id so output is stable + keyset-safe.
        v.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        v.truncate(limit.max(0) as usize);
        Ok(v)
    }

    async fn create_message(&self, message: &Message) -> Result<(), StoreError> {
        self.messages
            .lock()
            .expect("messages lock poisoned")
            .push(message.clone());
        Ok(())
    }

    async fn get_message(&self, id: &str) -> Result<Option<Message>, StoreError> {
        Ok(self
            .messages
            .lock()
            .expect("messages lock poisoned")
            .iter()
            .find(|m| m.id == id)
            .cloned())
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

    async fn toggle_reaction(
        &self,
        message_id: &str,
        user_sub: &str,
        emoji: &str,
    ) -> Result<bool, StoreError> {
        let mut reactions = self.reactions.lock().expect("reactions lock poisoned");
        let before = reactions.len();
        // Remove the exact triple if present (toggle-off).
        reactions.retain(|r| {
            !(r.message_id == message_id && r.user_sub == user_sub && r.emoji == emoji)
        });
        if reactions.len() != before {
            return Ok(false); // was present -> removed
        }
        // Absent -> add it (toggle-on).
        reactions.push(ReactionRow {
            message_id: message_id.to_string(),
            user_sub: user_sub.to_string(),
            emoji: emoji.to_string(),
        });
        Ok(true)
    }

    async fn list_reactions(&self, message_id: &str) -> Result<Vec<ReactionCount>, StoreError> {
        let reactions = self.reactions.lock().expect("reactions lock poisoned");
        // Tally distinct-user counts per emoji.
        let mut counts: Vec<ReactionCount> = Vec::new();
        for r in reactions.iter().filter(|r| r.message_id == message_id) {
            if let Some(c) = counts.iter_mut().find(|c| c.emoji == r.emoji) {
                c.count += 1;
            } else {
                counts.push(ReactionCount {
                    emoji: r.emoji.clone(),
                    count: 1,
                });
            }
        }
        // Most-used first, ties broken by emoji for a stable order (matches the SQL ORDER BY).
        counts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.emoji.cmp(&b.emoji)));
        Ok(counts)
    }

    async fn list_user_reactions(
        &self,
        message_id: &str,
        user_sub: &str,
    ) -> Result<Vec<String>, StoreError> {
        let mut mine: Vec<String> = self
            .reactions
            .lock()
            .expect("reactions lock poisoned")
            .iter()
            .filter(|r| r.message_id == message_id && r.user_sub == user_sub)
            .map(|r| r.emoji.clone())
            .collect();
        mine.sort();
        Ok(mine)
    }

    async fn count_replies(&self, room_id: &str, message_id: &str) -> Result<i64, StoreError> {
        Ok(self
            .messages
            .lock()
            .expect("messages lock poisoned")
            .iter()
            .filter(|message| {
                message.room_id == room_id && message.reply_to_id.as_deref() == Some(message_id)
            })
            .count() as i64)
    }

    async fn list_thread_participants(
        &self,
        room_id: &str,
        parent_message_id: &str,
    ) -> Result<Vec<Person>, StoreError> {
        let memberships = self.memberships.lock().expect("memberships lock poisoned");
        let messages = self.messages.lock().expect("messages lock poisoned");
        let mut out: Vec<(Person, i64)> = Vec::new();
        for msg in messages.iter().filter(|m| {
            m.room_id == room_id
                && (m.id == parent_message_id
                    || m.reply_to_id.as_deref() == Some(parent_message_id))
        }) {
            let Some(member) = memberships
                .iter()
                .find(|m| m.room_id == room_id && m.user_sub == msg.sender_sub && !m.banned)
            else {
                continue;
            };
            if let Some((_, first_at)) = out.iter_mut().find(|(p, _)| p.user_sub == member.user_sub)
            {
                *first_at = (*first_at).min(msg.created_at);
                continue;
            }
            out.push((
                Person {
                    user_sub: member.user_sub.clone(),
                    user_email: member.user_email.clone(),
                },
                msg.created_at,
            ));
        }
        out.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.user_sub.cmp(&b.0.user_sub)));
        Ok(out.into_iter().map(|(p, _)| p).collect())
    }

    async fn list_all_rooms(&self) -> Result<Vec<Room>, StoreError> {
        let mut out: Vec<Room> = self
            .rooms
            .lock()
            .expect("rooms lock poisoned")
            .iter()
            .cloned()
            .collect();
        // Newest-created first, ties broken by id for a stable admin ordering.
        out.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(out)
    }

    async fn set_room_archived(&self, room_id: &str, archived: bool) -> Result<(), StoreError> {
        let mut rooms = self.rooms.lock().expect("rooms lock poisoned");
        if let Some(r) = rooms.iter_mut().find(|r| r.id == room_id) {
            r.archived = archived;
        }
        Ok(())
    }

    async fn list_room_members(&self, room_id: &str) -> Result<Vec<Member>, StoreError> {
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
        Ok(out)
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

    async fn set_room_topic(&self, room_id: &str, topic: &str) -> Result<(), StoreError> {
        let mut rooms = self.rooms.lock().expect("rooms lock poisoned");
        if let Some(r) = rooms.iter_mut().find(|r| r.id == room_id) {
            r.topic = topic.to_string();
        }
        Ok(())
    }

    async fn pin_message(
        &self,
        room_id: &str,
        message_id: &str,
        pinned_by: &str,
        pinned_at: i64,
    ) -> Result<(), StoreError> {
        let mut pinned = self.pinned.lock().expect("pinned lock poisoned");
        if !pinned
            .iter()
            .any(|p| p.room_id == room_id && p.message_id == message_id)
        {
            pinned.push(PinnedRow {
                room_id: room_id.to_string(),
                message_id: message_id.to_string(),
                pinned_by: pinned_by.to_string(),
                pinned_at,
            });
        }
        Ok(())
    }

    async fn unpin_message(&self, room_id: &str, message_id: &str) -> Result<(), StoreError> {
        self.pinned
            .lock()
            .expect("pinned lock poisoned")
            .retain(|p| !(p.room_id == room_id && p.message_id == message_id));
        Ok(())
    }

    async fn list_pinned(&self, room_id: &str) -> Result<Vec<Message>, StoreError> {
        let pinned = self.pinned.lock().expect("pinned lock poisoned");
        let messages = self.messages.lock().expect("messages lock poisoned");
        // Most-recently-pinned first, ties broken by message id for stability.
        let mut rows: Vec<&PinnedRow> = pinned.iter().filter(|p| p.room_id == room_id).collect();
        rows.sort_by(|a, b| {
            b.pinned_at
                .cmp(&a.pinned_at)
                .then_with(|| b.message_id.cmp(&a.message_id))
        });
        Ok(rows
            .into_iter()
            .filter_map(|p| messages.iter().find(|m| m.id == p.message_id).cloned())
            .take(crate::config::PINNED_LIMIT as usize)
            .collect())
    }

    async fn is_pinned(&self, room_id: &str, message_id: &str) -> Result<bool, StoreError> {
        Ok(self
            .pinned
            .lock()
            .expect("pinned lock poisoned")
            .iter()
            .any(|p| p.room_id == room_id && p.message_id == message_id))
    }

    async fn search_user_messages(
        &self,
        user_sub: &str,
        query_lower: &str,
        before: Option<MessageCursor>,
        limit: i64,
    ) -> Result<Vec<MessageHit>, StoreError> {
        let memberships = self.memberships.lock().expect("memberships lock poisoned");
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let messages = self.messages.lock().expect("messages lock poisoned");
        // The rooms this user can see (member + not banned) — the ONLY rooms search may touch.
        let member_rooms: Vec<&str> = memberships
            .iter()
            .filter(|m| m.user_sub == user_sub && !m.banned)
            .map(|m| m.room_id.as_str())
            .collect();
        let mut hits: Vec<&Message> = messages
            .iter()
            .filter(|m| member_rooms.contains(&m.room_id.as_str()))
            .filter(|m| !m.deleted)
            .filter(|m| {
                before
                    .as_ref()
                    .is_none_or(|cursor| tuple_is_before(m, cursor))
            })
            .filter(|m| m.body.to_lowercase().contains(query_lower))
            .collect();
        hits.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        hits.truncate(limit.max(0) as usize);
        Ok(hits
            .into_iter()
            .map(|m| MessageHit {
                room_name: rooms
                    .iter()
                    .find(|r| r.id == m.room_id)
                    .map(|r| r.name.clone())
                    .unwrap_or_default(),
                message: m.clone(),
            })
            .collect())
    }

    async fn add_mention(
        &self,
        message_id: &str,
        room_id: &str,
        mentioned_sub: &str,
        created_at: i64,
    ) -> Result<(), StoreError> {
        let mut mentions = self.mentions.lock().expect("mentions lock poisoned");
        if !mentions
            .iter()
            .any(|mn| mn.message_id == message_id && mn.mentioned_sub == mentioned_sub)
        {
            mentions.push(MentionRow {
                message_id: message_id.to_string(),
                room_id: room_id.to_string(),
                mentioned_sub: mentioned_sub.to_string(),
                created_at,
            });
        }
        Ok(())
    }

    async fn list_user_mentions(
        &self,
        user_sub: &str,
        before: Option<MessageCursor>,
        limit: i64,
    ) -> Result<Vec<MessageHit>, StoreError> {
        let memberships = self.memberships.lock().expect("memberships lock poisoned");
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let messages = self.messages.lock().expect("messages lock poisoned");
        let mentions = self.mentions.lock().expect("mentions lock poisoned");
        let member_rooms: Vec<&str> = memberships
            .iter()
            .filter(|membership| membership.user_sub == user_sub && !membership.banned)
            .map(|membership| membership.room_id.as_str())
            .collect();
        let mut hits: Vec<&Message> = mentions
            .iter()
            .filter(|mn| mn.mentioned_sub == user_sub)
            .filter_map(|mn| {
                messages
                    .iter()
                    .find(|m| m.id == mn.message_id && m.room_id == mn.room_id)
            })
            .filter(|message| member_rooms.contains(&message.room_id.as_str()))
            .filter(|m| !m.deleted)
            .filter(|m| {
                before
                    .as_ref()
                    .is_none_or(|cursor| tuple_is_before(m, cursor))
            })
            .collect();
        hits.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        hits.truncate(limit.max(0) as usize);
        Ok(hits
            .into_iter()
            .map(|m| MessageHit {
                room_name: rooms
                    .iter()
                    .find(|r| r.id == m.room_id)
                    .map(|r| r.name.clone())
                    .unwrap_or_default(),
                message: m.clone(),
            })
            .collect())
    }

    async fn join_active_room(
        &self,
        room_id: &str,
        user_sub: &str,
        user_email: &str,
        joined_at: i64,
    ) -> Result<Room, StoreMutationError> {
        // Global Memory lock order starts with memberships, then rooms.
        let mut memberships = self.memberships.lock().expect("memberships lock poisoned");
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let room = rooms
            .iter()
            .find(|room| room.id == room_id)
            .cloned()
            .ok_or(StoreMutationError::ResourceUnavailable)?;
        if room.kind == "dm" {
            return Err(StoreMutationError::DirectMessageJoin);
        }
        if room.archived {
            return Err(StoreMutationError::RoomArchived);
        }
        if !memberships
            .iter()
            .any(|membership| membership.room_id == room_id && membership.user_sub == user_sub)
        {
            memberships.push(MembershipRow {
                room_id: room_id.to_string(),
                user_sub: user_sub.to_string(),
                user_email: user_email.to_string(),
                joined_at,
                last_read_at: 0,
                last_read_message_id: String::new(),
                banned: false,
            });
        }
        Ok(room)
    }

    async fn open_dm_authorized(
        &self,
        proposed: &Room,
        user_sub: &str,
        user_email: &str,
        peer_sub: &str,
        peer_email: &str,
        joined_at: i64,
    ) -> Result<Room, StoreMutationError> {
        // Global Memory lock order starts with memberships, then rooms. Holding both makes the
        // archive fence and the two membership writes one observable operation.
        let mut memberships = self.memberships.lock().expect("memberships lock poisoned");
        let mut rooms = self.rooms.lock().expect("rooms lock poisoned");
        let mut incarnations = self
            .room_incarnations
            .lock()
            .expect("room incarnations lock poisoned");
        let existing_room = rooms.iter().find(|room| room.id == proposed.id).cloned();
        let room = existing_room.clone().unwrap_or_else(|| proposed.clone());
        if room.kind != "dm" {
            return Err(StoreMutationError::ResourceUnavailable);
        }
        if room.archived {
            return Err(StoreMutationError::RoomArchived);
        }

        // Complete the authorization preflight before mutating either collection. Otherwise a
        // later banned participant could leave an inserted room or the first membership behind.
        for participant_sub in [user_sub, peer_sub] {
            if memberships.iter().any(|membership| {
                membership.room_id == room.id
                    && membership.user_sub == participant_sub
                    && membership.banned
            }) {
                return Err(StoreMutationError::Forbidden);
            }
        }

        if existing_room.is_none() {
            rooms.push(room.clone());
            incarnations.push(RoomIncarnationRow {
                room_id: room.id.clone(),
                incarnation: new_opaque_id(),
            });
        }
        for (participant_sub, participant_email) in [(user_sub, user_email), (peer_sub, peer_email)]
        {
            if memberships.iter().any(|membership| {
                membership.room_id == room.id && membership.user_sub == participant_sub
            }) {
                continue;
            }
            memberships.push(MembershipRow {
                room_id: room.id.clone(),
                user_sub: participant_sub.to_string(),
                user_email: participant_email.to_string(),
                joined_at,
                last_read_at: 0,
                last_read_message_id: String::new(),
                banned: false,
            });
        }
        Ok(room)
    }

    async fn create_message_authorized(
        &self,
        message: &Message,
        mention_tokens: &[String],
    ) -> Result<Vec<Member>, StoreMutationError> {
        // Hold the complete authorization and write set. No observer can see the message without
        // its mention rows, and archive/ban/remove must wait for this boundary to commit.
        let memberships = self.memberships.lock().expect("memberships lock poisoned");
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let mut messages = self.messages.lock().expect("messages lock poisoned");
        let mut mentions = self.mentions.lock().expect("mentions lock poisoned");
        authorize_memory_room(
            &memberships,
            &rooms,
            &message.room_id,
            &message.sender_sub,
            true,
        )?;
        if let Some(parent_id) = message.reply_to_id.as_deref() {
            if !messages
                .iter()
                .any(|parent| parent.id == parent_id && parent.room_id == message.room_id)
            {
                return Err(StoreMutationError::ReplyUnavailable);
            }
        }
        let mentioned: Vec<Member> = memberships
            .iter()
            .filter(|membership| {
                membership.room_id == message.room_id
                    && membership_matches_mention(membership, &message.sender_sub, mention_tokens)
            })
            .map(|membership| Member {
                user_sub: membership.user_sub.clone(),
                user_email: membership.user_email.clone(),
                banned: false,
                joined_at: membership.joined_at,
            })
            .collect();
        messages.push(message.clone());
        for member in &mentioned {
            if !mentions.iter().any(|mention| {
                mention.message_id == message.id && mention.mentioned_sub == member.user_sub
            }) {
                mentions.push(MentionRow {
                    message_id: message.id.clone(),
                    room_id: message.room_id.clone(),
                    mentioned_sub: member.user_sub.clone(),
                    created_at: message.created_at,
                });
            }
        }
        Ok(mentioned)
    }

    async fn edit_message_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
        body: &str,
        edited_at: i64,
    ) -> Result<Message, StoreMutationError> {
        let memberships = self.memberships.lock().expect("memberships lock poisoned");
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let mut messages = self.messages.lock().expect("messages lock poisoned");
        authorize_memory_room(&memberships, &rooms, room_id, user_sub, true)?;
        let message = messages
            .iter_mut()
            .find(|message| message.id == message_id && message.room_id == room_id)
            .ok_or(StoreMutationError::ResourceUnavailable)?;
        if message.sender_sub != user_sub {
            return Err(StoreMutationError::Forbidden);
        }
        if message.deleted {
            return Err(StoreMutationError::MessageDeleted);
        }
        message.body = body.to_string();
        message.edited_at = edited_at;
        Ok(message.clone())
    }

    async fn delete_message_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
    ) -> Result<Message, StoreMutationError> {
        let memberships = self.memberships.lock().expect("memberships lock poisoned");
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let mut messages = self.messages.lock().expect("messages lock poisoned");
        authorize_memory_room(&memberships, &rooms, room_id, user_sub, true)?;
        let message = messages
            .iter_mut()
            .find(|message| message.id == message_id && message.room_id == room_id)
            .ok_or(StoreMutationError::ResourceUnavailable)?;
        if message.sender_sub != user_sub {
            return Err(StoreMutationError::Forbidden);
        }
        message.deleted = true;
        message.body.clear();
        Ok(message.clone())
    }

    async fn toggle_reaction_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
        emoji: &str,
    ) -> Result<ReactionMutation, StoreMutationError> {
        let memberships = self.memberships.lock().expect("memberships lock poisoned");
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let messages = self.messages.lock().expect("messages lock poisoned");
        let mut reactions = self.reactions.lock().expect("reactions lock poisoned");
        authorize_memory_room(&memberships, &rooms, room_id, user_sub, true)?;
        if !messages
            .iter()
            .any(|message| message.id == message_id && message.room_id == room_id)
        {
            return Err(StoreMutationError::ResourceUnavailable);
        }
        let before = reactions.len();
        reactions.retain(|reaction| {
            !(reaction.message_id == message_id
                && reaction.user_sub == user_sub
                && reaction.emoji == emoji)
        });
        let added = if reactions.len() != before {
            false
        } else {
            reactions.push(ReactionRow {
                message_id: message_id.to_string(),
                user_sub: user_sub.to_string(),
                emoji: emoji.to_string(),
            });
            true
        };
        Ok(reaction_projection(&reactions, message_id, user_sub, added))
    }

    async fn update_last_read_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
    ) -> Result<Message, StoreMutationError> {
        let mut memberships = self.memberships.lock().expect("memberships lock poisoned");
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let messages = self.messages.lock().expect("messages lock poisoned");
        if !rooms.iter().any(|room| room.id == room_id) {
            return Err(StoreMutationError::ResourceUnavailable);
        }
        let membership = memberships
            .iter_mut()
            .find(|membership| {
                membership.room_id == room_id
                    && membership.user_sub == user_sub
                    && !membership.banned
            })
            .ok_or(StoreMutationError::ResourceUnavailable)?;
        let message = messages
            .iter()
            .find(|message| message.id == message_id && message.room_id == room_id)
            .cloned()
            .ok_or(StoreMutationError::ResourceUnavailable)?;
        if tuple_is_after(
            message.created_at,
            &message.id,
            membership.last_read_at,
            &membership.last_read_message_id,
        ) {
            membership.last_read_at = message.created_at;
            membership.last_read_message_id = message.id.clone();
        }
        Ok(message)
    }

    async fn set_room_topic_authorized(
        &self,
        room_id: &str,
        topic: &str,
    ) -> Result<Room, StoreMutationError> {
        let mut rooms = self.rooms.lock().expect("rooms lock poisoned");
        let room = rooms
            .iter_mut()
            .find(|room| room.id == room_id)
            .ok_or(StoreMutationError::ResourceUnavailable)?;
        if room.archived {
            return Err(StoreMutationError::RoomArchived);
        }
        room.topic = topic.to_string();
        Ok(room.clone())
    }

    async fn pin_message_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        pinned_by: &str,
        pinned_at: i64,
    ) -> Result<Vec<Message>, StoreMutationError> {
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let mut pinned = self.pinned.lock().expect("pinned lock poisoned");
        let messages = self.messages.lock().expect("messages lock poisoned");
        let room = rooms
            .iter()
            .find(|room| room.id == room_id)
            .ok_or(StoreMutationError::ResourceUnavailable)?;
        if room.archived {
            return Err(StoreMutationError::RoomArchived);
        }
        if !messages
            .iter()
            .any(|message| message.id == message_id && message.room_id == room_id)
        {
            return Err(StoreMutationError::ResourceUnavailable);
        }
        if !pinned
            .iter()
            .any(|row| row.room_id == room_id && row.message_id == message_id)
        {
            pinned.push(PinnedRow {
                room_id: room_id.to_string(),
                message_id: message_id.to_string(),
                pinned_by: pinned_by.to_string(),
                pinned_at,
            });
        }
        Ok(pinned_projection(&pinned, &messages, room_id))
    }

    async fn unpin_message_authorized(
        &self,
        room_id: &str,
        message_id: &str,
    ) -> Result<Vec<Message>, StoreMutationError> {
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let mut pinned = self.pinned.lock().expect("pinned lock poisoned");
        let messages = self.messages.lock().expect("messages lock poisoned");
        let room = rooms
            .iter()
            .find(|room| room.id == room_id)
            .ok_or(StoreMutationError::ResourceUnavailable)?;
        if room.archived {
            return Err(StoreMutationError::RoomArchived);
        }
        pinned.retain(|row| !(row.room_id == room_id && row.message_id == message_id));
        Ok(pinned_projection(&pinned, &messages, room_id))
    }

    async fn authorize_room_read(
        &self,
        room_id: &str,
        user_sub: &str,
        require_active: bool,
    ) -> Result<Room, StoreMutationError> {
        let memberships = self.memberships.lock().expect("memberships lock poisoned");
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        authorize_memory_read(&memberships, &rooms, room_id, user_sub, require_active)
    }

    async fn list_messages_authorized(
        &self,
        room_id: &str,
        user_sub: &str,
        before: Option<MessageCursor>,
        limit: i64,
    ) -> Result<Vec<Message>, StoreMutationError> {
        let memberships = self.memberships.lock().expect("memberships lock poisoned");
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let messages = self.messages.lock().expect("messages lock poisoned");
        authorize_memory_read(&memberships, &rooms, room_id, user_sub, false)?;
        let mut rows: Vec<Message> = messages
            .iter()
            .filter(|message| message.room_id == room_id)
            .filter(|message| {
                before
                    .as_ref()
                    .is_none_or(|cursor| tuple_is_before(message, cursor))
            })
            .cloned()
            .collect();
        rows.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        rows.truncate(limit.max(0) as usize);
        Ok(rows)
    }

    async fn get_message_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
    ) -> Result<Option<Message>, StoreMutationError> {
        let memberships = self.memberships.lock().expect("memberships lock poisoned");
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let messages = self.messages.lock().expect("messages lock poisoned");
        authorize_memory_read(&memberships, &rooms, room_id, user_sub, false)?;
        Ok(messages
            .iter()
            .find(|message| message.room_id == room_id && message.id == message_id)
            .cloned())
    }

    async fn reaction_projection_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
    ) -> Result<ReactionProjection, StoreMutationError> {
        let memberships = self.memberships.lock().expect("memberships lock poisoned");
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let messages = self.messages.lock().expect("messages lock poisoned");
        let reactions = self.reactions.lock().expect("reactions lock poisoned");
        authorize_memory_read(&memberships, &rooms, room_id, user_sub, false)?;
        if !messages
            .iter()
            .any(|message| message.room_id == room_id && message.id == message_id)
        {
            return Err(StoreMutationError::ResourceUnavailable);
        }
        let projected = reaction_projection(&reactions, message_id, user_sub, false);
        Ok(ReactionProjection {
            reactions: projected.reactions,
            mine: projected.mine,
        })
    }

    async fn list_pinned_authorized(
        &self,
        room_id: &str,
        user_sub: &str,
    ) -> Result<Vec<Message>, StoreMutationError> {
        let memberships = self.memberships.lock().expect("memberships lock poisoned");
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let pinned = self.pinned.lock().expect("pinned lock poisoned");
        let messages = self.messages.lock().expect("messages lock poisoned");
        authorize_memory_read(&memberships, &rooms, room_id, user_sub, false)?;
        Ok(pinned_projection(&pinned, &messages, room_id))
    }

    async fn count_replies_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
    ) -> Result<i64, StoreMutationError> {
        let memberships = self.memberships.lock().expect("memberships lock poisoned");
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let messages = self.messages.lock().expect("messages lock poisoned");
        authorize_memory_read(&memberships, &rooms, room_id, user_sub, false)?;
        if !messages
            .iter()
            .any(|message| message.room_id == room_id && message.id == message_id)
        {
            return Err(StoreMutationError::ResourceUnavailable);
        }
        Ok(messages
            .iter()
            .filter(|message| {
                message.room_id == room_id && message.reply_to_id.as_deref() == Some(message_id)
            })
            .count() as i64)
    }

    async fn issue_room_delete_token(
        &self,
        token_digest: &str,
        room_id: &str,
        actor_sub: &str,
        csrf_digest: &str,
        issued_at: i64,
        expires_at: i64,
    ) -> Result<Room, StoreMutationError> {
        let mut tokens = self
            .room_delete_tokens
            .lock()
            .expect("room delete tokens lock poisoned");
        tokens.retain(|token| token.expires_at >= issued_at);
        if tokens
            .iter()
            .any(|token| token.token_digest == token_digest)
        {
            return Err(StoreMutationError::Backend(
                "duplicate room delete token digest".to_string(),
            ));
        }
        let rooms = self.rooms.lock().expect("rooms lock poisoned");
        let room = rooms
            .iter()
            .find(|room| room.id == room_id)
            .cloned()
            .ok_or(StoreMutationError::ResourceUnavailable)?;
        let incarnations = self
            .room_incarnations
            .lock()
            .expect("room incarnations lock poisoned");
        let room_incarnation = incarnations
            .iter()
            .find(|row| row.room_id == room_id)
            .map(|row| row.incarnation.clone())
            .ok_or_else(|| {
                StoreMutationError::Backend("room incarnation unavailable".to_string())
            })?;
        tokens.push(RoomDeleteTokenRow {
            token_digest: token_digest.to_string(),
            room_id: room_id.to_string(),
            room_incarnation,
            actor_sub: actor_sub.to_string(),
            csrf_digest: csrf_digest.to_string(),
            purpose: ROOM_DELETE_PURPOSE.to_string(),
            issued_at,
            expires_at,
        });
        Ok(room)
    }

    async fn delete_room_with_token(
        &self,
        room_id: &str,
        token_digest: &str,
        actor_sub: &str,
        csrf_digest: &str,
        now: i64,
    ) -> Result<RoomDeleteAuditConsequence, StoreMutationError> {
        let mut tokens = self
            .room_delete_tokens
            .lock()
            .expect("room delete tokens lock poisoned");
        let bound_incarnation = tokens
            .iter()
            .find(|token| {
                token.token_digest == token_digest
                    && token.room_id == room_id
                    && token.actor_sub == actor_sub
                    && token.csrf_digest == csrf_digest
                    && token.purpose == ROOM_DELETE_PURPOSE
                    && token.issued_at <= now
                    && token.expires_at >= now
            })
            .map(|token| token.room_incarnation.clone())
            .ok_or(StoreMutationError::ConsequenceTokenInvalid)?;

        let mut memberships = self.memberships.lock().expect("memberships lock poisoned");
        let mut rooms = self.rooms.lock().expect("rooms lock poisoned");
        if !rooms.iter().any(|room| room.id == room_id) {
            return Err(StoreMutationError::ConsequenceTokenInvalid);
        }
        let mut incarnations = self
            .room_incarnations
            .lock()
            .expect("room incarnations lock poisoned");
        let current_incarnation = incarnations
            .iter()
            .find(|row| row.room_id == room_id)
            .map(|row| row.incarnation.as_str())
            .ok_or_else(|| {
                StoreMutationError::Backend("room incarnation unavailable".to_string())
            })?;
        if current_incarnation != bound_incarnation {
            return Err(StoreMutationError::ConsequenceTokenInvalid);
        }
        let mut pinned = self.pinned.lock().expect("pinned lock poisoned");
        let mut messages = self.messages.lock().expect("messages lock poisoned");
        let mut reactions = self.reactions.lock().expect("reactions lock poisoned");
        let mut mentions = self.mentions.lock().expect("mentions lock poisoned");
        let mut audits = self
            .room_delete_audits
            .lock()
            .expect("room delete audits lock poisoned");
        let consequence = RoomDeleteAuditConsequence {
            id: new_opaque_id(),
            room_id: room_id.to_string(),
            actor_sub: actor_sub.to_string(),
            occurred_at: now,
        };
        audits.push(RoomDeleteAuditRow {
            consequence: consequence.clone(),
            delivered_at: 0,
        });
        let doomed: Vec<&str> = messages
            .iter()
            .filter(|message| message.room_id == room_id)
            .map(|message| message.id.as_str())
            .collect();
        reactions.retain(|reaction| !doomed.contains(&reaction.message_id.as_str()));
        messages.retain(|message| message.room_id != room_id);
        pinned.retain(|row| row.room_id != room_id);
        mentions.retain(|mention| mention.room_id != room_id);
        memberships.retain(|membership| membership.room_id != room_id);
        rooms.retain(|room| room.id != room_id);
        incarnations.retain(|row| row.room_id != room_id);
        tokens.retain(|token| token.room_id != room_id);
        Ok(consequence)
    }

    async fn pending_room_delete_audits(
        &self,
        limit: i64,
    ) -> Result<Vec<RoomDeleteAuditConsequence>, StoreError> {
        let audits = self
            .room_delete_audits
            .lock()
            .expect("room delete audits lock poisoned");
        let mut pending: Vec<_> = audits
            .iter()
            .filter(|row| row.delivered_at == 0)
            .map(|row| row.consequence.clone())
            .collect();
        pending.sort_by(|a, b| {
            a.occurred_at
                .cmp(&b.occurred_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        pending.truncate(limit.max(0) as usize);
        Ok(pending)
    }

    async fn mark_room_delete_audit_delivered(
        &self,
        consequence_id: &str,
        delivered_at: i64,
    ) -> Result<(), StoreError> {
        let mut audits = self
            .room_delete_audits
            .lock()
            .expect("room delete audits lock poisoned");
        if let Some(row) = audits
            .iter_mut()
            .find(|row| row.consequence.id == consequence_id && row.delivered_at == 0)
        {
            row.delivered_at = delivered_at;
        }
        Ok(())
    }
}

fn authorize_memory_room(
    memberships: &[MembershipRow],
    rooms: &[Room],
    room_id: &str,
    user_sub: &str,
    require_active: bool,
) -> Result<(), StoreMutationError> {
    let room = rooms
        .iter()
        .find(|room| room.id == room_id)
        .ok_or(StoreMutationError::ResourceUnavailable)?;
    if !memberships.iter().any(|membership| {
        membership.room_id == room_id && membership.user_sub == user_sub && !membership.banned
    }) {
        return Err(StoreMutationError::ResourceUnavailable);
    }
    if require_active && room.archived {
        return Err(StoreMutationError::RoomArchived);
    }
    Ok(())
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
use sqlx::{Postgres, Row, Transaction};

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
            "ALTER TABLE memberships \
             ADD COLUMN IF NOT EXISTS last_read_message_id TEXT NOT NULL DEFAULT ''",
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

        // Threaded-reply parent pointer. Nullable TEXT (no default => NULL = top-level post);
        // added idempotently so an existing `messages` table upgrades in place.
        sqlx::query("ALTER TABLE messages ADD COLUMN IF NOT EXISTS reply_to_id TEXT")
            .execute(&self.pool)
            .await?;

        // Message reactions: one row per (message, user, emoji). The composite PRIMARY KEY IS the
        // uniqueness/idempotency guard, so a repeat toggle-on is a no-op via ON CONFLICT.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS message_reactions (\
                 message_id TEXT NOT NULL, \
                 user_sub TEXT NOT NULL, \
                 emoji TEXT NOT NULL, \
                 created_at BIGINT NOT NULL DEFAULT 0, \
                 PRIMARY KEY (message_id, user_sub, emoji)\
             )",
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
            "ALTER TABLE rooms ADD COLUMN IF NOT EXISTS incarnation TEXT NOT NULL DEFAULT ''",
        )
        .execute(&self.pool)
        .await?;
        // Existing rooms predate explicit incarnations. Assign each one a CSPRNG identity with a
        // compare-and-set predicate so concurrent startup migrations cannot overwrite a winner.
        let legacy_rooms = sqlx::query("SELECT id FROM rooms WHERE incarnation = ''")
            .fetch_all(&self.pool)
            .await?;
        for row in legacy_rooms {
            let room_id: String = row.try_get("id")?;
            sqlx::query("UPDATE rooms SET incarnation = $1 WHERE id = $2 AND incarnation = ''")
                .bind(new_opaque_id())
                .bind(room_id)
                .execute(&self.pool)
                .await?;
        }
        sqlx::query(
            "ALTER TABLE memberships ADD COLUMN IF NOT EXISTS banned BOOLEAN NOT NULL DEFAULT FALSE",
        )
        .execute(&self.pool)
        .await?;

        // Room topic / description / purpose. Added idempotently so an existing `rooms` table
        // upgrades in place (portable ALTER — the default backfills old rows).
        sqlx::query("ALTER TABLE rooms ADD COLUMN IF NOT EXISTS topic TEXT NOT NULL DEFAULT ''")
            .execute(&self.pool)
            .await?;

        // Pinned messages: one row per (room, message). The composite PRIMARY KEY IS the
        // idempotency guard, so a repeat pin is a no-op via ON CONFLICT.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS pinned_messages (\
                 room_id TEXT NOT NULL, \
                 message_id TEXT NOT NULL, \
                 pinned_by TEXT NOT NULL DEFAULT '', \
                 pinned_at BIGINT NOT NULL DEFAULT 0, \
                 PRIMARY KEY (room_id, message_id)\
             )",
        )
        .execute(&self.pool)
        .await?;

        // @mentions: one row per (message, mentioned user). `room_id` + `created_at` are copied off
        // the message so the room-list mention indicator + global mentions view scan without a join
        // to `messages`. The composite PRIMARY KEY guards idempotency.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS mentions (\
                 message_id TEXT NOT NULL, \
                 room_id TEXT NOT NULL, \
                 mentioned_sub TEXT NOT NULL, \
                 created_at BIGINT NOT NULL DEFAULT 0, \
                 PRIMARY KEY (message_id, mentioned_sub)\
             )",
        )
        .execute(&self.pool)
        .await?;

        // Digest-only, actor/room/CSRF-bound one-time authorization for the irreversible admin
        // room purge. Consumption and deletion commit in one transaction.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS room_delete_tokens (\
                 token_digest TEXT PRIMARY KEY, \
                 room_id TEXT NOT NULL, \
                 room_incarnation TEXT NOT NULL, \
                 actor_sub TEXT NOT NULL, \
                 csrf_digest TEXT NOT NULL, \
                 purpose TEXT NOT NULL, \
                 issued_at BIGINT NOT NULL, \
                 expires_at BIGINT NOT NULL\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE room_delete_tokens \
             ADD COLUMN IF NOT EXISTS room_incarnation TEXT NOT NULL DEFAULT ''",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE room_delete_tokens \
             ADD COLUMN IF NOT EXISTS purpose TEXT NOT NULL DEFAULT 'room-hard-delete'",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE room_delete_tokens \
             ADD COLUMN IF NOT EXISTS issued_at BIGINT NOT NULL DEFAULT 0",
        )
        .execute(&self.pool)
        .await?;

        // Transactional outbox for irreversible hard-delete consequences. A delete cannot commit
        // without exactly one durable row; Watchtower delivery is retried independently.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS room_delete_audit_outbox (\
                 consequence_id TEXT PRIMARY KEY, \
                 room_id TEXT NOT NULL, \
                 room_incarnation TEXT NOT NULL, \
                 actor_sub TEXT NOT NULL, \
                 occurred_at BIGINT NOT NULL, \
                 delivered_at BIGINT NOT NULL DEFAULT 0\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_room_delete_audit_pending \
             ON room_delete_audit_outbox (delivered_at, occurred_at, consequence_id)",
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
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_messages_room_cursor \
             ON messages (room_id, created_at, id)",
        )
        .execute(&self.pool)
        .await?;
        // Backs the "rooms this user belongs to" lookup.
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_memberships_user ON memberships (user_sub)")
            .execute(&self.pool)
            .await?;
        // Backs the per-message reaction tally + per-user reaction lookup.
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_reactions_message \
             ON message_reactions (message_id)",
        )
        .execute(&self.pool)
        .await?;
        // Backs the per-parent reply count.
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_reply ON messages (reply_to_id)")
            .execute(&self.pool)
            .await?;
        // Backs the per-room pinned panel lookup.
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_pinned_room ON pinned_messages (room_id)")
            .execute(&self.pool)
            .await?;
        // Backs the global mentions view + the per-room mention indicator.
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_mentions_user ON mentions (mentioned_sub)")
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
            topic: row.try_get("topic")?,
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
            reply_to_id: row.try_get("reply_to_id")?,
        })
    }

    /// Build a [`MessageHit`] from a joined row: the message columns plus the `room_name` alias.
    fn message_hit_from_row(row: &sqlx::postgres::PgRow) -> Result<MessageHit, sqlx::Error> {
        Ok(MessageHit {
            room_name: row.try_get("room_name")?,
            message: Self::message_from_row(row)?,
        })
    }

    async fn lock_member_room(
        tx: &mut Transaction<'_, Postgres>,
        room_id: &str,
        user_sub: &str,
        require_active: bool,
    ) -> Result<Room, StoreMutationError> {
        // One global PostgreSQL lock order: room, then membership, then message/projection rows.
        // This matches hard delete and prevents a revoke/delete race from becoming a deadlock.
        let room = Self::lock_room(tx, room_id, false).await?;
        let membership = sqlx::query(
            "SELECT 1 AS one FROM memberships \
             WHERE room_id = $1 AND user_sub = $2 AND banned = FALSE FOR UPDATE",
        )
        .bind(room_id)
        .bind(user_sub)
        .fetch_optional(&mut **tx)
        .await?;
        if membership.is_none() {
            return Err(StoreMutationError::ResourceUnavailable);
        }
        if require_active && room.archived {
            return Err(StoreMutationError::RoomArchived);
        }
        Ok(room)
    }

    async fn lock_read_authority(
        tx: &mut Transaction<'_, Postgres>,
        room_id: &str,
        user_sub: &str,
        require_active: bool,
    ) -> Result<Room, StoreMutationError> {
        // Lock order matches all admin revocation writes: room, then membership.
        let row = sqlx::query(
            "SELECT id, name, kind, created_by, created_at, archived, topic \
             FROM rooms WHERE id = $1 FOR SHARE",
        )
        .bind(room_id)
        .fetch_optional(&mut **tx)
        .await?;
        let room = row
            .as_ref()
            .map(Self::room_from_row)
            .transpose()?
            .ok_or(StoreMutationError::RoomNotFound)?;
        let membership = sqlx::query(
            "SELECT banned FROM memberships \
             WHERE room_id = $1 AND user_sub = $2 FOR SHARE",
        )
        .bind(room_id)
        .bind(user_sub)
        .fetch_optional(&mut **tx)
        .await?;
        let membership = membership.ok_or(StoreMutationError::NotMember)?;
        if membership.try_get::<bool, _>("banned")? {
            return Err(StoreMutationError::MemberBanned);
        }
        if require_active && room.archived {
            return Err(StoreMutationError::RoomArchived);
        }
        Ok(room)
    }

    async fn lock_room(
        tx: &mut Transaction<'_, Postgres>,
        room_id: &str,
        require_active: bool,
    ) -> Result<Room, StoreMutationError> {
        let row = sqlx::query(
            "SELECT id, name, kind, created_by, created_at, archived, topic \
             FROM rooms WHERE id = $1 FOR UPDATE",
        )
        .bind(room_id)
        .fetch_optional(&mut **tx)
        .await?;
        let room = row
            .as_ref()
            .map(Self::room_from_row)
            .transpose()?
            .ok_or(StoreMutationError::ResourceUnavailable)?;
        if require_active && room.archived {
            return Err(StoreMutationError::RoomArchived);
        }
        Ok(room)
    }

    async fn lock_message(
        tx: &mut Transaction<'_, Postgres>,
        room_id: &str,
        message_id: &str,
    ) -> Result<Message, StoreMutationError> {
        let row = sqlx::query(
            "SELECT id, room_id, sender_sub, sender_email, body, created_at, edited_at, deleted, \
             reply_to_id FROM messages WHERE id = $1 AND room_id = $2 FOR UPDATE",
        )
        .bind(message_id)
        .bind(room_id)
        .fetch_optional(&mut **tx)
        .await?;
        row.as_ref()
            .map(Self::message_from_row)
            .transpose()?
            .ok_or(StoreMutationError::ResourceUnavailable)
    }

    async fn pinned_projection_tx(
        tx: &mut Transaction<'_, Postgres>,
        room_id: &str,
    ) -> Result<Vec<Message>, StoreMutationError> {
        let rows = sqlx::query(
            "SELECT m.id, m.room_id, m.sender_sub, m.sender_email, m.body, m.created_at, \
             m.edited_at, m.deleted, m.reply_to_id \
             FROM pinned_messages p JOIN messages m ON m.id = p.message_id \
             WHERE p.room_id = $1 ORDER BY p.pinned_at DESC, m.id DESC LIMIT $2",
        )
        .bind(room_id)
        .bind(crate::config::PINNED_LIMIT)
        .fetch_all(&mut **tx)
        .await?;
        rows.iter()
            .map(Self::message_from_row)
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreMutationError::from)
    }

    async fn ensure_room_async(&self, room: &Room) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO rooms \
             (id, name, kind, created_by, created_at, archived, topic, incarnation) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (id) DO NOTHING",
        )
        .bind(&room.id)
        .bind(&room.name)
        .bind(&room.kind)
        .bind(&room.created_by)
        .bind(room.created_at)
        .bind(room.archived)
        .bind(&room.topic)
        .bind(new_opaque_id())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_room_async(&self, id: &str) -> Result<Option<Room>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, name, kind, created_by, created_at, archived, topic FROM rooms WHERE id = $1",
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
        // Two correlated subqueries derive the unread state per row: `unread` counts messages newer
        // than the read cursor that this user did NOT author; `mention_unread` counts unread
        // @mentions of this user. Both stay standard SQL (no window functions / lateral joins).
        let rows = sqlx::query(
            "SELECT r.id, r.name, r.kind, r.created_by, r.created_at, r.archived, r.topic, \
             m.last_read_at, m.last_read_message_id, \
             (SELECT COUNT(*) FROM messages msg WHERE msg.room_id = r.id \
                AND (msg.created_at > m.last_read_at OR \
                     (msg.created_at = m.last_read_at AND msg.id > m.last_read_message_id)) \
                AND msg.sender_sub <> $1 \
                AND msg.deleted = FALSE) AS unread, \
             (SELECT COUNT(*) FROM mentions mn JOIN messages mm ON mm.id = mn.message_id \
                WHERE mn.room_id = r.id AND mn.mentioned_sub = $1 \
                AND (mm.created_at > m.last_read_at OR \
                     (mm.created_at = m.last_read_at AND mm.id > m.last_read_message_id)) \
                AND mm.deleted = FALSE) AS mention_unread \
             FROM rooms r JOIN memberships m ON m.room_id = r.id \
             WHERE m.user_sub = $1 AND m.banned = FALSE \
             ORDER BY r.created_at ASC, r.id ASC LIMIT $2",
        )
        .bind(user_sub)
        .bind(crate::config::ROOM_LIST_LIMIT)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let mention_unread: i64 = r.try_get("mention_unread")?;
                Ok(UserRoom {
                    room: Self::room_from_row(r)?,
                    last_read_at: r.try_get("last_read_at")?,
                    last_read_message_id: r.try_get("last_read_message_id")?,
                    unread: r.try_get("unread")?,
                    mentioned: mention_unread > 0,
                })
            })
            .collect()
    }

    async fn list_directory_async(&self, exclude_sub: &str) -> Result<Vec<Person>, sqlx::Error> {
        // One row per distinct subject (portable GROUP BY; MIN picks a stable email), excluding
        // the caller. Standard SQL only — no DISTINCT ON, no window functions.
        let rows = sqlx::query(
            "SELECT user_sub, MIN(user_email) AS user_email FROM memberships \
             WHERE user_sub <> $1 GROUP BY user_sub \
             ORDER BY MIN(user_email) ASC, user_sub ASC LIMIT $2",
        )
        .bind(exclude_sub)
        .bind(crate::config::DIRECTORY_LIMIT)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(Person {
                    user_sub: r.try_get("user_sub")?,
                    user_email: r.try_get("user_email")?,
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

    async fn list_messages_async(
        &self,
        room_id: &str,
        before: Option<MessageCursor>,
        limit: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let cursor_at = before.as_ref().map(|cursor| cursor.created_at);
        let cursor_id = before.as_ref().map(|cursor| cursor.message_id.as_str());
        let rows = sqlx::query(
            "SELECT id, room_id, sender_sub, sender_email, body, created_at, edited_at, deleted, \
             reply_to_id \
             FROM messages WHERE room_id = $1 AND \
             ($2::BIGINT IS NULL OR created_at < $2 OR (created_at = $2 AND id < $3)) \
             ORDER BY created_at DESC, id DESC LIMIT $4",
        )
        .bind(room_id)
        .bind(cursor_at)
        .bind(cursor_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::message_from_row).collect()
    }

    async fn create_message_async(&self, m: &Message) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO messages \
             (id, room_id, sender_sub, sender_email, body, created_at, edited_at, deleted, \
              reply_to_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&m.id)
        .bind(&m.room_id)
        .bind(&m.sender_sub)
        .bind(&m.sender_email)
        .bind(&m.body)
        .bind(m.created_at)
        .bind(m.edited_at)
        .bind(m.deleted)
        .bind(&m.reply_to_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_message_async(&self, id: &str) -> Result<Option<Message>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, room_id, sender_sub, sender_email, body, created_at, edited_at, deleted, \
             reply_to_id \
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

    async fn toggle_reaction_async(
        &self,
        message_id: &str,
        user_sub: &str,
        emoji: &str,
    ) -> Result<bool, sqlx::Error> {
        // Toggle = try to delete the exact triple; if a row was removed it was ON, so we turned it
        // OFF. If nothing was deleted it was absent, so we insert (ON CONFLICT DO NOTHING keeps the
        // insert idempotent against a concurrent add). Standard SQL, no upsert-with-toggle needed.
        let deleted = sqlx::query(
            "DELETE FROM message_reactions \
             WHERE message_id = $1 AND user_sub = $2 AND emoji = $3",
        )
        .bind(message_id)
        .bind(user_sub)
        .bind(emoji)
        .execute(&self.pool)
        .await?;
        if deleted.rows_affected() > 0 {
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO message_reactions (message_id, user_sub, emoji, created_at) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (message_id, user_sub, emoji) DO NOTHING",
        )
        .bind(message_id)
        .bind(user_sub)
        .bind(emoji)
        .bind(crate::now_secs())
        .execute(&self.pool)
        .await?;
        Ok(true)
    }

    async fn list_reactions_async(
        &self,
        message_id: &str,
    ) -> Result<Vec<ReactionCount>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT emoji, COUNT(*) AS cnt FROM message_reactions \
             WHERE message_id = $1 GROUP BY emoji \
             ORDER BY COUNT(*) DESC, emoji ASC",
        )
        .bind(message_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(ReactionCount {
                    emoji: r.try_get("emoji")?,
                    count: r.try_get("cnt")?,
                })
            })
            .collect()
    }

    async fn list_user_reactions_async(
        &self,
        message_id: &str,
        user_sub: &str,
    ) -> Result<Vec<String>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT emoji FROM message_reactions \
             WHERE message_id = $1 AND user_sub = $2 ORDER BY emoji ASC",
        )
        .bind(message_id)
        .bind(user_sub)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|r| r.try_get("emoji")).collect()
    }

    async fn count_replies_async(
        &self,
        room_id: &str,
        message_id: &str,
    ) -> Result<i64, sqlx::Error> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS cnt FROM messages \
             WHERE room_id = $1 AND reply_to_id = $2",
        )
        .bind(room_id)
        .bind(message_id)
        .fetch_one(&self.pool)
        .await?;
        row.try_get("cnt")
    }

    async fn list_thread_participants_async(
        &self,
        room_id: &str,
        parent_message_id: &str,
    ) -> Result<Vec<Person>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT m.sender_sub AS user_sub, MIN(mem.user_email) AS user_email, \
             MIN(m.created_at) AS first_at \
             FROM messages m \
             JOIN memberships mem ON mem.room_id = m.room_id \
                AND mem.user_sub = m.sender_sub AND mem.banned = FALSE \
             WHERE m.room_id = $1 AND (m.id = $2 OR m.reply_to_id = $2) \
             GROUP BY m.sender_sub ORDER BY MIN(m.created_at) ASC, m.sender_sub ASC",
        )
        .bind(room_id)
        .bind(parent_message_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(Person {
                    user_sub: r.try_get("user_sub")?,
                    user_email: r.try_get("user_email")?,
                })
            })
            .collect()
    }

    async fn list_all_rooms_async(&self) -> Result<Vec<Room>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, name, kind, created_by, created_at, archived, topic FROM rooms \
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
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT id FROM rooms WHERE id = $1 FOR UPDATE")
            .bind(room_id)
            .fetch_optional(&mut *tx)
            .await?;
        sqlx::query("UPDATE rooms SET archived = $1 WHERE id = $2")
            .bind(archived)
            .bind(room_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
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

    async fn remove_member_async(&self, room_id: &str, user_sub: &str) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT id FROM rooms WHERE id = $1 FOR UPDATE")
            .bind(room_id)
            .fetch_optional(&mut *tx)
            .await?;
        sqlx::query(
            "SELECT banned FROM memberships \
             WHERE room_id = $1 AND user_sub = $2 FOR UPDATE",
        )
        .bind(room_id)
        .bind(user_sub)
        .fetch_optional(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM memberships WHERE room_id = $1 AND user_sub = $2")
            .bind(room_id)
            .bind(user_sub)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn ban_member_async(&self, room_id: &str, user_sub: &str) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT id FROM rooms WHERE id = $1 FOR UPDATE")
            .bind(room_id)
            .fetch_optional(&mut *tx)
            .await?;
        sqlx::query(
            "SELECT banned FROM memberships \
             WHERE room_id = $1 AND user_sub = $2 FOR UPDATE",
        )
        .bind(room_id)
        .bind(user_sub)
        .fetch_optional(&mut *tx)
        .await?;
        sqlx::query("UPDATE memberships SET banned = TRUE WHERE room_id = $1 AND user_sub = $2")
            .bind(room_id)
            .bind(user_sub)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
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

    async fn set_room_topic_async(&self, room_id: &str, topic: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE rooms SET topic = $1 WHERE id = $2")
            .bind(topic)
            .bind(room_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn pin_message_async(
        &self,
        room_id: &str,
        message_id: &str,
        pinned_by: &str,
        pinned_at: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO pinned_messages (room_id, message_id, pinned_by, pinned_at) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (room_id, message_id) DO NOTHING",
        )
        .bind(room_id)
        .bind(message_id)
        .bind(pinned_by)
        .bind(pinned_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn unpin_message_async(
        &self,
        room_id: &str,
        message_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM pinned_messages WHERE room_id = $1 AND message_id = $2")
            .bind(room_id)
            .bind(message_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_pinned_async(&self, room_id: &str) -> Result<Vec<Message>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT m.id, m.room_id, m.sender_sub, m.sender_email, m.body, m.created_at, \
             m.edited_at, m.deleted, m.reply_to_id \
             FROM pinned_messages p JOIN messages m ON m.id = p.message_id \
             WHERE p.room_id = $1 \
             ORDER BY p.pinned_at DESC, m.id DESC LIMIT $2",
        )
        .bind(room_id)
        .bind(crate::config::PINNED_LIMIT)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::message_from_row).collect()
    }

    async fn is_pinned_async(&self, room_id: &str, message_id: &str) -> Result<bool, sqlx::Error> {
        let row = sqlx::query(
            "SELECT 1 AS one FROM pinned_messages WHERE room_id = $1 AND message_id = $2",
        )
        .bind(room_id)
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    async fn search_user_messages_async(
        &self,
        user_sub: &str,
        query_lower: &str,
        before: Option<MessageCursor>,
        limit: i64,
    ) -> Result<Vec<MessageHit>, sqlx::Error> {
        // Membership JOIN is the scope guard: a message only surfaces when the caller is a
        // non-banned member of its room — a non-member room can NEVER leak. LIKE wildcards in the
        // needle are escaped so the query is a pure case-insensitive substring match.
        let needle = format!("%{}%", like_escape(query_lower));
        let cursor_at = before.as_ref().map(|cursor| cursor.created_at);
        let cursor_id = before.as_ref().map(|cursor| cursor.message_id.as_str());
        let rows = sqlx::query(
            "SELECT m.id, m.room_id, m.sender_sub, m.sender_email, m.body, m.created_at, \
             m.edited_at, m.deleted, m.reply_to_id, r.name AS room_name \
             FROM messages m \
             JOIN memberships mem ON mem.room_id = m.room_id \
                AND mem.user_sub = $1 AND mem.banned = FALSE \
             JOIN rooms r ON r.id = m.room_id \
             WHERE m.deleted = FALSE AND LOWER(m.body) LIKE $2 ESCAPE '\\' \
             AND ($3::BIGINT IS NULL OR m.created_at < $3 OR \
                  (m.created_at = $3 AND m.id < $4)) \
             ORDER BY m.created_at DESC, m.id DESC LIMIT $5",
        )
        .bind(user_sub)
        .bind(&needle)
        .bind(cursor_at)
        .bind(cursor_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::message_hit_from_row).collect()
    }

    async fn add_mention_async(
        &self,
        message_id: &str,
        room_id: &str,
        mentioned_sub: &str,
        created_at: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO mentions (message_id, room_id, mentioned_sub, created_at) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (message_id, mentioned_sub) DO NOTHING",
        )
        .bind(message_id)
        .bind(room_id)
        .bind(mentioned_sub)
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_user_mentions_async(
        &self,
        user_sub: &str,
        before: Option<MessageCursor>,
        limit: i64,
    ) -> Result<Vec<MessageHit>, sqlx::Error> {
        let cursor_at = before.as_ref().map(|cursor| cursor.created_at);
        let cursor_id = before.as_ref().map(|cursor| cursor.message_id.as_str());
        let rows = sqlx::query(
            "SELECT m.id, m.room_id, m.sender_sub, m.sender_email, m.body, m.created_at, \
             m.edited_at, m.deleted, m.reply_to_id, r.name AS room_name \
             FROM mentions mn \
             JOIN messages m ON m.id = mn.message_id AND m.room_id = mn.room_id \
             JOIN rooms r ON r.id = mn.room_id \
             JOIN memberships mem ON mem.room_id = mn.room_id \
                AND mem.user_sub = $1 AND mem.banned = FALSE \
             WHERE mn.mentioned_sub = $1 AND m.deleted = FALSE AND \
             ($2::BIGINT IS NULL OR m.created_at < $2 OR \
              (m.created_at = $2 AND m.id < $3)) \
             ORDER BY m.created_at DESC, m.id DESC LIMIT $4",
        )
        .bind(user_sub)
        .bind(cursor_at)
        .bind(cursor_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::message_hit_from_row).collect()
    }
}

/// Escape LIKE metacharacters (`\`, `%`, `_`) in a search needle so they match literally under an
/// `ESCAPE '\'` clause.
fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[async_trait]
impl Store for PgStore {
    async fn ensure_room(&self, room: &Room) -> Result<(), StoreError> {
        self.ensure_room_async(room)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn get_room(&self, id: &str) -> Result<Option<Room>, StoreError> {
        self.get_room_async(id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_user_rooms(&self, user_sub: &str) -> Result<Vec<UserRoom>, StoreError> {
        self.list_user_rooms_async(user_sub)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_directory(&self, exclude_sub: &str) -> Result<Vec<Person>, StoreError> {
        self.list_directory_async(exclude_sub)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
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

    async fn is_member(&self, room_id: &str, user_sub: &str) -> Result<bool, StoreError> {
        self.is_member_async(room_id, user_sub)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_messages(
        &self,
        room_id: &str,
        before: Option<MessageCursor>,
        limit: i64,
    ) -> Result<Vec<Message>, StoreError> {
        self.list_messages_async(room_id, before, limit)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn create_message(&self, message: &Message) -> Result<(), StoreError> {
        self.create_message_async(message)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn get_message(&self, id: &str) -> Result<Option<Message>, StoreError> {
        self.get_message_async(id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
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

    async fn toggle_reaction(
        &self,
        message_id: &str,
        user_sub: &str,
        emoji: &str,
    ) -> Result<bool, StoreError> {
        self.toggle_reaction_async(message_id, user_sub, emoji)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_reactions(&self, message_id: &str) -> Result<Vec<ReactionCount>, StoreError> {
        self.list_reactions_async(message_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_user_reactions(
        &self,
        message_id: &str,
        user_sub: &str,
    ) -> Result<Vec<String>, StoreError> {
        self.list_user_reactions_async(message_id, user_sub)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn count_replies(&self, room_id: &str, message_id: &str) -> Result<i64, StoreError> {
        self.count_replies_async(room_id, message_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_thread_participants(
        &self,
        room_id: &str,
        parent_message_id: &str,
    ) -> Result<Vec<Person>, StoreError> {
        self.list_thread_participants_async(room_id, parent_message_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_all_rooms(&self) -> Result<Vec<Room>, StoreError> {
        self.list_all_rooms_async()
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn set_room_archived(&self, room_id: &str, archived: bool) -> Result<(), StoreError> {
        self.set_room_archived_async(room_id, archived)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_room_members(&self, room_id: &str) -> Result<Vec<Member>, StoreError> {
        self.list_room_members_async(room_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
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

    async fn set_room_topic(&self, room_id: &str, topic: &str) -> Result<(), StoreError> {
        self.set_room_topic_async(room_id, topic)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn pin_message(
        &self,
        room_id: &str,
        message_id: &str,
        pinned_by: &str,
        pinned_at: i64,
    ) -> Result<(), StoreError> {
        self.pin_message_async(room_id, message_id, pinned_by, pinned_at)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn unpin_message(&self, room_id: &str, message_id: &str) -> Result<(), StoreError> {
        self.unpin_message_async(room_id, message_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_pinned(&self, room_id: &str) -> Result<Vec<Message>, StoreError> {
        self.list_pinned_async(room_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn is_pinned(&self, room_id: &str, message_id: &str) -> Result<bool, StoreError> {
        self.is_pinned_async(room_id, message_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn search_user_messages(
        &self,
        user_sub: &str,
        query_lower: &str,
        before: Option<MessageCursor>,
        limit: i64,
    ) -> Result<Vec<MessageHit>, StoreError> {
        self.search_user_messages_async(user_sub, query_lower, before, limit)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn add_mention(
        &self,
        message_id: &str,
        room_id: &str,
        mentioned_sub: &str,
        created_at: i64,
    ) -> Result<(), StoreError> {
        self.add_mention_async(message_id, room_id, mentioned_sub, created_at)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_user_mentions(
        &self,
        user_sub: &str,
        before: Option<MessageCursor>,
        limit: i64,
    ) -> Result<Vec<MessageHit>, StoreError> {
        self.list_user_mentions_async(user_sub, before, limit)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn join_active_room(
        &self,
        room_id: &str,
        user_sub: &str,
        user_email: &str,
        joined_at: i64,
    ) -> Result<Room, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        let room = Self::lock_room(&mut tx, room_id, true).await?;
        if room.kind == "dm" {
            return Err(StoreMutationError::DirectMessageJoin);
        }
        sqlx::query(
            "INSERT INTO memberships (room_id, user_sub, user_email, joined_at, last_read_at) \
             VALUES ($1, $2, $3, $4, 0) ON CONFLICT (room_id, user_sub) DO NOTHING",
        )
        .bind(room_id)
        .bind(user_sub)
        .bind(user_email)
        .bind(joined_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(room)
    }

    async fn open_dm_authorized(
        &self,
        proposed: &Room,
        user_sub: &str,
        user_email: &str,
        peer_sub: &str,
        peer_email: &str,
        joined_at: i64,
    ) -> Result<Room, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO rooms \
             (id, name, kind, created_by, created_at, archived, topic, incarnation) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (id) DO NOTHING",
        )
        .bind(&proposed.id)
        .bind(&proposed.name)
        .bind(&proposed.kind)
        .bind(&proposed.created_by)
        .bind(proposed.created_at)
        .bind(proposed.archived)
        .bind(&proposed.topic)
        .bind(new_opaque_id())
        .execute(&mut *tx)
        .await?;

        let row = sqlx::query(
            "SELECT id, name, kind, created_by, created_at, archived, topic \
             FROM rooms WHERE id = $1 FOR UPDATE",
        )
        .bind(&proposed.id)
        .fetch_one(&mut *tx)
        .await?;
        let room = Self::room_from_row(&row)?;
        if room.kind != "dm" {
            return Err(StoreMutationError::ResourceUnavailable);
        }
        if room.archived {
            return Err(StoreMutationError::RoomArchived);
        }

        let existing = sqlx::query(
            "SELECT user_sub, banned FROM memberships \
             WHERE room_id = $1 AND (user_sub = $2 OR user_sub = $3) FOR UPDATE",
        )
        .bind(&room.id)
        .bind(user_sub)
        .bind(peer_sub)
        .fetch_all(&mut *tx)
        .await?;
        for row in &existing {
            if row.try_get::<bool, _>("banned")? {
                return Err(StoreMutationError::Forbidden);
            }
        }

        for (participant_sub, participant_email) in [(user_sub, user_email), (peer_sub, peer_email)]
        {
            sqlx::query(
                "INSERT INTO memberships \
                 (room_id, user_sub, user_email, joined_at, last_read_at) \
                 VALUES ($1, $2, $3, $4, 0) \
                 ON CONFLICT (room_id, user_sub) DO NOTHING",
            )
            .bind(&room.id)
            .bind(participant_sub)
            .bind(participant_email)
            .bind(joined_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(room)
    }

    async fn create_message_authorized(
        &self,
        message: &Message,
        mention_tokens: &[String],
    ) -> Result<Vec<Member>, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        Self::lock_member_room(&mut tx, &message.room_id, &message.sender_sub, true).await?;
        if let Some(parent_id) = message.reply_to_id.as_deref() {
            let parent =
                sqlx::query("SELECT id FROM messages WHERE id = $1 AND room_id = $2 FOR SHARE")
                    .bind(parent_id)
                    .bind(&message.room_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if parent.is_none() {
                return Err(StoreMutationError::ReplyUnavailable);
            }
        }

        let mentioned = if mention_tokens.is_empty() {
            Vec::new()
        } else {
            let rows = sqlx::query(
                "SELECT user_sub, user_email, banned, joined_at FROM memberships \
                 WHERE room_id = $1 AND banned = FALSE FOR SHARE",
            )
            .bind(&message.room_id)
            .fetch_all(&mut *tx)
            .await?;
            rows.iter()
                .map(Self::member_from_row)
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|member| {
                    mention_identity_matches(
                        &member.user_sub,
                        &member.user_email,
                        member.banned,
                        &message.sender_sub,
                        mention_tokens,
                    )
                })
                .collect()
        };

        sqlx::query(
            "INSERT INTO messages \
             (id, room_id, sender_sub, sender_email, body, created_at, edited_at, deleted, \
              reply_to_id) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&message.id)
        .bind(&message.room_id)
        .bind(&message.sender_sub)
        .bind(&message.sender_email)
        .bind(&message.body)
        .bind(message.created_at)
        .bind(message.edited_at)
        .bind(message.deleted)
        .bind(&message.reply_to_id)
        .execute(&mut *tx)
        .await?;
        for member in &mentioned {
            sqlx::query(
                "INSERT INTO mentions (message_id, room_id, mentioned_sub, created_at) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (message_id, mentioned_sub) DO NOTHING",
            )
            .bind(&message.id)
            .bind(&message.room_id)
            .bind(&member.user_sub)
            .bind(message.created_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(mentioned)
    }

    async fn edit_message_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
        body: &str,
        edited_at: i64,
    ) -> Result<Message, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        Self::lock_member_room(&mut tx, room_id, user_sub, true).await?;
        let mut message = Self::lock_message(&mut tx, room_id, message_id).await?;
        if message.sender_sub != user_sub {
            return Err(StoreMutationError::Forbidden);
        }
        if message.deleted {
            return Err(StoreMutationError::MessageDeleted);
        }
        sqlx::query("UPDATE messages SET body = $1, edited_at = $2 WHERE id = $3")
            .bind(body)
            .bind(edited_at)
            .bind(message_id)
            .execute(&mut *tx)
            .await?;
        message.body = body.to_string();
        message.edited_at = edited_at;
        tx.commit().await?;
        Ok(message)
    }

    async fn delete_message_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
    ) -> Result<Message, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        Self::lock_member_room(&mut tx, room_id, user_sub, true).await?;
        let mut message = Self::lock_message(&mut tx, room_id, message_id).await?;
        if message.sender_sub != user_sub {
            return Err(StoreMutationError::Forbidden);
        }
        sqlx::query("UPDATE messages SET deleted = TRUE, body = '' WHERE id = $1")
            .bind(message_id)
            .execute(&mut *tx)
            .await?;
        message.deleted = true;
        message.body.clear();
        tx.commit().await?;
        Ok(message)
    }

    async fn toggle_reaction_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
        emoji: &str,
    ) -> Result<ReactionMutation, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        Self::lock_member_room(&mut tx, room_id, user_sub, true).await?;
        Self::lock_message(&mut tx, room_id, message_id).await?;
        let deleted = sqlx::query(
            "DELETE FROM message_reactions \
             WHERE message_id = $1 AND user_sub = $2 AND emoji = $3",
        )
        .bind(message_id)
        .bind(user_sub)
        .bind(emoji)
        .execute(&mut *tx)
        .await?;
        let added = if deleted.rows_affected() > 0 {
            false
        } else {
            sqlx::query(
                "INSERT INTO message_reactions (message_id, user_sub, emoji, created_at) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (message_id, user_sub, emoji) DO NOTHING",
            )
            .bind(message_id)
            .bind(user_sub)
            .bind(emoji)
            .bind(crate::now_secs())
            .execute(&mut *tx)
            .await?;
            true
        };
        let reaction_rows = sqlx::query(
            "SELECT emoji, COUNT(*) AS cnt FROM message_reactions \
             WHERE message_id = $1 GROUP BY emoji \
             ORDER BY COUNT(*) DESC, emoji ASC",
        )
        .bind(message_id)
        .fetch_all(&mut *tx)
        .await?;
        let reactions = reaction_rows
            .iter()
            .map(|row| {
                Ok(ReactionCount {
                    emoji: row.try_get("emoji")?,
                    count: row.try_get("cnt")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;
        let mine_rows = sqlx::query(
            "SELECT emoji FROM message_reactions \
             WHERE message_id = $1 AND user_sub = $2 ORDER BY emoji ASC",
        )
        .bind(message_id)
        .bind(user_sub)
        .fetch_all(&mut *tx)
        .await?;
        let mine = mine_rows
            .iter()
            .map(|row| row.try_get("emoji"))
            .collect::<Result<Vec<String>, sqlx::Error>>()?;
        tx.commit().await?;
        Ok(ReactionMutation {
            added,
            reactions,
            mine,
        })
    }

    async fn update_last_read_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
    ) -> Result<Message, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        Self::lock_member_room(&mut tx, room_id, user_sub, false).await?;
        let message = Self::lock_message(&mut tx, room_id, message_id).await?;
        sqlx::query(
            "UPDATE memberships \
             SET last_read_at = $1, last_read_message_id = $2 \
             WHERE room_id = $3 AND user_sub = $4 AND banned = FALSE \
               AND (last_read_at < $1 OR \
                    (last_read_at = $1 AND last_read_message_id < $2))",
        )
        .bind(message.created_at)
        .bind(&message.id)
        .bind(room_id)
        .bind(user_sub)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(message)
    }

    async fn set_room_topic_authorized(
        &self,
        room_id: &str,
        topic: &str,
    ) -> Result<Room, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        let mut room = Self::lock_room(&mut tx, room_id, true).await?;
        sqlx::query("UPDATE rooms SET topic = $1 WHERE id = $2")
            .bind(topic)
            .bind(room_id)
            .execute(&mut *tx)
            .await?;
        room.topic = topic.to_string();
        tx.commit().await?;
        Ok(room)
    }

    async fn pin_message_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        pinned_by: &str,
        pinned_at: i64,
    ) -> Result<Vec<Message>, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        Self::lock_room(&mut tx, room_id, true).await?;
        Self::lock_message(&mut tx, room_id, message_id).await?;
        sqlx::query(
            "INSERT INTO pinned_messages (room_id, message_id, pinned_by, pinned_at) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (room_id, message_id) DO NOTHING",
        )
        .bind(room_id)
        .bind(message_id)
        .bind(pinned_by)
        .bind(pinned_at)
        .execute(&mut *tx)
        .await?;
        let pinned = Self::pinned_projection_tx(&mut tx, room_id).await?;
        tx.commit().await?;
        Ok(pinned)
    }

    async fn unpin_message_authorized(
        &self,
        room_id: &str,
        message_id: &str,
    ) -> Result<Vec<Message>, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        Self::lock_room(&mut tx, room_id, true).await?;
        sqlx::query("DELETE FROM pinned_messages WHERE room_id = $1 AND message_id = $2")
            .bind(room_id)
            .bind(message_id)
            .execute(&mut *tx)
            .await?;
        let pinned = Self::pinned_projection_tx(&mut tx, room_id).await?;
        tx.commit().await?;
        Ok(pinned)
    }

    async fn authorize_room_read(
        &self,
        room_id: &str,
        user_sub: &str,
        require_active: bool,
    ) -> Result<Room, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        let room = Self::lock_read_authority(&mut tx, room_id, user_sub, require_active).await?;
        tx.commit().await?;
        Ok(room)
    }

    async fn list_messages_authorized(
        &self,
        room_id: &str,
        user_sub: &str,
        before: Option<MessageCursor>,
        limit: i64,
    ) -> Result<Vec<Message>, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        Self::lock_read_authority(&mut tx, room_id, user_sub, false).await?;
        let cursor_at = before.as_ref().map(|cursor| cursor.created_at);
        let cursor_id = before.as_ref().map(|cursor| cursor.message_id.as_str());
        let rows = sqlx::query(
            "SELECT id, room_id, sender_sub, sender_email, body, created_at, edited_at, deleted, \
             reply_to_id FROM messages WHERE room_id = $1 AND \
             ($2::BIGINT IS NULL OR created_at < $2 OR (created_at = $2 AND id < $3)) \
             ORDER BY created_at DESC, id DESC LIMIT $4",
        )
        .bind(room_id)
        .bind(cursor_at)
        .bind(cursor_id)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        let messages = rows
            .iter()
            .map(Self::message_from_row)
            .collect::<Result<Vec<_>, sqlx::Error>>()?;
        tx.commit().await?;
        Ok(messages)
    }

    async fn get_message_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
    ) -> Result<Option<Message>, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        Self::lock_read_authority(&mut tx, room_id, user_sub, false).await?;
        let row = sqlx::query(
            "SELECT id, room_id, sender_sub, sender_email, body, created_at, edited_at, deleted, \
             reply_to_id FROM messages WHERE room_id = $1 AND id = $2",
        )
        .bind(room_id)
        .bind(message_id)
        .fetch_optional(&mut *tx)
        .await?;
        let message = row.as_ref().map(Self::message_from_row).transpose()?;
        tx.commit().await?;
        Ok(message)
    }

    async fn reaction_projection_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
    ) -> Result<ReactionProjection, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        Self::lock_read_authority(&mut tx, room_id, user_sub, false).await?;
        Self::lock_message(&mut tx, room_id, message_id).await?;
        let reaction_rows = sqlx::query(
            "SELECT emoji, COUNT(*) AS cnt FROM message_reactions \
             WHERE message_id = $1 GROUP BY emoji \
             ORDER BY COUNT(*) DESC, emoji ASC",
        )
        .bind(message_id)
        .fetch_all(&mut *tx)
        .await?;
        let reactions = reaction_rows
            .iter()
            .map(|row| {
                Ok(ReactionCount {
                    emoji: row.try_get("emoji")?,
                    count: row.try_get("cnt")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;
        let mine_rows = sqlx::query(
            "SELECT emoji FROM message_reactions \
             WHERE message_id = $1 AND user_sub = $2 ORDER BY emoji ASC",
        )
        .bind(message_id)
        .bind(user_sub)
        .fetch_all(&mut *tx)
        .await?;
        let mine = mine_rows
            .iter()
            .map(|row| row.try_get("emoji"))
            .collect::<Result<Vec<String>, sqlx::Error>>()?;
        tx.commit().await?;
        Ok(ReactionProjection { reactions, mine })
    }

    async fn list_pinned_authorized(
        &self,
        room_id: &str,
        user_sub: &str,
    ) -> Result<Vec<Message>, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        Self::lock_read_authority(&mut tx, room_id, user_sub, false).await?;
        let pinned = Self::pinned_projection_tx(&mut tx, room_id).await?;
        tx.commit().await?;
        Ok(pinned)
    }

    async fn count_replies_authorized(
        &self,
        room_id: &str,
        message_id: &str,
        user_sub: &str,
    ) -> Result<i64, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        Self::lock_read_authority(&mut tx, room_id, user_sub, false).await?;
        Self::lock_message(&mut tx, room_id, message_id).await?;
        let row = sqlx::query(
            "SELECT COUNT(*) AS cnt FROM messages \
             WHERE room_id = $1 AND reply_to_id = $2",
        )
        .bind(room_id)
        .bind(message_id)
        .fetch_one(&mut *tx)
        .await?;
        let count = row.try_get("cnt")?;
        tx.commit().await?;
        Ok(count)
    }

    async fn issue_room_delete_token(
        &self,
        token_digest: &str,
        room_id: &str,
        actor_sub: &str,
        csrf_digest: &str,
        issued_at: i64,
        expires_at: i64,
    ) -> Result<Room, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM room_delete_tokens WHERE expires_at < $1")
            .bind(issued_at)
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query(
            "SELECT id, name, kind, created_by, created_at, archived, topic, incarnation \
             FROM rooms WHERE id = $1 FOR UPDATE",
        )
        .bind(room_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreMutationError::ResourceUnavailable)?;
        let room = Self::room_from_row(&row)?;
        let room_incarnation: String = row.try_get("incarnation")?;
        sqlx::query(
            "INSERT INTO room_delete_tokens \
             (token_digest, room_id, room_incarnation, actor_sub, csrf_digest, purpose, \
              issued_at, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(token_digest)
        .bind(room_id)
        .bind(room_incarnation)
        .bind(actor_sub)
        .bind(csrf_digest)
        .bind(ROOM_DELETE_PURPOSE)
        .bind(issued_at)
        .bind(expires_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(room)
    }

    async fn delete_room_with_token(
        &self,
        room_id: &str,
        token_digest: &str,
        actor_sub: &str,
        csrf_digest: &str,
        now: i64,
    ) -> Result<RoomDeleteAuditConsequence, StoreMutationError> {
        let mut tx = self.pool.begin().await?;
        // First reject an invalid grant without taking the room lock. The row is re-read FOR
        // UPDATE after the global room lock, so this optimistic check grants no authority.
        let token = sqlx::query(
            "SELECT room_incarnation, purpose, issued_at, expires_at FROM room_delete_tokens \
             WHERE token_digest = $1 AND room_id = $2 AND actor_sub = $3 \
               AND csrf_digest = $4",
        )
        .bind(token_digest)
        .bind(room_id)
        .bind(actor_sub)
        .bind(csrf_digest)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(token) = token else {
            return Err(StoreMutationError::ConsequenceTokenInvalid);
        };
        let bound_incarnation: String = token.try_get("room_incarnation")?;
        let purpose: String = token.try_get("purpose")?;
        let issued_at: i64 = token.try_get("issued_at")?;
        let expires_at: i64 = token.try_get("expires_at")?;
        if purpose != ROOM_DELETE_PURPOSE || issued_at > now || expires_at < now {
            return Err(StoreMutationError::ConsequenceTokenInvalid);
        }

        // Global delete order is room, then token. Concurrent forms for the same room therefore
        // cannot deadlock when the winner invalidates every other stale grant for that room.
        let room = sqlx::query("SELECT incarnation FROM rooms WHERE id = $1 FOR UPDATE")
            .bind(room_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreMutationError::ConsequenceTokenInvalid)?;
        let current_incarnation: String = room.try_get("incarnation")?;
        if current_incarnation != bound_incarnation {
            return Err(StoreMutationError::ConsequenceTokenInvalid);
        }
        let token = sqlx::query(
            "SELECT room_incarnation, purpose, issued_at, expires_at FROM room_delete_tokens \
             WHERE token_digest = $1 AND room_id = $2 AND actor_sub = $3 \
               AND csrf_digest = $4 FOR UPDATE",
        )
        .bind(token_digest)
        .bind(room_id)
        .bind(actor_sub)
        .bind(csrf_digest)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(token) = token else {
            return Err(StoreMutationError::ConsequenceTokenInvalid);
        };
        let locked_incarnation: String = token.try_get("room_incarnation")?;
        let purpose: String = token.try_get("purpose")?;
        let issued_at: i64 = token.try_get("issued_at")?;
        let expires_at: i64 = token.try_get("expires_at")?;
        if locked_incarnation != current_incarnation
            || purpose != ROOM_DELETE_PURPOSE
            || issued_at > now
            || expires_at < now
        {
            return Err(StoreMutationError::ConsequenceTokenInvalid);
        }

        let consequence = RoomDeleteAuditConsequence {
            id: new_opaque_id(),
            room_id: room_id.to_string(),
            actor_sub: actor_sub.to_string(),
            occurred_at: now,
        };
        sqlx::query(
            "INSERT INTO room_delete_audit_outbox \
             (consequence_id, room_id, room_incarnation, actor_sub, occurred_at, delivered_at) \
             VALUES ($1, $2, $3, $4, $5, 0)",
        )
        .bind(&consequence.id)
        .bind(room_id)
        .bind(&current_incarnation)
        .bind(actor_sub)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM message_reactions WHERE message_id IN \
             (SELECT id FROM messages WHERE room_id = $1)",
        )
        .bind(room_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM pinned_messages WHERE room_id = $1")
            .bind(room_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mentions WHERE room_id = $1")
            .bind(room_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM messages WHERE room_id = $1")
            .bind(room_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM memberships WHERE room_id = $1")
            .bind(room_id)
            .execute(&mut *tx)
            .await?;
        // Consuming all grants for the room prevents a separately rendered stale form from
        // carrying authority across the irreversible consequence.
        sqlx::query("DELETE FROM room_delete_tokens WHERE room_id = $1")
            .bind(room_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(consequence)
    }

    async fn pending_room_delete_audits(
        &self,
        limit: i64,
    ) -> Result<Vec<RoomDeleteAuditConsequence>, StoreError> {
        let rows = sqlx::query(
            "SELECT consequence_id, room_id, actor_sub, occurred_at \
             FROM room_delete_audit_outbox WHERE delivered_at = 0 \
             ORDER BY occurred_at ASC, consequence_id ASC LIMIT $1",
        )
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await
        .map_err(|error| StoreError::Backend(error.to_string()))?;
        rows.iter()
            .map(|row| {
                Ok(RoomDeleteAuditConsequence {
                    id: row.try_get("consequence_id")?,
                    room_id: row.try_get("room_id")?,
                    actor_sub: row.try_get("actor_sub")?,
                    occurred_at: row.try_get("occurred_at")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(|error| StoreError::Backend(error.to_string()))
    }

    async fn mark_room_delete_audit_delivered(
        &self,
        consequence_id: &str,
        delivered_at: i64,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE room_delete_audit_outbox SET delivered_at = $1 \
             WHERE consequence_id = $2 AND delivered_at = 0",
        )
        .bind(delivered_at)
        .bind(consequence_id)
        .execute(&self.pool)
        .await
        .map_err(|error| StoreError::Backend(error.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(id: &str) -> Room {
        Room {
            id: id.to_string(),
            name: "#test".to_string(),
            kind: "room".to_string(),
            created_by: "u_alice".to_string(),
            created_at: 1,
            archived: false,
            topic: String::new(),
        }
    }

    fn message(id: &str, sender: &str, email: &str, at: i64, reply_to: Option<&str>) -> Message {
        Message {
            id: id.to_string(),
            room_id: "room_1".to_string(),
            sender_sub: sender.to_string(),
            sender_email: email.to_string(),
            body: "body".to_string(),
            created_at: at,
            edited_at: 0,
            deleted: false,
            reply_to_id: reply_to.map(str::to_string),
        }
    }

    #[test]
    fn message_cursor_is_versioned_canonical_and_round_trips() {
        let cursor = MessageCursor {
            created_at: 42,
            message_id: "msg_α".to_string(),
        };
        let encoded = cursor.encode();
        assert_eq!(MessageCursor::decode(&encoded), Some(cursor));
        assert!(MessageCursor::decode("42").is_none());
        assert!(MessageCursor::decode("v1.42.4D").is_none());
        assert!(MessageCursor::decode("v2.42.4d").is_none());
        assert!(MessageCursor::decode("v1.-1.4d").is_none());
    }

    #[tokio::test]
    async fn tuple_cursor_paginates_same_second_without_omission() {
        let store = InMemoryStore::new();
        store.ensure_room(&room("room_1")).await.unwrap();
        store
            .ensure_membership("room_1", "u_bob", "bob@hf", 1)
            .await
            .unwrap();
        for index in 0..51 {
            store
                .create_message(&message(
                    &format!("msg_{index:03}"),
                    "u_alice",
                    "alice@hf",
                    100,
                    None,
                ))
                .await
                .unwrap();
        }

        let first = store
            .list_messages_authorized("room_1", "u_bob", None, 50)
            .await
            .unwrap();
        assert_eq!(first.len(), 50);
        let cursor = MessageCursor::from_message(first.last().unwrap());
        let second = store
            .list_messages_authorized("room_1", "u_bob", Some(cursor), 50)
            .await
            .unwrap();
        assert_eq!(
            second
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["msg_000"]
        );
        let mut ids = first
            .iter()
            .chain(second.iter())
            .map(|message| message.id.clone())
            .collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 51);
    }

    #[tokio::test]
    async fn tuple_read_marker_preserves_same_second_unread() {
        let store = InMemoryStore::new();
        store.ensure_room(&room("room_1")).await.unwrap();
        store
            .ensure_membership("room_1", "u_bob", "bob@hf", 1)
            .await
            .unwrap();
        store
            .create_message(&message("msg_a", "u_alice", "alice@hf", 100, None))
            .await
            .unwrap();
        store
            .create_message(&message("msg_b", "u_alice", "alice@hf", 100, None))
            .await
            .unwrap();

        store
            .update_last_read_authorized("room_1", "msg_a", "u_bob")
            .await
            .unwrap();
        let rooms = store.list_user_rooms("u_bob").await.unwrap();
        assert_eq!(rooms[0].last_read_at, 100);
        assert_eq!(rooms[0].last_read_message_id, "msg_a");
        assert_eq!(
            rooms[0].unread, 1,
            "msg_b remains unread in the same second"
        );

        // A lower tuple cannot move the marker backward.
        store
            .update_last_read_authorized("room_1", "msg_a", "u_bob")
            .await
            .unwrap();
        store
            .update_last_read_authorized("room_1", "msg_b", "u_bob")
            .await
            .unwrap();
        let rooms = store.list_user_rooms("u_bob").await.unwrap();
        assert_eq!(rooms[0].last_read_message_id, "msg_b");
        assert_eq!(rooms[0].unread, 0);
    }

    #[tokio::test]
    async fn protected_projection_returns_typed_denial_after_ban() {
        let store = InMemoryStore::new();
        store.ensure_room(&room("room_1")).await.unwrap();
        store
            .ensure_membership("room_1", "u_bob", "bob@hf", 1)
            .await
            .unwrap();
        store
            .create_message(&message("msg_secret", "u_alice", "alice@hf", 100, None))
            .await
            .unwrap();
        store.ban_member("room_1", "u_bob").await.unwrap();
        assert!(matches!(
            store
                .list_messages_authorized("room_1", "u_bob", None, 50)
                .await,
            Err(StoreMutationError::MemberBanned)
        ));
    }

    #[tokio::test]
    async fn delete_consequence_token_is_bound_and_one_time() {
        let store = InMemoryStore::new();
        store.ensure_room(&room("room_1")).await.unwrap();
        store
            .issue_room_delete_token("digest", "room_1", "u_admin", "csrf", 0, 100)
            .await
            .unwrap();

        assert!(matches!(
            store
                .delete_room_with_token("room_1", "digest", "u_other", "csrf", 50)
                .await,
            Err(StoreMutationError::ConsequenceTokenInvalid)
        ));
        assert!(store.get_room("room_1").await.unwrap().is_some());

        store
            .delete_room_with_token("room_1", "digest", "u_admin", "csrf", 50)
            .await
            .unwrap();
        assert!(store.get_room("room_1").await.unwrap().is_none());
        let pending = store.pending_room_delete_audits(10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].room_id, "room_1");
        store
            .mark_room_delete_audit_delivered(&pending[0].id, 51)
            .await
            .unwrap();
        assert!(store
            .pending_room_delete_audits(10)
            .await
            .unwrap()
            .is_empty());
        assert!(matches!(
            store
                .delete_room_with_token("room_1", "digest", "u_admin", "csrf", 50)
                .await,
            Err(StoreMutationError::ConsequenceTokenInvalid)
        ));
    }

    #[tokio::test]
    async fn stale_delete_token_cannot_cross_room_incarnation() {
        let store = InMemoryStore::new();
        store.ensure_room(&room("room_1")).await.unwrap();
        store
            .issue_room_delete_token("stale", "room_1", "u_admin", "csrf", 0, 100)
            .await
            .unwrap();

        // Simulate an out-of-band/legacy delete that failed to clean the new token table, then a
        // deterministic-id room recreation. The old grant must still fail on incarnation.
        store
            .rooms
            .lock()
            .expect("rooms lock poisoned")
            .retain(|room| room.id != "room_1");
        store
            .room_incarnations
            .lock()
            .expect("room incarnations lock poisoned")
            .retain(|row| row.room_id != "room_1");
        let mut recreated = room("room_1");
        recreated.name = "#recreated".to_string();
        store.ensure_room(&recreated).await.unwrap();

        assert!(matches!(
            store
                .delete_room_with_token("room_1", "stale", "u_admin", "csrf", 50)
                .await,
            Err(StoreMutationError::ConsequenceTokenInvalid)
        ));
        assert_eq!(
            store.get_room("room_1").await.unwrap().unwrap().name,
            "#recreated"
        );
        assert!(store
            .pending_room_delete_audits(10)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn thread_participants_are_current_non_banned_members() {
        let store = InMemoryStore::new();
        store.ensure_room(&room("room_1")).await.unwrap();
        store
            .ensure_membership("room_1", "u_alice", "alice@hf", 1)
            .await
            .unwrap();
        store
            .ensure_membership("room_1", "u_bob", "bob@hf", 2)
            .await
            .unwrap();
        store
            .ensure_membership("room_1", "u_carol", "carol@hf", 3)
            .await
            .unwrap();

        store
            .create_message(&message("msg_parent", "u_alice", "alice@hf", 10, None))
            .await
            .unwrap();
        store
            .create_message(&message(
                "msg_reply_1",
                "u_bob",
                "bob@hf",
                11,
                Some("msg_parent"),
            ))
            .await
            .unwrap();
        store
            .create_message(&message(
                "msg_reply_2",
                "u_carol",
                "carol@hf",
                12,
                Some("msg_parent"),
            ))
            .await
            .unwrap();
        store.ban_member("room_1", "u_carol").await.unwrap();

        let participants = store
            .list_thread_participants("room_1", "msg_parent")
            .await
            .unwrap();
        let subs: Vec<String> = participants.into_iter().map(|p| p.user_sub).collect();
        assert_eq!(subs, vec!["u_alice".to_string(), "u_bob".to_string()]);
    }

    #[tokio::test]
    async fn mentions_require_current_non_banned_membership() {
        let store = InMemoryStore::new();
        store.ensure_room(&room("room_1")).await.unwrap();
        store
            .ensure_membership("room_1", "u_alice", "alice@hf", 1)
            .await
            .unwrap();
        store
            .ensure_membership("room_1", "u_bob", "bob@hf", 2)
            .await
            .unwrap();
        let mut sent = message("msg_mention", "u_alice", "alice@hf", 10, None);
        sent.body = "hello @bob".to_string();
        let mentioned = store
            .create_message_authorized(&sent, &["bob".to_string()])
            .await
            .unwrap();
        assert_eq!(mentioned.len(), 1);
        assert_eq!(
            store
                .list_user_mentions("u_bob", None, 50)
                .await
                .unwrap()
                .len(),
            1
        );

        store.ban_member("room_1", "u_bob").await.unwrap();
        assert!(store
            .list_user_mentions("u_bob", None, 50)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn mentions_require_message_and_projection_room_to_match() {
        let store = InMemoryStore::new();
        store.ensure_room(&room("room_visible")).await.unwrap();
        store.ensure_room(&room("room_forged")).await.unwrap();
        store
            .ensure_membership("room_visible", "u_bob", "bob@hf", 1)
            .await
            .unwrap();
        store
            .create_message(&message("msg_projection", "u_alice", "alice@hf", 10, None))
            .await
            .unwrap();
        {
            let mut messages = store.messages.lock().expect("messages lock poisoned");
            messages.last_mut().expect("inserted message").room_id = "room_visible".to_string();
        }
        store
            .add_mention("msg_projection", "room_forged", "u_bob", 10)
            .await
            .unwrap();

        assert!(
            store
                .list_user_mentions("u_bob", None, 50)
                .await
                .unwrap()
                .is_empty(),
            "a mention projection cannot resolve a same-id message from another room"
        );
    }

    #[tokio::test]
    async fn authorized_memory_mutations_fail_closed_after_archive_or_ban() {
        let store = InMemoryStore::new();
        store.ensure_room(&room("room_1")).await.unwrap();
        store
            .ensure_membership("room_1", "u_alice", "alice@hf", 1)
            .await
            .unwrap();
        store.ban_member("room_1", "u_alice").await.unwrap();
        let denied = store
            .create_message_authorized(&message("msg_denied", "u_alice", "alice@hf", 10, None), &[])
            .await;
        assert!(matches!(
            denied,
            Err(StoreMutationError::ResourceUnavailable)
        ));
        assert!(store.get_message("msg_denied").await.unwrap().is_none());

        store.remove_member("room_1", "u_alice").await.unwrap();
        store
            .ensure_membership("room_1", "u_alice", "alice@hf", 2)
            .await
            .unwrap();
        store.set_room_archived("room_1", true).await.unwrap();
        let denied = store
            .create_message_authorized(
                &message("msg_archived", "u_alice", "alice@hf", 11, None),
                &[],
            )
            .await;
        assert!(matches!(denied, Err(StoreMutationError::RoomArchived)));
        assert!(store.get_message("msg_archived").await.unwrap().is_none());

        store.remove_member("room_1", "u_alice").await.unwrap();
        let denied = store
            .create_message_authorized(
                &message("msg_archived_nonmember", "u_alice", "alice@hf", 12, None),
                &[],
            )
            .await;
        assert!(matches!(
            denied,
            Err(StoreMutationError::ResourceUnavailable)
        ));
        assert!(store
            .get_message("msg_archived_nonmember")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn opening_dm_is_atomic_with_the_archive_fence() {
        let store = InMemoryStore::new();
        let mut dm = room("dm_u_alice__u_bob");
        dm.kind = "dm".to_string();
        store
            .open_dm_authorized(&dm, "u_alice", "alice@hf", "u_bob", "bob@hf", 1)
            .await
            .unwrap();
        assert!(store.is_member(&dm.id, "u_alice").await.unwrap());
        assert!(store.is_member(&dm.id, "u_bob").await.unwrap());

        store.remove_member(&dm.id, "u_bob").await.unwrap();
        store.set_room_archived(&dm.id, true).await.unwrap();
        let denied = store
            .open_dm_authorized(&dm, "u_alice", "alice@hf", "u_bob", "bob@hf", 2)
            .await;
        assert!(matches!(denied, Err(StoreMutationError::RoomArchived)));
        assert!(
            !store.is_member(&dm.id, "u_bob").await.unwrap(),
            "an archived DM must not regain a removed member"
        );
    }

    #[tokio::test]
    async fn opening_dm_preflights_every_participant_before_any_memory_write() {
        let store = InMemoryStore::new();
        let mut dm = room("dm_u_alice__u_bob");
        dm.kind = "dm".to_string();
        store
            .open_dm_authorized(&dm, "u_alice", "alice@hf", "u_bob", "bob@hf", 1)
            .await
            .unwrap();
        store.remove_member(&dm.id, "u_alice").await.unwrap();
        store.ban_member(&dm.id, "u_bob").await.unwrap();

        let denied = store
            .open_dm_authorized(&dm, "u_alice", "alice@hf", "u_bob", "bob@hf", 2)
            .await;
        assert!(matches!(denied, Err(StoreMutationError::Forbidden)));
        assert!(
            !store.is_member(&dm.id, "u_alice").await.unwrap(),
            "a later banned participant must not leave an earlier membership behind"
        );

        let invalid = room("dm_invalid_kind");
        let denied = store
            .open_dm_authorized(&invalid, "u_alice", "alice@hf", "u_carol", "carol@hf", 3)
            .await;
        assert!(matches!(
            denied,
            Err(StoreMutationError::ResourceUnavailable)
        ));
        assert!(
            store.get_room(&invalid.id).await.unwrap().is_none(),
            "an invalid proposed room must not be inserted before validation"
        );
    }
}
