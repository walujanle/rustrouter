//! Claude anti-ban cloaking.
//!
//! One transform, gated on an OAuth token (`sk-ant-oat…`): `apply_cloaking`
//! injects the Claude Code billing header as `system[0]` and a deterministic
//! fake `metadata.user_id`, so an `anthropic-compatible-*` upstream sees a
//! native Claude Code client.
//!
//! `generateBillingHeader` hashes `JSON.stringify(body)`, so the body must be a
//! `preserve_order` `Value` (it is) and the serialization must match JS's.

use rand::Rng;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// `CLAUDE_CLI_VERSION`.
pub const CLAUDE_CLI_VERSION: &str = "2.1.280";
/// `CC_ENTRYPOINT`.
const CC_ENTRYPOINT: &str = "sdk-cli";

fn hex_sha256(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}

/// `randomBytes(2).toString("hex")`.
fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// `generateBillingHeader(payload)`.
fn generate_billing_header(payload: &Value) -> String {
    let content = serde_json::to_string(payload).unwrap_or_default();
    let cch = &hex_sha256(&content)[..5];
    let build_hash = &random_hex(2)[..3];
    format!(
        "x-anthropic-billing-header: cc_version={CLAUDE_CLI_VERSION}.{build_hash}; cc_entrypoint={CC_ENTRYPOINT}; cch={cch};"
    )
}

/// `deriveUuid(seed)` — a deterministic UUID-v4-shaped string.
fn derive_uuid(seed: &str) -> String {
    let h = hex_sha256(seed);
    let variant = (u8::from_str_radix(&h[16..17], 16).unwrap_or(0) & 0x3) | 0x8;
    format!(
        "{}-{}-4{}-{:x}{}-{}",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        variant,
        &h[17..20],
        &h[20..32]
    )
}

/// `generateFakeUserID(sessionId, apiKey)` — a JSON string.
fn generate_fake_user_id(session_id: Option<&str>, api_key: &str) -> String {
    let device_id = hex_sha256(&format!("device:{api_key}"));
    let account_uuid = derive_uuid(&format!("account:{api_key}"));
    let session_uuid = session_id
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    format!(
        "{{\"device_id\":\"{device_id}\",\"account_uuid\":\"{account_uuid}\",\"session_id\":\"{session_uuid}\"}}"
    )
}

/// `applyCloaking(body, apiKey, sessionId)`, in place. A no-op unless the key is
/// an OAuth token.
pub fn apply_cloaking(body: &mut Value, api_key: Option<&str>, session_id: Option<&str>) {
    let Some(api_key) = api_key.filter(|k| k.contains("sk-ant-oat")) else {
        return;
    };

    let billing_text = generate_billing_header(body);
    let billing_block = json!({"type": "text", "text": billing_text});

    match body.get("system") {
        Some(Value::Array(system)) => {
            let already = system
                .first()
                .and_then(|b| b.get("text"))
                .and_then(Value::as_str)
                .is_some_and(|t| t.starts_with("x-anthropic-billing-header:"));
            if !already {
                let mut next = vec![billing_block];
                next.extend(system.iter().cloned());
                body["system"] = Value::Array(next);
            }
        }
        Some(Value::String(existing)) => {
            let existing = existing.clone();
            body["system"] = json!([billing_block, {"type": "text", "text": existing}]);
        }
        _ => {
            body["system"] = Value::Array(vec![billing_block]);
        }
    }

    let has_user_id = body
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .is_some_and(|v| !v.is_null() && v.as_str() != Some(""));
    if !has_user_id {
        let user_id = generate_fake_user_id(session_id, api_key);
        let mut metadata = body
            .get("metadata")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        metadata.insert("user_id".into(), Value::String(user_id));
        body["metadata"] = Value::Object(metadata);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cloaking_is_a_noop_without_an_oauth_token() {
        let mut body = json!({"system": "be brief", "metadata": {}});
        let before = body.clone();
        apply_cloaking(&mut body, Some("sk-regular-key"), Some("sid"));
        assert_eq!(body, before);
        apply_cloaking(&mut body, None, None);
        assert_eq!(body, before);
    }

    #[test]
    fn cloaking_injects_the_billing_header_first() {
        let mut body = json!({"system": [{"type": "text", "text": "existing"}]});
        apply_cloaking(&mut body, Some("sk-ant-oat-1"), Some("sid"));
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        let header = system[0]["text"].as_str().unwrap();
        assert!(header.starts_with("x-anthropic-billing-header: cc_version=2.1.280."));
        assert!(header.contains("cc_entrypoint=sdk-cli;"));
        assert!(header.ends_with(";"));
        assert_eq!(system[1]["text"], json!("existing"));
    }

    #[test]
    fn cloaking_wraps_a_string_system_and_does_not_double_inject() {
        let mut body = json!({"system": "be brief"});
        apply_cloaking(&mut body, Some("sk-ant-oat-1"), None);
        assert_eq!(body["system"].as_array().unwrap().len(), 2);
        assert_eq!(body["system"][1]["text"], json!("be brief"));

        apply_cloaking(&mut body, Some("sk-ant-oat-1"), None);
        assert_eq!(
            body["system"].as_array().unwrap().len(),
            2,
            "second call is idempotent"
        );
    }

    #[test]
    fn fake_user_id_is_deterministic_per_key_but_session_varies() {
        let mut body = json!({});
        apply_cloaking(&mut body, Some("sk-ant-oat-1"), Some("sid-1"));
        let uid = body["metadata"]["user_id"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(uid).unwrap();
        assert_eq!(parsed["session_id"], json!("sid-1"));
        assert_eq!(parsed["device_id"].as_str().unwrap().len(), 64);

        let mut body2 = json!({});
        apply_cloaking(&mut body2, Some("sk-ant-oat-1"), Some("sid-2"));
        let uid2: Value =
            serde_json::from_str(body2["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(
            uid2["device_id"], parsed["device_id"],
            "device id is per key"
        );
        assert_ne!(uid2["session_id"], parsed["session_id"]);
    }

    #[test]
    fn derive_uuid_is_deterministic_and_shaped_like_v4() {
        let a = derive_uuid("account:sk-ant-oat-1");
        let b = derive_uuid("account:sk-ant-oat-1");
        assert_eq!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        let variant = u8::from_str_radix(&a[19..20], 16).unwrap();
        assert!(variant & 0x8 == 0x8, "variant nibble is 8-b");
    }
}
