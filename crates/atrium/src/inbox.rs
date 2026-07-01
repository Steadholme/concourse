//! The aggregating engine + the per-viewer inbox snapshot cache.
//!
//! [`Engine`] holds the three federated sources (any of which may be absent) and, for a given
//! viewer subject, fans out ALL present sources CONCURRENTLY (`tokio::join!`) into one [`Inbox`].
//! Each section is independently resilient: an absent source renders as an empty (but available)
//! column, an unreachable one as [`SectionState::Unavailable`] — one slow/down source can never
//! block the others or error the page (Portal's concurrent-fetch contract, applied per viewer).
//!
//! [`InboxCache`] memoizes each viewer's inbox for [`CACHE_TTL`] (~10 s), so back-to-back loads by
//! the same person share one concurrent refresh and don't re-hammer the three source databases.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::source::{Section, SectionKind, Source};

/// How long a viewer's inbox snapshot stays fresh before the next load refetches.
pub const CACHE_TTL: Duration = Duration::from_secs(10);

/// One column's state: either a fetched [`Section`] (possibly empty) or an unreachable source.
#[derive(Clone, Debug)]
pub enum SectionState {
    /// The source answered (an absent/unconfigured source yields an empty `Section` here).
    Ready(Section),
    /// The source is configured but was unreachable/broken — render "unavailable".
    Unavailable,
}

impl SectionState {
    /// The unread headline for the summary bar: the section total, or `0` when unavailable.
    pub fn total(&self) -> i64 {
        match self {
            SectionState::Ready(s) => s.total,
            SectionState::Unavailable => 0,
        }
    }

    /// True when the source was reachable (configured-and-answered or simply absent).
    pub fn is_available(&self) -> bool {
        matches!(self, SectionState::Ready(_))
    }
}

/// A viewer's aggregated activity across the three columns. Cheap to share behind `Arc`.
#[derive(Clone, Debug)]
pub struct Inbox {
    pub chat: SectionState,
    pub notifications: SectionState,
    pub feed: SectionState,
}

impl Inbox {
    /// Total unread across every available column (the summary-bar grand total).
    pub fn total_unread(&self) -> i64 {
        self.chat.total() + self.notifications.total() + self.feed.total()
    }

    /// The kinds whose source was unreachable on this fetch (for the audit signal).
    pub fn unavailable_kinds(&self) -> Vec<SectionKind> {
        let mut v = Vec::new();
        if !self.chat.is_available() {
            v.push(SectionKind::Chat);
        }
        if !self.notifications.is_available() {
            v.push(SectionKind::Notifications);
        }
        if !self.feed.is_available() {
            v.push(SectionKind::Feed);
        }
        v
    }
}

/// A unified search + source view over an aggregated [`Inbox`]. `q` is a case-insensitive
/// substring matched across a row's title / snippet / origin; `source` restricts the view to a
/// single column (all others render empty). Both default to "no restriction".
#[derive(Clone, Debug, Default)]
pub struct ViewFilter {
    /// Case-insensitive free-text query (empty = match everything).
    pub q: String,
    /// Restrict to one column (`None` = all three).
    pub source: Option<SectionKind>,
}

impl ViewFilter {
    /// Build from the raw query-string values (`?q=` / `?source=`). `all`/unknown source = no
    /// restriction; the query is trimmed.
    pub fn new(q: Option<String>, source: Option<String>) -> Self {
        ViewFilter {
            q: q.unwrap_or_default().trim().to_string(),
            source: source.as_deref().and_then(SectionKind::from_slug),
        }
    }

    /// True when neither a query nor a source restriction is set (the default full view).
    pub fn is_empty(&self) -> bool {
        self.q.is_empty() && self.source.is_none()
    }
}

/// Produce the viewer-facing [`Inbox`]: hide rows the viewer has acted on (`hidden`, keyed by
/// source slug) and apply the search + source [`ViewFilter`]. Totals are recomputed from the rows
/// that survive (a per-room chat count, or one per notification/feed row), so the summary always
/// matches what is shown. Unavailable columns pass through untouched.
pub fn view(
    inbox: &Inbox,
    hidden: &HashMap<String, HashSet<String>>,
    filter: &ViewFilter,
) -> Inbox {
    Inbox {
        chat: view_section(&inbox.chat, SectionKind::Chat, hidden, filter),
        notifications: view_section(&inbox.notifications, SectionKind::Notifications, hidden, filter),
        feed: view_section(&inbox.feed, SectionKind::Feed, hidden, filter),
    }
}

fn view_section(
    state: &SectionState,
    kind: SectionKind,
    hidden: &HashMap<String, HashSet<String>>,
    filter: &ViewFilter,
) -> SectionState {
    let SectionState::Ready(section) = state else {
        return state.clone(); // an unavailable column stays unavailable
    };
    // A source restriction blanks every other column (kept available, just empty).
    if let Some(only) = filter.source {
        if only != kind {
            return SectionState::Ready(Section::empty());
        }
    }
    let hidden_keys = hidden.get(kind.slug());
    let needle = filter.q.to_lowercase();
    let mut rows = Vec::new();
    let mut total: i64 = 0;
    for r in &section.rows {
        if let Some(keys) = hidden_keys {
            if !r.key.is_empty() && keys.contains(&r.key) {
                continue; // acted-on: hidden from the unread view
            }
        }
        if !needle.is_empty() {
            let hay = format!("{} {} {}", r.title, r.snippet, r.source).to_lowercase();
            if !hay.contains(&needle) {
                continue;
            }
        }
        total += r.count.unwrap_or(1);
        rows.push(r.clone());
    }
    SectionState::Ready(Section { total, rows })
}

/// The aggregating engine: the (optional) source per column. An absent source = that column is
/// always an empty, available section. Cheap to clone (sources behind `Arc`).
#[derive(Clone, Default)]
pub struct Engine {
    chat: Option<Arc<dyn Source>>,
    notifications: Option<Arc<dyn Source>>,
    feed: Option<Arc<dyn Source>>,
}

impl Engine {
    /// Build an engine from the three optional sources (registration matches the config DSNs).
    pub fn new(
        chat: Option<Arc<dyn Source>>,
        notifications: Option<Arc<dyn Source>>,
        feed: Option<Arc<dyn Source>>,
    ) -> Self {
        Engine {
            chat,
            notifications,
            feed,
        }
    }

    /// Aggregate one viewer's inbox: fan out every present source CONCURRENTLY and assemble. An
    /// absent source contributes an empty section; an erroring one contributes `Unavailable`.
    /// Always succeeds — never errors the page.
    pub async fn aggregate(&self, user_sub: &str, limit: i64) -> Inbox {
        let (chat, notifications, feed) = tokio::join!(
            fetch_section(self.chat.as_ref(), user_sub, limit),
            fetch_section(self.notifications.as_ref(), user_sub, limit),
            fetch_section(self.feed.as_ref(), user_sub, limit),
        );
        Inbox {
            chat,
            notifications,
            feed,
        }
    }
}

/// Fetch one column: `None` source -> empty-and-available; `Some` source -> `Ready` on success,
/// `Unavailable` on error (the error is logged, never propagated).
async fn fetch_section(source: Option<&Arc<dyn Source>>, user_sub: &str, limit: i64) -> SectionState {
    let Some(source) = source else {
        return SectionState::Ready(Section::empty());
    };
    match source.fetch(user_sub, limit).await {
        Ok(section) => SectionState::Ready(section),
        Err(e) => {
            tracing::warn!(error = %e, "federated source unavailable — rendering placeholder");
            SectionState::Unavailable
        }
    }
}

/// Per-viewer cached cell: the freshest inbox tagged with when it was taken.
type Cached = HashMap<String, (Instant, Arc<Inbox>)>;

/// A ~10 s cache around [`Engine::aggregate`], keyed by viewer subject. Cheap to clone (state
/// behind `Arc`). The lock is NEVER held across the network `.await`, so concurrent loads stay
/// non-blocking.
#[derive(Clone)]
pub struct InboxCache {
    ttl: Duration,
    inner: Arc<Mutex<Cached>>,
}

impl InboxCache {
    /// New cache with the given freshness window.
    pub fn new(ttl: Duration) -> Self {
        InboxCache {
            ttl,
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Return `(inbox, fresh)` for `user_sub`: a cached inbox while it is fresh (`fresh = false`),
    /// otherwise a freshly aggregated one (`fresh = true`). `fresh` lets the caller emit a single
    /// audit event per real refresh rather than on every cached view.
    pub async fn get(&self, engine: &Engine, user_sub: &str, limit: i64) -> (Arc<Inbox>, bool) {
        if let Some((at, inbox)) = self.inner.lock().expect("inbox cache poisoned").get(user_sub) {
            if at.elapsed() < self.ttl {
                return (Arc::clone(inbox), false);
            }
        }
        let fresh = Arc::new(engine.aggregate(user_sub, limit).await);
        self.inner
            .lock()
            .expect("inbox cache poisoned")
            .insert(user_sub.to_string(), (Instant::now(), Arc::clone(&fresh)));
        (fresh, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{InMemorySource, InboxRow};

    fn section(total: i64, n: usize) -> Section {
        Section {
            total,
            rows: (0..n).map(|i| InboxRow { title: format!("r{i}"), ..Default::default() }).collect(),
        }
    }

    fn engine_all_up() -> Engine {
        Engine::new(
            Some(Arc::new(InMemorySource::new(SectionKind::Chat, section(5, 2)))),
            Some(Arc::new(InMemorySource::new(SectionKind::Notifications, section(3, 3)))),
            Some(Arc::new(InMemorySource::new(SectionKind::Feed, section(7, 1)))),
        )
    }

    #[tokio::test]
    async fn aggregate_sums_totals_across_columns() {
        let inbox = engine_all_up().aggregate("u", 50).await;
        assert_eq!(inbox.total_unread(), 15);
        assert!(inbox.unavailable_kinds().is_empty());
    }

    #[tokio::test]
    async fn absent_source_is_empty_not_unavailable() {
        let engine = Engine::new(None, None, None);
        let inbox = engine.aggregate("u", 50).await;
        assert_eq!(inbox.total_unread(), 0);
        // Absent sources are available (empty), not "unavailable".
        assert!(inbox.unavailable_kinds().is_empty());
        assert!(inbox.chat.is_available());
    }

    #[tokio::test]
    async fn down_source_is_unavailable_others_still_render() {
        let engine = Engine::new(
            Some(Arc::new(InMemorySource::down(SectionKind::Chat))),
            Some(Arc::new(InMemorySource::new(SectionKind::Notifications, section(4, 1)))),
            None,
        );
        let inbox = engine.aggregate("u", 50).await;
        assert_eq!(inbox.unavailable_kinds(), vec![SectionKind::Chat]);
        // The reachable column still reports.
        assert_eq!(inbox.total_unread(), 4);
    }

    fn keyed_row(key: &str, title: &str, snippet: &str, count: Option<i64>) -> InboxRow {
        InboxRow {
            key: key.to_string(),
            title: title.to_string(),
            snippet: snippet.to_string(),
            count,
            ..Default::default()
        }
    }

    fn sample_inbox() -> Inbox {
        Inbox {
            chat: SectionState::Ready(Section {
                total: 5,
                rows: vec![
                    keyed_row("r1", "#general", "hi", Some(2)),
                    keyed_row("r2", "#random", "lunch?", Some(3)),
                ],
            }),
            notifications: SectionState::Ready(Section {
                total: 1,
                rows: vec![keyed_row("n1", "Deploy done", "current is live", None)],
            }),
            feed: SectionState::Unavailable,
        }
    }

    #[test]
    fn view_hides_acted_on_rows_and_recomputes_totals() {
        let inbox = sample_inbox();
        let mut hidden: HashMap<String, HashSet<String>> = HashMap::new();
        hidden.entry("chat".into()).or_default().insert("r1".into());
        let out = view(&inbox, &hidden, &ViewFilter::default());
        let SectionState::Ready(chat) = &out.chat else { panic!() };
        assert_eq!(chat.rows.len(), 1, "dismissed room removed");
        assert_eq!(chat.rows[0].title, "#random");
        assert_eq!(chat.total, 3, "total drops by the hidden room's unread count");
        // Unavailable column passes through untouched.
        assert!(!out.feed.is_available());
    }

    #[test]
    fn view_search_matches_title_snippet_and_origin() {
        let inbox = sample_inbox();
        let empty = HashMap::new();
        let f = ViewFilter::new(Some("lunch".into()), None);
        let out = view(&inbox, &empty, &f);
        let SectionState::Ready(chat) = &out.chat else { panic!() };
        assert_eq!(chat.rows.len(), 1);
        assert_eq!(chat.rows[0].title, "#random");
        // The notification doesn't match "lunch" -> emptied.
        let SectionState::Ready(notifs) = &out.notifications else { panic!() };
        assert!(notifs.rows.is_empty());
        assert_eq!(notifs.total, 0);
    }

    #[test]
    fn view_source_filter_blanks_other_columns() {
        let inbox = sample_inbox();
        let empty = HashMap::new();
        let f = ViewFilter::new(None, Some("notifications".into()));
        let out = view(&inbox, &empty, &f);
        // Only notifications keeps rows; chat is emptied (still available).
        let SectionState::Ready(chat) = &out.chat else { panic!() };
        assert!(chat.rows.is_empty());
        let SectionState::Ready(notifs) = &out.notifications else { panic!() };
        assert_eq!(notifs.rows.len(), 1);
    }

    #[test]
    fn empty_filter_preserves_the_original_view() {
        let inbox = sample_inbox();
        let empty = HashMap::new();
        assert!(ViewFilter::default().is_empty());
        let out = view(&inbox, &empty, &ViewFilter::default());
        assert_eq!(out.total_unread(), inbox.total_unread());
    }

    #[tokio::test]
    async fn cache_serves_fresh_then_cached() {
        let cache = InboxCache::new(Duration::from_secs(10));
        let engine = engine_all_up();
        let (_a, fresh1) = cache.get(&engine, "u", 50).await;
        assert!(fresh1, "first load is a real fetch");
        let (_b, fresh2) = cache.get(&engine, "u", 50).await;
        assert!(!fresh2, "second load within TTL is cached");
        // A different viewer is a distinct cache key -> a real fetch.
        let (_c, fresh3) = cache.get(&engine, "other", 50).await;
        assert!(fresh3);
    }
}
