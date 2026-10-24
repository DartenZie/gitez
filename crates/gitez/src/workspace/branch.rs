//! The forms that create a branch and rename one.

use gitez_command::{Git, not_merged};
use gitez_git::{Head, Oid, RefKind, branch_name_problem};
use gitez_review::State;
use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState, SelectAll};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{NewBranch, Workspace};
use crate::theme::color;

/// What the form makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchFormKind {
    /// A new branch at commit `start`, described by `from` ("main",
    /// "a1b2c3d Add the parser").
    New { start: Oid, from: String },
    /// A new name for branch `from`.
    Rename { from: String },
}

/// What a filled-in form asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchRequest {
    Create {
        name: String,
        start: Oid,
        checkout: bool,
    },
    Rename {
        from: String,
        to: String,
    },
}

/// What the form asks of the window.
pub enum BranchFormEvent {
    /// Enter was pressed in the name.
    Submit,
}

pub struct BranchForm {
    kind: BranchFormKind,
    name: Entity<InputState>,
    /// Whether to check the new branch out.
    checkout: bool,
    /// The branches there are, which a new name must not repeat.
    branches: Vec<String>,
    /// Why the name cannot be used; shown once something is typed, or on
    /// submitting.
    error: Option<SharedString>,
    _name_events: Subscription,
}

impl EventEmitter<BranchFormEvent> for BranchForm {}

impl BranchForm {
    pub fn new(
        kind: BranchFormKind,
        branches: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let value = match &kind {
            BranchFormKind::New { .. } => String::new(),
            BranchFormKind::Rename { from } => from.clone(),
        };
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("feature/my-change")
                .default_value(value)
        });
        let name_events = cx.subscribe_in(&name, window, |this, _, event, _, cx| match event {
            InputEvent::PressEnter { .. } => cx.emit(BranchFormEvent::Submit),
            InputEvent::Change => {
                let name = this.name(cx);
                this.error = if name.is_empty() {
                    None
                } else {
                    this.problem(&name).map(Into::into)
                };
                cx.notify();
            }
            _ => {}
        });
        Self {
            kind,
            name,
            checkout: true,
            branches,
            error: None,
            _name_events: name_events,
        }
    }

    pub fn title(&self) -> &'static str {
        match self.kind {
            BranchFormKind::New { .. } => "New branch",
            BranchFormKind::Rename { .. } => "Rename branch",
        }
    }

    pub fn submit_label(&self) -> &'static str {
        match self.kind {
            BranchFormKind::New { .. } => "Create branch",
            BranchFormKind::Rename { .. } => "Rename",
        }
    }

    /// The name box, focused when the dialog opens.
    pub fn name_input(&self) -> &Entity<InputState> {
        &self.name
    }

    fn name(&self, cx: &App) -> String {
        self.name.read(cx).value().trim().to_owned()
    }

    /// Why `name` cannot be used, if it cannot.
    fn problem(&self, name: &str) -> Option<String> {
        if let BranchFormKind::Rename { from } = &self.kind
            && name == from
        {
            return Some("Type a new name.".to_owned());
        }
        if let Some(problem) = branch_name_problem(name) {
            return Some(problem.to_owned());
        }
        self.branches
            .iter()
            .any(|branch| branch == name)
            .then(|| format!("A branch named {name} already exists."))
    }

    /// What the form asks for; `None` after showing why when the name
    /// cannot be used.
    pub fn submit(&mut self, cx: &mut Context<Self>) -> Option<BranchRequest> {
        let name = self.name(cx);
        if let Some(problem) = self.problem(&name) {
            self.error = Some(problem.into());
            cx.notify();
            return None;
        }
        Some(match &self.kind {
            BranchFormKind::New { start, .. } => BranchRequest::Create {
                name,
                start: *start,
                checkout: self.checkout,
            },
            BranchFormKind::Rename { from } => BranchRequest::Rename {
                from: from.clone(),
                to: name,
            },
        })
    }
}

impl Render for BranchForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let caption = |text: &'static str| {
            div()
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_MUTED))
                .child(text)
        };
        let from = match &self.kind {
            BranchFormKind::New { from, .. } => Some(from.clone()),
            BranchFormKind::Rename { .. } => None,
        };
        div()
            .v_flex()
            .gap(px(14.))
            .pb(px(4.))
            .child(
                div()
                    .v_flex()
                    .gap(px(6.))
                    .child(caption("Name"))
                    .child(Input::new(&self.name))
                    .when_some(self.error.clone(), |this, error| {
                        this.child(
                            div()
                                .text_size(px(12.5))
                                .text_color(rgb(color::DIFF_REMOVED))
                                .child(error),
                        )
                    }),
            )
            .when_some(from, |this, from| {
                this.child(
                    div()
                        .v_flex()
                        .gap(px(6.))
                        .child(caption("Starts from"))
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_color(rgb(color::TEXT_SECONDARY))
                                .child(from),
                        ),
                )
                .child(
                    Checkbox::new("branch-checkout")
                        .checked(self.checkout)
                        .label("Check it out")
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.checkout = *checked;
                            cx.notify();
                        })),
                )
            })
    }
}

impl Workspace {
    /// Asks for a branch to create at the commit selected in the history,
    /// or where `HEAD` is.
    pub(super) fn new_branch(
        &mut self,
        _: &NewBranch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected = match self.history.read(cx).selected() {
            Some(super::history::Selection::Commit(commit, _)) => Some(commit.id),
            _ => None,
        };
        let head = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.refs.head.as_ref())
            .and_then(Head::target);
        if let Some(start) = selected.or(head) {
            self.new_branch_at(start, window, cx);
        }
    }

    /// Asks for a branch to create at commit `start`.
    pub(super) fn new_branch_at(
        &mut self,
        start: Oid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let history = self.history.read(cx);
        let branch = history
            .labels_of(start)
            .iter()
            .find(|label| label.kind == RefKind::Branch)
            .map(|label| label.name.clone());
        let summary = history
            .commits()
            .iter()
            .find(|commit| commit.id == start)
            .map(|commit| commit.summary.clone())
            .unwrap_or_default();
        let commit = format!("{} {summary}", start.to_hex_with_len(7));
        // "main · 4ff18e3 Render the commit graph"
        let from = match branch {
            Some(branch) => format!("{branch} · {commit}"),
            None => commit,
        };
        self.open_branch_form(BranchFormKind::New { start, from }, window, cx);
    }

    /// Asks for a new name for branch `branch`.
    pub(super) fn rename_branch(
        &mut self,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(number) = self.pull_request_on(&branch) {
            window.push_notification(
                Notification::info(format!(
                    "{branch} is a branch of local pull request #{number}; close or merge it first."
                )),
                cx,
            );
            return;
        }
        self.open_branch_form(BranchFormKind::Rename { from: branch }, window, cx);
    }

    /// Deletes branch `branch`, offering to bring it back. A branch with
    /// commits no other branch has is deleted only when `force`; otherwise
    /// the notification asks.
    pub(super) fn delete_branch(
        &mut self,
        branch: String,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(number) = self.pull_request_on(&branch) {
            window.push_notification(
                Notification::info(format!(
                    "{branch} is a branch of local pull request #{number}; close or merge it first."
                )),
                cx,
            );
            return;
        }
        let Some(tip) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.branch_tip(&branch))
        else {
            return;
        };
        let name = branch.clone();
        self.run_branch_command(
            move |git| git.delete_branch(&name, force),
            move |this, result, window, cx| match result {
                Ok(()) => {
                    let short = tip.to_hex_with_len(7).to_string();
                    let restore = branch.clone();
                    this.notify_with_action(
                        Notification::success(format!("Deleted {branch} (was {short})")),
                        "Undo",
                        move |this, window, cx| {
                            let restore = restore.clone();
                            this.run_branch_command(
                                {
                                    let restore = restore.clone();
                                    move |git| git.create_branch(&restore, &tip.to_string(), false)
                                },
                                move |_, result, window, cx| {
                                    window.push_notification(
                                        match result {
                                            Ok(()) => {
                                                Notification::success(format!("{restore} is back"))
                                            }
                                            Err(error) => Notification::error(format!(
                                                "Could not bring {restore} back: {error}"
                                            )),
                                        },
                                        cx,
                                    )
                                },
                                window,
                                cx,
                            );
                        },
                        window,
                        cx,
                    );
                }
                Err(error) if not_merged(&error) => {
                    let again = branch.clone();
                    this.notify_with_action(
                        Notification::warning(format!(
                            "{branch} has commits that are not on the branch checked out or its \
                             upstream. Deleting it leaves them on no branch."
                        )),
                        "Delete anyway",
                        move |this, window, cx| this.delete_branch(again.clone(), true, window, cx),
                        window,
                        cx,
                    );
                }
                Err(error) => window.push_notification(
                    Notification::error(format!("Could not delete {branch}: {error}")),
                    cx,
                ),
            },
            window,
            cx,
        );
    }

    /// The open local pull request that branch `branch` is the head or base
    /// of, if one is.
    fn pull_request_on(&self, branch: &str) -> Option<u32> {
        self.snapshot
            .as_ref()?
            .pull_requests
            .iter()
            .find_map(|pull_request| {
                (pull_request.state == State::Open
                    && (pull_request.head.branch == branch || pull_request.base.branch == branch))
                    .then_some(pull_request.number)
            })
    }

    fn open_branch_form(
        &mut self,
        kind: BranchFormKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let branches = snapshot
            .refs
            .branches
            .iter()
            .map(|branch| branch.name.clone())
            .collect();
        let form = cx.new(|cx| BranchForm::new(kind, branches, window, cx));
        let subscription =
            cx.subscribe_in(&form, window, |this, _, event, window, cx| match event {
                BranchFormEvent::Submit => this.submit_branch_form(window, cx),
            });
        self.branch_form = Some((form.clone(), subscription));
        let workspace = cx.entity().downgrade();
        let focus_name = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if focus_name.replace(false) {
                let name = form.read(cx).name_input().focus_handle(cx);
                window.focus(&name, cx);
                // A new name replaces the old one as it is typed.
                window.dispatch_action(Box::new(SelectAll), cx);
            }
            let (title, submit_label) = {
                let form = form.read(cx);
                (form.title(), form.submit_label())
            };
            let submit = workspace.clone();
            dialog
                .title(title)
                .w(px(420.))
                .child(form.clone())
                .footer(
                    div()
                        .h_flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            Button::new("branch-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("branch-submit")
                                .primary()
                                .label(submit_label)
                                .on_click(move |_, window, cx| {
                                    submit
                                        .update(cx, |this, cx| this.submit_branch_form(window, cx))
                                        .ok();
                                }),
                        ),
                )
                .on_close({
                    let workspace = workspace.clone();
                    move |_, _, cx| {
                        workspace.update(cx, |this, _| this.branch_form = None).ok();
                    }
                })
        });
    }

    /// Creates or renames the branch the open form describes.
    fn submit_branch_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((form, _)) = &self.branch_form else {
            return;
        };
        let Some(request) = form.update(cx, |form, cx| form.submit(cx)) else {
            return;
        };
        window.close_dialog(cx);
        self.branch_form = None;
        match request {
            BranchRequest::Create {
                name,
                start,
                checkout,
            } => {
                let branch = name.clone();
                self.run_branch_command(
                    move |git| git.create_branch(&branch, &start.to_string(), checkout),
                    move |_, result, window, cx| {
                        let notification = match result {
                            Ok(()) if checkout => {
                                Notification::success(format!("Created and checked out {name}"))
                            }
                            Ok(()) => Notification::success(format!("Created {name}")),
                            Err(error) => {
                                Notification::error(format!("Could not create {name}: {error}"))
                            }
                        };
                        window.push_notification(notification, cx);
                    },
                    window,
                    cx,
                );
            }
            BranchRequest::Rename { from, to } => {
                let (old, new) = (from.clone(), to.clone());
                self.run_branch_command(
                    move |git| git.rename_branch(&old, &new),
                    move |_, result, window, cx| {
                        let notification = match result {
                            Ok(()) => Notification::success(format!("Renamed {from} to {to}")),
                            Err(error) => {
                                Notification::error(format!("Could not rename {from}: {error}"))
                            }
                        };
                        window.push_notification(notification, cx);
                    },
                    window,
                    cx,
                );
            }
        }
    }

    /// Runs `command` off the UI thread, then reads the repository again;
    /// `done` tells about the outcome.
    pub(super) fn run_branch_command<T: Send + 'static>(
        &mut self,
        command: impl FnOnce(Git) -> gitez_command::Result<T> + Send + 'static,
        done: impl FnOnce(&mut Self, gitez_command::Result<T>, &mut Window, &mut Context<Self>)
        + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = self.snapshot.as_ref().map(|snapshot| snapshot.path.clone()) else {
            return;
        };
        let run = cx
            .background_executor()
            .spawn(async move { command(Git::new(dir)) });
        self.branch_command = Some(cx.spawn_in(window, async move |this, cx| {
            let result = run.await;
            this.update_in(cx, |this, window, cx| {
                this.branch_command = None;
                done(this, result, window, cx);
                this.reload(window, cx);
                this.refresh_status(window, cx);
            })
            .ok();
        }));
    }
}
