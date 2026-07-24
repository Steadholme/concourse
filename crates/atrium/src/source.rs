//! Federated activity sources + the render-ready inbox model.
//!
//! A [`Source`] is one upstream activity service Atrium aggregates. Atrium owns NONE of this data:
//! it queries each source database **READ-ONLY** (separate sqlx pools) and never writes anywhere.
//! The seam mirrors the cortex federation: the engine depends only on the async trait, so an
//! in-memory fake ([`InMemorySource`], used by the unit/handler tests) and the live Pg sources are
//! interchangeable behind `Arc<dyn Source>`.
//!
//! RESILIENCE IS THE CONTRACT (copied from Portal's concurrent-tile fetch): a section whose DSN is
//! UNSET is simply never registered and renders as an empty column; a registered-but-unreachable
//! source FAILS its query, which the engine catches and renders as "unavailable" — the page still
//! renders the other two columns and NEVER errors or hangs. Pools are built with
//! [`PgPoolOptions::connect_lazy`], so a down source DB never blocks startup; the failure surfaces
//! (and is contained) only when a query runs.
//!
//! PORTABLE SQL ONLY: standard `SELECT` / `JOIN` / correlated subqueries / `COUNT(*)`, parameter
//! binding, `BIGINT`/`TEXT`/`BOOLEAN` columns — NO JSONB, arrays, vendor types, or compile-time
//! macros, so the same statements later run unchanged on FusionDB over pgwire.

use std::time::Duration;

use async_trait::async_trait;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;

/// Per-source acquire timeout — a down source DB fails fast (and is rendered "unavailable")
/// instead of hanging the page load.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(3);

/// Which activity column a source feeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionKind {
    /// Murmur — unread chat by room.
    Chat,
    /// Klaxon — unread notifications.
    Notifications,
    /// Current — fresh (unread) feed items.
    Feed,
}

impl SectionKind {
    /// The stable, lowercase slug for this column. Used as the `source` key everywhere a row is
    /// addressed by (the overlay action table, the `?source=` filter, the audit target).
    pub fn slug(&self) -> &'static str {
        match self {
            SectionKind::Chat => "chat",
            SectionKind::Notifications => "notifications",
            SectionKind::Feed => "feed",
        }
    }

    /// Parse a column slug back to its kind (for the `?source=` filter / an action POST body).
    /// `all`, an empty string, or anything unknown yields `None` (= "no source restriction").
    pub fn from_slug(s: &str) -> Option<SectionKind> {
        match s.trim() {
            "chat" => Some(SectionKind::Chat),
            "notifications" => Some(SectionKind::Notifications),
            "feed" => Some(SectionKind::Feed),
            _ => None,
        }
    }
}

/// One render-ready row in a section. The fields are deliberately generic so all three columns
/// share one renderer: `title` is the primary line (room / notification title / item title),
/// `snippet` the secondary line (latest message preview / notification body / item summary),
/// `source` an optional origin tag (feed title for feed items), `at` the event time, `link` the
/// deep-link OUT to the owning service, and `count` the per-room unread badge (chat only).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InboxRow {
    /// Stable per-row identity WITHIN its source (room id / notification id / feed item id). This
    /// is the token a mark-read / dismiss action addresses the row by, and the key the per-viewer
    /// overlay ([`crate::store`]) hides it against. Never rendered.
    pub key: String,
    pub title: String,
    pub snippet: String,
    pub source: String,
    pub at: Option<i64>,
    pub link: String,
    pub count: Option<i64>,
}

/// One activity column: a headline unread `total` (drives the summary bar) plus its rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Section {
    pub total: i64,
    pub rows: Vec<InboxRow>,
}

impl Section {
    /// An empty (but available) section — used when a source DSN is unset.
    pub fn empty() -> Self {
        Section::default()
    }
}

/// One federated activity source. `fetch` returns the viewer's rows for this section; an `Err`
/// means the source is unreachable/broken and the column should render "unavailable", NEVER as a
/// page error.
#[async_trait]
pub trait Source: Send + Sync {
    fn kind(&self) -> SectionKind;
    async fn fetch(&self, user_sub: &str, limit: i64) -> Result<Section, String>;
}

// ---------------------------------------------------------------------------
// In-memory fake source (tests): holds a canned section, can simulate an outage.
// ---------------------------------------------------------------------------

/// An in-memory [`Source`] for tests. Set `down = true` to simulate an unreachable source.
pub struct InMemorySource {
    pub kind: SectionKind,
    pub section: Section,
    pub down: bool,
}

impl InMemorySource {
    pub fn new(kind: SectionKind, section: Section) -> Self {
        Self {
            kind,
            section,
            down: false,
        }
    }

    /// A source that always errors (to exercise the "unavailable" render path).
    pub fn down(kind: SectionKind) -> Self {
        Self {
            kind,
            section: Section::empty(),
            down: true,
        }
    }
}

#[async_trait]
impl Source for InMemorySource {
    fn kind(&self) -> SectionKind {
        self.kind
    }

    async fn fetch(&self, _user_sub: &str, limit: i64) -> Result<Section, String> {
        if self.down {
            return Err("simulated source outage".to_string());
        }
        let mut s = self.section.clone();
        s.rows.truncate(limit.max(0) as usize);
        Ok(s)
    }
}

// ---------------------------------------------------------------------------
// Postgres-backed sources (read-only, portable standard SQL, runtime queries).
// ---------------------------------------------------------------------------

/// Build a lazily-connected, read-only pool to a source DSN. Never touches the network here — a
/// down source DB is discovered (and contained) only when the first query runs.
pub fn lazy_pool(dsn: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(ACQUIRE_TIMEOUT)
        .connect_lazy(dsn)
}

/// Murmur (chat) source — the viewer's joined rooms with their unread message counts.
///
/// Schema (Murmur owns it; Atrium only reads it):
/// `memberships(user_sub, room_id, last_read_at, last_read_message_id)` records who joined which
/// room and the viewer's tuple read cursor; `rooms(id, name)` is the room; and
/// `messages(id, room_id, sender_sub, body, created_at, deleted)` is the chat log. For each
/// non-banned membership we count non-deleted messages from other users whose
/// `(created_at, id)` tuple is newer than the viewer's read cursor, then pull the latest message as
/// a preview via portable correlated subqueries. Rooms with zero unread are dropped in-process so
/// the column shows only "what's new".
pub struct MurmurSource {
    pool: PgPool,
}

impl MurmurSource {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Source for MurmurSource {
    fn kind(&self) -> SectionKind {
        SectionKind::Chat
    }

    async fn fetch(&self, user_sub: &str, limit: i64) -> Result<Section, String> {
        let rows = sqlx::query(
            "SELECT r.id AS room_id, \
                    r.name AS room_name, \
                    (SELECT COUNT(*) FROM messages m \
                       WHERE m.room_id = r.id \
                         AND (m.created_at > mem.last_read_at OR \
                              (m.created_at = mem.last_read_at \
                               AND m.id > mem.last_read_message_id)) \
                         AND m.sender_sub <> mem.user_sub \
                         AND m.deleted = FALSE) AS unread, \
                    (SELECT m2.body FROM messages m2 \
                       WHERE m2.room_id = r.id \
                       ORDER BY m2.created_at DESC, m2.id DESC LIMIT 1) AS latest_body, \
                    (SELECT m3.created_at FROM messages m3 \
                       WHERE m3.room_id = r.id \
                       ORDER BY m3.created_at DESC, m3.id DESC LIMIT 1) AS latest_at \
             FROM memberships mem \
             JOIN rooms r ON r.id = mem.room_id \
             WHERE mem.user_sub = $1 AND mem.banned = FALSE \
             LIMIT $2",
        )
        .bind(user_sub)
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;

        let mut out: Vec<InboxRow> = Vec::new();
        let mut total: i64 = 0;
        for r in &rows {
            let unread: i64 = r.try_get("unread").map_err(|e| e.to_string())?;
            if unread <= 0 {
                continue; // only surface rooms with something new
            }
            total += unread;
            let room_id: String = r.try_get("room_id").map_err(|e| e.to_string())?;
            let name: String = r.try_get("room_name").map_err(|e| e.to_string())?;
            let preview: Option<String> = r.try_get("latest_body").map_err(|e| e.to_string())?;
            let at: Option<i64> = r.try_get("latest_at").map_err(|e| e.to_string())?;
            out.push(InboxRow {
                key: room_id.clone(),
                title: name,
                snippet: preview.unwrap_or_default(),
                source: String::new(),
                at,
                link: format!("https://chat.w33d.xyz/r/{room_id}"),
                count: Some(unread),
            });
        }
        // Newest-active room first; rooms with an unknown latest time sort last.
        out.sort_by(|a, b| {
            b.at.unwrap_or(i64::MIN)
                .cmp(&a.at.unwrap_or(i64::MIN))
                .then_with(|| a.title.cmp(&b.title))
        });
        Ok(Section { total, rows: out })
    }
}

/// Klaxon (notifications) source — the viewer's UNREAD notifications, newest first.
///
/// Schema (Klaxon owns it; Atrium only reads it): `notifications(id, user_sub, title, body, link,
/// created_at, read_at)`, where `read_at = 0` marks a still-unread notification. We select the
/// viewer's unread rows ordered newest-first.
pub struct KlaxonSource {
    pool: PgPool,
}

impl KlaxonSource {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Source for KlaxonSource {
    fn kind(&self) -> SectionKind {
        SectionKind::Notifications
    }

    async fn fetch(&self, user_sub: &str, limit: i64) -> Result<Section, String> {
        let rows = sqlx::query(
            "SELECT CAST(id AS TEXT) AS row_key, title, body, url AS link, created_at \
             FROM notifications \
             WHERE user_sub = $1 AND read_at = 0 \
             ORDER BY created_at DESC \
             LIMIT $2",
        )
        .bind(user_sub)
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;

        let rows: Vec<InboxRow> = rows
            .iter()
            .map(|r| {
                Ok(InboxRow {
                    key: r
                        .try_get("row_key")
                        .map_err(|e: sqlx::Error| e.to_string())?,
                    title: r.try_get("title").map_err(|e: sqlx::Error| e.to_string())?,
                    snippet: r.try_get("body").map_err(|e: sqlx::Error| e.to_string())?,
                    source: String::new(),
                    at: Some(
                        r.try_get("created_at")
                            .map_err(|e: sqlx::Error| e.to_string())?,
                    ),
                    link: r.try_get("link").map_err(|e: sqlx::Error| e.to_string())?,
                    count: None,
                })
            })
            .collect::<Result<_, String>>()?;
        let total = rows.len() as i64;
        Ok(Section { total, rows })
    }
}

/// Current (RSS reader) source — the viewer's FRESH (unread) feed items, newest first.
///
/// Schema is Current's REAL one (read from `current/src/store.rs`): `items(id, feed_id, guid,
/// title, link, summary, published_at, read)` joined to `feeds(id, owner_sub, …, title)`. We
/// select the owner's unread items with their feed title, exactly mirroring Current's own "river"
/// query (`f.owner_sub = $1 AND i.read = FALSE`, newest publish time first).
pub struct CurrentSource {
    pool: PgPool,
}

impl CurrentSource {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Source for CurrentSource {
    fn kind(&self) -> SectionKind {
        SectionKind::Feed
    }

    async fn fetch(&self, user_sub: &str, limit: i64) -> Result<Section, String> {
        let rows = sqlx::query(
            "SELECT CAST(i.id AS TEXT) AS row_key, \
                    i.title AS title, \
                    i.link AS link, \
                    i.summary AS summary, \
                    i.published_at AS published_at, \
                    f.title AS feed_title \
             FROM items i \
             JOIN feeds f ON f.id = i.feed_id \
             WHERE f.owner_sub = $1 AND i.read = FALSE \
             ORDER BY COALESCE(i.published_at, 0) DESC, i.id DESC \
             LIMIT $2",
        )
        .bind(user_sub)
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;

        let rows: Vec<InboxRow> = rows
            .iter()
            .map(|r| {
                Ok(InboxRow {
                    key: r
                        .try_get("row_key")
                        .map_err(|e: sqlx::Error| e.to_string())?,
                    title: r.try_get("title").map_err(|e: sqlx::Error| e.to_string())?,
                    snippet: r
                        .try_get("summary")
                        .map_err(|e: sqlx::Error| e.to_string())?,
                    source: r
                        .try_get("feed_title")
                        .map_err(|e: sqlx::Error| e.to_string())?,
                    at: r
                        .try_get("published_at")
                        .map_err(|e: sqlx::Error| e.to_string())?,
                    link: r.try_get("link").map_err(|e: sqlx::Error| e.to_string())?,
                    count: None,
                })
            })
            .collect::<Result<_, String>>()?;
        let total = rows.len() as i64;
        Ok(Section { total, rows })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(title: &str, at: i64) -> InboxRow {
        InboxRow {
            title: title.to_string(),
            at: Some(at),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn in_memory_source_returns_and_truncates() {
        let section = Section {
            total: 3,
            rows: vec![row("a", 1), row("b", 2), row("c", 3)],
        };
        let s = InMemorySource::new(SectionKind::Feed, section);
        let got = s.fetch("u", 2).await.unwrap();
        assert_eq!(got.total, 3);
        assert_eq!(got.rows.len(), 2);
        assert_eq!(s.kind(), SectionKind::Feed);
    }

    #[tokio::test]
    async fn down_source_errors() {
        let s = InMemorySource::down(SectionKind::Chat);
        assert!(s.fetch("u", 10).await.is_err());
    }
}
