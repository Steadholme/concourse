//! Server configuration, env-driven with working dev defaults.
//!
//! Every value keeps its dev default when the corresponding env var is unset/empty, so the
//! in-memory dev path boots with NO configuration and NO database — exactly like
//! inkwell/sanctum. Production overrides each via the environment.

/// Default listen address (all interfaces, internal-only port 9060).
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:9060";

/// Hard cap on how many messages a single `GET /messages` page returns.
pub const MESSAGE_PAGE_LIMIT: i64 = 50;

/// Hard cap on how many rooms the room list returns for one user.
pub const ROOM_LIST_LIMIT: i64 = 500;

/// Maximum stored message body length, in characters (oversized bodies are rejected, not
/// silently truncated).
pub const MAX_BODY_CHARS: usize = 8 * 1024;

/// Maximum room name length, in characters.
pub const MAX_ROOM_NAME_CHARS: usize = 120;

/// Tombstone body a message is redacted TO by an admin/moderator (replaces the original text).
pub const REDACTED_BODY: &str = "[removed by moderator]";

/// The global lobby room every user is auto-joined to on first visit, so the UI is never empty.
pub const LOBBY_ID: &str = "lobby";
/// Display name of the lobby room.
pub const LOBBY_NAME: &str = "#lobby";

/// Runtime configuration. Cheap to clone; shared read-only behind `Arc`.
#[derive(Clone, Debug)]
pub struct Config {
    /// Listen address (`BIND_ADDR`).
    pub bind_addr: String,
}

impl Config {
    /// Default development configuration (in-memory friendly, no database).
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
pub fn env_nonempty(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}
