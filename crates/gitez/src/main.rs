//! GitEZ entry point: installs the theme and opens the main window.

mod theme;
mod workspace;

use gpui_kit::*;

use workspace::Workspace;

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx: &mut App| {
            // Must run before any gpui-kit component is used.
            gpui_kit::init(cx);
            theme::init(cx);

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
                cx.new(|cx| Workspace::new(window, cx))
            })
            .expect("failed to open the main window");
            cx.activate(true);
        });
}
