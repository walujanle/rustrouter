//! HS256 JWT, byte-compatible with the `jose` library used for signing.
//!
//! `jsonwebtoken` 11 makes the crypto backend a feature flag and neither
//! `aws_lc_rs` (needs a C toolchain) nor `rust_crypto` (pulls rsa, p256, p384
//! and ed25519 to compute one HMAC) fits a 24-hour cookie. HS256 is HMAC-SHA256
//! over the base64url signing input; that is the whole algorithm.
//!
//! Byte compatibility is the point, not a nicety: a token minted here has to
//! verify against the same secret elsewhere and vice versa. So the header is
//! exactly `{"alg":"HS256"}` (`jose` emits no `typ`), the payload is serialized
//! in insertion order, base64url is unpadded, and `exp` is rejected at
//! `exp <= now` with no clock tolerance (`jose`'s default).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// The exact protected header `jose` writes for `setProtectedHeader({ alg: "HS256" })`.
const HEADER_JSON: &str = r#"{"alg":"HS256"}"#;

/// A fixed `jose` token for the test: HS256, key `secret`, payload
/// `{"authenticated":true,"iat":1700000000,"exp":4102444800}`. Cross-checked
/// with `openssl dgst -sha256 -hmac secret` over the signing input.
#[cfg(test)]
const JOSE_TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.eyJhdXRoZW50aWNhdGVkIjp0cnVlLCJpYXQiOjE3MDAwMDAwMDAsImV4cCI6NDEwMjQ0NDgwMH0.ZicbIJqaC2NIDwFmvqUKLF0UyEO9yWqKlw_H1AraOso";

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Sign `payload` into a compact JWS. The payload must be a JSON object; the
/// caller owns its key order.
pub fn encode(payload: &Value, secret: &[u8]) -> String {
    let header = URL_SAFE_NO_PAD.encode(HEADER_JSON);
    let body = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(payload).expect("a serde_json::Value always serializes"));
    let signing_input = format!("{header}.{body}");
    let signature = URL_SAFE_NO_PAD.encode(hmac_sha256(secret, signing_input.as_bytes()));
    format!("{signing_input}.{signature}")
}

/// Verify and decode a compact JWS.
///
/// Returns `None` for a malformed token, a signature that does not match, an
/// `alg` other than HS256, an `exp` at or before now, or an `nbf` in the
/// future. `iat` is not validated — `jose` does not reject a future `iat`
/// either.
pub fn decode(token: &str, secret: &[u8]) -> Option<Value> {
    let mut parts = token.split('.');
    let header_b64 = parts.next()?;
    let payload_b64 = parts.next()?;
    let signature_b64 = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    // Pin the algorithm before touching the key: this is what stops an
    // `alg: "none"` or `alg: "HS512"` token from being accepted.
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header_b64).ok()?).ok()?;
    if header.get("alg").and_then(Value::as_str) != Some("HS256") {
        return None;
    }

    let signature = URL_SAFE_NO_PAD.decode(signature_b64).ok()?;
    let signing_input = format!("{header_b64}.{payload_b64}");
    let expected = hmac_sha256(secret, signing_input.as_bytes());
    if expected.as_slice().ct_eq(signature.as_slice()).unwrap_u8() != 1 {
        return None;
    }

    let payload: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload_b64).ok()?).ok()?;
    let now = now_secs();
    if payload
        .get("exp")
        .and_then(Value::as_i64)
        .is_some_and(|exp| exp <= now)
    {
        return None;
    }
    if payload
        .get("nbf")
        .and_then(Value::as_i64)
        .is_some_and(|nbf| nbf > now)
    {
        return None;
    }
    Some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn header_is_exactly_alg_hs256() {
        let token = encode(&json!({"a": 1}), b"k");
        let header_b64 = token.split('.').next().unwrap();
        let raw = URL_SAFE_NO_PAD.decode(header_b64).unwrap();
        assert_eq!(String::from_utf8(raw).unwrap(), r#"{"alg":"HS256"}"#);
        assert!(!token.contains('='), "base64url must be unpadded");
    }

    #[test]
    fn matches_a_known_jose_token() {
        let payload = json!({"authenticated": true, "iat": 1_700_000_000, "exp": 4_102_444_800i64});
        assert_eq!(encode(&payload, b"secret"), JOSE_TOKEN);
        assert!(decode(JOSE_TOKEN, b"secret").is_some());
        assert!(decode(JOSE_TOKEN, b"wrong").is_none());
    }

    #[test]
    fn payload_key_order_survives_the_round_trip() {
        let payload = json!({"authenticated": true, "iat": 1, "exp": now_secs() + 60});
        let decoded = decode(&encode(&payload, b"k"), b"k").unwrap();
        let keys: Vec<&String> = decoded.as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["authenticated", "iat", "exp"]);
    }

    #[test]
    fn signature_binds_the_secret() {
        let token = encode(&json!({"authenticated": true}), b"secret-a");
        assert!(decode(&token, b"secret-a").is_some());
        assert!(decode(&token, b"secret-b").is_none());
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        assert!(decode("", b"k").is_none());
        assert!(decode("a.b", b"k").is_none());
        assert!(decode("a.b.c.d", b"k").is_none());
        assert!(decode("not-a-jwt", b"k").is_none());
        assert!(decode("!!!.!!!.!!!", b"k").is_none());
    }

    #[test]
    fn a_tampered_payload_fails_the_signature() {
        let token = encode(&json!({"role": "user"}), b"k");
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged = URL_SAFE_NO_PAD.encode(br#"{"role":"admin"}"#);
        parts[1] = &forged;
        let tampered = parts.join(".");
        assert!(decode(&tampered, b"k").is_none());
    }

    #[test]
    fn exp_at_or_before_now_is_rejected() {
        let now = now_secs();
        assert!(decode(&encode(&json!({"exp": now}), b"k"), b"k").is_none());
        assert!(decode(&encode(&json!({"exp": now - 1}), b"k"), b"k").is_none());
        assert!(decode(&encode(&json!({"exp": now + 60}), b"k"), b"k").is_some());
        // No exp at all is valid, matching jose.
        assert!(decode(&encode(&json!({"authenticated": true}), b"k"), b"k").is_some());
    }

    #[test]
    fn future_nbf_is_rejected() {
        let now = now_secs();
        assert!(decode(&encode(&json!({"nbf": now + 60}), b"k"), b"k").is_none());
        assert!(decode(&encode(&json!({"nbf": now - 60}), b"k"), b"k").is_some());
    }

    #[test]
    fn non_hs256_alg_is_refused() {
        // Forge `alg: "none"` with an empty signature.
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let body = URL_SAFE_NO_PAD.encode(r#"{"authenticated":true}"#);
        assert!(decode(&format!("{header}.{body}."), b"k").is_none());
    }
}
