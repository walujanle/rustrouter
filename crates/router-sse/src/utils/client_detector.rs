//! Which CLI tool is calling, and whether its request can be forwarded to this
//! provider without translation.
//!
//! The pair matters because a native passthrough skips the translator
//! entirely, which is the only way a Claude Code request reaches Anthropic with
//! its thinking blocks, tool ids and 1M-context beta intact.

use serde_json::Value;

/// `NATIVE_PAIRS`: the providers each detected client is native to.
fn native_providers(client_tool: &str) -> Option<&'static [&'static str]> {
    match client_tool {
        "claude" => Some(&["claude", "anthropic"]),
        "codex" => Some(&["codex"]),
        _ => None,
    }
}

/// `detectClientTool(headers, body)`.
///
/// `headers` must be lowercase-keyed, as they are on the wire. `body` is kept
/// in the signature to mirror the reference; no current detection reads it.
pub fn detect_client_tool(headers: &Value, _body: &Value) -> Option<&'static str> {
    let header = |key: &str| {
        headers
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase()
    };
    let ua = header("user-agent");
    let x_app = header("x-app");
    let openai_intent = header("openai-intent");
    let initiator = if headers.get("x-initiator").is_some() {
        header("x-initiator")
    } else {
        header("X-Initiator")
    };
    let originator = header("originator");

    // An OAI-compatible extension using Copilot chat headers.
    if ua.contains("githubcopilotchat")
        || openai_intent == "conversation-panel"
        || initiator == "user"
    {
        return Some("github-copilot");
    }

    // Claude Code / Claude CLI.
    if ua.contains("claude-cli") || ua.contains("claude-code") || x_app == "cli" {
        return Some("claude");
    }

    if ua.contains("gemini-cli") {
        return Some("gemini-cli");
    }

    // Codex CLI/Desktop: `codex-tui` is the current Rust CLI, `codex-cli` and
    // `codex_cli_rs` are legacy; Desktop uses the UA or an `originator`.
    if ua.contains("codex-tui")
        || ua.contains("codex-cli")
        || ua.contains("codex_cli_rs")
        || ua.contains("codex desktop")
        || originator.starts_with("codex_")
    {
        return Some("codex");
    }

    if ua.contains("deepseek-tui") {
        return Some("deepseek-tui");
    }

    None
}

/// `isNativePassthrough(clientTool, provider)`.
pub fn is_native_passthrough(client_tool: Option<&str>, provider: &str) -> bool {
    let Some(client_tool) = client_tool else {
        return false;
    };
    let Some(native) = native_providers(client_tool) else {
        return false;
    };
    // `anthropic-compatible-*` variants count as Anthropic.
    let normalized = if provider.starts_with("anthropic-compatible") {
        "anthropic"
    } else {
        provider
    };
    native.contains(&normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn headers(pairs: &[(&str, &str)]) -> Value {
        Value::Object(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), json!(v)))
                .collect(),
        )
    }

    #[test]
    fn copilot_headers_win_over_the_rest() {
        assert_eq!(
            detect_client_tool(
                &headers(&[("user-agent", "GitHubCopilotChat/1.0")]),
                &json!({})
            ),
            Some("github-copilot")
        );
        assert_eq!(
            detect_client_tool(
                &headers(&[("openai-intent", "conversation-panel")]),
                &json!({})
            ),
            Some("github-copilot")
        );
        assert_eq!(
            detect_client_tool(&headers(&[("x-initiator", "user")]), &json!({})),
            Some("github-copilot")
        );
    }

    #[test]
    fn the_claude_code_user_agents_are_recognised() {
        for ua in ["claude-cli/1.0", "Claude-Code/2.0", "x"] {
            let h = if ua == "x" {
                headers(&[("x-app", "cli")])
            } else {
                headers(&[("user-agent", ua)])
            };
            assert_eq!(detect_client_tool(&h, &json!({})), Some("claude"), "{ua}");
        }
    }

    #[test]
    fn the_codex_user_agents_and_originator_are_recognised() {
        for ua in [
            "codex-tui/1",
            "codex-cli/0.1",
            "codex_cli_rs/0.154.0",
            "Codex Desktop/1",
        ] {
            assert_eq!(
                detect_client_tool(&headers(&[("user-agent", ua)]), &json!({})),
                Some("codex"),
                "{ua}"
            );
        }
        assert_eq!(
            detect_client_tool(
                &headers(&[("originator", "codex_work_desktop")]),
                &json!({})
            ),
            Some("codex")
        );
        // A non-codex originator does not match.
        assert_eq!(
            detect_client_tool(&headers(&[("originator", "other_thing")]), &json!({})),
            None
        );
    }

    #[test]
    fn the_remaining_clients_are_recognised() {
        assert_eq!(
            detect_client_tool(&headers(&[("user-agent", "gemini-cli/1")]), &json!({})),
            Some("gemini-cli")
        );
        assert_eq!(
            detect_client_tool(&headers(&[("user-agent", "deepseek-tui/1")]), &json!({})),
            Some("deepseek-tui")
        );
        assert_eq!(detect_client_tool(&json!({}), &json!({})), None);
    }

    #[test]
    fn native_passthrough_needs_a_matching_pair() {
        assert!(is_native_passthrough(Some("claude"), "claude"));
        assert!(is_native_passthrough(Some("claude"), "anthropic"));
        // The `anthropic-compatible-*` variants normalise to `anthropic`.
        assert!(is_native_passthrough(
            Some("claude"),
            "anthropic-compatible-2024"
        ));
        assert!(is_native_passthrough(Some("codex"), "codex"));

        assert!(!is_native_passthrough(Some("claude"), "openai"));
        assert!(!is_native_passthrough(None, "claude"));
        // A detected client with no pair is never native.
        assert!(!is_native_passthrough(Some("github-copilot"), "claude"));
        assert!(!is_native_passthrough(Some("deepseek-tui"), "deepseek"));
    }
}
