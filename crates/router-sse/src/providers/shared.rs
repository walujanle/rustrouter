//! Shared provider constants and the Anthropic beta selection.
//!
//! `selectAnthropicBeta` is the one piece here that is not a plain constant.
//! Two of the flags are gated: `advanced-tool-use`/`effort` only go to
//! opus/sonnet, and `redact-thinking` is dropped when the client explicitly
//! asked for summarized thinking (redacting blanks the summaries it requested).

use serde_json::Value;

/// `ANTHROPIC_API_VERSION`.
pub const ANTHROPIC_API_VERSION: &str = "2023-06-01";
/// The spoofed Claude Code client version. Two modules declare it; keep them in
/// sync deliberately.
pub const CLAUDE_CLI_VERSION: &str = "2.1.280";

/// `OPENAI_COMPAT_BASE`: the default base for `openai-compatible-*` nodes with
/// no user-supplied `baseUrl`.
pub const OPENAI_COMPAT_BASE: &str = "https://api.openai.com/v1";
/// `ANTHROPIC_COMPAT_BASE`, likewise for `anthropic-compatible-*`.
pub const ANTHROPIC_COMPAT_BASE: &str = "https://api.anthropic.com/v1";

/// `mapStainlessOs()`.
pub fn map_stainless_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "MacOS",
        "windows" => "Windows",
        "linux" => "Linux",
        "freebsd" => "FreeBSD",
        _ => "Other",
    }
}

/// `mapStainlessArch()`.
pub fn map_stainless_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "x86",
        _ => "other",
    }
}

/// `CLAUDE_CLI_SPOOF_HEADERS`: the full Claude CLI fingerprint, required by
/// providers that gate on client identity.
pub fn claude_cli_spoof_headers() -> Vec<(&'static str, String)> {
    vec![
        ("Anthropic-Version", ANTHROPIC_API_VERSION.to_string()),
        (
            "Anthropic-Beta",
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,context-management-2025-06-27,prompt-caching-scope-2026-01-05,advanced-tool-use-2025-11-20,effort-2025-11-24,structured-outputs-2025-12-15,fast-mode-2026-02-01,redact-thinking-2026-02-12,token-efficient-tools-2026-03-28"
                .to_string(),
        ),
        ("Anthropic-Dangerous-Direct-Browser-Access", "true".to_string()),
        (
            "User-Agent",
            format!("claude-cli/{CLAUDE_CLI_VERSION} (external, sdk-cli)"),
        ),
        ("X-App", "cli".to_string()),
        ("X-Stainless-Helper-Method", "stream".to_string()),
        ("X-Stainless-Retry-Count", "0".to_string()),
        ("X-Stainless-Runtime-Version", "v24.14.0".to_string()),
        ("X-Stainless-Package-Version", "0.80.0".to_string()),
        ("X-Stainless-Runtime", "node".to_string()),
        ("X-Stainless-Lang", "js".to_string()),
        ("X-Stainless-Arch", map_stainless_arch().to_string()),
        ("X-Stainless-Os", map_stainless_os().to_string()),
        ("X-Stainless-Timeout", "600".to_string()),
    ]
}

/// `ANTHROPIC_BETA_BASE`.
const ANTHROPIC_BETA_BASE: [&str; 9] = [
    "claude-code-20250219",
    "oauth-2025-04-20",
    "interleaved-thinking-2025-05-14",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
    "structured-outputs-2025-12-15",
    "fast-mode-2026-02-01",
    "redact-thinking-2026-02-12",
    "token-efficient-tools-2026-03-28",
];
/// `ANTHROPIC_BETA_HEAVY_AGENT`.
const ANTHROPIC_BETA_HEAVY_AGENT: [&str; 2] = ["advanced-tool-use-2025-11-20", "effort-2025-11-24"];
/// `ANTHROPIC_BETA_REDACT_THINKING`.
const ANTHROPIC_BETA_REDACT_THINKING: &str = "redact-thinking-2026-02-12";

/// `wantsThinkingSummaries(body)`: `body?.thinking?.display === "summarized"`.
pub fn wants_thinking_summaries(body: Option<&Value>) -> bool {
    body.and_then(|b| b.get("thinking"))
        .and_then(|t| t.get("display"))
        .and_then(Value::as_str)
        == Some("summarized")
}

/// `selectAnthropicBeta(model, body)`: the comma-joined beta flag list.
pub fn select_anthropic_beta(model: &str, body: Option<&Value>) -> String {
    let drop_redact = wants_thinking_summaries(body);
    let mut flags: Vec<&str> = ANTHROPIC_BETA_BASE
        .iter()
        .copied()
        .filter(|f| *f != ANTHROPIC_BETA_REDACT_THINKING || !drop_redact)
        .collect();
    if model.starts_with("claude-opus") || model.starts_with("claude-sonnet") {
        flags.extend_from_slice(&ANTHROPIC_BETA_HEAVY_AGENT);
    }
    flags.join(",")
}

/// `mergeAnthropicBeta(...values)`: split each value on commas, trim, drop
/// empties, and dedupe in first-seen order. A non-string value contributes
/// nothing.
pub fn merge_anthropic_beta(values: &[Option<&str>]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for value in values.iter().flatten() {
        for flag in value.split(',') {
            let flag = flag.trim();
            if !flag.is_empty() && !seen.contains(&flag) {
                seen.push(flag);
            }
        }
    }
    seen.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn beta_flags_gate_heavy_agent_to_opus_and_sonnet() {
        let haiku = select_anthropic_beta("claude-haiku-4", None);
        assert!(!haiku.contains("advanced-tool-use-2025-11-20"));
        let opus = select_anthropic_beta("claude-opus-5", None);
        assert!(opus.contains("advanced-tool-use-2025-11-20"));
        assert!(opus.contains("effort-2025-11-24"));
    }

    #[test]
    fn summarized_thinking_drops_the_redact_flag() {
        let plain = select_anthropic_beta("claude-opus-5", None);
        assert!(plain.contains("redact-thinking-2026-02-12"));
        let summarized = select_anthropic_beta(
            "claude-opus-5",
            Some(&json!({"thinking": {"display": "summarized"}})),
        );
        assert!(!summarized.contains("redact-thinking-2026-02-12"));
    }

    #[test]
    fn merging_beta_flags_dedupes_and_drops_empties() {
        // The client set is appended after the selected set, deduped in
        // first-seen order.
        let merged = merge_anthropic_beta(&[Some("a-2025,b-2025"), Some("b-2025, c-2025 ,, ")]);
        assert_eq!(merged, "a-2025,b-2025,c-2025");
        // An absent client header contributes nothing.
        assert_eq!(merge_anthropic_beta(&[Some("x"), None]), "x");
        assert_eq!(merge_anthropic_beta(&[None, None]), "");
    }
}
