//! Server configuration, env-driven with working dev defaults.
//!
//! Every value keeps its dev default when the corresponding env var is unset/empty, so the
//! in-memory dev path boots with NO configuration and NO database — exactly like the rest of
//! the estate. Production overrides each via the environment.

/// Default listen address (all interfaces, internal-only port 8960).
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8960";
/// Hard cap on how many of the owner's events a single load lists. Keeps an unbounded calendar
/// bounded; the month grid + agenda both draw from this set.
pub const EVENT_LIST_LIMIT: usize = 2000;
/// Hard cap on how many of the owner's contacts the address book lists.
pub const CONTACT_LIST_LIMIT: usize = 2000;
/// How many upcoming events the agenda/sidebar shows at once.
pub const AGENDA_LIMIT: usize = 50;
/// How many event chips a single calendar-day cell renders before collapsing to "+N more".
pub const DAY_CHIP_LIMIT: usize = 3;
/// How far past "now" recurring series are expanded for the upcoming agenda, in milliseconds
/// (366 days). Bounds occurrence generation so an open-ended series stays cheap to render.
pub const AGENDA_HORIZON_MS: i64 = 366 * 86_400_000;

/// How often the standalone reminder scanner runs one bounded due-scan, seconds.
pub const REMINDER_SCAN_SECS: u64 = 30;

/// Runtime configuration. Cheap to clone; shared read-only behind `Arc`.
///
/// The `klaxon_notify_*` fields are the OPTIONAL reminder-delivery hook. They live on `Config`
/// (populated by [`Config::from_env`]) rather than on `AppState` so the composite — which builds
/// `AppState` with an explicit `{ config, store }` literal — picks them up unchanged. Both `None`
/// (the default) leaves reminders stored + queryable but undelivered.
#[derive(Clone, Debug)]
pub struct Config {
    /// Listen address (`BIND_ADDR`).
    pub bind_addr: String,
    /// Klaxon internal ingest URL, e.g. `http://klaxon:9050/api/notify` (`ALMANAC_KLAXON_NOTIFY_URL`).
    pub klaxon_notify_url: Option<String>,
    /// Bearer token for that ingest (`ALMANAC_KLAXON_INGEST_TOKEN` = Klaxon's `KLAXON_INGEST_TOKEN`).
    pub klaxon_ingest_token: Option<String>,
}

impl Config {
    /// Default development configuration (in-memory, no database, no persistence, no delivery).
    pub fn dev() -> Self {
        Config {
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
            klaxon_notify_url: None,
            klaxon_ingest_token: None,
        }
    }

    /// Configuration with the dev defaults overridden by environment variables.
    pub fn from_env() -> Self {
        let mut config = Config::dev();
        if let Some(v) = env_nonempty("BIND_ADDR") {
            config.bind_addr = v;
        }
        config.klaxon_notify_url = env_nonempty("ALMANAC_KLAXON_NOTIFY_URL");
        config.klaxon_ingest_token = env_nonempty("ALMANAC_KLAXON_INGEST_TOKEN");
        config
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::dev()
    }
}

/// Read an env var, returning `None` when unset OR empty (empty never clobbers a default).
fn env_nonempty(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}
