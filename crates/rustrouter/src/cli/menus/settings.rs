//! The Settings menu: the RTK (Token Saver) toggle, the password reset and the
//! auth-mode reset.

use serde_json::{Value, json};

use crate::cli::menu;
use crate::cli::term::{self, StatusKind};
use crate::cli::{Ctx, colors};

const DEFAULT_PASSWORD: &str = "123456";

pub fn show(ctx: &Ctx, breadcrumb: &[String]) {
    let header = |data: &Value| {
        let settings = data.get("settings").cloned().unwrap_or(json!({}));
        let rtk_on = settings.get("rtkEnabled") != Some(&Value::Bool(false));
        let auth_mode = settings
            .get("authMode")
            .and_then(Value::as_str)
            .unwrap_or("password");
        let auth_color = if auth_mode == "password" {
            colors::GREEN
        } else {
            colors::YELLOW
        };
        format!(
            "  Endpoint: http://localhost:{}/v1\n  RTK:      {}{}{} {}(Token Saver){}\n  Auth:     {auth_color}{}{} {}(login mode){}",
            ctx.port,
            if rtk_on { colors::GREEN } else { colors::RED },
            if rtk_on { "ON" } else { "OFF" },
            colors::RESET,
            colors::DIM,
            colors::RESET,
            auth_mode.to_uppercase(),
            colors::RESET,
            colors::DIM,
            colors::RESET,
        )
    };

    let rtk = |data: &Value| {
        let on =
            data.get("settings").and_then(|s| s.get("rtkEnabled")) != Some(&Value::Bool(false));
        toggle_rtk(ctx, on);
        true
    };
    let reset_pw = |_: &Value| {
        reset_password(ctx);
        true
    };
    let reset_auth = |_: &Value| {
        reset_auth_mode(ctx);
        true
    };

    let mut items = vec![
        menu::Entry::action(
            |d| {
                let on = d.get("settings").and_then(|s| s.get("rtkEnabled"))
                    != Some(&Value::Bool(false));
                format!(
                    "Token Saver (RTK): {} → toggle",
                    if on { "ON" } else { "OFF" }
                )
            },
            move |d| rtk(d),
        ),
        menu::Entry::action(
            |_| "🔑 Reset Password to Default".to_string(),
            move |d| reset_pw(d),
        ),
        menu::Entry::action(
            |d| {
                let mode = d
                    .get("settings")
                    .and_then(|s| s.get("authMode"))
                    .and_then(Value::as_str)
                    .unwrap_or("password");
                if mode == "password" {
                    "🔓 Reset Auth Mode (already password)".to_string()
                } else {
                    format!("🔓 Reset Auth Mode to Password (current: {mode})")
                }
            },
            move |d| reset_auth(d),
        ),
    ];

    menu::show_menu_with_back(
        "⚙️  Settings",
        breadcrumb,
        "← Back",
        0,
        || {
            let settings = ctx.api.get_settings();
            Some(json!({
                "settings": if settings.success { settings.data } else { json!({}) },
            }))
        },
        header,
        &mut items,
    );
}

fn toggle_rtk(ctx: &Ctx, currently_on: bool) {
    let next = !currently_on;
    let result = ctx.api.update_settings(&json!({ "rtkEnabled": next }));
    if result.success {
        term::show_status(
            &format!("Token Saver {}", if next { "enabled" } else { "disabled" }),
            StatusKind::Success,
        );
    } else {
        term::show_status(&format!("Failed: {}", result.error), StatusKind::Error);
    }
    term::pause("Press Enter to continue...");
}

/// Reset `authMode` to password. The CLI bypasses auth via the CLI token, so
/// this is the escape hatch when OIDC is misconfigured and the dashboard is
/// locked out.
fn reset_auth_mode(ctx: &Ctx) {
    if !term::confirm("Reset auth mode to PASSWORD (disable OIDC)?") {
        term::show_status("Cancelled", StatusKind::Info);
        term::pause("Press Enter to continue...");
        return;
    }
    let result = ctx.api.update_settings(&json!({ "authMode": "password" }));
    if result.success {
        term::show_status(
            "Auth mode reset to password. OIDC disabled.",
            StatusKind::Success,
        );
    } else {
        term::show_status(&format!("Failed: {}", result.error), StatusKind::Error);
    }
    term::pause("Press Enter to continue...");
}

fn reset_password(ctx: &Ctx) {
    if !term::confirm(&format!(
        "Reset dashboard password to default \"{DEFAULT_PASSWORD}\"?"
    )) {
        term::show_status("Cancelled", StatusKind::Info);
        term::pause("Press Enter to continue...");
        return;
    }
    let result = ctx.api.reset_password();
    if result.success {
        term::show_status(
            &format!("Password reset. Default: {DEFAULT_PASSWORD}"),
            StatusKind::Success,
        );
    } else {
        term::show_status(
            &format!("Failed to reset password: {}", result.error),
            StatusKind::Error,
        );
    }
    term::pause("Press Enter to continue...");
}
