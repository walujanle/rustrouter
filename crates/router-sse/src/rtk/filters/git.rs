//! The git output filters: diff, status and log.

use std::sync::LazyLock;

use regex::Regex;

use super::{GIT_DIFF_HUNK_MAX_LINES, GIT_LOG_MAX_LINES, STATUS_MAX_FILES, STATUS_MAX_UNTRACKED};

/// Compress a `git diff`. The line cap is fixed at 500 — the pipeline never
/// passes a different one.
pub fn git_diff(diff: &str) -> String {
    let max_lines = 500usize;
    let mut result: Vec<String> = Vec::new();
    let mut current_file = String::new();
    let mut added = 0u64;
    let mut removed = 0u64;
    let mut in_hunk = false;
    let mut hunk_shown = 0usize;
    let mut hunk_skipped = 0usize;
    let mut was_truncated = false;
    let max_hunk_lines = GIT_DIFF_HUNK_MAX_LINES;

    for line in diff.split('\n') {
        if line.starts_with("diff --git") {
            if hunk_skipped > 0 {
                result.push(format!("  ... ({hunk_skipped} lines truncated)"));
                was_truncated = true;
                hunk_skipped = 0;
            }
            if !current_file.is_empty() && (added > 0 || removed > 0) {
                result.push(format!("  +{added} -{removed}"));
            }
            let parts: Vec<&str> = line.split(" b/").collect();
            current_file = if parts.len() > 1 {
                parts[1..].join(" b/")
            } else {
                "unknown".to_string()
            };
            result.push(format!("\n{current_file}"));
            added = 0;
            removed = 0;
            in_hunk = false;
            hunk_shown = 0;
        } else if line.starts_with("@@") {
            if hunk_skipped > 0 {
                result.push(format!("  ... ({hunk_skipped} lines truncated)"));
                was_truncated = true;
                hunk_skipped = 0;
            }
            in_hunk = true;
            hunk_shown = 0;
            result.push(format!("  {line}"));
        } else if in_hunk {
            if line.starts_with('+') && !line.starts_with("+++") {
                added += 1;
                if hunk_shown < max_hunk_lines {
                    result.push(format!("  {line}"));
                    hunk_shown += 1;
                } else {
                    hunk_skipped += 1;
                }
            } else if line.starts_with('-') && !line.starts_with("---") {
                removed += 1;
                if hunk_shown < max_hunk_lines {
                    result.push(format!("  {line}"));
                    hunk_shown += 1;
                } else {
                    hunk_skipped += 1;
                }
            } else if hunk_shown < max_hunk_lines && hunk_shown > 0 && !line.starts_with('\\') {
                result.push(format!("  {line}"));
                hunk_shown += 1;
            }
        }

        if result.len() >= max_lines {
            result.push("\n... (more changes truncated)".to_string());
            was_truncated = true;
            break;
        }
    }

    if hunk_skipped > 0 {
        result.push(format!("  ... ({hunk_skipped} lines truncated)"));
        was_truncated = true;
    }
    if !current_file.is_empty() && (added > 0 || removed > 0) {
        result.push(format!("  +{added} -{removed}"));
    }
    if was_truncated {
        result.push("[full diff: rtk git diff --no-compact]".to_string());
    }

    result.join("\n")
}

static LONG_BRANCH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^On branch (\S+)").expect("static pattern"));
static PORCELAIN_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[ MADRCU?!][ MADRCU?!] ").expect("static pattern"));
static LONG_STATUS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(modified|new file|deleted|renamed|both modified):\s+(.+)$")
        .expect("static pattern")
});

/// `gitStatus(input)`.
pub fn git_status(input: &str) -> String {
    let lines: Vec<&str> = input.split('\n').collect();
    if lines.is_empty() || (lines.len() == 1 && lines[0].trim().is_empty()) {
        return "Clean working tree".to_string();
    }

    let mut branch = String::new();
    let mut staged_files: Vec<String> = Vec::new();
    let mut modified_files: Vec<String> = Vec::new();
    let mut untracked_files: Vec<String> = Vec::new();
    let mut staged = 0usize;
    let mut modified = 0usize;
    let mut untracked = 0usize;
    let mut conflicts = 0usize;

    for raw in &lines {
        if raw.trim().is_empty() {
            continue;
        }

        if let Some(caps) = LONG_BRANCH_RE.captures(raw) {
            branch = caps[1].to_string();
            continue;
        }
        if raw.starts_with("##") {
            branch = raw.trim_start_matches('#').trim_start().to_string();
            continue;
        }

        if raw.chars().count() >= 3 && PORCELAIN_HEADER_RE.is_match(raw) {
            let bytes: Vec<char> = raw.chars().collect();
            let x = bytes[0];
            let y = bytes[1];
            let file: String = bytes[3..].iter().collect();
            let xy: String = bytes[..2].iter().collect();

            if xy == "??" {
                untracked += 1;
                untracked_files.push(file);
                continue;
            }
            if "MADRC".contains(x) {
                staged += 1;
                staged_files.push(file.clone());
            } else if x == 'U' {
                conflicts += 1;
            }
            if y == 'M' || y == 'D' {
                modified += 1;
                modified_files.push(file);
            }
            continue;
        }

        if let Some(caps) = LONG_STATUS_RE.captures(raw) {
            let kind = &caps[1];
            let path = caps[2].trim().to_string();
            match kind {
                "both modified" => conflicts += 1,
                "modified" | "deleted" => {
                    modified += 1;
                    modified_files.push(path);
                }
                "new file" | "renamed" => {
                    staged += 1;
                    staged_files.push(path);
                }
                _ => {}
            }
        }
    }

    let mut out = String::new();
    if !branch.is_empty() {
        out.push_str(&format!("* {branch}\n"));
    }
    if staged > 0 {
        out.push_str(&format!("+ Staged: {staged} files\n"));
        for f in staged_files.iter().take(STATUS_MAX_FILES) {
            out.push_str(&format!("   {f}\n"));
        }
        if staged_files.len() > STATUS_MAX_FILES {
            out.push_str(&format!(
                "   ... +{} more\n",
                staged_files.len() - STATUS_MAX_FILES
            ));
        }
    }
    if modified > 0 {
        out.push_str(&format!("~ Modified: {modified} files\n"));
        for f in modified_files.iter().take(STATUS_MAX_FILES) {
            out.push_str(&format!("   {f}\n"));
        }
        if modified_files.len() > STATUS_MAX_FILES {
            out.push_str(&format!(
                "   ... +{} more\n",
                modified_files.len() - STATUS_MAX_FILES
            ));
        }
    }
    if untracked > 0 {
        out.push_str(&format!("? Untracked: {untracked} files\n"));
        for f in untracked_files.iter().take(STATUS_MAX_UNTRACKED) {
            out.push_str(&format!("   {f}\n"));
        }
        if untracked_files.len() > STATUS_MAX_UNTRACKED {
            out.push_str(&format!(
                "   ... +{} more\n",
                untracked_files.len() - STATUS_MAX_UNTRACKED
            ));
        }
    }
    if conflicts > 0 {
        out.push_str(&format!("conflicts: {conflicts} files\n"));
    }
    if staged == 0 && modified == 0 && untracked == 0 && conflicts == 0 {
        out.push_str("clean — nothing to commit\n");
    }

    out.trim_end_matches('\n').to_string()
}

/// `gitLog(text, maxLines = GIT_LOG_MAX_LINES)`.
pub fn git_log(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let input = text;
    let max_lines = GIT_LOG_MAX_LINES;
    let mut out: Vec<String> = Vec::new();
    let mut skipped = 0usize;
    let mut in_commit = false;
    let mut subject_seen = false;

    fn commit_header(trimmed: &str) -> bool {
        static PLAIN: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?i)^commit [0-9a-f]{7,40}$").expect("static pattern"));
        static GRAPH: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?i)^[*|/\\ ]+commit [0-9a-f]{7,40}").expect("static pattern")
        });
        PLAIN.is_match(trimmed) || GRAPH.is_match(trimmed)
    }

    for raw in input.split('\n') {
        let line = raw.trim_end();
        let trimmed = line.trim();

        if commit_header(trimmed) {
            in_commit = true;
            subject_seen = false;
            if out.len() < max_lines {
                out.push(line.to_string());
            } else {
                skipped += 1;
            }
            continue;
        }

        if in_commit {
            static AUTHOR_RE: LazyLock<Regex> = LazyLock::new(|| {
                Regex::new(r"(?i)^[*|/\\ ]*(Author|Date):").expect("static pattern")
            });
            if AUTHOR_RE.is_match(trimmed) {
                if out.len() < max_lines {
                    out.push(trimmed.to_string());
                } else {
                    skipped += 1;
                }
                continue;
            }
            if trimmed.is_empty() {
                continue;
            }
            static SUBJECT_RE: LazyLock<Regex> =
                LazyLock::new(|| Regex::new(r"^[*|/\\ ]*    \S").expect("static pattern"));
            if !subject_seen && SUBJECT_RE.is_match(line) {
                if out.len() < max_lines {
                    out.push(format!("  Subject: {trimmed}"));
                } else {
                    skipped += 1;
                }
                subject_seen = true;
                continue;
            }
            static STAT_RE: LazyLock<Regex> =
                LazyLock::new(|| Regex::new(r"^\d+ file\w* changed").expect("static pattern"));
            if STAT_RE.is_match(trimmed) {
                if out.len() < max_lines {
                    out.push(format!("  {trimmed}"));
                } else {
                    skipped += 1;
                }
                continue;
            }
            if trimmed.starts_with("diff --git ") {
                if out.len() < max_lines {
                    out.push("  ... diff body omitted".to_string());
                } else {
                    skipped += 1;
                }
                continue;
            }
            continue;
        }

        static GRAPH_SHA_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?i)^[*|/\\ ]+([0-9a-f]{7,40}\s+.+)").expect("static pattern")
        });
        if let Some(caps) = GRAPH_SHA_RE.captures(trimmed) {
            if out.len() < max_lines {
                out.push(caps[1].to_string());
            } else {
                skipped += 1;
            }
            continue;
        }

        static PLAIN_SHA_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?i)^[0-9a-f]{7,40}\s+").expect("static pattern"));
        if PLAIN_SHA_RE.is_match(trimmed) {
            if out.len() < max_lines {
                out.push(trimmed.to_string());
            } else {
                skipped += 1;
            }
            continue;
        }

        static PURE_GRAPH_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"^[*|/\\ ]+$").expect("static pattern"));
        static HAS_GRAPH_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"[*|/\\]").expect("static pattern"));
        if PURE_GRAPH_RE.is_match(trimmed) && HAS_GRAPH_RE.is_match(trimmed) {
            continue;
        }

        if out.len() < max_lines {
            out.push(trimmed.to_string());
        } else {
            skipped += 1;
        }
    }

    if skipped > 0 {
        out.push(format!("... ({skipped} more lines)"));
    }
    let result = out.join("\n");
    if result.is_empty() && !input.is_empty() {
        return input.to_string();
    }
    if result.len() > input.len() {
        return input.to_string();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_diff() -> String {
        let mut lines: Vec<String> = vec![
            "diff --git a/foo.js b/foo.js".into(),
            "index abc..def 100644".into(),
            "--- a/foo.js".into(),
            "+++ b/foo.js".into(),
            "@@ -1,3 +1,200 @@".into(),
        ];
        for i in 0..200 {
            lines.push(format!("+added line {i} {}", "x".repeat(20)));
        }
        lines.join("\n")
    }

    #[test]
    fn git_diff_truncates_hunks_and_keeps_file_header() {
        let input = long_diff();
        let out = git_diff(&input);
        assert!(out.contains("foo.js"));
        assert!(out.contains("lines truncated"));
        assert!(out.len() < input.len());
    }

    #[test]
    fn git_status_groups_by_kind() {
        let input = [
            "On branch main",
            "Your branch is up to date with 'origin/main'.",
            "",
            "Changes not staged for commit:",
            "  (use \"git add <file>...\" to update what will be committed)",
            "\tmodified:   src/a.js",
            "\tmodified:   src/b.js",
            "\tnew file:   src/c.js",
            "\tdeleted:    src/old.js",
            "",
            "Untracked files:",
            "\tnotes.txt",
            "",
            "no changes added to commit",
        ]
        .join("\n");
        let out = git_status(&input);
        assert!(out.contains("* main"));
        assert!(out.contains("~ Modified: 3 files"), "got: {out}");
        assert!(out.contains("src/a.js"));
        assert!(out.contains("+ Staged: 1 files"));
        assert!(out.len() < input.len());
    }

    #[test]
    fn git_log_oneline_keeps_subjects() {
        let input =
            "abc1234 Add auth middleware\ndef5678 Fix token refresh race\nfedcba9 Update docs";
        let out = git_log(input);
        assert!(out.contains("abc1234"));
        assert!(out.contains("Add auth middleware"));
        assert!(out.len() <= input.len());
    }

    #[test]
    fn git_log_default_drops_body() {
        let input = [
            "commit abc1234def5678abc1234def5678abc1234def5",
            "Author: Dev One <dev1@example.com>",
            "Date:   Sun Jul 6 10:00:00 2026 +0700",
            "",
            "    Add auth middleware",
            "",
            "    More body detail should be dropped.",
            "    This is padding that consumes tokens.",
        ]
        .join("\n");
        let out = git_log(&input);
        assert!(out.contains("commit abc1234def5678abc1234def5678abc1234def5"));
        assert!(out.contains("Add auth middleware"));
        assert!(!out.contains("More body detail should be dropped."));
    }

    #[test]
    fn git_log_empty_is_empty() {
        assert_eq!(git_log(""), "");
    }
}
