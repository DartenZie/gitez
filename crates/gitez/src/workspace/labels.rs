//! Small pieces shared by the history list and the commit detail: ref labels
//! and author avatars.

use gitez_git::{RefKind, RefLabel};
use gpui_kit::component::StyledExt as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::theme::{self, color};

/// Labels longer than this are cut short with an ellipsis, so a long branch
/// name leaves room for the commit summary beside it.
const MAX_LABEL_WIDTH: Pixels = px(200.);
/// Labels shrink to this when several share a row with little space.
const MIN_LABEL_WIDTH: Pixels = px(96.);

/// A label for a branch, remote-tracking branch, tag or detached `HEAD`.
pub fn ref_label(label: &RefLabel) -> impl IntoElement {
    let name = || {
        div()
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .child(label.name.clone())
    };
    let chip = div()
        .h_flex()
        .min_w(MIN_LABEL_WIDTH)
        .max_w(MAX_LABEL_WIDTH)
        .h(px(20.))
        .px(px(7.))
        .gap(px(4.))
        .rounded(px(4.))
        .text_size(theme::SMALL_TEXT_SIZE)
        .font_weight(FontWeight::MEDIUM)
        .whitespace_nowrap();
    match label.kind {
        RefKind::Branch => {
            let branch_color = theme::branch_color(&label.name);
            chip.bg(theme::with_alpha(branch_color, 0.16))
                .text_color(rgb(branch_color))
                .when(label.is_head, |this| this.child(head_badge(branch_color)))
                .child(name())
        }
        RefKind::RemoteBranch => chip
            .border_1()
            .border_color(rgb(color::REMOTE_LABEL_BORDER))
            .text_color(rgb(color::REMOTE_LABEL_TEXT))
            .child(name()),
        RefKind::Tag => chip
            .border_1()
            .border_dashed()
            .border_color(rgb(color::TAG_LABEL_BORDER))
            .text_color(rgb(color::TEXT_SECONDARY))
            .child(name()),
        RefKind::Head => chip
            .bg(theme::with_alpha(color::BRANCH_BLUE, 0.16))
            .child(head_badge(color::BRANCH_BLUE))
            .text_color(rgb(color::BRANCH_BLUE))
            .child("detached"),
    }
}

/// The small "HEAD" badge at the start of the current branch's label.
fn head_badge(background: u32) -> impl IntoElement {
    div()
        .flex_none()
        .px(px(4.))
        .rounded(px(3.))
        .bg(rgb(background))
        .text_color(rgb(color::HEAD_BADGE_TEXT))
        .text_size(px(9.5))
        .font_weight(FontWeight::BOLD)
        .child("HEAD")
}

/// A round avatar showing the initials of `name`.
pub fn avatar(name: &str, diameter: Pixels) -> impl IntoElement {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(diameter)
        .rounded_full()
        .bg(rgb(color::AVATAR))
        .text_color(rgb(color::AVATAR_TEXT))
        .text_size(diameter * 0.45)
        .font_weight(FontWeight::SEMIBOLD)
        .child(initials(name))
}

/// Up to two initials: the first letters of the first and last words.
fn initials(name: &str) -> String {
    let mut words = name.split_whitespace();
    let first = words.next().and_then(|word| word.chars().next());
    let last = words.last().and_then(|word| word.chars().next());
    first
        .into_iter()
        .chain(last)
        .flat_map(char::to_uppercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::initials;

    #[test]
    fn initials_take_first_and_last_words() {
        assert_eq!(initials("Miroslav Pašek"), "MP");
        assert_eq!(initials("Jana Marie Horáková"), "JH");
        assert_eq!(initials("dependabot"), "D");
        assert_eq!(initials("  "), "");
        assert_eq!(initials("šárka Ňuková"), "ŠŇ");
    }
}
