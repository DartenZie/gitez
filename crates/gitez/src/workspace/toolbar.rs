//! The toolbar across the top of the window.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::palette::lowercase_first;
use super::remote::{RemoteActions, RemoteOp};
use super::{
    Fetch, NewBranch, NewPullRequest, OpenPalette, OpenRepository, Pull, Push, RedoStep,
    StashChanges, UndoStep,
};
use crate::theme::{self, color};

/// Height of toolbar buttons in the design.
const BUTTON_HEIGHT: Pixels = px(30.);

/// Repository switcher, undo and redo, remote operations, branch and stash,
/// the command palette entry and "New local PR".
///
#[derive(IntoElement)]
pub struct Toolbar {
    /// Name of the open repository.
    pub repository_name: Option<SharedString>,
    /// Whether a repository is being opened.
    pub loading: bool,
    /// What fetch, pull and push would do; `None` without a repository.
    pub remote: Option<RemoteActions>,
    /// The remote operation running, if one is.
    pub running: Option<RemoteOp>,
    /// Whether there is a commit to start a branch from.
    pub can_branch: bool,
    /// Whether there are uncommitted changes to stash.
    pub can_stash: bool,
    /// What undo and redo would do next, such as "Commit “Fix the parser”".
    pub undo: Option<String>,
    pub redo: Option<String>,
}

impl RenderOnce for Toolbar {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let repository_label = self
            .repository_name
            .clone()
            .unwrap_or_else(|| "Open repository…".into());
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
                div().h_flex().gap(px(2.)).pl(px(6.)).child(logo()).child(
                    Button::new("repository")
                        .ghost()
                        .h(BUTTON_HEIGHT)
                        .font_weight(FontWeight::SEMIBOLD)
                        .label(repository_label)
                        .loading(self.loading)
                        .tooltip("Open a repository")
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(OpenRepository), cx)
                        }),
                ),
            )
            .child(
                div()
                    .h_flex()
                    .gap(px(2.))
                    .child(
                        icon_button("undo", IconName::Undo2, "Undo", self.undo.as_deref())
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(UndoStep), cx)
                            }),
                    )
                    .child(
                        icon_button("redo", IconName::Redo2, "Redo", self.redo.as_deref())
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(RedoStep), cx)
                            }),
                    ),
            )
            .child(separator())
            .child(div().h_flex().gap(px(6.)).children(self.remote_buttons(cx)))
            .child(separator())
            .child(
                div()
                    .h_flex()
                    .gap(px(6.))
                    .child(
                        Button::new("branch")
                            .h(BUTTON_HEIGHT)
                            .icon(IconName::GitBranch)
                            .label("Branch")
                            .tooltip("New branch from the selected commit")
                            .disabled(!self.can_branch)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(NewBranch), cx)
                            }),
                    )
                    .child(
                        Button::new("stash")
                            .h(BUTTON_HEIGHT)
                            .icon(IconName::Archive)
                            .label("Stash")
                            .tooltip(if self.can_stash {
                                "Put the uncommitted changes aside"
                            } else {
                                "There are no changes to stash"
                            })
                            .disabled(!self.can_stash)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(StashChanges), cx)
                            }),
                    ),
            )
            .child(palette_entry())
            .child(
                Button::new("new-local-pr")
                    .primary()
                    .h(BUTTON_HEIGHT)
                    .icon(IconName::GitPullRequestArrow)
                    .label("New local PR")
                    .tooltip("Review a branch as a local pull request")
                    .disabled(self.repository_name.is_none())
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(NewPullRequest), cx)),
            )
    }
}

impl Toolbar {
    /// Fetch, Pull with the commits to pull and Push with the commits to
    /// push. All three wait while one of them runs.
    fn remote_buttons(&self, cx: &App) -> [Button; 3] {
        let actions = self.remote.as_ref();
        let (ahead, behind) = actions.map_or((0, 0), |actions| (actions.ahead, actions.behind));
        let tooltip = |what: Option<Result<String, String>>| match what {
            Some(Ok(text) | Err(text)) => text,
            None => "No repository open".to_owned(),
        };
        let mono = cx.theme().mono_font_family.clone();
        let remote_button = |id: &'static str,
                             icon: IconName,
                             label: &'static str,
                             op: RemoteOp,
                             what: Option<Result<String, String>>,
                             count: usize| {
            let enabled = matches!(what, Some(Ok(_)));
            Button::new(id)
                .h(BUTTON_HEIGHT)
                .icon(icon)
                .label(label)
                .tooltip(tooltip(what))
                .loading(self.running == Some(op))
                .disabled(!enabled || self.running.is_some())
                .when(count > 0, |this| {
                    this.child(
                        div()
                            .font_family(mono.clone())
                            .text_size(px(11.))
                            .text_color(rgb(color::REMOTE_COUNT))
                            .child(count.to_string()),
                    )
                })
        };
        let push = actions.map(|actions| {
            actions
                .push
                .as_ref()
                .map(|plan| plan.describe())
                .map_err(Clone::clone)
        });
        [
            remote_button(
                "fetch",
                IconName::RefreshCw,
                "Fetch",
                RemoteOp::Fetch,
                actions.map(|actions| actions.fetch.clone()),
                0,
            )
            .on_click(|_, window, cx| window.dispatch_action(Box::new(Fetch), cx)),
            remote_button(
                "pull",
                IconName::ArrowDown,
                "Pull",
                RemoteOp::Pull,
                actions.map(|actions| actions.pull.clone()),
                behind,
            )
            .on_click(|_, window, cx| window.dispatch_action(Box::new(Pull), cx)),
            remote_button(
                "push",
                IconName::ArrowUp,
                "Push",
                RemoteOp::Push,
                push,
                ahead,
            )
            .on_click(|_, window, cx| window.dispatch_action(Box::new(Push), cx)),
        ]
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

/// Undo or redo, `verb`, of `step`; disabled without one.
fn icon_button(id: &'static str, icon: IconName, verb: &'static str, step: Option<&str>) -> Button {
    let tooltip = match step {
        Some(step) => format!("{verb} {}", lowercase_first(step)),
        None => format!("Nothing to {}", verb.to_lowercase()),
    };
    Button::new(id)
        .ghost()
        .size(BUTTON_HEIGHT)
        .icon(icon)
        .accessibility_label(verb)
        .tooltip(tooltip)
        .disabled(step.is_none())
}

/// The search field that opens the command palette (`⌘K`).
fn palette_entry() -> impl IntoElement {
    let shortcut =
        Keystroke::parse("secondary-k").expect("the palette shortcut is a valid keystroke");

    div()
        .id("palette-entry")
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
        .cursor_text()
        .hover(|style| style.border_color(rgb(color::PICK_BORDER)))
        .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenPalette), cx))
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
