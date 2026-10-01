//! The launcher's first screen: `Choose Interface (vX)`, the server URL as a
//! subtitle, and the interface rows. Web UI, Terminal UI and Exit are the three
//! interfaces this build offers.

use super::term::{self, MenuChoice, MenuItem};
use super::{colors, open_browser};

/// What the interface menu resolved to.
pub enum Interface {
    Web,
    Terminal,
    Exit,
}

/// Draw the menu and return the choice. On a non-TTY this returns `Exit`.
pub fn choose(port: u16, version: &str) -> Interface {
    let items = vec![
        MenuItem::new("🌐 Web UI (Open in Browser)"),
        MenuItem::new("💻 Terminal UI (Interactive CLI)"),
        MenuItem::new("🚪 Exit"),
    ];
    let title = format!("Choose Interface (v{version})");
    let subtitle = format!(
        "🚀 Server: {}http://localhost:{port}{}",
        colors::GREEN,
        colors::RESET
    );
    match term::select_menu(&title, &items, 0, &subtitle, "", &[]) {
        MenuChoice::Index(0) => Interface::Web,
        MenuChoice::Index(1) => Interface::Terminal,
        _ => Interface::Exit,
    }
}

/// Open the dashboard in the browser and wait for the user to come back.
pub fn open_dashboard(port: u16) {
    open_browser(&format!("http://localhost:{port}/dashboard"));
    term::pause("\nPress Enter to go back to menu...");
}
