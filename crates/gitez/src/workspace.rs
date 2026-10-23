//! The main window: toolbar, repository sidebar, history, commit detail and
//! status bar, laid out as in the `Main.dc.html` artboard.

mod sidebar;
mod status_bar;
mod toolbar;

use gpui_kit::component::{StyledExt as _, h_resizable, resizable_panel};
use gpui_kit::*;

use crate::theme::{self, color};

/// The root view of a GitEZ window.
///
/// No repository is open yet, so every pane shows its empty state. Opening a
/// repository and filling the panes is the next step of milestone 1.
pub struct Workspace;

impl Workspace {
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .size_full()
            .bg(rgb(color::BACKGROUND))
            .text_color(rgb(color::TEXT))
            .text_size(theme::TEXT_SIZE)
            .child(toolbar::Toolbar)
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("workspace-panes")
                        .child(
                            resizable_panel()
                                .size(px(240.))
                                .size_range(px(180.)..px(400.))
                                .fixed()
                                .child(sidebar::Sidebar),
                        )
                        .child(resizable_panel().child(history_pane()))
                        .child(
                            resizable_panel()
                                .size(px(360.))
                                .size_range(px(280.)..px(560.))
                                .fixed()
                                .child(detail_pane()),
                        ),
                ),
            )
            .child(status_bar::StatusBar)
    }
}

/// Column widths of the history list, shared by its header and its rows.
mod columns {
    use gpui_kit::{Pixels, px};

    pub const GRAPH: Pixels = px(112.);
    pub const AUTHOR: Pixels = px(150.);
    pub const DATE: Pixels = px(96.);
    pub const SHA: Pixels = px(70.);
    pub const GAP: Pixels = px(12.);
}

/// The commit history: a title bar, the column header and the commit list.
fn history_pane() -> impl IntoElement {
    let column = |width: Pixels, label: &'static str| div().w(width).flex_none().child(label);

    div()
        .v_flex()
        .size_full()
        .min_w_0()
        .child(
            div()
                .h_flex()
                .h(px(46.))
                .px(px(14.))
                .gap(px(8.))
                .border_b_1()
                .border_color(rgb(color::BORDER))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("All branches"),
                ),
        )
        .child(
            div()
                .h_flex()
                .h(px(28.))
                .pr(px(14.))
                .gap(columns::GAP)
                .border_b_1()
                .border_color(rgb(color::BORDER))
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(color::TEXT_FAINT))
                .child(column(columns::GRAPH, "GRAPH").pl(px(14.)))
                .child(div().flex_1().min_w_0().child("DESCRIPTION"))
                .child(column(columns::AUTHOR, "AUTHOR"))
                .child(column(columns::DATE, "DATE"))
                .child(column(columns::SHA, "SHA")),
        )
        .child(empty_state(
            "No repository open",
            "Open a repository to see its history.",
        ))
}

/// Details of the selected commit.
fn detail_pane() -> impl IntoElement {
    div()
        .v_flex()
        .size_full()
        .bg(rgb(color::PANEL))
        .child(empty_state(
            "No commit selected",
            "Select a commit to see its message and changed files.",
        ))
}

/// A centred title and explanation filling the rest of a pane.
fn empty_state(title: &'static str, detail: &'static str) -> impl IntoElement {
    div()
        .v_flex()
        .flex_1()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .p(px(24.))
        .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
        .child(div().text_color(rgb(color::TEXT_MUTED)).child(detail))
}
