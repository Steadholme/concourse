//! Reminder delivery: turn a due reminder into an in-app notification POSTed to the estate's
//! Klaxon ingest.
//!
//! Storage + the bounded due-scan live in [`crate::store`]; this module is the DELIVERY HOOK. A
//! [`ReminderSink`] abstracts the destination so the logic is testable with an in-memory fake:
//! [`deliver_due_reminders`] runs ONE bounded [`Store::due_reminders`] query, builds a notification
//! per row, hands it to the sink, and marks each delivered so it never fires twice. The concrete
//! [`HttpKlaxonSink`] mirrors the estate's sibling pattern (tempo/pulse/vitals → `POST
//! http://klaxon:9050/api/notify` with a `Bearer` token) using a raw TCP HTTP/1.1 write — the same
//! shape Klaxon itself uses for webhook egress, so no new HTTP-client dependency is pulled in.
//!
//! WIRING: standalone Almanac ([`crate::build_state_from_env`]) spawns [`spawn_reminder_scanner`]
//! on a timer when `ALMANAC_KLAXON_NOTIFY_URL` is set. In the COMPOSITE (concourse) Almanac's
//! `AppState` is built by an explicit literal and no background task is started for it, so the scan
//! is a NOTED follow-up there (its `Config` still parses the env, so only the composite's task
//! spawn is missing — no code in this crate needs to change).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::store::{DueReminder, Store};

/// The public host events link back to (notification deep-link + UID domain).
const CAL_HOST: &str = "cal.w33d.xyz";
/// The `source` label on every reminder notification (Klaxon groups + mutes by source).
const SOURCE: &str = "almanac";
/// Max reminders handed off per scan tick — bounds the query and the per-tick work.
pub const SCAN_BATCH: usize = 200;

/// A ready-to-send in-app notification derived from a due reminder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReminderNote {
    pub user_sub: String,
    pub title: String,
    pub body: String,
    pub url: String,
}

impl ReminderNote {
    /// Build the notification for one due reminder.
    pub fn from_due(d: &DueReminder) -> ReminderNote {
        let lead = match d.minutes_before {
            0 => "Starting now".to_string(),
            1 => "Starts in 1 minute".to_string(),
            m if m % 60 == 0 && m >= 60 => {
                let h = m / 60;
                format!("Starts in {h} hour{}", if h == 1 { "" } else { "s" })
            }
            m => format!("Starts in {m} minutes"),
        };
        let mut body = lead;
        if !d.event_location.trim().is_empty() {
            body.push_str(" · ");
            body.push_str(d.event_location.trim());
        }
        ReminderNote {
            user_sub: d.owner_sub.clone(),
            title: format!("Reminder: {}", d.event_title),
            body,
            url: format!("https://{CAL_HOST}/event/{}", d.event_id),
        }
    }

    /// The Klaxon `POST /api/notify` JSON envelope for this note (manually built + escaped — no
    /// serde_json dependency in this crate).
    pub fn to_klaxon_json(&self) -> String {
        format!(
            "{{\"user_sub\":\"{}\",\"source\":\"{}\",\"severity\":\"info\",\"title\":\"{}\",\"body\":\"{}\",\"url\":\"{}\"}}",
            json_escape(&self.user_sub),
            json_escape(SOURCE),
            json_escape(&self.title),
            json_escape(&self.body),
            json_escape(&self.url),
        )
    }
}

/// Escape a string for embedding in a JSON double-quoted value (RFC 8259 §7).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Where a reminder notification is delivered. Abstracted so [`deliver_due_reminders`] is testable
/// without a live Klaxon.
#[async_trait]
pub trait ReminderSink: Send + Sync {
    /// Deliver one notification. `Ok(())` marks the reminder delivered; `Err` leaves it pending
    /// (a later scan retries).
    async fn deliver(&self, note: &ReminderNote) -> Result<(), String>;
}

/// Run ONE bounded due-scan and deliver each result, marking successes delivered. Returns the count
/// delivered. This is the whole reminder engine tick — a caller (a timer, see
/// [`spawn_reminder_scanner`]) invokes it periodically; there is NO busy loop.
pub async fn deliver_due_reminders(
    store: &dyn Store,
    sink: &dyn ReminderSink,
    now_ms: i64,
    limit: usize,
) -> usize {
    let due = match store.due_reminders(now_ms, limit).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "reminder due-scan failed");
            return 0;
        }
    };
    let mut delivered = 0usize;
    for d in &due {
        let note = ReminderNote::from_due(d);
        match sink.deliver(&note).await {
            Ok(()) => {
                if let Err(e) = store.mark_reminder_delivered(&d.reminder_id, now_ms).await {
                    tracing::warn!(error = %e, id = %d.reminder_id, "mark reminder delivered failed");
                    continue;
                }
                delivered += 1;
                // Audit: value-free notice of a delivered reminder (no recipient payload).
                tracing::info!(target: "audit", event = "reminder.delivered", "reminder delivered");
            }
            Err(e) => {
                tracing::warn!(error = %e, id = %d.reminder_id, "reminder delivery failed — will retry")
            }
        }
    }
    delivered
}

/// A [`ReminderSink`] that POSTs to Klaxon's Bearer-authed `POST /api/notify` over plain HTTP
/// (the internal `http://klaxon:9050` ingest). HTTPS is intentionally NOT supported here — the
/// internal ingest is plaintext on the estate network, exactly like the sibling producers.
pub struct HttpKlaxonSink {
    url: String,
    token: String,
}

impl HttpKlaxonSink {
    /// Build from the ingest URL (`http://klaxon:9050/api/notify`) and Bearer token.
    pub fn new(url: String, token: String) -> Self {
        Self { url, token }
    }
}

#[async_trait]
impl ReminderSink for HttpKlaxonSink {
    async fn deliver(&self, note: &ReminderNote) -> Result<(), String> {
        let (host, port, path) = parse_http_url(&self.url).ok_or_else(|| {
            format!(
                "ALMANAC_KLAXON_NOTIFY_URL is not a plain http:// URL: {}",
                self.url
            )
        })?;
        let body = note.to_klaxon_json();
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\n\
             Content-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
            path = path,
            host = host,
            token = self.token,
            len = body.len(),
            body = body,
        );
        let fut = async {
            let mut stream = TcpStream::connect((host.as_str(), port))
                .await
                .map_err(|e| e.to_string())?;
            stream
                .write_all(req.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            let mut buf = Vec::with_capacity(256);
            stream
                .read_to_end(&mut buf)
                .await
                .map_err(|e| e.to_string())?;
            Ok::<Vec<u8>, String>(buf)
        };
        let buf = tokio::time::timeout(Duration::from_secs(5), fut)
            .await
            .map_err(|_| "klaxon ingest timed out".to_string())??;
        let head = String::from_utf8_lossy(&buf);
        let status_line = head.lines().next().unwrap_or("");
        if status_line.contains(" 20") {
            Ok(())
        } else {
            Err(format!("klaxon ingest returned: {status_line}"))
        }
    }
}

/// Split a plain `http://host[:port]/path` URL into `(host, port, path)` (default port 80, default
/// path `/`). Returns `None` for a non-http URL.
fn parse_http_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (authority.to_string(), 80u16),
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port, path))
}

/// Spawn a detached, bounded reminder scanner: every `period`, run one [`deliver_due_reminders`]
/// pass. Used by standalone Almanac when Klaxon delivery is configured. The interval is a TIMER,
/// not a busy loop; each tick is a single bounded query + best-effort deliveries.
pub fn spawn_reminder_scanner(
    store: Arc<dyn Store>,
    sink: Arc<dyn ReminderSink>,
    period: Duration,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        loop {
            ticker.tick().await;
            let n =
                deliver_due_reminders(store.as_ref(), sink.as_ref(), crate::now_ms(), SCAN_BATCH)
                    .await;
            if n > 0 {
                tracing::info!(count = n, "reminder scan delivered notifications");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Event, InMemoryStore, Reminder};
    use std::sync::Mutex;

    fn due(id: &str, owner: &str, title: &str, loc: &str, minutes: i64, start: i64) -> DueReminder {
        DueReminder {
            reminder_id: id.to_string(),
            owner_sub: owner.to_string(),
            minutes_before: minutes,
            event_id: "e1".to_string(),
            event_title: title.to_string(),
            event_starts_at: start,
            event_location: loc.to_string(),
        }
    }

    #[test]
    fn note_wording_and_json_escaping() {
        let n = ReminderNote::from_due(&due("r", "u_alice", "Q1 review", "War room", 10, 0));
        assert_eq!(n.title, "Reminder: Q1 review");
        assert_eq!(n.body, "Starts in 10 minutes · War room");
        assert_eq!(n.url, "https://cal.w33d.xyz/event/e1");
        assert_eq!(
            ReminderNote::from_due(&due("r", "u", "x", "", 0, 0)).body,
            "Starting now"
        );
        assert_eq!(
            ReminderNote::from_due(&due("r", "u", "x", "", 60, 0)).body,
            "Starts in 1 hour"
        );

        // JSON is escaped: quotes/backslashes/newlines cannot break out of the envelope.
        let evil = ReminderNote::from_due(&due("r", "u\"x", "a\"b\\c\nd", "", 5, 0));
        let json = evil.to_klaxon_json();
        assert!(json.contains("\\\"x"), "user_sub quote escaped: {json}");
        assert!(
            json.contains("Reminder: a\\\"b\\\\c\\nd"),
            "title escaped: {json}"
        );
        assert!(json.contains("\"source\":\"almanac\""));
        // It parses as a single flat object shape (balanced braces, one leading {).
        assert!(json.starts_with('{') && json.ends_with('}'));
    }

    #[test]
    fn parses_http_url_forms() {
        assert_eq!(
            parse_http_url("http://klaxon:9050/api/notify"),
            Some(("klaxon".to_string(), 9050, "/api/notify".to_string()))
        );
        assert_eq!(
            parse_http_url("http://klaxon"),
            Some(("klaxon".to_string(), 80, "/".to_string()))
        );
        assert!(
            parse_http_url("https://klaxon/api").is_none(),
            "https not supported here"
        );
    }

    /// Records deliveries; can be told to fail for a specific reminder to test retry semantics.
    struct FakeSink {
        seen: Mutex<Vec<ReminderNote>>,
        fail_for: Option<String>,
    }
    #[async_trait]
    impl ReminderSink for FakeSink {
        async fn deliver(&self, note: &ReminderNote) -> Result<(), String> {
            if self.fail_for.as_deref() == Some(note.user_sub.as_str()) {
                return Err("boom".to_string());
            }
            self.seen.lock().unwrap().push(note.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn deliver_marks_delivered_and_does_not_refire() {
        let store = InMemoryStore::new();
        let start = 10_000_000i64;
        store
            .upsert_event(Event {
                id: "e1".into(),
                owner_sub: "u_alice".into(),
                calendar_id: String::new(),
                title: "Standup".into(),
                starts_at: start,
                ends_at: start + 900_000,
                all_day: false,
                location: String::new(),
                notes: String::new(),
                rrule: String::new(),
                series_id: String::new(),
                override_occurrence_date: 0,
                exception_dates: Vec::new(),
                created_at: 0,
            })
            .await
            .unwrap();
        store
            .replace_reminders(
                "u_alice",
                "e1",
                vec![Reminder {
                    id: "r10".into(),
                    event_id: "e1".into(),
                    owner_sub: "u_alice".into(),
                    minutes_before: 10,
                    delivered_at: 0,
                    created_at: 0,
                }],
            )
            .await
            .unwrap();

        let sink = FakeSink {
            seen: Mutex::new(Vec::new()),
            fail_for: None,
        };
        // First scan (after the fire time, before start): delivers once.
        let n = deliver_due_reminders(&store, &sink, start - 300_000, SCAN_BATCH).await;
        assert_eq!(n, 1);
        assert_eq!(sink.seen.lock().unwrap().len(), 1);
        assert_eq!(sink.seen.lock().unwrap()[0].title, "Reminder: Standup");
        // A second scan finds nothing (delivered_at set).
        let n2 = deliver_due_reminders(&store, &sink, start - 200_000, SCAN_BATCH).await;
        assert_eq!(n2, 0);
        assert_eq!(sink.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn failed_delivery_stays_pending_for_retry() {
        let store = InMemoryStore::new();
        let start = 10_000_000i64;
        store
            .upsert_event(Event {
                id: "e1".into(),
                owner_sub: "u_alice".into(),
                calendar_id: String::new(),
                title: "Standup".into(),
                starts_at: start,
                ends_at: start + 1,
                all_day: false,
                location: String::new(),
                notes: String::new(),
                rrule: String::new(),
                series_id: String::new(),
                override_occurrence_date: 0,
                exception_dates: Vec::new(),
                created_at: 0,
            })
            .await
            .unwrap();
        store
            .replace_reminders(
                "u_alice",
                "e1",
                vec![Reminder {
                    id: "r".into(),
                    event_id: "e1".into(),
                    owner_sub: "u_alice".into(),
                    minutes_before: 10,
                    delivered_at: 0,
                    created_at: 0,
                }],
            )
            .await
            .unwrap();

        let failing = FakeSink {
            seen: Mutex::new(Vec::new()),
            fail_for: Some("u_alice".to_string()),
        };
        assert_eq!(
            deliver_due_reminders(&store, &failing, start - 100_000, SCAN_BATCH).await,
            0
        );
        // Still pending -> a working sink later delivers it.
        let ok = FakeSink {
            seen: Mutex::new(Vec::new()),
            fail_for: None,
        };
        assert_eq!(
            deliver_due_reminders(&store, &ok, start - 100_000, SCAN_BATCH).await,
            1
        );
    }
}
