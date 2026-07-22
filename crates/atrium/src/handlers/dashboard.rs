//! The unified activity inbox: `GET /` renders the "what's new for me" triage river.
//!
//! A sticky app-bar, a compact source gauge, and one chronological river merge Chat,
//! Notifications, and Feeds while preserving redundant source words and glyphs. Native action
//! forms keep the triage path usable without JavaScript; progressive enhancement can still update
//! it in place.
//!
//! Every source is best-effort: data comes from one cached (~10 s), concurrent federation of the
//! three source databases ([`crate::inbox`]). Unreachable sources are named in a degraded-state
//! notice while available activity keeps flowing; the page never errors or hangs. All federated
//! text is HTML-escaped (untrusted cross-service content).

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap};
use axum::response::{Html, IntoResponse, Response};

use crate::audit::AuditEvent;
use crate::auth;
use crate::config::SECTION_LIMIT;
use crate::csrf;
use crate::handlers::{app_css, esc, rel_time, topbar, truncate, InboxQuery};
use crate::inbox::{self, Inbox, SectionState, ViewFilter};
use crate::source::{InboxRow, SectionKind};
use crate::AppState;

const DASHBOARD_HTML: &str = include_str!("../../templates/dashboard.html");

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
        return page(
            render(&view, &email, &filter, &token, crate::now_secs()),
            &token,
        );
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
    page(
        render(&view, &email, &filter, &token, crate::now_secs()),
        &token,
    )
}

/// Wrap the rendered HTML in a response that (re-)plants the CSRF cookie on every page load.
fn page(html: String, token: &str) -> Response {
    ([(header::SET_COOKIE, csrf::set_cookie(token))], Html(html)).into_response()
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
    let csrf = esc(token);
    let bar = topbar("Inbox", email);
    let search = render_search(filter);
    let summary = render_summary(inbox);
    let columns = render_columns(inbox, now, token, filter);
    fill_template(
        DASHBOARD_HTML,
        &[
            ("CSS", app_css()),
            ("CSRF", &csrf),
            ("TOPBAR", &bar),
            ("SEARCH", &search),
            ("SUMMARY", &summary),
            ("COLUMNS", &columns),
        ],
    )
}

/// Replace slots while scanning only the original template bytes. Inserted user or federated text
/// is never interpreted as a second template token.
fn fill_template(template: &str, slots: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut remaining = template;
    while let Some(start) = remaining.find("{{") {
        out.push_str(&remaining[..start]);
        let token_start = start + 2;
        let Some(relative_end) = remaining[token_start..].find("}}") else {
            out.push_str(&remaining[start..]);
            return out;
        };
        let token_end = token_start + relative_end;
        let name = &remaining[token_start..token_end];
        if let Some((_, replacement)) = slots.iter().find(|(slot, _)| *slot == name) {
            out.push_str(replacement);
        } else {
            out.push_str(&remaining[start..token_end + 2]);
        }
        remaining = &remaining[token_end + 2..];
    }
    out.push_str(remaining);
    out
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
    let all_selected = if filter.source.is_none() {
        " selected"
    } else {
        ""
    };
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

/// One chronological river fragment shared by the full page and JSON poll. Slot ids belong to the
/// K3-owned page shell and are deliberately absent here.
fn compare_river_rows(left: &InboxRow, right: &InboxRow) -> std::cmp::Ordering {
    match (left.at, right.at) {
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

pub(crate) fn render_columns(inbox: &Inbox, now: i64, token: &str, filter: &ViewFilter) -> String {
    let states = [
        (SectionKind::Chat, &inbox.chat),
        (SectionKind::Notifications, &inbox.notifications),
        (SectionKind::Feed, &inbox.feed),
    ];
    let mut rows: Vec<(SectionKind, &InboxRow)> = Vec::new();
    for (kind, state) in states {
        if let SectionState::Ready(section) = state {
            rows.extend(section.rows.iter().map(|row| (kind, row)));
        }
    }
    // `sort_by` is stable: equal timestamps retain the fixed source order above and each source's
    // original order. Explicit Option ordering keeps `None` last even beside `Some(i64::MIN)`.
    rows.sort_by(|(_, left), (_, right)| compare_river_rows(left, right));

    let mut body = String::new();
    let unavailable = inbox.unavailable_kinds();
    if !unavailable.is_empty() {
        let details = unavailable
            .iter()
            .map(|kind| {
                format!(
                    r#"<span class="river__degraded-item" data-source="{slug}">{word} unavailable</span>"#,
                    slug = kind.slug(),
                    word = source_word(*kind),
                )
            })
            .collect::<String>();
        body.push_str(&format!(
            r#"<div class="river__degraded"><strong>Some activity is unavailable right now.</strong>{details}</div>"#
        ));
    }

    if rows.is_empty() {
        let message = if filter.is_empty() {
            "You're all caught up."
        } else {
            "No matching activity."
        };
        body.push_str(&format!(r#"<div class="river__empty">{message}</div>"#));
    } else {
        for (kind, row) in rows {
            body.push_str(&river_row(kind, row, now, token, filter));
        }
    }

    format!(r#"<section class="river" aria-label="Activity, newest first">{body}</section>"#)
}

/// Compact reach gauge: total unread plus a word/glyph/count channel for every source. It is a
/// programmatic refresh focus target, not an ordinary Tab stop.
pub(crate) fn render_summary(inbox: &Inbox) -> String {
    let grand = inbox.total_unread();
    let lead = if grand == 0 {
        "You're all caught up".to_string()
    } else {
        format!("{grand} unread")
    };
    format!(
        r#"<p class="gauge" tabindex="-1" aria-label="Inbox reach"><span class="gauge__total">{lead}</span><span class="gauge__reach">{chat}{notifications}{feed}</span></p>"#,
        lead = esc(&lead),
        chat = gauge_reach(SectionKind::Chat, &inbox.chat),
        notifications = gauge_reach(SectionKind::Notifications, &inbox.notifications),
        feed = gauge_reach(SectionKind::Feed, &inbox.feed),
    )
}

fn gauge_reach(kind: SectionKind, state: &SectionState) -> String {
    let value = match state {
        SectionState::Unavailable => "—".to_string(),
        SectionState::Ready(section) => section.total.to_string(),
    };
    format!(
        r#"<span class="gauge__reach-item" data-source="{slug}"><span class="gauge__glyph" aria-hidden="true">{glyph}</span><span class="gauge__word">{word}</span><span class="gauge__count">{value}</span></span>"#,
        slug = kind.slug(),
        glyph = source_glyph(kind),
        word = source_word(kind),
        value = esc(&value),
    )
}

fn river_row(
    kind: SectionKind,
    row: &InboxRow,
    now: i64,
    token: &str,
    filter: &ViewFilter,
) -> String {
    let title = if kind == SectionKind::Feed && row.title.trim().is_empty() {
        "(untitled)"
    } else {
        &row.title
    };
    let badge = if kind == SectionKind::Chat {
        match row.count {
            Some(count) if count > 0 => {
                format!(r#"<span class="row__badge">{count}</span>"#)
            }
            _ => String::new(),
        }
    } else {
        String::new()
    };
    let snippet = truncate(&row.snippet, 120);
    let snippet = if snippet.is_empty() {
        String::new()
    } else {
        format!(r#"<div class="row__snippet">{}</div>"#, esc(&snippet))
    };
    let origin = if kind == SectionKind::Feed {
        row.source.trim()
    } else {
        ""
    };
    let when = row.at.map(|at| rel_time(at, now)).unwrap_or_default();
    let meta = match (origin.is_empty(), when.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!(r#"<div class="row__meta">{}</div>"#, esc(origin)),
        (true, false) => format!(r#"<div class="row__meta">{}</div>"#, esc(&when)),
        (false, false) => format!(
            r#"<div class="row__meta">{} · {}</div>"#,
            esc(origin),
            esc(&when)
        ),
    };
    let link = safe_row_link(&row.link);

    format!(
        r#"<div class="row" data-source="{slug}" data-key="{key}">
  <span class="river__source" data-source="{slug}"><span class="river__glyph" aria-hidden="true">{glyph}</span><span class="river__source-word">{word}</span></span>
  <a class="row__link" href="{link}"><div class="row__top"><span class="row__title">{title}</span>{badge}</div>{snippet}{meta}</a>
  {actions}
</div>"#,
        slug = kind.slug(),
        key = esc(&row.key),
        glyph = source_glyph(kind),
        word = source_word(kind),
        link = link,
        title = esc(title),
        badge = badge,
        snippet = snippet,
        meta = meta,
        actions = river_actions(kind, &row.key, token, filter),
    )
}

/// Keep row navigation in browser-safe URL classes before HTML-attribute
/// escaping. Federated/source-provided links are data, not trusted markup.
fn safe_row_link(raw: &str) -> String {
    if raw.chars().any(|ch| ch.is_control() || ch == '\\') {
        return "/".to_string();
    }

    let link = raw.trim();
    let lower = link.to_ascii_lowercase();
    let same_origin_path = link.starts_with('/') && !link.starts_with("//");
    let safe_scheme = lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("mailto:");

    if same_origin_path || safe_scheme {
        esc(link)
    } else {
        "/".to_string()
    }
}

fn river_actions(kind: SectionKind, key: &str, token: &str, filter: &ViewFilter) -> String {
    if key.trim().is_empty() {
        return String::new();
    }
    format!(
        r#"<div class="row__actions">{}{}</div>"#,
        action_form("read", "Mark read", "row__act", kind, key, token, filter),
        action_form(
            "dismiss",
            "Dismiss",
            "row__act row__act--dismiss",
            kind,
            key,
            token,
            filter,
        ),
    )
}

#[allow(clippy::too_many_arguments)]
fn action_form(
    action: &str,
    label: &str,
    button_class: &str,
    kind: SectionKind,
    key: &str,
    token: &str,
    filter: &ViewFilter,
) -> String {
    let return_source = filter
        .source
        .map(|source| source.slug())
        .unwrap_or_default();
    format!(
        r#"<form class="river__actform" method="post" action="/api/inbox/{action}"><input type="hidden" name="source" value="{source}"><input type="hidden" name="key" value="{key}"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="return_q" value="{return_q}"><input type="hidden" name="return_source" value="{return_source}"><button type="submit" class="{button_class}" data-action="{action}">{label}</button></form>"#,
        source = kind.slug(),
        key = esc(key),
        csrf = esc(token),
        return_q = esc(&filter.q),
        return_source = esc(return_source),
        label = esc(label),
    )
}

fn source_word(kind: SectionKind) -> &'static str {
    match kind {
        SectionKind::Chat => "Chat",
        SectionKind::Notifications => "Notifications",
        SectionKind::Feed => "Feeds",
    }
}

fn source_glyph(kind: SectionKind) -> &'static str {
    match kind {
        SectionKind::Chat => "◒",
        SectionKind::Notifications => "◆",
        SectionKind::Feed => "≋",
    }
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
    use crate::inbox::Engine;
    use crate::source::{InMemorySource, Section};
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
            Some(Arc::new(InMemorySource::new(
                SectionKind::Chat,
                section(2, &["#general"]),
            ))),
            Some(Arc::new(InMemorySource::down(SectionKind::Notifications))),
            Some(Arc::new(InMemorySource::new(
                SectionKind::Feed,
                Section::empty(),
            ))),
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
        assert!(
            html.contains("Notifications unavailable"),
            "down source is named in the river"
        );
        assert!(
            html.contains(r#"class="river""#),
            "single activity river rendered"
        );
        assert!(
            html.contains(r#"data-source="feed""#),
            "feed reach remains visible at zero"
        );
        assert!(html.contains("ops@w33d.xyz"), "signed-in email in app bar");
        // Live auto-refresh wiring: the poll targets + fetch of the JSON endpoint are present.
        assert!(
            html.contains(r#"id="summary-slot""#),
            "summary refresh slot present"
        );
        assert!(
            html.contains(r#"id="columns-slot""#),
            "columns refresh slot present"
        );
        assert!(
            html.contains("/api/inbox"),
            "poll fetches the JSON endpoint"
        );
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
        assert!(
            set_cookie.contains("SameSite=Strict"),
            "csrf cookie is strict"
        );
        let html = String::from_utf8(
            axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(
            html.contains(r#"class="searchbar""#),
            "unified search bar rendered"
        );
        assert!(html.contains(r#"name="source""#), "source filter present");
        assert!(
            html.contains(r#"data-action="dismiss""#),
            "row dismiss control present"
        );
        assert!(
            html.contains(r#"data-action="read""#),
            "row mark-read control present"
        );
        assert!(
            html.contains(r#"meta name="csrf-token""#),
            "csrf meta echoed into page"
        );
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
            axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(html.contains("#random"), "matching row kept");
        assert!(!html.contains("#general"), "non-matching row filtered out");
        // The query is echoed back into the search input, escaped.
        assert!(
            html.contains(r#"value="random""#),
            "query reflected in the search box"
        );
    }

    #[tokio::test]
    async fn dashboard_search_text_cannot_smuggle_template_slots() {
        let app = crate::app(crate::build_dev_state());
        for token in ["COLUMNS", "SUMMARY", "CSRF", "TOPBAR"] {
            let uri = format!("/?q=%7B%7B{token}%7D%7D");
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header("x-auth-subject", "u_1")
                        .header("x-auth-email", "ops@w33d.xyz")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let html = String::from_utf8(
                axum::body::to_bytes(res.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap();
            assert!(
                html.contains(&format!(r#"value="{{{{{token}}}}}""#)),
                "query token must remain inert text"
            );
            assert_eq!(html.matches(r#"class="river""#).count(), 1);
            assert_eq!(html.matches(r#"class="gauge""#).count(), 1);
            assert_eq!(html.matches(r#"id="summary-slot""#).count(), 1);
            assert_eq!(html.matches(r#"id="columns-slot""#).count(), 1);
        }
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

    #[test]
    fn river_merges_sources_stably_newest_first_and_none_last() {
        let row = |key: &str, at: Option<i64>| InboxRow {
            key: key.to_string(),
            title: key.to_string(),
            link: "/".to_string(),
            at,
            ..Default::default()
        };
        let inbox = Inbox {
            chat: SectionState::Ready(Section {
                total: 4,
                rows: vec![
                    row("chat-tie", Some(50)),
                    row("chat-none", None),
                    row("chat-old", Some(-1)),
                ],
            }),
            notifications: SectionState::Ready(Section {
                total: 2,
                rows: vec![row("notify-new", Some(200)), row("notify-tie", Some(50))],
            }),
            feed: SectionState::Ready(Section {
                total: 1,
                rows: vec![row("feed-mid", Some(100))],
            }),
        };
        let html = render_columns(&inbox, 300, "tok", &ViewFilter::default());
        let pos = |needle: &str| html.find(needle).expect("row rendered");
        assert!(pos("notify-new") < pos("feed-mid"));
        assert!(pos("feed-mid") < pos("chat-tie"));
        assert!(
            pos("chat-tie") < pos("notify-tie"),
            "equal timestamps stay stable"
        );
        assert!(pos("notify-tie") < pos("chat-old"));
        assert!(
            pos("chat-old") < pos("chat-none"),
            "Some timestamp stays before None"
        );
    }

    #[test]
    fn river_timestamp_comparator_keeps_exact_min_before_none() {
        let row = |key: &str, at: Option<i64>| InboxRow {
            key: key.to_string(),
            title: key.to_string(),
            link: "/".to_string(),
            at,
            ..Default::default()
        };
        let minimum = row("minimum", Some(i64::MIN));
        let absent = row("absent", None);
        assert_eq!(
            compare_river_rows(&minimum, &absent),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_river_rows(&absent, &minimum),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn fragments_have_no_slot_ids_and_one_programmatic_gauge_target() {
        let inbox = empty_inbox();
        let summary = render_summary(&inbox);
        let river = render_columns(&inbox, 0, "tok", &ViewFilter::default());
        let combined = format!("{summary}{river}");
        assert!(!combined.contains("summary-slot"));
        assert!(!combined.contains("columns-slot"));
        assert!(!combined.contains("river-live"));
        assert_eq!(summary.matches(r#"class="gauge""#).count(), 1);
        assert_eq!(summary.matches(r#"tabindex="-1""#).count(), 1);
        assert!(!summary.contains(r#"tabindex="0""#));
    }

    #[test]
    fn keyless_row_is_read_only() {
        let inbox = Inbox {
            chat: SectionState::Ready(Section {
                total: 1,
                rows: vec![InboxRow {
                    title: "Visible but unaddressable".into(),
                    link: "/".into(),
                    ..Default::default()
                }],
            }),
            notifications: SectionState::Ready(Section::empty()),
            feed: SectionState::Ready(Section::empty()),
        };
        let html = render_columns(&inbox, 0, "tok", &ViewFilter::default());
        assert!(html.contains("Visible but unaddressable"));
        assert!(!html.contains("<form"));
    }

    #[test]
    fn keyed_row_emits_both_native_forms_and_return_context() {
        let inbox = Inbox {
            chat: SectionState::Ready(section(2, &["room-1"])),
            notifications: SectionState::Ready(Section::empty()),
            feed: SectionState::Ready(Section::empty()),
        };
        let filter = ViewFilter::new(Some("urgent".into()), Some("chat".into()));
        let html = render_columns(&inbox, crate::now_secs(), "csrf-token", &filter);
        assert_eq!(html.matches(r#"class="river__actform""#).count(), 2);
        assert!(html.contains(r#"method="post" action="/api/inbox/read""#));
        assert!(html.contains(r#"method="post" action="/api/inbox/dismiss""#));
        for name in ["source", "key", "csrf", "return_q", "return_source"] {
            assert!(html.contains(&format!(r#"name="{name}""#)));
        }
        assert!(html.contains(r#"name="csrf" value="csrf-token""#));
        assert!(html.contains(r#"name="return_q" value="urgent""#));
        assert!(html.contains(r#"name="return_source" value="chat""#));
    }

    #[test]
    fn hostile_row_and_hidden_values_are_escaped() {
        let inbox = Inbox {
            chat: SectionState::Ready(Section {
                total: 1,
                rows: vec![InboxRow {
                    key: "k\"<bad>".into(),
                    title: "<script>alert(1)</script>".into(),
                    snippet: "\"><img src=x>".into(),
                    link: "/open?x=\"<".into(),
                    at: None,
                    count: Some(1),
                    source: String::new(),
                }],
            }),
            notifications: SectionState::Ready(Section::empty()),
            feed: SectionState::Ready(Section::empty()),
        };
        let filter = ViewFilter::new(Some("<query>\"".into()), None);
        let html = render_columns(&inbox, 0, "tok\"<", &filter);
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<img src=x>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("k&quot;&lt;bad&gt;"));
        assert!(html.contains("&lt;query&gt;&quot;"));
        assert!(html.contains("tok&quot;&lt;"));
    }

    #[test]
    fn row_links_allow_safe_classes_and_escape_the_attribute() {
        assert_eq!(
            safe_row_link("https://example.test/a?x=1&y=\"<"),
            "https://example.test/a?x=1&amp;y=&quot;&lt;"
        );
        assert_eq!(
            safe_row_link("HTTP://example.test/path"),
            "HTTP://example.test/path"
        );
        assert_eq!(
            safe_row_link("mailto:ops@example.test?subject=a&body=b"),
            "mailto:ops@example.test?subject=a&amp;body=b"
        );
        assert_eq!(safe_row_link("/open?x=\"<"), "/open?x=&quot;&lt;");
    }

    #[test]
    fn rendered_row_links_fail_closed_for_executable_or_ambiguous_urls() {
        for unsafe_link in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            " data:text/html,<script>alert(1)</script> ",
            "//evil.example/path",
            "example.test/path",
            "https://example.test/\r\nnext",
        ] {
            let inbox = Inbox {
                chat: SectionState::Ready(Section {
                    total: 1,
                    rows: vec![InboxRow {
                        key: "unsafe-link".into(),
                        title: "Unsafe link".into(),
                        link: unsafe_link.into(),
                        ..Default::default()
                    }],
                }),
                notifications: SectionState::Ready(Section::empty()),
                feed: SectionState::Ready(Section::empty()),
            };

            let html = render_columns(&inbox, 0, "tok", &ViewFilter::default());
            assert!(
                html.contains(r#"<a class="row__link" href="/">"#),
                "unsafe link must fail closed: {unsafe_link:?}"
            );
            assert!(!html.contains(unsafe_link));
        }
    }

    #[test]
    fn rendered_row_links_reject_backslash_normalization_bypasses() {
        for unsafe_link in [
            "/\\evil.example",
            "/\\/evil.example",
            "\\evil.example",
            "https://trusted.example/\\@evil.example",
        ] {
            let inbox = Inbox {
                chat: SectionState::Ready(Section {
                    total: 1,
                    rows: vec![InboxRow {
                        key: "backslash-link".into(),
                        title: "Backslash link".into(),
                        link: unsafe_link.into(),
                        ..Default::default()
                    }],
                }),
                notifications: SectionState::Ready(Section::empty()),
                feed: SectionState::Ready(Section::empty()),
            };

            let html = render_columns(&inbox, 0, "tok", &ViewFilter::default());
            assert!(
                html.contains(r#"<a class="row__link" href="/">"#),
                "backslash-normalized URL must fail closed: {unsafe_link:?}"
            );
            assert!(!html.contains(unsafe_link));
            assert!(!html.contains("evil.example"));
        }
    }

    #[test]
    fn partial_failure_and_source_redundancy_are_explicit() {
        let inbox = Inbox {
            chat: SectionState::Ready(section(2, &["#general"])),
            notifications: SectionState::Unavailable,
            feed: SectionState::Ready(Section::empty()),
        };
        let html = render_columns(&inbox, crate::now_secs(), "tok", &ViewFilter::default());
        assert!(html.contains(r#"class="river__degraded""#));
        assert!(html.contains(r#"data-source="notifications">Notifications unavailable"#));
        assert!(html.contains(r#"class="river__glyph" aria-hidden="true""#));
        assert!(html.contains(r#"class="river__source-word">Chat"#));
    }

    #[test]
    fn filtered_empty_state_is_distinct_from_caught_up() {
        let inbox = empty_inbox();
        let filtered = ViewFilter::new(Some("missing".into()), None);
        assert!(render_columns(&inbox, 0, "tok", &filtered).contains("No matching activity."));
        assert!(render_columns(&inbox, 0, "tok", &ViewFilter::default())
            .contains("You're all caught up."));
    }
}
