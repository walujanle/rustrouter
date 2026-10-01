//! The API Keys menu: list, create, copy and delete keys, all against the
//! local server's `/api/keys` routes.

use serde_json::Value;

use crate::cli::menu::{self, ListMenu};
use crate::cli::term::{self, StatusKind};
use crate::cli::{Ctx, copy_to_clipboard, format_date, mask_key, relative_time};

pub fn show(ctx: &Ctx, breadcrumb: &[String]) {
    let endpoint = format!("http://localhost:{}/v1", ctx.port);

    let on_select = |key: &Value| show_key_actions(ctx, key, breadcrumb);
    let create = || handle_create_key(ctx);

    let mut list = ListMenu {
        title: "🔑 API Keys Management",
        breadcrumb,
        back_label: "← Back",
        header: Box::new({
            let endpoint = endpoint.clone();
            move |_| format!("Endpoint: {endpoint}")
        }),
        fetch_items: Box::new(|| {
            let result = ctx.api.get_api_keys();
            if !result.success {
                term::clear_screen();
                term::show_status(
                    &format!("Failed to fetch API keys: {}", result.error),
                    StatusKind::Error,
                );
                term::pause("Press Enter to continue...");
                return None;
            }
            let items = result
                .data
                .get("keys")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Some((items, Value::Null))
        }),
        format_item: Box::new(|key| {
            let name = key.get("name").and_then(Value::as_str).unwrap_or("");
            let masked = mask_key(key.get("key").and_then(Value::as_str).unwrap_or(""));
            format!("{name} ({masked})")
        }),
        on_select: Box::new(move |key| on_select(key)),
        create_action: Some(("Create New API Key".to_string(), Box::new(create))),
    };
    menu::show_list_menu(&mut list);
}

fn show_key_actions(ctx: &Ctx, key: &Value, breadcrumb: &[String]) {
    let name = key
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let full_key = key
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let endpoint = format!("http://localhost:{}/v1", ctx.port);

    let copy = |_: &Value| {
        handle_copy_key(key);
        true
    };
    let delete = |_: &Value| {
        handle_delete_key(ctx, key);
        false
    };

    let mut items = vec![
        menu::Entry::action(|_| "Copy to Clipboard".to_string(), move |v| copy(v)),
        menu::Entry::action(|_| "Delete Key".to_string(), move |v| delete(v)),
    ];

    let header = format!("Name: {name}\nKey: {full_key}\nEndpoint: {endpoint}");
    let mut crumb = breadcrumb.to_vec();
    crumb.push(name.clone());
    menu::show_menu_with_back(
        &format!("🔑 {name}"),
        &crumb,
        "← Back",
        0,
        || Some(Value::Null),
        move |_| header.clone(),
        &mut items,
    );
}

fn handle_create_key(ctx: &Ctx) {
    println!("\n📝 Create New API Key");
    println!("{}", "─".repeat(30));

    let name = term::prompt("Enter key name: ");
    if name.is_empty() {
        term::show_status("Key name cannot be empty", StatusKind::Error);
        term::pause("Press Enter to continue...");
        return;
    }

    let result = ctx.api.create_api_key(&name);
    if !result.success {
        term::show_status(
            &format!("Failed to create key: {}", result.error),
            StatusKind::Error,
        );
        term::pause("Press Enter to continue...");
        return;
    }

    let key = result.data.get("key").and_then(Value::as_str).unwrap_or("");
    println!("\n✅ API Key created successfully!");
    println!("\n⚠️  IMPORTANT: Save this key now. You won't be able to see it again!");
    println!("\nKey: {key}");
    println!(
        "Name: {}",
        result
            .data
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
    );
    println!(
        "ID: {}",
        result.data.get("id").and_then(Value::as_str).unwrap_or("")
    );

    if term::confirm("\nCopy key to clipboard?") {
        if copy_to_clipboard(key) {
            term::show_status("Key copied to clipboard!", StatusKind::Success);
        } else {
            term::show_status("Failed to copy to clipboard", StatusKind::Error);
        }
    }
    term::pause("Press Enter to continue...");
}

fn handle_copy_key(key: &Value) {
    let name = key.get("name").and_then(Value::as_str).unwrap_or("");
    let full = key.get("key").and_then(Value::as_str).unwrap_or("");
    if copy_to_clipboard(full) {
        term::show_status(
            &format!("Key \"{name}\" copied to clipboard!"),
            StatusKind::Success,
        );
    } else {
        term::show_status("Failed to copy to clipboard", StatusKind::Error);
    }
    term::pause("Press Enter to continue...");
}

fn handle_delete_key(ctx: &Ctx, key: &Value) {
    let name = key.get("name").and_then(Value::as_str).unwrap_or("");
    let id = key.get("id").and_then(Value::as_str).unwrap_or("");
    println!("\n⚠️  Delete API Key: {name}");
    println!("{}", "─".repeat(30));
    println!(
        "Key: {}",
        mask_key(key.get("key").and_then(Value::as_str).unwrap_or(""))
    );
    println!(
        "Created: {}",
        format_date(key.get("createdAt").and_then(Value::as_str))
    );

    if !term::confirm("\nAre you sure you want to delete this key?") {
        term::show_status("Deletion cancelled", StatusKind::Info);
        term::pause("Press Enter to continue...");
        return;
    }

    let result = ctx.api.delete_api_key(id);
    if !result.success {
        term::show_status(
            &format!("Failed to delete key: {}", result.error),
            StatusKind::Error,
        );
    } else {
        term::show_status("API key deleted successfully", StatusKind::Success);
    }
    term::pause("Press Enter to continue...");
}

/// Unused today, kept so `relative_time` has a caller; it renders a key's
/// last-used timestamp in the same shape the key list shows.
#[allow(dead_code)]
fn last_used(key: &Value) -> String {
    match key.get("lastUsedAt").and_then(Value::as_str) {
        Some(raw) => relative_time(Some(raw)),
        None => "Never".to_string(),
    }
}
