//! The twelve output filters and the name→function registry.
//!
//! Every filter is total — any input produces a string, no panics. `apply_filter`
//! still wraps the call in `catch_unwind`, because the contract is fail-open: a
//! filter that panics must degrade to passthrough, not kill the request.

use std::panic::{AssertUnwindSafe, catch_unwind};

pub mod git;
pub mod misc;
pub mod search;

pub use git::{git_diff, git_log, git_status};
pub use misc::{build_output, dedup_log, is_line_numbered_line, read_numbered, smart_truncate};
pub use search::{find, grep, ls, search_list, tree};

/// Caps are measured in UTF-16 code units, not Rust bytes, so the length
/// comparisons in `compress_text` go through `u16len` rather than `str::len` or
/// `chars().count()`.
pub const RAW_CAP: usize = 10 * 1024 * 1024;
pub const MIN_COMPRESS_SIZE: usize = 500;
pub const DETECT_WINDOW: usize = 1024;
pub const GIT_DIFF_HUNK_MAX_LINES: usize = 100;
pub const GIT_LOG_MAX_LINES: usize = 200;
pub const DEDUP_LINE_MAX: usize = 2000;
pub const GREP_PER_FILE_MAX: usize = 10;
pub const FIND_PER_DIR_MAX: usize = 10;
pub const FIND_TOTAL_DIR_MAX: usize = 20;
pub const STATUS_MAX_FILES: usize = 10;
pub const STATUS_MAX_UNTRACKED: usize = 10;
pub const LS_EXT_SUMMARY_TOP: usize = 5;
pub const TREE_MAX_LINES: usize = 200;
pub const SEARCH_LIST_PER_DIR_MAX: usize = 10;
pub const SEARCH_LIST_TOTAL_DIR_MAX: usize = 20;
pub const SMART_TRUNCATE_HEAD: usize = 120;
pub const SMART_TRUNCATE_TAIL: usize = 60;
pub const SMART_TRUNCATE_MIN_LINES: usize = 250;
pub const READ_NUMBERED_MIN_HIT_RATIO: f64 = 0.7;

pub const LS_NOISE_DIRS: [&str; 25] = [
    "node_modules",
    ".git",
    "target",
    "__pycache__",
    ".next",
    "dist",
    "build",
    ".cache",
    ".turbo",
    ".vercel",
    ".pytest_cache",
    ".mypy_cache",
    ".tox",
    ".venv",
    "venv",
    "env",
    "coverage",
    ".nyc_output",
    ".DS_Store",
    "Thumbs.db",
    ".idea",
    ".vscode",
    ".vs",
    "*.egg-info",
    ".eggs",
];

/// The exact name strings the registry is keyed by.
pub mod names {
    pub const GIT_DIFF: &str = "git-diff";
    pub const GIT_STATUS: &str = "git-status";
    pub const GIT_LOG: &str = "git-log";
    pub const GREP: &str = "grep";
    pub const FIND: &str = "find";
    pub const LS: &str = "ls";
    pub const TREE: &str = "tree";
    pub const DEDUP_LOG: &str = "dedup-log";
    pub const SMART_TRUNCATE: &str = "smart-truncate";
    pub const READ_NUMBERED: &str = "read-numbered";
    pub const SEARCH_LIST: &str = "search-list";
    pub const BUILD_OUTPUT: &str = "build-output";
}

/// A filter is a plain function; the registry supplies the name.
pub type Filter = fn(&str) -> String;

/// The 12 registry entries plus the `grep|rg` and `find|fd` aliases.
pub fn resolve_filter(name: &str) -> Option<Filter> {
    match name {
        names::GIT_DIFF => Some(git_diff),
        names::GIT_STATUS => Some(git_status),
        names::GIT_LOG => Some(git_log),
        names::GREP | "rg" => Some(grep),
        names::FIND | "fd" => Some(find),
        names::LS => Some(ls),
        names::TREE => Some(tree),
        names::DEDUP_LOG => Some(dedup_log),
        names::SMART_TRUNCATE => Some(smart_truncate),
        names::READ_NUMBERED => Some(read_numbered),
        names::SEARCH_LIST => Some(search_list),
        names::BUILD_OUTPUT => Some(build_output),
        _ => None,
    }
}

/// Never panic, return the input unchanged on any problem — including a filter
/// that panics.
pub fn apply_filter(name: &str, text: &str) -> String {
    let Some(filter) = resolve_filter(name) else {
        return text.to_string();
    };
    match catch_unwind(AssertUnwindSafe(|| filter(text))) {
        Ok(out) => out,
        Err(_) => {
            // `catch_unwind` swallows the payload, so only the filter name is
            // available to report.
            eprintln!("[rtk] warning: filter '{name}' panicked — passing through raw output");
            text.to_string()
        }
    }
}

/// Apply an already-resolved filter, never panicking.
pub fn safe_apply(filter: Filter, text: &str) -> String {
    match catch_unwind(AssertUnwindSafe(|| filter(text))) {
        Ok(out) => out,
        Err(_) => {
            eprintln!("[rtk] warning: filter panicked — passing through raw output");
            text.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_all_twelve_and_aliases() {
        let names = [
            "git-diff",
            "git-status",
            "git-log",
            "grep",
            "find",
            "ls",
            "tree",
            "dedup-log",
            "smart-truncate",
            "read-numbered",
            "search-list",
            "build-output",
            "rg",
            "fd",
        ];
        for n in names {
            assert!(resolve_filter(n).is_some(), "missing filter {n}");
        }
        assert!(resolve_filter("nope").is_none());
    }

    #[test]
    fn apply_filter_passes_through_unknown_name() {
        assert_eq!(apply_filter("nope", "hello"), "hello");
    }

    #[test]
    fn apply_filter_runs_a_known_filter() {
        assert_eq!(apply_filter("git-diff", "diff --git a/x b/x"), "\nx");
    }
}
