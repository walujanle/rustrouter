//! The CLI-tools menu: configure Claude Code, Codex CLI and Hermes to point at
//! this server, with Quick Setup, per-model picks and Reset.
//!
//! Only three writers exist server-side (`cli-tools/{claude,codex,hermes}-settings`),
//! so the menu offers those three.

use serde_json::{Value, json};

use crate::cli::menu::{self, Entry};
use crate::cli::model_selector::{first_api_key, select_model_from_list};
use crate::cli::term::{self, StatusKind};
use crate::cli::{Ctx, colors};

/// `CLAUDE_MODEL_TYPES`: the three Anthropic model env vars. The defaults are
/// empty — `cc` is no longer a live alias, so the writer must not seed a model
/// the user cannot reach; the picker fills them in.
const CLAUDE_MODEL_TYPES: [(&str, &str, &str); 3] = [
    ("Sonnet", "ANTHROPIC_DEFAULT_SONNET_MODEL", ""),
    ("Opus", "ANTHROPIC_DEFAULT_OPUS_MODEL", ""),
    ("Haiku", "ANTHROPIC_DEFAULT_HAIKU_MODEL", ""),
];

/// `getEndpoint(port)`: tunnel is dropped, so this is the local endpoint.
fn endpoint(ctx: &Ctx) -> String {
    format!("http://localhost:{}/v1", ctx.port)
}

pub fn show(ctx: &Ctx, breadcrumb: &[String]) {
    let claude = {
        let mut crumb = breadcrumb.to_vec();
        crumb.push("Claude Code".to_string());
        move |_: &Value| {
            show_claude_menu(ctx, &crumb);
            true
        }
    };
    let codex = {
        let mut crumb = breadcrumb.to_vec();
        crumb.push("Codex CLI".to_string());
        move |_: &Value| {
            show_codex_menu(ctx, &crumb);
            true
        }
    };
    let hermes = {
        let mut crumb = breadcrumb.to_vec();
        crumb.push("Hermes".to_string());
        move |_: &Value| {
            show_hermes_menu(ctx, &crumb);
            true
        }
    };

    let mut items = vec![
        Entry::action(|_| "Claude Code".to_string(), move |v| claude(v)),
        Entry::action(|_| "Codex CLI".to_string(), move |v| codex(v)),
        Entry::action(|_| "Hermes".to_string(), move |v| hermes(v)),
    ];

    let header = format!(
        "Configure CLI tools to use RustRouter\nEndpoint: {}",
        endpoint(ctx)
    );
    menu::show_menu_with_back(
        "🔧 CLI Tools",
        breadcrumb,
        "← Back",
        0,
        || Some(Value::Null),
        move |_| header.clone(),
        &mut items,
    );
}

// ─── Claude Code ──────────────────────────────────────────────────────────

/// `buildClaudeHeader`: configured/not, endpoint and the key's first ten chars.
fn claude_header(ctx: &Ctx) -> String {
    let result = ctx.api.get_cli_tool_settings("claude");
    if !result.success {
        return format!("  {}Failed to load settings{}", colors::RED, colors::RESET);
    }
    let settings = result.data.get("settings").cloned().unwrap_or(Value::Null);
    let env = settings.get("env");
    let current_url = env
        .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
        .and_then(Value::as_str);
    let current_key = env
        .and_then(|e| e.get("ANTHROPIC_AUTH_TOKEN"))
        .and_then(Value::as_str);

    let mut lines = Vec::new();
    if let Some(url) = current_url {
        lines.push(format!(
            "Status:   {}✓ Configured{}",
            colors::GREEN,
            colors::RESET
        ));
        lines.push(format!("Endpoint: {}{url}{}", colors::CYAN, colors::RESET));
        if let Some(key) = current_key {
            let prefix: String = key.chars().take(10).collect();
            lines.push(format!(
                "API Key:  {}{prefix}...{}",
                colors::DIM,
                colors::RESET
            ));
        }
    } else {
        lines.push(format!(
            "Status:   {}✗ Not configured{}",
            colors::RED,
            colors::RESET
        ));
        lines.push(format!(
            "{}Run \"Quick Setup\" to configure{}",
            colors::DIM,
            colors::RESET
        ));
    }
    lines.join("\n")
}

/// `getClaudeModel(envKey)`.
fn claude_model(ctx: &Ctx, env_key: &str) -> String {
    let result = ctx.api.get_cli_tool_settings("claude");
    if !result.success {
        return "Not set".to_string();
    }
    result
        .data
        .get("settings")
        .and_then(|s| s.get("env"))
        .and_then(|e| e.get(env_key))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "Not set".to_string())
}

fn show_claude_menu(ctx: &Ctx, breadcrumb: &[String]) {
    let header = claude_header(ctx);
    let quick = |_: &Value| {
        claude_quick_setup(ctx);
        true
    };
    let sonnet = |_: &Value| {
        claude_select_model(ctx, 0);
        true
    };
    let opus = |_: &Value| {
        claude_select_model(ctx, 1);
        true
    };
    let haiku = |_: &Value| {
        claude_select_model(ctx, 2);
        true
    };
    let reset = |_: &Value| {
        claude_reset(ctx);
        true
    };

    let mut items = vec![
        Entry::action(
            |_| "⚡ Quick Setup (recommended)".to_string(),
            move |v| quick(v),
        ),
        Entry::action(
            move |d| {
                let current = d.get("sonnet").and_then(Value::as_str).unwrap_or("Not set");
                format!("Sonnet → {current}")
            },
            move |v| sonnet(v),
        ),
        Entry::action(
            move |d| {
                let current = d.get("opus").and_then(Value::as_str).unwrap_or("Not set");
                format!("Opus → {current}")
            },
            move |v| opus(v),
        ),
        Entry::action(
            move |d| {
                let current = d.get("haiku").and_then(Value::as_str).unwrap_or("Not set");
                format!("Haiku → {current}")
            },
            move |v| haiku(v),
        ),
        Entry::action(|_| "Reset to Default".to_string(), move |v| reset(v)),
    ];

    menu::show_menu_with_back(
        "🔧 Claude Code Settings",
        breadcrumb,
        "← Back",
        0,
        || {
            Some(json!({
                "sonnet": claude_model(ctx, "ANTHROPIC_DEFAULT_SONNET_MODEL"),
                "opus": claude_model(ctx, "ANTHROPIC_DEFAULT_OPUS_MODEL"),
                "haiku": claude_model(ctx, "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            }))
        },
        move |_| header.clone(),
        &mut items,
    );
}

/// `claudeQuickSetup`: endpoint, the first key, the timeout and all three
/// default models in one write.
fn claude_quick_setup(ctx: &Ctx) {
    let Some(api_key) = first_api_key(&ctx.api) else {
        term::show_status(
            "No API keys found. Create one in API Keys menu first.",
            StatusKind::Error,
        );
        term::pause("Press Enter to continue...");
        return;
    };
    let mut env = json!({
        "ANTHROPIC_BASE_URL": endpoint(ctx),
        "ANTHROPIC_AUTH_TOKEN": api_key,
        "API_TIMEOUT_MS": "600000",
    });
    for (_, env_key, default) in CLAUDE_MODEL_TYPES {
        env[env_key] = json!(default);
    }
    let result = ctx
        .api
        .apply_cli_tool_settings("claude", &json!({ "env": env }));
    if result.success {
        term::show_status("Quick Setup completed!", StatusKind::Success);
    } else {
        term::show_status(&format!("Failed: {}", result.error), StatusKind::Error);
    }
    term::pause("Press Enter to continue...");
}

/// `claudeSelectModel`: pick one model, and backfill the base URL if the tool
/// was never configured.
fn claude_select_model(ctx: &Ctx, index: usize) {
    let (name, env_key, _) = CLAUDE_MODEL_TYPES[index];
    let current = claude_model(ctx, env_key);
    let Some(selected) =
        select_model_from_list(&ctx.api, &format!("Select {name} Model"), &current, true)
    else {
        return;
    };

    let mut env = json!({ env_key: selected });
    let settings = ctx.api.get_cli_tool_settings("claude");
    let configured = settings
        .data
        .get("settings")
        .and_then(|s| s.get("env"))
        .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
        .is_some();
    if !configured {
        env["ANTHROPIC_BASE_URL"] = json!(endpoint(ctx));
        env["API_TIMEOUT_MS"] = json!("600000");
        if let Some(api_key) = first_api_key(&ctx.api) {
            env["ANTHROPIC_AUTH_TOKEN"] = json!(api_key);
        }
    }

    let result = ctx
        .api
        .apply_cli_tool_settings("claude", &json!({ "env": env }));
    if result.success {
        let selected = env[env_key].as_str().unwrap_or("");
        term::show_status(&format!("{name} → {selected} saved!"), StatusKind::Success);
    } else {
        term::show_status(&format!("Failed: {}", result.error), StatusKind::Error);
    }
    term::pause("Press Enter to continue...");
}

fn claude_reset(ctx: &Ctx) {
    let result = ctx.api.reset_cli_tool_settings("claude");
    if result.success {
        term::show_status("Settings reset successfully!", StatusKind::Success);
    } else {
        term::show_status(&format!("Failed: {}", result.error), StatusKind::Error);
    }
    term::pause("Press Enter to continue...");
}

// ─── Codex CLI ────────────────────────────────────────────────────────────

/// `buildCodexHeader`: the raw TOML is scanned for `base_url` and `model` —
/// there is no parsed form on this endpoint.
fn codex_header(ctx: &Ctx) -> String {
    let result = ctx.api.get_cli_tool_settings("codex");
    if !result.success {
        return format!("  {}Failed to load settings{}", colors::RED, colors::RESET);
    }
    let installed = result.data.get("installed").and_then(Value::as_bool) == Some(true);
    if !installed {
        return format!(
            "Status:   {}✗ Codex CLI not installed{}",
            colors::RED,
            colors::RESET
        );
    }
    let has_9router = result.data.get("has9Router").and_then(Value::as_bool) == Some(true);
    if !has_9router {
        return format!(
            "Status:   {}✗ Not configured{}\n{}Run \"Quick Setup\" to configure{}",
            colors::RED,
            colors::RESET,
            colors::DIM,
            colors::RESET
        );
    }
    let config = result
        .data
        .get("config")
        .and_then(Value::as_str)
        .unwrap_or("");
    let base_url = toml_string_value(config, "base_url");
    let model = toml_string_value(config, "model");
    let mut lines = vec![format!(
        "Status:   {}✓ Configured{}",
        colors::GREEN,
        colors::RESET
    )];
    if let Some(base_url) = base_url {
        lines.push(format!(
            "Endpoint: {}{base_url}{}",
            colors::CYAN,
            colors::RESET
        ));
    }
    if let Some(model) = model {
        lines.push(format!("Model:    {}{model}{}", colors::DIM, colors::RESET));
    }
    lines.join("\n")
}

/// The first `key = "value"` line in a TOML document. `key` is matched at the
/// start of a line for `model` and anywhere for `base_url`, enough for this
/// display-only read.
fn toml_string_value(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim_start();
        let rest = if key == "model" {
            // `continue`, not `?`: a comment or table header above `model =`
            // must not abort the whole scan. `?` here returned None from the
            // function, so the Codex header lost its model line for any config
            // that is not `model`-first.
            let Some(rest) = trimmed.strip_prefix("model") else {
                continue;
            };
            rest
        } else {
            let Some(pos) = line.find(key) else {
                continue;
            };
            &line[pos + key.len()..]
        };
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('"') else {
            continue;
        };
        if let Some(end) = rest.find('"') {
            return Some(rest[..end].to_string());
        }
    }
    None
}

fn show_codex_menu(ctx: &Ctx, breadcrumb: &[String]) {
    let header = codex_header(ctx);
    let quick = |_: &Value| {
        codex_quick_setup(ctx);
        true
    };
    let reset = |_: &Value| {
        codex_reset(ctx);
        true
    };
    let mut items = vec![
        Entry::action(|_| "⚡ Quick Setup".to_string(), move |v| quick(v)),
        Entry::action(|_| "Reset to Default".to_string(), move |v| reset(v)),
    ];
    menu::show_menu_with_back(
        "🤖 Codex CLI Settings",
        breadcrumb,
        "← Back",
        0,
        || Some(Value::Null),
        move |_| header.clone(),
        &mut items,
    );
}

fn codex_quick_setup(ctx: &Ctx) {
    let Some(api_key) = first_api_key(&ctx.api) else {
        term::show_status(
            "No API keys found. Create one in API Keys menu first.",
            StatusKind::Error,
        );
        term::pause("Press Enter to continue...");
        return;
    };
    let Some(model) = select_model_from_list(
        &ctx.api,
        "Select Codex Model",
        "cx/claude-sonnet-4-5-20250929",
        true,
    ) else {
        return;
    };
    let result = ctx.api.apply_cli_tool_settings(
        "codex",
        &json!({ "baseUrl": endpoint(ctx), "apiKey": api_key, "model": model }),
    );
    if result.success {
        term::show_status("Codex setup completed!", StatusKind::Success);
    } else {
        term::show_status(&format!("Failed: {}", result.error), StatusKind::Error);
    }
    term::pause("Press Enter to continue...");
}

fn codex_reset(ctx: &Ctx) {
    let result = ctx.api.reset_cli_tool_settings("codex");
    if result.success {
        term::show_status("Codex settings reset!", StatusKind::Success);
    } else {
        term::show_status(&format!("Failed: {}", result.error), StatusKind::Error);
    }
    term::pause("Press Enter to continue...");
}

// ─── Hermes Agent ─────────────────────────────────────────────────────────

fn hermes_header(ctx: &Ctx) -> String {
    let result = ctx.api.get_cli_tool_settings("hermes");
    if !result.success {
        return format!("  {}Failed to load settings{}", colors::RED, colors::RESET);
    }
    let installed = result.data.get("installed").and_then(Value::as_bool) == Some(true);
    if !installed {
        return format!(
            "Status:   {}✗ Hermes Agent not installed{}",
            colors::RED,
            colors::RESET
        );
    }
    let has_9router = result.data.get("has9Router").and_then(Value::as_bool) == Some(true);
    if !has_9router {
        return format!(
            "Status:   {}✗ Not configured{}\n{}Run \"Quick Setup\" to configure{}",
            colors::RED,
            colors::RESET,
            colors::DIM,
            colors::RESET
        );
    }
    let model = result
        .data
        .get("settings")
        .and_then(|s| s.get("model"))
        .cloned()
        .unwrap_or(Value::Null);
    let mut lines = vec![format!(
        "Status:   {}✓ Configured{}",
        colors::GREEN,
        colors::RESET
    )];
    if let Some(base_url) = model.get("base_url").and_then(Value::as_str) {
        lines.push(format!(
            "Endpoint: {}{base_url}{}",
            colors::CYAN,
            colors::RESET
        ));
    }
    if let Some(default) = model.get("default").and_then(Value::as_str) {
        lines.push(format!(
            "Model:    {}{default}{}",
            colors::DIM,
            colors::RESET
        ));
    }
    lines.join("\n")
}

fn show_hermes_menu(ctx: &Ctx, breadcrumb: &[String]) {
    let header = hermes_header(ctx);
    let quick = |_: &Value| {
        hermes_quick_setup(ctx);
        true
    };
    let reset = |_: &Value| {
        hermes_reset(ctx);
        true
    };
    let mut items = vec![
        Entry::action(|_| "⚡ Quick Setup".to_string(), move |v| quick(v)),
        Entry::action(|_| "Reset to Default".to_string(), move |v| reset(v)),
    ];
    menu::show_menu_with_back(
        "⚡ Hermes Agent Settings",
        breadcrumb,
        "← Back",
        0,
        || Some(Value::Null),
        move |_| header.clone(),
        &mut items,
    );
}

fn hermes_quick_setup(ctx: &Ctx) {
    let Some(api_key) = first_api_key(&ctx.api) else {
        term::show_status(
            "No API keys found. Create one in API Keys menu first.",
            StatusKind::Error,
        );
        term::pause("Press Enter to continue...");
        return;
    };
    let Some(model) = select_model_from_list(&ctx.api, "Select Hermes Model", "", true) else {
        return;
    };
    let result = ctx.api.apply_cli_tool_settings(
        "hermes",
        &json!({ "baseUrl": endpoint(ctx), "apiKey": api_key, "model": model }),
    );
    if result.success {
        term::show_status("Hermes setup completed!", StatusKind::Success);
    } else {
        term::show_status(&format!("Failed: {}", result.error), StatusKind::Error);
    }
    term::pause("Press Enter to continue...");
}

fn hermes_reset(ctx: &Ctx) {
    let result = ctx.api.reset_cli_tool_settings("hermes");
    if result.success {
        term::show_status("Hermes settings reset!", StatusKind::Success);
    } else {
        term::show_status(&format!("Failed: {}", result.error), StatusKind::Error);
    }
    term::pause("Press Enter to continue...");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_model_reads_a_top_level_key() {
        let text = "model = \"cx/gpt-5\"\nmodel_provider = \"9router\"\n";
        assert_eq!(
            toml_string_value(text, "model").as_deref(),
            Some("cx/gpt-5")
        );
    }

    #[test]
    fn toml_model_is_found_after_other_lines() {
        // A table header or comment above `model =` must not abort the scan.
        let text = "# Codex config\n[model_providers.9router]\nname = \"RustRouter\"\nmodel = \"cx/gpt-5\"\n";
        assert_eq!(
            toml_string_value(text, "model").as_deref(),
            Some("cx/gpt-5")
        );
    }

    #[test]
    fn toml_base_url_reads_a_nested_key() {
        let text = "[model_providers.9router]\nname = \"RustRouter\"\nbase_url = \"http://localhost:20129/v1\"\n";
        assert_eq!(
            toml_string_value(text, "base_url").as_deref(),
            Some("http://localhost:20129/v1")
        );
    }

    #[test]
    fn missing_key_is_none() {
        assert!(toml_string_value("", "model").is_none());
    }
}
