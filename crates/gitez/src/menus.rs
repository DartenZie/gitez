//! The menu bar. Set again whenever Open Recent changes.

use gpui_kit::*;

use crate::recent;
use crate::workspace::{
    CloneRepository, CloseRepository, InitRepository, OpenRecent, OpenRepository, RevealRepository,
};

actions!(gitez, [Quit, CloseWindow, ClearRecent]);

/// Registers the app-wide menu actions and their key bindings.
pub fn init(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &ClearRecent, cx| recent::clear(cx));
    cx.on_action(|_: &CloseWindow, cx| {
        if let Some(window) = cx.active_window() {
            window
                .update(cx, |_, window, _| window.remove_window())
                .ok();
        }
    });
    cx.bind_keys([
        KeyBinding::new("secondary-o", OpenRepository, None),
        KeyBinding::new("secondary-w", CloseWindow, None),
        KeyBinding::new("secondary-q", Quit, None),
    ]);
    set(cx);
}

/// Sets the menu bar from the current list of recent repositories.
pub fn set(cx: &mut App) {
    let mut recent_items: Vec<MenuItem> = recent::paths(cx)
        .iter()
        .map(|path| MenuItem::action(recent::display(path), OpenRecent { path: path.clone() }))
        .collect();
    let empty = recent_items.is_empty();
    if !empty {
        recent_items.push(MenuItem::separator());
    }
    recent_items.push(MenuItem::Action {
        name: "Clear Menu".into(),
        action: Box::new(ClearRecent),
        os_action: None,
        checked: false,
        disabled: empty,
    });

    let reveal = if cfg!(target_os = "macos") {
        "Reveal in Finder"
    } else {
        "Show in File Manager"
    };

    cx.set_menus([
        Menu {
            name: "GitEZ".into(),
            items: vec![MenuItem::action("Quit GitEZ", Quit)],
            disabled: false,
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("New Repository…", InitRepository),
                MenuItem::action("Open Repository…", OpenRepository),
                MenuItem::submenu(Menu {
                    name: "Open Recent".into(),
                    items: recent_items,
                    disabled: false,
                }),
                MenuItem::action("Clone Repository…", CloneRepository),
                MenuItem::separator(),
                MenuItem::action(reveal, RevealRepository),
                MenuItem::separator(),
                MenuItem::action("Close Repository", CloseRepository),
                MenuItem::action("Close Window", CloseWindow),
            ],
            disabled: false,
        },
    ]);
}
