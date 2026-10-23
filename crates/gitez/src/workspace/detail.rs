//! Details of the selected commit, in the right-hand pane.

use gitez_git::{Commit, RefLabel};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::labels;
use crate::theme::color;
use crate::time;

/// SHA, parents, summary, author and refs of one commit.
///
/// The full message and the changed files need reads that do not exist yet.
#[derive(IntoElement)]
pub struct CommitDetail {
    pub commit: Commit,
    pub labels: Vec<RefLabel>,
}

impl RenderOnce for CommitDetail {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let commit = self.commit;
        let full_sha = commit.id.to_string();
        let short = |id: &gitez_git::Oid| id.to_hex_with_len(7).to_string();
        let parents = match commit.parents.as_slice() {
            [] => "root commit".to_owned(),
            [parent] => format!("parent {}", short(parent)),
            parents => format!(
                "parents {}",
                parents.iter().map(short).collect::<Vec<_>>().join(", ")
            ),
        };

        div()
            .id("commit-detail")
            .v_flex()
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .v_flex()
                    .gap(px(12.))
                    .pt(px(16.))
                    .px(px(18.))
                    .pb(px(14.))
                    .border_b_1()
                    .border_color(rgb(color::BORDER))
                    .child(
                        div()
                            .h_flex()
                            .gap(px(8.))
                            .text_size(px(12.))
                            .child(
                                div()
                                    .font_family(mono.clone())
                                    .text_color(rgb(color::ACCENT))
                                    .child(short(&commit.id)),
                            )
                            .child(
                                Button::new("copy-sha")
                                    .ghost()
                                    .size(px(24.))
                                    .icon(Icon::new(IconName::Copy).size(px(13.)))
                                    .accessibility_label("Copy full SHA")
                                    .tooltip("Copy full SHA")
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            full_sha.clone(),
                                        ));
                                    }),
                            )
                            .child(
                                div()
                                    .ml_auto()
                                    .font_family(mono)
                                    .text_color(rgb(color::TEXT_MUTED))
                                    .child(parents),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(17.))
                            .line_height(relative(1.35))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(commit.summary.clone()),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap(px(10.))
                            .child(labels::avatar(&commit.author.name, px(28.)))
                            .child(
                                div()
                                    .v_flex()
                                    .gap(px(1.))
                                    .min_w_0()
                                    .child(
                                        div()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(commit.author.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.5))
                                            .text_color(rgb(color::TEXT_MUTED))
                                            .child(time::absolute(commit.author.time)),
                                    ),
                            ),
                    )
                    .when(!self.labels.is_empty(), |this| {
                        this.child(
                            div()
                                .h_flex()
                                .flex_wrap()
                                .gap(px(6.))
                                .children(self.labels.iter().map(labels::ref_label)),
                        )
                    }),
            )
    }
}
