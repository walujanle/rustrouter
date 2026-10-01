//! Pick a filter from the shape of the text.
//!
//! Detection order is load-bearing: build-output is checked before the
//! porcelain check so a cargo `Compiling` line is not misread as git-status.

use std::sync::LazyLock;

use regex::Regex;

use super::filters::{
    DETECT_WINDOW, READ_NUMBERED_MIN_HIT_RATIO, SMART_TRUNCATE_MIN_LINES, is_line_numbered_line,
    names, search::SEARCH_LIST_HEADER_RE,
};

static RE_GIT_DIFF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^diff --git ").expect("static pattern"));
static RE_GIT_DIFF_HUNK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^@@ ").expect("static pattern"));
static RE_GIT_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^On branch |^nothing to commit|^Changes (not |to be )|^Untracked files:")
        .expect("static pattern")
});
static RE_GIT_LOG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^[*|/\\ ]*commit [0-9a-f]{7,40}$").expect("static pattern"));
static RE_PORCELAIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^[ MADRCU?!][ MADRCU?!] \S").expect("static pattern"));
static RE_BUILD_OUTPUT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^(npm (warn|error|ERR!)|yarn (warn|error)|\s*Compiling\s+\S+|\s*Downloading\s+\S+|added \d+ package|\[ERROR\]|BUILD (SUCCESS|FAILED)|\s*Finished\s+|Successfully (installed|built)|ERROR:)")
        .expect("static pattern")
});
static RE_TREE_GLYPH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[├└]──|│  ").expect("static pattern"));
static RE_LS_ROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^[-dlbcps][rwx-]{9}").expect("static pattern"));
static RE_LS_TOTAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^total \d+$").expect("static pattern"));

/// `autoDetectFilter(text)` → filter name, or `None`.
pub fn auto_detect_filter(text: &str) -> Option<&'static str> {
    let head: String = if u16len(text) > DETECT_WINDOW {
        text.chars().take(DETECT_WINDOW).collect()
    } else {
        text.to_string()
    };

    if RE_GIT_LOG.is_match(&head) {
        return Some(names::GIT_LOG);
    }
    if RE_GIT_DIFF.is_match(&head) || RE_GIT_DIFF_HUNK.is_match(&head) {
        return Some(names::GIT_DIFF);
    }
    if RE_GIT_STATUS.is_match(&head) {
        return Some(names::GIT_STATUS);
    }
    if RE_BUILD_OUTPUT.is_match(&head) {
        return Some(names::BUILD_OUTPUT);
    }
    if is_mostly_porcelain(&head) {
        return Some(names::GIT_STATUS);
    }

    let lines: Vec<&str> = head.split('\n').collect();
    let non_empty: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| !l.trim().is_empty())
        .collect();

    if non_empty.iter().take(5).any(|l| is_grep_line(l)) {
        return Some(names::GREP);
    }
    if non_empty.len() >= 3 && non_empty.iter().all(|l| is_path_like(l)) {
        return Some(names::FIND);
    }
    if RE_TREE_GLYPH.is_match(&head) {
        return Some(names::TREE);
    }
    if RE_LS_TOTAL.is_match(&head) || count_matches(&head, &RE_LS_ROW) >= 3 {
        return Some(names::LS);
    }
    if SEARCH_LIST_HEADER_RE.is_match(&head) {
        return Some(names::SEARCH_LIST);
    }
    if lines.len() >= SMART_TRUNCATE_MIN_LINES && is_line_numbered(&lines) {
        return Some(names::READ_NUMBERED);
    }
    if non_empty.len() >= 5 {
        return Some(names::DEDUP_LOG);
    }
    if text.split('\n').count() >= SMART_TRUNCATE_MIN_LINES {
        return Some(names::SMART_TRUNCATE);
    }
    None
}

/// `isGrepLine`: `file:number:content`.
fn is_grep_line(line: &str) -> bool {
    let Some(first) = line.find(':') else {
        return false;
    };
    let Some(second_rel) = line[first + 1..].find(':') else {
        return false;
    };
    let second = first + 1 + second_rel;
    let lineno = &line[first + 1..second];
    !lineno.is_empty() && lineno.bytes().all(|b| b.is_ascii_digit())
}

/// `isPathLike`. A drive-letter prefix marks a Windows absolute path, so the
/// whole line is path-like; other colons disqualify it (grep-style dumps).
fn is_path_like(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    let bytes = t.as_bytes();
    let drive = bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes.get(1) == Some(&b':')
        && matches!(bytes.get(2), Some(b'\\') | Some(b'/'));
    if drive {
        return true;
    }
    if t.contains(':') {
        return false;
    }
    t.starts_with('.') || t.starts_with('/') || t.contains('/')
}

/// `isMostlyPorcelain`: >=60% of non-empty lines match the porcelain row.
fn is_mostly_porcelain(head: &str) -> bool {
    let lines: Vec<&str> = head.split('\n').filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < 3 {
        return false;
    }
    let hits = lines.iter().filter(|l| RE_PORCELAIN.is_match(l)).count();
    hits as f64 / lines.len() as f64 >= 0.6
}

/// `isLineNumbered`: of the first 100 non-empty lines, >=70% look numbered.
fn is_line_numbered(lines: &[&str]) -> bool {
    let mut hits = 0usize;
    let mut non_empty = 0usize;
    for l in lines.iter().take(100) {
        if l.is_empty() {
            continue;
        }
        non_empty += 1;
        if is_line_numbered_line(l) {
            hits += 1;
        }
    }
    if non_empty < 5 {
        return false;
    }
    hits as f64 / non_empty as f64 >= READ_NUMBERED_MIN_HIT_RATIO
}

fn count_matches(text: &str, re: &Regex) -> usize {
    re.find_iter(text).count()
}

/// JS `.length` counts UTF-16 code units. The detect window is 1024 of them.
pub(crate) fn u16len(s: &str) -> usize {
    s.encode_utf16().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_each_shape() {
        assert_eq!(
            auto_detect_filter("diff --git a/x b/x\n@@ -1 +1 @@\n+a"),
            Some("git-diff")
        );
        assert_eq!(
            auto_detect_filter("On branch main\n  modified:   x.js\n"),
            Some("git-status")
        );
        assert_eq!(
            auto_detect_filter("a.js:1:hello\nb.js:2:world\nc.js:3:foo"),
            Some("grep")
        );
        assert_eq!(
            auto_detect_filter("./a/b.js\n./a/c.js\n./a/d.js"),
            Some("find")
        );
        let log = [
            "commit abc1234def5678abc1234def5678abc1234def5",
            "Author: Dev One <dev1@example.com>",
            "Date:   Sun Jul 6 10:00:00 2026 +0700",
            "",
            "    Add auth middleware",
        ]
        .join("\n");
        assert_eq!(auto_detect_filter(&log), Some("git-log"));
        assert_eq!(
            auto_detect_filter("line1\nline2\nline3\nline4\nline5\nline6\n"),
            Some("dedup-log")
        );
    }

    #[test]
    fn detects_tree_ls_and_search_list() {
        assert_eq!(
            auto_detect_filter(".\n├── src\n│   └── main.rs\n└── Cargo.toml\n"),
            Some("tree")
        );
        let ls = [
            "total 48",
            "drwxr-xr-x  2 user staff   64 Jan  1 12:00 src",
            "-rw-r--r--  1 user staff 1234 Jan  1 12:00 main.js",
            "-rw-r--r--  1 user staff 5678 Jan  1 12:00 README.md",
        ]
        .join("\n");
        assert_eq!(auto_detect_filter(&ls), Some("ls"));
        assert_eq!(
            auto_detect_filter(
                "Result of search in '/x' (total 3 files):\n- a/b.js\n- a/c.js\n- a/d.js"
            ),
            Some("search-list")
        );
    }

    #[test]
    fn windows_paths_detect_as_find() {
        let win = "C:\\Users\\me\\project\\src\\a.js\nC:\\Users\\me\\project\\src\\b.js\nC:\\Users\\me\\project\\src\\c.js";
        assert_eq!(auto_detect_filter(win), Some("find"));
        let unix = "./src/a.js\n./src/b.js\n./src/c.js";
        assert_eq!(auto_detect_filter(unix), Some("find"));
    }

    #[test]
    fn build_output_wins_over_porcelain() {
        let cargo = "   Compiling foo v0.1.0\n   Compiling bar v0.1.0\n    Finished dev profile";
        assert_eq!(auto_detect_filter(cargo), Some("build-output"));
    }
}
