//! Drop a client's built-in tools when an equivalent MCP tool is present.
//!
//! The rules are deliberately narrow. A trigger that is not present leaves the
//! tool list untouched — this is not a general "remove duplicate tools" pass,
//! and a rule that fired on a name it merely resembles would silently delete a
//! tool the model needs.

use serde_json::Value;

/// One dedup rule: any `triggers` name present pulls every `strip` name out.
struct Rule {
    triggers: &'static [Pattern],
    strip: &'static [Pattern],
}

enum Pattern {
    Exact(&'static str),
    /// A `RegExp.test`-style prefix match, restricted to the two prefixes the
    /// rules actually use.
    Prefix(&'static str),
}

impl Pattern {
    fn matches(&self, name: &str) -> bool {
        match self {
            Pattern::Exact(want) => name == *want,
            Pattern::Prefix(prefix) => name.starts_with(prefix),
        }
    }
}

/// `DEDUP_RULES`.
const RULES: &[Rule] = &[
    // Exa MCP present: the built-in web tools go.
    Rule {
        triggers: &[
            Pattern::Exact("mcp__exa__web_search_exa"),
            Pattern::Exact("mcp__exa__web_fetch_exa"),
        ],
        strip: &[
            Pattern::Exact("WebSearch"),
            Pattern::Exact("WebFetch"),
            Pattern::Exact("mcp__workspace__web_fetch"),
        ],
    },
    Rule {
        triggers: &[
            Pattern::Exact("mcp__tavily__tavily_search"),
            Pattern::Exact("mcp__tavily__tavily_extract"),
        ],
        strip: &[
            Pattern::Exact("WebSearch"),
            Pattern::Exact("WebFetch"),
            Pattern::Exact("mcp__workspace__web_fetch"),
        ],
    },
    // Browser MCP present: Cowork's duplicate Claude-in-Chrome connector goes.
    Rule {
        triggers: &[Pattern::Prefix("mcp__browsermcp__")],
        strip: &[Pattern::Prefix("mcp__Claude_in_Chrome__")],
    },
];

/// `getToolName(t)`: Anthropic nests under `name`, OpenAI under `function.name`.
fn tool_name(tool: &Value) -> &str {
    tool.get("name")
        .or_else(|| tool.get("function").and_then(|f| f.get("name")))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// The result of a dedup pass.
pub struct Deduped {
    pub tools: Value,
    /// The names that were dropped, in insertion order — the order the names
    /// were scanned.
    pub stripped: Vec<String>,
}

/// `dedupeTools(tools)`.
pub fn dedupe_tools(tools: &Value) -> Deduped {
    let Some(list) = tools.as_array().filter(|l| !l.is_empty()) else {
        return Deduped {
            tools: tools.clone(),
            stripped: Vec::new(),
        };
    };

    let names: Vec<&str> = list.iter().map(tool_name).collect();
    let mut to_strip: Vec<&str> = Vec::new();
    for rule in RULES {
        let has_trigger = names
            .iter()
            .any(|n| rule.triggers.iter().any(|p| p.matches(n)));
        if !has_trigger {
            continue;
        }
        for name in &names {
            if rule.strip.iter().any(|p| p.matches(name)) && !to_strip.contains(name) {
                to_strip.push(name);
            }
        }
    }

    if to_strip.is_empty() {
        return Deduped {
            tools: tools.clone(),
            stripped: Vec::new(),
        };
    }
    let kept: Vec<Value> = list
        .iter()
        .filter(|t| !to_strip.contains(&tool_name(t)))
        .cloned()
        .collect();
    Deduped {
        tools: Value::Array(kept),
        stripped: to_strip.iter().map(|s| s.to_string()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_exa_tool_drops_the_builtin_web_tools() {
        let tools = json!([
            {"name": "mcp__exa__web_search_exa"},
            {"name": "WebSearch"},
            {"name": "WebFetch"},
            {"name": "mcp__workspace__web_fetch"},
            {"name": "Read"},
        ]);
        let out = dedupe_tools(&tools);
        let kept: Vec<&str> = out
            .tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(kept, ["mcp__exa__web_search_exa", "Read"]);
        assert_eq!(out.stripped.len(), 3);
    }

    #[test]
    fn a_rule_without_its_trigger_changes_nothing() {
        let tools = json!([{"name": "WebSearch"}, {"name": "WebFetch"}, {"name": "Read"}]);
        let out = dedupe_tools(&tools);
        assert_eq!(out.tools, tools);
        assert!(out.stripped.is_empty());
    }

    #[test]
    fn a_tavily_trigger_behaves_like_exa() {
        let tools = json!([{"name": "mcp__tavily__tavily_extract"}, {"name": "WebSearch"}]);
        let out = dedupe_tools(&tools);
        assert_eq!(out.tools.as_array().unwrap().len(), 1);
        assert_eq!(out.stripped, ["WebSearch"]);
    }

    #[test]
    fn the_browser_rule_matches_by_prefix() {
        let tools = json!([
            {"name": "mcp__browsermcp__navigate"},
            {"name": "mcp__Claude_in_Chrome__click"},
            {"name": "mcp__Claude_in_Chrome__type"},
            {"name": "Read"},
        ]);
        let out = dedupe_tools(&tools);
        let kept: Vec<&str> = out
            .tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(kept, ["mcp__browsermcp__navigate", "Read"]);
    }

    #[test]
    fn an_openai_shaped_tool_is_read_through_function_name() {
        let tools = json!([
            {"function": {"name": "mcp__exa__web_search_exa"}},
            {"function": {"name": "WebSearch"}},
        ]);
        let out = dedupe_tools(&tools);
        assert_eq!(out.tools.as_array().unwrap().len(), 1);
        assert_eq!(out.stripped, ["WebSearch"]);
    }

    #[test]
    fn empty_and_non_array_inputs_pass_through() {
        for tools in [json!([]), json!(null), json!("nope")] {
            let out = dedupe_tools(&tools);
            assert_eq!(out.tools, tools);
            assert!(out.stripped.is_empty());
        }
    }

    #[test]
    fn a_name_matching_no_rule_survives_every_rule() {
        let tools = json!([
            {"name": "mcp__exa__web_search_exa"},
            {"name": "mcp__browsermcp__navigate"},
            {"name": "WebSearch"},
            {"name": "mcp__Claude_in_Chrome__click"},
        ]);
        let out = dedupe_tools(&tools);
        let kept: Vec<&str> = out
            .tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            kept,
            ["mcp__exa__web_search_exa", "mcp__browsermcp__navigate"]
        );
    }
}
