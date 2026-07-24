//! `GET /` — the server-rendered SSO chat dashboard.
//!
//! Renders the room list, the selected room's recent timeline (bodies sanitized + autolinked
//! server-side via [`crate::text::render_body`]), and a composer. Live updates are layered on by
//! the embedded client script, which opens `/ws` and calls the JSON API. A double-submit CSRF
//! token is minted into the page (cookie + JS-readable value) and echoed by every mutating
//! `fetch`.

use axum::extract::{rejection::QueryRejection, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::auth;
use crate::config::{
    LOBBY_ID, MAX_BODY_CHARS, MAX_SEARCH_QUERY_CHARS, MESSAGE_PAGE_LIMIT, MIN_SEARCH_QUERY_CHARS,
};
use crate::error::AppError;
use crate::handlers::{app_css, esc, fmt_time, query_or_invalid, render_template, topbar, APP_JS};
use crate::store::{Message, MessageHit, ReactionCount, UserRoom};
use crate::text::{preview_text, render_body, render_preview};
use crate::{ensure_lobby, AppState};

/// Longest quoted-parent snippet shown above a threaded reply, in characters (the full parent is
/// one click away in the timeline).
const QUOTE_SNIPPET_CHARS: usize = 120;

const DASHBOARD_HTML: &str = include_str!("../../templates/dashboard.html");

#[derive(Clone, Debug, Default, Deserialize)]
pub struct DashboardQueryV1 {
    pub room: Option<String>,
    pub message: Option<String>,
    pub reply_to: Option<String>,
    pub ledger: Option<LedgerTabV1>,
    pub q: Option<String>,
    pub receipt_message: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LedgerTabV1 {
    Search,
    Mentions,
    Pins,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoomAuthorityV1 {
    Active,
    ArchivedReadOnly,
}

impl RoomAuthorityV1 {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::ArchivedReadOnly => "archived_read_only",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SafeErrorViewV1 {
    pub code: ErrorCodeV1,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCodeV1 {
    InvalidRequest,
    Unauthorized,
    CsrfInvalid,
    Forbidden,
    NotFound,
    RoomArchived,
    Conflict,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SendReceiptV1 {
    Draft,
    Submitting,
    Persisted { message_id: String, created_at: i64 },
    Rejected { code: ErrorCodeV1 },
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TapeOutcomeV1 {
    Bounded,
    Empty,
    Unavailable(SafeErrorViewV1),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LedgerOutcomeV1 {
    Bounded,
    Empty,
    Unavailable(SafeErrorViewV1),
}

#[derive(Clone, Debug)]
pub struct IdentityViewV1 {
    pub display_email: String,
}

#[derive(Clone, Debug)]
pub struct RoomBanksViewV1 {
    pub active: Vec<UserRoom>,
    pub archived: Vec<UserRoom>,
}

#[derive(Clone, Debug)]
pub struct SelectedRoomViewV1 {
    pub room_id: String,
    pub name: String,
    pub topic: String,
    pub authority: RoomAuthorityV1,
    pub kind: String,
    pub available: bool,
}

#[derive(Clone, Debug)]
pub struct TapeViewV1 {
    pub outcome: TapeOutcomeV1,
    pub messages: Vec<Message>,
    pub located_message_id: Option<String>,
    pub locator_copy: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ComposerViewV1 {
    pub enabled: bool,
    pub reply_to_id: Option<String>,
    pub reply_context: Option<String>,
}

#[derive(Clone, Debug)]
pub struct LedgerItemViewV1 {
    pub room_id: String,
    pub room_name: String,
    pub message: Message,
}

#[derive(Clone, Debug)]
pub struct LedgerViewV1 {
    pub tab: Option<LedgerTabV1>,
    pub outcome: LedgerOutcomeV1,
    pub items: Vec<LedgerItemViewV1>,
    pub query: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DashboardViewV1 {
    pub identity: IdentityViewV1,
    pub room_banks: RoomBanksViewV1,
    pub selected: SelectedRoomViewV1,
    pub tape: TapeViewV1,
    pub composer: ComposerViewV1,
    pub ledger: LedgerViewV1,
    pub receipt: Option<SendReceiptV1>,
}

/// `GET /` — the chat dashboard for the signed-in user.
pub async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<DashboardQueryV1>, QueryRejection>,
) -> Response {
    let query = match query_or_invalid(query) {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    let (sub, email) = match auth::require_user(&headers) {
        Ok(v) => v,
        Err(_) => return unauthorized_page(),
    };

    // Moderation affordances (edit topic, pin) are gated to admins/mods — the same gate the
    // endpoints enforce; the flag only controls whether the UI OFFERS the controls.
    let is_mod = auth::is_admin(&headers);
    let mut unavailable = false;
    let mut view = match ensure_lobby(&state, &sub, &email).await {
        Ok(()) => match build_dashboard_view(&state, &sub, &email, &query).await {
            Ok(view) => view,
            Err(error) => {
                tracing::error!(error = %error, "build dashboard projections failed");
                unavailable = true;
                unavailable_dashboard_view(&email, &query)
            }
        },
        Err(error) => {
            tracing::error!(error = %error, "provision dashboard lobby failed");
            unavailable = true;
            unavailable_dashboard_view(&email, &query)
        }
    };
    let timeline = if unavailable {
        String::new()
    } else {
        match render_timeline(
            &state,
            &sub,
            &view.tape.messages,
            is_mod,
            view.tape.located_message_id.as_deref(),
        )
        .await
        {
            Ok(timeline) => timeline,
            Err(error) => {
                tracing::error!(error = %error, "render dashboard tape projections failed");
                unavailable = true;
                view = unavailable_dashboard_view(&email, &query);
                String::new()
            }
        }
    };
    let (csrf, set_cookie) = auth::ensure_csrf(&headers);
    let boot_json = render_boot_json(&view, &csrf, is_mod);

    let theme = odyssey::resolve_theme(
        headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok()),
    );

    let deck_state = if !view.selected.available {
        " deck--unavailable"
    } else if view.selected.authority == RoomAuthorityV1::ArchivedReadOnly {
        " deck--archived"
    } else {
        ""
    };
    let topbar_html = topbar("Chat", &email, theme);
    let rooms_html = render_room_banks(&view.room_banks, &view.selected.room_id);
    let room_title_html = esc(&view.selected.name);
    let topic_html = esc(&view.selected.topic);
    let tape_status_html = render_tape_status(&view.tape);
    let ledger_html = render_ledger(&view.ledger);
    let ledger_query_html = esc(view.ledger.query.as_deref().unwrap_or(""));
    let receipt_html = render_receipt(view.receipt.as_ref());
    let reply_context_html = esc(view.composer.reply_context.as_deref().unwrap_or(""));
    let reply_to_html = esc(view.composer.reply_to_id.as_deref().unwrap_or(""));
    let composer_disabled = if view.composer.enabled {
        ""
    } else {
        " disabled"
    };
    let csrf_html = json_string_content(&csrf);
    let selected_html = json_string_content(&view.selected.room_id);
    let page = match render_template(
        DASHBOARD_HTML,
        &[
            ("CSS", app_css()),
            ("THEME", odyssey::html_theme_attr(theme)),
            ("COLOR_SCHEME", odyssey::color_scheme_meta(theme)),
            ("TOPBAR", &topbar_html),
            ("ROOMS", &rooms_html),
            ("ROOM_TITLE", &room_title_html),
            ("TOPIC", &topic_html),
            ("DECK_STATE", deck_state),
            ("TAPE_STATUS", &tape_status_html),
            ("MESSAGES", &timeline),
            ("RECEIPT", &receipt_html),
            ("REPLY_CONTEXT", &reply_context_html),
            ("REPLY_TO_ID", &reply_to_html),
            ("COMPOSER_DISABLED", composer_disabled),
            ("LEDGER", &ledger_html),
            ("LEDGER_Q", &ledger_query_html),
            ("SELECTED", &selected_html),
            ("CSRF", &csrf_html),
            ("BOOT_JSON", &boot_json),
            ("JS", APP_JS),
        ],
    ) {
        Ok(page) => page,
        Err(error) => {
            tracing::error!(error = %error, "dashboard template rendering failed");
            return AppError::Unavailable("dashboard rendering unavailable".to_string())
                .into_response();
        }
    };

    let mut response = html_with_cookie(page, set_cookie);
    if unavailable {
        *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    }
    response
}

async fn build_dashboard_view(
    state: &AppState,
    sub: &str,
    email: &str,
    query: &DashboardQueryV1,
) -> Result<DashboardViewV1, AppError> {
    let rooms = state.store.list_user_rooms(sub).await?;
    let (active, archived): (Vec<UserRoom>, Vec<UserRoom>) =
        rooms.into_iter().partition(|room| !room.room.archived);
    let selected_room = match query.room.as_deref() {
        Some(requested) => active
            .iter()
            .chain(archived.iter())
            .find(|room| room.room.id == requested),
        None => active.first().or_else(|| archived.first()),
    };
    let requested_unavailable = query.room.is_some() && selected_room.is_none();

    let selected = selected_room.map_or_else(
        || SelectedRoomViewV1 {
            room_id: String::new(),
            name: "Room unavailable".to_string(),
            topic: String::new(),
            authority: RoomAuthorityV1::Active,
            kind: "room".to_string(),
            available: false,
        },
        |room| SelectedRoomViewV1 {
            room_id: room.room.id.clone(),
            name: room.room.name.clone(),
            topic: room.room.topic.clone(),
            authority: if room.room.archived {
                RoomAuthorityV1::ArchivedReadOnly
            } else {
                RoomAuthorityV1::Active
            },
            kind: room.room.kind.clone(),
            available: true,
        },
    );

    let messages = if selected.available {
        state
            .store
            .list_messages_authorized(&selected.room_id, sub, None, MESSAGE_PAGE_LIMIT)
            .await?
    } else {
        Vec::new()
    };
    let located_message_id = query.message.as_ref().and_then(|target| {
        messages
            .iter()
            .any(|message| &message.id == target)
            .then(|| target.clone())
    });
    let locator_copy = match query.message.as_ref() {
        Some(_) if located_message_id.is_none() && selected.available => {
            Some("Message is not in the latest window".to_string())
        }
        _ => None,
    };
    let tape_outcome = if requested_unavailable || !selected.available {
        TapeOutcomeV1::Unavailable(SafeErrorViewV1 {
            code: ErrorCodeV1::NotFound,
            message: "Resource unavailable".to_string(),
        })
    } else if messages.is_empty() {
        TapeOutcomeV1::Empty
    } else {
        TapeOutcomeV1::Bounded
    };

    let (reply_to_id, reply_context) = match query.reply_to.as_deref() {
        Some(reply_id) if selected.available => match state
            .store
            .get_message_authorized(&selected.room_id, reply_id, sub)
            .await?
        {
            Some(message) => (
                Some(message.id),
                Some(format!(
                    "Replying to {} · {}",
                    message.sender_email,
                    preview_text(&message.body)
                )),
            ),
            _ => (None, Some("Loop unavailable".to_string())),
        },
        Some(_) => (None, Some("Loop unavailable".to_string())),
        None => (None, None),
    };
    let receipt = match query.receipt_message.as_deref() {
        Some(message_id) if selected.available => state
            .store
            .get_message_authorized(&selected.room_id, message_id, sub)
            .await?
            .filter(|message| message.sender_sub == sub)
            .map(|message| SendReceiptV1::Persisted {
                message_id: message.id,
                created_at: message.created_at,
            }),
        _ => None,
    };
    let ledger = build_ledger_view(state, sub, &selected, query).await?;

    Ok(DashboardViewV1 {
        identity: IdentityViewV1 {
            display_email: email.to_string(),
        },
        room_banks: RoomBanksViewV1 { active, archived },
        selected: selected.clone(),
        tape: TapeViewV1 {
            outcome: tape_outcome,
            messages,
            located_message_id,
            locator_copy,
        },
        composer: ComposerViewV1 {
            enabled: selected.available && selected.authority == RoomAuthorityV1::Active,
            reply_to_id,
            reply_context,
        },
        ledger,
        receipt,
    })
}

fn unavailable_dashboard_view(email: &str, query: &DashboardQueryV1) -> DashboardViewV1 {
    let safe_error = || SafeErrorViewV1 {
        code: ErrorCodeV1::Unavailable,
        message: "Murmur is temporarily unavailable — refresh to retry".to_string(),
    };
    DashboardViewV1 {
        identity: IdentityViewV1 {
            display_email: email.to_string(),
        },
        room_banks: RoomBanksViewV1 {
            active: Vec::new(),
            archived: Vec::new(),
        },
        selected: SelectedRoomViewV1 {
            room_id: String::new(),
            name: "Room unavailable".to_string(),
            topic: String::new(),
            authority: RoomAuthorityV1::Active,
            kind: "room".to_string(),
            available: false,
        },
        tape: TapeViewV1 {
            outcome: TapeOutcomeV1::Unavailable(safe_error()),
            messages: Vec::new(),
            located_message_id: None,
            locator_copy: None,
        },
        composer: ComposerViewV1 {
            enabled: false,
            reply_to_id: None,
            reply_context: None,
        },
        ledger: LedgerViewV1 {
            tab: query.ledger,
            outcome: LedgerOutcomeV1::Unavailable(safe_error()),
            items: Vec::new(),
            query: query.q.clone(),
        },
        receipt: query
            .receipt_message
            .as_ref()
            .map(|_| SendReceiptV1::Unknown),
    }
}

async fn build_ledger_view(
    state: &AppState,
    sub: &str,
    selected: &SelectedRoomViewV1,
    query: &DashboardQueryV1,
) -> Result<LedgerViewV1, AppError> {
    let Some(tab) = query.ledger else {
        return Ok(LedgerViewV1 {
            tab: None,
            outcome: LedgerOutcomeV1::Empty,
            items: Vec::new(),
            query: None,
        });
    };
    let (items, normalized_query, invalid) = match tab {
        LedgerTabV1::Search => {
            let needle = query.q.as_deref().unwrap_or("").trim();
            let length = needle.chars().count();
            if !(MIN_SEARCH_QUERY_CHARS..=MAX_SEARCH_QUERY_CHARS).contains(&length) {
                (Vec::new(), Some(needle.to_string()), true)
            } else {
                let hits = state
                    .store
                    .search_user_messages(
                        sub,
                        &needle.to_lowercase(),
                        None,
                        crate::config::SEARCH_PAGE_LIMIT,
                    )
                    .await?;
                (
                    ledger_items_from_hits(hits),
                    Some(needle.to_string()),
                    false,
                )
            }
        }
        LedgerTabV1::Mentions => (
            ledger_items_from_hits(
                state
                    .store
                    .list_user_mentions(sub, None, crate::config::MENTIONS_PAGE_LIMIT)
                    .await?,
            ),
            None,
            false,
        ),
        LedgerTabV1::Pins if selected.available => (
            state
                .store
                .list_pinned_authorized(&selected.room_id, sub)
                .await?
                .into_iter()
                .map(|message| LedgerItemViewV1 {
                    room_id: selected.room_id.clone(),
                    room_name: selected.name.clone(),
                    message,
                })
                .collect(),
            None,
            false,
        ),
        LedgerTabV1::Pins => (Vec::new(), None, true),
    };
    let outcome = if invalid {
        LedgerOutcomeV1::Unavailable(SafeErrorViewV1 {
            code: ErrorCodeV1::InvalidRequest,
            message: "Enter a search query between 2 and 200 characters".to_string(),
        })
    } else if items.is_empty() {
        LedgerOutcomeV1::Empty
    } else {
        LedgerOutcomeV1::Bounded
    };
    Ok(LedgerViewV1 {
        tab: Some(tab),
        outcome,
        items,
        query: normalized_query,
    })
}

fn ledger_items_from_hits(hits: Vec<MessageHit>) -> Vec<LedgerItemViewV1> {
    hits.into_iter()
        .map(|hit| LedgerItemViewV1 {
            room_id: hit.message.room_id.clone(),
            room_name: hit.room_name,
            message: hit.message,
        })
        .collect()
}

/// Boot payload for the Patchbay client. HTML-significant characters are escaped so the JSON can
/// live inside a `<script type="application/json">` element without creating a closing tag seam.
pub fn render_boot_json(view: &DashboardViewV1, csrf: &str, is_mod: bool) -> String {
    let value = json!({
        "csrf": csrf,
        "me": { "display_email": view.identity.display_email },
        "selected": {
            "room_id": view.selected.room_id,
            "authority": view.selected.authority.as_str(),
            "kind": view.selected.kind,
        },
        "isMod": is_mod,
        "limits": {
            "messagePage": MESSAGE_PAGE_LIMIT,
            "body": MAX_BODY_CHARS,
            "searchMin": MIN_SEARCH_QUERY_CHARS,
            "searchMax": MAX_SEARCH_QUERY_CHARS,
        },
        "transport": { "initial": "unknown" },
    });
    escape_json_for_html(&serde_json::to_string(&value).expect("boot JSON is serializable"))
}

fn escape_json_for_html(value: &str) -> String {
    value
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn json_string_content(value: &str) -> String {
    let quoted = escape_json_for_html(
        &serde_json::to_string(value).expect("string JSON serialization cannot fail"),
    );
    quoted[1..quoted.len() - 1].to_string()
}

fn render_room_banks(banks: &RoomBanksViewV1, selected: &str) -> String {
    let mut out = String::new();
    if !banks.active.is_empty() {
        out.push_str(r#"<li class="jackfield__bank-label" role="presentation">Patched</li>"#);
        out.push_str(&render_room_list(&banks.active, selected));
    }
    if !banks.archived.is_empty() {
        out.push_str(
            r#"<li class="jackfield__bank-label" role="presentation">Archive — read only</li>"#,
        );
        out.push_str(&render_room_list(&banks.archived, selected));
    }
    if out.is_empty() {
        r#"<li class="jackfield__note">No authorized rooms</li>"#.to_string()
    } else {
        out
    }
}

fn render_tape_status(tape: &TapeViewV1) -> String {
    let outcome = match &tape.outcome {
        TapeOutcomeV1::Bounded => "Latest window · up to 50 messages",
        TapeOutcomeV1::Empty => "No messages in the latest window",
        TapeOutcomeV1::Unavailable(error) => &error.message,
    };
    match &tape.locator_copy {
        Some(locator) => format!("{} · {}", esc(outcome), esc(locator)),
        None => esc(outcome),
    }
}

fn render_receipt(receipt: Option<&SendReceiptV1>) -> String {
    match receipt {
        Some(SendReceiptV1::Persisted {
            message_id,
            created_at,
        }) => format!(
            r#"<span data-receipt-message="{}">Saved · {} UTC</span>"#,
            esc(message_id),
            esc(&fmt_time(*created_at)),
        ),
        Some(SendReceiptV1::Submitting) => "Sending…".to_string(),
        Some(SendReceiptV1::Rejected { .. }) => "Not sent".to_string(),
        Some(SendReceiptV1::Unknown) => {
            "Send uncertain — your text is preserved; check the tape before retrying".to_string()
        }
        Some(SendReceiptV1::Draft) | None => String::new(),
    }
}

fn render_ledger(ledger: &LedgerViewV1) -> String {
    if ledger.tab.is_none() {
        return String::new();
    }
    let status = match &ledger.outcome {
        LedgerOutcomeV1::Bounded => "Showing up to 50 results",
        LedgerOutcomeV1::Empty => "No results in this bounded view",
        LedgerOutcomeV1::Unavailable(error) => &error.message,
    };
    let mut out = format!(r#"<p class="pb-ledger__status">{}</p>"#, esc(status));
    for item in &ledger.items {
        out.push_str(&format!(
            r#"<a class="pb-ledger__item" href="/?room={room}&message={message}#msg-{fragment}"><span>{room_name}</span><b>{author}</b><span>{body}</span></a>"#,
            room = query_escape(&item.room_id),
            message = query_escape(&item.message.id),
            fragment = query_escape(&item.message.id),
            room_name = esc(&item.room_name),
            author = esc(&item.message.sender_email),
            body = render_preview(&item.message.body),
        ));
    }
    out
}

fn query_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Render the Patchbay jack list. Every room remains a real GET anchor for no-JS navigation and
/// exposes only the frozen room identity/authority attributes. Unread is intentionally a boolean
/// cue: the product deliberately communicates unread state without turning the rail into a counter.
fn render_room_list(rooms: &[UserRoom], selected: &str) -> String {
    if rooms.is_empty() {
        return r#"<li class="jackfield__note">No rooms yet</li>"#.to_string();
    }
    let mut out = String::new();
    for r in rooms {
        let is_selected = r.room.id == selected;
        let active = if is_selected { " is-patched" } else { "" };
        let current = if is_selected {
            r#" aria-current="page""#
        } else {
            ""
        };
        let authority = if r.room.archived {
            "archived"
        } else {
            "active"
        };
        // The open room is read: never show a badge on it even if the stored cursor lags.
        let unread = if is_selected { 0 } else { r.unread.max(0) };
        let mentioned = !is_selected && r.mentioned;
        let system_mark = if r.room.id == LOBBY_ID {
            r#"<span class="jack__sys">SYS</span>"#
        } else {
            ""
        };
        let kind_mark = if r.room.kind == "dm" {
            r#"<span class="jack__kind">DM</span>"#
        } else {
            ""
        };
        let archive_mark = if r.room.archived {
            r#"<span class="jack__kind">Read only</span>"#
        } else {
            ""
        };
        out.push_str(&format!(
            r#"<li class="jack">
  <a class="jack__link{active}" href="/?room={query_id}" data-room-id="{id}" data-room-kind="{kind}" data-room-state="{authority}"{current}>
    <span class="jack__ring" aria-hidden="true"></span>
    <span class="jack__label">{name}</span>
    {system_mark}{kind_mark}{archive_mark}{badge}
  </a>
</li>"#,
            active = active,
            query_id = query_escape(&r.room.id),
            id = esc(&r.room.id),
            kind = esc(&r.room.kind),
            authority = authority,
            name = esc(&r.room.name),
            current = current,
            system_mark = system_mark,
            kind_mark = kind_mark,
            archive_mark = archive_mark,
            badge = room_badge_html(unread, mentioned),
        ));
    }
    out
}

/// The unread cue for a room-list row: a mention dot plus a truthy `Unread` marker. Exact counts
/// deliberately stay out of the DOM because the product contract uses a binary scan cue.
fn room_badge_html(unread: i64, mentioned: bool) -> String {
    let dot = if mentioned {
        r#"<span class="jack__mark--mention" title="You were mentioned" aria-label="You were mentioned"></span>"#
    } else {
        ""
    };
    let hidden = if unread > 0 { "" } else { " hidden" };
    format!(
        r#"<span class="jack__marks">{dot}<span class="jack__mark--unread"{hidden}>Unread</span></span>"#,
        dot = dot,
        hidden = hidden,
    )
}

/// Render the timeline (oldest-first). The store returns newest-first, so we reverse for reading.
/// Each row is enriched with its threaded-reply context (quoted parent + reply count) and its
/// reaction tallies (with the caller's own reactions highlighted) — all fetched from the store.
async fn render_timeline(
    state: &AppState,
    sub: &str,
    messages: &[Message],
    is_mod: bool,
    located_message_id: Option<&str>,
) -> Result<String, AppError> {
    if messages.is_empty() {
        return Ok(String::new());
    }
    let mut out = String::new();
    for m in messages.iter().rev() {
        // Threaded-reply parent (best-effort: a purged parent simply renders no quote).
        let parent = match &m.reply_to_id {
            Some(pid) => {
                state
                    .store
                    .get_message_authorized(&m.room_id, pid, sub)
                    .await?
            }
            None => None,
        };
        let reply_count = state
            .store
            .count_replies_authorized(&m.room_id, &m.id, sub)
            .await?;
        let projection = state
            .store
            .reaction_projection_authorized(&m.room_id, &m.id, sub)
            .await?;
        out.push_str(&render_message_row(
            m,
            parent.as_ref(),
            reply_count,
            &projection.reactions,
            &projection.mine,
            MessageRenderFlags {
                is_mod,
                own: m.sender_sub == sub,
                located: located_message_id == Some(m.id.as_str()),
            },
        ));
    }
    Ok(out)
}

/// Render a message body to safe HTML: a soft-deleted / redacted row shows a fixed tombstone
/// (`[deleted]` or the stored moderator text, escaped); a live body goes through the
/// escape-then-autolink renderer.
fn message_body_html(m: &Message) -> String {
    if m.deleted {
        let label = if m.body.is_empty() {
            "[deleted]".to_string()
        } else {
            esc(&m.body)
        };
        format!(r#"<span class="msg__deleted">{label}</span>"#)
    } else {
        render_body(&m.body)
    }
}

/// The `(edited)` marker for a live, edited message (empty otherwise).
fn message_edited_html(m: &Message) -> &'static str {
    if m.edited_at > 0 && !m.deleted {
        r#"<span class="msg__edited">(edited)</span>"#
    } else {
        ""
    }
}

/// One bare message row (no reply/reaction chrome). `sender_email` / time are escaped; the body is
/// sanitized. Retained for callers that render a plain message.
pub fn render_message(m: &Message) -> String {
    let lifecycle = message_lifecycle(m);
    format!(
        r#"<li class="msg" id="msg-{id}" data-id="{id}" data-msg-id="{id}" data-created-at="{created_at}" data-lifecycle="{lifecycle}" data-own="false">
  <div class="msg__head"><span class="msg__author">{author}</span><span class="msg__time">{time}</span>{edited}</div>
  <div class="msg__body">{body}</div>
</li>"#,
        id = esc(&m.id),
        created_at = m.created_at,
        lifecycle = lifecycle,
        author = esc(&m.sender_email),
        time = esc(&fmt_time(m.created_at)),
        edited = message_edited_html(m),
        body = message_body_html(m),
    )
}

/// One enriched timeline row: an optional quoted parent (threaded reply), the message head/body, a
/// reply-count marker, and the reaction chip row. Every interpolated field is escaped/sanitized.
struct MessageRenderFlags {
    is_mod: bool,
    own: bool,
    located: bool,
}

fn render_message_row(
    m: &Message,
    parent: Option<&Message>,
    reply_count: i64,
    reactions: &[ReactionCount],
    mine: &[String],
    flags: MessageRenderFlags,
) -> String {
    let located_class = if flags.located { " is-located" } else { "" };
    let tabindex = if flags.located {
        r#" tabindex="-1""#
    } else {
        ""
    };
    format!(
        r#"<li class="msg{located_class}" id="msg-{id}" data-id="{id}" data-msg-id="{id}" data-created-at="{created_at}" data-lifecycle="{lifecycle}" data-own="{own}" data-author="{author}"{tabindex}>
  {quote}<div class="msg__head"><span class="msg__author">{author}</span><span class="msg__time">{time}</span>{edited}{replies}<span class="msg__tools"><button type="button" class="msg__tool" data-act="react">React</button><button type="button" class="msg__tool" data-act="reply">Reply</button>{pin_tool}</span></div>
  <div class="msg__body">{body}</div>
  {reactions}
</li>"#,
        id = esc(&m.id),
        created_at = m.created_at,
        lifecycle = message_lifecycle(m),
        own = if flags.own { "true" } else { "false" },
        located_class = located_class,
        tabindex = tabindex,
        author = esc(&m.sender_email),
        time = esc(&fmt_time(m.created_at)),
        edited = message_edited_html(m),
        replies = reply_count_html(reply_count),
        quote = quote_html(parent),
        body = message_body_html(m),
        reactions = reactions_html(reactions, mine),
        pin_tool = if flags.is_mod {
            r#"<button type="button" class="msg__tool" data-act="pin">Pin</button>"#
        } else {
            ""
        },
    )
}

fn message_lifecycle(message: &Message) -> &'static str {
    if !message.deleted {
        "live"
    } else if message.body == crate::config::REDACTED_BODY {
        "redacted"
    } else {
        "deleted"
    }
}

/// The quoted-parent block shown above a threaded reply. Empty for a top-level post (or when the
/// parent was purged). The parent author + a short snippet are escaped as plain text.
fn quote_html(parent: Option<&Message>) -> String {
    let Some(p) = parent else {
        return String::new();
    };
    let snippet = if p.deleted {
        if p.body.is_empty() {
            "[deleted]".to_string()
        } else {
            p.body.clone()
        }
    } else {
        p.body.clone()
    };
    // A one-line, length-capped plain-text snippet (escaped) — no markup, no autolinking.
    let mut oneline: String = snippet.chars().take(QUOTE_SNIPPET_CHARS).collect();
    if snippet.chars().count() > QUOTE_SNIPPET_CHARS {
        oneline.push('…');
    }
    format!(
        r#"<div class="msg__quote" data-parent-id="{id}"><span class="msg__quote-author">{author}</span><span class="msg__quote-body">{body}</span></div>
  "#,
        id = esc(&p.id),
        author = esc(&p.sender_email),
        body = render_preview(&oneline),
    )
}

/// The reply-count marker on a parent message (empty when it has no replies).
fn reply_count_html(reply_count: i64) -> String {
    if reply_count <= 0 {
        return String::new();
    }
    let label = if reply_count == 1 { "reply" } else { "replies" };
    format!(
        r#"<span class="msg__replies">{count} {label}</span>"#,
        count = reply_count,
        label = label,
    )
}

/// The reaction chip row for a message. Each chip carries its emoji + distinct-user count; a chip
/// the caller reacted with gets `is-mine`. Empty when the message has no reactions.
fn reactions_html(reactions: &[ReactionCount], mine: &[String]) -> String {
    if reactions.is_empty() {
        return String::new();
    }
    let mut chips = String::new();
    for r in reactions {
        let is_mine = if mine.iter().any(|e| e == &r.emoji) {
            " is-mine"
        } else {
            ""
        };
        chips.push_str(&format!(
            r#"<button type="button" class="reaction{mine}" data-emoji="{emoji}"><span class="reaction__emoji">{emoji}</span><span class="reaction__count">{count}</span></button>"#,
            mine = is_mine,
            emoji = esc(&r.emoji),
            count = r.count,
        ));
    }
    format!(
        r#"<div class="msg__reactions">{chips}</div>"#,
        chips = chips
    )
}

/// A minimal HTML "session required" page (defense in depth — the gateway normally guarantees an
/// SSO identity before `/` is reached).
fn unauthorized_page() -> Response {
    let page = format!(
        r#"<!DOCTYPE html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sign in · Murmur</title><style>{css}</style></head>
<body class="page-chat">{topbar}<main class="chat chat--center">
<div class="signin-card"><h1>Session required</h1>
<p>Sign in through the Steadholme gateway to use Murmur.</p>
<a class="btn btn-primary" href="/">Reload</a></div>
</main></body></html>"#,
        css = app_css(),
        topbar = topbar("Chat", "", "light"),
    );
    (StatusCode::UNAUTHORIZED, Html(page)).into_response()
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
