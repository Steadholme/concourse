//! Calendar + contacts storage: `events` and `contacts`, both scoped to an owner subject.
//!
//! `Store` is a small async trait with an in-memory and a PostgreSQL implementation, mirroring
//! the keystone/lattice/watchtower seam: handlers depend only on the trait, so a FusionDB-backed
//! store can drop in later. The PostgreSQL layer uses ONLY portable standard SQL
//! (TEXT/BIGINT/BOOLEAN, PRIMARY KEY/NOT NULL/DEFAULT, `INSERT .. ON CONFLICT .. DO UPDATE`,
//! plain indexes) and runtime queries (no compile-time macros), so the build needs NO database
//! and the same statements later run unchanged on FusionDB over pgwire.
//!
//! EVERY read/write takes `owner_sub` and filters by it, so one user can never see or mutate
//! another's rows. `owner_sub` always comes from the gateway-injected `X-Auth-Subject`, never
//! from a client field. The optional text columns are `NOT NULL DEFAULT ''` rather than nullable,
//! so the Rust types stay plain `String` with no `Option` edge cases.

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use thiserror::Error;

use crate::config::{CONTACT_LIST_LIMIT, EVENT_LIST_LIMIT};

/// Storage failure surfaced to the handler layer (mapped to a 500).
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("store error: {0}")]
    Backend(String),
    #[error("event is not available to this owner")]
    OwnershipConflict,
}

/// A calendar event. `id` is a random hex string; `owner_sub` scopes it to one user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub id: String,
    pub owner_sub: String,
    /// Owning calendar id. Empty legacy rows are mapped to the owner's default calendar at read time.
    pub calendar_id: String,
    pub title: String,
    /// UTC epoch ms.
    pub starts_at: i64,
    /// UTC epoch ms (>= `starts_at`).
    pub ends_at: i64,
    pub all_day: bool,
    pub location: String,
    pub notes: String,
    /// RFC 5545 RRULE for a recurring series (e.g. `FREQ=WEEKLY;BYDAY=MO,WE`); empty = a one-off.
    /// The row IS the series (its `starts_at`/`ends_at` are DTSTART); occurrences are expanded
    /// virtually at render time by [`crate::rrule`].
    pub rrule: String,
    /// Non-empty when this row is a detached override for one occurrence of a recurring series.
    pub series_id: String,
    /// UTC epoch ms of the original occurrence this override replaces.
    pub override_occurrence_date: i64,
    /// UTC epoch ms occurrences skipped for this recurring series. Populated from
    /// `event_exceptions` when events are listed/fetched; the exceptions table remains the source
    /// of truth.
    pub exception_dates: Vec<i64>,
    pub created_at: i64,
}

/// One bounded event load plus an explicit completeness signal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventListing {
    pub events: Vec<Event>,
    pub has_more: bool,
}

#[derive(Clone, Copy)]
struct EventCandidate<'a>(&'a Event);

impl PartialEq for EventCandidate<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.0.starts_at == other.0.starts_at && self.0.id == other.0.id
    }
}

impl Eq for EventCandidate<'_> {}

impl PartialOrd for EventCandidate<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for EventCandidate<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .starts_at
            .cmp(&other.0.starts_at)
            .then_with(|| self.0.id.cmp(&other.0.id))
    }
}

struct BoundedEventProjection<'a> {
    events: Vec<&'a Event>,
    has_more: bool,
    #[cfg(test)]
    max_retained: usize,
}

fn bounded_owned_event_projection<'a>(
    events: impl IntoIterator<Item = &'a Event>,
    owner_sub: &str,
) -> BoundedEventProjection<'a> {
    let capacity = EVENT_LIST_LIMIT + 1;
    let mut earliest = BinaryHeap::with_capacity(capacity);
    #[cfg(test)]
    let mut max_retained = 0;

    for event in events
        .into_iter()
        .filter(|event| event.owner_sub == owner_sub)
    {
        let candidate = EventCandidate(event);
        if earliest.len() < capacity {
            earliest.push(candidate);
        } else if earliest
            .peek()
            .is_some_and(|latest_retained| candidate < *latest_retained)
        {
            let mut latest_retained = earliest
                .peek_mut()
                .expect("bounded event heap is non-empty at capacity");
            *latest_retained = candidate;
        }
        #[cfg(test)]
        {
            max_retained = max_retained.max(earliest.len());
        }
    }

    let has_more = earliest.len() > EVENT_LIST_LIMIT;
    let mut candidates = earliest.into_vec();
    candidates.sort_unstable();
    let events = candidates
        .into_iter()
        .take(EVENT_LIST_LIMIT)
        .map(|candidate| candidate.0)
        .collect();
    BoundedEventProjection {
        events,
        has_more,
        #[cfg(test)]
        max_retained,
    }
}

/// A user-owned calendar bucket. Events point at `calendar_id`; legacy events with a blank
/// calendar id render in the lazily-created default calendar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Calendar {
    pub id: String,
    pub owner_sub: String,
    pub name: String,
    pub color: String,
    pub position: i64,
}

/// A recurring-series exception: skip the occurrence that originally started at
/// `occurrence_date` (UTC epoch ms).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventException {
    pub id: String,
    pub event_id: String,
    pub owner_sub: String,
    pub occurrence_date: i64,
    pub created_at: i64,
}

pub const DEFAULT_CALENDAR_NAME: &str = "Default";
pub const DEFAULT_CALENDAR_COLOR: &str = "#0891b2";

pub fn default_calendar_id(owner_sub: &str) -> String {
    format!("default:{owner_sub}")
}

pub fn default_calendar_for(owner_sub: &str) -> Calendar {
    Calendar {
        id: default_calendar_id(owner_sub),
        owner_sub: owner_sub.to_string(),
        name: DEFAULT_CALENDAR_NAME.to_string(),
        color: DEFAULT_CALENDAR_COLOR.to_string(),
        position: 0,
    }
}

/// An address-book contact, scoped to one owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contact {
    pub id: String,
    pub owner_sub: String,
    pub name: String,
    pub email: String,
    pub phone: String,
    pub notes: String,
    pub created_at: i64,
}

/// Per-owner display preferences. There is at most ONE row per subject (`owner_sub` is the key),
/// so it is fetched with a plain default when the user has never saved: the whole app keeps
/// working (renders in UTC, Sunday-first) with no settings row present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub owner_sub: String,
    /// Timezone string, e.g. `UTC`, `UTC+08:00`, `UTC-05:00`. Interpreted as a fixed UTC offset.
    pub timezone: String,
    /// First column of the month grid: `sunday` (default) or `monday`.
    pub week_start: String,
    /// Epoch ms of the last save (0 for the synthesized default).
    pub updated_at: i64,
}

impl Settings {
    /// The defaults used until the owner saves anything: UTC, Sunday-first.
    pub fn default_for(owner_sub: &str) -> Self {
        Settings {
            owner_sub: owner_sub.to_string(),
            timezone: "UTC".to_string(),
            week_start: "sunday".to_string(),
            updated_at: 0,
        }
    }
}

/// An invited attendee on one event. Owned (like everything) by the event's `owner_sub`; the
/// per-attendee `token` is an unguessable RSVP capability handed out in a public link, so its
/// lookup is the ONLY store read that is NOT owner-scoped. `status` is one of the four RFC 5545
/// PARTSTAT keywords (see [`ATTENDEE_STATUSES`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attendee {
    pub id: String,
    pub event_id: String,
    pub owner_sub: String,
    pub email: String,
    pub name: String,
    /// `needs-action` (default) | `accepted` | `declined` | `tentative`.
    pub status: String,
    /// Unguessable RSVP capability (64 hex chars); the public `/rsvp/{token}` link carries it.
    pub token: String,
    pub created_at: i64,
}

/// Normalized attendee data accepted by an atomic event-bundle write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttendeeInput {
    pub email: String,
    pub name: String,
}

/// The RSVP states an attendee row may hold. `needs-action` is the initial state; the public RSVP
/// link only ever sets one of the latter three.
pub const ATTENDEE_STATUSES: [&str; 4] = ["needs-action", "accepted", "declined", "tentative"];

/// A pre-event reminder: fire `minutes_before` the owning event's start by delivering an in-app
/// notification. `delivered_at` is `0` until a due-scan hands it off (so a reminder fires at most
/// once); the scan filters on it with a bounded query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reminder {
    pub id: String,
    pub event_id: String,
    pub owner_sub: String,
    pub minutes_before: i64,
    /// Epoch ms of successful hand-off, or `0` while still pending.
    pub delivered_at: i64,
    pub created_at: i64,
}

/// One event and its complete attendee/reminder projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventBundle {
    pub event: Event,
    pub attendees: Vec<AttendeeInput>,
    pub reminder_minutes: Vec<i64>,
    pub now_ms: i64,
    pub reconcile_series: bool,
}

/// A reminder joined to its (still-upcoming) event, produced by [`Store::due_reminders`]. Carries
/// everything the delivery hook needs to build the notification without a second lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DueReminder {
    pub reminder_id: String,
    pub owner_sub: String,
    pub minutes_before: i64,
    pub event_id: String,
    pub event_title: String,
    pub event_starts_at: i64,
    pub event_location: String,
}

fn attendee_email_key(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

fn reconcile_attendee_rows(
    owner_sub: &str,
    event_id: &str,
    inputs: &[AttendeeInput],
    existing: &[Attendee],
    now_ms: i64,
) -> Vec<Attendee> {
    let mut prior = existing.to_vec();
    prior.sort_by(|a, b| a.id.cmp(&b.id));
    let mut by_email: HashMap<String, &Attendee> = HashMap::new();
    for attendee in &prior {
        let key = attendee_email_key(&attendee.email);
        if !key.is_empty() {
            by_email.entry(key).or_insert(attendee);
        }
    }

    let mut seen = HashSet::new();
    inputs
        .iter()
        .filter_map(|input| {
            let email = input.email.trim();
            let key = attendee_email_key(email);
            if key.is_empty() || !seen.insert(key.clone()) {
                return None;
            }
            Some(match by_email.get(&key) {
                Some(previous) => Attendee {
                    id: previous.id.clone(),
                    event_id: event_id.to_string(),
                    owner_sub: owner_sub.to_string(),
                    email: email.to_string(),
                    name: input.name.trim().to_string(),
                    status: previous.status.clone(),
                    token: previous.token.clone(),
                    created_at: previous.created_at,
                },
                None => Attendee {
                    id: crate::auth::random_hex(),
                    event_id: event_id.to_string(),
                    owner_sub: owner_sub.to_string(),
                    email: email.to_string(),
                    name: input.name.trim().to_string(),
                    status: "needs-action".to_string(),
                    token: crate::auth::random_hex(),
                    created_at: now_ms,
                },
            })
        })
        .collect()
}

fn reconcile_reminder_rows(
    owner_sub: &str,
    event_id: &str,
    minutes: &[i64],
    existing: &[Reminder],
    now_ms: i64,
) -> Vec<Reminder> {
    let mut prior = existing.to_vec();
    prior.sort_by(|a, b| a.id.cmp(&b.id));
    let mut by_minute: HashMap<i64, &Reminder> = HashMap::new();
    for reminder in &prior {
        by_minute.entry(reminder.minutes_before).or_insert(reminder);
    }

    let mut values = minutes.to_vec();
    values.sort_unstable();
    values.dedup();
    values
        .into_iter()
        .map(|minutes_before| match by_minute.get(&minutes_before) {
            Some(previous) => Reminder {
                id: previous.id.clone(),
                event_id: event_id.to_string(),
                owner_sub: owner_sub.to_string(),
                minutes_before,
                delivered_at: previous.delivered_at,
                created_at: previous.created_at,
            },
            None => Reminder {
                id: crate::auth::random_hex(),
                event_id: event_id.to_string(),
                owner_sub: owner_sub.to_string(),
                minutes_before,
                delivered_at: 0,
                created_at: now_ms,
            },
        })
        .collect()
}

/// Pluggable store. Methods are `async`: the axum handlers `.await` them directly on the serving
/// runtime, and `PgStore` drives sqlx natively, so a worker thread is never blocked on a DB
/// round-trip (no `block_in_place`, no sync-over-async bridge).
#[async_trait]
pub trait Store: Send + Sync {
    /// The owner's calendars, ordered by `position`, with the default calendar created lazily.
    async fn list_calendars(&self, owner_sub: &str) -> Result<Vec<Calendar>, StoreError>;

    /// One calendar by id, only if it belongs to `owner_sub`.
    async fn get_calendar(&self, owner_sub: &str, id: &str)
        -> Result<Option<Calendar>, StoreError>;

    /// Create or update a calendar. Returns the stored row.
    async fn upsert_calendar(&self, calendar: Calendar) -> Result<Calendar, StoreError>;

    /// Delete an empty, non-default calendar; returns `true` only when a row was removed.
    async fn delete_calendar(&self, owner_sub: &str, id: &str) -> Result<bool, StoreError>;

    /// The first bounded page of the owner's events, ordered by `(starts_at, id)`, plus whether
    /// another row exists.
    async fn list_events(&self, owner_sub: &str) -> Result<EventListing, StoreError>;

    /// One event by id, only if it belongs to `owner_sub`.
    async fn get_event(&self, owner_sub: &str, id: &str) -> Result<Option<Event>, StoreError>;

    /// Create or update an event (insert, or update when the id already exists AND is owned by
    /// the same subject). Returns the stored event.
    async fn upsert_event(&self, event: Event) -> Result<Event, StoreError>;

    /// Delete an event; returns `true` if a row owned by `owner_sub` was removed.
    async fn delete_event(&self, owner_sub: &str, id: &str) -> Result<bool, StoreError>;

    /// Atomically write an event and reconcile its full attendee/reminder projection. A series edit
    /// additionally removes overrides/exceptions that no longer belong to the new recurrence.
    async fn save_event_bundle(&self, bundle: EventBundle) -> Result<Event, StoreError>;

    /// Atomically skip one recurring occurrence and remove its detached override projection.
    async fn delete_occurrence_bundle(
        &self,
        owner_sub: &str,
        series_id: &str,
        occurrence_date: i64,
        exception: EventException,
    ) -> Result<bool, StoreError>;

    /// Atomically delete one event or a complete recurring series tree.
    async fn delete_event_tree(&self, owner_sub: &str, event_id: &str) -> Result<bool, StoreError>;

    /// Insert a recurring occurrence exception. Idempotent per `(owner,event,occurrence_date)`.
    async fn upsert_event_exception(
        &self,
        exception: EventException,
    ) -> Result<EventException, StoreError>;

    /// Remove all occurrence exceptions for a recurring series.
    async fn delete_event_exceptions(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), StoreError>;

    /// The detached override row for one series occurrence, if present.
    async fn get_event_override(
        &self,
        owner_sub: &str,
        series_id: &str,
        occurrence_date: i64,
    ) -> Result<Option<Event>, StoreError>;

    /// Delete one detached override row and return it for caller-side cascades.
    async fn delete_event_override(
        &self,
        owner_sub: &str,
        series_id: &str,
        occurrence_date: i64,
    ) -> Result<Option<Event>, StoreError>;

    /// Delete every detached override for a series and return the removed rows for cascades.
    async fn delete_event_overrides(
        &self,
        owner_sub: &str,
        series_id: &str,
    ) -> Result<Vec<Event>, StoreError>;

    /// All of the owner's contacts, ordered by `name` (case-insensitive), capped.
    async fn list_contacts(&self, owner_sub: &str) -> Result<Vec<Contact>, StoreError>;

    /// One contact by id, only if it belongs to `owner_sub`.
    async fn get_contact(&self, owner_sub: &str, id: &str) -> Result<Option<Contact>, StoreError>;

    /// Create or update a contact (same ownership guard as events). Returns the stored contact.
    async fn upsert_contact(&self, contact: Contact) -> Result<Contact, StoreError>;

    /// Delete a contact; returns `true` if a row owned by `owner_sub` was removed.
    async fn delete_contact(&self, owner_sub: &str, id: &str) -> Result<bool, StoreError>;

    /// The owner's display settings, or [`Settings::default_for`] when no row exists yet.
    async fn get_settings(&self, owner_sub: &str) -> Result<Settings, StoreError>;

    /// Insert or replace the owner's settings row. Returns the stored value.
    async fn upsert_settings(&self, settings: Settings) -> Result<Settings, StoreError>;

    // --- Attendees / invites ----------------------------------------------------------------

    /// The event's attendees, ordered by name (case-insensitive), only if the event belongs to
    /// `owner_sub`.
    async fn list_attendees(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<Vec<Attendee>, StoreError>;

    /// Replace the whole attendee set for `(owner_sub, event_id)` with `attendees` (delete-then-
    /// insert). The caller reconciles tokens/status beforehand so retained emails keep their RSVP
    /// links; this call just persists the final set.
    async fn replace_attendees(
        &self,
        owner_sub: &str,
        event_id: &str,
        attendees: Vec<Attendee>,
    ) -> Result<(), StoreError>;

    /// Remove all attendees of an event (used when the event itself is deleted).
    async fn delete_event_attendees(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), StoreError>;

    /// Look up one attendee by its RSVP `token`. NOT owner-scoped: the token IS the capability.
    async fn get_attendee_by_token(&self, token: &str) -> Result<Option<Attendee>, StoreError>;

    /// Set an attendee's RSVP status by `token`; returns the updated row (or `None` if unknown).
    async fn set_attendee_status_by_token(
        &self,
        token: &str,
        status: &str,
    ) -> Result<Option<Attendee>, StoreError>;

    // --- Reminders --------------------------------------------------------------------------

    /// The event's reminders, ordered by `minutes_before` ascending, only if the event belongs to
    /// `owner_sub`.
    async fn list_reminders(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<Vec<Reminder>, StoreError>;

    /// Replace the whole reminder set for `(owner_sub, event_id)` with `reminders`.
    async fn replace_reminders(
        &self,
        owner_sub: &str,
        event_id: &str,
        reminders: Vec<Reminder>,
    ) -> Result<(), StoreError>;

    /// Remove all reminders of an event (used when the event itself is deleted).
    async fn delete_event_reminders(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), StoreError>;

    /// Pending reminders whose fire time (`event.starts_at - minutes_before`) has arrived while the
    /// event is still upcoming, joined to their event. A SINGLE bounded query (`LIMIT limit`) — the
    /// delivery hook scans on a timer, never a busy loop. Ordered by event start ascending.
    async fn due_reminders(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<DueReminder>, StoreError>;

    /// Mark a reminder delivered (sets `delivered_at`), so it never fires twice.
    async fn mark_reminder_delivered(
        &self,
        reminder_id: &str,
        when_ms: i64,
    ) -> Result<(), StoreError>;
}

// --------------------------------------------------------------------------------------
// In-memory store (the default; keeps the whole service database-free for dev + tests).
// --------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct MemData {
    calendars: HashMap<String, Calendar>,
    events: HashMap<String, Event>,
    exceptions: HashMap<String, EventException>,
    contacts: HashMap<String, Contact>,
    settings: HashMap<String, Settings>,
    /// Attendees keyed by attendee id.
    attendees: HashMap<String, Attendee>,
    /// Reminders keyed by reminder id.
    reminders: HashMap<String, Reminder>,
    #[cfg(test)]
    fail_save_bundle_after_event: bool,
}

/// In-memory `Store`. A single `Mutex` guards both maps.
#[derive(Default)]
pub struct InMemoryStore {
    data: Mutex<MemData>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure_default_calendar(data: &mut MemData, owner_sub: &str) -> Calendar {
        let default = default_calendar_for(owner_sub);
        data.calendars
            .entry(default.id.clone())
            .or_insert_with(|| default.clone())
            .clone()
    }

    fn normalize_event_for_read(data: &MemData, mut event: Event, default_id: &str) -> Event {
        if event.calendar_id.trim().is_empty() {
            event.calendar_id = default_id.to_string();
        }
        event.exception_dates = data
            .exceptions
            .values()
            .filter(|x| x.owner_sub == event.owner_sub && x.event_id == event.id)
            .map(|x| x.occurrence_date)
            .collect();
        event.exception_dates.sort_unstable();
        event.exception_dates.dedup();
        event
    }

    fn normalize_event_for_write(data: &mut MemData, mut event: Event) -> Event {
        let default = Self::ensure_default_calendar(data, &event.owner_sub);
        let calendar_owned = !event.calendar_id.trim().is_empty()
            && data
                .calendars
                .get(&event.calendar_id)
                .map(|c| c.owner_sub == event.owner_sub)
                .unwrap_or(false);
        if !calendar_owned {
            event.calendar_id = default.id;
        }
        event.exception_dates.clear();
        event
    }

    fn event_attendees(data: &MemData, owner_sub: &str, event_id: &str) -> Vec<Attendee> {
        data.attendees
            .values()
            .filter(|attendee| attendee.owner_sub == owner_sub && attendee.event_id == event_id)
            .cloned()
            .collect()
    }

    fn event_reminders(data: &MemData, owner_sub: &str, event_id: &str) -> Vec<Reminder> {
        data.reminders
            .values()
            .filter(|reminder| reminder.owner_sub == owner_sub && reminder.event_id == event_id)
            .cloned()
            .collect()
    }

    fn remove_event_projection(data: &mut MemData, owner_sub: &str, event_id: &str) {
        data.attendees.retain(|_, attendee| {
            !(attendee.owner_sub == owner_sub && attendee.event_id == event_id)
        });
        data.reminders.retain(|_, reminder| {
            !(reminder.owner_sub == owner_sub && reminder.event_id == event_id)
        });
    }

    fn replace_event_projection(
        data: &mut MemData,
        owner_sub: &str,
        event_id: &str,
        attendees: Vec<Attendee>,
        reminders: Vec<Reminder>,
    ) -> Result<(), StoreError> {
        for attendee in &attendees {
            if data.attendees.get(&attendee.id).is_some_and(|existing| {
                existing.owner_sub != owner_sub || existing.event_id != event_id
            }) || data.attendees.values().any(|existing| {
                existing.token == attendee.token
                    && existing.id != attendee.id
                    && (existing.owner_sub != owner_sub || existing.event_id != event_id)
            }) {
                return Err(StoreError::OwnershipConflict);
            }
        }
        for reminder in &reminders {
            if data.reminders.get(&reminder.id).is_some_and(|existing| {
                existing.owner_sub != owner_sub || existing.event_id != event_id
            }) {
                return Err(StoreError::OwnershipConflict);
            }
        }

        Self::remove_event_projection(data, owner_sub, event_id);
        for attendee in attendees {
            data.attendees.insert(attendee.id.clone(), attendee);
        }
        for reminder in reminders {
            data.reminders.insert(reminder.id.clone(), reminder);
        }
        Ok(())
    }

    fn reconcile_series_projection(data: &mut MemData, series: &Event) {
        let invalid_override_ids: Vec<String> = data
            .events
            .values()
            .filter(|event| {
                event.owner_sub == series.owner_sub
                    && event.series_id == series.id
                    && !crate::rrule::occurrence_belongs_to_series(
                        series.starts_at,
                        &series.rrule,
                        event.override_occurrence_date,
                    )
            })
            .map(|event| event.id.clone())
            .collect();
        for override_id in invalid_override_ids {
            Self::remove_event_projection(data, &series.owner_sub, &override_id);
            data.events.remove(&override_id);
        }
        data.exceptions.retain(|_, exception| {
            exception.owner_sub != series.owner_sub
                || exception.event_id != series.id
                || crate::rrule::occurrence_belongs_to_series(
                    series.starts_at,
                    &series.rrule,
                    exception.occurrence_date,
                )
        });
    }

    #[cfg(test)]
    fn fail_next_save_bundle_after_event(&self) {
        self.data
            .lock()
            .expect("almanac store lock poisoned")
            .fail_save_bundle_after_event = true;
    }
}

#[async_trait]
impl Store for InMemoryStore {
    async fn list_calendars(&self, owner_sub: &str) -> Result<Vec<Calendar>, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        Self::ensure_default_calendar(&mut data, owner_sub);
        let mut calendars: Vec<Calendar> = data
            .calendars
            .values()
            .filter(|c| c.owner_sub == owner_sub)
            .cloned()
            .collect();
        calendars.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.id.cmp(&b.id)));
        Ok(calendars)
    }

    async fn get_calendar(
        &self,
        owner_sub: &str,
        id: &str,
    ) -> Result<Option<Calendar>, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        Self::ensure_default_calendar(&mut data, owner_sub);
        Ok(data
            .calendars
            .get(id)
            .filter(|c| c.owner_sub == owner_sub)
            .cloned())
    }

    async fn upsert_calendar(&self, calendar: Calendar) -> Result<Calendar, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        Self::ensure_default_calendar(&mut data, &calendar.owner_sub);
        if let Some(existing) = data.calendars.get(&calendar.id) {
            if existing.owner_sub != calendar.owner_sub {
                return Ok(existing.clone());
            }
        }
        data.calendars.insert(calendar.id.clone(), calendar.clone());
        Ok(calendar)
    }

    async fn delete_calendar(&self, owner_sub: &str, id: &str) -> Result<bool, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        let default = Self::ensure_default_calendar(&mut data, owner_sub);
        if id == default.id {
            return Ok(false);
        }
        let owned = data
            .calendars
            .get(id)
            .map(|c| c.owner_sub == owner_sub)
            .unwrap_or(false);
        if !owned {
            return Ok(false);
        }
        let has_events = data.events.values().any(|e| {
            e.owner_sub == owner_sub
                && if e.calendar_id.trim().is_empty() {
                    default.id == id
                } else {
                    e.calendar_id == id
                }
        });
        if has_events {
            return Ok(false);
        }
        data.calendars.remove(id);
        Ok(true)
    }

    async fn list_events(&self, owner_sub: &str) -> Result<EventListing, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        let default = Self::ensure_default_calendar(&mut data, owner_sub);
        let projection = bounded_owned_event_projection(data.events.values(), owner_sub);
        let events = projection
            .events
            .into_iter()
            .cloned()
            .map(|event| Self::normalize_event_for_read(&data, event, &default.id))
            .collect();
        let has_more = projection.has_more;
        Ok(EventListing { events, has_more })
    }

    async fn get_event(&self, owner_sub: &str, id: &str) -> Result<Option<Event>, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        let default = Self::ensure_default_calendar(&mut data, owner_sub);
        Ok(data
            .events
            .get(id)
            .filter(|e| e.owner_sub == owner_sub)
            .cloned()
            .map(|e| Self::normalize_event_for_read(&data, e, &default.id)))
    }

    async fn upsert_event(&self, event: Event) -> Result<Event, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        let event = Self::normalize_event_for_write(&mut data, event);
        // Ownership guard: never overwrite a row owned by a different subject.
        if let Some(existing) = data.events.get(&event.id) {
            if existing.owner_sub != event.owner_sub {
                return Ok(existing.clone());
            }
        }
        data.events.insert(event.id.clone(), event.clone());
        Ok(event)
    }

    async fn delete_event(&self, owner_sub: &str, id: &str) -> Result<bool, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        let owned = data
            .events
            .get(id)
            .map(|e| e.owner_sub == owner_sub)
            .unwrap_or(false);
        if owned {
            data.events.remove(id);
        }
        Ok(owned)
    }

    async fn save_event_bundle(&self, bundle: EventBundle) -> Result<Event, StoreError> {
        let EventBundle {
            mut event,
            attendees: attendee_inputs,
            reminder_minutes,
            now_ms,
            reconcile_series,
        } = bundle;
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        let mut duplicate_override_ids = Vec::new();
        if !event.series_id.is_empty() {
            let Some(series) = data
                .events
                .get(&event.series_id)
                .filter(|series| series.owner_sub == event.owner_sub)
            else {
                return Err(StoreError::OwnershipConflict);
            };
            if !crate::rrule::occurrence_belongs_to_series(
                series.starts_at,
                &series.rrule,
                event.override_occurrence_date,
            ) {
                return Err(StoreError::OwnershipConflict);
            }
            if let Some(existing_override) = data
                .events
                .values()
                .filter(|existing| {
                    existing.owner_sub == event.owner_sub
                        && existing.series_id == event.series_id
                        && existing.override_occurrence_date == event.override_occurrence_date
                })
                .min_by(|a, b| a.id.cmp(&b.id))
            {
                event.id = existing_override.id.clone();
                event.created_at = existing_override.created_at;
            }
            duplicate_override_ids = data
                .events
                .values()
                .filter(|existing| {
                    existing.owner_sub == event.owner_sub
                        && existing.series_id == event.series_id
                        && existing.override_occurrence_date == event.override_occurrence_date
                        && existing.id != event.id
                })
                .map(|existing| existing.id.clone())
                .collect();
        }
        if let Some(existing) = data.events.get(&event.id) {
            if existing.owner_sub != event.owner_sub
                || (!event.series_id.is_empty()
                    && (existing.series_id != event.series_id
                        || existing.override_occurrence_date != event.override_occurrence_date))
                || (event.series_id.is_empty() && !existing.series_id.is_empty())
            {
                return Err(StoreError::OwnershipConflict);
            }
            event.created_at = existing.created_at;
        }

        let existing_attendees = Self::event_attendees(&data, &event.owner_sub, &event.id);
        let existing_reminders = Self::event_reminders(&data, &event.owner_sub, &event.id);
        let attendees = reconcile_attendee_rows(
            &event.owner_sub,
            &event.id,
            &attendee_inputs,
            &existing_attendees,
            now_ms,
        );
        let reminders = reconcile_reminder_rows(
            &event.owner_sub,
            &event.id,
            &reminder_minutes,
            &existing_reminders,
            now_ms,
        );

        #[cfg(test)]
        let fail_after_event = {
            let fail = data.fail_save_bundle_after_event;
            data.fail_save_bundle_after_event = false;
            fail
        };
        let mut staged = data.clone();
        event = Self::normalize_event_for_write(&mut staged, event);
        staged.events.insert(event.id.clone(), event.clone());

        #[cfg(test)]
        if fail_after_event {
            return Err(StoreError::Backend(
                "injected failure after event write".to_string(),
            ));
        }

        for duplicate_id in duplicate_override_ids {
            Self::remove_event_projection(&mut staged, &event.owner_sub, &duplicate_id);
            staged.events.remove(&duplicate_id);
        }
        if reconcile_series {
            Self::reconcile_series_projection(&mut staged, &event);
        }
        Self::replace_event_projection(
            &mut staged,
            &event.owner_sub,
            &event.id,
            attendees,
            reminders,
        )?;
        *data = staged;
        Ok(event)
    }

    async fn delete_occurrence_bundle(
        &self,
        owner_sub: &str,
        series_id: &str,
        occurrence_date: i64,
        mut exception: EventException,
    ) -> Result<bool, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        let Some(series) = data
            .events
            .get(series_id)
            .filter(|event| event.owner_sub == owner_sub)
        else {
            return Ok(false);
        };
        if !crate::rrule::occurrence_belongs_to_series(
            series.starts_at,
            &series.rrule,
            occurrence_date,
        ) {
            return Ok(false);
        }

        let mut staged = data.clone();
        exception.owner_sub = owner_sub.to_string();
        exception.event_id = series_id.to_string();
        exception.occurrence_date = occurrence_date;
        let exception_exists = staged.exceptions.values().any(|existing| {
            existing.owner_sub == owner_sub
                && existing.event_id == series_id
                && existing.occurrence_date == occurrence_date
        });
        if !exception_exists {
            if staged.exceptions.contains_key(&exception.id) {
                return Err(StoreError::OwnershipConflict);
            }
            staged.exceptions.insert(exception.id.clone(), exception);
        }

        let override_ids: Vec<String> = staged
            .events
            .values()
            .filter(|event| {
                event.owner_sub == owner_sub
                    && event.series_id == series_id
                    && event.override_occurrence_date == occurrence_date
            })
            .map(|event| event.id.clone())
            .collect();
        for override_id in override_ids {
            Self::remove_event_projection(&mut staged, owner_sub, &override_id);
            staged.events.remove(&override_id);
        }
        *data = staged;
        Ok(true)
    }

    async fn delete_event_tree(&self, owner_sub: &str, event_id: &str) -> Result<bool, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        let owned = data
            .events
            .get(event_id)
            .is_some_and(|event| event.owner_sub == owner_sub);
        if !owned {
            return Ok(false);
        }

        let mut staged = data.clone();
        let override_ids: Vec<String> = staged
            .events
            .values()
            .filter(|event| event.owner_sub == owner_sub && event.series_id == event_id)
            .map(|event| event.id.clone())
            .collect();
        for override_id in override_ids {
            Self::remove_event_projection(&mut staged, owner_sub, &override_id);
            staged.events.remove(&override_id);
        }
        staged.exceptions.retain(|_, exception| {
            exception.owner_sub != owner_sub || exception.event_id != event_id
        });
        Self::remove_event_projection(&mut staged, owner_sub, event_id);
        staged.events.remove(event_id);
        *data = staged;
        Ok(true)
    }

    async fn upsert_event_exception(
        &self,
        exception: EventException,
    ) -> Result<EventException, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        let owned = data
            .events
            .get(&exception.event_id)
            .map(|e| e.owner_sub == exception.owner_sub)
            .unwrap_or(false);
        if !owned {
            return Ok(exception);
        }
        if let Some(existing) = data.exceptions.values().find(|x| {
            x.owner_sub == exception.owner_sub
                && x.event_id == exception.event_id
                && x.occurrence_date == exception.occurrence_date
        }) {
            return Ok(existing.clone());
        }
        data.exceptions
            .insert(exception.id.clone(), exception.clone());
        Ok(exception)
    }

    async fn delete_event_exceptions(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        data.exceptions
            .retain(|_, x| !(x.owner_sub == owner_sub && x.event_id == event_id));
        Ok(())
    }

    async fn get_event_override(
        &self,
        owner_sub: &str,
        series_id: &str,
        occurrence_date: i64,
    ) -> Result<Option<Event>, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        let default = Self::ensure_default_calendar(&mut data, owner_sub);
        Ok(data
            .events
            .values()
            .find(|e| {
                e.owner_sub == owner_sub
                    && e.series_id == series_id
                    && e.override_occurrence_date == occurrence_date
            })
            .cloned()
            .map(|e| Self::normalize_event_for_read(&data, e, &default.id)))
    }

    async fn delete_event_override(
        &self,
        owner_sub: &str,
        series_id: &str,
        occurrence_date: i64,
    ) -> Result<Option<Event>, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        let id = data
            .events
            .values()
            .find(|e| {
                e.owner_sub == owner_sub
                    && e.series_id == series_id
                    && e.override_occurrence_date == occurrence_date
            })
            .map(|e| e.id.clone());
        Ok(id.and_then(|id| data.events.remove(&id)))
    }

    async fn delete_event_overrides(
        &self,
        owner_sub: &str,
        series_id: &str,
    ) -> Result<Vec<Event>, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        let ids: Vec<String> = data
            .events
            .values()
            .filter(|e| e.owner_sub == owner_sub && e.series_id == series_id)
            .map(|e| e.id.clone())
            .collect();
        let mut removed = Vec::new();
        for id in ids {
            if let Some(e) = data.events.remove(&id) {
                removed.push(e);
            }
        }
        Ok(removed)
    }

    async fn list_contacts(&self, owner_sub: &str) -> Result<Vec<Contact>, StoreError> {
        let data = self.data.lock().expect("almanac store lock poisoned");
        let mut contacts: Vec<Contact> = data
            .contacts
            .values()
            .filter(|c| c.owner_sub == owner_sub)
            .cloned()
            .collect();
        contacts.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        contacts.truncate(CONTACT_LIST_LIMIT);
        Ok(contacts)
    }

    async fn get_contact(&self, owner_sub: &str, id: &str) -> Result<Option<Contact>, StoreError> {
        let data = self.data.lock().expect("almanac store lock poisoned");
        Ok(data
            .contacts
            .get(id)
            .filter(|c| c.owner_sub == owner_sub)
            .cloned())
    }

    async fn upsert_contact(&self, contact: Contact) -> Result<Contact, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        if let Some(existing) = data.contacts.get(&contact.id) {
            if existing.owner_sub != contact.owner_sub {
                return Ok(existing.clone());
            }
        }
        data.contacts.insert(contact.id.clone(), contact.clone());
        Ok(contact)
    }

    async fn delete_contact(&self, owner_sub: &str, id: &str) -> Result<bool, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        let owned = data
            .contacts
            .get(id)
            .map(|c| c.owner_sub == owner_sub)
            .unwrap_or(false);
        if owned {
            data.contacts.remove(id);
        }
        Ok(owned)
    }

    async fn get_settings(&self, owner_sub: &str) -> Result<Settings, StoreError> {
        let data = self.data.lock().expect("almanac store lock poisoned");
        Ok(data
            .settings
            .get(owner_sub)
            .cloned()
            .unwrap_or_else(|| Settings::default_for(owner_sub)))
    }

    async fn upsert_settings(&self, settings: Settings) -> Result<Settings, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        data.settings
            .insert(settings.owner_sub.clone(), settings.clone());
        Ok(settings)
    }

    async fn list_attendees(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<Vec<Attendee>, StoreError> {
        let data = self.data.lock().expect("almanac store lock poisoned");
        let mut out: Vec<Attendee> = data
            .attendees
            .values()
            .filter(|a| a.owner_sub == owner_sub && a.event_id == event_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.email.to_lowercase().cmp(&b.email.to_lowercase()))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }

    async fn replace_attendees(
        &self,
        owner_sub: &str,
        event_id: &str,
        attendees: Vec<Attendee>,
    ) -> Result<(), StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        data.attendees
            .retain(|_, a| !(a.owner_sub == owner_sub && a.event_id == event_id));
        for a in attendees {
            if a.owner_sub == owner_sub && a.event_id == event_id {
                data.attendees.insert(a.id.clone(), a);
            }
        }
        Ok(())
    }

    async fn delete_event_attendees(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        data.attendees
            .retain(|_, a| !(a.owner_sub == owner_sub && a.event_id == event_id));
        Ok(())
    }

    async fn get_attendee_by_token(&self, token: &str) -> Result<Option<Attendee>, StoreError> {
        let data = self.data.lock().expect("almanac store lock poisoned");
        Ok(data.attendees.values().find(|a| a.token == token).cloned())
    }

    async fn set_attendee_status_by_token(
        &self,
        token: &str,
        status: &str,
    ) -> Result<Option<Attendee>, StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        let id = data
            .attendees
            .values()
            .find(|a| a.token == token)
            .map(|a| a.id.clone());
        match id {
            Some(id) => {
                let a = data.attendees.get_mut(&id).expect("just found");
                a.status = status.to_string();
                Ok(Some(a.clone()))
            }
            None => Ok(None),
        }
    }

    async fn list_reminders(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<Vec<Reminder>, StoreError> {
        let data = self.data.lock().expect("almanac store lock poisoned");
        let mut out: Vec<Reminder> = data
            .reminders
            .values()
            .filter(|r| r.owner_sub == owner_sub && r.event_id == event_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            a.minutes_before
                .cmp(&b.minutes_before)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }

    async fn replace_reminders(
        &self,
        owner_sub: &str,
        event_id: &str,
        reminders: Vec<Reminder>,
    ) -> Result<(), StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        data.reminders
            .retain(|_, r| !(r.owner_sub == owner_sub && r.event_id == event_id));
        for r in reminders {
            if r.owner_sub == owner_sub && r.event_id == event_id {
                data.reminders.insert(r.id.clone(), r);
            }
        }
        Ok(())
    }

    async fn delete_event_reminders(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        data.reminders
            .retain(|_, r| !(r.owner_sub == owner_sub && r.event_id == event_id));
        Ok(())
    }

    async fn due_reminders(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<DueReminder>, StoreError> {
        let data = self.data.lock().expect("almanac store lock poisoned");
        let mut out: Vec<DueReminder> = data
            .reminders
            .values()
            .filter(|r| r.delivered_at == 0)
            .filter_map(|r| {
                let event = data.events.get(&r.event_id)?;
                if event.owner_sub != r.owner_sub {
                    return None;
                }
                let fire_at = event.starts_at - r.minutes_before * 60_000;
                // Due once the fire time has arrived AND the event has not yet started.
                if fire_at <= now_ms && event.starts_at >= now_ms {
                    Some(DueReminder {
                        reminder_id: r.id.clone(),
                        owner_sub: r.owner_sub.clone(),
                        minutes_before: r.minutes_before,
                        event_id: event.id.clone(),
                        event_title: event.title.clone(),
                        event_starts_at: event.starts_at,
                        event_location: event.location.clone(),
                    })
                } else {
                    None
                }
            })
            .collect();
        out.sort_by(|a, b| {
            a.event_starts_at
                .cmp(&b.event_starts_at)
                .then_with(|| a.reminder_id.cmp(&b.reminder_id))
        });
        out.truncate(limit);
        Ok(out)
    }

    async fn mark_reminder_delivered(
        &self,
        reminder_id: &str,
        when_ms: i64,
    ) -> Result<(), StoreError> {
        let mut data = self.data.lock().expect("almanac store lock poisoned");
        if let Some(r) = data.reminders.get_mut(reminder_id) {
            r.delivered_at = when_ms;
        }
        Ok(())
    }
}

// --------------------------------------------------------------------------------------
// PostgreSQL-backed store (portable: standard SQL, runtime queries, no macros).
// --------------------------------------------------------------------------------------
//
// Selected at runtime by `ALMANAC_STORE=postgres`. The `Store` trait is async, so each method
// uses sqlx natively and the handlers `.await` it on the serving runtime — there is NO
// `block_in_place` and NO sync-over-async. The upsert `ON CONFLICT (id) DO UPDATE ... WHERE
// <table>.owner_sub = EXCLUDED.owner_sub` clause is a second ownership guard at the SQL layer.

use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use sqlx::{Postgres, Row, Transaction};

/// PostgreSQL-backed [`Store`].
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

    /// Construct from an existing pool (used by tests that share a pool).
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Idempotent, portable migration. Standard SQL only — safe to run on every startup.
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS events (\
                 id TEXT PRIMARY KEY, \
                 owner_sub TEXT NOT NULL, \
                 title TEXT NOT NULL, \
                 starts_at BIGINT NOT NULL, \
                 ends_at BIGINT NOT NULL, \
                 all_day BOOLEAN NOT NULL DEFAULT FALSE, \
                 location TEXT NOT NULL DEFAULT '', \
                 notes TEXT NOT NULL DEFAULT '', \
                 created_at BIGINT NOT NULL\
             )",
        )
        .execute(&self.pool)
        .await?;
        // Recurrence: an optional RRULE column added idempotently so an existing `events` table
        // migrates forward with no data loss. Portable standard SQL (TEXT NOT NULL DEFAULT '').
        sqlx::query("ALTER TABLE events ADD COLUMN IF NOT EXISTS rrule TEXT NOT NULL DEFAULT ''")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "ALTER TABLE events ADD COLUMN IF NOT EXISTS calendar_id TEXT NOT NULL DEFAULT ''",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE events ADD COLUMN IF NOT EXISTS series_id TEXT NOT NULL DEFAULT ''",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE events ADD COLUMN IF NOT EXISTS override_occurrence_date BIGINT NOT NULL DEFAULT 0",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_events_owner_start ON events (owner_sub, starts_at)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_events_owner_calendar ON events (owner_sub, calendar_id)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_events_owner_series ON events (owner_sub, series_id, override_occurrence_date)",
        )
        .execute(&self.pool)
        .await?;

        // Multiple calendars: lazily-created per-owner default plus user-created buckets.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS calendars (\
                 id TEXT PRIMARY KEY, \
                 owner_sub TEXT NOT NULL, \
                 name TEXT NOT NULL, \
                 color TEXT NOT NULL DEFAULT '#0891b2', \
                 position BIGINT NOT NULL DEFAULT 0\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_calendars_owner_position ON calendars (owner_sub, position)",
        )
        .execute(&self.pool)
        .await?;

        // Recurrence exceptions: `occurrence_date` is the original occurrence start UTC epoch ms.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS event_exceptions (\
                 id TEXT PRIMARY KEY, \
                 event_id TEXT NOT NULL, \
                 owner_sub TEXT NOT NULL, \
                 occurrence_date BIGINT NOT NULL, \
                 created_at BIGINT NOT NULL\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_event_exceptions_once ON event_exceptions (owner_sub, event_id, occurrence_date)",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS contacts (\
                 id TEXT PRIMARY KEY, \
                 owner_sub TEXT NOT NULL, \
                 name TEXT NOT NULL, \
                 email TEXT NOT NULL DEFAULT '', \
                 phone TEXT NOT NULL DEFAULT '', \
                 notes TEXT NOT NULL DEFAULT '', \
                 created_at BIGINT NOT NULL\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_contacts_owner_name ON contacts (owner_sub, name)",
        )
        .execute(&self.pool)
        .await?;

        // Per-owner settings: one row per subject. Created as a new table; the ADD COLUMN IF NOT
        // EXISTS statements make the migration forward-safe (idempotent) if an older, narrower
        // `settings` table already exists. Portable standard SQL only.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS settings (\
                 owner_sub TEXT PRIMARY KEY\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE settings ADD COLUMN IF NOT EXISTS timezone TEXT NOT NULL DEFAULT 'UTC'",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE settings ADD COLUMN IF NOT EXISTS week_start TEXT NOT NULL DEFAULT 'sunday'",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "ALTER TABLE settings ADD COLUMN IF NOT EXISTS updated_at BIGINT NOT NULL DEFAULT 0",
        )
        .execute(&self.pool)
        .await?;

        // Invites / attendees: one row per invited attendee of an event. `token` is the unguessable
        // public RSVP capability (unique). Portable standard SQL only.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS event_attendees (\
                 id TEXT PRIMARY KEY, \
                 event_id TEXT NOT NULL, \
                 owner_sub TEXT NOT NULL, \
                 email TEXT NOT NULL DEFAULT '', \
                 name TEXT NOT NULL DEFAULT '', \
                 status TEXT NOT NULL DEFAULT 'needs-action', \
                 token TEXT NOT NULL, \
                 created_at BIGINT NOT NULL\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_attendees_owner_event ON event_attendees (owner_sub, event_id)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_attendees_token ON event_attendees (token)",
        )
        .execute(&self.pool)
        .await?;

        // Reminders: one row per (event, minutes_before). `delivered_at = 0` means pending; the
        // bounded due-scan filters on it. Portable standard SQL only.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS event_reminders (\
                 id TEXT PRIMARY KEY, \
                 event_id TEXT NOT NULL, \
                 owner_sub TEXT NOT NULL, \
                 minutes_before BIGINT NOT NULL, \
                 delivered_at BIGINT NOT NULL DEFAULT 0, \
                 created_at BIGINT NOT NULL\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_reminders_owner_event ON event_reminders (owner_sub, event_id)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_reminders_due ON event_reminders (delivered_at)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    fn event_from_row(row: &PgRow) -> Result<Event, sqlx::Error> {
        Ok(Event {
            id: row.try_get("id")?,
            owner_sub: row.try_get("owner_sub")?,
            calendar_id: row.try_get("calendar_id")?,
            title: row.try_get("title")?,
            starts_at: row.try_get("starts_at")?,
            ends_at: row.try_get("ends_at")?,
            all_day: row.try_get("all_day")?,
            location: row.try_get("location")?,
            notes: row.try_get("notes")?,
            rrule: row.try_get("rrule")?,
            series_id: row.try_get("series_id")?,
            override_occurrence_date: row.try_get("override_occurrence_date")?,
            exception_dates: Vec::new(),
            created_at: row.try_get("created_at")?,
        })
    }

    fn calendar_from_row(row: &PgRow) -> Result<Calendar, sqlx::Error> {
        Ok(Calendar {
            id: row.try_get("id")?,
            owner_sub: row.try_get("owner_sub")?,
            name: row.try_get("name")?,
            color: row.try_get("color")?,
            position: row.try_get("position")?,
        })
    }

    fn contact_from_row(row: &PgRow) -> Result<Contact, sqlx::Error> {
        Ok(Contact {
            id: row.try_get("id")?,
            owner_sub: row.try_get("owner_sub")?,
            name: row.try_get("name")?,
            email: row.try_get("email")?,
            phone: row.try_get("phone")?,
            notes: row.try_get("notes")?,
            created_at: row.try_get("created_at")?,
        })
    }

    fn settings_from_row(row: &PgRow) -> Result<Settings, sqlx::Error> {
        Ok(Settings {
            owner_sub: row.try_get("owner_sub")?,
            timezone: row.try_get("timezone")?,
            week_start: row.try_get("week_start")?,
            updated_at: row.try_get("updated_at")?,
        })
    }

    fn attendee_from_row(row: &PgRow) -> Result<Attendee, sqlx::Error> {
        Ok(Attendee {
            id: row.try_get("id")?,
            event_id: row.try_get("event_id")?,
            owner_sub: row.try_get("owner_sub")?,
            email: row.try_get("email")?,
            name: row.try_get("name")?,
            status: row.try_get("status")?,
            token: row.try_get("token")?,
            created_at: row.try_get("created_at")?,
        })
    }

    fn reminder_from_row(row: &PgRow) -> Result<Reminder, sqlx::Error> {
        Ok(Reminder {
            id: row.try_get("id")?,
            event_id: row.try_get("event_id")?,
            owner_sub: row.try_get("owner_sub")?,
            minutes_before: row.try_get("minutes_before")?,
            delivered_at: row.try_get("delivered_at")?,
            created_at: row.try_get("created_at")?,
        })
    }

    fn event_exception_from_row(row: &PgRow) -> Result<EventException, sqlx::Error> {
        Ok(EventException {
            id: row.try_get("id")?,
            event_id: row.try_get("event_id")?,
            owner_sub: row.try_get("owner_sub")?,
            occurrence_date: row.try_get("occurrence_date")?,
            created_at: row.try_get("created_at")?,
        })
    }

    fn due_reminder_from_row(row: &PgRow) -> Result<DueReminder, sqlx::Error> {
        Ok(DueReminder {
            reminder_id: row.try_get("reminder_id")?,
            owner_sub: row.try_get("owner_sub")?,
            minutes_before: row.try_get("minutes_before")?,
            event_id: row.try_get("event_id")?,
            event_title: row.try_get("event_title")?,
            event_starts_at: row.try_get("event_starts_at")?,
            event_location: row.try_get("event_location")?,
        })
    }

    async fn lock_event_tx(
        tx: &mut Transaction<'_, Postgres>,
        event_id: &str,
    ) -> Result<Option<Event>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, owner_sub, calendar_id, title, starts_at, ends_at, all_day, location, \
                    notes, rrule, series_id, override_occurrence_date, created_at \
             FROM events WHERE id = $1 FOR UPDATE",
        )
        .bind(event_id)
        .fetch_optional(&mut **tx)
        .await?;
        row.as_ref().map(Self::event_from_row).transpose()
    }

    async fn lock_series_overrides_tx(
        tx: &mut Transaction<'_, Postgres>,
        owner_sub: &str,
        series_id: &str,
    ) -> Result<Vec<Event>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, owner_sub, calendar_id, title, starts_at, ends_at, all_day, location, \
                    notes, rrule, series_id, override_occurrence_date, created_at \
             FROM events WHERE owner_sub = $1 AND series_id = $2 ORDER BY id ASC FOR UPDATE",
        )
        .bind(owner_sub)
        .bind(series_id)
        .fetch_all(&mut **tx)
        .await?;
        rows.iter().map(Self::event_from_row).collect()
    }

    async fn lock_occurrence_overrides_tx(
        tx: &mut Transaction<'_, Postgres>,
        owner_sub: &str,
        series_id: &str,
        occurrence_date: i64,
    ) -> Result<Vec<Event>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, owner_sub, calendar_id, title, starts_at, ends_at, all_day, location, \
                    notes, rrule, series_id, override_occurrence_date, created_at \
             FROM events \
             WHERE owner_sub = $1 AND series_id = $2 AND override_occurrence_date = $3 \
             ORDER BY id ASC FOR UPDATE",
        )
        .bind(owner_sub)
        .bind(series_id)
        .bind(occurrence_date)
        .fetch_all(&mut **tx)
        .await?;
        rows.iter().map(Self::event_from_row).collect()
    }

    async fn lock_series_exceptions_tx(
        tx: &mut Transaction<'_, Postgres>,
        owner_sub: &str,
        series_id: &str,
    ) -> Result<Vec<EventException>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, event_id, owner_sub, occurrence_date, created_at \
             FROM event_exceptions WHERE owner_sub = $1 AND event_id = $2 \
             ORDER BY id ASC FOR UPDATE",
        )
        .bind(owner_sub)
        .bind(series_id)
        .fetch_all(&mut **tx)
        .await?;
        rows.iter().map(Self::event_exception_from_row).collect()
    }

    async fn lock_attendees_tx(
        tx: &mut Transaction<'_, Postgres>,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<Vec<Attendee>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, event_id, owner_sub, email, name, status, token, created_at \
             FROM event_attendees WHERE owner_sub = $1 AND event_id = $2 \
             ORDER BY id ASC FOR UPDATE",
        )
        .bind(owner_sub)
        .bind(event_id)
        .fetch_all(&mut **tx)
        .await?;
        rows.iter().map(Self::attendee_from_row).collect()
    }

    async fn lock_reminders_tx(
        tx: &mut Transaction<'_, Postgres>,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<Vec<Reminder>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, event_id, owner_sub, minutes_before, delivered_at, created_at \
             FROM event_reminders WHERE owner_sub = $1 AND event_id = $2 \
             ORDER BY id ASC FOR UPDATE",
        )
        .bind(owner_sub)
        .bind(event_id)
        .fetch_all(&mut **tx)
        .await?;
        rows.iter().map(Self::reminder_from_row).collect()
    }

    async fn ensure_default_calendar_tx(
        tx: &mut Transaction<'_, Postgres>,
        owner_sub: &str,
    ) -> Result<Calendar, sqlx::Error> {
        let default = default_calendar_for(owner_sub);
        sqlx::query(
            "INSERT INTO calendars (id, owner_sub, name, color, position) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (id) DO NOTHING",
        )
        .bind(&default.id)
        .bind(&default.owner_sub)
        .bind(&default.name)
        .bind(&default.color)
        .bind(default.position)
        .execute(&mut **tx)
        .await?;
        Ok(default)
    }

    async fn normalize_calendar_id_tx(
        tx: &mut Transaction<'_, Postgres>,
        owner_sub: &str,
        calendar_id: &str,
    ) -> Result<String, sqlx::Error> {
        let default = Self::ensure_default_calendar_tx(tx, owner_sub).await?;
        if calendar_id.trim().is_empty() {
            return Ok(default.id);
        }
        let owned =
            sqlx::query("SELECT id FROM calendars WHERE id = $1 AND owner_sub = $2 FOR SHARE")
                .bind(calendar_id)
                .bind(owner_sub)
                .fetch_optional(&mut **tx)
                .await?
                .is_some();
        Ok(if owned {
            calendar_id.to_string()
        } else {
            default.id
        })
    }

    async fn upsert_event_tx(
        tx: &mut Transaction<'_, Postgres>,
        event: &Event,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "INSERT INTO events \
                 (id, owner_sub, calendar_id, title, starts_at, ends_at, all_day, location, notes, \
                  rrule, series_id, override_occurrence_date, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
             ON CONFLICT (id) DO UPDATE SET \
                 title = EXCLUDED.title, \
                 calendar_id = EXCLUDED.calendar_id, \
                 starts_at = EXCLUDED.starts_at, \
                 ends_at = EXCLUDED.ends_at, \
                 all_day = EXCLUDED.all_day, \
                 location = EXCLUDED.location, \
                 notes = EXCLUDED.notes, \
                 rrule = EXCLUDED.rrule, \
                 series_id = EXCLUDED.series_id, \
                 override_occurrence_date = EXCLUDED.override_occurrence_date \
             WHERE events.owner_sub = EXCLUDED.owner_sub",
        )
        .bind(&event.id)
        .bind(&event.owner_sub)
        .bind(&event.calendar_id)
        .bind(&event.title)
        .bind(event.starts_at)
        .bind(event.ends_at)
        .bind(event.all_day)
        .bind(&event.location)
        .bind(&event.notes)
        .bind(&event.rrule)
        .bind(&event.series_id)
        .bind(event.override_occurrence_date)
        .bind(event.created_at)
        .execute(&mut **tx)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn delete_event_projection_tx(
        tx: &mut Transaction<'_, Postgres>,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM event_attendees WHERE owner_sub = $1 AND event_id = $2")
            .bind(owner_sub)
            .bind(event_id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM event_reminders WHERE owner_sub = $1 AND event_id = $2")
            .bind(owner_sub)
            .bind(event_id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    async fn replace_event_projection_tx(
        tx: &mut Transaction<'_, Postgres>,
        owner_sub: &str,
        event_id: &str,
        attendees: &[Attendee],
        reminders: &[Reminder],
    ) -> Result<(), sqlx::Error> {
        Self::delete_event_projection_tx(tx, owner_sub, event_id).await?;
        for attendee in attendees {
            sqlx::query(
                "INSERT INTO event_attendees \
                     (id, event_id, owner_sub, email, name, status, token, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(&attendee.id)
            .bind(&attendee.event_id)
            .bind(&attendee.owner_sub)
            .bind(&attendee.email)
            .bind(&attendee.name)
            .bind(&attendee.status)
            .bind(&attendee.token)
            .bind(attendee.created_at)
            .execute(&mut **tx)
            .await?;
        }
        for reminder in reminders {
            sqlx::query(
                "INSERT INTO event_reminders \
                     (id, event_id, owner_sub, minutes_before, delivered_at, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(&reminder.id)
            .bind(&reminder.event_id)
            .bind(&reminder.owner_sub)
            .bind(reminder.minutes_before)
            .bind(reminder.delivered_at)
            .bind(reminder.created_at)
            .execute(&mut **tx)
            .await?;
        }
        Ok(())
    }

    async fn ensure_default_calendar_async(
        &self,
        owner_sub: &str,
    ) -> Result<Calendar, sqlx::Error> {
        let default = default_calendar_for(owner_sub);
        sqlx::query(
            "INSERT INTO calendars (id, owner_sub, name, color, position) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&default.id)
        .bind(&default.owner_sub)
        .bind(&default.name)
        .bind(&default.color)
        .bind(default.position)
        .execute(&self.pool)
        .await?;
        Ok(default)
    }

    async fn normalize_calendar_id_async(
        &self,
        owner_sub: &str,
        calendar_id: &str,
    ) -> Result<String, sqlx::Error> {
        let default = self.ensure_default_calendar_async(owner_sub).await?;
        if calendar_id.trim().is_empty() {
            return Ok(default.id);
        }
        let exists = sqlx::query("SELECT id FROM calendars WHERE id = $1 AND owner_sub = $2")
            .bind(calendar_id)
            .bind(owner_sub)
            .fetch_optional(&self.pool)
            .await?
            .is_some();
        if exists {
            Ok(calendar_id.to_string())
        } else {
            Ok(default.id)
        }
    }

    async fn attach_event_exceptions(
        &self,
        owner_sub: &str,
        events: &mut [Event],
    ) -> Result<(), sqlx::Error> {
        let rows = sqlx::query(
            "SELECT event_id, occurrence_date FROM event_exceptions \
             WHERE owner_sub = $1 ORDER BY event_id ASC, occurrence_date ASC",
        )
        .bind(owner_sub)
        .fetch_all(&self.pool)
        .await?;
        let mut by_event: HashMap<String, Vec<i64>> = HashMap::new();
        for row in rows {
            let event_id: String = row.try_get("event_id")?;
            let occurrence_date: i64 = row.try_get("occurrence_date")?;
            by_event.entry(event_id).or_default().push(occurrence_date);
        }
        for event in events {
            if let Some(mut dates) = by_event.remove(&event.id) {
                dates.sort_unstable();
                dates.dedup();
                event.exception_dates = dates;
            }
        }
        Ok(())
    }

    async fn list_calendars_async(&self, owner_sub: &str) -> Result<Vec<Calendar>, sqlx::Error> {
        self.ensure_default_calendar_async(owner_sub).await?;
        let rows = sqlx::query(
            "SELECT id, owner_sub, name, color, position FROM calendars \
             WHERE owner_sub = $1 ORDER BY position ASC, id ASC",
        )
        .bind(owner_sub)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::calendar_from_row).collect()
    }

    async fn get_calendar_async(
        &self,
        owner_sub: &str,
        id: &str,
    ) -> Result<Option<Calendar>, sqlx::Error> {
        self.ensure_default_calendar_async(owner_sub).await?;
        let row = sqlx::query(
            "SELECT id, owner_sub, name, color, position FROM calendars \
             WHERE id = $1 AND owner_sub = $2",
        )
        .bind(id)
        .bind(owner_sub)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(Self::calendar_from_row).transpose()
    }

    async fn upsert_calendar_async(&self, c: &Calendar) -> Result<(), sqlx::Error> {
        self.ensure_default_calendar_async(&c.owner_sub).await?;
        sqlx::query(
            "INSERT INTO calendars (id, owner_sub, name, color, position) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (id) DO UPDATE SET \
                 name = EXCLUDED.name, \
                 color = EXCLUDED.color, \
                 position = EXCLUDED.position \
             WHERE calendars.owner_sub = EXCLUDED.owner_sub",
        )
        .bind(&c.id)
        .bind(&c.owner_sub)
        .bind(&c.name)
        .bind(&c.color)
        .bind(c.position)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_calendar_async(&self, owner_sub: &str, id: &str) -> Result<bool, sqlx::Error> {
        let default = self.ensure_default_calendar_async(owner_sub).await?;
        if id == default.id {
            return Ok(false);
        }
        let count: i64 = sqlx::query(
            "SELECT count(*) AS n FROM events \
             WHERE owner_sub = $1 AND calendar_id = $2",
        )
        .bind(owner_sub)
        .bind(id)
        .fetch_one(&self.pool)
        .await?
        .try_get("n")?;
        if count > 0 {
            return Ok(false);
        }
        let res = sqlx::query("DELETE FROM calendars WHERE id = $1 AND owner_sub = $2")
            .bind(id)
            .bind(owner_sub)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn list_events_async(&self, owner_sub: &str) -> Result<EventListing, sqlx::Error> {
        let default = self.ensure_default_calendar_async(owner_sub).await?;
        let rows = sqlx::query(
            "SELECT id, owner_sub, \
                    CASE WHEN calendar_id = '' THEN $2 ELSE calendar_id END AS calendar_id, \
                    title, starts_at, ends_at, all_day, location, notes, rrule, \
                    series_id, override_occurrence_date, created_at \
             FROM events WHERE owner_sub = $1 ORDER BY starts_at ASC, id ASC LIMIT $3",
        )
        .bind(owner_sub)
        .bind(&default.id)
        .bind((EVENT_LIST_LIMIT + 1) as i64)
        .fetch_all(&self.pool)
        .await?;
        let mut events: Vec<Event> = rows
            .iter()
            .map(Self::event_from_row)
            .collect::<Result<_, _>>()?;
        let has_more = events.len() > EVENT_LIST_LIMIT;
        events.truncate(EVENT_LIST_LIMIT);
        self.attach_event_exceptions(owner_sub, &mut events).await?;
        Ok(EventListing { events, has_more })
    }

    async fn get_event_async(
        &self,
        owner_sub: &str,
        id: &str,
    ) -> Result<Option<Event>, sqlx::Error> {
        let default = self.ensure_default_calendar_async(owner_sub).await?;
        let row = sqlx::query(
            "SELECT id, owner_sub, \
                    CASE WHEN calendar_id = '' THEN $3 ELSE calendar_id END AS calendar_id, \
                    title, starts_at, ends_at, all_day, location, notes, rrule, \
                    series_id, override_occurrence_date, created_at \
             FROM events WHERE id = $1 AND owner_sub = $2",
        )
        .bind(id)
        .bind(owner_sub)
        .bind(&default.id)
        .fetch_optional(&self.pool)
        .await?;
        let mut event = row.as_ref().map(Self::event_from_row).transpose()?;
        if let Some(e) = event.as_mut() {
            self.attach_event_exceptions(owner_sub, std::slice::from_mut(e))
                .await?;
        }
        Ok(event)
    }

    async fn upsert_event_async(&self, e: &Event) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO events \
                 (id, owner_sub, calendar_id, title, starts_at, ends_at, all_day, location, notes, \
                  rrule, series_id, override_occurrence_date, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
             ON CONFLICT (id) DO UPDATE SET \
                 title = EXCLUDED.title, \
                 calendar_id = EXCLUDED.calendar_id, \
                 starts_at = EXCLUDED.starts_at, \
                 ends_at = EXCLUDED.ends_at, \
                 all_day = EXCLUDED.all_day, \
                 location = EXCLUDED.location, \
                 notes = EXCLUDED.notes, \
                 rrule = EXCLUDED.rrule, \
                 series_id = EXCLUDED.series_id, \
                 override_occurrence_date = EXCLUDED.override_occurrence_date \
             WHERE events.owner_sub = EXCLUDED.owner_sub",
        )
        .bind(&e.id)
        .bind(&e.owner_sub)
        .bind(&e.calendar_id)
        .bind(&e.title)
        .bind(e.starts_at)
        .bind(e.ends_at)
        .bind(e.all_day)
        .bind(&e.location)
        .bind(&e.notes)
        .bind(&e.rrule)
        .bind(&e.series_id)
        .bind(e.override_occurrence_date)
        .bind(e.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_event_async(&self, owner_sub: &str, id: &str) -> Result<bool, sqlx::Error> {
        let res = sqlx::query("DELETE FROM events WHERE id = $1 AND owner_sub = $2")
            .bind(id)
            .bind(owner_sub)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn upsert_event_exception_async(&self, x: &EventException) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO event_exceptions (id, event_id, owner_sub, occurrence_date, created_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (owner_sub, event_id, occurrence_date) DO UPDATE SET \
                 occurrence_date = EXCLUDED.occurrence_date",
        )
        .bind(&x.id)
        .bind(&x.event_id)
        .bind(&x.owner_sub)
        .bind(x.occurrence_date)
        .bind(x.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_event_exceptions_async(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM event_exceptions WHERE owner_sub = $1 AND event_id = $2")
            .bind(owner_sub)
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn get_event_override_async(
        &self,
        owner_sub: &str,
        series_id: &str,
        occurrence_date: i64,
    ) -> Result<Option<Event>, sqlx::Error> {
        let default = self.ensure_default_calendar_async(owner_sub).await?;
        let row = sqlx::query(
            "SELECT id, owner_sub, \
                    CASE WHEN calendar_id = '' THEN $4 ELSE calendar_id END AS calendar_id, \
                    title, starts_at, ends_at, all_day, location, notes, rrule, \
                    series_id, override_occurrence_date, created_at \
             FROM events \
             WHERE owner_sub = $1 AND series_id = $2 AND override_occurrence_date = $3 \
             ORDER BY id ASC LIMIT 1",
        )
        .bind(owner_sub)
        .bind(series_id)
        .bind(occurrence_date)
        .bind(&default.id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(Self::event_from_row).transpose()
    }

    async fn list_event_overrides_async(
        &self,
        owner_sub: &str,
        series_id: &str,
    ) -> Result<Vec<Event>, sqlx::Error> {
        let default = self.ensure_default_calendar_async(owner_sub).await?;
        let rows = sqlx::query(
            "SELECT id, owner_sub, \
                    CASE WHEN calendar_id = '' THEN $3 ELSE calendar_id END AS calendar_id, \
                    title, starts_at, ends_at, all_day, location, notes, rrule, \
                    series_id, override_occurrence_date, created_at \
             FROM events WHERE owner_sub = $1 AND series_id = $2 \
             ORDER BY override_occurrence_date ASC, id ASC",
        )
        .bind(owner_sub)
        .bind(series_id)
        .bind(&default.id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::event_from_row).collect()
    }

    async fn delete_event_override_async(
        &self,
        owner_sub: &str,
        series_id: &str,
        occurrence_date: i64,
    ) -> Result<Option<Event>, sqlx::Error> {
        let event = self
            .get_event_override_async(owner_sub, series_id, occurrence_date)
            .await?;
        if let Some(e) = event.as_ref() {
            sqlx::query("DELETE FROM events WHERE id = $1 AND owner_sub = $2")
                .bind(&e.id)
                .bind(owner_sub)
                .execute(&self.pool)
                .await?;
        }
        Ok(event)
    }

    async fn delete_event_overrides_async(
        &self,
        owner_sub: &str,
        series_id: &str,
    ) -> Result<Vec<Event>, sqlx::Error> {
        let events = self
            .list_event_overrides_async(owner_sub, series_id)
            .await?;
        sqlx::query("DELETE FROM events WHERE owner_sub = $1 AND series_id = $2")
            .bind(owner_sub)
            .bind(series_id)
            .execute(&self.pool)
            .await?;
        Ok(events)
    }

    async fn list_contacts_async(&self, owner_sub: &str) -> Result<Vec<Contact>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, owner_sub, name, email, phone, notes, created_at \
             FROM contacts WHERE owner_sub = $1 ORDER BY lower(name) ASC, id ASC LIMIT $2",
        )
        .bind(owner_sub)
        .bind(CONTACT_LIST_LIMIT as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::contact_from_row).collect()
    }

    async fn get_contact_async(
        &self,
        owner_sub: &str,
        id: &str,
    ) -> Result<Option<Contact>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, owner_sub, name, email, phone, notes, created_at \
             FROM contacts WHERE id = $1 AND owner_sub = $2",
        )
        .bind(id)
        .bind(owner_sub)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(Self::contact_from_row).transpose()
    }

    async fn upsert_contact_async(&self, c: &Contact) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO contacts (id, owner_sub, name, email, phone, notes, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (id) DO UPDATE SET \
                 name = EXCLUDED.name, \
                 email = EXCLUDED.email, \
                 phone = EXCLUDED.phone, \
                 notes = EXCLUDED.notes \
             WHERE contacts.owner_sub = EXCLUDED.owner_sub",
        )
        .bind(&c.id)
        .bind(&c.owner_sub)
        .bind(&c.name)
        .bind(&c.email)
        .bind(&c.phone)
        .bind(&c.notes)
        .bind(c.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_contact_async(&self, owner_sub: &str, id: &str) -> Result<bool, sqlx::Error> {
        let res = sqlx::query("DELETE FROM contacts WHERE id = $1 AND owner_sub = $2")
            .bind(id)
            .bind(owner_sub)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn get_settings_async(&self, owner_sub: &str) -> Result<Settings, sqlx::Error> {
        let row = sqlx::query(
            "SELECT owner_sub, timezone, week_start, updated_at FROM settings WHERE owner_sub = $1",
        )
        .bind(owner_sub)
        .fetch_optional(&self.pool)
        .await?;
        match row.as_ref() {
            Some(r) => Self::settings_from_row(r),
            None => Ok(Settings::default_for(owner_sub)),
        }
    }

    async fn upsert_settings_async(&self, s: &Settings) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO settings (owner_sub, timezone, week_start, updated_at) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (owner_sub) DO UPDATE SET \
                 timezone = EXCLUDED.timezone, \
                 week_start = EXCLUDED.week_start, \
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(&s.owner_sub)
        .bind(&s.timezone)
        .bind(&s.week_start)
        .bind(s.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_attendees_async(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<Vec<Attendee>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, event_id, owner_sub, email, name, status, token, created_at \
             FROM event_attendees WHERE owner_sub = $1 AND event_id = $2 \
             ORDER BY lower(name) ASC, lower(email) ASC, id ASC",
        )
        .bind(owner_sub)
        .bind(event_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::attendee_from_row).collect()
    }

    async fn replace_attendees_async(
        &self,
        owner_sub: &str,
        event_id: &str,
        attendees: &[Attendee],
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM event_attendees WHERE owner_sub = $1 AND event_id = $2")
            .bind(owner_sub)
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        for a in attendees {
            if a.owner_sub != owner_sub || a.event_id != event_id {
                continue;
            }
            sqlx::query(
                "INSERT INTO event_attendees \
                     (id, event_id, owner_sub, email, name, status, token, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(&a.id)
            .bind(&a.event_id)
            .bind(&a.owner_sub)
            .bind(&a.email)
            .bind(&a.name)
            .bind(&a.status)
            .bind(&a.token)
            .bind(a.created_at)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    async fn delete_event_attendees_async(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM event_attendees WHERE owner_sub = $1 AND event_id = $2")
            .bind(owner_sub)
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn get_attendee_by_token_async(
        &self,
        token: &str,
    ) -> Result<Option<Attendee>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, event_id, owner_sub, email, name, status, token, created_at \
             FROM event_attendees WHERE token = $1",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(Self::attendee_from_row).transpose()
    }

    async fn set_attendee_status_by_token_async(
        &self,
        token: &str,
        status: &str,
    ) -> Result<Option<Attendee>, sqlx::Error> {
        sqlx::query("UPDATE event_attendees SET status = $1 WHERE token = $2")
            .bind(status)
            .bind(token)
            .execute(&self.pool)
            .await?;
        self.get_attendee_by_token_async(token).await
    }

    async fn list_reminders_async(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<Vec<Reminder>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, event_id, owner_sub, minutes_before, delivered_at, created_at \
             FROM event_reminders WHERE owner_sub = $1 AND event_id = $2 \
             ORDER BY minutes_before ASC, id ASC",
        )
        .bind(owner_sub)
        .bind(event_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::reminder_from_row).collect()
    }

    async fn replace_reminders_async(
        &self,
        owner_sub: &str,
        event_id: &str,
        reminders: &[Reminder],
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM event_reminders WHERE owner_sub = $1 AND event_id = $2")
            .bind(owner_sub)
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        for r in reminders {
            if r.owner_sub != owner_sub || r.event_id != event_id {
                continue;
            }
            sqlx::query(
                "INSERT INTO event_reminders \
                     (id, event_id, owner_sub, minutes_before, delivered_at, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(&r.id)
            .bind(&r.event_id)
            .bind(&r.owner_sub)
            .bind(r.minutes_before)
            .bind(r.delivered_at)
            .bind(r.created_at)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    async fn delete_event_reminders_async(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM event_reminders WHERE owner_sub = $1 AND event_id = $2")
            .bind(owner_sub)
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn due_reminders_async(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<DueReminder>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT r.id AS reminder_id, r.owner_sub AS owner_sub, \
                    r.minutes_before AS minutes_before, e.id AS event_id, \
                    e.title AS event_title, e.starts_at AS event_starts_at, \
                    e.location AS event_location \
             FROM event_reminders r \
             JOIN events e ON e.id = r.event_id AND e.owner_sub = r.owner_sub \
             WHERE r.delivered_at = 0 \
               AND (e.starts_at - r.minutes_before * 60000) <= $1 \
               AND e.starts_at >= $1 \
             ORDER BY e.starts_at ASC, r.id ASC LIMIT $2",
        )
        .bind(now_ms)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::due_reminder_from_row).collect()
    }

    async fn mark_reminder_delivered_async(
        &self,
        reminder_id: &str,
        when_ms: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE event_reminders SET delivered_at = $1 WHERE id = $2")
            .bind(when_ms)
            .bind(reminder_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl Store for PgStore {
    async fn list_calendars(&self, owner_sub: &str) -> Result<Vec<Calendar>, StoreError> {
        self.list_calendars_async(owner_sub)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn get_calendar(
        &self,
        owner_sub: &str,
        id: &str,
    ) -> Result<Option<Calendar>, StoreError> {
        self.get_calendar_async(owner_sub, id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn upsert_calendar(&self, calendar: Calendar) -> Result<Calendar, StoreError> {
        self.upsert_calendar_async(&calendar)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(calendar)
    }

    async fn delete_calendar(&self, owner_sub: &str, id: &str) -> Result<bool, StoreError> {
        self.delete_calendar_async(owner_sub, id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_events(&self, owner_sub: &str) -> Result<EventListing, StoreError> {
        self.list_events_async(owner_sub)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn get_event(&self, owner_sub: &str, id: &str) -> Result<Option<Event>, StoreError> {
        self.get_event_async(owner_sub, id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn upsert_event(&self, mut event: Event) -> Result<Event, StoreError> {
        event.calendar_id = self
            .normalize_calendar_id_async(&event.owner_sub, &event.calendar_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        event.exception_dates.clear();
        self.upsert_event_async(&event)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(event)
    }

    async fn delete_event(&self, owner_sub: &str, id: &str) -> Result<bool, StoreError> {
        self.delete_event_async(owner_sub, id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn save_event_bundle(&self, bundle: EventBundle) -> Result<Event, StoreError> {
        let EventBundle {
            mut event,
            attendees: attendee_inputs,
            reminder_minutes,
            now_ms,
            reconcile_series,
        } = bundle;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;

        let mut duplicate_overrides = Vec::new();
        let existing = if event.series_id.is_empty() {
            Self::lock_event_tx(&mut tx, &event.id)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?
        } else {
            let Some(series) = Self::lock_event_tx(&mut tx, &event.series_id)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?
            else {
                return Err(StoreError::OwnershipConflict);
            };
            if series.owner_sub != event.owner_sub
                || !series.series_id.is_empty()
                || !crate::rrule::occurrence_belongs_to_series(
                    series.starts_at,
                    &series.rrule,
                    event.override_occurrence_date,
                )
            {
                return Err(StoreError::OwnershipConflict);
            }

            let mut matching = Self::lock_occurrence_overrides_tx(
                &mut tx,
                &event.owner_sub,
                &event.series_id,
                event.override_occurrence_date,
            )
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
            if matching.is_empty() {
                Self::lock_event_tx(&mut tx, &event.id)
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?
            } else {
                let canonical = matching.remove(0);
                event.id = canonical.id.clone();
                duplicate_overrides = matching;
                Some(canonical)
            }
        };
        if let Some(existing) = existing {
            if existing.owner_sub != event.owner_sub
                || (!event.series_id.is_empty()
                    && (existing.series_id != event.series_id
                        || existing.override_occurrence_date != event.override_occurrence_date))
                || (event.series_id.is_empty() && !existing.series_id.is_empty())
            {
                return Err(StoreError::OwnershipConflict);
            }
            event.created_at = existing.created_at;
        }

        let (overrides, exceptions) = if reconcile_series {
            (
                Self::lock_series_overrides_tx(&mut tx, &event.owner_sub, &event.id)
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?,
                Self::lock_series_exceptions_tx(&mut tx, &event.owner_sub, &event.id)
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?,
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let existing_attendees = Self::lock_attendees_tx(&mut tx, &event.owner_sub, &event.id)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let existing_reminders = Self::lock_reminders_tx(&mut tx, &event.owner_sub, &event.id)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        for override_event in overrides.iter().chain(&duplicate_overrides) {
            Self::lock_attendees_tx(&mut tx, &event.owner_sub, &override_event.id)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
            Self::lock_reminders_tx(&mut tx, &event.owner_sub, &override_event.id)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
        }

        event.calendar_id =
            Self::normalize_calendar_id_tx(&mut tx, &event.owner_sub, &event.calendar_id)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
        event.exception_dates.clear();
        let attendees = reconcile_attendee_rows(
            &event.owner_sub,
            &event.id,
            &attendee_inputs,
            &existing_attendees,
            now_ms,
        );
        let reminders = reconcile_reminder_rows(
            &event.owner_sub,
            &event.id,
            &reminder_minutes,
            &existing_reminders,
            now_ms,
        );
        if !Self::upsert_event_tx(&mut tx, &event)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?
        {
            return Err(StoreError::OwnershipConflict);
        }

        for duplicate in duplicate_overrides {
            Self::delete_event_projection_tx(&mut tx, &event.owner_sub, &duplicate.id)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
            sqlx::query("DELETE FROM events WHERE id = $1 AND owner_sub = $2")
                .bind(&duplicate.id)
                .bind(&event.owner_sub)
                .execute(&mut *tx)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
        }

        if reconcile_series {
            for override_event in overrides {
                if !crate::rrule::occurrence_belongs_to_series(
                    event.starts_at,
                    &event.rrule,
                    override_event.override_occurrence_date,
                ) {
                    Self::delete_event_projection_tx(&mut tx, &event.owner_sub, &override_event.id)
                        .await
                        .map_err(|error| StoreError::Backend(error.to_string()))?;
                    sqlx::query("DELETE FROM events WHERE id = $1 AND owner_sub = $2")
                        .bind(&override_event.id)
                        .bind(&event.owner_sub)
                        .execute(&mut *tx)
                        .await
                        .map_err(|error| StoreError::Backend(error.to_string()))?;
                }
            }
            for exception in exceptions {
                if !crate::rrule::occurrence_belongs_to_series(
                    event.starts_at,
                    &event.rrule,
                    exception.occurrence_date,
                ) {
                    sqlx::query(
                        "DELETE FROM event_exceptions \
                         WHERE id = $1 AND owner_sub = $2 AND event_id = $3",
                    )
                    .bind(&exception.id)
                    .bind(&event.owner_sub)
                    .bind(&event.id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|error| StoreError::Backend(error.to_string()))?;
                }
            }
        }
        Self::replace_event_projection_tx(
            &mut tx,
            &event.owner_sub,
            &event.id,
            &attendees,
            &reminders,
        )
        .await
        .map_err(|error| StoreError::Backend(error.to_string()))?;
        tx.commit()
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        Ok(event)
    }

    async fn delete_occurrence_bundle(
        &self,
        owner_sub: &str,
        series_id: &str,
        occurrence_date: i64,
        mut exception: EventException,
    ) -> Result<bool, StoreError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let Some(series) = Self::lock_event_tx(&mut tx, series_id)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?
        else {
            return Ok(false);
        };
        if series.owner_sub != owner_sub
            || !crate::rrule::occurrence_belongs_to_series(
                series.starts_at,
                &series.rrule,
                occurrence_date,
            )
        {
            return Ok(false);
        }

        let overrides = Self::lock_series_overrides_tx(&mut tx, owner_sub, series_id)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let exceptions = Self::lock_series_exceptions_tx(&mut tx, owner_sub, series_id)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let matching_overrides: Vec<Event> = overrides
            .into_iter()
            .filter(|event| event.override_occurrence_date == occurrence_date)
            .collect();
        for override_event in &matching_overrides {
            Self::lock_attendees_tx(&mut tx, owner_sub, &override_event.id)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
            Self::lock_reminders_tx(&mut tx, owner_sub, &override_event.id)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
        }

        let exists = exceptions
            .iter()
            .any(|existing| existing.occurrence_date == occurrence_date);
        if !exists {
            exception.owner_sub = owner_sub.to_string();
            exception.event_id = series_id.to_string();
            exception.occurrence_date = occurrence_date;
            sqlx::query(
                "INSERT INTO event_exceptions \
                     (id, event_id, owner_sub, occurrence_date, created_at) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (owner_sub, event_id, occurrence_date) DO NOTHING",
            )
            .bind(&exception.id)
            .bind(&exception.event_id)
            .bind(&exception.owner_sub)
            .bind(exception.occurrence_date)
            .bind(exception.created_at)
            .execute(&mut *tx)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        }

        for override_event in matching_overrides {
            Self::delete_event_projection_tx(&mut tx, owner_sub, &override_event.id)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
            sqlx::query("DELETE FROM events WHERE id = $1 AND owner_sub = $2")
                .bind(&override_event.id)
                .bind(owner_sub)
                .execute(&mut *tx)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        Ok(true)
    }

    async fn delete_event_tree(&self, owner_sub: &str, event_id: &str) -> Result<bool, StoreError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let Some(event) = Self::lock_event_tx(&mut tx, event_id)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?
        else {
            return Ok(false);
        };
        if event.owner_sub != owner_sub {
            return Ok(false);
        }

        let overrides = Self::lock_series_overrides_tx(&mut tx, owner_sub, event_id)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let _exceptions = Self::lock_series_exceptions_tx(&mut tx, owner_sub, event_id)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        Self::lock_attendees_tx(&mut tx, owner_sub, event_id)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        Self::lock_reminders_tx(&mut tx, owner_sub, event_id)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        for override_event in &overrides {
            Self::lock_attendees_tx(&mut tx, owner_sub, &override_event.id)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
            Self::lock_reminders_tx(&mut tx, owner_sub, &override_event.id)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
        }

        for override_event in overrides {
            Self::delete_event_projection_tx(&mut tx, owner_sub, &override_event.id)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
            sqlx::query("DELETE FROM events WHERE id = $1 AND owner_sub = $2")
                .bind(&override_event.id)
                .bind(owner_sub)
                .execute(&mut *tx)
                .await
                .map_err(|error| StoreError::Backend(error.to_string()))?;
        }
        sqlx::query("DELETE FROM event_exceptions WHERE owner_sub = $1 AND event_id = $2")
            .bind(owner_sub)
            .bind(event_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        Self::delete_event_projection_tx(&mut tx, owner_sub, event_id)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let deleted = sqlx::query("DELETE FROM events WHERE id = $1 AND owner_sub = $2")
            .bind(event_id)
            .bind(owner_sub)
            .execute(&mut *tx)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?
            .rows_affected()
            > 0;
        tx.commit()
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        Ok(deleted)
    }

    async fn upsert_event_exception(
        &self,
        exception: EventException,
    ) -> Result<EventException, StoreError> {
        self.upsert_event_exception_async(&exception)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(exception)
    }

    async fn delete_event_exceptions(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), StoreError> {
        self.delete_event_exceptions_async(owner_sub, event_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn get_event_override(
        &self,
        owner_sub: &str,
        series_id: &str,
        occurrence_date: i64,
    ) -> Result<Option<Event>, StoreError> {
        self.get_event_override_async(owner_sub, series_id, occurrence_date)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn delete_event_override(
        &self,
        owner_sub: &str,
        series_id: &str,
        occurrence_date: i64,
    ) -> Result<Option<Event>, StoreError> {
        self.delete_event_override_async(owner_sub, series_id, occurrence_date)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn delete_event_overrides(
        &self,
        owner_sub: &str,
        series_id: &str,
    ) -> Result<Vec<Event>, StoreError> {
        self.delete_event_overrides_async(owner_sub, series_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_contacts(&self, owner_sub: &str) -> Result<Vec<Contact>, StoreError> {
        self.list_contacts_async(owner_sub)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn get_contact(&self, owner_sub: &str, id: &str) -> Result<Option<Contact>, StoreError> {
        self.get_contact_async(owner_sub, id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn upsert_contact(&self, contact: Contact) -> Result<Contact, StoreError> {
        self.upsert_contact_async(&contact)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(contact)
    }

    async fn delete_contact(&self, owner_sub: &str, id: &str) -> Result<bool, StoreError> {
        self.delete_contact_async(owner_sub, id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn get_settings(&self, owner_sub: &str) -> Result<Settings, StoreError> {
        self.get_settings_async(owner_sub)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn upsert_settings(&self, settings: Settings) -> Result<Settings, StoreError> {
        self.upsert_settings_async(&settings)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(settings)
    }

    async fn list_attendees(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<Vec<Attendee>, StoreError> {
        self.list_attendees_async(owner_sub, event_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn replace_attendees(
        &self,
        owner_sub: &str,
        event_id: &str,
        attendees: Vec<Attendee>,
    ) -> Result<(), StoreError> {
        self.replace_attendees_async(owner_sub, event_id, &attendees)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn delete_event_attendees(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), StoreError> {
        self.delete_event_attendees_async(owner_sub, event_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn get_attendee_by_token(&self, token: &str) -> Result<Option<Attendee>, StoreError> {
        self.get_attendee_by_token_async(token)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn set_attendee_status_by_token(
        &self,
        token: &str,
        status: &str,
    ) -> Result<Option<Attendee>, StoreError> {
        self.set_attendee_status_by_token_async(token, status)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn list_reminders(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<Vec<Reminder>, StoreError> {
        self.list_reminders_async(owner_sub, event_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn replace_reminders(
        &self,
        owner_sub: &str,
        event_id: &str,
        reminders: Vec<Reminder>,
    ) -> Result<(), StoreError> {
        self.replace_reminders_async(owner_sub, event_id, &reminders)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn delete_event_reminders(
        &self,
        owner_sub: &str,
        event_id: &str,
    ) -> Result<(), StoreError> {
        self.delete_event_reminders_async(owner_sub, event_id)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn due_reminders(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<DueReminder>, StoreError> {
        self.due_reminders_async(now_ms, limit)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }

    async fn mark_reminder_delivered(
        &self,
        reminder_id: &str,
        when_ms: i64,
    ) -> Result<(), StoreError> {
        self.mark_reminder_delivered_async(reminder_id, when_ms)
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, owner: &str, title: &str, starts: i64) -> Event {
        Event {
            id: id.to_string(),
            owner_sub: owner.to_string(),
            calendar_id: String::new(),
            title: title.to_string(),
            starts_at: starts,
            ends_at: starts + 3_600_000,
            all_day: false,
            location: String::new(),
            notes: String::new(),
            rrule: String::new(),
            series_id: String::new(),
            override_occurrence_date: 0,
            exception_dates: Vec::new(),
            created_at: starts,
        }
    }

    fn bundle(
        event: Event,
        attendees: &[(&str, &str)],
        reminder_minutes: &[i64],
        now_ms: i64,
        reconcile_series: bool,
    ) -> EventBundle {
        EventBundle {
            event,
            attendees: attendees
                .iter()
                .map(|(name, email)| AttendeeInput {
                    email: (*email).to_string(),
                    name: (*name).to_string(),
                })
                .collect(),
            reminder_minutes: reminder_minutes.to_vec(),
            now_ms,
            reconcile_series,
        }
    }

    #[tokio::test]
    async fn events_are_scoped_per_owner() {
        let store = InMemoryStore::new();
        store
            .upsert_event(ev("e1", "alice", "Standup", 200))
            .await
            .unwrap();
        store
            .upsert_event(ev("e2", "alice", "Lunch", 100))
            .await
            .unwrap();
        store
            .upsert_event(ev("e3", "bob", "Bob only", 150))
            .await
            .unwrap();

        let alice = store.list_events("alice").await.unwrap().events;
        assert_eq!(alice.len(), 2, "only alice's events");
        assert_eq!(alice[0].id, "e2", "ordered by starts_at ascending");
        assert_eq!(alice[1].id, "e1");

        // Bob cannot read or delete alice's event.
        assert!(store.get_event("bob", "e1").await.unwrap().is_none());
        assert!(!store.delete_event("bob", "e1").await.unwrap());
        assert!(store.get_event("alice", "e1").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn event_listing_reports_the_limit_plus_one_truthfully() {
        let store = InMemoryStore::new();
        for index in 0..=EVENT_LIST_LIMIT {
            store
                .upsert_event(ev(
                    &format!("event-{index:04}"),
                    "alice",
                    "Loaded",
                    index as i64,
                ))
                .await
                .unwrap();
        }

        let listing = store.list_events("alice").await.unwrap();
        assert_eq!(listing.events.len(), EVENT_LIST_LIMIT);
        assert!(listing.has_more);
        assert_eq!(listing.events.first().unwrap().starts_at, 0);
        assert_eq!(
            listing.events.last().unwrap().starts_at,
            (EVENT_LIST_LIMIT - 1) as i64
        );

        store
            .delete_event("alice", &format!("event-{EVENT_LIST_LIMIT:04}"))
            .await
            .unwrap();
        let complete = store.list_events("alice").await.unwrap();
        assert_eq!(complete.events.len(), EVENT_LIST_LIMIT);
        assert!(!complete.has_more);
    }

    #[test]
    fn bounded_event_projection_never_retains_more_than_limit_plus_one() {
        let count = EVENT_LIST_LIMIT * 4 + 17;
        let mut events: Vec<Event> = (0..count)
            .rev()
            .map(|index| {
                ev(
                    &format!("event-{index:05}"),
                    "alice",
                    "Adversarial order",
                    index as i64,
                )
            })
            .collect();
        events.push(ev("foreign", "bob", "Not retained", -1));

        let projection = bounded_owned_event_projection(events.iter(), "alice");
        assert!(projection.has_more);
        assert_eq!(projection.events.len(), EVENT_LIST_LIMIT);
        assert_eq!(projection.events.first().unwrap().starts_at, 0);
        assert_eq!(
            projection.events.last().unwrap().starts_at,
            (EVENT_LIST_LIMIT - 1) as i64
        );
        assert!(
            projection.max_retained <= EVENT_LIST_LIMIT + 1,
            "bounded projection retained {} candidates",
            projection.max_retained
        );
    }

    #[tokio::test]
    async fn event_bundle_is_atomic_and_preserves_child_capabilities() {
        let store = InMemoryStore::new();
        store
            .save_event_bundle(bundle(
                ev("bundle", "alice", "Original", 1_000),
                &[("Guest", "Guest@Example.test")],
                &[10],
                10,
                false,
            ))
            .await
            .unwrap();
        let original_attendee = store
            .list_attendees("alice", "bundle")
            .await
            .unwrap()
            .pop()
            .unwrap();
        store
            .set_attendee_status_by_token(&original_attendee.token, "accepted")
            .await
            .unwrap();
        let original_reminder = store
            .list_reminders("alice", "bundle")
            .await
            .unwrap()
            .pop()
            .unwrap();
        store
            .mark_reminder_delivered(&original_reminder.id, 77)
            .await
            .unwrap();

        store
            .save_event_bundle(bundle(
                ev("bundle", "alice", "Updated", 2_000),
                &[("Renamed", "guest@example.test")],
                &[10, 30],
                20,
                false,
            ))
            .await
            .unwrap();
        let attendees = store.list_attendees("alice", "bundle").await.unwrap();
        assert_eq!(attendees.len(), 1);
        assert_eq!(attendees[0].id, original_attendee.id);
        assert_eq!(attendees[0].token, original_attendee.token);
        assert_eq!(attendees[0].status, "accepted");
        assert_eq!(attendees[0].name, "Renamed");
        let reminders = store.list_reminders("alice", "bundle").await.unwrap();
        let retained = reminders
            .iter()
            .find(|reminder| reminder.minutes_before == 10)
            .unwrap();
        assert_eq!(retained.id, original_reminder.id);
        assert_eq!(retained.delivered_at, 77);
        assert_eq!(reminders.len(), 2);

        store.fail_next_save_bundle_after_event();
        let failed = store
            .save_event_bundle(bundle(
                ev("bundle", "alice", "Must roll back", 3_000),
                &[("Replacement", "replacement@example.test")],
                &[60],
                30,
                false,
            ))
            .await;
        assert!(matches!(failed, Err(StoreError::Backend(_))));
        assert_eq!(
            store
                .get_event("alice", "bundle")
                .await
                .unwrap()
                .unwrap()
                .title,
            "Updated"
        );
        let after_attendees = store.list_attendees("alice", "bundle").await.unwrap();
        assert_eq!(after_attendees, attendees);
        assert_eq!(
            store.list_reminders("alice", "bundle").await.unwrap(),
            reminders
        );
    }

    #[tokio::test]
    async fn event_bundle_rejects_another_owners_colliding_id() {
        let store = InMemoryStore::new();
        store
            .upsert_event(ev("collision", "bob", "Bob", 1))
            .await
            .unwrap();
        let result = store
            .save_event_bundle(bundle(
                ev("collision", "alice", "Alice", 2),
                &[],
                &[],
                2,
                false,
            ))
            .await;
        assert!(matches!(result, Err(StoreError::OwnershipConflict)));
        assert_eq!(
            store
                .get_event("bob", "collision")
                .await
                .unwrap()
                .unwrap()
                .title,
            "Bob"
        );
        assert!(store
            .get_event("alice", "collision")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn series_bundle_prunes_only_children_outside_the_new_rule() {
        let store = InMemoryStore::new();
        let day = 86_400_000;
        let start = 1_000;
        let mut series = ev("series", "alice", "Series", start);
        series.rrule = "FREQ=DAILY;COUNT=3".to_string();
        store
            .save_event_bundle(bundle(
                series.clone(),
                &[("Base", "base@example.test")],
                &[5],
                1,
                true,
            ))
            .await
            .unwrap();

        for (id, occurrence) in [("override-2", start + day), ("override-3", start + 2 * day)] {
            let mut override_event = ev(id, "alice", id, occurrence + 3_600_000);
            override_event.series_id = series.id.clone();
            override_event.override_occurrence_date = occurrence;
            store
                .save_event_bundle(bundle(
                    override_event,
                    &[("Child", &format!("{id}@example.test"))],
                    &[15],
                    2,
                    false,
                ))
                .await
                .unwrap();
            store
                .upsert_event_exception(EventException {
                    id: format!("exception-{id}"),
                    event_id: series.id.clone(),
                    owner_sub: "alice".to_string(),
                    occurrence_date: occurrence,
                    created_at: 2,
                })
                .await
                .unwrap();
        }

        series.rrule = "FREQ=DAILY;COUNT=2".to_string();
        store
            .save_event_bundle(bundle(
                series.clone(),
                &[("Base", "base@example.test")],
                &[5],
                3,
                true,
            ))
            .await
            .unwrap();
        assert!(store
            .get_event_override("alice", "series", start + day)
            .await
            .unwrap()
            .is_some());
        assert!(store
            .get_event_override("alice", "series", start + 2 * day)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .list_attendees("alice", "override-2")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(store
            .list_attendees("alice", "override-3")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .get_event("alice", "series")
                .await
                .unwrap()
                .unwrap()
                .exception_dates,
            vec![start + day]
        );

        series.rrule.clear();
        store
            .save_event_bundle(bundle(
                series,
                &[("Base", "base@example.test")],
                &[5],
                4,
                true,
            ))
            .await
            .unwrap();
        assert!(store
            .get_event_override("alice", "series", start + day)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .get_event("alice", "series")
            .await
            .unwrap()
            .unwrap()
            .exception_dates
            .is_empty());
        assert!(store
            .list_reminders("alice", "override-2")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn occurrence_and_tree_deletes_are_complete_and_owner_scoped() {
        let store = InMemoryStore::new();
        let day = 86_400_000;
        let start = 1_000;
        let mut series = ev("delete-series", "alice", "Series", start);
        series.rrule = "FREQ=DAILY;COUNT=2".to_string();
        store
            .save_event_bundle(bundle(series, &[], &[], 1, true))
            .await
            .unwrap();
        let mut override_event = ev("delete-override", "alice", "Moved", start + day);
        override_event.series_id = "delete-series".to_string();
        override_event.override_occurrence_date = start + day;
        store
            .save_event_bundle(bundle(
                override_event,
                &[("Child", "child@example.test")],
                &[10],
                2,
                false,
            ))
            .await
            .unwrap();

        let mut colliding_override =
            ev("delete-override-race", "alice", "Moved again", start + day);
        colliding_override.series_id = "delete-series".to_string();
        colliding_override.override_occurrence_date = start + day;
        let canonical = store
            .save_event_bundle(bundle(
                colliding_override,
                &[("Replacement", "replacement@example.test")],
                &[20],
                3,
                false,
            ))
            .await
            .unwrap();
        assert_eq!(canonical.id, "delete-override");
        assert!(store
            .get_event("alice", "delete-override-race")
            .await
            .unwrap()
            .is_none());

        let invalid = store
            .delete_occurrence_bundle(
                "alice",
                "delete-series",
                start + 2 * day,
                EventException {
                    id: "invalid".to_string(),
                    event_id: String::new(),
                    owner_sub: String::new(),
                    occurrence_date: 0,
                    created_at: 3,
                },
            )
            .await
            .unwrap();
        assert!(!invalid);
        assert!(store
            .get_event("alice", "delete-override")
            .await
            .unwrap()
            .is_some());

        assert!(store
            .delete_occurrence_bundle(
                "alice",
                "delete-series",
                start + day,
                EventException {
                    id: "valid".to_string(),
                    event_id: String::new(),
                    owner_sub: String::new(),
                    occurrence_date: 0,
                    created_at: 3,
                },
            )
            .await
            .unwrap());
        assert!(store
            .get_event("alice", "delete-override")
            .await
            .unwrap()
            .is_none());
        assert!(store
            .list_attendees("alice", "delete-override")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .get_event("alice", "delete-series")
                .await
                .unwrap()
                .unwrap()
                .exception_dates,
            vec![start + day]
        );
        assert!(!store
            .delete_event_tree("bob", "delete-series")
            .await
            .unwrap());
        assert!(store
            .get_event("alice", "delete-series")
            .await
            .unwrap()
            .is_some());
        assert!(store
            .delete_event_tree("alice", "delete-series")
            .await
            .unwrap());
        assert!(store
            .get_event("alice", "delete-series")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn upsert_updates_and_guards_owner() {
        let store = InMemoryStore::new();
        store
            .upsert_event(ev("e1", "alice", "v1", 100))
            .await
            .unwrap();
        let mut updated = ev("e1", "alice", "v2", 500);
        updated.created_at = 100;
        store.upsert_event(updated).await.unwrap();
        let got = store.get_event("alice", "e1").await.unwrap().unwrap();
        assert_eq!(got.title, "v2");
        assert_eq!(got.created_at, 100, "created_at preserved by caller");

        // Bob trying to hijack e1 by id is a no-op on the stored (alice) row.
        store
            .upsert_event(ev("e1", "bob", "stolen", 1))
            .await
            .unwrap();
        let still = store.get_event("alice", "e1").await.unwrap().unwrap();
        assert_eq!(still.title, "v2", "ownership guard prevented overwrite");
    }

    #[tokio::test]
    async fn contacts_sorted_and_scoped() {
        let store = InMemoryStore::new();
        let mk = |id: &str, owner: &str, name: &str| Contact {
            id: id.to_string(),
            owner_sub: owner.to_string(),
            name: name.to_string(),
            email: String::new(),
            phone: String::new(),
            notes: String::new(),
            created_at: 0,
        };
        store
            .upsert_contact(mk("c1", "alice", "Zoe"))
            .await
            .unwrap();
        store
            .upsert_contact(mk("c2", "alice", "amy"))
            .await
            .unwrap();
        store
            .upsert_contact(mk("c3", "bob", "Carol"))
            .await
            .unwrap();
        let alice = store.list_contacts("alice").await.unwrap();
        assert_eq!(alice.len(), 2);
        assert_eq!(alice[0].name, "amy", "case-insensitive name order");
        assert_eq!(alice[1].name, "Zoe");
    }

    #[tokio::test]
    async fn settings_default_then_persist_and_scope() {
        let store = InMemoryStore::new();
        // No row yet => the UTC/Sunday default, echoing the requested owner.
        let def = store.get_settings("alice").await.unwrap();
        assert_eq!(def, Settings::default_for("alice"));
        assert_eq!(def.timezone, "UTC");
        assert_eq!(def.week_start, "sunday");

        store
            .upsert_settings(Settings {
                owner_sub: "alice".to_string(),
                timezone: "UTC+08:00".to_string(),
                week_start: "monday".to_string(),
                updated_at: 42,
            })
            .await
            .unwrap();

        let got = store.get_settings("alice").await.unwrap();
        assert_eq!(got.timezone, "UTC+08:00");
        assert_eq!(got.week_start, "monday");
        assert_eq!(got.updated_at, 42);

        // Another owner is unaffected — still the default.
        assert_eq!(
            store.get_settings("bob").await.unwrap(),
            Settings::default_for("bob")
        );
    }

    #[tokio::test]
    async fn calendars_default_and_delete_blocks_nonempty() {
        let store = InMemoryStore::new();
        let default_id = default_calendar_id("alice");
        let calendars = store.list_calendars("alice").await.unwrap();
        assert_eq!(calendars.len(), 1);
        assert_eq!(calendars[0].id, default_id);
        assert_eq!(calendars[0].name, DEFAULT_CALENDAR_NAME);

        let work = Calendar {
            id: "work".to_string(),
            owner_sub: "alice".to_string(),
            name: "Work".to_string(),
            color: "#22c55e".to_string(),
            position: 1,
        };
        store.upsert_calendar(work.clone()).await.unwrap();
        let mut event = ev("e-cal", "alice", "Planning", 100);
        event.calendar_id = work.id.clone();
        store.upsert_event(event).await.unwrap();
        assert!(!store.delete_calendar("alice", "work").await.unwrap());
        assert_eq!(
            store.list_events("alice").await.unwrap().events[0].calendar_id,
            "work",
            "event keeps its owned calendar"
        );

        store.delete_event("alice", "e-cal").await.unwrap();
        assert!(store.delete_calendar("alice", "work").await.unwrap());
        assert!(!store.delete_calendar("alice", &default_id).await.unwrap());
    }

    #[tokio::test]
    async fn exceptions_and_overrides_are_owner_scoped() {
        let store = InMemoryStore::new();
        let mut series = ev("s1", "alice", "Daily", 1_000);
        series.rrule = "FREQ=DAILY;COUNT=3".to_string();
        store.upsert_event(series).await.unwrap();
        store
            .upsert_event_exception(EventException {
                id: "x1".to_string(),
                event_id: "s1".to_string(),
                owner_sub: "alice".to_string(),
                occurrence_date: 86_401_000,
                created_at: 0,
            })
            .await
            .unwrap();
        let events = store.list_events("alice").await.unwrap().events;
        assert_eq!(events[0].exception_dates, vec![86_401_000]);

        let mut override_event = ev("ov1", "alice", "Moved", 200_000);
        override_event.series_id = "s1".to_string();
        override_event.override_occurrence_date = 86_401_000;
        store.upsert_event(override_event).await.unwrap();
        assert!(store
            .get_event_override("alice", "s1", 86_401_000)
            .await
            .unwrap()
            .is_some());
        assert!(store
            .get_event_override("bob", "s1", 86_401_000)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .delete_event_override("alice", "s1", 86_401_000)
            .await
            .unwrap()
            .is_some());
    }

    fn attendee(id: &str, event: &str, owner: &str, email: &str, token: &str) -> Attendee {
        Attendee {
            id: id.to_string(),
            event_id: event.to_string(),
            owner_sub: owner.to_string(),
            email: email.to_string(),
            name: String::new(),
            status: "needs-action".to_string(),
            token: token.to_string(),
            created_at: 0,
        }
    }

    #[tokio::test]
    async fn attendees_replace_and_token_rsvp() {
        let store = InMemoryStore::new();
        store
            .replace_attendees(
                "alice",
                "e1",
                vec![
                    attendee("a1", "e1", "alice", "guest@x.co", "tok-guest"),
                    attendee("a2", "e1", "alice", "team@x.co", "tok-team"),
                ],
            )
            .await
            .unwrap();
        assert_eq!(store.list_attendees("alice", "e1").await.unwrap().len(), 2);
        // Another owner sees nothing for the same event id.
        assert!(store.list_attendees("bob", "e1").await.unwrap().is_empty());

        // Public token lookup is NOT owner-scoped; RSVP updates status.
        let got = store
            .get_attendee_by_token("tok-guest")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.email, "guest@x.co");
        let updated = store
            .set_attendee_status_by_token("tok-guest", "accepted")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, "accepted");
        // An unknown token yields None (no panic, no mutation).
        assert!(store
            .set_attendee_status_by_token("nope", "declined")
            .await
            .unwrap()
            .is_none());

        // Replace preserves only the final set; old rows are gone.
        store
            .replace_attendees(
                "alice",
                "e1",
                vec![attendee("a3", "e1", "alice", "solo@x.co", "tok-solo")],
            )
            .await
            .unwrap();
        assert_eq!(store.list_attendees("alice", "e1").await.unwrap().len(), 1);
        assert!(store
            .get_attendee_by_token("tok-guest")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn reminders_due_scan_and_mark_delivered() {
        let store = InMemoryStore::new();
        // Event starts at t=1_000_000 ms.
        let start = 1_000_000i64;
        store
            .upsert_event(ev("e1", "alice", "Standup", start))
            .await
            .unwrap();
        // Two reminders: 10 min before (fires at start-600_000) and at-time (fires at start).
        store
            .replace_reminders(
                "alice",
                "e1",
                vec![
                    Reminder {
                        id: "r10".into(),
                        event_id: "e1".into(),
                        owner_sub: "alice".into(),
                        minutes_before: 10,
                        delivered_at: 0,
                        created_at: 0,
                    },
                    Reminder {
                        id: "r0".into(),
                        event_id: "e1".into(),
                        owner_sub: "alice".into(),
                        minutes_before: 0,
                        delivered_at: 0,
                        created_at: 0,
                    },
                ],
            )
            .await
            .unwrap();
        assert_eq!(store.list_reminders("alice", "e1").await.unwrap().len(), 2);

        // Well before either fire time: nothing due.
        assert!(store
            .due_reminders(start - 700_000, 100)
            .await
            .unwrap()
            .is_empty());

        // After the 10-min fire time but before start: only r10 is due.
        let due = store.due_reminders(start - 300_000, 100).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].reminder_id, "r10");
        assert_eq!(due[0].event_title, "Standup");

        // Deliver r10; it no longer appears. At start, r0 fires.
        store.mark_reminder_delivered("r10", 42).await.unwrap();
        let due = store.due_reminders(start, 100).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].reminder_id, "r0");

        // Past the start instant: the upcoming-only guard drops everything.
        store.mark_reminder_delivered("r0", 43).await.unwrap();
        assert!(store
            .due_reminders(start + 1, 100)
            .await
            .unwrap()
            .is_empty());

        // Limit bounds the result set.
        store
            .replace_reminders(
                "alice",
                "e1",
                vec![
                    Reminder {
                        id: "x1".into(),
                        event_id: "e1".into(),
                        owner_sub: "alice".into(),
                        minutes_before: 30,
                        delivered_at: 0,
                        created_at: 0,
                    },
                    Reminder {
                        id: "x2".into(),
                        event_id: "e1".into(),
                        owner_sub: "alice".into(),
                        minutes_before: 20,
                        delivered_at: 0,
                        created_at: 0,
                    },
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            store.due_reminders(start - 100_000, 1).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn deleting_event_cascade_helpers_scope_to_owner() {
        let store = InMemoryStore::new();
        store
            .replace_attendees(
                "alice",
                "e1",
                vec![attendee("a1", "e1", "alice", "g@x.co", "t1")],
            )
            .await
            .unwrap();
        store
            .replace_reminders(
                "alice",
                "e1",
                vec![Reminder {
                    id: "r1".into(),
                    event_id: "e1".into(),
                    owner_sub: "alice".into(),
                    minutes_before: 5,
                    delivered_at: 0,
                    created_at: 0,
                }],
            )
            .await
            .unwrap();
        // A different owner's delete does not touch alice's rows.
        store.delete_event_attendees("bob", "e1").await.unwrap();
        store.delete_event_reminders("bob", "e1").await.unwrap();
        assert_eq!(store.list_attendees("alice", "e1").await.unwrap().len(), 1);
        assert_eq!(store.list_reminders("alice", "e1").await.unwrap().len(), 1);
        // The owner's cascade clears them.
        store.delete_event_attendees("alice", "e1").await.unwrap();
        store.delete_event_reminders("alice", "e1").await.unwrap();
        assert!(store
            .list_attendees("alice", "e1")
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .list_reminders("alice", "e1")
            .await
            .unwrap()
            .is_empty());
    }
}
