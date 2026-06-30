//! Gateway-injected identity, double-submit CSRF, and the internal ingest bearer token.
//!
//! Klaxon does NO login of its own. The notification inbox sits behind a Sluice `auth=sso` route,
//! where the gateway runs the OIDC browser login against Keystone, STRIPS any inbound `X-Auth-*`,
//! and injects the verified `X-Auth-Subject` / `X-Auth-Email`. Because Klaxon is internal-only
//! (never publicly reachable), it TRUSTS those headers as the signed-in user — the inbox is ALWAYS
//! scoped to the gateway identity, never to a client-supplied field.
//!
//! State-changing POSTs in the UI (`/api/read`, `/api/subscribe`) are double-submit CSRF
//! protected: a random token lives in a `__Host-csrf` cookie AND in the submitted form/field; the
//! POST is accepted only when the two match.
//!
//! The `POST /api/notify` ingest path uses its OWN bearer auth instead (it is called by other
//! internal services, not a browser): a `Authorization: Bearer <KLAXON_INGEST_TOKEN>` that matches
//! the configured token (constant-time). When no token is configured (dev), ingest is open.

use axum::http::{header, HeaderMap};

use crate::error::AppError;
use crate::random_alnum;

pub const HEADER_SUBJECT: &str = "x-auth-subject";
pub const HEADER_EMAIL: &str = "x-auth-email";

/// Double-submit CSRF cookie. `__Host-` prefix => Secure + Path=/ + no Domain, so the browser only
/// ever returns it over TLS to this exact host.
pub const CSRF_COOKIE: &str = "__Host-csrf";
/// CSRF cookie lifetime, seconds.
const CSRF_TTL: u64 = 3600;
/// CSRF token length (characters from the 62-symbol alphabet ~= 238 bits).
const CSRF_LEN: usize = 40;

/// The signed-in user's subject (stable id), if the gateway injected one.
pub fn subject(headers: &HeaderMap) -> Option<String> {
    header_value(headers, HEADER_SUBJECT)
}

/// The signed-in user's email, if the gateway injected one.
pub fn email(headers: &HeaderMap) -> Option<String> {
    header_value(headers, HEADER_EMAIL)
}

/// The user's email for display, falling back to a neutral label when unauthenticated.
pub fn display_email(headers: &HeaderMap) -> String {
    email(headers).unwrap_or_else(|| "—".to_string())
}

/// The addressing keys for the current viewer: the subject plus the email (a producer may target
/// either). Empty values are dropped. Returns `Unauthorized` when no subject is present.
pub fn require_keys(headers: &HeaderMap) -> Result<Vec<String>, AppError> {
    let sub = subject(headers).ok_or_else(|| {
        AppError::Unauthorized("no gateway SSO identity (X-Auth-Subject missing)".to_string())
    })?;
    let mut keys = vec![sub];
    if let Some(e) = email(headers) {
        if !keys.contains(&e) {
            keys.push(e);
        }
    }
    Ok(keys)
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse a `Authorization: Bearer <token>` header, returning the token.
pub fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = raw.strip_prefix("Bearer ").or_else(|| raw.strip_prefix("bearer "))?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Authorize a `POST /api/notify` ingest request. When `ingest_token` is configured, a `Bearer`
/// must match it (constant-time). When it is `None` (dev), ingest is OPEN (logged once at startup).
pub fn ingest_authorized(headers: &HeaderMap, ingest_token: Option<&str>) -> bool {
    match ingest_token {
        Some(cfg) if !cfg.is_empty() => match bearer_token(headers) {
            Some(presented) => ct_eq(presented.as_bytes(), cfg.as_bytes()),
            None => false,
        },
        // No token configured -> dev mode, ingest is open.
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Cookies + CSRF (double-submit)
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

/// Mint a fresh CSRF token (same value goes in the cookie and the form field).
pub fn new_csrf_token() -> String {
    random_alnum(CSRF_LEN)
}

/// Resolve the CSRF token to embed in this render's forms. Reuses the existing cookie token when
/// present (so it is stable across pages/tabs); otherwise mints one and returns the matching
/// `Set-Cookie` to attach to the response.
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

/// Double-submit check: the `submitted` form token must equal the `__Host-csrf` cookie.
pub fn verify_csrf(headers: &HeaderMap, submitted: &str) -> Result<(), AppError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn require_keys_needs_subject() {
        assert!(require_keys(&HeaderMap::new()).is_err());
        let mut h = HeaderMap::new();
        h.insert(HEADER_SUBJECT, HeaderValue::from_static("u_123"));
        h.insert(HEADER_EMAIL, HeaderValue::from_static("a@w33d.xyz"));
        let keys = require_keys(&h).unwrap();
        assert_eq!(keys, vec!["u_123".to_string(), "a@w33d.xyz".to_string()]);
    }

    #[test]
    fn ingest_open_when_unconfigured_and_checked_when_set() {
        // No token configured -> open.
        assert!(ingest_authorized(&HeaderMap::new(), None));
        // Token configured, none presented -> denied.
        assert!(!ingest_authorized(&HeaderMap::new(), Some("s3cr3t")));
        // Wrong bearer -> denied.
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer nope"));
        assert!(!ingest_authorized(&h, Some("s3cr3t")));
        // Right bearer -> allowed.
        let mut h2 = HeaderMap::new();
        h2.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer s3cr3t"));
        assert!(ingest_authorized(&h2, Some("s3cr3t")));
    }

    #[test]
    fn csrf_double_submit_matches_and_rejects() {
        let token = new_csrf_token();
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            format!("{CSRF_COOKIE}={token}").parse().unwrap(),
        );
        assert!(verify_csrf(&headers, &token).is_ok());
        assert!(verify_csrf(&headers, "not-the-token").is_err());
        assert!(verify_csrf(&HeaderMap::new(), &token).is_err());
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
        assert_eq!(t2.len(), CSRF_LEN);
        assert!(set2.unwrap().contains("__Host-csrf="));
    }
}
