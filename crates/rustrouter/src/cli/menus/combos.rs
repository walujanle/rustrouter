//! The Combos menu: list, create, edit and delete model-fallback chains.

use serde_json::{Value, json};

use crate::cli::menu::{self, ListMenu};
use crate::cli::term::{self, StatusKind};
use crate::cli::{Ctx, truncate};

/// `formatModel`: a string, or the `id`/`name`/`provider/model` of an object.
fn format_model(model: &Value) -> String {
    if let Some(s) = model.as_str() {
        return s.to_string();
    }
    if let Some(obj) = model.as_object() {
        if let Some(id) = obj.get("id").and_then(Value::as_str) {
            return id.to_string();
        }
        if let Some(name) = obj.get("name").and_then(Value::as_str) {
            return name.to_string();
        }
        if let (Some(provider), Some(m)) = (
            obj.get("provider").and_then(Value::as_str),
            obj.get("model").and_then(Value::as_str),
        ) {
            return format!("{provider}/{m}");
        }
        return model.to_string();
    }
    model.to_string()
}

fn models_chain(combo: &Value) -> String {
    combo
        .get("models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .map(format_model)
                .collect::<Vec<_>>()
                .join(" → ")
        })
        .unwrap_or_default()
}

pub fn show(ctx: &Ctx, breadcrumb: &[String]) {
    let on_select = |combo: &Value| show_combo_actions(ctx, combo, breadcrumb);
    let create = || handle_create_combo(ctx);

    let mut list = ListMenu {
        title: "🔀 Combos Management",
        breadcrumb,
        back_label: "← Back",
        header: Box::new(|_| String::new()),
        fetch_items: Box::new(|| {
            let result = ctx.api.get_combos();
            if !result.success {
                term::clear_screen();
                term::show_status(
                    &format!("Failed to load combos: {}", result.error),
                    StatusKind::Error,
                );
                term::pause("Press Enter to continue...");
                return None;
            }
            let items = result
                .data
                .get("combos")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Some((items, Value::Null))
        }),
        format_item: Box::new(|combo| {
            let name = combo.get("name").and_then(Value::as_str).unwrap_or("");
            format!("{name}: {}", truncate(&models_chain(combo), 35))
        }),
        on_select: Box::new(move |combo| on_select(combo)),
        create_action: Some(("Create New Combo".to_string(), Box::new(create))),
    };
    menu::show_list_menu(&mut list);
}

fn show_combo_actions(ctx: &Ctx, combo: &Value, breadcrumb: &[String]) {
    let name = combo
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let chain = models_chain(combo);
    let header = format!("Name: {name}\nModels: {chain}");

    let edit = |_: &Value| {
        handle_edit_single_combo(ctx, combo);
        true
    };
    let delete = |_: &Value| {
        handle_delete_single_combo(ctx, combo);
        false
    };

    let mut items = vec![
        menu::Entry::action(|_| "Edit Combo".to_string(), move |v| edit(v)),
        menu::Entry::action(|_| "Delete Combo".to_string(), move |v| delete(v)),
    ];
    let mut crumb = breadcrumb.to_vec();
    crumb.push(name.clone());
    menu::show_menu_with_back(
        &format!("🔀 {name}"),
        &crumb,
        "← Back",
        0,
        || Some(Value::Null),
        move |_| header.clone(),
        &mut items,
    );
}

fn handle_create_combo(ctx: &Ctx) {
    term::clear_screen();
    term::show_status("Create New Combo", StatusKind::Info);
    println!();

    let name = term::prompt("Combo name: ");
    if name.is_empty() {
        term::show_status("Combo name is required", StatusKind::Error);
        term::pause("Press Enter to continue...");
        return;
    }

    term::show_status("Loading available models...", StatusKind::Info);
    let models_result = ctx.api.get_models();
    if !models_result.success {
        term::show_status(
            &format!("Failed to load models: {}", models_result.error),
            StatusKind::Error,
        );
        term::pause("Press Enter to continue...");
        return;
    }
    let available = models_result
        .data
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if available.is_empty() {
        term::show_status(
            "No models available. Please add providers first.",
            StatusKind::Warning,
        );
        term::pause("Press Enter to continue...");
        return;
    }

    let label = |m: &Value| {
        let provider = m.get("provider").and_then(Value::as_str).unwrap_or("");
        let model = m.get("model").and_then(Value::as_str).unwrap_or("");
        format!("{provider}/{model}")
    };

    let mut selected: Vec<Value> = Vec::new();
    println!();
    term::show_status("Select models for the chain (minimum 2)", StatusKind::Info);

    loop {
        term::clear_screen();
        println!("Creating combo: {name}");
        println!("Selected models ({}):", selected.len());
        if selected.is_empty() {
            println!("  (none)");
        } else {
            for (i, m) in selected.iter().enumerate() {
                println!("  {}. {}", i + 1, label(m));
            }
        }
        println!();
        println!("Available models:");
        for (i, m) in available.iter().enumerate() {
            println!("  {}. {}", i + 1, label(m));
        }
        println!();
        println!("Actions:");
        println!("  - Enter number to add model");
        println!("  - Type 'done' to finish (min 2 models)");
        println!("  - Type 'cancel' to abort");

        let input = term::prompt("\nAction: ");
        let lower = input.trim().to_lowercase();
        if lower == "cancel" {
            term::show_status("Cancelled", StatusKind::Warning);
            term::pause("Press Enter to continue...");
            return;
        }
        if lower == "done" {
            if selected.len() < 2 {
                term::show_status("Please select at least 2 models", StatusKind::Error);
                term::pause("Press Enter to continue...");
                continue;
            }
            break;
        }
        match input.trim().parse::<usize>() {
            Ok(num) if num >= 1 && num <= available.len() => {
                selected.push(available[num - 1].clone());
            }
            _ => {
                term::show_status("Invalid model number", StatusKind::Error);
                term::pause("Press Enter to continue...");
            }
        }
    }

    term::show_status("Creating combo...", StatusKind::Info);
    let result = ctx
        .api
        .create_combo(&json!({ "name": name, "models": selected }));
    if !result.success {
        term::show_status(
            &format!("Failed to create combo: {}", result.error),
            StatusKind::Error,
        );
    } else {
        term::show_status(
            &format!("Combo \"{name}\" created successfully!"),
            StatusKind::Success,
        );
    }
    term::pause("Press Enter to continue...");
}

fn handle_edit_single_combo(ctx: &Ctx, combo: &Value) {
    term::clear_screen();
    let name = combo.get("name").and_then(Value::as_str).unwrap_or("");
    println!("\n✏️  Edit Combo: {name}\n");

    let new_name = term::prompt(&format!("New name (Enter to keep \"{name}\"): "));
    let final_name = if new_name.is_empty() {
        name.to_string()
    } else {
        new_name
    };

    println!("\nCurrent models: {}", models_chain(combo));
    println!("\nSelect models for this combo (add one by one):");

    let mut models: Vec<String> = Vec::new();
    loop {
        let current_chain = if models.is_empty() {
            "None".to_string()
        } else {
            models.join(" → ")
        };
        let title = format!("Add Model #{}", models.len() + 1);
        let Some(model) = crate::cli::model_selector::select_model_from_list(
            &ctx.api,
            &title,
            &format!("Chain: {current_chain}"),
            false,
        ) else {
            break;
        };
        println!("\n✓ Added: {model}");
        models.push(model);
        println!("Current chain: {}\n", models.join(" → "));
        if !term::confirm("Add another model?") {
            break;
        }
    }

    // New models if any were added, otherwise the existing chain.
    let final_models: Value = if models.is_empty() {
        combo.get("models").cloned().unwrap_or(json!([]))
    } else {
        json!(models)
    };

    let id = combo.get("id").and_then(Value::as_str).unwrap_or("");
    let result = ctx
        .api
        .update_combo(id, &json!({ "name": final_name, "models": final_models }));
    if result.success {
        term::show_status("Combo updated!", StatusKind::Success);
    } else {
        term::show_status(
            &format!("Update failed: {}", result.error),
            StatusKind::Error,
        );
    }
    term::pause("Press Enter to continue...");
}

fn handle_delete_single_combo(ctx: &Ctx, combo: &Value) {
    let name = combo.get("name").and_then(Value::as_str).unwrap_or("");
    if term::confirm(&format!("Delete combo \"{name}\"?")) {
        let id = combo.get("id").and_then(Value::as_str).unwrap_or("");
        let result = ctx.api.delete_combo(id);
        if result.success {
            term::show_status("Combo deleted!", StatusKind::Success);
        } else {
            term::show_status(
                &format!("Delete failed: {}", result.error),
                StatusKind::Error,
            );
        }
        term::pause("Press Enter to continue...");
    }
}
