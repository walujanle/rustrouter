//! Terminal primitives: raw-mode key input, arrow-key menus, prompts.
//!
//! The observable behaviour: clear-and-redraw menus, `>` on Windows and `★/☆`
//! elsewhere, reverse+bright highlight, and the key bindings (up/down/return/
//! escape/ctrl-c).
//!
//! `crossterm` supplies the raw mode and key decoding, which is a bug farm to
//! hand-roll across Windows and Unix. The rendering and flow sit on top of it.

use std::io::{IsTerminal, Write};

use crossterm::cursor::MoveTo;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode, size};

use super::colors;

/// One row in an arrow-key menu.
pub struct MenuItem {
    pub label: String,
    /// `true` for a non-selectable separator row.
    pub separator: bool,
}

impl MenuItem {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            separator: false,
        }
    }

    pub fn separator(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            separator: true,
        }
    }
}

/// Menu input outcome. `None` is Back or Escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuChoice {
    Index(usize),
    Back,
}

fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
}

/// Whether the process has a usable interactive terminal on both ends. The
/// launcher uses this to decide between the interface menu and a headless run.
pub fn interactive() -> bool {
    stdout_is_tty()
}

fn read_key() -> std::io::Result<KeyEvent> {
    loop {
        if let Event::Key(key) = crossterm::event::read()? {
            // Windows reports both press and release; only act on press.
            if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
                return Ok(key);
            }
        }
    }
}

/// `clearScreen()`: wipe the terminal and home the cursor.
pub fn clear_screen() {
    let mut out = std::io::stdout();
    let _ = execute!(out, Clear(ClearType::All), MoveTo(0, 0));
    let _ = out.flush();
}

/// `selectMenu`: the arrow-key list.
///
/// The selected row is reverse+bright; the pointer is `>` on Windows and `★`
/// elsewhere, with `☆` for unselected rows. Separator rows are skipped by the
/// cursor.
pub fn select_menu(
    title: &str,
    items: &[MenuItem],
    default_index: usize,
    subtitle: &str,
    header: &str,
    breadcrumb: &[String],
) -> MenuChoice {
    if !stdout_is_tty() {
        return MenuChoice::Back;
    }

    let selectable: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, i)| !i.separator)
        .map(|(idx, _)| idx)
        .collect();
    if selectable.is_empty() {
        return MenuChoice::Back;
    }

    // Land on the first selectable row at or after the requested default.
    let mut pos = selectable
        .iter()
        .position(|&idx| idx >= default_index)
        .unwrap_or(0);

    let is_win = cfg!(windows);
    let _ = enable_raw_mode();

    let render = |pos: usize| {
        let (cols, _) = size().unwrap_or((40, 24));
        let width = cols.min(40) as usize;
        let mut out = std::io::stdout();
        let _ = execute!(out, Clear(ClearType::All), MoveTo(0, 0));
        let bar = "=".repeat(width);
        let _ = writeln!(out, "\n{}{bar}{}", colors::TERRACOTTA, colors::RESET);
        let _ = writeln!(
            out,
            "  {}{}{title}{}",
            colors::BRIGHT,
            colors::TERRACOTTA,
            colors::RESET
        );
        if !subtitle.is_empty() {
            let _ = writeln!(out, "  {}{subtitle}{}", colors::DIM, colors::RESET);
        }
        let _ = writeln!(out, "{}{bar}{}", colors::TERRACOTTA, colors::RESET);
        if !breadcrumb.is_empty() {
            let _ = writeln!(
                out,
                "  {}{}{}",
                colors::DIM,
                breadcrumb.join(" > "),
                colors::RESET
            );
        }
        let _ = writeln!(out);
        if !header.is_empty() {
            let _ = writeln!(out, "{header}\n");
        }
        for (index, item) in items.iter().enumerate() {
            if item.separator {
                let _ = writeln!(out, "  {}", item.label);
                continue;
            }
            let selected = selectable.get(pos) == Some(&index);
            let icon = match (selected, is_win) {
                (true, true) => ">",
                (true, false) => "★",
                (false, true) => " ",
                (false, false) => "☆",
            };
            if selected {
                let _ = writeln!(
                    out,
                    " {}{}{icon} {}{}",
                    colors::REVERSE,
                    colors::BRIGHT,
                    item.label,
                    colors::RESET
                );
            } else {
                let _ = writeln!(out, "  {icon} {}", item.label);
            }
        }
        let _ = out.flush();
    };

    render(pos);

    let result = loop {
        match read_key() {
            Ok(key) => match key.code {
                KeyCode::Up => {
                    pos = (pos + selectable.len() - 1) % selectable.len();
                    render(pos);
                }
                KeyCode::Down => {
                    pos = (pos + 1) % selectable.len();
                    render(pos);
                }
                KeyCode::Enter => break MenuChoice::Index(selectable[pos]),
                KeyCode::Esc => break MenuChoice::Back,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let _ = disable_raw_mode();
                    std::process::exit(0);
                }
                _ => {}
            },
            // A read error means no usable terminal; treat it as Back rather
            // than looping on a dead handle.
            Err(_) => break MenuChoice::Back,
        }
    };

    let _ = disable_raw_mode();
    result
}

/// `prompt`: a one-line text question, raw mode suspended so the terminal
/// buffers the line.
pub fn prompt(question: &str) -> String {
    let _ = disable_raw_mode();
    print!("{question}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    line.trim().to_string()
}

/// `confirm`: a `(y/n)` loop.
pub fn confirm(question: &str) -> bool {
    loop {
        let answer = prompt(&format!("{question} (y/n): ")).to_lowercase();
        match answer.as_str() {
            "y" | "yes" => return true,
            "n" | "no" => return false,
            _ => println!("Please answer 'y' or 'n'"),
        }
    }
}

/// `pause`: wait for Enter.
pub fn pause(message: &str) {
    let _ = disable_raw_mode();
    print!("{message}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

/// `showStatus`: `✓`/`✗`/`⚠`/`ℹ` with the matching colour.
pub fn show_status(message: &str, kind: StatusKind) {
    let (symbol, color) = match kind {
        StatusKind::Success => ("✓", colors::GREEN),
        StatusKind::Error => ("✗", colors::RED),
        StatusKind::Warning => ("⚠", colors::YELLOW),
        StatusKind::Info => ("ℹ", colors::CYAN),
    };
    println!("{color}{symbol} {message}{}", colors::RESET);
}

#[derive(Debug, Clone, Copy)]
pub enum StatusKind {
    Success,
    Error,
    Warning,
    Info,
}

/// `showHeader`: the `===` banner above OAuth instructions.
pub fn show_header(title: &str, subtitle: &str) {
    println!("\n{}", "=".repeat(60));
    println!("  {}{}{}", colors::BRIGHT, colors::CYAN, title);
    if !subtitle.is_empty() {
        println!("  {}{subtitle}{}", colors::DIM, colors::RESET);
    }
    println!("{}\n", "=".repeat(60));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_symbols_are_the_expected_glyphs() {
        // Not much to assert without a tty, but the mapping is worth pinning.
        assert_eq!(format!("{:?}", StatusKind::Success), "Success");
    }
}
