//! The toolbar across the top of the window.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::palette::lowercase_first;
use super::remote::{RemoteActions, RemoteOp};
use super::{
    Fetch, NewBranch, NewPullRequest, OpenPalette, OpenRecent, OpenRepository, Pull, Push,
    RedoStep, StashChanges, UndoStep,
};
use crate::theme::{self, color};

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

/// Height of toolbar buttons in the design.
const BUTTON_HEIGHT: Pixels = px(30.);

/// Width and height of the repository's icon.
const LOGO_SIZE: Pixels = px(22.);

/// Width of the repository menu, whatever the length of the paths in it.
const REPOSITORY_MENU_WIDTH: Pixels = px(420.);

/// Width of a row in that menu, inside its padding and tick column.
const RECENT_ROW_WIDTH: Pixels = px(380.);

/// Width of the name and path in a row, beside the remove button.
const RECENT_TEXT_WIDTH: Pixels = px(348.);

/// Hover group of a row in the repository menu.
const RECENT_ROW_GROUP: &str = "recent-repository-row";

/// Repository switcher, undo and redo, remote operations, branch and stash,
/// the command palette entry and "New local PR".
///
#[derive(IntoElement)]
pub struct Toolbar {
    /// Name of the open repository.
    pub repository_name: Option<SharedString>,
    /// The open repository's own icon, if it has one.
    pub repository_icon: Option<Arc<Image>>,
    /// Folder of the open repository, ticked in the repository menu.
    pub repository_path: Option<PathBuf>,
    /// Where the repository menu sends its actions: the workspace.
    pub focus: FocusHandle,
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
                div()
                    .h_flex()
                    .gap(px(2.))
                    .pl(px(6.))
                    .child(logo(
                        self.repository_icon.clone(),
                        self.repository_name.as_deref(),
                    ))
                    .child(
                        Button::new("repository")
                            .ghost()
                            .h(BUTTON_HEIGHT)
                            .font_weight(FontWeight::SEMIBOLD)
                            .label(repository_label)
                            .loading(self.loading)
                            .dropdown_caret(true)
                            .tooltip("Switch repository")
                            .dropdown_menu({
                                let (focus, current) =
                                    (self.focus.clone(), self.repository_path.clone());
                                move |menu, _, cx| {
                                    repository_menu(menu, current.as_deref(), &focus, cx)
                                }
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

/// The recent repositories, the open one ticked and each with a remove
/// button on hover, then "Open repository…".
fn repository_menu(
    menu: PopupMenu,
    current: Option<&Path>,
    focus: &FocusHandle,
    cx: &Context<PopupMenu>,
) -> PopupMenu {
    let recent = crate::recent::paths(cx);
    let mut menu = menu
        .action_context(focus.clone())
        .min_w(REPOSITORY_MENU_WIDTH)
        .max_w(REPOSITORY_MENU_WIDTH);
    if !recent.is_empty() {
        menu = menu.label("Recent repositories");
    }
    for (index, path) in recent.iter().enumerate() {
        let name: SharedString = path
            .file_name()
            .map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            )
            .into();
        let location: SharedString = crate::recent::display(path).into();
        // Takes the row out and rebuilds the menu, which stays open.
        let remove = Rc::new({
            let path = path.clone();
            let current = current.map(Path::to_owned);
            let focus = focus.clone();
            let this = cx.weak_entity();
            move |window: &mut Window, cx: &mut App| {
                crate::recent::remove(&path, cx);
                this.update(cx, |menu, cx| {
                    menu.rebuild(window, cx, |menu, _, cx| {
                        repository_menu(menu, current.as_deref(), &focus, cx)
                    })
                })
                .ok();
            }
        });
        menu = menu.menu_element_with_check(
            current == Some(path.as_path()),
            Box::new(OpenRecent { path: path.clone() }),
            move |_, _| {
                let remove = remove.clone();
                // Definite widths: the popup sizes itself from its content,
                // and text measured without a width is not cut to fit.
                div()
                    .group(RECENT_ROW_GROUP)
                    .h_flex()
                    .justify_between()
                    .w(RECENT_ROW_WIDTH)
                    .child(
                        div()
                            .v_flex()
                            .w(RECENT_TEXT_WIDTH)
                            .child(
                                div()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis_middle()
                                    .child(name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(rgb(color::TEXT_MUTED))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis_middle()
                                    .child(location.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .opacity(0.)
                            .group_hover(RECENT_ROW_GROUP, |style| style.opacity(1.))
                            .child(
                                Button::new(("remove-recent", index))
                                    .ghost()
                                    .xsmall()
                                    .cursor_pointer()
                                    .icon(Icon::new(IconName::Close).size(px(13.)))
                                    .accessibility_label("Remove from recent repositories")
                                    .tooltip("Remove from recent repositories")
                                    .on_click(move |_, window, cx| {
                                        cx.stop_propagation();
                                        remove(window, cx);
                                    }),
                            ),
                    )
            },
        );
    }
    if !recent.is_empty() {
        menu = menu.separator();
    }
    menu.menu_with_icon(
        "Open repository…",
        IconName::FolderOpen,
        Box::new(OpenRepository),
    )
}

/// The icon of the repository called `name`, or the badge with its initial
/// if it has none or the icon cannot be decoded.
fn logo(icon: Option<Arc<Image>>, name: Option<&str>) -> AnyElement {
    let letter = initial(name);
    match icon {
        Some(icon) => img(icon)
            .flex_none()
            .size(LOGO_SIZE)
            .rounded(theme::RADIUS)
            .object_fit(ObjectFit::Contain)
            .with_fallback(move || badge(letter.clone()).into_any_element())
            .into_any_element(),
        None => badge(letter).into_any_element(),
    }
}

/// The amber badge with a letter in it.
fn badge(letter: SharedString) -> impl IntoElement {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(LOGO_SIZE)
        .rounded(theme::RADIUS)
        .bg(rgb(color::ACCENT))
        .text_color(rgb(color::ON_ACCENT))
        .text_size(px(12.))
        .font_weight(FontWeight::BOLD)
        .child(letter)
}

/// The first letter or digit of `name` in upper case; GitEZ's own "G" with
/// no repository open or nothing to take a letter from.
fn initial(name: Option<&str>) -> SharedString {
    name.and_then(|name| name.chars().find(|c| c.is_alphanumeric()))
        .map_or("G".into(), |c| c.to_uppercase().collect::<String>().into())
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

#[cfg(test)]
mod tests {
    use super::initial;

    #[test]
    fn the_initial_is_the_first_letter_or_digit_in_upper_case() {
        assert_eq!(initial(Some("gitez")).as_ref(), "G");
        assert_eq!(initial(Some(".dotfiles")).as_ref(), "D");
        assert_eq!(initial(Some("7zip")).as_ref(), "7");
        assert_eq!(initial(Some("čeština")).as_ref(), "Č");
    }

    #[test]
    fn without_a_name_the_initial_is_gitez_own() {
        assert_eq!(initial(None).as_ref(), "G");
        assert_eq!(initial(Some("---")).as_ref(), "G");
    }
}
