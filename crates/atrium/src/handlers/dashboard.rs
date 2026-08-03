//! The unified activity inbox: `GET /` renders the Dispatch Rotunda — one bounded view of the
//! viewer's unread activity across the estate.
//!
//! An annunciator panel records what this view holds from each office (Chat / Notifications /
//! Feeds); the sorting table lays out this view's dispatch slips ordered by the time each source
//! put on them. Native action forms keep the triage path usable without JavaScript; progressive
//! enhancement can still update it in place.
//!
//! Every source is best-effort: data comes from one cached (~10 s), concurrent federation of the
//! three source databases ([`crate::inbox`]). A source that errored is named in a notice slip
//! while available activity keeps flowing; the page never errors or hangs. The view is bounded
//! (per-source slice, per-viewer overlay, search/filter) and says so — it never claims source
//! reachability, coverage, or freshness. All federated text is HTML-escaped (untrusted
//! cross-service content).

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap};
use axum::response::{Html, IntoResponse, Response};

use crate::audit::AuditEvent;
use crate::auth;
use crate::config::SECTION_LIMIT;
use crate::csrf;
use crate::handlers::{app_css, esc, fmt_date, rel_time, topbar, truncate, InboxQuery};
use crate::inbox::{self, Inbox, SectionState, ViewFilter};
use crate::source::{InboxRow, SectionKind};
use crate::AppState;

const DASHBOARD_HTML: &str = include_str!("../../templates/dashboard.html");

/// `GET /` — the unified inbox. Renders for any request the gateway forwards; the viewer identity
/// (and the per-viewer federation scope) comes from the injected `X-Auth-Subject` / `X-Auth-Email`.
/// A request with no gateway identity gets the signed-out branch: no federation, no counts, no
/// forms — never a fake, signed-in-looking empty inbox.
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

    // No gateway identity -> nothing to scope a federation by; render the signed-out shell.
    let Some(sub) = subject else {
        let empty = empty_inbox();
        let view = inbox::view(&empty, &Default::default(), &filter);
        return page(
            render(&view, &email, &filter, &token, crate::now_secs(), false),
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
        render(&view, &email, &filter, &token, crate::now_secs(), true),
        &token,
    )
}

/// Wrap the rendered HTML in a response that (re-)plants the CSRF cookie on every page load and
/// marks the viewer-private page uncacheable beyond this response.
fn page(html: String, token: &str) -> Response {
    (
        [
            (header::SET_COOKIE, csrf::set_cookie(token)),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
        ],
        Html(html),
    )
        .into_response()
}

/// The empty inbox behind the signed-out branch (all sections empty + available). Shared with the
/// JSON poll endpoint so both surfaces render the same shell.
pub(crate) fn empty_inbox() -> Inbox {
    use crate::source::Section;
    Inbox {
        chat: SectionState::Ready(Section::empty()),
        notifications: SectionState::Ready(Section::empty()),
        feed: SectionState::Ready(Section::empty()),
    }
}

fn render(
    inbox: &Inbox,
    email: &str,
    filter: &ViewFilter,
    token: &str,
    now: i64,
    signed_in: bool,
) -> String {
    let csrf = esc(token);
    let bar = topbar("Inbox", email);
    // The signed-out branch renders no search/filter form at all (nothing personal to filter).
    let search = if signed_in {
        render_search(filter)
    } else {
        String::new()
    };
    let summary = render_summary(inbox, filter, signed_in);
    let columns = render_columns(inbox, now, token, filter, signed_in);
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

/// The dispatches-table fragment shared by the full page and JSON poll. Slot ids belong to the
/// page shell and are deliberately absent here. Ordering is over RETURNED rows only, by each
/// row's own source-provided timestamp — never a claim about the source as a whole.
fn compare_dispatch_rows(left: &InboxRow, right: &InboxRow) -> std::cmp::Ordering {
    match (left.at, right.at) {
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

pub(crate) fn render_columns(
    inbox: &Inbox,
    now: i64,
    token: &str,
    filter: &ViewFilter,
    signed_in: bool,
) -> String {
    if !signed_in {
        // Signed-out: a neutral statement, no forms, no counts, no fake empty inbox.
        return r#"<section class="dispatches" aria-label="Activity in this view"><div class="dispatches__empty">There is nothing personal to show on this page.</div></section>"#.to_string();
    }

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
    rows.sort_by(|(_, left), (_, right)| compare_dispatch_rows(left, right));

    let mut body = String::new();
    let unavailable = inbox.unavailable_kinds();
    if !unavailable.is_empty() {
        let details = unavailable
            .iter()
            .map(|kind| {
                format!(
                    r#"<span class="dispatches__notice-item" data-source="{slug}">{word} — unavailable in this view</span>"#,
                    slug = kind.slug(),
                    word = source_word(*kind),
                )
            })
            .collect::<String>();
        body.push_str(&format!(
            r#"<div class="dispatches__notice"><strong>This view does not include:</strong>{details}</div>"#
        ));
    }

    if rows.is_empty() {
        if filter.is_empty() {
            body.push_str(
                r#"<div class="dispatches__empty">No entries appear in this bounded Atrium view.</div>"#,
            );
        } else {
            body.push_str(
                r#"<div class="dispatches__empty">Nothing in this view matches the search or filter above. <a href="/">Clear</a></div>"#,
            );
        }
    } else {
        for (kind, row) in rows {
            body.push_str(&dispatch_slip(kind, row, now, token, filter));
        }
    }

    format!(r#"<section class="dispatches" aria-label="Activity in this view">{body}</section>"#)
}

/// The annunciator panel: this view's total plus a word/glyph/count channel for every office. It
/// is a programmatic refresh focus target, not an ordinary Tab stop. Counts are VIEW totals
/// (source bound + overlay + search + filter), never source-wide totals.
pub(crate) fn render_summary(inbox: &Inbox, filter: &ViewFilter, signed_in: bool) -> String {
    if !signed_in {
        return r#"<section class="annun is-signedout" tabindex="-1" aria-label="Inbox summary"><p class="annun__lead">No viewer on this request</p><p class="annun__qual">Atrium shows your own activity once the gateway signs you in.</p></section>"#.to_string();
    }

    let states = [
        (SectionKind::Chat, &inbox.chat),
        (SectionKind::Notifications, &inbox.notifications),
        (SectionKind::Feed, &inbox.feed),
    ];
    let unavailable = inbox.unavailable_kinds();
    let all_down = unavailable.len() == states.len();
    let grand = inbox.total_unread();
    let filtered = !filter.is_empty();

    let lead = if all_down {
        "No source is available in this view".to_string()
    } else if filtered {
        if grand == 0 {
            "No matches in this view".to_string()
        } else {
            format!("{grand} matching in this view")
        }
    } else if grand == 0 {
        "Nothing to show in this view".to_string()
    } else {
        format!("{grand} unread in this view")
    };

    let qualifier = if all_down {
        r#"<p class="annun__qual">This view does not include Chat, Notifications, or Feeds.</p>"#
            .to_string()
    } else if unavailable.is_empty() {
        String::new()
    } else {
        let words = unavailable
            .iter()
            .map(|kind| source_word(*kind))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"<p class="annun__qual">Not in this view: {} — unavailable.</p>"#,
            esc(&words)
        )
    };

    let mut bays = String::new();
    for (kind, state) in states {
        bays.push_str(&annun_bay(kind, state, filter));
    }

    format!(
        r#"<section class="annun" tabindex="-1" aria-label="Inbox summary"><p class="annun__lead">{lead}</p>{qualifier}<div class="annun__bays">{bays}</div></section>"#,
        lead = esc(&lead),
        qualifier = qualifier,
        bays = bays,
    )
}

/// One annunciator bay: office word + glyph + this view's count, or a plain-text state. An
/// unavailable office is named before any filter blanking (the engine passes `Unavailable`
/// through untouched); a filtered-out office says "outside this filter", never "unavailable".
fn annun_bay(kind: SectionKind, state: &SectionState, filter: &ViewFilter) -> String {
    let (class, value) = match state {
        SectionState::Unavailable => (
            "annun__bay is-unavailable",
            "— unavailable in this view".to_string(),
        ),
        SectionState::Ready(section) => {
            if filter.source.map(|only| only != kind).unwrap_or(false) {
                ("annun__bay is-outside", "— outside this filter".to_string())
            } else {
                ("annun__bay", section.total.to_string())
            }
        }
    };
    format!(
        r#"<span class="{class}" data-source="{slug}"><span class="annun__glyph" aria-hidden="true">{glyph}</span><span class="annun__word">{word}</span><span class="annun__count">{value}</span></span>"#,
        class = class,
        slug = kind.slug(),
        glyph = source_glyph(kind),
        word = source_word(kind),
        value = esc(&value),
    )
}

fn dispatch_slip(
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
                format!(r#"<span class="slip__badge">{count}</span>"#)
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
        format!(r#"<div class="slip__snippet">{}</div>"#, esc(&snippet))
    };
    let origin = if kind == SectionKind::Feed {
        row.source.trim()
    } else {
        ""
    };
    // A row's time describes that row's own source stamp only. A future/skewed stamp is shown as
    // the absolute UTC date it carries — "just now" would falsely imply the source is recent.
    let when = row
        .at
        .map(|at| {
            if at > now {
                fmt_date(at)
            } else {
                rel_time(at, now)
            }
        })
        .unwrap_or_default();
    let meta = match (origin.is_empty(), when.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!(r#"<div class="slip__meta">{}</div>"#, esc(origin)),
        (true, false) => format!(r#"<div class="slip__meta">{}</div>"#, esc(&when)),
        (false, false) => format!(
            r#"<div class="slip__meta">{} · {}</div>"#,
            esc(origin),
            esc(&when)
        ),
    };
    let link = safe_row_link(&row.link);

    format!(
        r#"<div class="slip" data-source="{slug}" data-key="{key}">
  <span class="slip__stamp" data-source="{slug}"><span class="slip__glyph" aria-hidden="true">{glyph}</span><span class="slip__word">{word}</span></span>
  <a class="slip__link" href="{link}"><div class="slip__top"><span class="slip__title">{title}</span>{badge}</div>{snippet}</a>
  {meta}
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
        actions = dispatch_actions(kind, &row.key, token, filter),
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

fn dispatch_actions(kind: SectionKind, key: &str, token: &str, filter: &ViewFilter) -> String {
    if key.trim().is_empty() {
        return String::new();
    }
    format!(
        r#"<div class="slip__actions">{}{}</div>"#,
        action_form("read", "Mark seen", "slip__act", kind, key, token, filter),
        action_form(
            "dismiss",
            "Dismiss",
            "slip__act slip__act--dismiss",
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
    // Both buttons point at the persistent disclosure block so "hides from this Atrium view only"
    // is programmatically linked from the control itself.
    format!(
        r#"<form class="slip__form" method="post" action="/api/inbox/{action}"><input type="hidden" name="source" value="{source}"><input type="hidden" name="key" value="{key}"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="return_q" value="{return_q}"><input type="hidden" name="return_source" value="{return_source}"><button type="submit" class="{button_class}" data-action="{action}" aria-describedby="viewtruth">{label}</button></form>"#,
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

    async fn body_string(res: Response) -> String {
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn dashboard_renders_dispatches_annunciator_and_resilience() {
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
        assert_eq!(
            res.headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default(),
            "private, no-store",
            "viewer-private page carries the frozen cache suppression"
        );
        let html = body_string(res).await;
        assert!(html.contains("#general"), "chat slip rendered");
        assert!(
            html.contains("Notifications — unavailable in this view"),
            "down office is named in the notice slip"
        );
        assert!(
            html.contains(r#"class="dispatches""#),
            "single dispatches table rendered"
        );
        assert!(
            html.contains(r#"aria-label="Activity in this view""#),
            "table is labelled as a bounded view"
        );
        assert!(
            html.contains(r#"aria-label="Inbox summary""#),
            "annunciator is labelled as a summary"
        );
        assert!(
            html.contains(r#"data-source="feed""#),
            "feed bay remains visible at zero"
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
        assert!(
            html.contains("What this view is"),
            "persistent disclosure block present"
        );
        assert!(
            html.contains(r#"id="refresh-status""#),
            "browser-side status line present"
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
        let html = body_string(res).await;
        assert!(
            html.contains(r#"class="searchbar""#),
            "unified search bar rendered"
        );
        assert!(html.contains(r#"name="source""#), "source filter present");
        assert!(
            html.contains(r#"data-action="dismiss""#),
            "slip dismiss control present"
        );
        assert!(
            html.contains(r#"data-action="read""#),
            "slip mark-seen control present"
        );
        assert!(html.contains("Mark seen"), "frozen verb on the read action");
        assert!(
            html.contains(r#"aria-describedby="viewtruth""#),
            "actions point at the persistent disclosure"
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
        let html = body_string(res).await;
        assert!(html.contains("#random"), "matching slip kept");
        assert!(!html.contains("#general"), "non-matching slip filtered out");
        // The query is echoed back into the search input, escaped.
        assert!(
            html.contains(r#"value="random""#),
            "query reflected in the search box"
        );
        assert!(
            html.contains("matching in this view"),
            "filtered lead names the bounded view"
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
            let html = body_string(res).await;
            assert!(
                html.contains(&format!(r#"value="{{{{{token}}}}}""#)),
                "query token must remain inert text"
            );
            assert_eq!(html.matches(r#"class="dispatches""#).count(), 1);
            assert_eq!(html.matches(r#"class="annun""#).count(), 1);
            assert_eq!(html.matches(r#"id="summary-slot""#).count(), 1);
            assert_eq!(html.matches(r#"id="columns-slot""#).count(), 1);
        }
    }

    #[tokio::test]
    async fn dashboard_without_identity_renders_signed_out_branch() {
        let app = crate::app(crate::build_dev_state());
        let res = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default(),
            "private, no-store",
            "signed-out page also carries the frozen cache suppression"
        );
        let html = body_string(res).await;
        assert!(
            html.contains("No viewer on this request"),
            "signed-out annunciator lead"
        );
        assert!(
            html.contains("There is nothing personal to show on this page."),
            "signed-out table copy"
        );
        let main = html.split("<main").nth(1).unwrap_or_default();
        assert!(!main.contains("<form"), "signed-out view renders no forms");
        assert!(
            !main.contains(r#"class="searchbar""#),
            "signed-out view renders no search bar"
        );
        // Scope to the body (not the whole page): the inlined stylesheet in <head> legitimately
        // defines `.annun__count` for the signed-in view; what must be absent is a rendered count.
        assert!(
            !main.contains("annun__count"),
            "signed-out view renders no counts"
        );
    }

    #[test]
    fn dispatches_merge_sources_stably_by_row_time_and_none_last() {
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
        let html = render_columns(&inbox, 300, "tok", &ViewFilter::default(), true);
        let pos = |needle: &str| html.find(needle).expect("slip rendered");
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
    fn dispatch_timestamp_comparator_keeps_exact_min_before_none() {
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
            compare_dispatch_rows(&minimum, &absent),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_dispatch_rows(&absent, &minimum),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn future_source_time_renders_absolute_date_never_just_now() {
        let now = 1_700_000_000;
        let future = now + 3600;
        let inbox = Inbox {
            chat: SectionState::Ready(Section {
                total: 1,
                rows: vec![InboxRow {
                    key: "future".into(),
                    title: "Skewed clock row".into(),
                    link: "/".into(),
                    at: Some(future),
                    ..Default::default()
                }],
            }),
            notifications: SectionState::Ready(Section {
                total: 1,
                rows: vec![InboxRow {
                    key: "present".into(),
                    title: "Present clock row".into(),
                    link: "/".into(),
                    at: Some(now),
                    ..Default::default()
                }],
            }),
            feed: SectionState::Ready(Section::empty()),
        };
        let html = render_columns(&inbox, now, "tok", &ViewFilter::default(), true);
        let future_pos = html.find("Skewed clock row").expect("future slip");
        let present_pos = html.find("Present clock row").expect("present slip");
        assert!(future_pos < present_pos, "future stamp still sorts first");
        let slip_end = html[future_pos..].find("</div>").unwrap_or(0) + future_pos;
        let slip_region =
            &html[future_pos..slip_end.max(future_pos) + 400.min(html.len() - future_pos)];
        assert!(
            !slip_region.contains("just now"),
            "a future stamp never reads as recent: {slip_region}"
        );
        assert!(
            html.contains(&fmt_date(future)),
            "future stamp shows its absolute UTC date"
        );
    }

    #[test]
    fn fragments_have_no_slot_ids_and_one_programmatic_annun_target() {
        let inbox = empty_inbox();
        let summary = render_summary(&inbox, &ViewFilter::default(), true);
        let dispatches = render_columns(&inbox, 0, "tok", &ViewFilter::default(), true);
        let combined = format!("{summary}{dispatches}");
        assert!(!combined.contains("summary-slot"));
        assert!(!combined.contains("columns-slot"));
        assert!(
            !combined.contains("aria-live"),
            "a swapped fragment announces nothing on its own"
        );
        assert_eq!(summary.matches(r#"class="annun""#).count(), 1);
        assert_eq!(summary.matches(r#"tabindex="-1""#).count(), 1);
        assert!(!summary.contains(r#"tabindex="0""#));
    }

    #[test]
    fn signed_out_fragments_are_neutral_and_formless() {
        let inbox = empty_inbox();
        let summary = render_summary(&inbox, &ViewFilter::default(), false);
        let dispatches = render_columns(&inbox, 0, "tok", &ViewFilter::default(), false);
        assert!(summary.contains("No viewer on this request"));
        assert!(summary.contains("is-signedout"));
        assert!(!summary.contains("annun__bay"));
        assert!(dispatches.contains("There is nothing personal to show on this page."));
        assert!(!dispatches.contains("<form"));
    }

    #[test]
    fn annunciator_states_follow_the_frozen_copy_table() {
        // Neutral zero: no rows, nothing unavailable, no filter.
        let zero = render_summary(&empty_inbox(), &ViewFilter::default(), true);
        assert!(zero.contains("Nothing to show in this view"));
        assert!(zero.contains(r#"data-source="chat""#));
        // Filtered zero.
        let filtered = ViewFilter::new(Some("missing".into()), None);
        let fzero = render_summary(&empty_inbox(), &filtered, true);
        assert!(fzero.contains("No matches in this view"));
        // All unavailable: never a "nothing unread"-style lead.
        let down = Inbox {
            chat: SectionState::Unavailable,
            notifications: SectionState::Unavailable,
            feed: SectionState::Unavailable,
        };
        let all = render_summary(&down, &ViewFilter::default(), true);
        assert!(all.contains("No source is available in this view"));
        assert!(all.contains("This view does not include Chat, Notifications, or Feeds."));
        assert!(!all.contains("Nothing"));
        assert_eq!(all.matches("unavailable in this view").count(), 3);
        // Source filter: excluded offices say "outside this filter", never "unavailable".
        let inbox = Inbox {
            chat: SectionState::Ready(section(2, &["#general"])),
            notifications: SectionState::Unavailable,
            feed: SectionState::Ready(Section::empty()),
        };
        let only_chat = ViewFilter::new(None, Some("chat".into()));
        let scoped = render_summary(&inbox, &only_chat, true);
        assert!(scoped.contains("— outside this filter"));
        assert!(!scoped.contains("Notifications — unavailable in this view"));
        // The unavailable office is still named as unavailable under a source filter (E7).
        assert!(scoped.contains(r#"data-source="notifications""#));
        assert!(scoped.contains("— unavailable in this view"));
    }

    #[test]
    fn keyless_slip_is_read_only() {
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
        let html = render_columns(&inbox, 0, "tok", &ViewFilter::default(), true);
        assert!(html.contains("Visible but unaddressable"));
        assert!(!html.contains("<form"));
    }

    #[test]
    fn keyed_slip_emits_both_native_forms_and_return_context() {
        let inbox = Inbox {
            chat: SectionState::Ready(section(2, &["room-1"])),
            notifications: SectionState::Ready(Section::empty()),
            feed: SectionState::Ready(Section::empty()),
        };
        let filter = ViewFilter::new(Some("urgent".into()), Some("chat".into()));
        let html = render_columns(&inbox, crate::now_secs(), "csrf-token", &filter, true);
        assert_eq!(html.matches(r#"class="slip__form""#).count(), 2);
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
    fn hostile_slip_and_hidden_values_are_escaped() {
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
        let html = render_columns(&inbox, 0, "tok\"<", &filter, true);
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<img src=x>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("k&quot;&lt;bad&gt;"));
        assert!(html.contains("&lt;query&gt;&quot;"));
        assert!(html.contains("tok&quot;&lt;"));
    }

    #[test]
    fn slip_links_allow_safe_classes_and_escape_the_attribute() {
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
    fn rendered_slip_links_fail_closed_for_executable_or_ambiguous_urls() {
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

            let html = render_columns(&inbox, 0, "tok", &ViewFilter::default(), true);
            assert!(
                html.contains(r#"<a class="slip__link" href="/">"#),
                "unsafe link must fail closed: {unsafe_link:?}"
            );
            assert!(!html.contains(unsafe_link));
        }
    }

    #[test]
    fn rendered_slip_links_reject_backslash_normalization_bypasses() {
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

            let html = render_columns(&inbox, 0, "tok", &ViewFilter::default(), true);
            assert!(
                html.contains(r#"<a class="slip__link" href="/">"#),
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
        let html = render_columns(
            &inbox,
            crate::now_secs(),
            "tok",
            &ViewFilter::default(),
            true,
        );
        assert!(html.contains(r#"class="dispatches__notice""#));
        assert!(html.contains("This view does not include:"));
        assert!(html
            .contains(r#"data-source="notifications">Notifications — unavailable in this view"#));
        assert!(html.contains(r#"class="slip__glyph" aria-hidden="true""#));
        assert!(html.contains(r#"class="slip__word">Chat"#));
    }

    #[test]
    fn filtered_empty_state_is_distinct_from_neutral_zero() {
        let inbox = empty_inbox();
        let filtered = ViewFilter::new(Some("missing".into()), None);
        let filtered_html = render_columns(&inbox, 0, "tok", &filtered, true);
        assert!(filtered_html.contains("Nothing in this view matches the search or filter above."));
        assert!(filtered_html.contains(r#"<a href="/">Clear</a>"#));
        assert!(
            render_columns(&inbox, 0, "tok", &ViewFilter::default(), true)
                .contains("No entries appear in this bounded Atrium view.")
        );
    }

    #[test]
    fn enhancement_locks_exact_url_integer_payload_and_menu_state_contracts() {
        assert!(DASHBOARD_HTML.contains("var request = {"));
        assert!(DASHBOARD_HTML.contains("gen: ++readGen"));
        assert!(DASHBOARD_HTML.contains("sentUrl: window.location.href"));
        assert!(DASHBOARD_HTML.contains("controlsSnapshot: currentControls()"));
        assert!(DASHBOARD_HTML.contains("window.location.href === request.sentUrl"));
        assert!(
            DASHBOARD_HTML.contains("controlsEqual(currentControls(), request.controlsSnapshot)")
        );
        assert!(DASHBOARD_HTML.contains("Number.isFinite(d.total_unread)"));
        assert!(DASHBOARD_HTML.contains("Number.isInteger(d.total_unread)"));
        assert!(DASHBOARD_HTML.contains("userMenuButton.setAttribute("));
        assert!(DASHBOARD_HTML.contains("'aria-expanded'"));
    }

    #[test]
    fn service_layer_freezes_a_forty_four_pixel_target_floor() {
        let css = crate::handlers::SERVICE_CSS;
        assert!(css.contains("--atrium-target: 44px"));
        for selector in [
            ".appbar__brand",
            ".appnav",
            ".iconbtn",
            ".usermenu__btn",
            ".menuitem",
            ".searchbar__clear",
            ".slip__link",
            ".slip__act",
            ".quicklinks__link",
        ] {
            assert!(
                css.contains(selector),
                "missing target-floor selector {selector}"
            );
        }
    }
}
