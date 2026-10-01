//! The chat pipeline's logger.
//!
//! A plain console logger with colored-dot tags so every line of one CLI
//! conversation shares a tag. Here that is a `tracing` adapter, so the same
//! correlation still shows up in the server log.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::executors::executor::ExecutorLog;
use crate::handlers::chat_core::ChatLog;

/// `REQ_TAGS`.
const REQ_TAGS: [&str; 8] = ["🟢", "🔵", "🟣", "🟡", "🟠", "🔴", "⚪", "🟤"];

static TAG_CURSOR: AtomicUsize = AtomicUsize::new(0);

/// `nextTag()`: the rotating tag used when a request has no session seed.
pub fn next_tag() -> &'static str {
    let index = TAG_CURSOR.fetch_add(1, Ordering::Relaxed) % REQ_TAGS.len();
    REQ_TAGS[index]
}

/// `tagForSession(seed)`: a stable tag for one session, so every line of a
/// conversation correlates without threading a counter through.
pub fn tag_for_session(seed: &str) -> &'static str {
    if seed.is_empty() {
        return next_tag();
    }
    // JS `h = (h * 31 + c) | 0` — a wrapping 32-bit signed accumulator.
    let mut h: i32 = 0;
    for c in seed.chars() {
        h = h.wrapping_mul(31).wrapping_add(c as i32);
    }
    let index = (h.unsigned_abs() as usize) % REQ_TAGS.len();
    REQ_TAGS[index]
}

/// `maskKey(key)`: `abcd...wxyz`, or `***` for anything shorter than 8.
pub fn mask_key(key: &str) -> String {
    if key.chars().count() < 8 {
        return "***".to_string();
    }
    let head: String = key.chars().take(4).collect();
    let tail: String = key.chars().skip(key.chars().count() - 4).collect();
    format!("{head}...{tail}")
}

/// `fmtThink(intent)`: the short thinking label for the request line.
pub fn fmt_think(intent: Option<&crate::thinking::ThinkingMode>) -> Option<String> {
    use crate::thinking::ThinkingMode;
    match intent? {
        ThinkingMode::None => Some("off".to_string()),
        ThinkingMode::Auto => Some("auto".to_string()),
        ThinkingMode::Budget(budget) => Some(if *budget >= 1000.0 {
            format!("{}k", (*budget / 1000.0).round() as i64)
        } else {
            (*budget as i64).to_string()
        }),
        ThinkingMode::Level(level) => Some(level.clone()),
    }
}

/// `formatTime()`: `toLocaleTimeString('en-US', { hour12: false })`, which is
/// `HH:MM:SS` in 24-hour form.
fn format_time() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// The `tracing`-backed logger the chat pipeline and executors share.
#[derive(Debug, Default)]
pub struct TracingChatLog;

impl ChatLog for TracingChatLog {
    fn line(&self, tag: &str, symbol: &str, message: &str) {
        // `line()` prepends `[HH:MM:SS] `, so the Console Log page shows the
        // timestamped line.
        let time = format_time();
        tracing::info!(target: "router_sse::chat", "[{time}] {tag} {symbol} {message}");
    }

    fn error_line(&self, tag: &str, symbol: &str, message: &str) {
        // `errorLine()` prepends the time too, and is never gated by LOG_LEVEL.
        let time = format_time();
        tracing::error!(target: "router_sse::chat", "[{time}] {tag} {symbol} {message}");
    }
}

impl ExecutorLog for TracingChatLog {
    fn debug(&self, tag: &str, message: &str) {
        tracing::debug!(target: "router_sse::executor", "{tag} | {message}");
    }

    fn info(&self, tag: &str, message: &str) {
        tracing::info!(target: "router_sse::executor", "{tag} | {message}");
    }

    fn error(&self, tag: &str, message: &str) {
        tracing::error!(target: "router_sse::executor", "{tag} | {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_key_keeps_the_ends_and_redacts_short_keys() {
        assert_eq!(mask_key("abc"), "***");
        assert_eq!(mask_key("1234567"), "***");
        assert_eq!(mask_key("12345678"), "1234...5678");
        assert_eq!(mask_key("sk-abcdefghijkl"), "sk-a...ijkl");
    }

    #[test]
    fn tag_for_session_is_stable_per_seed() {
        assert_eq!(tag_for_session("s1"), tag_for_session("s1"));
        assert!(REQ_TAGS.contains(&tag_for_session("")));
        assert!(REQ_TAGS.contains(&tag_for_session("some-seed")));
    }

    #[test]
    fn fmt_think_matches_the_expected_labels() {
        use crate::thinking::ThinkingMode;
        assert_eq!(fmt_think(None), None);
        assert_eq!(
            fmt_think(Some(&ThinkingMode::None)),
            Some("off".to_string())
        );
        assert_eq!(
            fmt_think(Some(&ThinkingMode::Auto)),
            Some("auto".to_string())
        );
        assert_eq!(
            fmt_think(Some(&ThinkingMode::Budget(10000.0))),
            Some("10k".to_string())
        );
        assert_eq!(
            fmt_think(Some(&ThinkingMode::Budget(500.0))),
            Some("500".to_string())
        );
        assert_eq!(
            fmt_think(Some(&ThinkingMode::Level("high".to_string()))),
            Some("high".to_string())
        );
    }
}
