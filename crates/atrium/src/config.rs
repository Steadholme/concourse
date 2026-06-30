//! Server configuration, env-driven with working dev defaults.
//!
//! Atrium owns NO data of its own — it is a pure read aggregator that federates three sibling
//! services' databases READ-ONLY (mirroring how Cortex federates the content services). Each
//! source is configured by its own DSN env var (`MURMUR_DATABASE_URL` / `KLAXON_DATABASE_URL` /
//! `CURRENT_DATABASE_URL`, each pointing at `postgres:5432/<db>`). An UNSET DSN means that
//! section simply degrades to empty — the service still boots and renders with whatever sources
//! are present (zero configured sources is a valid, if empty, deployment).
//!
//! Every value keeps its dev default when the corresponding env var is unset/empty, so the dev
//! path boots with NO configuration and NO database — exactly like cortex. The audit ingest
//! credentials are resolved in [`crate::build_state_from_env`], not here, so this stays free of
//! secrets.

/// Default listen address (all interfaces, internal-only port 9070).
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:9070";

/// Hard cap on how many rows each section pulls/renders (keeps every column bounded). A user's
/// joined-room set + unread queues are small, so this is generous headroom, not a real limit.
pub const SECTION_LIMIT: i64 = 50;

/// Runtime configuration. Cheap to clone; shared read-only behind `Arc`.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// Listen address (`BIND_ADDR`).
    pub bind_addr: String,
    /// Murmur (chat) database DSN; `None` = the Chat section degrades to empty.
    pub murmur_dsn: Option<String>,
    /// Klaxon (notifications) database DSN; `None` = the Notifications section degrades to empty.
    pub klaxon_dsn: Option<String>,
    /// Current (RSS reader) database DSN; `None` = the Feed section degrades to empty.
    pub current_dsn: Option<String>,
}

impl Config {
    /// Default development configuration: bound port set, no sources (so dev/tests need no DB).
    pub fn dev() -> Self {
        Config {
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
            murmur_dsn: None,
            klaxon_dsn: None,
            current_dsn: None,
        }
    }

    /// Configuration with the dev defaults overridden by environment variables.
    pub fn from_env() -> Self {
        Config {
            bind_addr: env_nonempty("BIND_ADDR").unwrap_or_else(|| DEFAULT_BIND_ADDR.to_string()),
            murmur_dsn: env_nonempty("MURMUR_DATABASE_URL"),
            klaxon_dsn: env_nonempty("KLAXON_DATABASE_URL"),
            current_dsn: env_nonempty("CURRENT_DATABASE_URL"),
        }
    }
}

/// Read an env var, returning `None` when unset OR empty (empty never clobbers a default).
pub fn env_nonempty(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

/// True when an env var is a recognized truthy value (`on` / `true` / `1` / `yes`).
pub fn env_truthy(key: &str) -> bool {
    matches!(
        std::env::var(key)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "on" | "true" | "1" | "yes"
    )
}
