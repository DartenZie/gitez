//! GitEZ entry point.
//!
//! For now this opens a single window styled with the GitEZ design tokens and
//! proves the GPUI + gpui-kit toolchain works end to end.

use gpui_kit::component::button::*;
use gpui_kit::component::*;
use gpui_kit::*;

/// Colours from the GitEZ design (dark theme). Will move into a theme module.
mod palette {
    pub const BG: u32 = 0x12_12_14;
    pub const BG_HEADER: u32 = 0x16_16_18;
    pub const BORDER: u32 = 0x26_26_2b;
    pub const TEXT: u32 = 0xe7_e5_e1;
    pub const TEXT_MUTED: u32 = 0x9a_98_8f;
    pub const ACCENT: u32 = 0xe8_b0_4b;
    pub const ON_ACCENT: u32 = 0x1a_14_08;
}

struct HelloWorld {
    text: SharedString,
    clicks: usize,
}

impl Render for HelloWorld {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .size_full()
            .bg(rgb(palette::BG))
            .text_color(rgb(palette::TEXT))
            .text_sm()
            .child(
                // Header bar, as in the "History — main window" artboard.
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .h(px(44.))
                    .px_3()
                    .bg(rgb(palette::BG_HEADER))
                    .border_b_1()
                    .border_color(rgb(palette::BORDER))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(22.))
                            .rounded_md()
                            .bg(rgb(palette::ACCENT))
                            .text_color(rgb(palette::ON_ACCENT))
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .child("G"),
                    )
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("gitez")),
            )
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("Hello, {}!", self.text)),
                    )
                    .child(div().text_color(rgb(palette::TEXT_MUTED)).child(format!(
                        "GPUI and gpui-kit are up and running. Clicks: {}",
                        self.clicks
                    )))
                    .child(
                        Button::new("hello")
                            .primary()
                            .label("Click me")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.clicks += 1;
                                cx.notify();
                            })),
                    ),
            )
    }
}

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx: &mut App| {
            // Must run before any gpui-kit component is used.
            gpui_kit::init(cx);

            let bounds = Bounds::centered(None, size(px(960.), px(600.)), cx);
            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("GitEZ".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |_, cx| {
                cx.new(|_| HelloWorld {
                    text: "GitEZ".into(),
                    clicks: 0,
                })
            })
            .expect("failed to open the main window");
            cx.activate(true);
        });
}
