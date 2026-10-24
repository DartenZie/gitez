//! Stashing uncommitted changes, and bringing stashes back.

use gitez_command::UnstashOutcome;
use gitez_git::{Head, WorkingTreeStatus, WorktreeChange};
use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{StashChanges, Workspace};
use crate::theme::color;

/// What the form asks of the window.
pub enum StashFormEvent {
    /// Enter was pressed in the message.
    Submit,
}

/// What a filled-in form asks for.
pub struct StashRequest {
    /// `None` for git's own "WIP on …".
    pub message: Option<String>,
    pub untracked: bool,
}

/// The "Stash changes" form: an optional message and whether to stash
/// untracked files too.
pub struct StashForm {
    message: Entity<InputState>,
    untracked: bool,
    /// Changed files, and how many of them are untracked.
    changed: usize,
    new_files: usize,
    _message_events: Subscription,
}

impl EventEmitter<StashFormEvent> for StashForm {}

impl StashForm {
    pub fn new(
        status: &WorkingTreeStatus,
        branch: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = match branch {
            Some(branch) => format!("WIP on {branch}"),
            None => "WIP".to_owned(),
        };
        let message = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let message_events = cx.subscribe_in(&message, window, |_, _, event, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                cx.emit(StashFormEvent::Submit);
            }
        });
        let new_files = status
            .files
            .iter()
            .filter(|file| file.unstaged == Some(WorktreeChange::Untracked))
            .count();
        Self {
            message,
            untracked: true,
            changed: status.files.len(),
            new_files,
            _message_events: message_events,
        }
    }

    /// The message box, focused when the dialog opens.
    pub fn message_input(&self) -> &Entity<InputState> {
        &self.message
    }

    pub fn submit(&self, cx: &App) -> StashRequest {
        let message = self.message.read(cx).value().trim().to_owned();
        StashRequest {
            message: (!message.is_empty()).then_some(message),
            untracked: self.untracked && self.new_files > 0,
        }
    }

    /// The files that will be stashed.
    fn stashed(&self) -> usize {
        if self.untracked {
            self.changed
        } else {
            self.changed - self.new_files
        }
    }
}

impl Render for StashForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let caption = |text: &'static str| {
            div()
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_MUTED))
                .child(text)
        };
        let summary = match self.stashed() {
            0 => "Only new files changed; include them to stash anything.".to_owned(),
            1 => {
                "Puts 1 changed file aside and the working tree back to the last commit.".to_owned()
            }
            count => format!(
                "Puts {count} changed files aside and the working tree back to the last commit."
            ),
        };
        let new_files = match self.new_files {
            1 => "1 file git does not track yet".to_owned(),
            count => format!("{count} files git does not track yet"),
        };
        div()
            .v_flex()
            .gap(px(14.))
            .pb(px(4.))
            .child(
                div()
                    .v_flex()
                    .gap(px(6.))
                    .child(caption("Message"))
                    .child(Input::new(&self.message)),
            )
            .when(self.new_files > 0, |this| {
                this.child(
                    div()
                        .h_flex()
                        .items_start()
                        .gap(px(8.))
                        .child(
                            Checkbox::new("stash-untracked")
                                .checked(self.untracked)
                                .mt(px(2.))
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.untracked = *checked;
                                    cx.notify();
                                })),
                        )
                        .child(
                            div().v_flex().gap(px(2.)).child("Include new files").child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(color::TEXT_MUTED))
                                    .child(new_files),
                            ),
                        ),
                )
            })
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(rgb(color::TEXT_MUTED))
                    .child(summary),
            )
    }
}

impl Workspace {
    /// Asks how to stash the uncommitted changes.
    pub(super) fn stash_changes(
        &mut self,
        _: &StashChanges,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(status) = self.status.clone().filter(|status| !status.is_clean()) else {
            window.push_notification(Notification::info("There are no changes to stash."), cx);
            return;
        };
        let branch = match self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.refs.head.as_ref())
        {
            Some(Head::Branch { name, .. }) => Some(name.clone()),
            _ => None,
        };
        let form = cx.new(|cx| StashForm::new(&status, branch.as_deref(), window, cx));
        let subscription =
            cx.subscribe_in(&form, window, |this, _, event, window, cx| match event {
                StashFormEvent::Submit => this.submit_stash_form(window, cx),
            });
        self.stash_form = Some((form.clone(), subscription));
        let workspace = cx.entity().downgrade();
        let focus_message = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if focus_message.replace(false) {
                let message = form.read(cx).message_input().focus_handle(cx);
                window.focus(&message, cx);
            }
            let submit = workspace.clone();
            dialog
                .title("Stash changes")
                .w(px(420.))
                .child(form.clone())
                .footer(
                    div()
                        .h_flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            Button::new("stash-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("stash-submit")
                                .primary()
                                .label("Stash")
                                .on_click(move |_, window, cx| {
                                    submit
                                        .update(cx, |this, cx| this.submit_stash_form(window, cx))
                                        .ok();
                                }),
                        ),
                )
                .on_close({
                    let workspace = workspace.clone();
                    move |_, _, cx| {
                        workspace.update(cx, |this, _| this.stash_form = None).ok();
                    }
                })
        });
    }

    fn submit_stash_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((form, _)) = self.stash_form.take() else {
            return;
        };
        let request = form.read(cx).submit(cx);
        window.close_dialog(cx);
        self.run_git(
            Some("Stash changes".to_owned()),
            move |git| git.stash(request.message.as_deref(), request.untracked),
            |this, result, step, window, cx| match result {
                Ok(true) => this.notify_done(
                    Notification::success("Stashed your changes"),
                    step,
                    window,
                    cx,
                ),
                Ok(false) => window
                    .push_notification(Notification::info("There were no changes to stash."), cx),
                Err(error) => window.push_notification(
                    Notification::error(format!("Could not stash: {error}")),
                    cx,
                ),
            },
            window,
            cx,
        );
    }

    /// Brings stash `index` back into the working tree, dropping it if
    /// `pop` and it applies cleanly.
    pub(super) fn unstash(
        &mut self,
        index: usize,
        pop: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_git(
            None,
            move |git| git.unstash(index, pop),
            move |_, result, _, window, cx| {
                let notification = match result {
                    Ok(UnstashOutcome::Applied) if pop => {
                        Notification::success("Popped the stash into the working tree")
                    }
                    Ok(UnstashOutcome::Applied) => {
                        Notification::success("Applied the stash to the working tree; it is kept")
                    }
                    Ok(UnstashOutcome::Conflicts) => Notification::warning(
                        "The stash conflicts with your files. The conflicts are marked in \
                         them, and the stash is kept until you drop it.",
                    ),
                    Err(error) => {
                        Notification::error(format!("Could not apply the stash: {error}"))
                    }
                };
                window.push_notification(notification, cx);
            },
            window,
            cx,
        );
    }

    /// Drops stash `index`; it can be undone.
    pub(super) fn drop_stash(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(stash) = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .refs
                .stashes
                .iter()
                .find(|stash| stash.index == index)
        }) else {
            return;
        };
        let label = format!("Drop stash “{}”", stash.message);
        self.run_git(
            Some(label),
            move |git| git.drop_stash(index),
            |this, result, step, window, cx| match result {
                Ok(()) => {
                    this.notify_done(Notification::success("Dropped the stash"), step, window, cx)
                }
                Err(error) => window.push_notification(
                    Notification::error(format!("Could not drop the stash: {error}")),
                    cx,
                ),
            },
            window,
            cx,
        );
    }
}
