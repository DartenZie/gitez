//! The toolbar across the top of the window.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::*;
use gpui_kit::*;

use crate::theme::{self, color};

/// Height of toolbar buttons in the design.
const BUTTON_HEIGHT: Pixels = px(30.);

/// Repository switcher, undo and redo, remote operations, branch and stash,
/// the command palette entry and "New local PR".
///
/// Every action needs an open repository, so all of them are disabled until
/// one can be opened.
#[derive(IntoElement)]
pub struct Toolbar;

impl RenderOnce for Toolbar {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        div()
            .h_flex()
            .flex_none()
            .h(theme::TOOLBAR_HEIGHT)
            .px(px(14.))
            .gap(px(10.))
            .bg(rgb(color::PANEL))
            .border_b_1()
            .border_color(rgb(color::BORDER))
            .child(
                div().h_flex().gap(px(8.)).pl(px(6.)).child(logo()).child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("No repository"),
                ),
            )
            .child(
                div()
                    .h_flex()
                    .gap(px(2.))
                    .child(icon_button("undo", IconName::Undo2, "Undo last git action"))
                    .child(icon_button("redo", IconName::Redo2, "Redo")),
            )
            .child(separator())
            .child(
                div()
                    .h_flex()
                    .gap(px(6.))
                    .child(button("fetch", IconName::RefreshCw, "Fetch"))
                    .child(button("pull", IconName::ArrowDown, "Pull"))
                    .child(button("push", IconName::ArrowUp, "Push")),
            )
            .child(separator())
            .child(
                div()
                    .h_flex()
                    .gap(px(6.))
                    .child(button("branch", IconName::GitBranch, "Branch"))
                    .child(button("stash", IconName::Archive, "Stash")),
            )
            .child(palette_entry())
            .child(
                Button::new("new-local-pr")
                    .primary()
                    .h(BUTTON_HEIGHT)
                    .icon(IconName::GitPullRequestArrow)
                    .label("New local PR")
                    .disabled(true),
            )
    }
}

/// The amber "G" badge.
fn logo() -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .size(px(22.))
        .rounded(theme::RADIUS)
        .bg(rgb(color::ACCENT))
        .text_color(rgb(color::ON_ACCENT))
        .text_size(px(12.))
        .font_weight(FontWeight::BOLD)
        .child("G")
}

fn separator() -> impl IntoElement {
    div().w(px(1.)).h(px(20.)).bg(rgb(color::SEPARATOR))
}

fn button(id: &'static str, icon: IconName, label: &'static str) -> Button {
    Button::new(id)
        .h(BUTTON_HEIGHT)
        .icon(icon)
        .label(label)
        .disabled(true)
}

fn icon_button(id: &'static str, icon: IconName, label: &'static str) -> Button {
    Button::new(id)
        .ghost()
        .size(BUTTON_HEIGHT)
        .icon(icon)
        .accessibility_label(label)
        .tooltip(label)
        .disabled(true)
}

/// The search field that opens the command palette (`⌘K`). The palette is
/// not built yet, so this is only its resting appearance.
fn palette_entry() -> impl IntoElement {
    let shortcut =
        Keystroke::parse("secondary-k").expect("the palette shortcut is a valid keystroke");

    div()
        .h_flex()
        .flex_1()
        .max_w(px(460.))
        .min_w(px(200.))
        .ml_auto()
        .h(BUTTON_HEIGHT)
        .px(px(10.))
        .gap(px(6.))
        .rounded(theme::RADIUS)
        .border_1()
        .border_color(rgb(color::BUTTON_BORDER))
        .bg(rgb(color::BACKGROUND))
        .text_color(rgb(color::TEXT_MUTED))
        .text_size(px(12.5))
        .child(Icon::new(IconName::Search).size(px(14.)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child("Search commits or run any git command"),
        )
        .child(Kbd::new(shortcut))
}
