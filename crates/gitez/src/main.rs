//! GitEZ entry point: installs the theme, menus and key bindings, and opens
//! the main window.
//!
//! `gitez [path]` opens the repository containing `path` straight away.

mod assets;
mod repository;
mod theme;
mod time;
mod undo;
mod workspace;

use std::path::PathBuf;

use gpui_kit::*;

use workspace::{CloneRepository, InitRepository, OpenRepository, Workspace};

actions!(gitez, [Quit]);

fn main() {
    let path = std::env::args_os()
        .nth(1)
        .map(|arg| std::path::absolute(PathBuf::from(&arg)).unwrap_or_else(|_| arg.into()));

    gpui_kit::application()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            // Must run before any gpui-kit component is used.
            gpui_kit::init(cx);
            theme::init(cx);

            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([
                KeyBinding::new("secondary-o", OpenRepository, None),
                KeyBinding::new("secondary-q", Quit, None),
            ]);
            cx.bind_keys(workspace::key_bindings());
            cx.set_menus([
                Menu {
                    name: "GitEZ".into(),
                    items: vec![MenuItem::action("Quit GitEZ", Quit)],
                    disabled: false,
                },
                Menu {
                    name: "File".into(),
                    items: vec![
                        MenuItem::action("Open Repository…", OpenRepository),
                        MenuItem::action("Clone Repository…", CloneRepository),
                        MenuItem::action("New Repository…", InitRepository),
                    ],
                    disabled: false,
                },
            ]);
            // One window per process for now, so closing it ends the app.
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("GitEZ".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(960.), px(600.))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Workspace::new(path, window, cx))
            })
            .expect("failed to open the main window");
            cx.activate(true);
        });
}
