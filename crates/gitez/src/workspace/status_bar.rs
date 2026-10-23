//! The status bar along the bottom of the window.

use std::sync::Arc;

use gitez_git::Head;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::repository::Snapshot;
use crate::theme::{self, color};

/// Where `HEAD` is, how the current branch compares to its upstream, and the
/// repository's location.
#[derive(IntoElement)]
pub struct StatusBar {
    pub snapshot: Option<Arc<Snapshot>>,
}

impl RenderOnce for StatusBar {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let bar = div()
            .h_flex()
            .flex_none()
            .h(theme::STATUS_BAR_HEIGHT)
            .px(px(14.))
            .gap(px(14.))
            .bg(rgb(color::PANEL))
            .border_t_1()
            .border_color(rgb(color::BORDER))
            .text_size(theme::SMALL_TEXT_SIZE)
            .text_color(rgb(color::TEXT_MUTED));

        let Some(snapshot) = self.snapshot else {
            return bar.child("No repository open");
        };

        let mono = cx.theme().mono_font_family.clone();
        let head = match &snapshot.refs.head {
            Some(Head::Branch { name, .. }) => {
                let branch_color = rgb(theme::branch_color(name));
                div()
                    .h_flex()
                    .gap(px(6.))
                    .text_color(branch_color)
                    .child(div().size(px(7.)).rounded_full().bg(branch_color))
                    .child(name.clone())
                    .into_any_element()
            }
            Some(Head::Unborn { name }) => format!("{name} · no commits yet").into_any_element(),
            Some(Head::Detached { target }) => div()
                .h_flex()
                .gap(px(4.))
                .child("HEAD detached at")
                .child(
                    div()
                        .font_family(mono)
                        .text_color(rgb(color::TEXT_SECONDARY))
                        .child(target.to_hex_with_len(7).to_string()),
                )
                .into_any_element(),
            None => "No HEAD".into_any_element(),
        };

        let tracking = match &snapshot.refs.head {
            Some(Head::Branch { name, .. }) => snapshot.tracking.get(name),
            _ => None,
        }
        .map(|tracking| match (tracking.ahead, tracking.behind) {
            (0, 0) => format!("Up to date with {}", tracking.upstream),
            (ahead, 0) => format!("↑{ahead} to {}", tracking.upstream),
            (0, behind) => format!("↓{behind} from {}", tracking.upstream),
            (ahead, behind) => format!("↑{ahead} ↓{behind} with {}", tracking.upstream),
        });

        bar.child(head)
            .when_some(tracking, |this, tracking| this.child(tracking))
            .child(
                div()
                    .ml_auto()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(snapshot.path.display().to_string()),
            )
    }
}
