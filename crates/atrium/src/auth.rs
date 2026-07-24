//! Gateway-injected identity.
//!
//! Atrium does NO login of its own. It sits behind a Sluice `auth=sso` route, where the gateway
//! runs the OIDC browser login against Keystone, STRIPS any inbound `X-Auth-*`, and injects the
//! verified `X-Auth-Subject` / `X-Auth-Email` / `X-Auth-Scope`. Atrium is internal-only (never
//! publicly reachable) and TRUSTS those headers: the subject is the key every federated query is
//! scoped by (so a viewer only ever sees their OWN chat/notifications/feed), and the email drives
//! the app-bar "signed in as" line.
//!
//! Atrium is a pure read aggregator over GET — there are NO state-changing POSTs, so there is no
//! CSRF surface — and it re-emits NO `X-Auth-*` header to any backend (it talks to the source
//! databases directly over sqlx), so there is nothing inbound to strip here.

use axum::http::HeaderMap;

pub const HEADER_SUBJECT: &str = "x-auth-subject";
pub const HEADER_EMAIL: &str = "x-auth-email";
pub const HEADER_SCOPE: &str = "x-auth-scope";
pub const HEADER_GROUPS: &str = "x-auth-groups";
/// HMAC binding the injected identity to a 1-minute window (set by Sluice when GATEWAY_HMAC_KEY
/// is configured). See [`gateway_identity_ok`].
pub const HEADER_SIG: &str = "x-auth-sig";

/// The authenticated viewer's subject (stable user id), if the gateway injected one. This is the
/// ownership key every federated section query is filtered by.
pub fn subject(headers: &HeaderMap) -> Option<String> {
    header_value(headers, HEADER_SUBJECT)
}

/// The authenticated viewer's email, if the gateway injected one.
pub fn email(headers: &HeaderMap) -> Option<String> {
    header_value(headers, HEADER_EMAIL)
}

/// The viewer's email for display, falling back to a neutral label when unauthenticated (e.g. a
/// probe with no gateway session).
pub fn display_email(headers: &HeaderMap) -> String {
    email(headers).unwrap_or_else(|| "—".to_string())
}

/// The viewer's granted scope string, if any (purely informational; Atrium gates nothing on it).
pub fn scope(headers: &HeaderMap) -> Option<String> {
    header_value(headers, HEADER_SCOPE)
}

// ---------------------------------------------------------------------------
// Gateway identity signature (X-Auth-Sig) verification
// ---------------------------------------------------------------------------

use std::sync::OnceLock;

/// The shared gateway HMAC key, read once from `GATEWAY_HMAC_KEY`. Empty (unset) disables
/// verification — the pre-signature behavior, fully backward compatible.
fn gateway_key() -> &'static str {
    static KEY: OnceLock<String> = OnceLock::new();
    KEY.get_or_init(|| std::env::var("GATEWAY_HMAC_KEY").unwrap_or_default())
        .as_str()
}

/// Verify the gateway-injected identity is authentic. When `GATEWAY_HMAC_KEY` is set AND an
/// identity (`X-Auth-Subject`) is present, a valid `X-Auth-Sig` — HMAC-SHA256 over
/// `subject "\n" groups "\n" minute` for the current OR previous minute — is REQUIRED; a rogue
/// peer that POSTs `X-Auth-Subject` directly (bypassing Sluice) cannot forge it. Returns:
/// - `true` when the key is unset (verification off), or no identity header is present
///   (public/dev path), or the signature is valid;
/// - `false` when an identity is present but the signature is missing or invalid (=> 401).
pub fn gateway_identity_ok(headers: &HeaderMap) -> bool {
    let key = gateway_key();
    if key.is_empty() {
        return true;
    }
    let Some(subject) = header_value(headers, HEADER_SUBJECT) else {
        return true; // no injected identity to verify (public route / local dev)
    };
    let groups = header_value(headers, HEADER_GROUPS).unwrap_or_default();
    let Some(sig) = header_value(headers, HEADER_SIG) else {
        return false; // identity present but unsigned — reject
    };
    let win = now_unix() / 60;
    // Accept the current and previous minute (clock skew + minute-boundary tolerance).
    [win, win - 1].iter().any(|&w| {
        ct_eq(
            sig.as_bytes(),
            sign_identity(key, &subject, &groups, w).as_bytes(),
        )
    })
}

/// Recompute the gateway signature — byte-identical to Sluice's `auth.SignIdentity` (Go).
fn sign_identity(key: &str, subject: &str, groups: &str, window: i64) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key len");
    mac.update(subject.as_bytes());
    mac.update(b"\n");
    mac.update(groups.as_bytes());
    mac.update(b"\n");
    mac.update(window.to_string().as_bytes());
    to_hex(&mac.finalize().into_bytes())
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Length-checked constant-time byte comparison (no early return on the first differing byte).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_identity_is_none_and_dash() {
        let h = HeaderMap::new();
        assert!(subject(&h).is_none());
        assert!(email(&h).is_none());
        assert_eq!(display_email(&h), "—");
    }

    #[test]
    fn injected_identity_is_trusted() {
        let mut h = HeaderMap::new();
        h.insert(HEADER_SUBJECT, "u_123".parse().unwrap());
        h.insert(HEADER_EMAIL, "ops@w33d.xyz".parse().unwrap());
        assert_eq!(subject(&h).as_deref(), Some("u_123"));
        assert_eq!(display_email(&h), "ops@w33d.xyz");
    }

    #[test]
    fn sign_identity_matches_go_vector() {
        // MUST equal sluice/internal/auth/sig_test.go — the cross-language contract.
        assert_eq!(
            sign_identity("test-key", "usr_alice", "admins,devs", 1),
            "ddc77236dcfb03dd9f462f7c84e1b25e58f5fc380997695a689e6c3ac4bb3777"
        );
        assert_eq!(
            sign_identity("test-key", "usr_bob", "", 2),
            "930f82fb1224e69c9c5bc46e545c3b108b1eeb6c9078c7a33fc24f30c595f658"
        );
    }

    #[test]
    fn gateway_ok_when_key_unset() {
        // No GATEWAY_HMAC_KEY in the test env => verification disabled => always ok.
        let mut h = HeaderMap::new();
        h.insert(HEADER_SUBJECT, "user-42".parse().unwrap());
        assert!(gateway_identity_ok(&h));
    }
}
