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
}
