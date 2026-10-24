//! Reverting a commit, and resetting the branch checked out to a commit.

use gitez_command::{Git, ResetMode, RevertOutcome};
use gitez_git::{Head, Oid};
use gpui_kit::component::button::*;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::*;
use gpui_kit::*;

use super::Workspace;
use crate::theme::color;
use crate::undo::{self, Discard, Recorder};

/// The ways to reset, as the form offers them.
const MODES: [(ResetMode, &str, &str); 3] = [
    (
        ResetMode::Soft,
        "Keep the changes, staged",
        "What the later commits changed waits in the index, ready to commit again.",
    ),
    (
        ResetMode::Mixed,
        "Keep the changes, unstaged",
        "What the later commits changed stays in your files, not staged.",
    ),
    (
        ResetMode::Hard,
        "Discard the changes",
        "Your files match the commit; uncommitted changes to tracked files go too. \
         Undo brings everything back.",
    ),
];

/// Asks how to reset branch `branch` to commit `target`.
pub struct ResetForm {
    branch: String,
    target: Oid,
    /// The commit as the form shows it.
    on: String,
    /// Index into [`MODES`].
    mode: usize,
}

impl ResetForm {
    pub fn new(branch: String, target: Oid, on: String) -> Self {
        Self {
            branch,
            target,
            on,
            mode: 1,
        }
    }
}

impl Render for ResetForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (_, _, explanation) = MODES[self.mode];
        let form = cx.entity().downgrade();
        div()
            .v_flex()
            .gap(px(14.))
            .pb(px(4.))
            .child(
                div()
                    .text_color(rgb(color::TEXT_SECONDARY))
                    .child(format!("Moves {} to {}.", self.branch, self.on)),
            )
            .child(
                RadioGroup::vertical("reset-mode")
                    .selected_index(Some(self.mode))
                    .children(
                        MODES
                            .iter()
                            .map(|(_, label, _)| Radio::new(*label).label(*label)),
                    )
                    .on_click(move |index, _, cx| {
                        form.update(cx, |form, cx| {
                            form.mode = *index;
                            cx.notify();
                        })
                        .ok();
                    }),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(rgb(color::TEXT_MUTED))
                    .child(explanation),
            )
    }
}

impl Workspace {
    /// The branch checked out, if one is.
    fn checked_out_branch(&self) -> Option<String> {
        match self.snapshot.as_ref()?.refs.head.as_ref()? {
            Head::Branch { name, .. } => Some(name.clone()),
            _ => None,
        }
    }

    /// Adds a commit to the branch checked out that takes back what commit
    /// `id` changed.
    pub(super) fn revert_commit(&mut self, id: Oid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(branch) = self.checked_out_branch() else {
            window.push_notification(Notification::info("Check out a branch to revert on."), cx);
            return;
        };
        let commit = self
            .history
            .read(cx)
            .commits()
            .iter()
            .find(|commit| commit.id == id)
            .cloned();
        let Some(commit) = commit else {
            return;
        };
        // A merge is reverted against its first parent: the branch it was
        // merged into.
        let mainline = (commit.parents.len() > 1).then_some(1);
        let short = id.to_hex_with_len(7).to_string();
        let label = format!("Revert {short} “{}”", commit.summary);
        self.run_git(
            Some(label),
            move |git| git.revert(&id.to_string(), mainline),
            move |this, result, step, window, cx| match result {
                Ok(RevertOutcome::Reverted) => this.notify_done(
                    Notification::success(format!("Reverted {short} on {branch}")),
                    step,
                    window,
                    cx,
                ),
                Ok(RevertOutcome::Conflicts) => window.push_notification(
                    Notification::warning(format!(
                        "Reverting {short} conflicts with later changes on {branch}, so \
                         nothing was changed."
                    )),
                    cx,
                ),
                Err(error) => window.push_notification(
                    Notification::error(format!("Could not revert {short}: {error}")),
                    cx,
                ),
            },
            window,
            cx,
        );
    }

    /// Asks how to reset the branch checked out to commit `target`.
    pub(super) fn reset_to(&mut self, target: Oid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(branch) = self.checked_out_branch() else {
            window.push_notification(Notification::info("Check out a branch to reset."), cx);
            return;
        };
        let on = self.describe_commit(target, cx);
        let form = cx.new(|_| ResetForm::new(branch.clone(), target, on));
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let (submit, form_for_submit) = (workspace.clone(), form.clone());
            dialog
                .title(format!("Reset {branch}"))
                .w(px(460.))
                .child(form.clone())
                .footer(
                    div()
                        .h_flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            Button::new("reset-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("reset-submit")
                                .primary()
                                .label("Reset")
                                .on_click(move |_, window, cx| {
                                    let (branch, target, mode) = {
                                        let form = form_for_submit.read(cx);
                                        (form.branch.clone(), form.target, MODES[form.mode].0)
                                    };
                                    window.close_dialog(cx);
                                    submit
                                        .update(cx, |this, cx| {
                                            this.reset_branch(branch, target, mode, window, cx)
                                        })
                                        .ok();
                                }),
                        ),
                )
        });
    }

    /// Moves branch `branch`, checked out, to `target` the way `mode` says,
    /// as one undo step. A hard reset first keeps the uncommitted changes
    /// it throws away, so undo can bring them back.
    fn reset_branch(
        &mut self,
        branch: String,
        target: Oid,
        mode: ResetMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = self.snapshot.as_ref().map(|snapshot| snapshot.path.clone()) else {
            return;
        };
        let short = target.to_hex_with_len(7).to_string();
        let label = format!("Reset {branch} to {short}");
        let run = cx.background_executor().spawn(async move {
            let git = Git::new(&dir);
            let recorder = Recorder::start(&dir);
            let saved = match mode {
                ResetMode::Hard => git.discard_tracked()?,
                _ => None,
            };
            if let Err(error) = git.reset(&target.to_string(), mode) {
                if let Some(saved) = &saved {
                    git.restore_discarded(saved)?;
                }
                return Err(error);
            }
            let recorded = recorder.finish(mode);
            Ok(match saved {
                Some(saved) => {
                    recorded.map(|recorded| recorded.with_discard(Discard::Tracked { saved }))
                }
                None => recorded,
            })
        });
        self.git_command = Some(cx.spawn_in(window, async move |this, cx| {
            let result: gitez_command::Result<_> = run.await;
            this.update_in(cx, |this, window, cx| {
                this.git_command = None;
                match result {
                    Ok(recorded) => {
                        let step = undo::record(cx, recorded, label);
                        this.notify_done(
                            Notification::success(format!("Reset {branch} to {short}")),
                            step,
                            window,
                            cx,
                        );
                    }
                    Err(error) => window.push_notification(
                        Notification::error(format!("Could not reset {branch}: {error}")),
                        cx,
                    ),
                }
                this.reload(window, cx);
                this.refresh_status(window, cx);
            })
            .ok();
        }));
    }
}
