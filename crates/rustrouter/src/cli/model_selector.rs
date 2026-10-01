//! Pick a model from the live catalog.
//!
//! Models are grouped by `/v1/models`' `owned_by` (an alias, or `combo`),
//! filtered to aliases with an active connection (plus the no-auth providers),
//! and offered through a category list, a search, and a free-text custom id.
//! Combos are a category of their own unless `exclude_combos` drops them.
//!
//! The provider metadata — the alias for a provider id, the display name, the
//! no-auth set — comes from `/api/registry`, the single source for it.

use serde_json::Value;

use super::api::Api;
use super::term;

/// The alias sort key. Aliases not in this list sort last, in the order the
/// catalog returns them.
const ALIAS_ORDER: &[&str] = &["cx", "oc", "openrouter"];

/// The registry-derived facts the selector needs.
struct ProviderIndex {
    /// provider id → alias.
    id_to_alias: Vec<(String, String)>,
    /// alias → display name.
    alias_to_name: Vec<(String, String)>,
    /// aliases of providers usable with no stored credential.
    no_auth_aliases: Vec<String>,
}

impl ProviderIndex {
    fn from_registry(registry: &Value) -> Self {
        let mut id_to_alias = Vec::new();
        let mut alias_to_name = Vec::new();
        let mut no_auth_aliases = Vec::new();

        if let Some(map) = registry.get("providerIdToAlias").and_then(Value::as_object) {
            for (id, alias) in map {
                if let Some(alias) = alias.as_str() {
                    id_to_alias.push((id.clone(), alias.to_string()));
                }
            }
        }

        if let Some(providers) = registry.get("providers").and_then(Value::as_object) {
            for category in providers.values() {
                let Some(entries) = category.as_object() else {
                    continue;
                };
                for (id, entry) in entries {
                    let alias = entry
                        .get("alias")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_string();
                    if let Some(name) = entry.get("name").and_then(Value::as_str) {
                        alias_to_name.push((alias.clone(), name.to_string()));
                    }
                    if entry.get("noAuth") == Some(&Value::Bool(true)) {
                        no_auth_aliases.push(alias.clone());
                        // The id is a usable alias too.
                        no_auth_aliases.push(id.clone());
                    }
                }
            }
        }

        Self {
            id_to_alias,
            alias_to_name,
            no_auth_aliases,
        }
    }

    fn alias_for(&self, provider_id: &str) -> String {
        self.id_to_alias
            .iter()
            .find(|(id, _)| id == provider_id)
            .map(|(_, alias)| alias.clone())
            .unwrap_or_else(|| provider_id.to_string())
    }

    fn name_for(&self, alias: &str) -> String {
        self.alias_to_name
            .iter()
            .find(|(a, _)| a == alias)
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| alias.to_string())
    }
}

/// `getAvailableModelsGrouped`: combos and alias-grouped models, filtered to
/// active connections.
fn grouped(api: &Api, index: &ProviderIndex) -> (Vec<String>, Vec<(String, Vec<String>)>) {
    let models = api.get_available_models();
    let providers = api.get_providers();
    if !models.success {
        return (Vec::new(), Vec::new());
    }

    let connections: Vec<Value> = if providers.success {
        providers
            .data
            .get("connections")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut active: Vec<String> = index.no_auth_aliases.clone();
    for conn in &connections {
        if conn.get("isActive") == Some(&Value::Bool(false)) {
            continue;
        }
        let Some(provider) = conn.get("provider").and_then(Value::as_str) else {
            continue;
        };
        active.push(provider.to_string());
        let prefix = conn
            .get("providerSpecificData")
            .and_then(|p| p.get("prefix"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        active.push(prefix.unwrap_or_else(|| index.alias_for(provider)));
    }

    let mut combos = Vec::new();
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    let list = models
        .data
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for model in list {
        let Some(id) = model.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(owner) = model.get("owned_by").and_then(Value::as_str) else {
            continue;
        };
        if owner == "combo" {
            combos.push(id.to_string());
            continue;
        }
        if !active.iter().any(|a| a == owner) {
            continue;
        }
        match groups.iter_mut().find(|(alias, _)| alias == owner) {
            Some((_, models)) => models.push(id.to_string()),
            None => groups.push((owner.to_string(), vec![id.to_string()])),
        }
    }

    // Alias-order sort; unknown aliases keep their arrival order.
    groups.sort_by_key(|(alias, _)| {
        ALIAS_ORDER
            .iter()
            .position(|a| a == alias)
            .unwrap_or(ALIAS_ORDER.len())
    });

    (combos, groups)
}

/// `selectModelFromList`.
pub fn select_model_from_list(
    api: &Api,
    title: &str,
    current: &str,
    exclude_combos: bool,
) -> Option<String> {
    let registry = api.get_registry();
    let index = ProviderIndex::from_registry(&registry.data);
    let (combos, groups) = grouped(api, &index);
    let combos = if exclude_combos { Vec::new() } else { combos };

    let all_models: Vec<String> = combos
        .iter()
        .cloned()
        .chain(groups.iter().flat_map(|(_, m)| m.iter().cloned()))
        .collect();

    if all_models.is_empty() {
        term::clear_screen();
        println!("\n🎯 {title}");
        println!("{}", "=".repeat(50));
        println!("\n  No connected providers found.");
        println!("  Please connect a provider in Providers menu first.\n");
        println!("  m. ✍️  Enter custom model ID");
        println!("  0. Cancel\n");
        let action = term::prompt("Select option (m/0): ");
        if action.trim().eq_ignore_ascii_case("m") {
            let custom = term::prompt("Enter custom model ID: ");
            return non_empty(custom);
        }
        return None;
    }

    // Categories: combos first, then providers in alias order.
    let mut categories: Vec<(String, Vec<String>)> = Vec::new();
    if !combos.is_empty() {
        categories.push(("[Combos]".to_string(), combos.clone()));
    }
    for (alias, models) in &groups {
        categories.push((index.name_for(alias), models.clone()));
    }

    let mut filter: Option<String> = None;

    loop {
        term::clear_screen();
        println!("\n🎯 {title}");
        println!("{}", "=".repeat(50));
        if current.is_empty() {
            println!();
        } else {
            println!("Current: {current}\n");
        }

        if let Some(query) = &filter {
            let q = query.trim().to_lowercase();
            let matched: Vec<&String> = all_models
                .iter()
                .filter(|m| m.to_lowercase().contains(&q))
                .collect();
            println!(
                "🔍 Search results for \"{query}\": ({} found)\n",
                matched.len()
            );
            if matched.is_empty() {
                println!("  No matching models found.\n");
                println!("  0. ← Back to providers");
                println!("  s. Search again\n");
                let act = term::prompt("Select option: ");
                if act.trim().eq_ignore_ascii_case("s") {
                    filter = non_empty(term::prompt("Enter search keyword: "));
                } else {
                    filter = None;
                }
                continue;
            }
            for (i, m) in matched.iter().enumerate() {
                println!("  {}. {}", i + 1, m);
            }
            println!("\n  0. ← Back to providers");
            println!("  s. Search again\n");
            let input = term::prompt("Enter number to select (or 0/s): ");
            if input.trim().eq_ignore_ascii_case("s") {
                filter = non_empty(term::prompt("Enter search keyword: "));
                continue;
            }
            let num: i64 = input.trim().parse().unwrap_or(0);
            if num == 0 {
                filter = None;
                continue;
            }
            if num > 0 && (num as usize) <= matched.len() {
                return Some(matched[num as usize - 1].clone());
            }
            continue;
        }

        // A single category goes straight to its model list.
        if categories.len() == 1 {
            let (name, models) = &categories[0];
            println!("[{name}]");
            for (i, m) in models.iter().enumerate() {
                println!("  {}. {}", i + 1, m);
            }
            println!();
            println!("  s. 🔍 Search models");
            println!("  m. ✍️  Enter custom model ID");
            println!("  0. Cancel\n");
            let input = term::prompt("Enter choice (number / s / m / 0): ");
            let trimmed = input.trim();
            if trimmed.is_empty() || trimmed == "0" {
                return None;
            }
            let lower = trimmed.to_lowercase();
            if lower == "s" {
                filter = non_empty(term::prompt("Enter search keyword: "));
                continue;
            }
            if lower == "m" {
                if let Some(custom) = non_empty(term::prompt("Enter custom model ID: ")) {
                    return Some(custom);
                }
                continue;
            }
            if let Ok(num) = trimmed.parse::<usize>()
                && num > 0
                && num <= models.len()
            {
                return Some(models[num - 1].clone());
            }
            filter = Some(trimmed.to_string());
            continue;
        }

        println!("[Providers & Groups]");
        for (i, (name, models)) in categories.iter().enumerate() {
            println!("  {}. {} ({} models)", i + 1, name, models.len());
        }
        println!();
        println!("  s. 🔍 Search models");
        println!("  m. ✍️  Enter custom model ID");
        println!("  0. Cancel\n");
        let input = term::prompt("Enter choice (number / keyword / s / m): ");
        let trimmed = input.trim();
        if trimmed.is_empty() || trimmed == "0" {
            return None;
        }
        let lower = trimmed.to_lowercase();
        if lower == "s" {
            filter = non_empty(term::prompt("Enter search keyword: "));
            continue;
        }
        if lower == "m" {
            if let Some(custom) = non_empty(term::prompt("Enter custom model ID: ")) {
                return Some(custom);
            }
            continue;
        }
        if let Ok(num) = trimmed.parse::<usize>()
            && num > 0
            && num <= categories.len()
        {
            let (name, models) = &categories[num - 1];
            term::clear_screen();
            println!("\n🎯 {title} > {name}");
            println!("{}", "=".repeat(50));
            if current.is_empty() {
                println!();
            } else {
                println!("Current: {current}\n");
            }
            for (i, m) in models.iter().enumerate() {
                println!("  {}. {}", i + 1, m);
            }
            println!("\n  0. ← Back\n");
            let choice = term::prompt("Enter number to select (0 to back): ");
            if let Ok(n) = choice.trim().parse::<usize>()
                && n > 0
                && n <= models.len()
            {
                return Some(models[n - 1].clone());
            }
            continue;
        }
        // Any other text is a search query.
        filter = Some(trimmed.to_string());
    }
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// `getFirstApiKey`: the first key, or `None`.
pub fn first_api_key(api: &Api) -> Option<String> {
    let result = api.get_api_keys();
    if !result.success {
        return None;
    }
    result
        .data
        .get("keys")
        .and_then(Value::as_array)
        .and_then(|keys| keys.first())
        .and_then(|k| k.get("key"))
        .and_then(Value::as_str)
        .map(str::to_string)
}
