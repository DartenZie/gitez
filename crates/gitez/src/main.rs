//! GitEZ entry point: installs the theme, menus and key bindings, and opens
//! the main window.
//!
//! `gitez [path]` opens the repository containing `path` straight away;
//! without one, GitEZ opens the repository that was open when it last quit.
//! `gitez supervise …` runs the supervisor of one agent thread instead
//! (see `gitez_supervisor::cli`); GitEZ starts it for every agent thread.

mod agent_threads;
mod assets;
mod menus;
mod project_icon;
mod recent;
mod repository;
// Screenshots of real views for agents; see the module.
#[cfg(all(test, target_os = "macos"))]
mod screenshot;
mod settings_sheet;
mod shell_path;
mod theme;
mod time;
mod toast;
mod undo;
mod workspace;

use std::path::PathBuf;
use std::process::ExitCode;

use gpui_kit::*;

use workspace::Workspace;

fn main() -> ExitCode {
    // A supervisor needs no window, theme or shell environment: it runs
    // with the environment GitEZ gave it.
    let mut args = std::env::args_os();
    if args.nth(1).is_some_and(|arg| arg == "supervise") {
        return gitez_supervisor::cli::main(args);
    }

    // Before anything else: it changes the environment, and git needs the
    // user's PATH to find filters such as git-crypt.
    shell_path::init();

    let path = std::env::args_os()
        .nth(1)
        .map(|arg| std::path::absolute(PathBuf::from(&arg)).unwrap_or_else(|_| arg.into()));

    gpui_kit::application()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            // Must run before any gpui-kit component is used.
            gpui_kit::init(cx);
            theme::init(cx);

            recent::init(cx);
            agent_threads::init(cx);
            settings_sheet::init(cx);
            menus::init(cx);
            cx.bind_keys(workspace::key_bindings());
            // One window per process for now, so closing it ends the app.
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let path = path.or_else(|| recent::last(cx));
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
    ExitCode::SUCCESS
}
