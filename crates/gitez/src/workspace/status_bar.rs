//! The status bar along the bottom of the window.

use gpui_kit::component::*;
use gpui_kit::*;

use crate::theme::{self, color};

/// Current branch, ahead/behind, working-tree summary and last fetch. Shows a
/// placeholder until a repository is open.
#[derive(IntoElement)]
pub struct StatusBar;

impl RenderOnce for StatusBar {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        div()
            .h_flex()
            .flex_none()
            .h(theme::STATUS_BAR_HEIGHT)
            .px(px(14.))
            .gap(px(14.))
            .bg(rgb(color::PANEL))
            .border_t_1()
            .border_color(rgb(color::BORDER))
            .text_size(theme::SMALL_TEXT_SIZE)
            .text_color(rgb(color::TEXT_MUTED))
            .child("No repository open")
    }
}
