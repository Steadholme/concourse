//! Server configuration, env-driven with working dev defaults.
//!
//! The in-memory dev path boots with NO configuration and NO database — exactly like
//! inkwell/sanctum. Production overrides each value via the environment. The ingest token, the
//! VAPID keypair, and the audit credentials are resolved here (or in
//! [`crate::build_state_from_env`]) so the request path can read them straight off the `Config`.

/// Default listen address (all interfaces, internal-only port 9050).
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:9050";

/// Public base URL of this service (used only for absolute links in the UI).
pub const DEFAULT_PUBLIC_BASE_URL: &str = "https://notify.w33d.xyz";

/// Default SMTP submission target (the estate's own mail server) for the optional email channel.
pub const DEFAULT_SMTP_ADDR: &str = "corvid:587";

/// Default envelope-from used for the optional email channel.
pub const DEFAULT_SMTP_FROM: &str = "klaxon@w33d.xyz";

/// Hard cap on how many notifications the inbox renders / the API returns.
pub const LIST_LIMIT: usize = 200;

/// Hard cap on a notification title, in characters.
pub const MAX_TITLE_CHARS: usize = 300;
/// Hard cap on a notification body, in characters.
pub const MAX_BODY_CHARS: usize = 8 * 1024;

/// Hard cap on a registered webhook URL, in characters.
pub const MAX_WEBHOOK_URL_CHARS: usize = 2048;
/// Hard cap on a webhook signing secret, in characters.
pub const MAX_WEBHOOK_SECRET_CHARS: usize = 256;
/// Hard cap on how many webhooks a single user may register.
pub const MAX_WEBHOOKS_PER_USER: usize = 50;

/// Runtime configuration. Cheap to clone; shared read-only behind `Arc`.
#[derive(Clone, Debug)]
pub struct Config {
    /// Listen address (`BIND_ADDR`).
    pub bind_addr: String,
    /// Public base URL (`PUBLIC_BASE_URL`).
    pub public_base_url: String,
    /// Internal ingest token (`KLAXON_INGEST_TOKEN`). When set, `POST /api/notify` requires a
    /// matching `Authorization: Bearer` (constant-time). `None` => dev mode: ingest is open.
    pub ingest_token: Option<String>,
    /// VAPID application-server public key (`VAPID_PUBLIC_KEY`), surfaced at `/vapidPublicKey`.
    pub vapid_public_key: Option<String>,
    /// VAPID application-server private key (`VAPID_PRIVATE_KEY`). Presence (with the public key)
    /// flips push to `configured: true`.
    pub vapid_private_key: Option<String>,
    /// Optional email channel toggle (`KLAXON_SMTP_ENABLED`).
    pub smtp_enabled: bool,
    /// SMTP submission target (`KLAXON_SMTP_ADDR`, default `corvid:587`).
    pub smtp_addr: String,
    /// Envelope-from for the email channel (`KLAXON_SMTP_FROM`).
    pub smtp_from: String,
}

impl Config {
    /// Default development configuration (in-memory, no database, no ingest token, no VAPID keys,
    /// email channel off).
    pub fn dev() -> Self {
        Config {
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
            public_base_url: DEFAULT_PUBLIC_BASE_URL.to_string(),
            ingest_token: None,
            vapid_public_key: None,
            vapid_private_key: None,
            smtp_enabled: false,
            smtp_addr: DEFAULT_SMTP_ADDR.to_string(),
            smtp_from: DEFAULT_SMTP_FROM.to_string(),
        }
    }

    /// Configuration with the dev defaults overridden by environment variables.
    pub fn from_env() -> Self {
        let mut config = Config::dev();
        if let Some(v) = env_nonempty("BIND_ADDR") {
            config.bind_addr = v;
        }
        if let Some(v) = env_nonempty("PUBLIC_BASE_URL") {
            config.public_base_url = v.trim_end_matches('/').to_string();
        }
        config.ingest_token = env_nonempty("KLAXON_INGEST_TOKEN");
        config.vapid_public_key = env_nonempty("VAPID_PUBLIC_KEY");
        config.vapid_private_key = env_nonempty("VAPID_PRIVATE_KEY");
        config.smtp_enabled = env_truthy("KLAXON_SMTP_ENABLED");
        if let Some(v) = env_nonempty("KLAXON_SMTP_ADDR") {
            config.smtp_addr = v;
        }
        if let Some(v) = env_nonempty("KLAXON_SMTP_FROM") {
            config.smtp_from = v;
        }
        config
    }

    /// True when both halves of the VAPID keypair are present (Web Push is "configured").
    pub fn push_configured(&self) -> bool {
        self.vapid_public_key.is_some() && self.vapid_private_key.is_some()
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

/// Interpret a boolean-ish env var (`on` / `true` / `1` / `yes`, case-insensitive).
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
