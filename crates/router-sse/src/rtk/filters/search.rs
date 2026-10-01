//! The file-listing and search-output filters: grep, find, ls, tree,
//! search-list.

use std::sync::LazyLock;

use regex::Regex;

use super::{
    FIND_PER_DIR_MAX, FIND_TOTAL_DIR_MAX, GREP_PER_FILE_MAX, LS_EXT_SUMMARY_TOP, LS_NOISE_DIRS,
    SEARCH_LIST_PER_DIR_MAX, SEARCH_LIST_TOTAL_DIR_MAX, TREE_MAX_LINES,
};

/// Compress grep output. Input format `file:lineno:content`, split on the first
/// two colons only.
pub fn grep(input: &str) -> String {
    let mut order: Vec<String> = Vec::new();
    let mut by_file: std::collections::HashMap<String, Vec<(String, String)>> =
        std::collections::HashMap::new();
    let mut total = 0usize;

    for line in input.split('\n') {
        let Some(first) = line.find(':') else {
            continue;
        };
        let Some(second_rel) = line[first + 1..].find(':') else {
            continue;
        };
        let second = first + 1 + second_rel;
        let file = &line[..first];
        let line_num = &line[first + 1..second];
        let content = &line[second + 1..];
        if !line_num.bytes().all(|b| b.is_ascii_digit()) || line_num.is_empty() {
            continue;
        }
        total += 1;
        if !by_file.contains_key(file) {
            order.push(file.to_string());
        }
        by_file
            .entry(file.to_string())
            .or_default()
            .push((line_num.to_string(), content.to_string()));
    }

    if total == 0 {
        return input.to_string();
    }

    order.sort();
    let mut out = format!("{total} matches in {}F:\n\n", order.len());
    for file in &order {
        let matches = &by_file[file];
        out.push_str(&format!("[file] {file} ({}):\n", matches.len()));
        for (line_num, content) in matches.iter().take(GREP_PER_FILE_MAX) {
            out.push_str(&format!("  {:>4}: {}\n", line_num, content.trim()));
        }
        if matches.len() > GREP_PER_FILE_MAX {
            out.push_str(&format!("  +{}\n", matches.len() - GREP_PER_FILE_MAX));
        }
        out.push('\n');
    }
    out
}

/// `find(input)`. Group by parent dir, show basenames.
pub fn find(input: &str) -> String {
    let lines: Vec<&str> = input.split('\n').filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return input.to_string();
    }

    let mut order: Vec<String> = Vec::new();
    let mut by_dir: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    for path in &lines {
        let last_sep = path.rfind(['/', '\\']);
        let (dir, basename) = match last_sep {
            None => (".".to_string(), (*path).to_string()),
            Some(i) => {
                let d = &path[..i];
                let dir = if d.is_empty() { "/" } else { d };
                (dir.to_string(), path[i + 1..].to_string())
            }
        };
        if !by_dir.contains_key(&dir) {
            order.push(dir.clone());
        }
        by_dir.entry(dir).or_default().push(basename);
    }

    order.sort();
    let mut out = format!("{} files in {} dirs:\n\n", lines.len(), order.len());
    for dir in order.iter().take(FIND_TOTAL_DIR_MAX) {
        let files = &by_dir[dir];
        let dir_label = dir.replace('\\', "/");
        out.push_str(&format!("{dir_label}/  ({})\n", files.len()));
        for f in files.iter().take(FIND_PER_DIR_MAX) {
            out.push_str(&format!("  {f}\n"));
        }
        if files.len() > FIND_PER_DIR_MAX {
            out.push_str(&format!("  +{}\n", files.len() - FIND_PER_DIR_MAX));
        }
    }
    if order.len() > FIND_TOTAL_DIR_MAX {
        out.push_str(&format!(
            "\n+{} more dirs\n",
            order.len() - FIND_TOTAL_DIR_MAX
        ));
    }
    out
}

/// Rust `LS_DATE_RE`: month + day + (year|HH:MM), and the name starts after it.
static LS_DATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\s+(Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+\d{1,2}\s+(\d{4}|\d{2}:\d{2})\s+",
    )
    .expect("static pattern")
});

fn human_size(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1}M", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1}K", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

struct LsEntry {
    file_type: char,
    size: u64,
    name: String,
}

fn parse_ls_line(line: &str) -> Option<LsEntry> {
    let m = LS_DATE_RE.find(line)?;
    let name = line[m.end()..].to_string();
    let before_date = &line[..m.start()];
    let before_parts: Vec<&str> = before_date.split_whitespace().collect();
    if before_parts.len() < 4 {
        return None;
    }
    let perms = before_parts[0];
    let file_type = perms.chars().next()?;

    // size = rightmost parseable number before the date
    let mut size = 0u64;
    for part in before_parts.iter().rev() {
        if let Ok(n) = part.parse::<u64>()
            && n.to_string() == *part
        {
            size = n;
            break;
        }
    }
    Some(LsEntry {
        file_type,
        size,
        name,
    })
}

/// `ls(input)`.
pub fn ls(input: &str) -> String {
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<(String, String)> = Vec::new();
    // Insertion-ordered so a count tie keeps first-seen order.
    let mut by_ext: Vec<(String, usize)> = Vec::new();

    for line in input.split('\n') {
        if line.starts_with("total ") || line.is_empty() {
            continue;
        }
        let Some(parsed) = parse_ls_line(line) else {
            continue;
        };
        if parsed.name == "." || parsed.name == ".." {
            continue;
        }
        if LS_NOISE_DIRS.contains(&parsed.name.as_str()) {
            continue;
        }

        if parsed.file_type == 'd' {
            dirs.push(parsed.name);
        } else if parsed.file_type == '-' || parsed.file_type == 'l' {
            let ext = match parsed.name.rfind('.') {
                Some(dot) if dot > 0 => parsed.name[dot..].to_string(),
                _ => "no ext".to_string(),
            };
            match by_ext.iter_mut().find(|(e, _)| *e == ext) {
                Some((_, c)) => *c += 1,
                None => by_ext.push((ext, 1)),
            }
            files.push((parsed.name, human_size(parsed.size)));
        }
    }

    if dirs.is_empty() && files.is_empty() {
        return input.to_string();
    }

    let mut out = String::new();
    for d in &dirs {
        out.push_str(&format!("{d}/\n"));
    }
    for (name, size) in &files {
        out.push_str(&format!("{name}  {size}\n"));
    }

    let mut summary = format!("\nSummary: {} files, {} dirs", files.len(), dirs.len());
    if !by_ext.is_empty() {
        // Count desc, stable so equal counts keep insertion order.
        by_ext.sort_by_key(|b| std::cmp::Reverse(b.1));
        let ext = &by_ext;
        let parts: Vec<String> = ext
            .iter()
            .take(LS_EXT_SUMMARY_TOP)
            .map(|(e, c)| format!("{c} {e}"))
            .collect();
        summary.push_str(&format!(" ({})", parts.join(", ")));
        if ext.len() > LS_EXT_SUMMARY_TOP {
            summary.push_str(&format!(", +{} more", ext.len() - LS_EXT_SUMMARY_TOP));
        }
        summary.push(')');
    }

    out.push_str(&summary);
    out
}

/// `tree(input)`.
pub fn tree(input: &str) -> String {
    let mut filtered: Vec<&str> = Vec::new();
    for line in input.split('\n') {
        if line.contains("director") && line.contains("file") {
            continue;
        }
        if line.trim().is_empty() && filtered.is_empty() {
            continue;
        }
        filtered.push(line);
    }
    while filtered.last().is_some_and(|l| l.trim().is_empty()) {
        filtered.pop();
    }
    if filtered.len() > TREE_MAX_LINES {
        let cut = filtered.len() - TREE_MAX_LINES;
        return format!(
            "{}\n... +{cut} more lines",
            filtered[..TREE_MAX_LINES].join("\n")
        );
    }
    filtered.join("\n")
}

/// `SEARCH_LIST_HEADER_RE`.
pub static SEARCH_LIST_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^Result of search in '[^']*' \(total (\d+) files?\):").expect("static pattern")
});

/// `searchList(input)`.
pub fn search_list(input: &str) -> String {
    let lines: Vec<&str> = input.split('\n').collect();
    if lines.is_empty() {
        return input.to_string();
    }
    let header = lines[0];
    let rest = &lines[1..];

    let mut paths: Vec<String> = Vec::new();
    for raw in rest {
        let t = raw.trim();
        if let Some(p) = t.strip_prefix("- ") {
            paths.push(p.to_string());
        }
    }
    if paths.is_empty() {
        return input.to_string();
    }

    let mut order: Vec<String> = Vec::new();
    let mut by_dir: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for p in &paths {
        let (dir, name) = match p.rfind('/') {
            None => (".".to_string(), p.clone()),
            Some(i) => {
                let d = &p[..i];
                let dir = if d.is_empty() { "/" } else { d };
                (dir.to_string(), p[i + 1..].to_string())
            }
        };
        if !by_dir.contains_key(&dir) {
            order.push(dir.clone());
        }
        by_dir.entry(dir).or_default().push(name);
    }

    order.sort();
    let mut out = format!(
        "{header}\n{} files in {} dirs:\n\n",
        paths.len(),
        order.len()
    );
    for dir in order.iter().take(SEARCH_LIST_TOTAL_DIR_MAX) {
        let names = &by_dir[dir];
        out.push_str(&format!("{dir}/ ({}):\n", names.len()));
        for n in names.iter().take(SEARCH_LIST_PER_DIR_MAX) {
            out.push_str(&format!("  {n}\n"));
        }
        if names.len() > SEARCH_LIST_PER_DIR_MAX {
            out.push_str(&format!("  +{}\n", names.len() - SEARCH_LIST_PER_DIR_MAX));
        }
        out.push('\n');
    }
    if order.len() > SEARCH_LIST_TOTAL_DIR_MAX {
        out.push_str(&format!(
            "+{} more dirs\n",
            order.len() - SEARCH_LIST_TOTAL_DIR_MAX
        ));
    }
    out.trim_end_matches('\n').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grep_groups_and_caps_per_file() {
        let mut lines = Vec::new();
        for i in 1..=40 {
            lines.push(format!(
                "src/foo.js:{i}:const x{i} = \"some value here with padding text padding text\""
            ));
        }
        for i in 1..=10 {
            lines.push(format!(
                "src/bar.js:{i}:const y{i} = \"another value here with padding padding padding\""
            ));
        }
        let input = lines.join("\n");
        let out = grep(&input);
        assert!(out.contains("50 matches in 2F:"));
        assert!(out.contains("[file] src/foo.js (40):"));
        assert!(out.contains("[file] src/bar.js (10):"));
        assert!(out.contains("+30"));
        assert!(out.len() < input.len());
    }

    #[test]
    fn find_groups_by_dir() {
        let mut lines = Vec::new();
        for i in 0..30 {
            lines.push(format!("./src/a/{i}.js"));
        }
        for i in 0..20 {
            lines.push(format!("./src/b/{i}.js"));
        }
        for i in 0..5 {
            lines.push(format!("./top{i}.md"));
        }
        let input = lines.join("\n");
        let out = find(&input);
        assert!(out.contains("55 files in 3 dirs:"));
        assert!(out.contains("./src/a/  (30)"));
        assert!(out.contains("./src/b/  (20)"));
        assert!(out.contains("./  (5)"));
        assert!(out.len() < input.len());
    }

    #[test]
    fn find_normalizes_windows_paths() {
        let input = "C:\\Users\\me\\project\\src\\a.js\nC:\\Users\\me\\project\\src\\b.js\nC:\\Users\\me\\project\\src\\c.js";
        let out = find(input);
        assert!(out.contains("3 files in 1 dirs"));
        assert!(out.contains("C:/Users/me/project/src/"));
        assert!(!out.contains('\\'));
    }

    #[test]
    fn ls_compacts_and_filters_noise() {
        let input = [
            "total 48",
            "drwxr-xr-x  2 user staff   64 Jan  1 12:00 .",
            "drwxr-xr-x  2 user staff   64 Jan  1 12:00 ..",
            "drwxr-xr-x  2 user staff   64 Jan  1 12:00 node_modules",
            "drwxr-xr-x  2 user staff   64 Jan  1 12:00 src",
            "-rw-r--r--  1 user staff 1234 Jan  1 12:00 Cargo.toml",
            "-rw-r--r--  1 user staff 5678 Jan  1 12:00 README.md",
        ]
        .join("\n");
        let out = ls(&input);
        assert!(out.contains("src/"));
        assert!(out.contains("Cargo.toml"));
        assert!(out.contains("1.2K"));
        assert!(out.contains("5.5K"));
        assert!(!out.contains("drwx"));
        assert!(!out.contains("node_modules"));
        assert!(out.contains("Summary: 2 files, 1 dirs"));
    }

    #[test]
    fn tree_removes_summary_line() {
        let input = ".\n├── src\n│   └── main.rs\n└── Cargo.toml\n\n2 directories, 3 files\n";
        let out = tree(input);
        assert!(!out.contains("directories"));
        assert!(out.contains("├──"));
        assert!(out.contains("main.rs"));
    }

    #[test]
    fn search_list_groups_cursor_glob() {
        let mut paths = Vec::new();
        for i in 0..30 {
            paths.push(format!("- src/a/f{i}.js"));
        }
        for i in 0..10 {
            paths.push(format!("- src/b/g{i}.js"));
        }
        let input = format!(
            "Result of search in '/Users/x' (total 40 files):\n{}",
            paths.join("\n")
        );
        let out = search_list(&input);
        assert!(out.contains("Result of search in"));
        assert!(out.contains("40 files in 2 dirs:"));
        assert!(out.contains("src/a/ (30):"));
        assert!(out.contains("src/b/ (10):"));
        assert!(out.len() < input.len());
    }
}
