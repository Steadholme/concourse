//! The SSO notification inbox: list, mark-read, Web Push subscribe/unsubscribe, the self test
//! notification, and the live SSE stream.
//!
//! Every endpoint here is mounted behind the Sluice `auth=sso` route: the viewer identity is
//! ALWAYS taken from the injected `X-Auth-Subject` / `X-Auth-Email` (never a client field), and the
//! inbox is scoped to that identity. State-changing POSTs (`/api/read`, `/api/subscribe`,
//! `/api/unsubscribe`, `/api/test`) are double-submit CSRF protected.

use std::collections::{BTreeSet, HashSet};
use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::{Form, Json};
use serde::Deserialize;

use crate::audit::AuditEvent;
use crate::auth;
use crate::delivery;
use crate::error::AppError;
use crate::handlers::{app_css, esc, fmt_datetime, safe_url, topbar};
use crate::store::{InboxListSnapshot, Notification, NotifyPrefs, PushSubscription, StoreError};
use crate::{new_id, now_secs, AppState};

const DASHBOARD_HTML: &str = include_str!("../../templates/dashboard.html");
const SERVICE_WORKER_JS: &str = include_str!("../../static/sw.js");

/// Mark-read form: an optional `id` (mark one) or none (mark all unread), plus the CSRF token.
#[derive(Debug, Deserialize)]
pub struct ReadForm {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub csrf_token: String,
}

/// Web Push subscription form (posted by the page's registration JS).
#[derive(Debug, Deserialize)]
pub struct SubscribeForm {
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub p256dh: String,
    #[serde(default)]
    pub auth: String,
    #[serde(default)]
    pub csrf_token: String,
}

/// Web Push unsubscribe form (posted by the page's registration JS): the subscription `endpoint`
/// to remove, plus the CSRF token.
#[derive(Debug, Deserialize)]
pub struct UnsubscribeForm {
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub csrf_token: String,
}

/// Test-notification form: just the CSRF token (the recipient is ALWAYS the signed-in user).
#[derive(Debug, Deserialize)]
pub struct TestForm {
    #[serde(default)]
    pub csrf_token: String,
}

/// Server-owned Signal Muster filters. An absent `source` means All; any present value is exact,
/// including the empty string and the literal string `all`.
#[derive(Debug, Default, Deserialize)]
pub struct InboxQuery {
    #[serde(default)]
    grade: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GradeFilter {
    All,
    Info,
    Warning,
    Critical,
    Other,
}

impl GradeFilter {
    fn from_query(value: Option<&str>) -> Self {
        match value {
            Some("info") => Self::Info,
            Some("warning") => Self::Warning,
            Some("critical") => Self::Critical,
            Some("other") => Self::Other,
            _ => Self::All,
        }
    }

    fn query_value(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Info => Some("info"),
            Self::Warning => Some("warning"),
            Self::Critical => Some("critical"),
            Self::Other => Some("other"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "ALL",
            Self::Info => "INFO",
            Self::Warning => "WARNING",
            Self::Critical => "CRITICAL",
            Self::Other => "OTHER",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StateFilter {
    All,
    Unread,
}

impl StateFilter {
    fn from_query(value: Option<&str>) -> Self {
        match value {
            Some("unread") => Self::Unread,
            _ => Self::All,
        }
    }

    fn query_value(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Unread => Some("unread"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "ALL",
            Self::Unread => "UNREAD",
        }
    }
}

#[derive(Clone, Debug)]
struct MusterFilters {
    grade: GradeFilter,
    state: StateFilter,
    source: Option<String>,
}

impl MusterFilters {
    fn from_query(query: InboxQuery) -> Self {
        Self {
            grade: GradeFilter::from_query(query.grade.as_deref()),
            state: StateFilter::from_query(query.state.as_deref()),
            source: query.source,
        }
    }

    fn is_active(&self) -> bool {
        self.grade != GradeFilter::All || self.state != StateFilter::All || self.source.is_some()
    }
}

// ---------------------------------------------------------------------------
// GET / — the inbox
// ---------------------------------------------------------------------------

/// `GET /` — this user's checked, bounded Signal Muster + independent delivery governance.
pub async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<InboxQuery>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    let email = auth::display_email(&headers);
    let (csrf, set_cookie) = auth::ensure_csrf(&headers);
    let filters = MusterFilters::from_query(query);

    let (list_read, prefs_read) = tokio::join!(
        state.store.list_notifications_checked(&keys),
        state.store.get_prefs_checked(&keys[0])
    );
    if list_read.is_err() {
        tracing::error!(
            read_kind = "notification-list",
            error_class = "backend-read-failed",
            "checked store read failed"
        );
    }
    if prefs_read.is_err() {
        tracing::error!(
            read_kind = "owner-preferences",
            error_class = "backend-read-failed",
            "checked store read failed"
        );
    }

    let snapshot = list_read.as_ref().ok();
    let filtered = snapshot
        .map(|value| filtered_notifications(&value.notifications, &filters))
        .unwrap_or_default();
    let filtered_unread = filtered.iter().filter(|n| n.read_at == 0).count();
    let unread_fragment = if filtered.is_empty() {
        String::new()
    } else {
        format!(r#"<span class="count" data-zero="0">{filtered_unread}</span>"#)
    };
    let mark_all = snapshot
        .map(|value| render_mark_all(value, &csrf))
        .unwrap_or_default();
    let severity_legend = snapshot
        .map(|value| render_summary(value, &filtered))
        .unwrap_or_default();
    let governance_rail = render_governance(&prefs_read, now_secs().rem_euclid(86_400) / 60);
    let filter_rail = snapshot
        .map(|value| render_filter_rail(&value.notifications, &filters))
        .unwrap_or_default();
    let items = match snapshot {
        Some(value) => render_muster_body(value, &filtered, &filters, &csrf),
        None => render_list_unavailable(),
    };

    let push_state = if state.config.push_configured() {
        r#"<span class="signal-capability signal-capability--ready">Configured</span>"#
    } else {
        r#"<span class="signal-capability signal-capability--absent">Not configured</span>"#
    };
    let vapid_public = esc(&state.config.vapid_public_key.clone().unwrap_or_default());
    let csrf_escaped = esc(&csrf);
    let topbar_html = topbar("Notifications", &email);
    let page = render_template_once(
        DASHBOARD_HTML,
        &[
            ("CSS", app_css()),
            ("TOPBAR", &topbar_html),
            ("UNREAD", &unread_fragment),
            ("MARK_ALL", &mark_all),
            ("PUSH_STATE", push_state),
            ("CSRF", &csrf_escaped),
            ("VAPID_PUBLIC", &vapid_public),
            ("ITEMS", &items),
            ("GOVERNANCE_RAIL", &governance_rail),
            ("SEVERITY_LEGEND", &severity_legend),
            ("FILTER_RAIL", &filter_rail),
        ],
    );

    Ok(html_with_cookie(page, set_cookie))
}

/// Replace known markers while scanning only the original template. Inserted values are never
/// scanned, so placeholder-shaped producer text stays escaped and literal.
fn render_template_once(template: &str, bindings: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 4096);
    let mut cursor = 0usize;
    while let Some(relative_start) = template[cursor..].find("{{") {
        let start = cursor + relative_start;
        out.push_str(&template[cursor..start]);
        let Some(relative_end) = template[start + 2..].find("}}") else {
            out.push_str(&template[start..]);
            return out;
        };
        let end = start + 2 + relative_end;
        let name = &template[start + 2..end];
        if let Some((_, value)) = bindings.iter().find(|(key, _)| *key == name) {
            out.push_str(value);
        } else {
            out.push_str(&template[start..end + 2]);
        }
        cursor = end + 2;
    }
    out.push_str(&template[cursor..]);
    out
}

fn notification_grade(n: &Notification) -> GradeFilter {
    match n.severity.as_str() {
        "info" => GradeFilter::Info,
        "warning" => GradeFilter::Warning,
        "critical" => GradeFilter::Critical,
        _ => GradeFilter::Other,
    }
}

fn matches_grade(n: &Notification, grade: GradeFilter) -> bool {
    grade == GradeFilter::All || notification_grade(n) == grade
}

fn matches_state(n: &Notification, state: StateFilter) -> bool {
    state == StateFilter::All || n.read_at == 0
}

fn matches_source(n: &Notification, source: Option<&str>) -> bool {
    source.is_none_or(|value| n.source == value)
}

fn filtered_notifications<'a>(
    notifications: &'a [Notification],
    filters: &MusterFilters,
) -> Vec<&'a Notification> {
    notifications
        .iter()
        .filter(|n| {
            matches_grade(n, filters.grade)
                && matches_state(n, filters.state)
                && matches_source(n, filters.source.as_deref())
        })
        .collect()
}

fn render_summary(snapshot: &InboxListSnapshot, filtered: &[&Notification]) -> String {
    if filtered.is_empty() {
        return String::new();
    }
    let unread = filtered.iter().filter(|n| n.read_at == 0).count();
    let warning = filtered
        .iter()
        .filter(|n| notification_grade(n) == GradeFilter::Warning)
        .count();
    let critical = filtered
        .iter()
        .filter(|n| notification_grade(n) == GradeFilter::Critical)
        .count();
    format!(
        r#"<section class="muster-summary" aria-labelledby="muster-summary-title">
  <div class="muster-summary__register"><span aria-hidden="true">§</span><h2 id="muster-summary-title">Bounded register</h2></div>
  <p class="muster-window">{window}</p>
  <ul class="muster-summary__facts">
    <li><span class="muster-summary__value">{visible}</span><span>visible signals</span></li>
    <li><span class="muster-summary__value">{unread}</span><span>unread here</span></li>
    <li><span class="muster-summary__value">{warning}</span><span>warning</span></li>
    <li><span class="muster-summary__value">{critical}</span><span>critical</span></li>
  </ul>
</section>"#,
        window = window_copy(snapshot),
        visible = filtered.len(),
        unread = unread,
        warning = warning,
        critical = critical,
    )
}

fn window_copy(snapshot: &InboxListSnapshot) -> String {
    if snapshot.truncated {
        "Showing up to 200 signals from this bounded view — unread first, newest within each read-state group; additional signals may be outside this view.".to_string()
    } else {
        format!(
            "Bounded view contains {} signal{} — unread first, newest within each read-state group.",
            snapshot.notifications.len(),
            if snapshot.notifications.len() == 1 { "" } else { "s" }
        )
    }
}

fn render_mark_all(snapshot: &InboxListSnapshot, csrf: &str) -> String {
    if !snapshot.notifications.iter().any(|n| n.read_at == 0) {
        return String::new();
    }
    format!(
        r#"<div class="muster-bulk">
  <form class="inline-form" method="post" action="/api/read">
    <input type="hidden" name="csrf_token" value="{csrf}">
    <button class="btn btn-secondary btn-sm" type="submit">Mark every unread signal</button>
  </form>
  <span class="muster-bulk__scope">Includes owner-scoped signals outside this bounded view.</span>
</div>"#,
        csrf = esc(csrf)
    )
}

fn bounded_rule_count(value: usize) -> String {
    if value > 99 {
        "99+".to_string()
    } else {
        value.to_string()
    }
}

fn render_governance(prefs_read: &Result<NotifyPrefs, StoreError>, minute: i64) -> String {
    let Ok(prefs) = prefs_read else {
        return r#"<section class="governance-rail governance-rail--unavailable" data-governance="unavailable" aria-labelledby="governance-title">
  <div><span class="governance-rail__kicker">Governance</span><h2 id="governance-title">Delivery settings unavailable</h2></div>
  <p>Klaxon could not verify delivery governance. Retry before changing delivery assumptions.</p>
  <a class="governance-rail__link" href="/settings/prefs">Open delivery preferences</a>
</section>"#
            .to_string();
    };

    let quiet_configured =
        prefs.quiet_start >= 0 && prefs.quiet_end >= 0 && prefs.quiet_start != prefs.quiet_end;
    let quiet_active = quiet_configured && prefs.in_quiet_hours(minute);
    let selective = !prefs.muted_sources.is_empty() || !prefs.muted_severities.is_empty();
    let (state_class, state_label) = if prefs.mute_all || prefs.digest || quiet_active {
        ("global", "Global pause")
    } else if selective {
        ("selective", "Selective rules")
    } else {
        ("clear", "No active delivery rule")
    };

    let mut categories = Vec::new();
    if prefs.mute_all {
        categories.push("all real-time channels paused");
    }
    if prefs.digest {
        categories.push("digest collection active");
    }
    if quiet_active {
        categories.push("quiet interval active now");
    } else if quiet_configured {
        categories.push("quiet interval scheduled / inactive");
    }
    if selective {
        categories.push("selective vocabulary configured");
    }
    if categories.is_empty() {
        categories.push("no suppression category active");
    }
    let category_items = categories
        .iter()
        .map(|value| format!("<li>{value}</li>"))
        .collect::<String>();

    format!(
        r#"<section class="governance-rail governance-rail--{state_class}" data-governance="{state_class}" aria-labelledby="governance-title">
  <div class="governance-rail__heading"><span class="governance-rail__kicker">Governance</span><h2 id="governance-title">{state_label}</h2></div>
  <ul class="governance-rail__categories">{category_items}</ul>
  <dl class="governance-rail__counts">
    <div><dt>Source rules</dt><dd>{source_count}</dd></div>
    <div><dt>Grade rules</dt><dd>{severity_count}</dd></div>
  </dl>
  <p class="governance-rail__retention">Inbox copies remain retained regardless of real-time delivery rules.</p>
  <a class="governance-rail__link" href="/settings/prefs">Open delivery preferences</a>
</section>"#,
        state_class = state_class,
        state_label = state_label,
        category_items = category_items,
        source_count = bounded_rule_count(prefs.muted_sources.len()),
        severity_count = bounded_rule_count(prefs.muted_severities.len()),
    )
}

fn percent_encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn filter_href(filters: &MusterFilters) -> String {
    let mut pairs = Vec::new();
    if let Some(grade) = filters.grade.query_value() {
        pairs.push(format!("grade={}", percent_encode_query(grade)));
    }
    if let Some(state) = filters.state.query_value() {
        pairs.push(format!("state={}", percent_encode_query(state)));
    }
    if let Some(source) = filters.source.as_deref() {
        pairs.push(format!("source={}", percent_encode_query(source)));
    }
    let raw = if pairs.is_empty() {
        "/".to_string()
    } else {
        format!("/?{}", pairs.join("&"))
    };
    esc(&raw)
}

fn source_label(source: &str) -> &str {
    if source.is_empty() {
        "EMPTY SOURCE"
    } else {
        source
    }
}

fn render_filter_link(label: &str, count: usize, current: bool, next: &MusterFilters) -> String {
    let current_attr = if current {
        r#" aria-current="true""#
    } else {
        ""
    };
    format!(
        r#"<li><a class="muster-filter{active}" href="{href}"{current}><span>{label}</span><span class="muster-filter__count" aria-hidden="true">{count}</span></a></li>"#,
        active = if current { " is-current" } else { "" },
        href = filter_href(next),
        current = current_attr,
        label = esc(label),
        count = count,
    )
}

fn render_filter_rail(notifications: &[Notification], filters: &MusterFilters) -> String {
    let mut grade_links = String::new();
    for grade in [
        GradeFilter::All,
        GradeFilter::Info,
        GradeFilter::Warning,
        GradeFilter::Critical,
        GradeFilter::Other,
    ] {
        let count = notifications
            .iter()
            .filter(|n| {
                matches_grade(n, grade)
                    && matches_state(n, filters.state)
                    && matches_source(n, filters.source.as_deref())
            })
            .count();
        let mut next = filters.clone();
        next.grade = grade;
        grade_links.push_str(&render_filter_link(
            grade.label(),
            count,
            filters.grade == grade,
            &next,
        ));
    }

    let mut state_links = String::new();
    for state in [StateFilter::All, StateFilter::Unread] {
        let count = notifications
            .iter()
            .filter(|n| {
                matches_grade(n, filters.grade)
                    && matches_state(n, state)
                    && matches_source(n, filters.source.as_deref())
            })
            .count();
        let mut next = filters.clone();
        next.state = state;
        state_links.push_str(&render_filter_link(
            state.label(),
            count,
            filters.state == state,
            &next,
        ));
    }

    let mut source_values = notifications
        .iter()
        .map(|n| n.source.clone())
        .collect::<BTreeSet<_>>();
    if let Some(source) = filters.source.as_ref() {
        source_values.insert(source.clone());
    }
    let all_source_count = notifications
        .iter()
        .filter(|n| matches_grade(n, filters.grade) && matches_state(n, filters.state))
        .count();
    let mut source_links = render_filter_link(
        "ALL SOURCES",
        all_source_count,
        filters.source.is_none(),
        &MusterFilters {
            grade: filters.grade,
            state: filters.state,
            source: None,
        },
    );
    for source in source_values {
        let count = notifications
            .iter()
            .filter(|n| {
                matches_grade(n, filters.grade)
                    && matches_state(n, filters.state)
                    && n.source == source
            })
            .count();
        let next = MusterFilters {
            grade: filters.grade,
            state: filters.state,
            source: Some(source.clone()),
        };
        source_links.push_str(&render_filter_link(
            source_label(&source),
            count,
            filters.source.as_deref() == Some(source.as_str()),
            &next,
        ));
    }

    format!(
        r#"<nav class="muster-filters" aria-label="Filter signal muster">
  <div class="muster-filter-group"><span class="muster-filter-group__label" id="filter-grade-label">Grade</span><ul aria-labelledby="filter-grade-label">{grade_links}</ul></div>
  <div class="muster-filter-group"><span class="muster-filter-group__label" id="filter-state-label">State</span><ul aria-labelledby="filter-state-label">{state_links}</ul></div>
  <div class="muster-filter-group"><span class="muster-filter-group__label" id="filter-source-label">Source</span><ul aria-labelledby="filter-source-label">{source_links}</ul></div>
</nav>"#,
        grade_links = grade_links,
        state_links = state_links,
        source_links = source_links,
    )
}

fn render_muster_body(
    snapshot: &InboxListSnapshot,
    filtered: &[&Notification],
    filters: &MusterFilters,
    csrf: &str,
) -> String {
    if !filtered.is_empty() {
        let rows = filtered
            .iter()
            .enumerate()
            .map(|(index, n)| render_item(n, csrf, index + 1))
            .collect::<String>();
        return format!(r#"<ol class="muster-list" aria-label="Signals">{rows}</ol>"#);
    }
    if snapshot.notifications.is_empty() && !filters.is_active() {
        return format!(
            r#"<section class="muster-state muster-state--clear" aria-labelledby="muster-clear-title">
  <span class="muster-state__mark" aria-hidden="true">○</span>
  <div><h2 id="muster-clear-title">Muster clear</h2><p>{window}</p><p>New estate signals will enter this bounded register.</p></div>
</section>"#,
            window = window_copy(snapshot)
        );
    }
    let source = filters
        .source
        .as_deref()
        .map(source_label)
        .unwrap_or("ALL SOURCES");
    format!(
        r#"<section class="muster-state muster-state--filtered" aria-labelledby="muster-filtered-title">
  <span class="muster-state__mark" aria-hidden="true">◇</span>
  <div><h2 id="muster-filtered-title">No signals match this register</h2>
  <p>Grade <strong>{grade}</strong>, state <strong>{state}</strong>, source <strong>{source}</strong>.</p>
  <p>{window}</p><a class="btn btn-secondary" href="/">Clear filters</a></div>
</section>"#,
        grade = filters.grade.label(),
        state = filters.state.label(),
        source = esc(source),
        window = window_copy(snapshot),
    )
}

fn render_list_unavailable() -> String {
    r#"<section class="muster-state muster-state--unavailable" aria-labelledby="muster-unavailable-title">
  <span class="muster-state__mark" aria-hidden="true">×</span>
  <div><h2 id="muster-unavailable-title">Muster unavailable</h2><p>Klaxon could not verify the bounded signal register. Retry this page before drawing conclusions about unread counts or register contents.</p><a class="btn btn-secondary" href="/">Retry muster</a></div>
</section>"#
        .to_string()
}

/// One ordered docket entry. Every producer field is escaped; URL authority remains the existing
/// scheme sanitizer followed by attribute escaping.
fn render_item(n: &Notification, csrf: &str, ordinal: usize) -> String {
    let grade = notification_grade(n);
    let (grade_class, glyph) = match grade {
        GradeFilter::Info => ("info", "●"),
        GradeFilter::Warning => ("warning", "▲"),
        GradeFilter::Critical => ("critical", "◆"),
        GradeFilter::Other | GradeFilter::All => ("other", "◇"),
    };
    let unread = n.read_at == 0;
    let open_link = if n.url.is_empty() {
        String::new()
    } else {
        format!(
            r#"<a class="signal-entry__open" href="{}" rel="noopener noreferrer">Open source ↗</a>"#,
            esc(&safe_url(&n.url))
        )
    };
    let body = if n.body.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="signal-entry__body">{}</p>"#, esc(&n.body))
    };
    let mark = if unread {
        format!(
            r#"<form class="inline-form signal-entry__read" method="post" action="/api/read">
  <input type="hidden" name="csrf_token" value="{csrf}">
  <input type="hidden" name="id" value="{id}">
  <button class="btn btn-ghost btn-sm" type="submit">Mark read</button>
</form>"#,
            csrf = esc(csrf),
            id = esc(&n.id),
        )
    } else {
        String::new()
    };
    let dismiss = format!(
        r#"<form class="inline-form signal-entry__dismiss" method="post" action="/api/dismiss">
  <input type="hidden" name="csrf_token" value="{csrf}">
  <input type="hidden" name="id" value="{id}">
  <button class="btn btn-subtle btn-sm" type="submit">Dismiss</button>
</form>"#,
        csrf = esc(csrf),
        id = esc(&n.id),
    );
    format!(
        r#"<li class="signal-entry{unread_class}" data-id="{id}" data-read="{read}">
  <div class="signal-entry__register"><span class="signal-entry__ordinal" aria-hidden="true">{ordinal:03}</span><span class="signal-grade signal-grade--{grade_class}"><span aria-hidden="true">{glyph}</span>{grade}</span><span class="signal-state">{state}</span></div>
  <div class="signal-entry__content"><h2 class="signal-entry__title">{title}</h2>{body}
    <dl class="signal-entry__provenance"><div><dt>Source</dt><dd><span class="badge-source">{source}</span></dd></div><div><dt>Received</dt><dd>{date}</dd></div></dl>
    <div class="signal-entry__actions">{open}{mark}{dismiss}</div>
  </div>
</li>"#,
        unread_class = if unread { " signal-entry--unread" } else { "" },
        id = esc(&n.id),
        read = if unread { "0" } else { "1" },
        ordinal = ordinal,
        grade_class = grade_class,
        glyph = glyph,
        grade = grade.label(),
        state = if unread { "UNREAD" } else { "READ" },
        title = esc(&n.title),
        body = body,
        source = esc(source_label(&n.source)),
        date = esc(&fmt_datetime(n.created_at)),
        open = open_link,
        mark = mark,
        dismiss = dismiss,
    )
}

// ---------------------------------------------------------------------------
// POST /api/read — mark one/all read
// ---------------------------------------------------------------------------

/// `POST /api/read` — mark one notification (when `id` is present) or all unread read.
pub async fn mark_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ReadForm>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    auth::verify_csrf(&headers, &form.csrf_token)?;

    let id = {
        let t = form.id.trim();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    };
    let updated = state.store.mark_read(&keys, id, now_secs()).await?;
    tracing::info!(updated, all = id.is_none(), "notifications marked read");

    Ok(redirect("/"))
}

// ---------------------------------------------------------------------------
// JSON siblings for the no-reload inbox (progressive enhancement)
// ---------------------------------------------------------------------------

/// JSON body for the optimistic inbox actions: an optional `id` (omit to mark all read) plus the
/// double-submit CSRF token (echoed from the page cookie, exactly like the form field).
#[derive(Debug, Deserialize)]
pub struct ActionJson {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub csrf_token: String,
}

/// `POST /api/read.json` — JSON sibling of [`mark_read`]. Same identity + CSRF + owner scope as the
/// form route (which is unchanged); returns `{ok, updated, unread}` so the page updates the badge in
/// place instead of reloading. No-JS users still use the `/api/read` form.
pub async fn mark_read_json(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ActionJson>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    auth::verify_csrf(&headers, &body.csrf_token)?;
    let id = {
        let t = body.id.trim();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    };
    let updated = state.store.mark_read(&keys, id, now_secs()).await?;
    let unread = state.store.unread_count(&keys).await;
    tracing::info!(
        updated,
        all = id.is_none(),
        "notifications marked read (json)"
    );
    Ok(
        Json(serde_json::json!({ "ok": true, "updated": updated, "unread": unread }))
            .into_response(),
    )
}

/// `POST /api/dismiss` — remove one notification from the inbox. A progressive-enhancement FORM
/// route (no-JS users get a 303 redirect), owner-scoped + CSRF, audited like the read path.
pub async fn dismiss(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ActionJson>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    auth::verify_csrf(&headers, &form.csrf_token)?;
    let id = form.id.trim();
    if id.is_empty() {
        return Err(AppError::InvalidRequest("id is required".to_string()));
    }
    let removed = state.store.delete_notification(&keys, id).await?;
    if removed > 0 {
        state.audit.emit(AuditEvent::notice(
            "notify.dismiss",
            &keys[0],
            id,
            "notification dismissed",
        ));
    }
    tracing::info!(user = %keys[0], removed, "notification dismissed");
    Ok(redirect("/"))
}

/// `POST /api/dismiss.json` — JSON sibling of [`dismiss`] for the optimistic inbox: same CSRF +
/// owner-scoped delete + audit; returns `{ok, removed, unread}`.
pub async fn dismiss_json(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ActionJson>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    auth::verify_csrf(&headers, &body.csrf_token)?;
    let id = body.id.trim();
    if id.is_empty() {
        return Err(AppError::InvalidRequest("id is required".to_string()));
    }
    let removed = state.store.delete_notification(&keys, id).await?;
    if removed > 0 {
        state.audit.emit(AuditEvent::notice(
            "notify.dismiss",
            &keys[0],
            id,
            "notification dismissed",
        ));
    }
    let unread = state.store.unread_count(&keys).await;
    Ok(
        Json(serde_json::json!({ "ok": true, "removed": removed, "unread": unread }))
            .into_response(),
    )
}

// ---------------------------------------------------------------------------
// POST /api/subscribe — store a Web Push subscription
// ---------------------------------------------------------------------------

/// `POST /api/subscribe` — persist this browser's Web Push subscription for the signed-in user.
pub async fn subscribe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SubscribeForm>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    auth::verify_csrf(&headers, &form.csrf_token)?;

    let endpoint = form.endpoint.trim();
    if endpoint.is_empty() {
        return Err(AppError::InvalidRequest("endpoint is required".to_string()));
    }
    let sub = PushSubscription {
        id: new_id("psub"),
        // The subject is always the first key (the gateway-injected X-Auth-Subject).
        user_sub: keys[0].clone(),
        endpoint: endpoint.to_string(),
        p256dh: form.p256dh.trim().to_string(),
        auth: form.auth.trim().to_string(),
        created_at: now_secs(),
    };
    state.store.upsert_subscription(&sub).await?;
    tracing::info!(user = %sub.user_sub, "web push subscription stored");

    // Audit: value-free detail (a push endpoint is a capability URL — never log it).
    state.audit.emit(AuditEvent::notice(
        "push.subscribe",
        &sub.user_sub,
        &sub.id,
        "web push subscription stored",
    ));

    Ok(redirect("/"))
}

// ---------------------------------------------------------------------------
// POST /api/unsubscribe — remove a Web Push subscription
// ---------------------------------------------------------------------------

/// `POST /api/unsubscribe` — remove this browser's Web Push subscription for the signed-in user.
/// The delete is owner-scoped in the store, so a forged `endpoint` belonging to another user's
/// subscription removes nothing.
pub async fn unsubscribe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<UnsubscribeForm>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    auth::verify_csrf(&headers, &form.csrf_token)?;

    let endpoint = form.endpoint.trim();
    if endpoint.is_empty() {
        return Err(AppError::InvalidRequest("endpoint is required".to_string()));
    }

    let owner = keys[0].clone();
    let removed = state.store.delete_subscription(&owner, endpoint).await?;
    tracing::info!(user = %owner, removed, "web push unsubscribe");

    if removed > 0 {
        state.audit.emit(AuditEvent::notice(
            "push.unsubscribe",
            &owner,
            &owner,
            "web push subscription removed",
        ));
    }

    Ok(redirect("/"))
}

// ---------------------------------------------------------------------------
// POST /api/test — send yourself a test notification
// ---------------------------------------------------------------------------

/// `POST /api/test` — store a self-addressed test notification and fan it out through the normal
/// delivery path (Web Push / webhooks / email), honoring the owner's delivery preferences exactly
/// like a real ingest. Lets a user verify their freshly-registered channels end-to-end.
pub async fn send_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TestForm>,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;
    auth::verify_csrf(&headers, &form.csrf_token)?;

    let notification = Notification {
        id: new_id("ntf"),
        // The recipient is ALWAYS the signed-in subject — a user can only test their own channels.
        user_sub: keys[0].clone(),
        source: "klaxon".to_string(),
        severity: "info".to_string(),
        title: "Test notification".to_string(),
        body: "This is a test from your Klaxon inbox. If it reached this browser or your webhook, delivery works.".to_string(),
        url: state.config.public_base_url.clone(),
        created_at: now_secs(),
        read_at: 0,
    };
    state.store.create_notification(&notification).await?;
    tracing::info!(user = %notification.user_sub, id = %notification.id, "test notification created");

    state.audit.emit(AuditEvent::notice(
        "notify.test",
        &notification.user_sub,
        &notification.id,
        "self test notification",
    ));

    // Best-effort fan-out on a detached task — never blocks this response.
    delivery::fan_out(state.clone(), notification);

    Ok(redirect("/"))
}

// ---------------------------------------------------------------------------
// GET /api/stream — Server-Sent-Events live stream
// ---------------------------------------------------------------------------

/// `GET /api/stream` — SSE of new notifications for this user. Polls the store every ~2s and emits
/// one `data:` frame (the notification JSON) per newly-seen row; a keep-alive comment holds the
/// connection open between events.
pub async fn stream(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let keys = auth::require_keys(&headers)?;

    let body = async_stream::stream! {
        let mut since = now_secs();
        let mut seen: HashSet<String> = HashSet::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        loop {
            ticker.tick().await;
            let fresh = state.store.list_since(&keys, since).await;
            for n in fresh {
                if !seen.insert(n.id.clone()) {
                    continue;
                }
                since = since.max(n.created_at);
                let json = serde_json::to_string(&n).unwrap_or_else(|_| "{}".to_string());
                yield Ok::<Event, Infallible>(Event::default().event("notification").data(json));
            }
        }
    };

    Ok(Sse::new(body)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response())
}

// ---------------------------------------------------------------------------
// GET /vapidPublicKey — the configured VAPID application-server public key (or empty)
// ---------------------------------------------------------------------------

/// `GET /vapidPublicKey` — the configured VAPID public key as plain text, or an empty body when
/// Web Push is not configured. No auth (a public key is not a secret).
pub async fn vapid_public_key(State(state): State<AppState>) -> Response {
    let key = state.config.vapid_public_key.clone().unwrap_or_default();
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], key).into_response()
}

/// `GET /sw.js` — the tiny push-display service worker registered by the inbox page. Served with a
/// JS content type so the browser accepts it as a same-origin worker, plus an explicit
/// `Service-Worker-Allowed: /` so the root scope is always permitted.
pub async fn service_worker() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (
                header::HeaderName::from_static("service-worker-allowed"),
                "/",
            ),
        ],
        SERVICE_WORKER_JS,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Render helpers
// ---------------------------------------------------------------------------

/// A 303 redirect (post/redirect/get).
fn redirect(location: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            HeaderValue::from_str(location).expect("valid location"),
        )],
    )
        .into_response()
}

/// An HTML response, optionally attaching a freshly-minted CSRF `Set-Cookie`.
fn html_with_cookie(body: String, set_cookie: Option<String>) -> Response {
    let mut resp = Html(body).into_response();
    if let Some(c) = set_cookie {
        if let Ok(value) = HeaderValue::from_str(&c) {
            resp.headers_mut().insert(header::SET_COOKIE, value);
        }
    }
    resp
}
