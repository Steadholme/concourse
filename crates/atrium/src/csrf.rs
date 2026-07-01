//! Double-submit CSRF for the inbox's state-changing POSTs.
//!
//! Atrium used to be pure-read (no CSRF surface). Adding mark-read / dismiss introduces writes, so
//! those POSTs are protected with the stateless double-submit-cookie pattern: the dashboard GET
//! plants an unpredictable token in a NON-HttpOnly, `SameSite=Strict` cookie AND echoes the same
//! token into the page (`<meta name="csrf-token">`). The action `fetch()` reads the meta token and
//! sends it back in the `X-CSRF-Token` header; the server accepts the POST only when the header
//! equals the cookie (constant-time). A cross-site attacker can drive the browser to send the
//! cookie but — bound by the same-origin policy — cannot read it to forge the matching header, so
//! the request fails. No server-side session state is needed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;

/// Cookie carrying the double-submit token. NOT HttpOnly (the page JS must read the paired meta
/// token to echo it) — its confidentiality rests on the same-origin policy, not on HttpOnly.
pub const COOKIE_NAME: &str = "atrium_csrf";
/// The request header the client echoes the token back in.
pub const HEADER_NAME: &str = "x-csrf-token";

/// Mint an unpredictable token: SHA-256 over a high-resolution timestamp, a per-process counter,
/// and a per-call stack address, hex-encoded. No extra crate — reuses the `sha2` primitive already
/// vendored for the gateway-signature check.
pub fn issue_token() -> String {
    use sha2::{Digest, Sha256};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = &n as *const _ as usize;
    let mut h = Sha256::new();
    h.update(nanos.to_le_bytes());
    h.update(n.to_le_bytes());
    h.update(seed.to_le_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(digest.len() * 2);
    for b in &digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The `Set-Cookie` value planting `token`. `SameSite=Strict` + `Secure` + `Path=/`; a day's life.
pub fn set_cookie(token: &str) -> String {
    format!("{COOKIE_NAME}={token}; Path=/; Max-Age=86400; SameSite=Strict; Secure")
}

/// The CSRF token already carried on the request's cookie, if any.
pub fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix(COOKIE_NAME).and_then(|r| r.strip_prefix('=')) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// The token echoed in the `X-CSRF-Token` header, if any.
fn header_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(HEADER_NAME)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Double-submit check: the request must carry BOTH a CSRF cookie and a matching `X-CSRF-Token`
/// header (constant-time compare). A missing cookie or header, or a mismatch, fails (=> 403).
pub fn valid(headers: &HeaderMap) -> bool {
    let (Some(cookie), Some(header)) = (cookie_token(headers), header_token(headers)) else {
        return false;
    };
    ct_eq(cookie.as_bytes(), header.as_bytes())
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

    fn with(cookie: Option<&str>, header: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(c) = cookie {
            h.insert(
                axum::http::header::COOKIE,
                format!("{COOKIE_NAME}={c}; other=1").parse().unwrap(),
            );
        }
        if let Some(x) = header {
            h.insert(HEADER_NAME, x.parse().unwrap());
        }
        h
    }

    #[test]
    fn issued_tokens_are_unpredictable_and_distinct() {
        let a = issue_token();
        let b = issue_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn matching_cookie_and_header_pass() {
        assert!(valid(&with(Some("tok-abc"), Some("tok-abc"))));
    }

    #[test]
    fn mismatch_or_missing_fails() {
        assert!(!valid(&with(Some("tok-abc"), Some("tok-xyz"))));
        assert!(!valid(&with(Some("tok-abc"), None)));
        assert!(!valid(&with(None, Some("tok-abc"))));
        assert!(!valid(&with(None, None)));
        // An empty cookie value is treated as absent.
        assert!(!valid(&with(Some(""), Some(""))));
    }

    #[test]
    fn cookie_token_is_parsed_out_of_a_cookie_jar() {
        assert_eq!(cookie_token(&with(Some("xyz"), None)).as_deref(), Some("xyz"));
    }
}
