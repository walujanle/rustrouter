//! Claude thinking-signature validation.
//!
//! Two encodings exist:
//!
//! - **E-form**: single-layer base64, decoded byte 0 is the Claude marker `0x12`.
//! - **R-form**: double-layer base64, outer decoded byte 0 is `'E'` (0x45), and
//!   the inner base64 of that decoded text starts with `0x12`.
//!
//! A `"…#sig"` cache prefix is stripped before either check.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

const MAX_CLAUDE_SIGNATURE_LEN: usize = 32 * 1024 * 1024;
const CLAUDE_SIGNATURE_MARKER: u8 = 0x12;

/// `stripCachePrefix(rawSignature)`.
fn strip_cache_prefix(raw: Option<&str>) -> &str {
    let sig = raw.unwrap_or("").trim();
    if sig.is_empty() {
        return "";
    }
    match sig.find('#') {
        Some(idx) => sig[idx + 1..].trim(),
        None => sig,
    }
}

/// `Buffer.from(s, "base64")` is lenient: it skips characters outside the
/// alphabet and ignores a bad padding tail. `STANDARD.decode` is strict, so
/// strip to the alphabet and re-pad before decoding.
fn lenient_base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut filtered: Vec<u8> = Vec::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'+' || b == b'/' {
            filtered.push(b);
        }
    }
    // A trailing group of 1 base64 char cannot decode; drop it like Node does.
    let mut rem = filtered.len() % 4;
    if rem == 1 {
        filtered.pop();
        rem = 0;
    }
    if rem > 0 {
        filtered.extend(std::iter::repeat_n(b'=', 4 - rem));
    }
    STANDARD.decode(&filtered).ok()
}

/// `hasClaudeSignaturePrefix(rawSignature)`.
pub fn has_claude_signature_prefix(raw_signature: Option<&str>) -> bool {
    let sig = strip_cache_prefix(raw_signature);
    matches!(sig.as_bytes().first(), Some(b'E') | Some(b'R'))
}

/// `isValidClaudeSignature(rawSignature)`.
pub fn is_valid_claude_signature(raw_signature: Option<&str>) -> bool {
    let sig = strip_cache_prefix(raw_signature);
    if sig.is_empty() || sig.len() > MAX_CLAUDE_SIGNATURE_LEN {
        return false;
    }

    match sig.as_bytes()[0] {
        b'E' => {
            lenient_base64_decode(sig).is_some_and(|d| d.first() == Some(&CLAUDE_SIGNATURE_MARKER))
        }
        b'R' => {
            let Some(outer) = lenient_base64_decode(sig) else {
                return false;
            };
            if outer.first() != Some(&0x45) {
                return false;
            }
            // Node decodes `outer.toString()` — the UTF-8 text of the outer bytes.
            let outer_text = String::from_utf8_lossy(&outer);
            lenient_base64_decode(&outer_text)
                .is_some_and(|inner| inner.first() == Some(&CLAUDE_SIGNATURE_MARKER))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as B64;

    #[test]
    fn the_cache_prefix_is_stripped_before_validation() {
        let e_form = B64.encode([0x12u8, 0x01, 0x02]);
        assert!(is_valid_claude_signature(Some(&e_form)));
        assert!(is_valid_claude_signature(Some(&format!("prefix#{e_form}"))));
        assert!(!has_claude_signature_prefix(Some("prefix#Zzz")));
    }

    #[test]
    fn an_e_form_signature_needs_the_marker_byte() {
        let good = B64.encode([0x12u8, 0xAA]);
        let bad = B64.encode([0x13u8, 0xAA]);
        assert!(is_valid_claude_signature(Some(&good)));
        assert!(!is_valid_claude_signature(Some(&bad)));
    }

    #[test]
    fn an_r_form_signature_is_double_encoded() {
        let inner = B64.encode([0x12u8, 0x07]);
        // The outer layer base64s the *text* of the inner layer, so the outer
        // decode starts with 'E' and the second decode lands on the marker.
        let outer = B64.encode(inner.as_bytes());
        assert!(is_valid_claude_signature(Some(&outer)));
        // Outer text not starting with 'E' fails the inner layer.
        let outer = B64.encode(b"Xnotbase64");
        assert!(!is_valid_claude_signature(Some(&outer)));
    }

    #[test]
    fn garbage_and_empty_are_rejected() {
        assert!(!is_valid_claude_signature(None));
        assert!(!is_valid_claude_signature(Some("")));
        assert!(!is_valid_claude_signature(Some("Zzz")));
        assert!(!has_claude_signature_prefix(Some("")));
    }
}
