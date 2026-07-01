//! The unified activity inbox: `GET /` renders the "what's new for me" command center.
//!
//! A sticky app-bar, a SUMMARY BAR of total unread (chat + notifications + feeds), and three
//! resilient COLUMNS — Chat unread (Murmur), Notifications (Klaxon), Feed river (Current) — each
//! with its own count, a deep link out to the owning service, and its rows. A row of static
//! quick-links (mail / forum / wiki / home) sits at the foot.
//!
//! Every column is best-effort: the data comes from one cached (~10 s), concurrent federation of
//! the three source databases ([`crate::inbox`]). A source whose DSN is unset renders an empty
//! "all caught up" column; an unreachable one renders an "unavailable" placeholder — the page
//! NEVER errors or hangs. All federated text is HTML-escaped (untrusted cross-service content).

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap};
use axum::response::{Html, IntoResponse, Response};

use crate::audit::AuditEvent;
use crate::auth;
use crate::config::SECTION_LIMIT;
use crate::csrf;
use crate::handlers::{esc, rel_time, topbar, truncate, InboxQuery, APP_CSS};
use crate::inbox::{self, Inbox, SectionState, ViewFilter};
use crate::source::{InboxRow, SectionKind};
use crate::AppState;

const DASHBOARD_HTML: &str = include_str!("../../templates/dashboard.html");

/// Deep links OUT to the owning services (the column headers link here).
const CHAT_URL: &str = "https://chat.w33d.xyz";
const NOTIFY_URL: &str = "https://notify.w33d.xyz";
const RSS_URL: &str = "https://rss.w33d.xyz";

/// `GET /` — the unified inbox. Renders for any request the gateway forwards; the viewer identity
/// (and the per-viewer federation scope) comes from the injected `X-Auth-Subject` / `X-Auth-Email`.
/// An unauthenticated probe (no subject) still renders an empty, calm inbox rather than erroring.
pub async fn dashboard(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<InboxQuery>,
) -> Response {
    let email = auth::display_email(&headers);
    let subject = auth::subject(&headers);
    let filter = ViewFilter::new(query.q, query.source);

    // Double-submit CSRF: reuse the token already on the request's cookie, or mint a fresh one.
    // The same token is planted in a cookie (below) AND echoed into the page so an action POST can
    // present it back in the X-CSRF-Token header.
    let token = csrf::cookie_token(&headers).unwrap_or_else(csrf::issue_token);

    // No gateway identity -> nothing to scope a federation by; render the empty inbox shell.
    let Some(sub) = subject else {
        let empty = empty_inbox();
        let view = inbox::view(&empty, &Default::default(), &filter);
        return page(render(&view, &email, &filter, &token, crate::now_secs()), &token);
    };

    let (raw, fresh) = state.cache.get(&state.engine, &sub, SECTION_LIMIT).await;

    // Audit only on a REAL refresh (not cached views), so Watchtower isn't flooded: one info
    // event per inbox load, plus a warning per unreachable federated source (an ops signal — the
    // private chat/notification/feed contents NEVER ride the event). The total is the true unread,
    // measured before any search/overlay filtering of the on-screen view.
    if fresh {
        state.audit.emit(AuditEvent::info(
            "inbox.view",
            &email,
            "/",
            &format!("total_unread={}", raw.total_unread()),
        ));
        for kind in raw.unavailable_kinds() {
            state.audit.emit(AuditEvent::warning(
                "inbox.source_unavailable",
                &email,
                kind.slug(),
                source_name(kind),
            ));
        }
    }

    // Hide acted-on rows (fail-open on a down overlay DB) and apply the search + source filter.
    let hidden = state.store.hidden(&sub).await.unwrap_or_default();
    let view = inbox::view(&raw, &hidden, &filter);
    page(render(&view, &email, &filter, &token, crate::now_secs()), &token)
}

/// Wrap the rendered HTML in a response that (re-)plants the CSRF cookie on every page load.
fn page(html: String, token: &str) -> Response {
    (
        [(header::SET_COOKIE, csrf::set_cookie(token))],
        Html(html),
    )
        .into_response()
}

/// The empty inbox shown to an unauthenticated probe (all columns empty + available). Shared with
/// the JSON poll endpoint so both surfaces render the same calm shell.
pub(crate) fn empty_inbox() -> Inbox {
    use crate::source::Section;
    Inbox {
        chat: SectionState::Ready(Section::empty()),
        notifications: SectionState::Ready(Section::empty()),
        feed: SectionState::Ready(Section::empty()),
    }
}

fn render(inbox: &Inbox, email: &str, filter: &ViewFilter, token: &str, now: i64) -> String {
    DASHBOARD_HTML
        .replace("{{CSS}}", APP_CSS)
        .replace("{{CSRF}}", &esc(token))
        .replace("{{TOPBAR}}", &topbar("Inbox", email))
        .replace("{{SEARCH}}", &render_search(filter))
        .replace("{{SUMMARY}}", &render_summary(inbox))
        .replace("{{COLUMNS}}", &render_columns(inbox, now))
}

/// The unified search + source-filter bar. A plain GET form (works with JS off); the live poll
/// carries the same `?q=` / `?source=` so a refresh preserves the filter. Current values are echoed
/// back HTML-escaped so the query is never reflected unescaped.
pub(crate) fn render_search(filter: &ViewFilter) -> String {
    let opt = |slug: &str, label: &str| {
        let selected = filter.source.map(|k| k.slug()) == Some(slug);
        format!(
            r#"<option value="{slug}"{sel}>{label}</option>"#,
            slug = esc(slug),
            sel = if selected { " selected" } else { "" },
            label = esc(label),
        )
    };
    let all_selected = if filter.source.is_none() { " selected" } else { "" };
    format!(
        r#"<form class="searchbar" method="get" role="search">
  <input class="searchbar__q" type="search" name="q" value="{q}" placeholder="Search chat, notifications & feeds…" aria-label="Search inbox">
  <select class="searchbar__source" name="source" aria-label="Filter by source">
    <option value="all"{all}>All sources</option>
    {chat}{notifs}{feed}
  </select>
  <button class="btn btn-secondary btn-sm" type="submit">Search</button>
  <a class="searchbar__clear" href="/">Clear</a>
</form>"#,
        q = esc(&filter.q),
        all = all_selected,
        chat = opt("chat", "Chat"),
        notifs = opt("notifications", "Notifications"),
        feed = opt("feed", "Feeds"),
    )
}

/// The three activity columns concatenated (Chat, Notifications, Feed). Shared by the full page
/// render and by the JSON poll endpoint, which swaps this fragment into the `#columns-slot`
/// container so a live refresh re-renders the columns without a full page reload.
pub(crate) fn render_columns(inbox: &Inbox, now: i64) -> String {
    format!(
        "{}{}{}",
        render_column(
            "Chat",
            "Unread messages",
            CHAT_URL,
            "Open chat",
            SectionKind::Chat,
            &inbox.chat,
            row_chat,
            "You're all caught up on chat.",
            now,
        ),
        render_column(
            "Notifications",
            "Unread alerts",
            NOTIFY_URL,
            "Open notifications",
            SectionKind::Notifications,
            &inbox.notifications,
            row_notification,
            "No unread notifications.",
            now,
        ),
        render_column(
            "Feed river",
            "Fresh items",
            RSS_URL,
            "Open reader",
            SectionKind::Feed,
            &inbox.feed,
            row_feed,
            "No fresh feed items.",
            now,
        ),
    )
}

/// The summary bar: a grand total plus a per-column unread chip (or "—" when unavailable). Shared
/// with the JSON poll endpoint, which swaps it into the `#summary-slot` container on each refresh.
pub(crate) fn render_summary(inbox: &Inbox) -> String {
    let grand = inbox.total_unread();
    let lead = if grand == 0 {
        "You're all caught up".to_string()
    } else {
        format!("{grand} unread")
    };
    format!(
        r#"<div class="summary">
  <div class="summary__lead">
    <span class="summary__total">{lead}</span>
    <span class="summary__sub">across chat, notifications &amp; feeds</span>
  </div>
  <div class="summary__chips">
    {chat}{notifs}{feed}
  </div>
</div>"#,
        lead = esc(&lead),
        chat = summary_chip("Chat", &inbox.chat),
        notifs = summary_chip("Notifications", &inbox.notifications),
        feed = summary_chip("Feeds", &inbox.feed),
    )
}

fn summary_chip(label: &str, state: &SectionState) -> String {
    let (value, cls) = match state {
        SectionState::Unavailable => ("—".to_string(), "chip chip-muted"),
        SectionState::Ready(s) if s.total > 0 => (s.total.to_string(), "chip chip-active"),
        SectionState::Ready(_) => ("0".to_string(), "chip"),
    };
    format!(
        r#"<span class="{cls}"><span class="chip__num">{value}</span><span class="chip__label">{label}</span></span>"#,
        cls = cls,
        value = esc(&value),
        label = esc(label),
    )
}

/// Render one activity column: header (title + count + deep link) and its body (rows, an empty
/// "caught up" state, or an "unavailable" placeholder).
#[allow(clippy::too_many_arguments)]
fn render_column(
    title: &str,
    subtitle: &str,
    open_url: &str,
    open_label: &str,
    kind: SectionKind,
    state: &SectionState,
    row_fn: fn(&InboxRow, &str, i64) -> String,
    empty_msg: &str,
    now: i64,
) -> String {
    let (count_badge, body) = match state {
        SectionState::Unavailable => (
            String::new(),
            r#"<div class="col__state col__state--down">
  <span class="col__state-dot" aria-hidden="true"></span>
  <div><strong>Source unavailable</strong><span>This view will return when the service is reachable.</span></div>
</div>"#
                .to_string(),
        ),
        SectionState::Ready(section) if section.rows.is_empty() => (
            count_badge(section.total),
            format!(
                r#"<div class="col__state col__state--empty"><div><strong>All clear</strong><span>{}</span></div></div>"#,
                esc(empty_msg)
            ),
        ),
        SectionState::Ready(section) => {
            let mut rows = String::new();
            for r in &section.rows {
                rows.push_str(&row_fn(r, kind.slug(), now));
            }
            (count_badge(section.total), rows)
        }
    };

    format!(
        r#"<section class="col">
  <div class="col__head">
    <div class="col__heading">
      <h2 class="col__title">{title}{badge}</h2>
      <p class="col__sub">{subtitle}</p>
    </div>
    <a class="col__open" href="{open_url}">{open_label} &rarr;</a>
  </div>
  <div class="col__body">
    {body}
  </div>
</section>"#,
        title = esc(title),
        badge = count_badge,
        subtitle = esc(subtitle),
        open_url = esc(open_url),
        open_label = esc(open_label),
        body = body,
    )
}

fn count_badge(total: i64) -> String {
    if total <= 0 {
        return String::new();
    }
    format!(r#" <span class="col__count">{total}</span>"#)
}

// --- Per-column row renderers ----------------------------------------------------------

/// A chat room row: room name + per-room unread badge, latest-message preview, relative time.
fn row_chat(r: &InboxRow, source_slug: &str, now: i64) -> String {
    let unread = match r.count {
        Some(n) if n > 0 => format!(r#"<span class="row__badge">{n}</span>"#),
        _ => String::new(),
    };
    row_shell(
        source_slug,
        &r.key,
        &r.link,
        &esc(&r.title),
        &unread,
        &esc(&truncate(&r.snippet, 120)),
        "",
        r.at,
        now,
    )
}

/// A notification row: title, body preview, relative time.
fn row_notification(r: &InboxRow, source_slug: &str, now: i64) -> String {
    row_shell(
        source_slug,
        &r.key,
        &r.link,
        &esc(&r.title),
        "",
        &esc(&truncate(&r.snippet, 120)),
        "",
        r.at,
        now,
    )
}

/// A feed item row: item title, summary preview, feed-title origin tag, relative time.
fn row_feed(r: &InboxRow, source_slug: &str, now: i64) -> String {
    let title = if r.title.trim().is_empty() {
        "(untitled)".to_string()
    } else {
        esc(&r.title)
    };
    row_shell(
        source_slug,
        &r.key,
        &r.link,
        &title,
        "",
        &esc(&truncate(&r.snippet, 120)),
        &esc(&r.source),
        r.at,
        now,
    )
}

/// The mark-read / dismiss controls for a row. Rendered only when the row carries a stable `key`
/// (the token the action addresses it by); a keyless row is display-only. The buttons carry the
/// `data-action` the delegated click handler reads; the owning `.row` carries `data-source` /
/// `data-key`.
fn row_actions(key: &str) -> String {
    if key.trim().is_empty() {
        return String::new();
    }
    r#"<div class="row__actions">
    <button type="button" class="row__act" data-action="read">Mark read</button>
    <button type="button" class="row__act row__act--dismiss" data-action="dismiss">Dismiss</button>
  </div>"#
        .to_string()
}

/// Shared row markup. `link` is treated as a same-origin-or-external URL and only escaped (the
/// federated services emit their own absolute links); `badge` is pre-rendered safe HTML. The row is
/// a container div (so the deep-link anchor and the action buttons are siblings, never nested) that
/// carries the `data-source` / `data-key` the in-place action handler uses to address it.
#[allow(clippy::too_many_arguments)]
fn row_shell(
    source_slug: &str,
    key: &str,
    link: &str,
    title_html: &str,
    badge_html: &str,
    snippet_html: &str,
    origin_html: &str,
    at: Option<i64>,
    now: i64,
) -> String {
    let when = match at {
        Some(t) => rel_time(t, now),
        None => String::new(),
    };
    let meta = match (origin_html.is_empty(), when.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!(r#"<div class="row__meta">{origin_html}</div>"#),
        (true, false) => format!(r#"<div class="row__meta">{}</div>"#, esc(&when)),
        (false, false) => format!(
            r#"<div class="row__meta">{origin_html} · {}</div>"#,
            esc(&when)
        ),
    };
    let snippet = if snippet_html.is_empty() {
        String::new()
    } else {
        format!(r#"<div class="row__snippet">{snippet_html}</div>"#)
    };
    format!(
        r#"<div class="row" data-source="{source}" data-key="{key}">
  <a class="row__link" href="{link}">
    <div class="row__top"><span class="row__title">{title_html}</span>{badge_html}</div>
    {snippet}
    {meta}
  </a>
  {actions}
</div>"#,
        source = esc(source_slug),
        key = esc(key),
        link = esc(link),
        title_html = title_html,
        badge_html = badge_html,
        snippet = snippet,
        meta = meta,
        actions = row_actions(key),
    )
}

// --- Audit helpers ---------------------------------------------------------------------

fn source_name(kind: SectionKind) -> &'static str {
    match kind {
        SectionKind::Chat => "murmur",
        SectionKind::Notifications => "klaxon",
        SectionKind::Feed => "current",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{InMemorySource, Section};
    use crate::inbox::Engine;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn section(total: i64, titles: &[&str]) -> Section {
        Section {
            total,
            rows: titles
                .iter()
                .map(|t| InboxRow {
                    key: t.to_string(),
                    title: t.to_string(),
                    snippet: "preview".to_string(),
                    link: "https://chat.w33d.xyz/r/x".to_string(),
                    at: Some(crate::now_secs()),
                    count: Some(2),
                    ..Default::default()
                })
                .collect(),
        }
    }

    fn state_with_sources() -> AppState {
        let engine = Engine::new(
            Some(Arc::new(InMemorySource::new(SectionKind::Chat, section(2, &["#general"])))),
            Some(Arc::new(InMemorySource::down(SectionKind::Notifications))),
            Some(Arc::new(InMemorySource::new(SectionKind::Feed, Section::empty()))),
        );
        crate::build_state_with_engine(engine)
    }

    #[tokio::test]
    async fn dashboard_renders_columns_summary_and_resilience() {
        let app = crate::app(state_with_sources());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("x-auth-subject", "u_1")
                    .header("x-auth-email", "ops@w33d.xyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("#general"), "chat row rendered");
        assert!(html.contains("Source unavailable"), "down notifications column degrades");
        assert!(html.contains("No fresh feed items."), "empty feed column shows caught-up state");
        assert!(html.contains("ops@w33d.xyz"), "signed-in email in app bar");
        // Live auto-refresh wiring: the poll targets + fetch of the JSON endpoint are present.
        assert!(html.contains(r#"id="summary-slot""#), "summary refresh slot present");
        assert!(html.contains(r#"id="columns-slot""#), "columns refresh slot present");
        assert!(html.contains("/api/inbox"), "poll fetches the JSON endpoint");
    }

    #[tokio::test]
    async fn dashboard_renders_search_actions_and_sets_csrf_cookie() {
        let app = crate::app(state_with_sources());
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("x-auth-subject", "u_1")
                    .header("x-auth-email", "ops@w33d.xyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        // A CSRF cookie is planted on the page load (double-submit half).
        let set_cookie = res
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(set_cookie.contains("atrium_csrf="), "csrf cookie set");
        assert!(set_cookie.contains("SameSite=Strict"), "csrf cookie is strict");
        let html = String::from_utf8(
            axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec(),
        )
        .unwrap();
        assert!(html.contains(r#"class="searchbar""#), "unified search bar rendered");
        assert!(html.contains(r#"name="source""#), "source filter present");
        assert!(html.contains(r#"data-action="dismiss""#), "row dismiss control present");
        assert!(html.contains(r#"data-action="read""#), "row mark-read control present");
        assert!(html.contains(r#"meta name="csrf-token""#), "csrf meta echoed into page");
    }

    #[tokio::test]
    async fn dashboard_search_filters_rows() {
        // Two chat rooms; a query keeps only the matching one.
        let engine = Engine::new(
            Some(Arc::new(InMemorySource::new(
                SectionKind::Chat,
                section(4, &["#general", "#random"]),
            ))),
            None,
            None,
        );
        let app = crate::app(crate::build_state_with_engine(engine));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/?q=random")
                    .header("x-auth-subject", "u_1")
                    .header("x-auth-email", "ops@w33d.xyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = String::from_utf8(
            axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec(),
        )
        .unwrap();
        assert!(html.contains("#random"), "matching row kept");
        assert!(!html.contains("#general"), "non-matching row filtered out");
        // The query is echoed back into the search input, escaped.
        assert!(html.contains(r#"value="random""#), "query reflected in the search box");
    }

    #[tokio::test]
    async fn dashboard_without_identity_still_renders_200() {
        let app = crate::app(crate::build_dev_state());
        let res = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
