//! Runtime constants, timeouts and the media-fetch limits.
//!
//! Values are code constants, not read from the environment: the binary owns
//! its own configuration surface, so there are no env-var overrides. If one is
//! wanted later it belongs in the server's config layer, not scattered through
//! the engine.

/// `HTTP_STATUS`.
pub mod http_status {
    pub const BAD_REQUEST: u16 = 400;
    pub const UNAUTHORIZED: u16 = 401;
    pub const PAYMENT_REQUIRED: u16 = 402;
    pub const FORBIDDEN: u16 = 403;
    pub const NOT_FOUND: u16 = 404;
    pub const NOT_ACCEPTABLE: u16 = 406;
    pub const REQUEST_TIMEOUT: u16 = 408;
    pub const RATE_LIMITED: u16 = 429;
    pub const SERVER_ERROR: u16 = 500;
    pub const BAD_GATEWAY: u16 = 502;
    pub const SERVICE_UNAVAILABLE: u16 = 503;
    pub const GATEWAY_TIMEOUT: u16 = 504;
}

/// `CACHE_TTL`, in seconds.
pub mod cache_ttl {
    pub const USER_INFO: u64 = 300;
    pub const MODEL_ALIAS: u64 = 3600;
}

/// `MEMORY_CONFIG`, in milliseconds.
pub mod memory_config {
    pub const SESSION_TTL_MS: i64 = 2 * 60 * 60 * 1000;
    pub const SESSION_CLEANUP_INTERVAL_MS: i64 = 30 * 60 * 1000;
    pub const DNS_CACHE_TTL_MS: i64 = 5 * 60 * 1000;
    pub const PROXY_DISPATCHERS_MAX_SIZE: usize = 20;
}

/// `STREAM_STALL_TIMEOUT_MS`: inter-chunk stall once tokens flow.
pub const STREAM_STALL_TIMEOUT_MS: u64 = 360 * 1000;
/// `STREAM_FIRST_CHUNK_TIMEOUT_MS`: time to first token.
pub const STREAM_FIRST_CHUNK_TIMEOUT_MS: u64 = 200 * 1000;
/// Idle interval after which a keepalive frame is written to the client while
/// the upstream is silent. The client-side SSE idle budget is far shorter than
/// [`STREAM_STALL_TIMEOUT_MS`] — Claude Code aborts a stream that goes ~10s
/// without bytes — so a slow first token or a long reasoning pause must not
/// leave the connection byte-silent.
pub const SSE_KEEPALIVE_INTERVAL_MS: u64 = 5 * 1000;
/// `FETCH_CONNECT_TIMEOUT_MS`.
pub const FETCH_CONNECT_TIMEOUT_MS: u64 = 60 * 1000;

/// `DEFAULT_MAX_TOKENS`.
pub const DEFAULT_MAX_TOKENS: i64 = 64000;
/// `DEFAULT_MIN_TOKENS`.
pub const DEFAULT_MIN_TOKENS: i64 = 32000;

/// `TOKEN_SAVER_HEADER`.
pub const TOKEN_SAVER_HEADER: &str = "x-9router-token-saver";

/// `RETRY_CONFIG.delayMs` — the legacy default delay.
pub const RETRY_DELAY_MS: u64 = 2000;

/// `DEFAULT_RETRY_CONFIG[status]` → `(attempts, delayMs)`.
pub fn default_retry_for_status(status: u16) -> (u32, u64) {
    match status {
        429 => (0, 0),
        502 => (3, 3000),
        503 => (3, 2000),
        504 => (2, 3000),
        _ => (0, RETRY_DELAY_MS),
    }
}

/// `SKIP_PATTERNS`: a request containing any of these bypasses the provider.
pub const SKIP_PATTERNS: [&str; 1] =
    ["Please write a 5-10 word title for the following conversation:"];

/// `MAX_IMAGE_BYTES`: cap a remote image fetch to prevent memory DoS.
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
/// `FETCH_TIMEOUT_MS` for remote media.
pub const MEDIA_FETCH_TIMEOUT_MS: u64 = 10_000;

/// `BLOCKED_HOSTS`: never fetched (loopback + cloud metadata).
pub const BLOCKED_HOSTS: [&str; 6] = [
    "localhost",
    "127.0.0.1",
    "0.0.0.0",
    "::1",
    "169.254.169.254",
    "metadata.google.internal",
];

/// One `IMAGE_SIGNATURES` entry: magic bytes at `offset` → mime.
pub struct ImageSignature {
    pub sig: &'static [u8],
    pub offset: usize,
    pub mime: &'static str,
    /// WEBP: bytes 8..11 must read `WEBP`, beyond the `RIFF` prefix.
    pub verify_webp: bool,
}

/// `IMAGE_SIGNATURES`, in match order.
pub const IMAGE_SIGNATURES: [ImageSignature; 5] = [
    ImageSignature {
        sig: &[0x89, 0x50, 0x4e, 0x47],
        offset: 0,
        mime: "image/png",
        verify_webp: false,
    },
    ImageSignature {
        sig: &[0xff, 0xd8, 0xff],
        offset: 0,
        mime: "image/jpeg",
        verify_webp: false,
    },
    ImageSignature {
        sig: &[0x47, 0x49, 0x46, 0x38],
        offset: 0,
        mime: "image/gif",
        verify_webp: false,
    },
    ImageSignature {
        sig: &[0x52, 0x49, 0x46, 0x46],
        offset: 0,
        mime: "image/webp",
        verify_webp: true,
    },
    ImageSignature {
        sig: &[0x42, 0x4d],
        offset: 0,
        mime: "image/bmp",
        verify_webp: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_table_matches_the_expected() {
        assert_eq!(default_retry_for_status(429), (0, 0));
        assert_eq!(default_retry_for_status(502), (3, 3000));
        assert_eq!(default_retry_for_status(503), (3, 2000));
        assert_eq!(default_retry_for_status(504), (2, 3000));
        // Unknown statuses carry no attempts but the legacy delay.
        assert_eq!(default_retry_for_status(500), (0, RETRY_DELAY_MS));
    }

    #[test]
    fn image_signatures_are_in_match_order() {
        // PNG before JPEG before GIF before WEBP before BMP.
        assert_eq!(IMAGE_SIGNATURES[0].mime, "image/png");
        assert!(IMAGE_SIGNATURES[3].verify_webp);
    }
}
