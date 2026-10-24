//! The status bar along the bottom of the window.

use std::path::{Path, PathBuf};
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
                    .min_w_0()
                    .gap(px(6.))
                    .text_color(branch_color)
                    .child(
                        div()
                            .flex_none()
                            .size(px(7.))
                            .rounded_full()
                            .bg(branch_color),
                    )
                    .child(truncated(name.clone()))
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
        .map(|tracking| {
            let upstream = match &snapshot.refs.head {
                Some(Head::Branch { name, .. }) => short_upstream(&tracking.upstream, name),
                _ => &tracking.upstream,
            };
            match (tracking.ahead, tracking.behind) {
                (0, 0) => format!("Up to date with {upstream}"),
                (ahead, 0) => format!("↑{ahead} to {upstream}"),
                (0, behind) => format!("↓{behind} from {upstream}"),
                (ahead, behind) => format!("↑{ahead} ↓{behind} with {upstream}"),
            }
        });

        // The branch and its tracking give way to the path when space runs
        // out; the path itself is cut short only beyond half the bar.
        bar.child(head)
            .when_some(tracking, |this, tracking| {
                this.child(truncated(tracking).flex_shrink_0().max_w(px(320.)))
            })
            .child(
                truncated(display_path(&snapshot.path))
                    .ml_auto()
                    .flex_shrink_0()
                    .max_w(relative(0.5)),
            )
    }
}

/// One line of text, cut short with an ellipsis when it does not fit.
fn truncated(text: impl Into<SharedString>) -> Div {
    div()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .child(text.into())
}

/// `upstream` without the branch name when it only repeats `branch`:
/// "origin" rather than "origin/feature" for the branch "feature".
fn short_upstream<'a>(upstream: &'a str, branch: &str) -> &'a str {
    upstream
        .strip_suffix(branch)
        .and_then(|remote| remote.strip_suffix('/'))
        .filter(|remote| !remote.is_empty())
        .unwrap_or(upstream)
}

/// `path` with the home directory written as `~`.
fn display_path(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(relative) if relative.as_os_str().is_empty() => "~".to_owned(),
        Some(relative) => format!("~/{}", relative.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::short_upstream;

    #[test]
    fn upstream_drops_a_repeated_branch_name() {
        assert_eq!(short_upstream("origin/feature", "feature"), "origin");
        assert_eq!(short_upstream("fork/team/feature", "feature"), "fork/team");
        assert_eq!(short_upstream("origin/main", "feature"), "origin/main");
        assert_eq!(
            short_upstream("origin/my-feature", "feature"),
            "origin/my-feature"
        );
    }
}
