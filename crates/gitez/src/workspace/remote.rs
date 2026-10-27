//! Fetch, pull and push: when each can run, and what it does.

use gitez_command::{
    Git, PullMethod, PullOutcome, PushRejection, ResetMode, needs_pull_method, not_fast_forward,
};
use gitez_git::Head;
use gpui_kit::component::button::*;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::*;
use gpui_kit::*;

use super::{Fetch, Pull, Push, Workspace};
use crate::repository::Snapshot;
use crate::undo::{self, Recorder};

/// One of the toolbar's remote operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteOp {
    Fetch,
    Pull,
    Push,
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
            None,
            |git| git.fetch(),
            move |_, result, _, window, cx| {
                let notification = match result {
                    // "Fetch from origin" becomes "Fetched from origin".
                    Ok(()) => Notification::success(description.replacen("Fetch", "Fetched", 1)),
                    Err(error) => Notification::error(format!("Could not fetch: {error}")),
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
        self.run_remote(
            RemoteOp::Pull,
            Some(label),
            move |git| git.pull(method),
            |this, result, step, window, cx| match result {
                Ok(PullOutcome::Pulled) => {
                    this.notify_done(Notification::success("Pulled"), step, window, cx)
                }
                Ok(PullOutcome::Conflicts) => window.push_notification(
                    Notification::warning(
                        "The pull stopped on conflicts. Resolve them to finish it, or abort it.",
                    ),
                    cx,
                ),
                Err(error) if needs_pull_method(&error) => this.notify_with_action(
                    Notification::warning(
                        "The branch and its upstream have diverged, and git is not set to \
                         merge or rebase them (pull.rebase).",
                    ),
                    "Merge them",
                    |this, window, cx| this.pull_with(PullMethod::Merge, window, cx),
                    window,
                    cx,
                ),
                Err(error) => window
                    .push_notification(Notification::error(format!("Could not pull: {error}")), cx),
            },
            window,
            cx,
        );
    }

    /// Brings the upstream's new commits into local branch `branch`: as
    /// [`Pull`] does for the branch checked out, and only by fast-forwarding
    /// for any other.
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
            None,
            move |git| git.pull_branch(&name),
            move |this, result, _, window, cx| match result {
                Ok(()) => window.push_notification(
                    Notification::success(format!("Pulled {upstream} into {branch}")),
                    cx,
                ),
                Err(error) if not_fast_forward(&error) => this.notify_with_action(
                    Notification::warning(format!(
                        "{branch} has commits {upstream} does not. Check it out to merge or \
                         rebase them."
                    )),
                    "Check out",
                    move |this, window, cx| this.switch_branch(branch.clone(), window, cx),
                    window,
                    cx,
                ),
                Err(error) => window
                    .push_notification(Notification::error(format!("Could not pull: {error}")), cx),
            },
            window,
            cx,
        );
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
                    window.push_notification(Notification::success(done), cx)
                }
                Err(error) => match PushRejection::of(&error) {
                    Some(PushRejection::NotFetched) => this.notify_with_action(
                        Notification::warning(format!(
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
                        Notification::warning(format!(
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
                        Notification::error(format!("Could not push: {error}")),
                        cx,
                    ),
                },
            },
            window,
            cx,
        );
    }

    /// Runs `command` off the UI thread as remote operation `op`, unless one
    /// is running already, then reads the repository again; `done` tells
    /// about the outcome.
    fn run_remote<T: Send + 'static>(
        &mut self,
        op: RemoteOp,
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
        if self.remote_op.is_some() {
            return;
        }
        let Some(dir) = self.snapshot.as_ref().map(|snapshot| snapshot.path.clone()) else {
            return;
        };
        let record = label.is_some();
        let run = cx.background_executor().spawn(async move {
            let recorder = record.then(|| Recorder::start(&dir));
            let result = command(Git::new(dir));
            let recorded = match (&result, recorder) {
                (Ok(_), Some(recorder)) => recorder.finish(ResetMode::Keep),
                _ => None,
            };
            (result, recorded)
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let (result, recorded) = run.await;
            this.update_in(cx, |this, window, cx| {
                this.remote_op = None;
                let step = label.and_then(|label| undo::record(cx, recorded, label));
                done(this, result, step, window, cx);
                this.reload(window, cx);
                this.refresh_status(window, cx);
                cx.notify();
            })
            .ok();
        });
        self.remote_op = Some((op, task));
        cx.notify();
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
        let workspace = cx.entity().downgrade();
        let action = std::rc::Rc::new(action);
        window.push_notification(
            notification
                .action(move |_, _, cx| {
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
                // An action turns autohide off; turn it back on so
                // notifications never pile up. Undo stays on ⌘Z after it goes.
                .autohide(true),
            cx,
        );
    }
}

/// Whether local branch `branch` is the one checked out.
fn checked_out(snapshot: &Snapshot, branch: &str) -> bool {
    matches!(&snapshot.refs.head, Some(Head::Branch { name, .. }) if name == branch)
}

#[cfg(test)]
mod tests {
    use super::RemoteActions;

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
