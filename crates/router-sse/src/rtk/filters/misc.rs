//! The log, truncation, numbered-read and build-output filters.

use std::sync::LazyLock;

use regex::Regex;

use super::{DEDUP_LINE_MAX, SMART_TRUNCATE_HEAD, SMART_TRUNCATE_MIN_LINES, SMART_TRUNCATE_TAIL};

/// Collapse repeated log lines and blank-line runs.
pub fn dedup_log(input: &str) -> String {
    let lines: Vec<&str> = input.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut prev: Option<&str> = None;
    let mut run_count = 0usize;
    let mut blank_streak = 0usize;

    for line in lines {
        if line.trim().is_empty() {
            if blank_streak < 1 {
                out.push(line.to_string());
            }
            blank_streak += 1;
            if prev.is_some() && run_count > 1 {
                out.push(format!("  ... ({} duplicate lines)", run_count - 1));
            }
            prev = None;
            run_count = 0;
            continue;
        }
        blank_streak = 0;
        if prev == Some(line) {
            run_count += 1;
            continue;
        }
        if prev.is_some() && run_count > 1 {
            out.push(format!("  ... ({} duplicate lines)", run_count - 1));
        }
        out.push(line.to_string());
        prev = Some(line);
        run_count = 1;
        if out.len() >= DEDUP_LINE_MAX {
            out.push(format!("... (truncated at {DEDUP_LINE_MAX} lines)"));
            return out.join("\n");
        }
    }
    if prev.is_some() && run_count > 1 {
        out.push(format!("  ... ({} duplicate lines)", run_count - 1));
    }
    out.join("\n")
}

/// `smartTruncate(input)`.
pub fn smart_truncate(input: &str) -> String {
    let lines: Vec<&str> = input.split('\n').collect();
    if lines.len() < SMART_TRUNCATE_MIN_LINES {
        return input.to_string();
    }
    let head = &lines[..SMART_TRUNCATE_HEAD];
    let tail = &lines[lines.len() - SMART_TRUNCATE_TAIL..];
    let cut = lines.len() - head.len() - tail.len();
    let mut out: Vec<&str> = head.to_vec();
    // The cut marker is owned; build the tail into a Vec<String> join instead.
    let mut pieces: Vec<String> = out.drain(..).map(str::to_string).collect();
    pieces.push(format!("... +{cut} lines truncated"));
    pieces.extend(tail.iter().map(|s| s.to_string()));
    pieces.join("\n")
}

/// `READ_NUMBERED_LINE_RE`.
static READ_NUMBERED_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\d+\|").expect("static pattern"));

/// `readNumbered(input)`. The hit-ratio shape check lives in autodetect; here we
/// only truncate.
pub fn read_numbered(input: &str) -> String {
    let lines: Vec<&str> = input.split('\n').collect();
    if lines.len() < SMART_TRUNCATE_MIN_LINES {
        return input.to_string();
    }
    let head = &lines[..SMART_TRUNCATE_HEAD];
    let tail = &lines[lines.len() - SMART_TRUNCATE_TAIL..];
    let cut = lines.len() - head.len() - tail.len();
    let mut pieces: Vec<String> = head.iter().map(|s| s.to_string()).collect();
    pieces.push(format!("... +{cut} lines truncated (file continues)"));
    pieces.extend(tail.iter().map(|s| s.to_string()));
    pieces.join("\n")
}

/// `READ_NUMBERED_LINE_RE`, exposed for autodetect.
pub fn is_line_numbered_line(line: &str) -> bool {
    READ_NUMBERED_LINE_RE.is_match(line)
}

static RE_CARGO_ERR_CONT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(-->|\||\d+\s*\||=)").expect("static pattern"));

/// `buildOutput(input)`.
pub fn build_output(input: &str) -> String {
    let lines: Vec<&str> = input.split('\n').collect();
    if lines.is_empty() {
        return input.to_string();
    }

    let mut errors: Vec<&str> = Vec::new();
    let mut warnings: Vec<&str> = Vec::new();
    let mut deprecations: Vec<&str> = Vec::new();
    let mut summary: Option<String> = None;
    let mut compiling_count = 0usize;
    let mut downloading_count = 0usize;
    let mut in_cargo_error = false;

    for line in &lines {
        let trimmed = line.trim();

        if in_cargo_error {
            if trimmed.is_empty() {
                in_cargo_error = false;
                continue;
            }
            if RE_CARGO_ERR_CONT.is_match(line) {
                errors.push(line);
                continue;
            }
            in_cargo_error = false;
        }

        if trimmed.is_empty() {
            continue;
        }

        if regex_is_match(r"(?i)^npm (ERR!|error)", trimmed)
            || regex_is_match(r"(?i)^yarn error", trimmed)
        {
            errors.push(line);
            continue;
        }
        if regex_is_match(r"(?i)^npm warn deprecated", trimmed) {
            deprecations.push(line);
            continue;
        }
        if regex_is_match(r"(?i)^npm warn", trimmed) || regex_is_match(r"(?i)^yarn warn", trimmed) {
            warnings.push(line);
            continue;
        }
        if regex_is_match(r"(?i)^error(\[|:)", trimmed) || trimmed.starts_with("error -->") {
            errors.push(line);
            in_cargo_error = true;
            continue;
        }
        if regex_is_match(r"(?i)^warning(\[|:)", trimmed) || trimmed.starts_with("warning -->") {
            warnings.push(line);
            in_cargo_error = true;
            continue;
        }
        if regex_is_match(r"(?i)^ERROR:", trimmed) {
            errors.push(line);
            continue;
        }
        if regex_is_match(r"(?i)^\[ERROR\]", trimmed)
            || regex_is_match(r"(?i)^BUILD FAILED", trimmed)
        {
            errors.push(line);
            continue;
        }
        if regex_is_match(r"(?i)^\[WARNING\]", trimmed) {
            warnings.push(line);
            continue;
        }
        if regex_is_match(r"(?i)^\s*Compiling\s+\S+", trimmed) {
            compiling_count += 1;
            continue;
        }
        if regex_is_match(r"(?i)^\s*Downloading\s+\S+", trimmed)
            || regex_is_match(r"(?i)^Fetching\s+", trimmed)
        {
            downloading_count += 1;
            continue;
        }
        if regex_is_match(
            r"(?i)^(added|removed|changed|audited|installed)\s+\d+\s+package",
            trimmed,
        ) || regex_is_match(r"(?i)^\s*Finished\s+", trimmed)
            || regex_is_match(r"(?i)^BUILD SUCCESS", trimmed)
            || regex_is_match(
                r"(?i)^\d+\s+(vulnerabilities|packages?|warnings?|errors?)",
                trimmed,
            )
            || regex_is_match(r"(?i)^Successfully (installed|built)", trimmed)
            || regex_is_match(r"(?i)^To address .* issues", trimmed)
            || regex_is_match(r"^Run `npm (audit|fund)`", trimmed)
            || regex_is_match(r"(?i)packages are looking for funding", trimmed)
        {
            summary = Some(match summary {
                Some(s) => format!("{s}\n{line}"),
                None => (*line).to_string(),
            });
            continue;
        }
    }

    let mut out = String::new();

    for d in deprecations.iter().take(3) {
        out.push_str(&format!("{d}\n"));
    }
    if deprecations.len() > 3 {
        out.push_str(&format!(
            "... +{} more deprecated packages\n",
            deprecations.len() - 3
        ));
    }

    if compiling_count > 0 {
        out.push_str(&format!("Compiled {compiling_count} packages\n"));
    }
    if downloading_count > 0 {
        out.push_str(&format!("Downloaded {downloading_count} packages\n"));
    }

    for e in &errors {
        out.push_str(&format!("{e}\n"));
    }

    for w in warnings.iter().take(5) {
        out.push_str(&format!("{w}\n"));
    }
    if warnings.len() > 5 {
        out.push_str(&format!("... +{} more warnings\n", warnings.len() - 5));
    }

    if let Some(s) = &summary {
        out.push_str(&format!("{s}\n"));
    }

    let trimmed = out.trim_end_matches('\n').to_string();
    if trimmed.is_empty() {
        input.to_string()
    } else {
        trimmed
    }
}

/// Compile-once helper for the build-output rules, so each rule stays readable
/// as a literal pattern next to its branch.
fn regex_is_match(pattern: &str, text: &str) -> bool {
    // The set is small and fixed; a tiny cache avoids recompiling per line.
    static CACHE: LazyLock<dashmap::DashMap<String, Regex>> = LazyLock::new(dashmap::DashMap::new);
    if let Some(re) = CACHE.get(pattern) {
        return re.is_match(text);
    }
    let Ok(re) = Regex::new(pattern) else {
        return false;
    };
    let matched = re.is_match(text);
    CACHE.insert(pattern.to_string(), re);
    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_log_collapses_consecutive_duplicates() {
        let mut input = vec!["repeated log line A"; 20].join("\n");
        input.push_str("\nunique\n");
        input.push_str(&["another dup"; 10].join("\n"));
        let out = dedup_log(&input);
        assert!(out.contains("repeated log line A"));
        assert!(out.contains("duplicate lines"));
        assert!(out.len() < input.len());
    }

    #[test]
    fn smart_truncate_keeps_head_and_tail() {
        let lines: Vec<String> = (0..400).map(|i| format!("line {i}")).collect();
        let input = lines.join("\n");
        let out = smart_truncate(&input);
        assert!(out.contains("line 0"));
        assert!(out.contains("line 399"));
        assert!(out.contains("lines truncated"));
        assert!(out.len() < input.len());
    }

    #[test]
    fn smart_truncate_passes_small_input() {
        let input = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(smart_truncate(&input), input);
    }

    #[test]
    fn read_numbered_compacts_long_dump() {
        let lines: Vec<String> = (1..=400).map(|i| format!("  {i}|content {i}")).collect();
        let input = lines.join("\n");
        let out = read_numbered(&input);
        assert!(out.contains("1|content 1"));
        assert!(out.contains("400|content 400"));
        assert!(out.contains("lines truncated"));
        assert!(out.len() < input.len());
    }

    #[test]
    fn build_output_keeps_errors_and_summary() {
        let mut lines = Vec::new();
        for i in 1..=20 {
            lines.push(format!("   Compiling package-{i} v1.0.{i}"));
        }
        lines.push(
            "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.34s".into(),
        );
        let input = lines.join("\n");
        let out = build_output(&input);
        assert!(out.contains("Compiled 20 packages"));
        assert!(out.contains("Finished"));
        assert!(out.len() < input.len());
    }

    #[test]
    fn build_output_keeps_npm_errors_and_deprecations() {
        let input = [
            "npm warn deprecated har-validator@5.1.5: this library is no longer supported",
            "npm warn deprecated uuid@3.4.0: uuid@10 and below is no longer supported",
            "npm warn deprecated request@2.88.2: request has been deprecated",
            "npm warn deprecated inflight@1.0.6: This module is not supported",
            "added 47 packages, and audited 48 packages in 13s",
            "4 vulnerabilities (2 moderate, 2 critical)",
        ]
        .join("\n");
        let out = build_output(&input);
        assert!(out.contains("more deprecated packages"));
        assert!(out.contains("added 47 packages"));
    }
}
