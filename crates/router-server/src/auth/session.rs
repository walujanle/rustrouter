//! Dashboard session: the `auth_token` JWT cookie and the bcrypt password
//! check.
//!
//! Signing goes through `super::jwt`, a hand-rolled HS256 that matches `jose`
//! byte for byte — see that module for why `jsonwebtoken` was not used.

use serde_json::{Map, Value, json};

use router_db::paths::Paths;

use super::jwt;

/// `DEFAULT_PASSWORD`.
pub const DEFAULT_PASSWORD: &str = "123456";
/// `SESSION_MAX_AGE_SEC`.
pub const SESSION_MAX_AGE_SEC: i64 = 24 * 60 * 60;
/// The cookie name, `auth_token`.
pub const COOKIE_NAME: &str = "auth_token";
/// bcrypt cost, matching `bcrypt.genSalt(10)`.
pub const BCRYPT_COST: u32 = 10;

/// The resolved signing secret, held once per process.
#[derive(Clone)]
pub struct Session {
    secret: Vec<u8>,
}

impl Session {
    pub fn load(paths: &Paths) -> router_db::DbResult<Self> {
        let secret = router_db::identity::jwt_secret(paths)?;
        Ok(Self::from_secret(secret.as_bytes()))
    }

    pub fn from_secret(secret: &[u8]) -> Self {
        Self {
            secret: secret.to_vec(),
        }
    }

    /// Caller claims land after `authenticated`, then `iat` and `exp` are
    /// appended, so that is the payload key order.
    pub fn create_token(&self, claims: Map<String, Value>) -> String {
        let mut payload = Map::new();
        payload.insert("authenticated".into(), json!(true));
        for (k, v) in claims {
            payload.insert(k, v);
        }
        let iat = chrono::Utc::now().timestamp();
        payload.insert("iat".into(), json!(iat));
        payload.insert("exp".into(), json!(iat + SESSION_MAX_AGE_SEC));
        jwt::encode(&Value::Object(payload), &self.secret)
    }

    /// The claims, or `None` when the token is missing, malformed, expired, or
    /// not a JSON object.
    pub fn get_session(&self, token: Option<&str>) -> Option<Map<String, Value>> {
        let token = token?;
        if token.is_empty() {
            return None;
        }
        match jwt::decode(token, &self.secret)? {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    pub fn verify_token(&self, token: Option<&str>) -> bool {
        self.get_session(token).is_some()
    }
}

/// Forced by env, else the request arrived over HTTPS according to the
/// forwarding proxy.
pub fn should_use_secure_cookie(forwarded_proto: Option<&str>) -> bool {
    let forced = std::env::var("AUTH_COOKIE_SECURE").is_ok_and(|v| v == "true");
    forced || forwarded_proto == Some("https")
}

/// The `Set-Cookie` value for setting the session cookie.
pub fn set_cookie_header(token: &str, secure: bool) -> String {
    let mut v = format!(
        "{COOKIE_NAME}={token}; Path=/; Max-Age={SESSION_MAX_AGE_SEC}; HttpOnly; SameSite=Lax"
    );
    if secure {
        v.push_str("; Secure");
    }
    v
}

/// The `Set-Cookie` value for clearing the session cookie.
pub fn clear_cookie_header() -> String {
    format!("{COOKIE_NAME}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax")
}

/// bcrypt against the stored hash, or a literal compare against
/// `INITIAL_PASSWORD` while no hash is stored.
///
/// An empty password is rejected before either branch.
pub fn verify_dashboard_password(stored_hash: Option<&str>, password: &str) -> bool {
    if password.is_empty() {
        return false;
    }
    match stored_hash.filter(|h| !h.is_empty()) {
        Some(hash) => bcrypt::verify(password, hash).unwrap_or(false),
        None => password == initial_password(),
    }
}

/// `INITIAL_PASSWORD` if set, else the default.
pub fn initial_password() -> String {
    std::env::var("INITIAL_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_PASSWORD.to_string())
}

/// Hash a new password for storage.
pub fn hash_password(password: &str) -> Result<String, bcrypt::BcryptError> {
    bcrypt::hash(password, BCRYPT_COST)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trips_and_carries_the_reference_claims() {
        let s = Session::from_secret(b"secret");
        let token = s.create_token(Map::new());
        let claims = s.get_session(Some(&token)).unwrap();
        assert_eq!(claims["authenticated"], json!(true));
        assert!(claims["iat"].is_i64());
        assert_eq!(
            claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
            SESSION_MAX_AGE_SEC
        );
        // `authenticated, iat, exp` — the order jose writes.
        let keys: Vec<&String> = claims.keys().collect();
        assert_eq!(keys, vec!["authenticated", "iat", "exp"]);
    }

    #[test]
    fn caller_claims_land_before_iat() {
        let s = Session::from_secret(b"secret");
        let mut claims = Map::new();
        claims.insert("oidcName".into(), json!("Ada"));
        let token = s.create_token(claims);
        let decoded = s.get_session(Some(&token)).unwrap();
        let keys: Vec<&String> = decoded.keys().collect();
        assert_eq!(keys, vec!["authenticated", "oidcName", "iat", "exp"]);
    }

    #[test]
    fn header_has_no_typ_field() {
        let s = Session::from_secret(b"secret");
        let token = s.create_token(Map::new());
        let header_b64 = token.split('.').next().unwrap();
        use base64::Engine;
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(header_b64)
            .unwrap();
        assert_eq!(String::from_utf8(raw).unwrap(), r#"{"alg":"HS256"}"#);
    }

    #[test]
    fn wrong_secret_and_garbage_are_rejected() {
        let a = Session::from_secret(b"secret-a");
        let b = Session::from_secret(b"secret-b");
        let token = a.create_token(Map::new());
        assert!(a.verify_token(Some(&token)));
        assert!(!b.verify_token(Some(&token)));
        assert!(!a.verify_token(Some("not-a-jwt")));
        assert!(!a.verify_token(Some("")));
        assert!(!a.verify_token(None));
    }

    #[test]
    fn expired_token_is_rejected_with_no_leeway() {
        let s = Session::from_secret(b"secret");
        let mut payload = Map::new();
        payload.insert("authenticated".into(), json!(true));
        let past = chrono::Utc::now().timestamp() - 1;
        payload.insert("iat".into(), json!(past - 10));
        payload.insert("exp".into(), json!(past));
        let token = jwt::encode(&Value::Object(payload), b"secret");
        assert!(!s.verify_token(Some(&token)), "1s past exp must fail");
    }

    #[test]
    fn non_object_payload_is_not_a_session() {
        let s = Session::from_secret(b"secret");
        let token = jwt::encode(&json!([1, 2, 3]), b"secret");
        assert!(s.get_session(Some(&token)).is_none());
    }

    #[test]
    fn password_uses_initial_password_until_a_hash_exists() {
        // No env set in the test process: the literal default applies.
        assert!(verify_dashboard_password(None, DEFAULT_PASSWORD));
        assert!(!verify_dashboard_password(None, "wrong"));
        assert!(!verify_dashboard_password(None, ""));
        // An empty stored hash falls through to the initial-password compare
        // rather than rejecting.
        assert!(verify_dashboard_password(Some(""), "123456"));
        assert!(!verify_dashboard_password(Some(""), "wrong"));
    }

    #[test]
    fn password_uses_bcrypt_when_a_hash_exists() {
        let hash = hash_password("hunter2").unwrap();
        assert!(verify_dashboard_password(Some(&hash), "hunter2"));
        assert!(!verify_dashboard_password(Some(&hash), "123456"));
        // A malformed hash must read as "no", not as an error.
        assert!(!verify_dashboard_password(Some("$2b$10$nope"), "hunter2"));
    }

    #[test]
    fn cookie_headers_match_the_reference_attributes() {
        let insecure = set_cookie_header("tok", false);
        assert_eq!(
            insecure,
            "auth_token=tok; Path=/; Max-Age=86400; HttpOnly; SameSite=Lax"
        );
        assert!(set_cookie_header("tok", true).ends_with("; Secure"));
        assert!(clear_cookie_header().starts_with("auth_token=; Path=/; Max-Age=0"));
    }
}
