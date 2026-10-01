//! RTK token saver: compress `tool_result` content in LLM request bodies.
//!
//! Runs in-place on the source-format body before translation (and again
//! post-translate for the providers whose translator rewrites the shapes).
//! **Fail-open**: any error returns `None` and leaves the body untouched, and a
//! filter that panics leaves the text unchanged. Error traces are preserved —
//! `is_error` (Claude) and `status: "error"` (Kiro) results are skipped.
//!
//! External compression proxying and image-context compression are not
//! implemented; no branch here calls them.

use serde_json::Value;

pub mod autodetect;
pub mod filters;
pub mod prompts;
pub mod system_inject;

pub use autodetect::auto_detect_filter;
pub use filters::apply_filter;
pub use system_inject::inject_system_prompt;

use filters::{MIN_COMPRESS_SIZE, RAW_CAP, resolve_filter, safe_apply};

/// One compression: which body shape it came from, which filter fired, and the
/// UTF-16 code units saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtkHit {
    pub shape: &'static str,
    pub filter: &'static str,
    pub saved: usize,
}

/// `{ bytesBefore, bytesAfter, hits }`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RtkStats {
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub hits: Vec<RtkHit>,
}

/// UTF-16 code units, the length unit used throughout this module. Byte
/// accounting and the never-grow guard must agree, otherwise astral text (emoji,
/// rare CJK) would compare a byte count against a code-unit count.
fn u16len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Read a `&mut Value` as an owned string, so the node can be written back
/// after the immutable borrow ends.
fn take_str(v: &mut Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// Compress `part["text"]` in place when `part["type"]` is `want`.
fn compress_typed_part(part: &mut Value, want: &str, stats: &mut RtkStats, shape: &'static str) {
    if part.get("type").and_then(Value::as_str) != Some(want) {
        return;
    }
    let Some(text) = part.get_mut("text").and_then(take_str) else {
        return;
    };
    let next = compress_text(&text, stats, shape);
    if let Some(obj) = part.as_object_mut() {
        obj.insert("text".into(), Value::String(next));
    }
}

/// `compressMessages(body, enabled)`. `None` on disabled, no eligible shape, or
/// any failure.
pub fn compress_messages(body: &mut Value, enabled: bool) -> Option<RtkStats> {
    if !enabled || body.is_null() {
        return None;
    }

    if body.get("conversationState").is_some() {
        return Some(compress_kiro_format(body));
    }

    let items_key = if body.get("messages").is_some_and(Value::is_array) {
        "messages"
    } else if body.get("input").is_some_and(Value::is_array) {
        "input"
    } else {
        return None;
    };

    let mut stats = RtkStats::default();
    let items = body.get_mut(items_key)?.as_array_mut()?;

    for msg in items.iter_mut() {
        if msg.is_null() {
            continue;
        }

        // Shape 4: OpenAI Responses — { type:"function_call_output", output }.
        if msg.get("type").and_then(Value::as_str) == Some("function_call_output") {
            match msg.get_mut("output") {
                Some(Value::String(s)) => {
                    let next = compress_text(s, &mut stats, "openai-responses-string");
                    *s = next;
                }
                Some(Value::Array(parts)) => {
                    for part in parts.iter_mut() {
                        compress_typed_part(
                            part,
                            "input_text",
                            &mut stats,
                            "openai-responses-array",
                        );
                    }
                }
                _ => {}
            }
            continue;
        }

        // Shape 1: OpenAI tool message — { role:"tool", content:"string" }.
        if msg.get("role").and_then(Value::as_str) == Some("tool")
            && msg.get("content").is_some_and(Value::is_string)
        {
            if let Some(s) = msg.get_mut("content").and_then(take_str) {
                let next = compress_text(&s, &mut stats, "openai-tool");
                if let Some(obj) = msg.as_object_mut() {
                    obj.insert("content".into(), Value::String(next));
                }
            }
            continue;
        }

        let is_tool = msg.get("role").and_then(Value::as_str) == Some("tool");
        let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };

        // Shape 1b: OpenAI tool message — content is [{type:"text", text}].
        if is_tool {
            for part in content.iter_mut() {
                compress_typed_part(part, "text", &mut stats, "openai-tool-array");
            }
            continue;
        }

        // Shape 2/3: Claude blocks with tool_result entries.
        for block in content.iter_mut() {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            match block.get_mut("content") {
                Some(Value::String(s)) => {
                    let next = compress_text(s, &mut stats, "claude-string");
                    *s = next;
                }
                Some(Value::Array(parts)) => {
                    for part in parts.iter_mut() {
                        compress_typed_part(part, "text", &mut stats, "claude-array");
                    }
                }
                _ => {}
            }
        }
    }

    Some(stats)
}

/// `conversationState.history[]` + `conversationState.currentMessage`, tool
/// results only. History turns come first, then currentMessage.
fn compress_kiro_format(body: &mut Value) -> RtkStats {
    let mut stats = RtkStats::default();
    let Some(state) = body
        .get_mut("conversationState")
        .and_then(Value::as_object_mut)
    else {
        return stats;
    };

    if let Some(Value::Array(hist)) = state.get_mut("history") {
        for msg in hist.iter_mut() {
            compress_kiro_turn(msg, &mut stats);
        }
    }
    if let Some(cur) = state.get_mut("currentMessage").filter(|c| !c.is_null()) {
        compress_kiro_turn(cur, &mut stats);
    }

    stats
}

/// Compress one Kiro turn's `toolResults[].content[].text`.
fn compress_kiro_turn(msg: &mut Value, stats: &mut RtkStats) {
    let Some(tool_results) = msg
        .get_mut("userInputMessage")
        .and_then(|u| u.get_mut("userInputMessageContext"))
        .and_then(|c| c.get_mut("toolResults"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for tr in tool_results.iter_mut() {
        if tr.get("status").and_then(Value::as_str) == Some("error") {
            continue;
        }
        let Some(content) = tr.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for part in content.iter_mut() {
            if let Some(text) = part.get_mut("text").and_then(take_str) {
                let next = compress_text(&text, stats, "kiro-tool-result");
                if let Some(obj) = part.as_object_mut() {
                    obj.insert("text".into(), Value::String(next));
                }
            }
        }
    }
}

/// `compressText(text, stats, shape)`: apply a detected filter, but never grow
/// and never empty the input.
fn compress_text(text: &str, stats: &mut RtkStats, shape: &'static str) -> String {
    let bytes_in = u16len(text);
    stats.bytes_before += bytes_in;

    if !(MIN_COMPRESS_SIZE..=RAW_CAP).contains(&bytes_in) {
        stats.bytes_after += bytes_in;
        return text.to_string();
    }

    let Some(name) = auto_detect_filter(text) else {
        stats.bytes_after += bytes_in;
        return text.to_string();
    };
    let Some(filter) = resolve_filter(name) else {
        stats.bytes_after += bytes_in;
        return text.to_string();
    };

    let out = safe_apply(filter, text);
    let out_len = u16len(&out);
    if out.is_empty() || out_len >= bytes_in {
        stats.bytes_after += bytes_in;
        return text.to_string();
    }

    stats.bytes_after += out_len;
    stats.hits.push(RtkHit {
        shape,
        filter: name,
        saved: bytes_in - out_len,
    });
    out
}

/// `formatRtkLog(stats)`.
pub fn format_rtk_log(stats: &Option<RtkStats>) -> Option<String> {
    let stats = stats.as_ref()?;
    if stats.hits.is_empty() {
        return None;
    }
    let saved = stats.bytes_before.saturating_sub(stats.bytes_after);
    let pct = if stats.bytes_before > 0 {
        format!("{:.1}", (saved as f64 / stats.bytes_before as f64) * 100.0)
    } else {
        "0".to_string()
    };
    // `Set` of filter names, insertion order preserved.
    let mut seen: Vec<&str> = Vec::new();
    for hit in &stats.hits {
        if !seen.contains(&hit.filter) {
            seen.push(hit.filter);
        }
    }
    Some(format!(
        "[RTK] saved {saved}B / {}B ({pct}%) via [{}] hits={}",
        stats.bytes_before,
        seen.join(","),
        stats.hits.len()
    ))
}

/// `injectCaveman(body, format, level)`.
pub fn inject_caveman(body: &mut Value, format: &str, level: &str) {
    if let Some(prompt) = prompts::caveman_prompt(level) {
        inject_system_prompt(body, format, &prompt);
    }
}

/// `injectPonytail(body, format, level)`.
pub fn inject_ponytail(body: &mut Value, format: &str, level: &str) {
    if let Some(prompt) = prompts::ponytail_prompt(level) {
        inject_system_prompt(body, format, &prompt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn long_diff() -> String {
        let mut lines: Vec<String> = vec![
            "diff --git a/foo.js b/foo.js".into(),
            "index abc..def 100644".into(),
            "--- a/foo.js".into(),
            "+++ b/foo.js".into(),
            "@@ -1,3 +1,200 @@".into(),
        ];
        for i in 0..200 {
            lines.push(format!("+added line {i} {}", "x".repeat(20)));
        }
        lines.join("\n")
    }

    #[test]
    fn disabled_returns_none() {
        let mut body = json!({"messages": [{"role": "tool", "content": long_diff()}]});
        assert!(compress_messages(&mut body, false).is_none());
    }

    #[test]
    fn no_messages_returns_none() {
        assert!(compress_messages(&mut json!({}), true).is_none());
        assert!(compress_messages(&mut json!({"messages": null}), true).is_none());
    }

    #[test]
    fn openai_tool_string_is_compressed() {
        let big = long_diff();
        let mut body =
            json!({"messages": [{"role": "tool", "tool_call_id": "call_1", "content": big}]});
        let stats = compress_messages(&mut body, true).unwrap();
        assert!(!stats.hits.is_empty());
        let after = body["messages"][0]["content"].as_str().unwrap();
        assert!(after.len() < big.len());
        assert!(stats.bytes_before > stats.bytes_after);
        assert_eq!(stats.hits[0].shape, "openai-tool");
        assert_eq!(stats.hits[0].filter, "git-diff");
    }

    #[test]
    fn claude_string_tool_result_is_compressed() {
        let big = long_diff();
        let mut body = json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": big}
        ]}]});
        let stats = compress_messages(&mut body, true).unwrap();
        assert!(!stats.hits.is_empty());
        let after = body["messages"][0]["content"][0]["content"]
            .as_str()
            .unwrap();
        assert!(after.len() < big.len());
    }

    #[test]
    fn claude_array_tool_result_compresses_text_parts() {
        let big = long_diff();
        let mut body = json!({"messages": [{"role": "user", "content": [{
            "type": "tool_result", "tool_use_id": "toolu_1",
            "content": [{"type": "text", "text": big}, {"type": "text", "text": "unchanged short"}]
        }]}]});
        let stats = compress_messages(&mut body, true).unwrap();
        assert!(!stats.hits.is_empty());
        let first = body["messages"][0]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(first.len() < big.len());
        assert_eq!(
            body["messages"][0]["content"][0]["content"][1]["text"],
            "unchanged short"
        );
    }

    #[test]
    fn is_error_tool_result_is_skipped() {
        let big = long_diff();
        let mut body = json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": big, "is_error": true}
        ]}]});
        let stats = compress_messages(&mut body, true).unwrap();
        assert_eq!(stats.hits.len(), 0);
        assert_eq!(body["messages"][0]["content"][0]["content"], big);
    }

    #[test]
    fn below_min_size_is_skipped() {
        let small = "diff --git a/x b/x\n@@ -1 +1 @@\n+a";
        let mut body =
            json!({"messages": [{"role": "tool", "tool_call_id": "x", "content": small}]});
        let stats = compress_messages(&mut body, true).unwrap();
        assert_eq!(stats.hits.len(), 0);
        assert_eq!(body["messages"][0]["content"], small);
    }

    #[test]
    fn never_produces_empty_content() {
        let input = "a".repeat(1000);
        let mut body =
            json!({"messages": [{"role": "tool", "tool_call_id": "x", "content": input}]});
        compress_messages(&mut body, true);
        let after = body["messages"][0]["content"].as_str().unwrap();
        assert!(!after.is_empty());
    }

    #[test]
    fn responses_function_call_output_string_and_array() {
        let big = long_diff();
        let mut body = json!({"input": [
            {"type": "function_call_output", "output": big},
            {"type": "function_call_output", "output": [{"type": "input_text", "text": big}]}
        ]});
        let stats = compress_messages(&mut body, true).unwrap();
        assert_eq!(stats.hits.len(), 2);
        assert!(
            stats
                .hits
                .iter()
                .any(|h| h.shape == "openai-responses-string")
        );
        assert!(
            stats
                .hits
                .iter()
                .any(|h| h.shape == "openai-responses-array")
        );
    }

    #[test]
    fn kiro_current_message_is_compressed() {
        let mut lines = Vec::new();
        for i in 1..=20 {
            lines.push(format!("   Compiling package-{i} v1.0.{i}"));
        }
        lines.push(
            "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.34s".into(),
        );
        let mut body = json!({"conversationState": {
            "currentMessage": {"userInputMessage": {"content": "go", "userInputMessageContext": {
                "toolResults": [{"toolUseId": "t", "status": "success", "content": [{"text": lines.join("\n")}]}]
            }}},
            "history": []
        }});
        let stats = compress_messages(&mut body, true).unwrap();
        assert_eq!(stats.hits.len(), 1);
        assert_eq!(stats.hits[0].filter, "build-output");
        assert_eq!(stats.hits[0].shape, "kiro-tool-result");
        assert!(stats.bytes_after < stats.bytes_before);
        let text = body["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"]["toolResults"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("Compiled 20 packages"));
    }

    #[test]
    fn kiro_error_status_is_preserved() {
        let original =
            "npm error code E404\nnpm error 404 Not Found - GET https://registry.npmjs.org/x";
        let mut body = json!({"conversationState": {
            "currentMessage": {"userInputMessage": {"content": "go", "userInputMessageContext": {
                "toolResults": [{"toolUseId": "t", "status": "error", "content": [{"text": original}]}]
            }}},
            "history": []
        }});
        let stats = compress_messages(&mut body, true).unwrap();
        assert_eq!(stats.hits.len(), 0);
        assert_eq!(
            body["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"]
                ["toolResults"][0]["content"][0]["text"],
            original
        );
    }

    #[test]
    fn kiro_history_and_current_both_compressed() {
        let mut d1 = Vec::new();
        let mut d2 = Vec::new();
        for i in 1..=10 {
            d1.push(format!(
                "npm warn deprecated package-{i}@1.0.0: This version is deprecated"
            ));
            d2.push(format!(
                "npm warn deprecated lib-{i}@2.0.0: This library is no longer supported"
            ));
        }
        d1.push("added 50 packages in 5s".into());
        d2.push("added 1 package in 2s".into());
        let mut body = json!({"conversationState": {
            "currentMessage": {"userInputMessage": {"content": "b", "userInputMessageContext": {
                "toolResults": [{"toolUseId": "t3", "status": "success", "content": [{"text": d2.join("\n")}]}]
            }}},
            "history": [{"userInputMessage": {"content": "a", "userInputMessageContext": {
                "toolResults": [{"toolUseId": "t4", "status": "success", "content": [{"text": d1.join("\n")}]}]
            }}}]
        }});
        let stats = compress_messages(&mut body, true).unwrap();
        assert_eq!(stats.hits.len(), 2);
        assert!(stats.hits.iter().all(|h| h.filter == "build-output"));
    }

    #[test]
    fn kiro_no_tool_results_is_empty_stats() {
        let mut body = json!({"conversationState": {
            "currentMessage": {"userInputMessage": {"content": "Hello"}},
            "history": []
        }});
        let stats = compress_messages(&mut body, true).unwrap();
        assert_eq!(stats.hits.len(), 0);
        assert_eq!(stats.bytes_before, 0);
        assert_eq!(stats.bytes_after, 0);
    }

    #[test]
    fn kiro_malformed_bodies_do_not_panic() {
        for mut body in [
            json!({"conversationState": null}),
            json!({"conversationState": {}}),
            json!({"conversationState": {"history": null, "currentMessage": null}}),
            json!({"conversationState": {"history": "not-an-array"}}),
        ] {
            let stats = compress_messages(&mut body, true).unwrap();
            assert_eq!(stats.hits.len(), 0);
        }
    }

    #[test]
    fn format_rtk_log_matches_reference_string() {
        let stats = RtkStats {
            bytes_before: 1000,
            bytes_after: 400,
            hits: vec![RtkHit {
                shape: "openai-tool",
                filter: "git-diff",
                saved: 600,
            }],
        };
        let line = format_rtk_log(&Some(stats)).unwrap();
        assert_eq!(
            line,
            "[RTK] saved 600B / 1000B (60.0%) via [git-diff] hits=1"
        );
    }

    #[test]
    fn format_rtk_log_dedups_filters_in_insertion_order() {
        let stats = RtkStats {
            bytes_before: 2000,
            bytes_after: 1000,
            hits: vec![
                RtkHit {
                    shape: "a",
                    filter: "grep",
                    saved: 500,
                },
                RtkHit {
                    shape: "b",
                    filter: "git-diff",
                    saved: 300,
                },
                RtkHit {
                    shape: "c",
                    filter: "grep",
                    saved: 200,
                },
            ],
        };
        let line = format_rtk_log(&Some(stats)).unwrap();
        assert_eq!(
            line,
            "[RTK] saved 1000B / 2000B (50.0%) via [grep,git-diff] hits=3"
        );
    }

    #[test]
    fn format_rtk_log_none_without_hits() {
        assert!(format_rtk_log(&None).is_none());
        assert!(format_rtk_log(&Some(RtkStats::default())).is_none());
    }

    #[test]
    fn inject_caveman_unknown_level_is_a_noop() {
        let mut body = json!({"messages": [{"role": "user", "content": "hi"}]});
        let before = body.clone();
        inject_caveman(&mut body, "openai", "nope");
        assert_eq!(body, before);
    }

    #[test]
    fn inject_ponytail_ultra_reaches_the_system_message() {
        let mut body = json!({"messages": [{"role": "system", "content": "base"}]});
        inject_ponytail(&mut body, "openai", "ultra");
        let content = body["messages"][0]["content"].as_str().unwrap();
        assert!(content.starts_with("base\n\n"));
        assert!(content.contains("You are a lazy senior developer."));
        assert!(content.contains("YAGNI extremist"));
    }
}
