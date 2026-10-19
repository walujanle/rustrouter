//! Netlify relay bundle and digest-deploy plumbing.
//!
//! Netlify's API is a digest deploy, not a build deploy:
//!
//! 1. `POST /api/v1/sites {name}` → `{id, ssl_url}` (422 = name taken).
//! 2. `POST /api/v1/sites/{id}/deploys {files, functions}` → `{id, required[],
//!    required_functions[]}`. SHA1 for files, SHA256 for functions.
//! 3. `PUT /api/v1/deploys/{id}/files/{path}` and
//!    `PUT /api/v1/deploys/{id}/functions/{name}?runtime=js`, each only when its
//!    digest appears in the matching `required` list.
//! 4. Poll `GET /api/v1/deploys/{id}` until `state == "ready"`.
//!
//! The relay speaks the same header spec as the Vercel/Cloudflare/Deno relays
//! (`x-relay-target` + `x-relay-path`), so the executor needs no changes.
//! Digest deploys run no build step, so the function is served from its default
//! endpoint: `<site>/.netlify/functions/relay`.

use serde_json::Value;

use crate::error::ApiError;

pub const NETLIFY_API: &str = "https://api.netlify.com/api/v1";
pub const NETLIFY_FUNCTION_NAME: &str = "relay";
pub const NETLIFY_FUNCTION_PATH: &str = "/.netlify/functions/relay";

/// Digest-deployed functions run on the Lambda-compatible runtime, so the
/// bundle must use `exports.handler` with a **string** body. Both alternatives
/// were live-verified to 502: ESM syntax fails with
/// `Runtime.UserCodeSyntaxError`, and returning a stream fails with
/// `cannot-unmarshal-object-into-Go-struct-field`.
///
/// The body is buffered (`arrayBuffer()`) and base64-encoded when binary, which
/// keeps SSE valid but means a stream arrives in one piece. Netlify's
/// synchronous execution limit is 60 s, the same wall as the executor's
/// `FETCH_CONNECT_TIMEOUT_MS`, so a long stream dies at both ends together.
pub const NETLIFY_RELAY_FUNCTION_CODE: &str = r#"exports.handler = async (event) => {
  const headers = {};
  for (const [k, v] of Object.entries(event.headers || {})) {
    headers[k.toLowerCase()] = v;
  }

  const target = headers["x-relay-target"];
  const relayPath = headers["x-relay-path"] || "/";

  if (!target) {
    return {
      statusCode: 400,
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ error: "Missing x-relay-target header" }),
    };
  }

  const targetUrl = target.replace(/\/$/, "") + relayPath;
  const forwardHeaders = { ...event.headers };
  for (const k of Object.keys(forwardHeaders)) {
    const lower = k.toLowerCase();
    if (lower === "x-relay-target" || lower === "x-relay-path" || lower === "host") {
      delete forwardHeaders[k];
    }
  }

  try {
    const upstream = await fetch(targetUrl, {
      method: event.httpMethod,
      headers: forwardHeaders,
      body: event.httpMethod !== "GET" && event.httpMethod !== "HEAD" && event.body
        ? event.isBase64Encoded
          ? Buffer.from(event.body, "base64")
          : event.body
        : undefined,
    });
    // Lambda response body must be a string: returning the stream object
    // fails with cannot-unmarshal-object-into-Go-struct-field (live 502).
    // So buffer here: text for API payloads (JSON/SSE), base64 for binary.
    // Tradeoff vs the other relays: SSE arrives buffered, stays valid SSE.
    const responseHeaders = {};
    upstream.headers.forEach((value, key) => {
      responseHeaders[key] = value;
    });
    const contentType = upstream.headers.get("content-type") || "";
    const isText = new RegExp("^(text/|[^;]*json|[^;]*event-stream|[^;]*javascript|[^;]*xml|[^;]*urlencoded)", "i").test(contentType);
    const rawBody = await upstream.arrayBuffer();
    const responseBody = isText
      ? Buffer.from(rawBody).toString("utf8")
      : Buffer.from(rawBody).toString("base64");
    return {
      statusCode: upstream.status,
      headers: responseHeaders,
      body: responseBody,
      isBase64Encoded: !isText,
    };
  } catch (error) {
    return {
      statusCode: 502,
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ error: error.message || "Relay fetch failed" }),
    };
  }
};
"#;

pub const NETLIFY_INDEX_HTML: &str = "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>9Router Relay</title></head>\n<body><p>9Router relay function lives at <code>/.netlify/functions/relay</code>.</p></body></html>\n";

pub fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// One stored (uncompressed) ZIP entry.
pub struct ZipEntry<'a> {
    pub name: &'a str,
    pub data: &'a [u8],
}

/// Minimal stored ZIP writer. The relay bundle is ~1.5 KB, so a compression
/// dependency would be dead weight; stored entries are plain CRC32 + headers
/// that any unzip tool reads.
pub fn build_stored_zip(entries: &[ZipEntry<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();

    for entry in entries {
        let offset = out.len() as u32;
        let crc = crc32fast::hash(entry.data);
        let size = entry.data.len() as u32;
        let name_len = entry.name.len() as u16;

        // Local file header.
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 filename
        out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0u16.to_le_bytes()); // mod date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes()); // compressed
        out.extend_from_slice(&size.to_le_bytes()); // uncompressed
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra length
        out.extend_from_slice(entry.name.as_bytes());
        out.extend_from_slice(entry.data);

        // Central directory entry.
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // version made by
        central.extend_from_slice(&0u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 filename
        central.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        central.extend_from_slice(&0u16.to_le_bytes()); // mod time
        central.extend_from_slice(&0u16.to_le_bytes()); // mod date
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&name_len.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra length
        central.extend_from_slice(&0u16.to_le_bytes()); // comment length
        central.extend_from_slice(&0u16.to_le_bytes()); // disk number start
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(entry.name.as_bytes());
    }

    let central_offset = out.len() as u32;
    let central_size = central.len() as u32;
    out.extend_from_slice(&central);

    // End of central directory.
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // this disk
    out.extend_from_slice(&0u16.to_le_bytes()); // disk with central dir
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment length
    out
}

/// The zipped single-file bundle Netlify expects for `runtime=js` uploads.
pub fn build_relay_function_zip() -> (Vec<u8>, String) {
    let zip = build_stored_zip(&[ZipEntry {
        name: "relay.js",
        data: NETLIFY_RELAY_FUNCTION_CODE.as_bytes(),
    }]);
    let sha256 = sha256_hex(&zip);
    (zip, sha256)
}

pub fn build_index_file() -> (&'static [u8], String) {
    let content = NETLIFY_INDEX_HTML.as_bytes();
    (content, sha1_hex(content))
}

pub fn build_relay_url(site_url: &str) -> String {
    format!("{}{NETLIFY_FUNCTION_PATH}", site_url.trim_end_matches('/'))
}

/// Netlify site names become subdomains: lowercase alphanumerics and hyphens.
pub fn sanitize_site_name(raw: Option<&str>) -> String {
    let name = raw.map(str::trim).unwrap_or("").to_lowercase();
    if name.is_empty() {
        return default_project_name();
    }
    let mut cleaned = String::with_capacity(name.len());
    let mut last_hyphen = false;
    for ch in name.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            cleaned.push(ch);
            last_hyphen = false;
        } else if !last_hyphen {
            cleaned.push('-');
            last_hyphen = true;
        }
    }
    let cleaned = cleaned.trim_matches('-');
    if cleaned.is_empty() {
        default_project_name()
    } else {
        cleaned.to_string()
    }
}

/// `relay-${Date.now().toString(36)}`.
pub fn default_project_name() -> String {
    let millis = chrono::Utc::now().timestamp_millis().max(0) as u64;
    format!("relay-{}", to_base36(millis))
}

pub fn to_base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// Netlify answers with a top-level `message`; older paths use `error`. This is
/// deliberately not `provider_error`, which reads `error.message`.
pub fn netlify_error_message(body: &Value, fallback: &str) -> String {
    body.get("message")
        .and_then(Value::as_str)
        .or_else(|| body.get("error").and_then(Value::as_str))
        .unwrap_or(fallback)
        .to_string()
}

/// What to do with a `GET /deploys/{id}` response body.
#[derive(Debug, PartialEq, Eq)]
pub enum DeployPoll {
    Ready,
    Failed(String),
    Pending,
}

pub fn deploy_poll_decision(body: &Value) -> DeployPoll {
    match body.get("state").and_then(Value::as_str) {
        Some("ready") => DeployPoll::Ready,
        Some("error") => DeployPoll::Failed(
            body.get("error_message")
                .and_then(Value::as_str)
                .unwrap_or("Netlify deploy failed")
                .to_string(),
        ),
        _ => DeployPoll::Pending,
    }
}

pub fn netlify_headers(token: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Authorization", format!("Bearer {token}")),
        ("User-Agent", "9Router".to_string()),
    ]
}

/// Map an upstream status to the error the dashboard shows.
pub fn netlify_api_error(status: u16, body: &Value, fallback: &str) -> ApiError {
    ApiError::new(
        axum::http::StatusCode::from_u16(status)
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
        netlify_error_message(body, fallback),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_u16(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([bytes[at], bytes[at + 1]])
    }

    fn read_u32(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    #[test]
    fn relay_source_is_lambda_shaped() {
        for needle in [
            "x-relay-target",
            "x-relay-path",
            "exports.handler",
            "arrayBuffer",
            "isBase64Encoded",
        ] {
            assert!(
                NETLIFY_RELAY_FUNCTION_CODE.contains(needle),
                "relay source lost {needle}"
            );
        }
        // ESM syntax is a live 502 on the digest runtime.
        assert!(!NETLIFY_RELAY_FUNCTION_CODE.contains("export const"));
        assert!(!NETLIFY_RELAY_FUNCTION_CODE.contains("export default"));
        // A template literal would be a copy artefact from the JS source.
        assert!(!NETLIFY_RELAY_FUNCTION_CODE.contains("${"));
    }

    /// The JS source shows two backslashes because it sits inside a template
    /// literal. The deployed function needs one, or every relay URL is wrong.
    #[test]
    fn relay_source_has_a_single_escaped_slash() {
        assert!(NETLIFY_RELAY_FUNCTION_CODE.contains(r#"target.replace(/\/$/, "")"#));
        assert!(!NETLIFY_RELAY_FUNCTION_CODE.contains(r#"replace(/\\/$/"#));
    }

    #[test]
    fn digests_are_lowercase_hex_of_the_right_length() {
        let sha1 = sha1_hex(b"abc");
        assert_eq!(sha1, "a9993e364706816aba3e25717850c26c9cd0d89d");
        let sha256 = sha256_hex(b"abc");
        assert_eq!(
            sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn stored_zip_round_trips_headers() {
        let data = b"hello relay";
        let zip = build_stored_zip(&[ZipEntry {
            name: "relay.js",
            data,
        }]);

        assert_eq!(read_u32(&zip, 0), 0x0403_4b50);
        assert_eq!(read_u16(&zip, 4), 20);
        assert_eq!(read_u16(&zip, 6), 0x0800);
        assert_eq!(read_u16(&zip, 8), 0); // stored
        assert_eq!(read_u32(&zip, 14), crc32fast::hash(data));
        assert_eq!(read_u32(&zip, 18), data.len() as u32);
        assert_eq!(read_u32(&zip, 22), data.len() as u32);
        assert_eq!(read_u16(&zip, 26), 8);
        assert_eq!(&zip[30..38], b"relay.js");
        assert_eq!(&zip[38..38 + data.len()], data);

        let central_at = 30 + 8 + data.len();
        assert_eq!(read_u32(&zip, central_at), 0x0201_4b50);
        assert_eq!(read_u32(&zip, central_at + 16), crc32fast::hash(data));
        assert_eq!(read_u32(&zip, central_at + 42), 0); // local header offset
        assert_eq!(&zip[central_at + 46..central_at + 54], b"relay.js");

        let eocd_at = central_at + 46 + 8;
        assert_eq!(read_u32(&zip, eocd_at), 0x0605_4b50);
        assert_eq!(read_u16(&zip, eocd_at + 8), 1);
        assert_eq!(read_u16(&zip, eocd_at + 10), 1);
        assert_eq!(read_u32(&zip, eocd_at + 12), 46 + 8);
        assert_eq!(read_u32(&zip, eocd_at + 16), central_at as u32);
    }

    #[test]
    fn relay_bundle_digest_matches_the_zip_it_ships() {
        let (zip, sha256) = build_relay_function_zip();
        assert_eq!(sha256, sha256_hex(&zip));
        assert_eq!(sha256.len(), 64);
        let (_, sha1) = build_index_file();
        assert_eq!(sha1.len(), 40);
    }

    #[test]
    fn site_names_are_dns_safe() {
        assert_eq!(sanitize_site_name(Some("My Relay")), "my-relay");
        assert_eq!(sanitize_site_name(Some("--relay--")), "relay");
        assert_eq!(sanitize_site_name(Some("relay__two")), "relay-two");
        assert!(sanitize_site_name(Some("")).starts_with("relay-"));
        assert!(sanitize_site_name(None).starts_with("relay-"));
    }

    #[test]
    fn relay_url_has_one_separator() {
        assert_eq!(
            build_relay_url("https://x.netlify.app/"),
            "https://x.netlify.app/.netlify/functions/relay"
        );
        assert_eq!(
            build_relay_url("https://x.netlify.app"),
            "https://x.netlify.app/.netlify/functions/relay"
        );
    }

    #[test]
    fn poll_decisions_cover_every_state() {
        assert_eq!(
            deploy_poll_decision(&serde_json::json!({"state": "ready"})),
            DeployPoll::Ready
        );
        assert_eq!(
            deploy_poll_decision(&serde_json::json!({"state": "error", "error_message": "boom"})),
            DeployPoll::Failed("boom".to_string())
        );
        assert_eq!(
            deploy_poll_decision(&serde_json::json!({"state": "error"})),
            DeployPoll::Failed("Netlify deploy failed".to_string())
        );
        assert_eq!(
            deploy_poll_decision(&serde_json::json!({"state": "building"})),
            DeployPoll::Pending
        );
        assert_eq!(
            deploy_poll_decision(&serde_json::json!({})),
            DeployPoll::Pending
        );
    }

    #[test]
    fn error_message_prefers_message_then_error() {
        assert_eq!(
            netlify_error_message(&serde_json::json!({"message": "m", "error": "e"}), "f"),
            "m"
        );
        assert_eq!(
            netlify_error_message(&serde_json::json!({"error": "e"}), "f"),
            "e"
        );
        assert_eq!(
            netlify_error_message(&serde_json::json!({"error": {"message": "e"}}), "f"),
            "f"
        );
        assert_eq!(netlify_error_message(&serde_json::json!({}), "f"), "f");
    }

    #[test]
    fn base36_matches_js_number_tostring() {
        assert_eq!(to_base36(0), "0");
        assert_eq!(to_base36(35), "z");
        assert_eq!(to_base36(36), "10");
    }
}
