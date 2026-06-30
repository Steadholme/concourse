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

/// Runtime configuration. Cheap to clone; shared read-only behind `Arc`.
#[derive(Clone, Debug)]
pub struct Config {
    /// Listen address (`BIND_ADDR`).
    pub bind_addr: String,
}

impl Config {
    /// Default development configuration (in-memory, no database, no persistence).
    pub fn dev() -> Self {
        Config {
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
        }
    }

    /// Configuration with the dev defaults overridden by environment variables.
    pub fn from_env() -> Self {
        let mut config = Config::dev();
        if let Some(v) = env_nonempty("BIND_ADDR") {
            config.bind_addr = v;
        }
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
