//! Web Push cryptography: VAPID (RFC 8292) JWT signing + RFC 8291 `aes128gcm` payload encryption.
//!
//! Both primitives run on the pure-Rust `ring` backend that the estate already links through
//! rustls — NO OpenSSL:
//! - **VAPID (RFC 8292).** An ES256 (ECDSA P-256 / SHA-256) JWT over the push endpoint's origin,
//!   signed with the configured application-server keypair. The `Authorization: vapid t=<jwt>,
//!   k=<pubkey>` header authenticates *this* server to the push service.
//! - **Payload encryption (RFC 8291).** An ephemeral ECDH-P256 agreement with the browser's
//!   subscription key, HKDF-SHA256 key derivation, and a single AES-128-GCM record, framed with the
//!   RFC 8188 `aes128gcm` content-encoding header. The push service relays the ciphertext blind; only
//!   the subscribed browser can decrypt it.
//!
//! The VAPID/subscription keys are the standard base64url (unpadded) Web Push encodings: the private
//! key is the raw 32-byte P-256 scalar, and every public key is the 65-byte uncompressed point.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hkdf::Hkdf;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_128_GCM};
use ring::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use sha2::Sha256;

/// VAPID JWT lifetime, seconds (RFC 8292 caps it at 24h; 12h leaves generous skew room).
const VAPID_JWT_TTL_SECS: i64 = 12 * 60 * 60;
/// Uncompressed P-256 public point length (`0x04 || X(32) || Y(32)`).
const P256_PUBLIC_LEN: usize = 65;

/// Decode a base64url value, tolerating both padded and unpadded input (browsers emit either).
pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let trimmed = s.trim().trim_end_matches('=');
    URL_SAFE_NO_PAD.decode(trimmed).ok()
}

/// Encode bytes as unpadded base64url.
pub fn b64url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The origin (`scheme://authority`) of a push endpoint — the VAPID JWT `aud` claim. Returns `None`
/// for a non-`http(s)` or authority-less URL.
pub fn endpoint_origin(endpoint: &str) -> Option<String> {
    let (scheme, rest) = endpoint.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

/// Sign a VAPID JWT (RFC 8292): ES256 over `base64url(header).base64url(claims)` with the
/// application-server keypair. `private_b64`/`public_b64` are the standard Web Push base64url keys
/// (raw 32-byte scalar / 65-byte point). Returns the compact JWT, or `None` on a malformed key.
pub fn sign_vapid_jwt(
    private_b64: &str,
    public_b64: &str,
    audience: &str,
    subject: &str,
    now: i64,
) -> Option<String> {
    let private_key = b64url_decode(private_b64)?;
    let public_key = b64url_decode(public_b64)?;
    let rng = SystemRandom::new();
    let key_pair = EcdsaKeyPair::from_private_key_and_public_key(
        &ECDSA_P256_SHA256_FIXED_SIGNING,
        &private_key,
        &public_key,
        &rng,
    )
    .ok()?;

    let header = b64url_encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let exp = now + VAPID_JWT_TTL_SECS;
    // aud/sub come from our own config (origin + configured contact) — never user input — so the
    // fixed-shape JSON needs no escaping.
    let claims = format!(r#"{{"aud":"{audience}","exp":{exp},"sub":"{subject}"}}"#);
    let payload = b64url_encode(claims.as_bytes());
    let signing_input = format!("{header}.{payload}");

    let signature = key_pair.sign(&rng, signing_input.as_bytes()).ok()?;
    Some(format!("{signing_input}.{}", b64url_encode(signature.as_ref())))
}

/// Build the `Authorization` header value for an `aes128gcm` Web Push request:
/// `vapid t=<jwt>, k=<application-server public key>`.
pub fn vapid_authorization(
    private_b64: &str,
    public_b64: &str,
    subject: &str,
    endpoint: &str,
    now: i64,
) -> Option<String> {
    let audience = endpoint_origin(endpoint)?;
    let jwt = sign_vapid_jwt(private_b64, public_b64, &audience, subject, now)?;
    // Re-emit the public key canonically (unpadded base64url) regardless of how it was configured.
    let public_key = b64url_decode(public_b64)?;
    Some(format!("vapid t={jwt}, k={}", b64url_encode(&public_key)))
}

/// HKDF-SHA256: `Expand(Extract(salt, ikm), info, len)`.
fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Option<Vec<u8>> {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut out = vec![0u8; len];
    hk.expand(info, &mut out).ok()?;
    Some(out)
}

/// Encrypt `plaintext` for a browser subscription per RFC 8291, returning the full `aes128gcm` body
/// (RFC 8188 header + one AES-128-GCM record) to POST with `Content-Encoding: aes128gcm`.
///
/// `p256dh_b64` is the subscription's 65-byte public key, `auth_b64` its 16-byte auth secret (both
/// standard Web Push base64url). Returns `None` on malformed keys or a crypto failure.
pub fn encrypt_payload(p256dh_b64: &str, auth_b64: &str, plaintext: &[u8]) -> Option<Vec<u8>> {
    let ua_public = b64url_decode(p256dh_b64)?;
    let auth_secret = b64url_decode(auth_b64)?;
    if ua_public.len() != P256_PUBLIC_LEN {
        return None;
    }

    let rng = SystemRandom::new();

    // Ephemeral application-server ECDH keypair (fresh per message, RFC 8291 §3.1).
    let as_private = EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).ok()?;
    let as_public = as_private.compute_public_key().ok()?;
    let as_public = as_public.as_ref().to_vec();

    let peer = UnparsedPublicKey::new(&agreement::ECDH_P256, ua_public.clone());
    let ecdh_secret =
        agreement::agree_ephemeral(as_private, &peer, |secret| secret.to_vec()).ok()?;

    // RFC 8291 §3.3/3.4: derive the RFC 8188 input keying material from the ECDH secret, keyed by
    // the auth secret and bound to both public keys.
    let mut key_info = Vec::with_capacity(14 + P256_PUBLIC_LEN * 2);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(&ua_public);
    key_info.extend_from_slice(&as_public);
    let ikm = hkdf(&auth_secret, &ecdh_secret, &key_info, 32)?;

    // RFC 8188 §2.1: content-encryption key + nonce from a random salt.
    let mut salt = [0u8; 16];
    rng.fill(&mut salt).ok()?;
    let cek = hkdf(&salt, &ikm, b"Content-Encoding: aes128gcm\0", 16)?;
    let nonce = hkdf(&salt, &ikm, b"Content-Encoding: nonce\0", 12)?;

    // One record: plaintext followed by the RFC 8188 last-record delimiter (0x02), then sealed.
    let mut record = Vec::with_capacity(plaintext.len() + 1 + AES_128_GCM.tag_len());
    record.extend_from_slice(plaintext);
    record.push(0x02);

    let unbound = UnboundKey::new(&AES_128_GCM, &cek).ok()?;
    let key = LessSafeKey::new(unbound);
    let nonce = Nonce::assume_unique_for_key(nonce.as_slice().try_into().ok()?);
    key.seal_in_place_append_tag(nonce, Aad::empty(), &mut record).ok()?;

    // RFC 8188 §2.1 framing: salt(16) || rs(4, BE) || idlen(1) || keyid(as_public) || ciphertext.
    let record_size: u32 = 4096;
    let mut body = Vec::with_capacity(16 + 4 + 1 + as_public.len() + record.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&record_size.to_be_bytes());
    body.push(as_public.len() as u8);
    body.extend_from_slice(&as_public);
    body.extend_from_slice(&record);
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A valid P-256 application-server (VAPID) keypair in Web Push base64url form.
    const VAPID_PRIVATE: &str = "8jsxhSo-oULXEXikP9_ztyA2roWlC9u6YRkumCBKX3A";
    const VAPID_PUBLIC: &str =
        "BPZJwTgzHz_8B01CNha0TvANNiyq8AD9OuiuQA1JPJt_ojU0KKKGF7_qWRvRLkMXSO1dL00WKuBdRRdy4dWQw9g";
    // A browser subscription (ECDH public key + auth secret).
    const SUB_P256DH: &str =
        "BMQKpc-kR3wquOl5TxQuEyUI0EY9M0aJhK-DkYl8UHoTxCGq76Y9CO5GCy48VshMKOLVcrzA1Ey7XLDNRUn1T6k";
    const SUB_AUTH: &str = "hVyvhjZcOHdWrGYxIq_-Bw";

    fn part(jwt: &str, idx: usize) -> Vec<u8> {
        b64url_decode(jwt.split('.').nth(idx).unwrap()).unwrap()
    }

    #[test]
    fn vapid_jwt_has_es256_header_and_expected_claims() {
        let jwt = sign_vapid_jwt(
            VAPID_PRIVATE,
            VAPID_PUBLIC,
            "https://fcm.googleapis.com",
            "mailto:ops@w33d.xyz",
            1_700_000_000,
        )
        .expect("signs");
        assert_eq!(jwt.split('.').count(), 3, "compact JWS: header.payload.sig");

        let header: serde_json::Value = serde_json::from_slice(&part(&jwt, 0)).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["typ"], "JWT");

        let claims: serde_json::Value = serde_json::from_slice(&part(&jwt, 1)).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["sub"], "mailto:ops@w33d.xyz");
        assert_eq!(claims["exp"], 1_700_000_000i64 + VAPID_JWT_TTL_SECS);

        // The ES256 signature must verify under the configured public key — this proves the raw
        // scalar was loaded correctly and the JWT was actually signed (not just shaped).
        let signing_input = {
            let mut it = jwt.rsplitn(2, '.');
            let _sig = it.next();
            it.next().unwrap().to_string()
        };
        let sig = part(&jwt, 2);
        let public = b64url_decode(VAPID_PUBLIC).unwrap();
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_FIXED,
            public,
        )
        .verify(signing_input.as_bytes(), &sig)
        .expect("VAPID signature verifies under the application-server public key");
    }

    #[test]
    fn vapid_authorization_header_shape() {
        let auth = vapid_authorization(
            VAPID_PRIVATE,
            VAPID_PUBLIC,
            "mailto:ops@w33d.xyz",
            "https://fcm.googleapis.com/fcm/send/abc123",
            1_700_000_000,
        )
        .expect("builds header");
        assert!(auth.starts_with("vapid t="));
        assert!(auth.contains(", k="));
        // The advertised key is the canonical unpadded base64url of the configured public key.
        assert!(auth.ends_with(&b64url_encode(&b64url_decode(VAPID_PUBLIC).unwrap())));
    }

    #[test]
    fn endpoint_origin_extraction() {
        assert_eq!(
            endpoint_origin("https://fcm.googleapis.com/fcm/send/x").as_deref(),
            Some("https://fcm.googleapis.com")
        );
        assert_eq!(
            endpoint_origin("https://host:8443/p?q=1").as_deref(),
            Some("https://host:8443")
        );
        assert!(endpoint_origin("ftp://host/x").is_none());
        assert!(endpoint_origin("https://").is_none());
    }

    #[test]
    fn encrypt_payload_produces_valid_aes128gcm_framing() {
        let plaintext = b"When I grow up, I want to be a watermelon";
        let body = encrypt_payload(SUB_P256DH, SUB_AUTH, plaintext).expect("encrypts");

        // Header: salt(16) || rs(4) || idlen(1) || keyid(idlen).
        assert!(body.len() > 16 + 4 + 1 + P256_PUBLIC_LEN);
        let rs = u32::from_be_bytes(body[16..20].try_into().unwrap());
        assert_eq!(rs, 4096);
        let idlen = body[20] as usize;
        assert_eq!(idlen, P256_PUBLIC_LEN, "keyid is the 65-byte ephemeral point");
        // keyid is an uncompressed point (leading 0x04).
        assert_eq!(body[21], 0x04);

        // Ciphertext = plaintext + 1 delimiter byte + GCM tag.
        let header_len = 16 + 4 + 1 + idlen;
        let ciphertext_len = body.len() - header_len;
        assert_eq!(ciphertext_len, plaintext.len() + 1 + AES_128_GCM.tag_len());

        // Each call uses a fresh ephemeral key + salt, so bodies differ (nonce reuse would be fatal).
        let body2 = encrypt_payload(SUB_P256DH, SUB_AUTH, plaintext).expect("encrypts");
        assert_ne!(body, body2, "ephemeral keypair + salt must be per-message");
    }

    #[test]
    fn encrypt_payload_rejects_malformed_keys() {
        assert!(encrypt_payload("not base64!!", SUB_AUTH, b"x").is_none());
        assert!(encrypt_payload(&b64url_encode(&[0u8; 10]), SUB_AUTH, b"x").is_none());
    }
}
