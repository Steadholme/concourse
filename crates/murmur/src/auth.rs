//! Gateway-injected identity + double-submit CSRF.
//!
//! Murmur does NO login of its own. It sits behind a Sluice `auth=sso` route at the subdomain
//! ROOT (`chat.w33d.xyz`), where the gateway runs the OIDC browser login against Keystone,
//! STRIPS any inbound `X-Auth-*`, and injects the verified `X-Auth-Subject` / `X-Auth-Email` /
//! `X-Auth-Scope`. Because Murmur is internal-only (never publicly reachable), it TRUSTS those
//! headers as the authenticated user — the message sender / room creator is ALWAYS taken from
//! the gateway headers, never from a client-supplied field.
//!
//! State-changing POSTs (create room / join / send) are double-submit CSRF protected: a random
//! token lives in a JS-readable `__Host-csrf` cookie AND is echoed by the dashboard's `fetch`
//! calls in the `X-CSRF-Token` header; the POST is accepted only when the two match. The token
//! is minted once on the dashboard render and REUSED across reloads for the cookie lifetime.

use axum::http::{header, HeaderMap};

use crate::error::AppError;

pub const HEADER_SUBJECT: &str = "x-auth-subject";
pub const HEADER_EMAIL: &str = "x-auth-email";
pub const HEADER_SCOPE: &str = "x-auth-scope";
pub const HEADER_GROUPS: &str = "x-auth-groups";
/// HMAC binding the injected identity to a 1-minute window (set by Sluice when GATEWAY_HMAC_KEY
/// is configured). See [`gateway_identity_ok`].
pub const HEADER_SIG: &str = "x-auth-sig";

/// Request header the dashboard echoes the CSRF token in (double-submit, paired with the cookie).
pub const HEADER_CSRF: &str = "x-csrf-token";

/// Double-submit CSRF cookie. `__Host-` prefix => Secure + Path=/ + no Domain, so the browser
/// only ever returns it over TLS to this exact host. It is intentionally JS-readable (not
/// HttpOnly) so the dashboard's `fetch` can copy it into the `X-CSRF-Token` header.
pub const CSRF_COOKIE: &str = "__Host-csrf";
/// CSRF cookie lifetime, seconds.
const CSRF_TTL: u64 = 3600;

/// The authenticated user's subject (stable id), if the gateway injected one.
pub fn user_sub(headers: &HeaderMap) -> Option<String> {
    header_value(headers, HEADER_SUBJECT)
}

/// The authenticated user's email, if the gateway injected one.
pub fn user_email(headers: &HeaderMap) -> Option<String> {
    header_value(headers, HEADER_EMAIL)
}

/// The user's email for display, falling back to a neutral label when unauthenticated.
pub fn display_email(headers: &HeaderMap) -> String {
    user_email(headers).unwrap_or_else(|| "—".to_string())
}

/// Require an authenticated user. Returns `(subject, email)`, or `Unauthorized` when no SSO
/// identity is present — defense in depth behind the gateway.
pub fn require_user(headers: &HeaderMap) -> Result<(String, String), AppError> {
    let sub = user_sub(headers).ok_or_else(|| {
        AppError::Unauthorized("no gateway SSO identity (X-Auth-Subject missing)".to_string())
    })?;
    let email = user_email(headers).unwrap_or_default();
    Ok((sub, email))
}

/// The GLOBAL admin groups that authorize the `/admin` panel — the ALWAYS-present seed of the
/// effective set. Membership in ANY resolved admin group (see [`admin_groups`]) unlocks the panel.
/// These two never change, so anything that works today keeps working.
pub const ADMIN_GROUPS: &[&str] = &["admins", "infra-admins"];

/// Default PRODUCT-SPECIFIC operator group for Murmur (the "chat" product). Overridable via the
/// `MURMUR_ADMIN_GROUP` env var (see [`admin_groups`]).
const PRODUCT_ADMIN_GROUP: &str = "chat-admins";

/// The effective admin group set: the two globals from [`ADMIN_GROUPS`] ALWAYS, PLUS one
/// product-scoped operator group (`chat-admins` by default, overridable via `MURMUR_ADMIN_GROUP`).
/// Resolved once from the environment and cached.
///
/// # Delegated admin
/// Chat moderation is DELEGABLE without granting GLOBAL admin: an operator adds a trusted user to
/// `chat-admins` (via Census) and they gain the SAME `/admin` powers, but ONLY for chat — no other
/// product trusts that group. This is purely ADDITIVE and safe — the two globals are never removed,
/// and until someone is actually placed in the product group the gate behaves exactly as before.
pub fn admin_groups() -> &'static [String] {
    static GROUPS: OnceLock<Vec<String>> = OnceLock::new();
    GROUPS.get_or_init(|| {
        let product = std::env::var("MURMUR_ADMIN_GROUP")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| PRODUCT_ADMIN_GROUP.to_string());
        let mut groups: Vec<String> = ADMIN_GROUPS.iter().map(|s| s.to_string()).collect();
        if !groups.iter().any(|g| g == &product) {
            groups.push(product);
        }
        groups
    })
}

/// The authenticated user's groups, parsed from the comma-separated `X-Auth-Groups` header
/// (injected AND HMAC-verified by the gateway, so it is trustworthy). Empty when absent/blank.
pub fn author_groups(headers: &HeaderMap) -> Vec<String> {
    header_value(headers, HEADER_GROUPS)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether the authenticated user belongs to `group` (exact match against `X-Auth-Groups`).
pub fn has_group(headers: &HeaderMap, group: &str) -> bool {
    author_groups(headers).iter().any(|g| g == group)
}

/// Whether the authenticated user is in ANY [`admin_groups`] entry (the two globals plus the
/// resolved product group).
pub fn is_admin(headers: &HeaderMap) -> bool {
    let groups = author_groups(headers);
    admin_groups().iter().any(|a| groups.iter().any(|g| g == a))
}

/// Require admin group membership for an `/admin` action. `Forbidden` (403) when the
/// authenticated user carries no admin group — an ordinary signed-in user gets 403 and never
/// sees the panel. `admin` overrides ownership: it does NOT consult `created_by`.
pub fn require_admin(headers: &HeaderMap) -> Result<(), AppError> {
    if is_admin(headers) {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "admin panel requires an admin group".to_string(),
        ))
    }
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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
    [win, win - 1]
        .iter()
        .any(|&w| ct_eq(sig.as_bytes(), sign_identity(key, &subject, &groups, w).as_bytes()))
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

// ---------------------------------------------------------------------------
// Cookies
// ---------------------------------------------------------------------------

/// Read a single cookie value from the request's `Cookie` header(s).
pub fn get_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    for hv in headers.get_all(header::COOKIE).iter() {
        let Ok(raw) = hv.to_str() else { continue };
        for pair in raw.split(';') {
            let pair = pair.trim();
            if let Some((k, v)) = pair.split_once('=') {
                if k.trim() == name {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}

/// `Set-Cookie` value for the (JS-readable) CSRF cookie.
pub fn csrf_cookie(value: &str) -> String {
    format!("{CSRF_COOKIE}={value}; Path=/; Secure; SameSite=Lax; Max-Age={CSRF_TTL}")
}

// ---------------------------------------------------------------------------
// CSRF (double-submit)
// ---------------------------------------------------------------------------

/// Mint a fresh CSRF token: 32 CSPRNG bytes, hex-encoded.
pub fn new_csrf_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG unavailable");
    hex::encode(bytes)
}

/// Resolve the CSRF token to embed in the dashboard render. Reuses the existing cookie token
/// when present (so it is stable across reloads/tabs); otherwise mints one and returns the
/// matching `Set-Cookie` to attach to the response.
pub fn ensure_csrf(headers: &HeaderMap) -> (String, Option<String>) {
    match get_cookie(headers, CSRF_COOKIE) {
        Some(c) if !c.is_empty() => (c, None),
        _ => {
            let token = new_csrf_token();
            let set = csrf_cookie(&token);
            (token, Some(set))
        }
    }
}

/// Double-submit check: the `X-CSRF-Token` header must equal the `__Host-csrf` cookie.
pub fn verify_csrf(headers: &HeaderMap) -> Result<(), AppError> {
    let submitted = header_value(headers, HEADER_CSRF).unwrap_or_default();
    let ok = match get_cookie(headers, CSRF_COOKIE) {
        Some(cookie) if !cookie.is_empty() => ct_eq(cookie.as_bytes(), submitted.as_bytes()),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(AppError::Unauthorized("CSRF token mismatch".to_string()))
    }
}

/// Double-submit check for server-rendered HTML forms: the `submitted` hidden `csrf` field must
/// equal the `__Host-csrf` cookie. Mirrors [`verify_csrf`] but takes the token from the form body
/// (the admin panel posts real `<form>`s, not the dashboard's `fetch` + `X-CSRF-Token` header).
pub fn verify_csrf_field(headers: &HeaderMap, submitted: &str) -> Result<(), AppError> {
    let ok = match get_cookie(headers, CSRF_COOKIE) {
        Some(cookie) if !cookie.is_empty() => ct_eq(cookie.as_bytes(), submitted.as_bytes()),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(AppError::Unauthorized("CSRF token mismatch".to_string()))
    }
}

/// Length-checked constant-time byte comparison.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csrf_token_is_random_and_hex() {
        let a = new_csrf_token();
        let b = new_csrf_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn csrf_double_submit_matches_and_rejects() {
        let token = new_csrf_token();
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            format!("{CSRF_COOKIE}={token}").parse().unwrap(),
        );
        headers.insert(HEADER_CSRF, token.parse().unwrap());
        assert!(verify_csrf(&headers).is_ok());

        // Wrong header token -> rejected.
        let mut bad = HeaderMap::new();
        bad.append(
            header::COOKIE,
            format!("{CSRF_COOKIE}={token}").parse().unwrap(),
        );
        bad.insert(HEADER_CSRF, "not-the-token".parse().unwrap());
        assert!(verify_csrf(&bad).is_err());

        // No cookie / no header at all -> rejected.
        assert!(verify_csrf(&HeaderMap::new()).is_err());
    }

    #[test]
    fn ensure_csrf_reuses_existing_cookie() {
        let token = new_csrf_token();
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            format!("{CSRF_COOKIE}={token}").parse().unwrap(),
        );
        let (t, set) = ensure_csrf(&headers);
        assert_eq!(t, token);
        assert!(set.is_none(), "existing cookie reused, no new Set-Cookie");

        let (t2, set2) = ensure_csrf(&HeaderMap::new());
        assert_eq!(t2.len(), 64);
        assert!(set2.unwrap().contains("__Host-csrf="));
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

    #[test]
    fn admin_gate_accepts_globals_and_product_group() {
        // Default (no MURMUR_ADMIN_GROUP set): globals PLUS the product group `chat-admins`.
        let resolved = admin_groups();
        assert!(resolved.iter().any(|g| g == "admins"));
        assert!(resolved.iter().any(|g| g == "infra-admins"));
        assert!(resolved.iter().any(|g| g == "chat-admins"));

        let admin = |g: &str| {
            let mut h = HeaderMap::new();
            h.insert(HEADER_GROUPS, g.parse().unwrap());
            is_admin(&h)
        };
        // Both globals still work.
        assert!(admin("admins"));
        assert!(admin("infra-admins"));
        // The delegated product group is accepted too.
        assert!(admin("chat-admins"));
        assert!(admin("dev, chat-admins"));
        // A random group is still denied, and no groups at all is denied.
        assert!(!admin("readers,writers"));
        assert!(!is_admin(&HeaderMap::new()));
    }

    #[test]
    fn require_user_needs_subject() {
        assert!(require_user(&HeaderMap::new()).is_err());
        let mut headers = HeaderMap::new();
        headers.insert(HEADER_SUBJECT, "u_123".parse().unwrap());
        headers.insert(HEADER_EMAIL, "a@steadholme.local".parse().unwrap());
        let (sub, email) = require_user(&headers).unwrap();
        assert_eq!(sub, "u_123");
        assert_eq!(email, "a@steadholme.local");
    }
}
