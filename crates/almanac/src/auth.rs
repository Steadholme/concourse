//! Gateway-injected identity + CSRF (double-submit).
//!
//! Almanac does NO login of its own. It sits behind a Sluice `auth=sso` route on
//! `cal.w33d.xyz`, where the gateway runs the OIDC browser login, STRIPS any inbound
//! `X-Auth-*`, and injects the verified `X-Auth-Subject` / `X-Auth-Email`. Because Almanac is
//! internal-only (never publicly reachable except through Sluice), it TRUSTS the injected
//! headers: the OWNER of every event/contact is the `X-Auth-Subject` (a stable id), NEVER a
//! client-supplied field; `X-Auth-Email` is used only for the signed-in display.
//!
//! State-changing POSTs are additionally guarded by a double-submit CSRF token: a random token
//! is minted on every form-bearing GET, set in the JS-free `__Host-almanac_csrf` cookie AND
//! placed in each form's hidden field; the POST requires the two to match (constant-time).

use axum::http::{header, HeaderMap};

/// Header carrying the gateway-verified signed-in email (display only).
pub const HEADER_EMAIL: &str = "x-auth-email";
/// Header carrying the gateway-verified stable subject id (the row owner).
pub const HEADER_SUBJECT: &str = "x-auth-subject";
/// Owner used when no gateway identity is present (direct dev hit with no Sluice in front). In
/// production the gateway always injects a real subject, so this only affects local dev/tests.
pub const ANON_OWNER: &str = "anonymous";
/// Double-submit CSRF cookie. `__Host-` prefix => browsers only return it over TLS to this
/// exact host with `Path=/` and no `Domain`, so it cannot be planted by a sibling subdomain.
pub const CSRF_COOKIE: &str = "__Host-almanac_csrf";
/// CSRF cookie lifetime, seconds.
const CSRF_TTL: u64 = 3600;

/// The signed-in user's stable subject id, used as the row OWNER. Falls back to [`ANON_OWNER`]
/// when the gateway injected nothing (local dev), so the app still works without Sluice.
pub fn owner_subject(headers: &HeaderMap) -> String {
    header_nonempty(headers, HEADER_SUBJECT).unwrap_or_else(|| ANON_OWNER.to_string())
}

/// The signed-in user's email, if the gateway injected one. `None` when absent or blank, letting
/// the caller fall back to a generic label.
pub fn signed_in_email(headers: &HeaderMap) -> Option<String> {
    header_nonempty(headers, HEADER_EMAIL)
}

fn header_nonempty(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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

/// `Set-Cookie` value for the CSRF cookie.
pub fn csrf_cookie(value: &str) -> String {
    format!("{CSRF_COOKIE}={value}; Path=/; Secure; SameSite=Lax; Max-Age={CSRF_TTL}")
}

// ---------------------------------------------------------------------------
// CSRF (double-submit)
// ---------------------------------------------------------------------------

/// Mint a fresh CSRF token (the same value goes in the cookie and every hidden form field).
pub fn new_csrf_token() -> String {
    random_hex()
}

/// Double-submit check: the form-`submitted` token must equal the `__Host-almanac_csrf` cookie.
pub fn verify_csrf(headers: &HeaderMap, submitted: &str) -> bool {
    match get_cookie(headers, CSRF_COOKIE) {
        Some(cookie) if !cookie.is_empty() => ct_eq(cookie.as_bytes(), submitted.as_bytes()),
        _ => false,
    }
}

/// A 32-byte CSPRNG value, hex-encoded. Used for CSRF tokens and event/contact ids.
pub fn random_hex() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG unavailable");
    hex::encode(bytes)
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
    fn csrf_double_submit() {
        let token = new_csrf_token();
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            format!("{CSRF_COOKIE}={token}").parse().unwrap(),
        );
        assert!(verify_csrf(&headers, &token));
        assert!(!verify_csrf(&headers, "not-the-token"));
        // No cookie -> never valid.
        assert!(!verify_csrf(&HeaderMap::new(), &token));
    }

    #[test]
    fn random_hex_is_64_chars_and_unique() {
        let a = random_hex();
        let b = random_hex();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }

    #[test]
    fn owner_falls_back_to_anon_then_uses_subject() {
        let mut headers = HeaderMap::new();
        assert_eq!(owner_subject(&headers), ANON_OWNER);
        headers.insert(HEADER_SUBJECT, "  u_alice  ".parse().unwrap());
        assert_eq!(owner_subject(&headers), "u_alice");
    }

    #[test]
    fn signed_in_email_trims_and_filters_blank() {
        let mut headers = HeaderMap::new();
        assert_eq!(signed_in_email(&headers), None);
        headers.insert(HEADER_EMAIL, "  a@b.co  ".parse().unwrap());
        assert_eq!(signed_in_email(&headers).as_deref(), Some("a@b.co"));
    }
}
