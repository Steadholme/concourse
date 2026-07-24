//! Server configuration with explicit development and production constructors.
//!
//! Tests and embedded callers may opt into [`Config::dev`] for an in-memory unsigned setup.
//! The standalone binary calls [`Config::from_env`], which always requires a non-empty
//! `GATEWAY_HMAC_KEY`; its database remains optional and defaults to the in-memory Store.

/// Default listen address (all interfaces, internal-only port 9060).
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:9060";

/// Canonical browser origin for the production WebSocket upgrade.
pub const CHAT_ORIGIN: &str = "https://chat.w33d.xyz";

/// Hard cap on how many messages a single `GET /messages` page returns.
pub const MESSAGE_PAGE_LIMIT: i64 = 50;

/// Hard cap on how many rooms the room list returns for one user.
pub const ROOM_LIST_LIMIT: i64 = 500;

/// Hard cap on how many people the "New DM" directory returns.
pub const DIRECTORY_LIMIT: i64 = 500;

/// Maximum stored message body length, in characters (oversized bodies are rejected, not
/// silently truncated).
pub const MAX_BODY_CHARS: usize = 8 * 1024;

/// Maximum room name length, in characters.
pub const MAX_ROOM_NAME_CHARS: usize = 120;

/// Maximum reaction emoji length, in characters. Generous enough for multi-codepoint emoji
/// (skin-tone modifiers, ZWJ / flag sequences) while rejecting arbitrary text as a "reaction".
pub const MAX_EMOJI_CHARS: usize = 32;

/// Maximum room topic / description length, in characters.
pub const MAX_TOPIC_CHARS: usize = 500;

/// Hard cap on how many pinned messages a room's pinned panel returns.
pub const PINNED_LIMIT: i64 = 100;

/// Hard cap on how many hits one search page returns.
pub const SEARCH_PAGE_LIMIT: i64 = 50;

/// Hard cap on how many mentions the global mentions view returns per page.
pub const MENTIONS_PAGE_LIMIT: i64 = 50;

/// Minimum search needle length (characters). Shorter queries are rejected to avoid scanning the
/// whole corpus for a single-character substring.
pub const MIN_SEARCH_QUERY_CHARS: usize = 2;

/// Maximum search needle length, in characters.
pub const MAX_SEARCH_QUERY_CHARS: usize = 200;

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
    /// Gateway identity-signing key. `None` is permitted only in the explicit dev config.
    pub gateway_hmac_key: Option<String>,
    /// Exact WebSocket Origin accepted in production. `None` is the explicit dev relaxation.
    pub websocket_origin: Option<String>,
}

impl Config {
    /// Default development configuration (in-memory friendly, no database).
    pub fn dev() -> Self {
        Config {
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
            gateway_hmac_key: None,
            websocket_origin: None,
        }
    }

    /// Production configuration. A non-empty gateway key is mandatory; there is no implicit
    /// unsigned production mode.
    pub fn from_env() -> Result<Self, String> {
        Self::production(
            env_nonempty("BIND_ADDR").unwrap_or_else(|| DEFAULT_BIND_ADDR.to_string()),
            env_nonempty("GATEWAY_HMAC_KEY"),
        )
    }

    fn production(bind_addr: String, gateway_hmac_key: Option<String>) -> Result<Self, String> {
        let gateway_hmac_key = gateway_hmac_key
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| "GATEWAY_HMAC_KEY must be non-empty".to_string())?;
        Ok(Config {
            bind_addr,
            gateway_hmac_key: Some(gateway_hmac_key),
            websocket_origin: Some(CHAT_ORIGIN.to_string()),
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_requires_a_non_empty_gateway_key() {
        assert!(Config::production("127.0.0.1:9060".to_string(), None).is_err());
        assert!(Config::production("127.0.0.1:9060".to_string(), Some("   ".to_string())).is_err());
        let config =
            Config::production("127.0.0.1:9060".to_string(), Some("secret".to_string())).unwrap();
        assert_eq!(config.gateway_hmac_key.as_deref(), Some("secret"));
        assert_eq!(config.websocket_origin.as_deref(), Some(CHAT_ORIGIN));
    }
}
