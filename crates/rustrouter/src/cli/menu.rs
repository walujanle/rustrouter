//! The two menu drivers the terminal UI is built on.
//!
//! `show_menu_with_back` is a fixed list with a Back row at index 0; an action
//! returning `false` exits the menu, and a `refresh` returning `None` exits too.
//! `show_list_menu` fetches its rows each loop, optionally with a create row at
//! index 1.
//!
//! Both redraw on every loop, so a rename or a delete is visible the moment the
//! action returns. The dynamic-label plumbing (`label` and `header` as functions
//! of the refreshed data) lets rows render counts and current model names that
//! only exist after the refresh.

use serde_json::Value;

use super::term::{self, MenuItem};

/// A row in `show_menu_with_back`.
pub struct Entry<'a> {
    pub label: Box<dyn Fn(&Value) -> String + 'a>,
    pub action: Box<dyn FnMut(&Value) -> bool + 'a>,
    pub separator: bool,
}

impl<'a> Entry<'a> {
    /// A plain action row: `label` may be static or a function of the data.
    pub fn action(
        label: impl Fn(&Value) -> String + 'a,
        action: impl FnMut(&Value) -> bool + 'a,
    ) -> Self {
        Self {
            label: Box::new(label),
            action: Box::new(action),
            separator: false,
        }
    }

    /// A non-selectable separator row.
    pub fn separator(label: impl Into<String>) -> Self {
        let text = label.into();
        Self {
            label: Box::new(move |_| text.clone()),
            action: Box::new(|_| true),
            separator: true,
        }
    }
}

/// `showMenuWithBack`.
pub fn show_menu_with_back(
    title: &str,
    breadcrumb: &[String],
    back_label: &str,
    default_index: usize,
    mut refresh: impl FnMut() -> Option<Value>,
    header: impl Fn(&Value) -> String,
    items: &mut [Entry<'_>],
) {
    loop {
        // `refresh` returning null exits the menu.
        let Some(data) = refresh() else {
            return;
        };

        let mut menu_items = vec![MenuItem::new(back_label)];
        for entry in items.iter() {
            if entry.separator {
                menu_items.push(MenuItem::separator((entry.label)(&data)));
            } else {
                menu_items.push(MenuItem::new((entry.label)(&data)));
            }
        }

        let choice = term::select_menu(
            title,
            &menu_items,
            default_index,
            "",
            &header(&data),
            breadcrumb,
        );

        let index = match choice {
            term::MenuChoice::Back => return,
            term::MenuChoice::Index(i) => i,
        };
        if index == 0 {
            return;
        }
        if let Some(entry) = items.get_mut(index - 1) {
            if entry.separator {
                continue;
            }
            if !(entry.action)(&data) {
                return;
            }
        }
    }
}

/// `showListMenu`. `fetch_items` returns `None` to exit, otherwise the rows
/// plus the metadata passed to `header`.
pub struct ListMenu<'a> {
    pub title: &'a str,
    pub breadcrumb: &'a [String],
    pub back_label: &'a str,
    pub header: Box<dyn Fn(&Value) -> String + 'a>,
    pub fetch_items: Box<dyn FnMut() -> Option<(Vec<Value>, Value)> + 'a>,
    pub format_item: Box<dyn Fn(&Value) -> String + 'a>,
    pub on_select: Box<dyn FnMut(&Value) + 'a>,
    pub create_action: Option<(String, Box<dyn FnMut() + 'a>)>,
}

/// `showListMenu`.
pub fn show_list_menu(menu: &mut ListMenu<'_>) {
    loop {
        let Some((items, metadata)) = (menu.fetch_items)() else {
            return;
        };

        let mut menu_items = vec![MenuItem::new(menu.back_label)];
        let has_create = menu.create_action.is_some();
        if let Some((label, _)) = &menu.create_action {
            menu_items.push(MenuItem::new(label.clone()));
        }
        for item in &items {
            menu_items.push(MenuItem::new((menu.format_item)(item)));
        }

        let choice = term::select_menu(
            menu.title,
            &menu_items,
            0,
            "",
            &(menu.header)(&metadata),
            menu.breadcrumb,
        );

        let index = match choice {
            term::MenuChoice::Back => return,
            term::MenuChoice::Index(i) => i,
        };
        if index == 0 {
            return;
        }

        if has_create && index == 1 {
            if let Some((_, action)) = &mut menu.create_action {
                action();
            }
            continue;
        }

        let offset = if has_create { 2 } else { 1 };
        if index >= offset
            && let Some(item) = items.get(index - offset)
        {
            (menu.on_select)(item);
        }
    }
}
