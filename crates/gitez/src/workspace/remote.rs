//! Fetch, pull and push: when each can run, and what it does.

use std::path::{Path, PathBuf};

use gitez_command::{
    Error, Git, Operation, PullMethod, PullOutcome, PushRejection, ResetMode, WorktreeProblem,
    blocked_by_changes, needs_pull_method, not_fast_forward,
};
use gitez_git::Head;
use gpui_kit::component::button::*;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::*;
use gpui_kit::*;

use super::sidebar::RunningRow;
use super::{CancelRemote, Fetch, Pull, Push, Workspace};
use crate::repository::Snapshot;
use crate::undo::{self, Recorder};

/// One of the toolbar's remote operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteOp {
    Fetch,
    Pull,
    Push,
}

impl RemoteOp {
    /// "fetch", "pull" or "push", as in "Cancel the pull".
    pub fn noun(self) -> &'static str {
        match self {
            Self::Fetch => "fetch",
            Self::Pull => "pull",
            Self::Push => "push",
        }
    }
}

/// What a remote operation works on: the sidebar row that shows it running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteTarget {
    /// Every remote.
    Remotes,
    /// The remote with this name.
    Remote(String),
    /// The local branch with this name.
    Branch(String),
}

/// The fetch, pull or push running.
pub(super) struct RunningRemote {
    pub op: RemoteOp,
    target: RemoteTarget,
    /// Ends the git it runs.
    operation: Operation,
    /// Whether it was cancelled and is ending.
    pub cancelling: bool,
    _run: Task<()>,
    /// Shows what git wrote when it wrote something and then did not exit.
    _stalled: Task<()>,
}

impl RunningRemote {
    /// What the sidebar shows of it.
    fn row(&self) -> RunningRow {
        RunningRow {
            op: self.op,
            target: self.target.clone(),
            cancelling: self.cancelling,
        }
    }
}

/// Identifies the notification that shows what git wrote while a remote
/// operation is still running, so it goes when the operation ends.
struct StalledRemote;

/// How pulling a branch that is not checked out here ended, when git ran.
/// The worktree is the other one the branch is checked out in, if it is
/// checked out.
enum BranchPull {
    Pulled {
        worktree: Option<PathBuf>,
    },
    /// Git refused, changing nothing.
    Refused {
        error: Error,
        worktree: Option<PathBuf>,
    },
}

/// What pushing the branch checked out does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushPlan {
    pub branch: String,
    /// Where it goes: its upstream, or the remote it is published to.
    pub to: String,
    /// Whether the branch has no upstream yet, so it is published to remote
    /// `to` and that becomes its upstream.
    pub publish: bool,
}

impl PushPlan {
    /// "Push main to origin/main", "Publish feature to origin".
    pub fn describe(&self) -> String {
        let verb = if self.publish { "Publish" } else { "Push" };
        format!("{verb} {} to {}", self.branch, self.to)
    }
}

/// What each remote operation would do now, or why it cannot run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteActions {
    /// What fetching and pulling do, or why they cannot run.
    pub fetch: Result<String, String>,
    pub pull: Result<String, String>,
    pub push: Result<PushPlan, String>,
    /// Commits the branch checked out has that its upstream lacks, and the
    /// other way round.
    pub ahead: usize,
    pub behind: usize,
}

impl RemoteActions {
    /// For the branch checked out.
    pub fn of(snapshot: &Snapshot) -> Self {
        let branch = match &snapshot.refs.head {
            Some(Head::Branch { name, .. }) => Some(name.as_str()),
            _ => None,
        };
        Self::of_branch(snapshot, branch)
    }

    /// For local branch `branch`, checked out or not.
    fn of_branch(snapshot: &Snapshot, branch: Option<&str>) -> Self {
        let upstream = branch.and_then(|name| {
            snapshot
                .refs
                .branches
                .iter()
                .find(|branch| branch.name == name)
                .and_then(|branch| branch.upstream.as_deref())
        });
        let (ahead, behind) = branch
            .and_then(|name| snapshot.tracking.get(name))
            .map_or((0, 0), |tracking| (tracking.ahead, tracking.behind));
        Self::new(
            &snapshot.remotes,
            branch,
            upstream,
            snapshot.repository.work_dir().is_none(),
            (ahead, behind),
        )
    }

    /// For the branch checked out, `branch`, with upstream `upstream`,
    /// in a repository with `remotes`.
    fn new(
        remotes: &[String],
        branch: Option<&str>,
        upstream: Option<&str>,
        bare: bool,
        (ahead, behind): (usize, usize),
    ) -> Self {
        let fetch = match remotes {
            [] => Err("This repository has no remotes".to_owned()),
            [remote] => Ok(format!("Fetch from {remote}")),
            remotes => Ok(format!("Fetch from all {} remotes", remotes.len())),
        };
        let Some(branch) = branch else {
            let detached = "No branch is checked out".to_owned();
            return Self {
                fetch,
                pull: Err(detached.clone()),
                push: Err(detached),
                ahead: 0,
                behind: 0,
            };
        };
        let pull = match upstream {
            _ if bare => Err("A bare repository has no working tree to pull into".to_owned()),
            None => Err(format!("{branch} has no upstream to pull from")),
            Some(upstream) => Ok(format!("Pull {upstream} into {branch}")),
        };
        // Like `git push -u origin`, unless there is no `origin`.
        let publish_to = remotes
            .iter()
            .find(|remote| *remote == "origin")
            .or(remotes.first());
        let push = match (upstream, publish_to) {
            (Some(upstream), _) => Ok(PushPlan {
                branch: branch.to_owned(),
                to: upstream.to_owned(),
                publish: false,
            }),
            (None, Some(remote)) => Ok(PushPlan {
                branch: branch.to_owned(),
                to: remote.clone(),
                publish: true,
            }),
            (None, None) => Err("This repository has no remote to push to".to_owned()),
        };
        Self {
            fetch,
            pull,
            push,
            ahead,
            behind,
        }
    }
}

impl Workspace {
    pub(super) fn fetch(&mut self, _: &Fetch, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let Ok(description) = RemoteActions::of(snapshot).fetch else {
            return;
        };
        self.run_remote(
            RemoteOp::Fetch,
            RemoteTarget::Remotes,
            None,
            |git| git.fetch(),
            move |_, result, _, window, cx| {
                let notification = match result {
                    // "Fetch from origin" becomes "Fetched from origin".
                    Ok(()) => crate::toast::success(description.replacen("Fetch", "Fetched", 1)),
                    Err(error) => crate::toast::error(format!("Could not fetch: {error}")),
                };
                window.push_notification(notification, cx);
            },
            window,
            cx,
        );
    }

    pub(super) fn pull(&mut self, _: &Pull, window: &mut Window, cx: &mut Context<Self>) {
        self.pull_with(PullMethod::Configured, window, cx);
    }

    fn pull_with(&mut self, method: PullMethod, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        // "Pull origin/main into main"
        let Ok(label) = RemoteActions::of(snapshot).pull else {
            return;
        };
        let Some(Head::Branch { name, .. }) = &snapshot.refs.head else {
            return;
        };
        self.run_remote(
            RemoteOp::Pull,
            RemoteTarget::Branch(name.clone()),
            Some(label),
            move |git| git.pull(method),
            |this, result, step, window, cx| match result {
                Ok(PullOutcome::Pulled) => {
                    this.notify_done(crate::toast::success("Pulled"), step, window, cx)
                }
                Ok(PullOutcome::Conflicts) => window.push_notification(
                    crate::toast::warning(
                        "The pull stopped on conflicts. Resolve them to finish it, or abort it.",
                    ),
                    cx,
                ),
                Err(error) if needs_pull_method(&error) => this.notify_with_action(
                    crate::toast::warning(
                        "The branch and its upstream have diverged, and git is not set to \
                         merge or rebase them (pull.rebase).",
                    ),
                    "Merge them",
                    |this, window, cx| this.pull_with(PullMethod::Merge, window, cx),
                    window,
                    cx,
                ),
                Err(error) => window
                    .push_notification(crate::toast::error(format!("Could not pull: {error}")), cx),
            },
            window,
            cx,
        );
    }

    /// Brings the upstream's new commits into local branch `branch`: as
    /// [`Pull`] does for the branch checked out, and only by fast-forwarding
    /// for any other. A branch checked out in another worktree is pulled
    /// there, so its files follow it.
    pub(super) fn pull_branch(
        &mut self,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        if checked_out(snapshot, &branch) {
            return self.pull_with(PullMethod::Configured, window, cx);
        }
        let Some(upstream) = snapshot
            .refs
            .branches
            .iter()
            .find(|local| local.name == branch)
            .and_then(|local| local.upstream.clone())
        else {
            return;
        };
        let name = branch.clone();
        self.run_remote(
            RemoteOp::Pull,
            RemoteTarget::Branch(branch.clone()),
            None,
            move |git| {
                // This one's own branch is pulled with `pull_with`, so a
                // worktree is named only when it is another.
                let elsewhere = |worktree: Option<PathBuf>| {
                    worktree.filter(|path| !same_folder(path, git.work_dir()))
                };
                match git.pull_branch(&name) {
                    Ok(worktree) => Ok(BranchPull::Pulled {
                        worktree: elsewhere(worktree),
                    }),
                    Err(error @ Error::Failed { .. }) => {
                        // Where git refused, to say where to go on. A new
                        // handle: the operation may be cancelled by now.
                        let worktree = Git::new(git.work_dir())
                            .worktree_of(&name)
                            .ok()
                            .flatten()
                            .map(|worktree| worktree.path);
                        Ok(BranchPull::Refused {
                            error,
                            worktree: elsewhere(worktree),
                        })
                    }
                    Err(error) => Err(error),
                }
            },
            move |this, result, _, window, cx| {
                this.pulled_branch(branch, upstream, result, window, cx)
            },
            window,
            cx,
        );
    }

    /// Tells how pulling `branch`, not checked out here, from `upstream`
    /// ended.
    fn pulled_branch(
        &mut self,
        branch: String,
        upstream: String,
        result: gitez_command::Result<BranchPull>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open = |this: &mut Self,
                    notification: Notification,
                    worktree: PathBuf,
                    window: &mut Window,
                    cx: &mut Context<Self>| {
            this.notify_with_action(
                notification,
                "Open worktree",
                move |this, window, cx| this.open(worktree.clone(), window, cx),
                window,
                cx,
            )
        };
        match result {
            Ok(BranchPull::Pulled { worktree }) => {
                let done = match worktree {
                    Some(worktree) => format!(
                        "Pulled {upstream} into {branch}, in its worktree {}",
                        crate::recent::display(&worktree)
                    ),
                    None => format!("Pulled {upstream} into {branch}"),
                };
                window.push_notification(crate::toast::success(done), cx)
            }
            Ok(BranchPull::Refused { error, worktree }) => match worktree {
                Some(worktree) if not_fast_forward(&error) => open(
                    self,
                    crate::toast::warning(format!(
                        "{branch} was not pulled: it has commits {upstream} does not. Merge or \
                         rebase them in its worktree, {}.",
                        crate::recent::display(&worktree)
                    )),
                    worktree,
                    window,
                    cx,
                ),
                Some(worktree) if blocked_by_changes(&error) => open(
                    self,
                    crate::toast::warning(format!(
                        "{branch} was not pulled: uncommitted changes in its worktree, {}, are \
                         in the way. Commit or stash them there, then pull again.",
                        crate::recent::display(&worktree)
                    )),
                    worktree,
                    window,
                    cx,
                ),
                None if not_fast_forward(&error) => self.notify_with_action(
                    crate::toast::warning(format!(
                        "{branch} has commits {upstream} does not. Check it out to merge or \
                         rebase them."
                    )),
                    "Check out",
                    move |this, window, cx| this.switch_branch(branch.clone(), window, cx),
                    window,
                    cx,
                ),
                _ => window
                    .push_notification(crate::toast::error(format!("Could not pull: {error}")), cx),
            },
            Err(Error::WorktreeUnavailable { path, problem, .. }) => {
                let path = crate::recent::display(&path);
                let message = match problem {
                    WorktreeProblem::Missing => format!(
                        "{branch} was not pulled: it is checked out in a worktree that no \
                         longer exists, {path}. Run `git worktree prune` to release the branch, \
                         then pull again."
                    ),
                    WorktreeProblem::Locked(None) => format!(
                        "{branch} was not pulled: it is checked out in a locked worktree, \
                         {path}. Unlock the worktree, then pull again."
                    ),
                    WorktreeProblem::Locked(Some(reason)) => format!(
                        "{branch} was not pulled: it is checked out in a locked worktree, \
                         {path} ({reason}). Unlock the worktree, then pull again."
                    ),
                };
                window.push_notification(crate::toast::warning(message), cx)
            }
            Err(error) => window
                .push_notification(crate::toast::error(format!("Could not pull: {error}")), cx),
        }
    }

    pub(super) fn push(&mut self, _: &Push, window: &mut Window, cx: &mut Context<Self>) {
        self.push_with(false, window, cx);
    }

    /// Pushes the branch checked out; with `force`, replaces the remote
    /// branch if it is still where it was last fetched.
    fn push_with(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let Ok(plan) = RemoteActions::of(snapshot).push else {
            return;
        };
        self.push_planned(plan, true, force, window, cx);
    }

    /// Pushes local branch `branch`, checked out or not, as [`Push`] does.
    pub(super) fn push_branch(
        &mut self,
        branch: String,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        if checked_out(snapshot, &branch) {
            return self.push_with(force, window, cx);
        }
        let Ok(plan) = RemoteActions::of_branch(snapshot, Some(&branch)).push else {
            return;
        };
        self.push_planned(plan, false, force, window, cx);
    }

    /// Runs `plan`, for a branch `checked_out` or not.
    fn push_planned(
        &mut self,
        plan: PushPlan,
        checked_out: bool,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (branch, publish_to) = (plan.branch.clone(), plan.publish.then(|| plan.to.clone()));
        self.run_remote(
            RemoteOp::Push,
            RemoteTarget::Branch(plan.branch.clone()),
            None,
            move |git| match publish_to {
                None if !checked_out => git.push_branch(&branch, force),
                publish_to => git.push(&branch, publish_to.as_deref(), force),
            },
            move |this, result, _, window, cx| match result {
                Ok(()) => {
                    let done = if plan.publish {
                        format!("Published {} to {}", plan.branch, plan.to)
                    } else {
                        format!("Pushed {} to {}", plan.branch, plan.to)
                    };
                    window.push_notification(crate::toast::success(done), cx)
                }
                Err(error) => match PushRejection::of(&error) {
                    Some(PushRejection::NotFetched) => this.notify_with_action(
                        crate::toast::warning(format!(
                            "{} has commits not fetched yet. Pull them in, then push.",
                            plan.to
                        )),
                        "Pull",
                        {
                            let branch = plan.branch.clone();
                            move |this, window, cx| this.pull_branch(branch.clone(), window, cx)
                        },
                        window,
                        cx,
                    ),
                    Some(PushRejection::Diverged) if !force => this.notify_with_action(
                        crate::toast::warning(format!(
                            "{} has commits {} does not. Pull them in, or force push to \
                             replace them.",
                            plan.to, plan.branch
                        )),
                        "Force push",
                        {
                            let branch = plan.branch.clone();
                            move |this, window, cx| {
                                this.push_branch(branch.clone(), true, window, cx)
                            }
                        },
                        window,
                        cx,
                    ),
                    _ => window.push_notification(
                        crate::toast::error(format!("Could not push: {error}")),
                        cx,
                    ),
                },
            },
            window,
            cx,
        );
    }

    /// Runs `command` off the UI thread as remote operation `op` on
    /// `target`, unless one is running already, then reads the repository
    /// again; `done` tells about the outcome, unless the operation was
    /// cancelled.
    ///
    /// While it runs, the toolbar and the sidebar row of `target` show it
    /// and offer to cancel it. When git writes something and then does not
    /// exit, what it wrote is shown with the operation still running.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_remote<T: Send + 'static>(
        &mut self,
        op: RemoteOp,
        target: RemoteTarget,
        label: Option<String>,
        command: impl FnOnce(Git) -> gitez_command::Result<T> + Send + 'static,
        done: impl FnOnce(
            &mut Self,
            gitez_command::Result<T>,
            Option<u64>,
            &mut Window,
            &mut Context<Self>,
        ) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(running) = &self.remote_op {
            window.push_notification(
                crate::toast::info(format!(
                    "A {} is still running. Wait for it to finish, or cancel it.",
                    running.op.noun()
                )),
                cx,
            );
            return;
        }
        let Some(dir) = self.snapshot.as_ref().map(|snapshot| snapshot.path.clone()) else {
            return;
        };
        let (said, mut stalled) = futures::channel::mpsc::unbounded::<String>();
        // Called on the thread that runs git; nothing listens once the
        // operation has ended.
        let operation =
            Operation::new().on_stalled(move |text| drop(said.unbounded_send(text.to_owned())));
        let record = label.is_some();
        let run = cx.background_executor().spawn({
            let operation = operation.clone();
            async move {
                let recorder = record.then(|| Recorder::start(&dir));
                let result = command(Git::new(dir).with_operation(&operation));
                let recorded = match (&result, recorder) {
                    (Ok(_), Some(recorder)) => recorder.finish(ResetMode::Keep),
                    _ => None,
                };
                (result, recorded)
            }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let (result, recorded) = run.await;
            this.update_in(cx, |this, window, cx| {
                this.remote_op = None;
                this.show_running_remote(cx);
                window.remove_notification::<StalledRemote>(cx);
                let step = label.and_then(|label| undo::record(cx, recorded, label));
                match result {
                    Err(Error::Cancelled { output, .. }) => {
                        window.push_notification(crate::toast::info(cancelled(op, &output)), cx)
                    }
                    result => done(this, result, step, window, cx),
                }
                this.reload(window, cx);
                this.refresh_status(window, cx);
                cx.notify();
            })
            .ok();
        });
        let stalled = cx.spawn_in(window, async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(text) = stalled.next().await {
                let shown = this.update_in(cx, |this, window, cx| {
                    this.show_stalled_remote(&text, window, cx)
                });
                if shown.is_err() {
                    return;
                }
            }
        });
        self.remote_op = Some(RunningRemote {
            op,
            target,
            operation,
            cancelling: false,
            _run: task,
            _stalled: stalled,
        });
        self.show_running_remote(cx);
        cx.notify();
    }

    /// Ends the fetch, pull or push running, with every process git
    /// started for it. It is over when git has gone, shortly after.
    pub(super) fn cancel_remote(
        &mut self,
        _: &CancelRemote,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(running) = &mut self.remote_op else {
            return;
        };
        if running.cancelling {
            return;
        }
        running.cancelling = true;
        // Returns at once; git is ended on the thread that runs it.
        running.operation.cancel();
        window.remove_notification::<StalledRemote>(cx);
        self.show_running_remote(cx);
        cx.notify();
    }

    /// Shows the running operation, or that none is, in the sidebar.
    fn show_running_remote(&mut self, cx: &mut Context<Self>) {
        let row = self.remote_op.as_ref().map(RunningRemote::row);
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.set_running(row, cx));
    }

    /// Shows `said`, what git wrote before it stopped writing without
    /// exiting, while the operation still runs: it stays until the
    /// operation ends, and offers to cancel it.
    fn show_stalled_remote(&mut self, said: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(running) = &self.remote_op else {
            return;
        };
        if running.cancelling {
            return;
        }
        let notification = self
            .with_action(
                crate::toast::warning(stalled(running.op, said)),
                "Cancel",
                |this, window, cx| this.cancel_remote(&CancelRemote, window, cx),
                cx,
            )
            .id::<StalledRemote>()
            .autohide(false);
        window.push_notification(notification, cx);
    }

    /// Shows `notification` with a button, `label`, that runs `action`.
    pub(super) fn notify_with_action(
        &mut self,
        notification: Notification,
        label: &'static str,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.push_notification(
            self.with_action(notification, label, action, cx)
                // An action turns autohide off; turn it back on so
                // notifications never pile up. Undo stays on ⌘Z after it goes.
                .autohide(true),
            cx,
        );
    }

    /// `notification` with a button, `label`, that runs `action` and
    /// dismisses it. It no longer hides by itself.
    fn with_action(
        &self,
        notification: Notification,
        label: &'static str,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Notification {
        let workspace = cx.entity().downgrade();
        let action = std::rc::Rc::new(action);
        notification.action(move |_, _, cx| {
            let notification = cx.entity().downgrade();
            let (workspace, action) = (workspace.clone(), action.clone());
            Button::new("notification-action")
                .small()
                .label(label)
                .on_click(move |_, window, cx| {
                    workspace
                        .update(cx, |this, cx| action(this, window, cx))
                        .ok();
                    notification
                        .update(cx, |notification, cx| notification.dismiss(window, cx))
                        .ok();
                })
        })
    }
}

/// What to say when remote operation `op` has not finished although git
/// wrote `said`, usually an error, and then nothing more.
fn stalled(op: RemoteOp, said: &str) -> String {
    format!(
        "The {} is still running, but git has not gone on since it wrote this:\n{said}",
        op.noun()
    )
}

/// What to say when remote operation `op` was cancelled after git wrote
/// `said`.
fn cancelled(op: RemoteOp, said: &str) -> String {
    if said.is_empty() {
        format!("Cancelled the {}.", op.noun())
    } else {
        format!("Cancelled the {}. Git had written:\n{said}", op.noun())
    }
}

/// Whether `path` and `other` are the same folder, however they are
/// spelled.
/// Reads the disk, so not for the UI thread.
fn same_folder(path: &Path, other: &Path) -> bool {
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_owned());
    canonical(path) == canonical(other)
}

/// Whether local branch `branch` is the one checked out.
fn checked_out(snapshot: &Snapshot, branch: &str) -> bool {
    matches!(&snapshot.refs.head, Some(Head::Branch { name, .. }) if name == branch)
}

#[cfg(test)]
mod tests {
    use super::{RemoteActions, RemoteOp, cancelled, same_folder, stalled};

    #[test]
    fn says_what_git_wrote_while_running_and_after_a_cancel() {
        assert_eq!(
            stalled(RemoteOp::Pull, "fatal: refusing to fetch"),
            "The pull is still running, but git has not gone on since it wrote this:\n\
             fatal: refusing to fetch"
        );
        assert_eq!(cancelled(RemoteOp::Fetch, ""), "Cancelled the fetch.");
        assert_eq!(
            cancelled(RemoteOp::Push, "fatal: the remote end hung up"),
            "Cancelled the push. Git had written:\nfatal: the remote end hung up"
        );
    }

    #[test]
    fn a_folder_is_the_same_however_it_is_spelled() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let nested = dir.path().join("a");
        std::fs::create_dir(&nested).expect("create");
        assert!(same_folder(&nested.join(".."), dir.path()));
        assert!(!same_folder(&nested, dir.path()));
    }

    fn remotes(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_tracked_branch_pulls_and_pushes_its_upstream() {
        let actions = RemoteActions::new(
            &remotes(&["origin"]),
            Some("main"),
            Some("origin/main"),
            false,
            (2, 1),
        );
        assert_eq!(actions.fetch.as_deref(), Ok("Fetch from origin"));
        assert_eq!(actions.pull.as_deref(), Ok("Pull origin/main into main"));
        let push = actions.push.expect("push");
        assert_eq!(push.describe(), "Push main to origin/main");
        assert_eq!((actions.ahead, actions.behind), (2, 1));
    }

    #[test]
    fn a_new_branch_is_published_to_origin_first() {
        let actions = RemoteActions::new(
            &remotes(&["fork", "origin"]),
            Some("feature"),
            None,
            false,
            (0, 0),
        );
        assert_eq!(actions.fetch.as_deref(), Ok("Fetch from all 2 remotes"));
        assert_eq!(
            actions.pull,
            Err("feature has no upstream to pull from".to_owned())
        );
        let push = actions.push.expect("push");
        assert!(push.publish);
        assert_eq!(push.describe(), "Publish feature to origin");

        let push = RemoteActions::new(&remotes(&["fork"]), Some("feature"), None, false, (0, 0))
            .push
            .expect("push");
        assert_eq!(push.to, "fork");
    }

    #[test]
    fn nothing_runs_without_a_remote_or_a_branch() {
        let actions = RemoteActions::new(&[], Some("main"), None, false, (0, 0));
        assert!(actions.fetch.is_err() && actions.pull.is_err() && actions.push.is_err());

        let actions = RemoteActions::new(&remotes(&["origin"]), None, None, false, (0, 0));
        assert!(actions.fetch.is_ok());
        assert_eq!(actions.push, Err("No branch is checked out".to_owned()));
    }
}
