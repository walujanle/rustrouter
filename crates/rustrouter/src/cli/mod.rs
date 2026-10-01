//! The interactive Terminal UI.
//!
//! It drives the running server over HTTP with an `x-9r-cli-token` header, in
//! the same process as the server. The token is derived from the machine id and
//! secret file, so no auth dance is needed.
//!
//! Provider metadata (names, aliases, categories, the model table) comes from
//! `GET /api/registry`, which the server projects from the generated registry —
//! the same source the dashboard reads. Hardcoding a provider set would render a
//! menu with no backend behind it.

pub mod api;
pub mod interface;
pub mod menu;
pub mod menus;
pub mod model_selector;
pub mod term;
pub mod tui;

use std::process::{Command, Stdio};

/// ANSI colours used by every menu and prompt.
pub mod colors {
    pub const RESET: &str = "\x1b[0m";
    pub const BRIGHT: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const REVERSE: &str = "\x1b[7m";
    pub const GREEN: &str = "\x1b[32m";
    pub const RED: &str = "\x1b[31m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const CYAN: &str = "\x1b[36m";
    pub const TERRACOTTA: &str = "\x1b[38;2;217;119;87m";
}

/// The server context every menu needs: the HTTP client and the port the
/// server is listening on (used for endpoints and OAuth redirect URIs).
pub struct Ctx {
    pub api: api::Api,
    pub port: u16,
}

/// First four chars, `*` for the middle, last four. `***` under 8 chars.
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() < 8 {
        return "***".to_string();
    }
    let first: String = chars[..4].iter().collect();
    let last: String = chars[chars.len() - 4..].iter().collect();
    format!("{first}{}{last}", "*".repeat(chars.len() - 8))
}

/// `truncate`: cut to `max_len`, ending in `...`.
pub fn truncate(text: &str, max_len: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_len {
        return text.to_string();
    }
    let keep: String = chars[..max_len.saturating_sub(3)].iter().collect();
    format!("{keep}...")
}

/// `YYYY-MM-DD HH:MM:SS` in local time, or `Invalid Date`.
pub fn format_date(input: Option<&str>) -> String {
    let Some(raw) = input else {
        return "Invalid Date".to_string();
    };
    match chrono::DateTime::parse_from_rfc3339(raw) {
        Ok(dt) => dt
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        Err(_) => "Invalid Date".to_string(),
    }
}

/// A coarse "N units ago" label against the wall clock.
pub fn relative_time(input: Option<&str>) -> String {
    let Some(raw) = input else {
        return "Invalid Date".to_string();
    };
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) else {
        return "Invalid Date".to_string();
    };
    let diff = chrono::Utc::now().signed_duration_since(dt.with_timezone(&chrono::Utc));
    let sec = diff.num_seconds().max(0);
    let min = sec / 60;
    let hour = min / 60;
    let day = hour / 24;
    let month = day / 30;
    let year = day / 365;
    let plural = |n: i64, unit: &str| format!("{n} {unit}{} ago", if n > 1 { "s" } else { "" });
    if sec < 60 {
        "just now".to_string()
    } else if min < 60 {
        plural(min, "minute")
    } else if hour < 24 {
        plural(hour, "hour")
    } else if day < 30 {
        plural(day, "day")
    } else if month < 12 {
        plural(month, "month")
    } else {
        plural(year, "year")
    }
}

/// Copy text to the system clipboard via the platform's tool.
pub fn copy_to_clipboard(text: &str) -> bool {
    #[cfg(target_os = "windows")]
    let (program, args): (&str, &[&str]) = ("clip", &[]);
    #[cfg(target_os = "macos")]
    let (program, args): (&str, &[&str]) = ("pbcopy", &[]);
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let (program, args): (&str, &[&str]) = ("xclip", &["-selection", "clipboard"]);

    use std::io::Write;
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(stdin) = child.stdin.as_mut()
        && stdin.write_all(text.as_bytes()).is_err()
    {
        return false;
    }
    child.wait().is_ok_and(|s| s.success())
}

/// Hand a URL to the platform's opener.
pub fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    {
        // `start` is a cmd builtin; the empty title arg keeps a quoted URL from
        // being read as the window title.
        let _ = Command::new("cmd")
            .args(["/c", "start", "", url])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(url).spawn();
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_key_matches_the_expected_shape() {
        assert_eq!(mask_key("sk-1234567890"), "sk-1*****7890");
        assert_eq!(mask_key("short"), "***");
        assert_eq!(mask_key(""), "***");
    }

    #[test]
    fn truncate_appends_ellipsis_only_when_long() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdefgh", 5), "ab...");
    }

    #[test]
    fn relative_time_handles_bad_input() {
        assert_eq!(relative_time(None), "Invalid Date");
        assert_eq!(relative_time(Some("nonsense")), "Invalid Date");
    }
}
