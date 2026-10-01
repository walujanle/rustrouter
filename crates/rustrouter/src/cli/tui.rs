//! The main menu the terminal UI opens on: a `📡 RustRouter Terminal UI` header
//! with the endpoint and the first API key, five rows, and
//! `← Back to Interface Menu` as the back label.

use serde_json::Value;

use super::menu::{self, Entry};
use super::menus;
use super::{Ctx, colors};

/// Start the terminal UI against a running server on `port`, using `token` for
/// the CLI header.
pub fn start(port: u16, token: String) -> anyhow::Result<()> {
    let ctx = Ctx {
        api: super::api::Api::new(port, token)?,
        port,
    };
    // A shared borrow, so every row's closure captures a `Copy` of the
    // borrow rather than moving the context.
    let ctx = &ctx;

    let base_path = vec!["RustRouter".to_string()];

    let providers = {
        let mut crumb = base_path.clone();
        crumb.push("Providers".to_string());
        move |_: &Value| {
            menus::providers::show(ctx, &crumb);
            true
        }
    };
    let api_keys = {
        let mut crumb = base_path.clone();
        crumb.push("API Keys".to_string());
        move |_: &Value| {
            menus::api_keys::show(ctx, &crumb);
            true
        }
    };
    let combos = {
        let mut crumb = base_path.clone();
        crumb.push("Combos".to_string());
        move |_: &Value| {
            menus::combos::show(ctx, &crumb);
            true
        }
    };
    let cli_tools = {
        let mut crumb = base_path.clone();
        crumb.push("CLI Tools".to_string());
        move |_: &Value| {
            menus::cli_tools::show(ctx, &crumb);
            true
        }
    };
    let settings = {
        let mut crumb = base_path.clone();
        crumb.push("Settings".to_string());
        move |_: &Value| {
            menus::settings::show(ctx, &crumb);
            true
        }
    };

    let mut items = vec![
        Entry::action(|_| "Providers".to_string(), move |v| providers(v)),
        Entry::action(|_| "API Keys".to_string(), move |v| api_keys(v)),
        Entry::action(|_| "Combos".to_string(), move |v| combos(v)),
        Entry::action(|_| "CLI Tools".to_string(), move |v| cli_tools(v)),
        Entry::action(|_| "Settings".to_string(), move |v| settings(v)),
    ];

    // The whole loop is synchronous, so the header is re-read on each redraw.
    let header = |_: &Value| render_header(ctx);

    menu::show_menu_with_back(
        "📡 RustRouter Terminal UI",
        &base_path,
        "← Back to Interface Menu",
        0,
        || Some(Value::Null),
        header,
        &mut items,
    );
    Ok(())
}

/// `renderHeader(port, keys, tunnel)` with the tunnel branches dropped.
fn render_header(ctx: &Ctx) -> String {
    let keys_result = ctx.api.get_api_keys();
    let keys: Vec<Value> = if keys_result.success {
        keys_result
            .data
            .get("keys")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut lines = vec![format!("Endpoint: http://localhost:{}/v1", ctx.port)];
    if keys.is_empty() {
        lines.push(format!(
            "Key:      {}No API keys yet{}",
            colors::DIM,
            colors::RESET
        ));
    } else {
        for (i, key) in keys.iter().enumerate() {
            let value = key.get("key").and_then(Value::as_str).unwrap_or("");
            let label = if i == 0 { "Key:      " } else { "          " };
            lines.push(format!("{label}{}{value}{}", colors::CYAN, colors::RESET));
        }
    }
    lines.join("\n")
}
