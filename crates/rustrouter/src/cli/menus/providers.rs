//! The provider list, per-provider connections, the connection actions and the
//! custom provider-node section.
//!
//! The provider set and the model table both come from `GET /api/registry`,
//! which the server projects from the generated registry. A hardcoded list
//! would render rows with no backend behind them.
//!
//! The two group labels: `oauth` providers read "(OAuth)", everything else
//! reads "(API)".

use serde_json::{Map, Value, json};

use crate::cli::menu::{self, Entry, ListMenu};
use crate::cli::term::{self, StatusKind};
use crate::cli::{Ctx, colors, copy_to_clipboard};

/// The custom-node types and the OpenAI-compatible API types.
const CUSTOM_NODE_TYPES: [&str; 2] = ["openai-compatible", "anthropic-compatible"];
const OPENAI_API_TYPES: [&str; 2] = ["chat", "responses"];

/// One registry provider, flattened for the menu.
struct Row {
    id: String,
    name: String,
    alias: String,
    is_oauth: bool,
}

/// Read the provider rows out of `/api/registry`, in category order: oauth,
/// apikey, freeTier, free.
fn provider_rows(registry: &Value) -> Vec<Row> {
    let Some(categories) = registry.get("providers").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for category in ["oauth", "apikey", "freeTier", "free"] {
        let Some(entries) = categories.get(category).and_then(Value::as_object) else {
            continue;
        };
        for (id, entry) in entries {
            let name = entry
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_string();
            let alias = entry
                .get("alias")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_string();
            rows.push(Row {
                id: id.clone(),
                name,
                alias,
                is_oauth: category == "oauth",
            });
        }
    }
    rows
}

/// `buildProviderHeader`: the alias and the first five models for that alias.
fn provider_header(ctx: &Ctx, row: &Row) -> String {
    let registry = ctx.api.get_registry();
    let models = registry
        .data
        .get("providerModels")
        .and_then(|m| m.get(&row.alias))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut lines = vec![format!(
        "Alias: {}{}{}",
        colors::CYAN,
        row.alias,
        colors::RESET
    )];
    if models.is_empty() {
        lines.push(format!(
            "Models: {}No models configured{}",
            colors::DIM,
            colors::RESET
        ));
    } else {
        let shown: Vec<String> = models
            .iter()
            .take(5)
            .filter_map(|m| m.get("id").and_then(Value::as_str))
            .map(|id| format!("{}/{id}", row.alias))
            .collect();
        let more = if models.len() > 5 {
            format!(" (+{} more)", models.len() - 5)
        } else {
            String::new()
        };
        lines.push(format!(
            "Models: {}{}{}{more}{}",
            colors::DIM,
            shown.join(", "),
            colors::RESET,
            colors::RESET
        ));
    }
    lines.join("\n")
}

/// `countConnectionsByProvider`.
fn connection_counts(connections: &[Value]) -> Map<String, Value> {
    let mut counts = Map::new();
    for conn in connections {
        let Some(provider) = conn
            .get("provider")
            .or_else(|| conn.get("providerId"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let next = counts.get(provider).and_then(Value::as_u64).unwrap_or(0) + 1;
        counts.insert(provider.to_string(), json!(next));
    }
    counts
}

pub fn show(ctx: &Ctx, breadcrumb: &[String]) {
    let registry = ctx.api.get_registry();
    let rows = provider_rows(&registry.data);

    let mut items: Vec<Entry<'_>> = Vec::new();
    for row in rows {
        let id = row.id.clone();
        let name = row.name.clone();
        let group = if row.is_oauth { "OAuth" } else { "API" };
        items.push(Entry::action(
            {
                let id = id.clone();
                let label_name = name.clone();
                move |data| {
                    let count = data
                        .get("counts")
                        .and_then(|c| c.get(&id))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    format!("{label_name} ({group}) - {count} Connected")
                }
            },
            {
                let id = id.clone();
                let name = name.clone();
                move |_| {
                    let mut crumb = breadcrumb.to_vec();
                    crumb.push(name.clone());
                    show_provider_detail(ctx, &id, &name, group == "OAuth", &crumb);
                    true
                }
            },
        ));
    }

    items.push(Entry::separator(format!(
        "{}── Custom Providers ──{}",
        colors::DIM,
        colors::RESET
    )));
    items.push(Entry::action(
        |data| {
            let count = data.get("nodeCount").and_then(Value::as_u64).unwrap_or(0);
            format!("Custom Providers - {count} Configured")
        },
        {
            let mut crumb = breadcrumb.to_vec();
            crumb.push("Custom Providers".to_string());
            move |_| {
                show_custom_providers(ctx, &crumb);
                true
            }
        },
    ));

    menu::show_menu_with_back(
        "🔌 Providers Management",
        breadcrumb,
        "← Back",
        0,
        || {
            let prov = ctx.api.get_providers();
            if !prov.success {
                term::clear_screen();
                term::show_status(
                    &format!("Failed to fetch providers: {}", prov.error),
                    StatusKind::Error,
                );
                term::pause("Press Enter to continue...");
                return None;
            }
            let connections = prov
                .data
                .get("connections")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let nodes = ctx.api.get_provider_nodes();
            let node_count = nodes
                .data
                .get("nodes")
                .and_then(Value::as_array)
                .map(|n| n.len())
                .unwrap_or(0);
            Some(json!({
                "connections": connections,
                "counts": Value::Object(connection_counts(&connections)),
                "nodeCount": node_count,
            }))
        },
        |_| String::new(),
        &mut items,
    );
}

/// `showProviderDetail`.
fn show_provider_detail(
    ctx: &Ctx,
    provider_id: &str,
    name: &str,
    is_oauth: bool,
    breadcrumb: &[String],
) {
    let title = format!("🔌 {name} ({})", if is_oauth { "OAUTH" } else { "API" });
    let header = {
        let registry = ctx.api.get_registry();
        let row = Row {
            id: provider_id.to_string(),
            name: name.to_string(),
            alias: registry
                .data
                .get("providerIdToAlias")
                .and_then(|m| m.get(provider_id))
                .and_then(Value::as_str)
                .unwrap_or(provider_id)
                .to_string(),
            is_oauth,
        };
        provider_header(ctx, &row)
    };

    let on_select = |conn: &Value| show_connection_actions(ctx, conn, breadcrumb);
    let create = || {
        if is_oauth {
            handle_add_oauth(ctx, provider_id, name);
        } else {
            handle_add_api_key(ctx, provider_id, name);
        }
    };

    let mut list = ListMenu {
        title: &title,
        breadcrumb,
        back_label: "← Back to Providers",
        header: Box::new(move |_| header.clone()),
        fetch_items: Box::new({
            let provider_id = provider_id.to_string();
            move || {
                let response = ctx.api.get_providers();
                let all = response
                    .data
                    .get("connections")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let items: Vec<Value> = all
                    .into_iter()
                    .filter(|c| {
                        c.get("provider")
                            .or_else(|| c.get("providerId"))
                            .and_then(Value::as_str)
                            == Some(provider_id.as_str())
                    })
                    .collect();
                Some((items, Value::Null))
            }
        }),
        format_item: Box::new(|conn| {
            let status = match conn.get("testStatus").and_then(Value::as_str) {
                Some("active") => "✓",
                Some("error") => "✗",
                _ => "?",
            };
            format!("{} ({status})", connection_name(conn))
        }),
        on_select: Box::new(move |conn| on_select(conn)),
        create_action: Some(("Add New Connection".to_string(), Box::new(create))),
    };
    menu::show_list_menu(&mut list);
}

/// `conn.name || conn.email || conn.displayName || "Unnamed"`.
fn connection_name(conn: &Value) -> String {
    for key in ["name", "email", "displayName"] {
        if let Some(v) = conn
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return v.to_string();
        }
    }
    "Unnamed".to_string()
}

/// `showConnectionActions`.
fn show_connection_actions(ctx: &Ctx, conn: &Value, breadcrumb: &[String]) {
    let name = connection_name(conn);
    let status = match conn.get("testStatus").and_then(Value::as_str) {
        Some("active") => "✓ Active",
        Some("error") => "✗ Error",
        _ => "? Unknown",
    };
    let header = format!("Connection: {name}\nStatus: {status}");
    let id = conn
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let rename = |_: &Value| {
        let new_name = term::prompt(&format!("New name (current: {name}): "));
        if !new_name.trim().is_empty() {
            term::show_status("Renaming connection...", StatusKind::Info);
            let result = ctx
                .api
                .update_connection(&id, &json!({ "name": new_name.trim() }));
            if result.success {
                term::show_status("Connection renamed!", StatusKind::Success);
            } else {
                term::show_status(
                    &format!("Rename failed: {}", result.error),
                    StatusKind::Error,
                );
            }
            term::pause("Press Enter to continue...");
        }
        true
    };
    let test = |_: &Value| {
        term::show_status("Testing connection...", StatusKind::Info);
        let result = ctx.api.test_provider(&id);
        if result.success {
            term::show_status("Connection is working!", StatusKind::Success);
        } else {
            term::show_status(&format!("Test failed: {}", result.error), StatusKind::Error);
        }
        term::pause("Press Enter to continue...");
        true
    };
    let delete = |_: &Value| {
        if term::confirm(&format!("Delete connection \"{name}\"?")) {
            let result = ctx.api.delete_provider(&id);
            if result.success {
                term::show_status("Connection deleted!", StatusKind::Success);
            } else {
                term::show_status(
                    &format!("Delete failed: {}", result.error),
                    StatusKind::Error,
                );
            }
            term::pause("Press Enter to continue...");
            return false;
        }
        true
    };

    let mut items = vec![
        Entry::action(|_| "Rename Connection".to_string(), move |v| rename(v)),
        Entry::action(|_| "Test Connection".to_string(), move |v| test(v)),
        Entry::action(|_| "Delete Connection".to_string(), move |v| delete(v)),
    ];
    let mut crumb = breadcrumb.to_vec();
    crumb.push(name.clone());
    menu::show_menu_with_back(
        &format!("🔌 {name}"),
        &crumb,
        "← Back",
        0,
        || Some(Value::Null),
        move |_| header.clone(),
        &mut items,
    );
}

/// `handleAddApiKeyConnection`.
fn handle_add_api_key(ctx: &Ctx, provider_id: &str, provider_name: &str) {
    term::clear_screen();
    println!("\n➕ Add {provider_name} API Key Connection\n");

    let name = term::prompt("Connection Name: ");
    if name.is_empty() {
        term::show_status("Cancelled", StatusKind::Warning);
        term::pause("Press Enter to continue...");
        return;
    }
    let api_key = term::prompt("API Key: ");
    if api_key.is_empty() {
        term::show_status("Cancelled", StatusKind::Warning);
        term::pause("Press Enter to continue...");
        return;
    }

    term::show_status("Creating connection...", StatusKind::Info);
    let result = ctx.api.create_api_key_provider(
        &json!({ "provider": provider_id, "name": name, "apiKey": api_key }),
    );
    if result.success {
        term::show_status("✓ Connection created successfully!", StatusKind::Success);
    } else {
        term::show_status(&format!("✗ Failed: {}", result.error), StatusKind::Error);
    }
    term::pause("Press Enter to continue...");
}

/// `handleAddConnection`: device code for the device-code providers, the
/// authorization-code paste flow otherwise. The flow table lives in
/// `router_sse::services::oauth_flow`, so it is not duplicated here.
fn handle_add_oauth(ctx: &Ctx, provider_id: &str, provider_name: &str) {
    match router_sse::services::oauth_flow::flow_type(provider_id) {
        Some("device_code") => handle_add_device_code(ctx, provider_id, provider_name),
        _ => handle_add_auth_code(ctx, provider_id, provider_name),
    }
}

/// `handleAddOAuthConnection`: open the URL, paste the callback URL back.
fn handle_add_auth_code(ctx: &Ctx, provider_id: &str, provider_name: &str) {
    term::clear_screen();
    term::show_status("Requesting authorization URL...", StatusKind::Info);
    let auth_result = ctx.api.get_oauth_auth_url(provider_id, ctx.port);
    if !auth_result.success {
        term::show_status(&format!("Failed: {}", auth_result.error), StatusKind::Error);
        term::pause("Press Enter to continue...");
        return;
    }
    let data = auth_result.data;
    let auth_url = data.get("authUrl").and_then(Value::as_str).unwrap_or("");
    let code_verifier = data
        .get("codeVerifier")
        .and_then(Value::as_str)
        .unwrap_or("");
    let state = data.get("state").and_then(Value::as_str).unwrap_or("");
    let redirect_uri = data
        .get("redirectUri")
        .and_then(Value::as_str)
        .unwrap_or("");
    if auth_url.is_empty() {
        term::show_status("Failed: No auth URL received", StatusKind::Error);
        term::pause("Press Enter to continue...");
        return;
    }

    term::clear_screen();
    term::show_header(
        "🔐 OAuth Login",
        &format!("Providers > {provider_name} > Add Connection"),
    );
    println!(
        "  {}{}1.{} Open this URL in your browser:",
        colors::BRIGHT,
        colors::CYAN,
        colors::RESET
    );
    println!("     {}{auth_url}{}", colors::DIM, colors::RESET);
    if copy_to_clipboard(auth_url) {
        println!("     \x1b[32m✓ Link copied to clipboard!\x1b[0m");
    }
    println!();
    println!(
        "  {}{}2.{} Complete authorization in browser",
        colors::BRIGHT,
        colors::CYAN,
        colors::RESET
    );
    println!();
    println!(
        "  {}{}3.{} Copy the callback URL from address bar",
        colors::BRIGHT,
        colors::CYAN,
        colors::RESET
    );
    println!(
        "     {}(looks like: http://localhost:{}/callback?code=...){}",
        colors::DIM,
        ctx.port,
        colors::RESET
    );
    println!();

    let callback_url = term::prompt("  Paste callback URL: ");
    if callback_url.is_empty() {
        term::show_status("Cancelled", StatusKind::Warning);
        term::pause("Press Enter to continue...");
        return;
    }

    let Some((code, url_state, error)) = parse_callback(&callback_url) else {
        term::show_status("Invalid URL format", StatusKind::Error);
        term::pause("Press Enter to continue...");
        return;
    };
    if let Some(error) = error {
        term::show_status(&format!("Authorization failed: {error}"), StatusKind::Error);
        term::pause("Press Enter to continue...");
        return;
    }
    let Some(code) = code else {
        term::show_status("No authorization code found in URL", StatusKind::Error);
        term::pause("Press Enter to continue...");
        return;
    };

    println!();
    term::show_status("Exchanging code for tokens...", StatusKind::Info);
    let exchange = ctx.api.exchange_oauth_code(
        provider_id,
        &json!({
            "code": code,
            "redirectUri": redirect_uri,
            "codeVerifier": code_verifier,
            "state": if url_state.is_empty() { state } else { &url_state },
        }),
    );
    if exchange.success {
        term::show_status("Connection created successfully!", StatusKind::Success);
    } else {
        term::show_status(&format!("Failed: {}", exchange.error), StatusKind::Error);
    }
    term::pause("Press Enter to continue...");
}

/// Pull `code`, `state` and `error` out of a pasted callback URL. `None` when
/// the input is not a URL at all; the `error` slot carries the provider's
/// `error_description` when authorization was refused.
fn parse_callback(raw: &str) -> Option<(Option<String>, String, Option<String>)> {
    let url = url::Url::parse(raw.trim()).ok()?;
    let mut code = None;
    let mut state = String::new();
    let mut error = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.to_string()),
            "state" => state = v.to_string(),
            "error" => error = Some(v.to_string()),
            "error_description" if error.is_none() => error = Some(v.to_string()),
            _ => {}
        }
    }
    Some((code, state, error))
}

/// `handleAddDeviceCodeConnection`: request the code, poll until it lands.
fn handle_add_device_code(ctx: &Ctx, provider_id: &str, provider_name: &str) {
    term::clear_screen();
    term::show_status("Requesting device code...", StatusKind::Info);
    let device_result = ctx.api.get_oauth_device_code(provider_id);
    if !device_result.success {
        term::show_status(
            &format!("Failed: {}", device_result.error),
            StatusKind::Error,
        );
        term::pause("Press Enter to continue...");
        return;
    }
    let data = device_result.data;
    let device_code = data
        .get("device_code")
        .and_then(Value::as_str)
        .unwrap_or("");
    let user_code = data.get("user_code").and_then(Value::as_str).unwrap_or("");
    let verification_uri = data
        .get("verification_uri")
        .and_then(Value::as_str)
        .unwrap_or("");
    let complete = data
        .get("verification_uri_complete")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let code_verifier = data.get("codeVerifier").cloned().unwrap_or(Value::Null);
    if device_code.is_empty() {
        term::show_status("Failed: No device code received", StatusKind::Error);
        term::pause("Press Enter to continue...");
        return;
    }

    term::clear_screen();
    let device_url = complete.unwrap_or(verification_uri);
    term::show_header(
        "📱 Device Login",
        &format!("Providers > {provider_name} > Add Connection"),
    );
    println!(
        "  {}{}1.{} Open: {}{device_url}{}",
        colors::BRIGHT,
        colors::CYAN,
        colors::RESET,
        colors::DIM,
        colors::RESET
    );
    if copy_to_clipboard(device_url) {
        println!("     \x1b[32m✓ Link copied to clipboard!\x1b[0m");
    }
    println!();
    if complete.is_none() && !user_code.is_empty() {
        println!(
            "  {}{}2.{} Enter code: {}{user_code}{}",
            colors::BRIGHT,
            colors::CYAN,
            colors::RESET,
            colors::BRIGHT,
            colors::RESET
        );
        println!();
    }
    println!(
        "  {}Waiting for authorization...{}",
        colors::DIM,
        colors::RESET
    );
    println!();

    // 60 attempts at the provider's 5s interval.
    const MAX_ATTEMPTS: u32 = 60;
    for _ in 0..MAX_ATTEMPTS {
        std::thread::sleep(std::time::Duration::from_secs(5));
        let poll = ctx.api.poll_oauth_token(
            provider_id,
            &json!({
                "deviceCode": device_code,
                "codeVerifier": code_verifier,
                "extraData": data,
            }),
        );
        if poll.success {
            term::show_status("\nConnection created successfully!", StatusKind::Success);
            term::pause("Press Enter to continue...");
            return;
        }
        let pending = poll.data.get("pending").and_then(Value::as_bool) == Some(true)
            || poll.error == "authorization_pending"
            || poll.error == "slow_down";
        if !pending {
            term::show_status(
                &format!(
                    "\nFailed: {}",
                    if poll.error.is_empty() {
                        "Unknown error"
                    } else {
                        &poll.error
                    }
                ),
                StatusKind::Error,
            );
            term::pause("Press Enter to continue...");
            return;
        }
        print!(".");
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
    term::show_status("\nTimeout waiting for authorization", StatusKind::Error);
    term::pause("Press Enter to continue...");
}

// ─── Custom providers (provider nodes) ────────────────────────────────────

/// `showCustomProvidersMenu`.
fn show_custom_providers(ctx: &Ctx, breadcrumb: &[String]) {
    let on_select = |node: &Value| show_custom_node_detail(ctx, node, breadcrumb);
    let create = || handle_add_custom_node(ctx);

    let mut list = ListMenu {
        title: "🔧 Custom Providers",
        breadcrumb,
        back_label: "← Back to Providers",
        header: Box::new(|_| String::new()),
        fetch_items: Box::new(|| {
            let res = ctx.api.get_provider_nodes();
            if !res.success {
                return Some((Vec::new(), Value::Null));
            }
            let items = res
                .data
                .get("nodes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Some((items, Value::Null))
        }),
        format_item: Box::new(|node| {
            let prefix = node.get("prefix").and_then(Value::as_str).unwrap_or("");
            let name = node.get("name").and_then(Value::as_str).unwrap_or("");
            let kind = node.get("type").and_then(Value::as_str).unwrap_or("");
            format!("[{prefix}] {name} ({kind})")
        }),
        on_select: Box::new(move |node| on_select(node)),
        create_action: Some(("➕ Add Custom Provider".to_string(), Box::new(create))),
    };
    menu::show_list_menu(&mut list);
}

/// `showCustomNodeDetail`.
fn show_custom_node_detail(ctx: &Ctx, node: &Value, breadcrumb: &[String]) {
    let name = node
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let header = format!(
        "Type: {}\nPrefix: {}{}{}\nBase URL: {}{}{}",
        node.get("type").and_then(Value::as_str).unwrap_or(""),
        colors::CYAN,
        node.get("prefix").and_then(Value::as_str).unwrap_or(""),
        colors::RESET,
        colors::DIM,
        node.get("baseUrl").and_then(Value::as_str).unwrap_or(""),
        colors::RESET,
    );
    let node = node.clone();

    let connections = {
        let node = node.clone();
        move |_: &Value| {
            show_custom_node_connections(ctx, &node, breadcrumb);
            true
        }
    };
    let edit = {
        let node = node.clone();
        move |_: &Value| {
            handle_edit_custom_node(ctx, &node);
            true
        }
    };
    let delete = {
        let node = node.clone();
        move |_: &Value| {
            if term::confirm(&format!(
                "Delete \"{}\" and all its connections?",
                node.get("name").and_then(Value::as_str).unwrap_or("")
            )) {
                let id = node.get("id").and_then(Value::as_str).unwrap_or("");
                let res = ctx.api.delete_provider_node(id);
                if res.success {
                    term::show_status("Node deleted!", StatusKind::Success);
                } else {
                    term::show_status(&format!("Delete failed: {}", res.error), StatusKind::Error);
                }
                term::pause("Press Enter to continue...");
                return false;
            }
            true
        }
    };

    let mut items = vec![
        Entry::action(|_| "Connections".to_string(), move |v| connections(v)),
        Entry::action(|_| "Edit Node".to_string(), move |v| edit(v)),
        Entry::action(|_| "Delete Node".to_string(), move |v| delete(v)),
    ];
    let mut crumb = breadcrumb.to_vec();
    crumb.push(name.clone());
    menu::show_menu_with_back(
        &format!("🔧 {name}"),
        &crumb,
        "← Back",
        0,
        || Some(Value::Null),
        move |_| header.clone(),
        &mut items,
    );
}

/// `showCustomNodeConnections`.
fn show_custom_node_connections(ctx: &Ctx, node: &Value, breadcrumb: &[String]) {
    let name = node
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let node_id = node
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let title = format!("🔌 {name} – Connections");

    let on_select = |conn: &Value| show_connection_actions(ctx, conn, breadcrumb);
    let create = {
        let node = node.clone();
        move || handle_add_custom_node_connection(ctx, &node)
    };

    let mut list = ListMenu {
        title: &title,
        breadcrumb,
        back_label: "← Back",
        header: Box::new(|_| String::new()),
        fetch_items: Box::new({
            let node_id = node_id.clone();
            move || {
                let res = ctx.api.get_providers();
                let all = res
                    .data
                    .get("connections")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let items: Vec<Value> = all
                    .into_iter()
                    .filter(|c| c.get("provider").and_then(Value::as_str) == Some(node_id.as_str()))
                    .collect();
                Some((items, Value::Null))
            }
        }),
        format_item: Box::new(|conn| {
            let status = match conn.get("testStatus").and_then(Value::as_str) {
                Some("active") => "✓",
                Some("error") => "✗",
                _ => "?",
            };
            format!("{} ({status})", connection_name(conn))
        }),
        on_select: Box::new(move |conn| on_select(conn)),
        create_action: Some(("Add API Key Connection".to_string(), Box::new(create))),
    };
    menu::show_list_menu(&mut list);
}

/// `handleAddCustomNodeConnection`.
fn handle_add_custom_node_connection(ctx: &Ctx, node: &Value) {
    let name = node.get("name").and_then(Value::as_str).unwrap_or("");
    let node_id = node.get("id").and_then(Value::as_str).unwrap_or("");
    term::clear_screen();
    println!("\n➕ Add Connection to {name}\n");

    let conn_name = term::prompt("Connection Name: ");
    if conn_name.is_empty() {
        term::show_status("Cancelled", StatusKind::Warning);
        term::pause("Press Enter to continue...");
        return;
    }
    let api_key = term::prompt("API Key: ");
    if api_key.is_empty() {
        term::show_status("Cancelled", StatusKind::Warning);
        term::pause("Press Enter to continue...");
        return;
    }

    term::show_status("Creating connection...", StatusKind::Info);
    let res = ctx.api.create_api_key_provider(
        &json!({ "provider": node_id, "name": conn_name, "apiKey": api_key }),
    );
    if res.success {
        term::show_status("✓ Connection created!", StatusKind::Success);
    } else {
        term::show_status(&format!("✗ Failed: {}", res.error), StatusKind::Error);
    }
    term::pause("Press Enter to continue...");
}

/// `handleAddCustomNode`.
fn handle_add_custom_node(ctx: &Ctx) {
    term::clear_screen();
    println!("\n➕ Add Custom Provider\n");

    for (i, kind) in CUSTOM_NODE_TYPES.iter().enumerate() {
        println!("  {}. {kind}", i + 1);
    }
    println!();
    let type_idx = term::prompt("Type (1/2): ")
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1));
    let Some(node_type) = type_idx.and_then(|i| CUSTOM_NODE_TYPES.get(i)) else {
        term::show_status("Cancelled", StatusKind::Warning);
        term::pause("Press Enter to continue...");
        return;
    };

    let name = term::prompt("Name: ");
    if name.is_empty() {
        term::show_status("Cancelled", StatusKind::Warning);
        term::pause("Press Enter to continue...");
        return;
    }
    let prefix = term::prompt("Prefix (used in model IDs, e.g. myapi): ");
    if prefix.is_empty() {
        term::show_status("Cancelled", StatusKind::Warning);
        term::pause("Press Enter to continue...");
        return;
    }
    let base_url = term::prompt("Base URL (e.g. https://api.example.com/v1): ");
    if base_url.is_empty() {
        term::show_status("Cancelled", StatusKind::Warning);
        term::pause("Press Enter to continue...");
        return;
    }

    let mut body =
        json!({ "name": name, "prefix": prefix, "baseUrl": base_url, "type": node_type });
    if *node_type == "openai-compatible" {
        println!();
        for (i, api_type) in OPENAI_API_TYPES.iter().enumerate() {
            println!("  {}. {api_type}", i + 1);
        }
        println!();
        let api_type_idx = term::prompt("API Type (1/2, default 1): ")
            .parse::<usize>()
            .ok();
        let api_type = api_type_idx
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| OPENAI_API_TYPES.get(i))
            .copied()
            .unwrap_or("chat");
        body["apiType"] = json!(api_type);
    }

    term::show_status("Creating provider node...", StatusKind::Info);
    let res = ctx.api.create_provider_node(&body);
    if res.success {
        term::show_status("✓ Provider created!", StatusKind::Success);
    } else {
        term::show_status(&format!("✗ Failed: {}", res.error), StatusKind::Error);
    }
    term::pause("Press Enter to continue...");
}

/// `handleEditCustomNode`.
fn handle_edit_custom_node(ctx: &Ctx, node: &Value) {
    let name = node.get("name").and_then(Value::as_str).unwrap_or("");
    let base_url = node.get("baseUrl").and_then(Value::as_str).unwrap_or("");
    let prefix = node.get("prefix").and_then(Value::as_str).unwrap_or("");
    term::clear_screen();
    println!("\n✏️  Edit {name}\n");
    println!(
        "{}Leave blank to keep current value{}\n",
        colors::DIM,
        colors::RESET
    );

    let new_name = term::prompt(&format!("Name ({name}): "));
    let new_base = term::prompt(&format!("Base URL ({base_url}): "));
    let new_prefix = term::prompt(&format!("Prefix ({prefix}): "));

    let mut updates = Map::new();
    if !new_name.trim().is_empty() {
        updates.insert("name".into(), json!(new_name.trim()));
    }
    if !new_base.trim().is_empty() {
        updates.insert("baseUrl".into(), json!(new_base.trim()));
    }
    if !new_prefix.trim().is_empty() {
        updates.insert("prefix".into(), json!(new_prefix.trim()));
    }
    if updates.is_empty() {
        term::show_status("No changes", StatusKind::Warning);
        term::pause("Press Enter to continue...");
        return;
    }

    term::show_status("Updating...", StatusKind::Info);
    let id = node.get("id").and_then(Value::as_str).unwrap_or("");
    let res = ctx.api.update_provider_node(id, &Value::Object(updates));
    if res.success {
        term::show_status("✓ Updated!", StatusKind::Success);
    } else {
        term::show_status(&format!("✗ Failed: {}", res.error), StatusKind::Error);
    }
    term::pause("Press Enter to continue...");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_url_yields_code_and_state() {
        let parsed = parse_callback("http://localhost:20129/callback?code=abc&state=xyz")
            .expect("valid url");
        assert_eq!(parsed.0.as_deref(), Some("abc"));
        assert_eq!(parsed.1, "xyz");
        assert!(parsed.2.is_none());
    }

    #[test]
    fn callback_url_reports_provider_error() {
        let parsed = parse_callback("http://localhost:20129/callback?error=access_denied").unwrap();
        assert!(parsed.0.is_none());
        assert_eq!(parsed.2.as_deref(), Some("access_denied"));
    }

    #[test]
    fn non_url_input_is_rejected() {
        assert!(parse_callback("not a url").is_none());
    }

    #[test]
    fn counts_group_by_provider() {
        let conns = vec![
            json!({ "provider": "claude" }),
            json!({ "provider": "claude" }),
        ];
        let counts = connection_counts(&conns);
        assert_eq!(counts["claude"], json!(2));
    }
}
