//! Changes to git repositories for GitEZ.
//!
//! Every change runs the user's own `git` binary, so hooks, signing and
//! credential helpers behave exactly as they do in the terminal. Reading the
//! repository is `gitez-git`'s job.
//!
//! The methods of [`Git`] wait for `git` to finish, so call them off the UI
//! thread. A git that never finishes, because a process it started (ssh, a
//! hook) is still there, is ended with an [`Operation`]: see
//! [`Git::with_operation`].

mod run;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

pub use run::{CANCEL_GRACE, Operation, STALLED_AFTER};
use run::{End, Stream};

/// Errors from running `git`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not run git")]
    Spawn(#[source] std::io::Error),
    #[error("could not write {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// `git` ran and failed. `output` is what it wrote, often the output of a
    /// hook that rejected the change.
    #[error("{output}")]
    Failed { command: String, output: String },
    /// The command was ended with [`Operation::cancel`], or never started
    /// because the operation was cancelled already. `output` is what git
    /// had written to standard error by then; it is often why git was
    /// still running.
    #[error("cancelled")]
    Cancelled { command: String, output: String },
    /// Branch `branch` is checked out in a worktree that cannot be used, so
    /// nothing was changed: see [`Git::pull_branch`].
    #[error("{branch} is checked out in the worktree at {}, which {problem}", .path.display())]
    WorktreeUnavailable {
        branch: String,
        path: PathBuf,
        problem: WorktreeProblem,
    },
}

impl Error {
    /// Whether the command was ended with [`Operation::cancel`].
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled { .. })
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Why git cannot be run in a worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeProblem {
    /// Its folder is gone; git still holds the branch for it until
    /// `git worktree prune`.
    Missing,
    /// It is locked (`git worktree lock`), with the reason given, if one
    /// was.
    Locked(Option<String>),
}

impl std::fmt::Display for WorktreeProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("no longer exists"),
            Self::Locked(None) => f.write_str("is locked"),
            Self::Locked(Some(reason)) => write!(f, "is locked ({reason})"),
        }
    }
}

/// A working tree of the repository, as `git worktree list` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// The local branch checked out in it, such as `main`; `None` when it
    /// is on no branch or has no working tree (a bare repository).
    pub branch: Option<String>,
    /// Why git cannot be run in it, if it cannot.
    pub problem: Option<WorktreeProblem>,
}

/// How [`Git::merge_branch`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged,
    /// The merge stopped with conflicts to resolve.
    Conflicts,
}

/// One side of a merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The branch checked out.
    Ours,
    /// The branch merged in.
    Theirs,
}

/// How [`Git::merge_branch`] brings a branch's commits into another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMethod {
    /// A merge commit, even when a fast-forward is possible
    /// (`git merge --no-ff`).
    Merge,
    /// One new commit with all the changes (`git merge --squash`).
    Squash,
    /// The branch's commits replayed on top of the other branch, which
    /// then fast-forwards to them (`git rebase`).
    Rebase,
    /// The other branch moved to the branch's last commit; only possible
    /// when it has no commits of its own (`git merge --ff-only`).
    FastForward,
}

/// How [`Git::cherry_pick`] applies commits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PickOptions {
    /// Adds "(cherry picked from commit …)" to each message (`-x`).
    pub note_source: bool,
    /// Leaves the changes staged instead of committing them (`-n`).
    pub no_commit: bool,
    /// Stashes uncommitted changes first and brings them back afterwards.
    pub stash: bool,
}

/// How [`Git::commit_with`] commits, beyond the message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommitOptions {
    /// Replaces the last commit instead of adding one (`--amend`).
    pub amend: bool,
    /// Skips the pre-commit and commit-msg hooks (`--no-verify`).
    pub no_verify: bool,
    /// Adds a Signed-off-by trailer with the committer (`--signoff`).
    pub sign_off: bool,
}

/// What [`Git::rewrite`] does with one commit, as in `git rebase -i`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewriteAction {
    /// Keeps it as it is.
    Pick,
    /// Keeps it with this message.
    Reword(String),
    /// Stops after it, so it can be changed.
    Edit,
    /// Folds it into the commit before, joining their messages.
    Squash,
    /// Folds it into the commit before, keeping that commit's message.
    Fixup,
    /// Leaves it out.
    Drop,
}

/// One line of a rewrite: a commit (its full id) and what to do with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteStep {
    pub commit: String,
    pub action: RewriteAction,
}

/// How [`Git::rewrite`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteOutcome {
    Done,
    /// Stopped at an `edit`; [`Git::continue_rebase`] goes on.
    Stopped,
}

/// How [`Git::pull`] brings in the upstream's commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullMethod {
    /// As the user's configuration says (`pull.rebase`, `pull.ff`).
    Configured,
    /// A merge commit when the branches have diverged (`--no-rebase`).
    Merge,
    /// Only by moving the branch forward; refused, changing nothing, when
    /// the branch has commits its upstream lacks (`--ff-only`).
    FastForward,
}

/// How [`Git::pull`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullOutcome {
    Pulled,
    /// The merge or rebase stopped with conflicts to resolve.
    Conflicts,
}

/// Why `git push` refused to replace a remote branch, read from what it
/// wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushRejection {
    /// The remote has commits not fetched yet ("fetch first").
    NotFetched,
    /// The branch and the remote one have diverged, as after rewriting
    /// pushed commits.
    Diverged,
}

impl PushRejection {
    /// Why `error`, from [`Git::push`], is a rejection, if it is one.
    pub fn of(error: &Error) -> Option<Self> {
        let Error::Failed { output, .. } = error else {
            return None;
        };
        if !output.contains("[rejected]") {
            return None;
        }
        if output.contains("(fetch first)") {
            Some(Self::NotFetched)
        } else {
            Some(Self::Diverged)
        }
    }
}

/// Whether `error`, from [`Git::pull`], says the branches have diverged
/// and the configuration does not say how to bring them together.
pub fn needs_pull_method(error: &Error) -> bool {
    matches!(error, Error::Failed { output, .. } if output.contains("divergent branches"))
}

/// Whether `error`, from [`Git::pull_branch`], says the branch has commits
/// its upstream lacks, so it cannot be fast-forwarded.
pub fn not_fast_forward(error: &Error) -> bool {
    // A fetch into the branch says the first, a pull in its worktree the
    // second.
    matches!(error, Error::Failed { output, .. }
        if output.contains("[rejected]") || output.contains("Not possible to fast-forward"))
}

/// Whether `error`, from [`Git::pull_branch`], says uncommitted changes in
/// the branch's working tree are in the way of the new commits.
pub fn blocked_by_changes(error: &Error) -> bool {
    matches!(error, Error::Failed { output, .. }
        if output.contains("would be overwritten by merge"))
}

/// Whether `error`, from [`Git::delete_branch`], says the branch has
/// commits that would be lost.
pub fn not_merged(error: &Error) -> bool {
    matches!(error, Error::Failed { output, .. } if output.contains("not fully merged"))
}

/// How [`Git::unstash`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnstashOutcome {
    Applied,
    /// The changes conflict with the working tree; the stash is kept.
    Conflicts,
}

/// One change to a reference, made only if the reference is still where it
/// was expected: see [`Git::update_refs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    /// The full name, such as `refs/heads/main`.
    pub name: String,
    /// The commit it must be on now (a full id), or `None` if it must not
    /// exist.
    pub old: Option<String>,
    /// The commit to move it to, or `None` to delete it.
    pub new: Option<String>,
}

/// What [`Git::reset`] does to the index and the files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    /// Leaves both as they are, so the difference shows as staged changes
    /// (`--soft`).
    Soft,
    /// Leaves the files as they are and unstages the difference
    /// (`--mixed`).
    Mixed,
    /// Makes the index and the tracked files match the commit, throwing
    /// away their changes (`--hard`).
    Hard,
    /// Updates the files that differ between the commits and keeps local
    /// changes, refusing if one would be lost (`--keep`).
    Keep,
}

/// How [`Git::revert`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevertOutcome {
    Reverted,
    /// It conflicted with the branch, so nothing changed.
    Conflicts,
}

/// Runs `git` in one working tree.
#[derive(Debug, Clone)]
pub struct Git {
    work_dir: PathBuf,
    /// Set for every git it runs, after the environment of GitEZ itself.
    env: Vec<(OsString, OsString)>,
    /// What can cancel the git it runs.
    operation: Option<Operation>,
    /// Whether it undoes a half-done change, and so runs even after the
    /// operation was cancelled.
    undoing: bool,
}

impl Git {
    /// Runs `git` in `work_dir`, a repository's working tree.
    pub fn new(work_dir: impl Into<PathBuf>) -> Self {
        Self {
            work_dir: work_dir.into(),
            env: Vec::new(),
            operation: None,
            undoing: false,
        }
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// Makes every git this runs part of `operation`, which can cancel it
    /// and knows what it has written: see [`Operation`]. Every git runs in
    /// its own process group either way.
    #[must_use]
    pub fn with_operation(mut self, operation: &Operation) -> Self {
        self.operation = Some(operation.clone());
        self
    }

    /// Sets environment variable `key` to `value` for every git this runs,
    /// such as `GIT_SSH_COMMAND`.
    #[must_use]
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// The same in the working tree `work_dir` of the same repository.
    fn in_work_dir(&self, work_dir: &Path) -> Self {
        Self {
            work_dir: work_dir.to_owned(),
            ..self.clone()
        }
    }

    /// The same for undoing a half-done change: what it runs starts even
    /// after the operation was cancelled, so a cancelled command leaves
    /// nothing half done. Cancelling again ends that too.
    fn undoing(&self) -> Self {
        Self {
            undoing: true,
            ..self.clone()
        }
    }

    /// Clones `url` into `dest`, which must not exist yet or be empty, like
    /// `git clone`. The folder `dest` is in must exist. Returns the clone.
    pub fn clone_repository(url: &str, dest: &Path) -> Result<Self> {
        let parent = dest.parent().unwrap_or(dest);
        Self::new(parent)
            .run(
                [
                    OsStr::new("clone"),
                    OsStr::new("--quiet"),
                    OsStr::new("--"),
                    OsStr::new(url),
                    dest.as_os_str(),
                ],
                None,
            )
            .map(|_| Self::new(dest))
    }

    /// Makes `dir`, which must exist, a new repository with no commits,
    /// like `git init`. Returns it.
    pub fn init_repository(dir: &Path) -> Result<Self> {
        let git = Self::new(dir);
        git.run(["init", "--quiet"], None).map(|_| git)
    }

    /// Stages every change to `paths`, including deletions and untracked
    /// files, like `git add`. Paths are relative to the working tree and
    /// taken literally, not as patterns.
    pub fn stage(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        self.run_with_paths(["add", "--all"], paths)
    }

    /// Stages every change in the working tree, like `git add --all`.
    pub fn stage_all(&self) -> Result<()> {
        self.run(["add", "--all"], None).map(drop)
    }

    /// Unstages `paths`, keeping the changes in the working tree, like
    /// `git reset -- <paths>`. Works before the first commit too.
    pub fn unstage(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        self.run_with_paths(["reset", "--quiet"], paths)
    }

    /// Unstages everything, like `git reset`.
    pub fn unstage_all(&self) -> Result<()> {
        self.run(["reset", "--quiet"], None).map(drop)
    }

    /// Commits the staged changes with `message`. Hooks run as usual; when
    /// one rejects the commit, its output is the error.
    pub fn commit(&self, message: &str) -> Result<()> {
        self.commit_with(message, CommitOptions::default())
    }

    /// Commits the working-tree content of `paths`, which git must already
    /// track, with `message`, like `git commit --only`; other staged changes
    /// stay staged and out of the commit.
    pub fn commit_only(&self, message: &str, paths: &[String]) -> Result<()> {
        let args = ["commit", "--quiet", "--file=-", "--only", "--"]
            .into_iter()
            .map(str::to_owned)
            .chain(paths.iter().cloned());
        self.run(args, Some(message.as_bytes())).map(drop)
    }

    /// Commits the staged changes with `message` as `options` say. Hooks
    /// run unless skipped; when one rejects the commit, its output is the
    /// error.
    pub fn commit_with(&self, message: &str, options: CommitOptions) -> Result<()> {
        let mut args = vec!["commit", "--quiet", "--file=-"];
        if options.amend {
            args.push("--amend");
        }
        if options.no_verify {
            args.push("--no-verify");
        }
        if options.sign_off {
            args.push("--signoff");
        }
        self.run(args, Some(message.as_bytes())).map(drop)
    }

    /// Checks out the local branch `branch`, like `git switch`. Fails, with
    /// git's explanation, when uncommitted changes would be overwritten.
    pub fn switch(&self, branch: &str) -> Result<()> {
        self.run(["switch", "--quiet", branch], None).map(drop)
    }

    /// Creates branch `name` at commit `start` (a full id or a ref), like
    /// `git branch name start`, and checks it out if `checkout`, like
    /// `git switch -c name start`.
    pub fn create_branch(&self, name: &str, start: &str, checkout: bool) -> Result<()> {
        let args = if checkout {
            ["switch", "--quiet", "--create", name, start]
        } else {
            ["branch", "--quiet", "--no-track", name, start]
        };
        self.run(args, None).map(drop)
    }

    /// Renames branch `from` to `to`, like `git branch -m`; its upstream
    /// and reflog go with it.
    pub fn rename_branch(&self, from: &str, to: &str) -> Result<()> {
        self.run(["branch", "--move", from, to], None).map(drop)
    }

    /// Deletes branch `name`, like `git branch -d`, which refuses while
    /// its commits are on neither its upstream nor the branch checked out
    /// (see [`not_merged`]); `force` deletes it anyway (`-D`).
    pub fn delete_branch(&self, name: &str, force: bool) -> Result<()> {
        let flag = if force { "-D" } else { "-d" };
        self.run(["branch", flag, name], None).map(drop)
    }

    /// Creates local branch `name` from remote-tracking branch `remote`/
    /// `name`, set to track it, and checks it out, like `git switch
    /// --track`.
    pub fn track(&self, remote: &str, name: &str) -> Result<()> {
        let upstream = format!("refs/remotes/{remote}/{name}");
        self.run(
            ["switch", "--quiet", "--create", name, "--track", &upstream],
            None,
        )
        .map(drop)
    }

    /// Deletes branch `name` on `remote`, like `git push <remote>
    /// --delete`, and the remote-tracking branch with it. Credential
    /// helpers and hooks run as usual.
    pub fn delete_remote_branch(&self, remote: &str, name: &str) -> Result<()> {
        let branch = format!("refs/heads/{name}");
        self.run(["push", "--quiet", remote, "--delete", &branch], None)
            .map(drop)
    }

    /// Creates tag `name` on `commit` (a full id or a ref): annotated, with
    /// `message`, when there is one, like `git tag -a`; lightweight
    /// otherwise.
    pub fn create_tag(&self, name: &str, commit: &str, message: Option<&str>) -> Result<()> {
        match message {
            Some(message) => self.run(
                ["tag", "--annotate", "--file=-", name, commit],
                Some(message.as_bytes()),
            ),
            None => self.run(["tag", name, commit], None),
        }
        .map(drop)
    }

    /// Deletes tag `name`, like `git tag -d`.
    pub fn delete_tag(&self, name: &str) -> Result<()> {
        self.run(["tag", "--delete", name], None).map(drop)
    }

    /// Sends tag `name` to `remote`, like `git push <remote> <tag>`.
    pub fn push_tag(&self, remote: &str, name: &str) -> Result<()> {
        let tag = format!("refs/tags/{name}");
        self.run(["push", "--quiet", remote, &tag], None).map(drop)
    }

    /// Deletes tag `name` on `remote`, like `git push <remote> --delete`.
    pub fn delete_remote_tag(&self, remote: &str, name: &str) -> Result<()> {
        let tag = format!("refs/tags/{name}");
        self.run(["push", "--quiet", remote, "--delete", &tag], None)
            .map(drop)
    }

    /// Brings the commits of branch `head` into branch `base` the way `how`
    /// says, and leaves `base` checked out. A merge commit that conflicts
    /// stops with the conflicts in the working tree, to be resolved and
    /// committed with [`Git::commit_merge`] or given up with
    /// [`Git::abort_merge`]. Otherwise, when git cannot finish, because of a
    /// conflict or a hook, everything it did is undone and `return_to`, the
    /// branch checked out before, is checked out again.
    pub fn merge_branch(
        &self,
        head: &str,
        base: &str,
        how: MergeMethod,
        message: &str,
        return_to: Option<&str>,
    ) -> Result<MergeOutcome> {
        let merged = self.merge_steps(head, base, how, message);
        if merged.is_err()
            && let Some(branch) = return_to
        {
            // Best effort: the error that stopped the merge is the one to
            // report.
            self.undoing().switch(branch).ok();
        }
        merged
    }

    fn merge_steps(
        &self,
        head: &str,
        base: &str,
        how: MergeMethod,
        message: &str,
    ) -> Result<MergeOutcome> {
        if how == MergeMethod::Rebase {
            // Checks out `head` and replays its commits on top of `base`.
            if let Err(error) = self.run(["rebase", "--quiet", base, head], None) {
                self.undoing().run(["rebase", "--abort"], None).ok();
                return Err(error);
            }
        }
        self.switch(base)?;
        let merged = match how {
            MergeMethod::Merge => {
                // With the base's lines in each conflict, to show them.
                let merged = self.run(
                    [
                        "-c",
                        "merge.conflictStyle=diff3",
                        "merge",
                        "--quiet",
                        "--no-ff",
                        "-m",
                        message,
                        head,
                    ],
                    None,
                );
                // A cancelled merge is undone below, not left to resolve.
                if matches!(&merged, Err(error) if !error.is_cancelled())
                    && !self.conflicted()?.is_empty()
                {
                    return Ok(MergeOutcome::Conflicts);
                }
                merged.map(drop)
            }
            MergeMethod::Squash => self
                .run(["merge", "--quiet", "--squash", head], None)
                .and_then(|_| self.commit(message)),
            MergeMethod::FastForward | MergeMethod::Rebase => self
                .run(["merge", "--quiet", "--ff-only", head], None)
                .map(drop),
        };
        if merged.is_err() {
            // As `git merge --abort` does; a squash leaves no merge to
            // abort, so this undoes it too. Git refused to start if
            // uncommitted changes were in the way, so none are lost.
            self.undoing()
                .run(["reset", "--quiet", "--merge"], None)
                .ok();
        }
        merged.map(|()| MergeOutcome::Merged)
    }

    /// Replays the commits of branch `branch` on top of branch `onto`, like
    /// `git rebase onto branch`; `onto` does not move. When git cannot
    /// finish, the rebase is given up and nothing changes. Afterwards
    /// `return_to`, the branch checked out before, is checked out again.
    pub fn rebase_branch(&self, branch: &str, onto: &str, return_to: Option<&str>) -> Result<()> {
        let rebased = self.run(["rebase", "--quiet", onto, branch], None);
        if rebased.is_err() {
            self.undoing().run(["rebase", "--abort"], None).ok();
        }
        if let Some(return_to) = return_to
            && return_to != branch
        {
            let switched = self.undoing().switch(return_to);
            // The error that stopped the rebase is the one to report.
            if rebased.is_ok() {
                switched?;
            }
        }
        rebased.map(drop)
    }

    /// Applies `commits` (full ids, oldest first) to branch `onto` and
    /// leaves it checked out, like `git switch onto` then
    /// `git cherry-pick commits`. When git cannot finish, because of a
    /// conflict or a hook, the cherry-pick is given up and `return_to`, the
    /// branch checked out before, is checked out again. Stashed changes come
    /// back either way; if they cannot, they stay in the stash and that is
    /// the error.
    pub fn cherry_pick(
        &self,
        commits: &[String],
        onto: &str,
        options: PickOptions,
        return_to: Option<&str>,
    ) -> Result<()> {
        if commits.is_empty() {
            return Ok(());
        }
        let stashed = options.stash && self.stash_push("GitEZ: before cherry-picking")?;
        let mut picked = self.switch(onto).and_then(|()| {
            let mut args = vec!["cherry-pick"];
            if options.note_source {
                args.push("-x");
            }
            if options.no_commit {
                args.push("--no-commit");
            }
            args.extend(commits.iter().map(String::as_str));
            let picked = self.run(args, None).map(drop);
            let undo = self.undoing();
            if picked.is_err() && undo.run(["cherry-pick", "--abort"], None).is_err() {
                // Nothing to abort after `--no-commit`: undo what it staged.
                undo.run(["reset", "--quiet", "--merge"], None).ok();
            }
            picked
        });
        if picked.is_err()
            && let Some(branch) = return_to
        {
            // Best effort: the error that stopped the cherry-pick is the one
            // to report.
            self.undoing().switch(branch).ok();
        }
        if stashed && let Err(error) = self.undoing().run(["stash", "pop", "--quiet"], None) {
            let Error::Failed { command, output } = error else {
                return Err(error);
            };
            let output = format!(
                "Your uncommitted changes could not be brought back; they are kept in the \
                 stash.\n{output}"
            );
            picked = picked.and(Err(Error::Failed { command, output }));
        }
        picked
    }

    /// Fetches from every remote, like `git fetch --all --prune`, so
    /// branches deleted on a remote disappear from its remote branches.
    pub fn fetch(&self) -> Result<()> {
        self.run(["fetch", "--all", "--prune", "--quiet"], None)
            .map(drop)
    }

    /// Fetches remote `name` alone, like `git fetch --prune <name>`.
    pub fn fetch_remote(&self, name: &str) -> Result<()> {
        self.run(["fetch", "--prune", "--quiet", name], None)
            .map(drop)
    }

    /// Adds remote `name` fetching from `url`, like `git remote add`. It is
    /// not fetched.
    pub fn add_remote(&self, name: &str, url: &str) -> Result<()> {
        self.run(["remote", "add", "--", name, url], None).map(drop)
    }

    /// Renames remote `from` to `to`, like `git remote rename`: its
    /// remote-tracking branches and the upstreams that name it follow.
    pub fn rename_remote(&self, from: &str, to: &str) -> Result<()> {
        self.run(["remote", "rename", "--", from, to], None)
            .map(drop)
    }

    /// Points remote `name` at `url`, like `git remote set-url`.
    pub fn set_remote_url(&self, name: &str, url: &str) -> Result<()> {
        self.run(["remote", "set-url", "--", name, url], None)
            .map(drop)
    }

    /// Removes remote `name`, like `git remote remove`, with its
    /// remote-tracking branches and the upstreams that name it.
    pub fn remove_remote(&self, name: &str) -> Result<()> {
        self.run(["remote", "remove", "--", name], None).map(drop)
    }

    /// Sets the upstream of local branch `branch` to remote-tracking branch
    /// `upstream` (such as `origin/main`), like `git branch
    /// --set-upstream-to`; `None` unsets it.
    pub fn set_upstream(&self, branch: &str, upstream: Option<&str>) -> Result<()> {
        match upstream {
            Some(upstream) => {
                let flag = format!("--set-upstream-to=refs/remotes/{upstream}");
                self.run(["branch", "--quiet", &flag, "--", branch], None)
            }
            None => self.run(["branch", "--unset-upstream", "--", branch], None),
        }
        .map(drop)
    }

    /// Brings the upstream's new commits into the branch checked out, like
    /// `git pull`. A merge or rebase that conflicts stops with the conflicts
    /// in the working tree, to be resolved as after any merge or rebase.
    pub fn pull(&self, method: PullMethod) -> Result<PullOutcome> {
        match self.run_pull(method) {
            Ok(()) => Ok(PullOutcome::Pulled),
            // Also when the pull was cancelled after it stopped.
            Err(_)
                if self
                    .undoing()
                    .conflicted()
                    .is_ok_and(|paths| !paths.is_empty()) =>
            {
                Ok(PullOutcome::Conflicts)
            }
            Err(error) => Err(error),
        }
    }

    /// `git pull` in this working tree, the way `method` says.
    fn run_pull(&self, method: PullMethod) -> Result<()> {
        let mut args = vec!["pull", "--quiet"];
        match method {
            PullMethod::Configured => {}
            PullMethod::Merge => args.push("--no-rebase"),
            PullMethod::FastForward => args.push("--ff-only"),
        }
        // No editor can open for a merge message.
        self.run_with_env(args, None, &[("GIT_MERGE_AUTOEDIT", "no")])
            .map(drop)
    }

    /// Sends branch `branch`, checked out, to its upstream, like `git push`.
    /// When `publish_to` names a remote, the branch has no upstream yet: it
    /// goes there under its own name and that becomes its upstream
    /// (`--set-upstream`). With `force`, the remote branch is replaced, but
    /// only if it is still where it was last fetched (`--force-with-lease`).
    pub fn push(&self, branch: &str, publish_to: Option<&str>, force: bool) -> Result<()> {
        let mut args = vec!["push", "--quiet"];
        if force {
            args.push("--force-with-lease");
        }
        if let Some(remote) = publish_to {
            args.extend(["--set-upstream", remote, branch]);
        }
        self.run(args, None).map(drop)
    }

    /// Fast-forwards branch `branch` to its upstream on the remote, where
    /// the branch is.
    ///
    /// Checked out nowhere, it is fetched into, like `git fetch <remote>
    /// <upstream>:<branch>`. Checked out in a working tree, this one or
    /// another, it is pulled there, like `git pull --ff-only` in that
    /// folder: git refuses to fetch into a branch that is checked out, and
    /// its files have to follow it. Returns that working tree, if there is
    /// one.
    ///
    /// Fails, changing nothing, when the branch has commits its upstream
    /// lacks (see [`not_fast_forward`]), when uncommitted changes in its
    /// working tree are in the way (see [`blocked_by_changes`]), when that
    /// working tree is missing or locked
    /// ([`Error::WorktreeUnavailable`]), or when the branch has no
    /// upstream.
    pub fn pull_branch(&self, branch: &str) -> Result<Option<PathBuf>> {
        let (remote, merge) = self.upstream_of(branch)?;
        if let Some(worktree) = self.worktree_of(branch)? {
            if let Some(problem) = worktree.problem {
                return Err(Error::WorktreeUnavailable {
                    branch: branch.to_owned(),
                    path: worktree.path,
                    problem,
                });
            }
            self.in_work_dir(&worktree.path)
                .run_pull(PullMethod::FastForward)?;
            return Ok(Some(worktree.path));
        }
        let refspec = format!("{merge}:refs/heads/{branch}");
        // Not `--quiet`: it hides the line saying why a fetch was rejected.
        self.run(["fetch", &remote, &refspec], None)?;
        Ok(None)
    }

    /// The working tree local branch `branch` is checked out in, if it is
    /// checked out: this one, the repository's main one or a linked one.
    pub fn worktree_of(&self, branch: &str) -> Result<Option<Worktree>> {
        Ok(self
            .worktrees()?
            .into_iter()
            .find(|worktree| worktree.branch.as_deref() == Some(branch)))
    }

    /// The repository's working trees, the main one first, like `git
    /// worktree list`.
    pub fn worktrees(&self) -> Result<Vec<Worktree>> {
        // `-z` keeps a path with a line break in it whole; git before 2.36
        // does not know it.
        let (output, end) = match self.run(["worktree", "list", "--porcelain", "-z"], None) {
            Ok(output) => (output, b'\0'),
            Err(Error::Failed { .. }) => {
                (self.run(["worktree", "list", "--porcelain"], None)?, b'\n')
            }
            Err(error) => return Err(error),
        };
        Ok(parse_worktrees(&output, end))
    }

    /// Sends branch `branch`, checked out or not, to its upstream, like
    /// `git push <remote> <branch>:<upstream>`. With `force`, the remote
    /// branch is replaced if it is still where it was last fetched
    /// (`--force-with-lease`). Rejections are those of [`Git::push`].
    pub fn push_branch(&self, branch: &str, force: bool) -> Result<()> {
        let (remote, merge) = self.upstream_of(branch)?;
        let refspec = format!("refs/heads/{branch}:{merge}");
        let mut args = vec!["push", "--quiet"];
        if force {
            args.push("--force-with-lease");
        }
        args.extend([remote.as_str(), &refspec]);
        self.run(args, None).map(drop)
    }

    /// The remote local branch `branch` tracks and the branch there, such
    /// as `origin` and `refs/heads/main`.
    fn upstream_of(&self, branch: &str) -> Result<(String, String)> {
        let output = self.run(
            [
                "for-each-ref",
                "--format=%(upstream:remotename)%00%(upstream:remoteref)",
                &format!("refs/heads/{branch}"),
            ],
            None,
        )?;
        let output = String::from_utf8_lossy(&output);
        match output.trim_end().split_once('\0') {
            Some((remote, merge)) if !remote.is_empty() && !merge.is_empty() => {
                Ok((remote.to_owned(), merge.to_owned()))
            }
            _ => Err(Error::Failed {
                command: format!("git for-each-ref refs/heads/{branch}"),
                output: format!("{branch} has no upstream"),
            }),
        }
    }

    /// Brings the changes of stash `index` (`stash@{index}`) back into the
    /// working tree, like `git stash apply`, and with `pop` drops it once
    /// they apply cleanly. On a conflict the stash is kept and the
    /// conflicts stay in the working tree.
    pub fn unstash(&self, index: usize, pop: bool) -> Result<UnstashOutcome> {
        let stash = format!("stash@{{{index}}}");
        let command = if pop { "pop" } else { "apply" };
        match self.run(["stash", command, "--quiet", &stash], None) {
            Ok(_) => Ok(UnstashOutcome::Applied),
            Err(_)
                if self
                    .undoing()
                    .conflicted()
                    .is_ok_and(|paths| !paths.is_empty()) =>
            {
                Ok(UnstashOutcome::Conflicts)
            }
            Err(error) => Err(error),
        }
    }

    /// Removes stash `index`, like `git stash drop`.
    pub fn drop_stash(&self, index: usize) -> Result<()> {
        self.run(
            ["stash", "drop", "--quiet", &format!("stash@{{{index}}}")],
            None,
        )
        .map(drop)
    }

    /// Puts the stash commit `id` (a full id), dropped earlier, back on
    /// top of the stashes with `message`, like `git stash store`.
    pub fn restore_stash(&self, id: &str, message: &str) -> Result<()> {
        self.run(["stash", "store", "--quiet", "-m", message, id], None)
            .map(drop)
    }

    /// Makes `updates` together or not at all, like `git update-ref
    /// --stdin` in one transaction. Fails, changing nothing, if any
    /// reference is not where `old` says.
    pub fn update_refs(&self, updates: &[RefUpdate]) -> Result<()> {
        let mut input = String::from("start\n");
        for update in updates {
            let name = &update.name;
            let line = match (&update.old, &update.new) {
                (None, Some(new)) => format!("create {name} {new}\n"),
                (Some(old), Some(new)) => format!("update {name} {new} {old}\n"),
                (Some(old), None) => format!("delete {name} {old}\n"),
                (None, None) => continue,
            };
            input.push_str(&line);
        }
        input.push_str("commit\n");
        self.run(["update-ref", "--stdin"], Some(input.as_bytes()))
            .map(drop)
    }

    /// Moves the branch checked out to `commit`, like `git reset`, the way
    /// `mode` says.
    pub fn reset(&self, commit: &str, mode: ResetMode) -> Result<()> {
        let flag = match mode {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
            ResetMode::Keep => "--keep",
        };
        self.run(["reset", "--quiet", flag, commit], None).map(drop)
    }

    /// Adds a commit that takes back what `commit` changed, like `git
    /// revert --no-edit`; `mainline` picks the parent of a merge commit to
    /// keep (`-m`). A revert that conflicts is given up, changing nothing.
    pub fn revert(&self, commit: &str, mainline: Option<u32>) -> Result<RevertOutcome> {
        let mainline = mainline.map(|parent| parent.to_string());
        let mut args = vec!["revert", "--no-edit"];
        if let Some(parent) = &mainline {
            args.extend(["-m", parent]);
        }
        args.push(commit);
        match self.run(args, None) {
            Ok(_) => Ok(RevertOutcome::Reverted),
            Err(_)
                if self
                    .undoing()
                    .conflicted()
                    .is_ok_and(|paths| !paths.is_empty()) =>
            {
                self.undoing().run(["revert", "--abort"], None)?;
                Ok(RevertOutcome::Conflicts)
            }
            Err(error) => Err(error),
        }
    }

    /// Checks out `commit` without a branch, like `git switch --detach`.
    pub fn switch_detached(&self, commit: &str) -> Result<()> {
        self.run(["switch", "--quiet", "--detach", commit], None)
            .map(drop)
    }

    /// Runs `git <args>` as the user typed it, calling `output` with each
    /// line it writes, to standard output or standard error, as it comes.
    /// Nothing can ask for input: no prompt, no editor (messages stay as git
    /// proposes them) and no pager. Returns whether git succeeded.
    pub fn run_as_typed(&self, args: &[String], mut output: impl FnMut(String)) -> Result<bool> {
        let mut command = self.command();
        command
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", "true")
            .env("GIT_SEQUENCE_EDITOR", "true")
            .env("GIT_PAGER", "cat");
        // The line each stream is in the middle of.
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut said = Vec::new();
        let end = run::run(
            command,
            None,
            self.operation.as_ref(),
            self.undoing,
            |stream, bytes| {
                let line = match stream {
                    Stream::Out => &mut out,
                    Stream::Err => {
                        said.extend_from_slice(bytes);
                        &mut err
                    }
                };
                for byte in bytes {
                    if *byte == b'\n' {
                        output(line_of(line));
                        line.clear();
                    } else {
                        line.push(*byte);
                    }
                }
            },
        )
        .map_err(Error::Spawn)?;
        for line in [out, err] {
            if !line.is_empty() {
                output(line_of(&line));
            }
        }
        match end {
            End::Exited(status) => Ok(status.success()),
            End::Cancelled => Err(Error::Cancelled {
                command: describe(args),
                output: run::text(&said),
            }),
        }
    }

    /// Rewrites the commits after `base` (all of them, from the root, when
    /// `None`) on the branch checked out, `branch`, as `steps` say, oldest
    /// first, like `git rebase -i --autostash base` with that todo list.
    /// The tip is saved as `backup` (a full ref name) first. Uncommitted
    /// changes are stashed and come back afterwards. When git cannot finish,
    /// because of a conflict or a hook, the rewrite is given up and nothing
    /// changes; at an `edit` it stops, to go on with
    /// [`Git::continue_rebase`].
    pub fn rewrite(
        &self,
        branch: &str,
        base: Option<&str>,
        steps: &[RewriteStep],
        backup: &str,
    ) -> Result<RewriteOutcome> {
        self.run(
            [
                "update-ref",
                "-m",
                &format!("GitEZ: before rewriting {branch}"),
                backup,
                "HEAD",
            ],
            None,
        )?;
        let dir = self.rewrite_dir()?;
        // Left over from an earlier rewrite.
        let _ = std::fs::remove_dir_all(&dir);
        let messages = dir.join("messages");
        std::fs::create_dir_all(&messages).map_err(|source| Error::Write {
            path: messages.clone(),
            source,
        })?;
        let mut todo = String::new();
        for step in steps {
            let command = match &step.action {
                RewriteAction::Pick => "pick",
                RewriteAction::Reword(message) => {
                    write(&messages.join(&step.commit), message)?;
                    "reword"
                }
                RewriteAction::Edit => "edit",
                RewriteAction::Squash => "squash",
                RewriteAction::Fixup => "fixup",
                RewriteAction::Drop => "drop",
            };
            todo.push_str(&format!("{command} {}\n", step.commit));
        }
        let todo_path = dir.join("todo");
        write(&todo_path, &todo)?;
        let git_dir = self.git_dir()?;
        let editor = dir.join("editor.sh");
        write(&editor, &editor_script(&git_dir, &messages))?;

        let mut args = vec![
            "-c",
            // Dropped commits are listed with `drop`, never left out.
            "rebase.missingCommitsCheck=ignore",
            "rebase",
            "--interactive",
            "--autostash",
            "--no-autosquash",
        ];
        args.extend(base.map_or(["--root"], |base| [base]));
        let sequence_editor = format!("cp {}", shell_quote(&todo_path));
        let rebased = self.run_with_env(
            args,
            None,
            &[
                ("GIT_SEQUENCE_EDITOR", sequence_editor.as_str()),
                ("GIT_EDITOR", &format!("sh {}", shell_quote(&editor))),
            ],
        );
        self.finish_rebase(rebased)
    }

    /// Goes on with the rebase in progress, like `git rebase --continue`,
    /// using the messages GitEZ's rewrite chose. When it stops again on a
    /// conflict, the rebase stays stopped.
    pub fn continue_rebase(&self) -> Result<RewriteOutcome> {
        let editor = self.rewrite_dir()?.join("editor.sh");
        let editor = if editor.exists() {
            format!("sh {}", shell_quote(&editor))
        } else {
            // Keeps the message git proposes.
            "true".to_owned()
        };
        self.run_with_env(
            ["rebase", "--continue"],
            None,
            &[("GIT_EDITOR", editor.as_str())],
        )?;
        Ok(if self.rebasing()? {
            RewriteOutcome::Stopped
        } else {
            self.clean_rewrite_dir();
            RewriteOutcome::Done
        })
    }

    /// Gives up the rebase in progress, like `git rebase --abort`: the
    /// branch and the working tree return to how they were before it.
    pub fn abort_rebase(&self) -> Result<()> {
        self.run(["rebase", "--abort"], None)?;
        self.clean_rewrite_dir();
        Ok(())
    }

    /// Moves branch `branch`, which must be checked out, back to `backup`,
    /// keeping uncommitted changes, like `git reset --keep backup`.
    pub fn restore(&self, branch: &str, backup: &str) -> Result<()> {
        let head = self.run(["symbolic-ref", "--quiet", "--short", "HEAD"], None)?;
        if String::from_utf8_lossy(&head).trim() != branch {
            return Err(Error::Failed {
                command: format!("git reset --keep {backup}"),
                output: format!("{branch} is no longer checked out."),
            });
        }
        self.run(["reset", "--quiet", "--keep", backup], None)
            .map(drop)
    }

    /// Ends `rebased`, the result of starting a rebase: done, stopped at an
    /// `edit`, or given up when git could not finish.
    fn finish_rebase(&self, rebased: Result<Vec<u8>>) -> Result<RewriteOutcome> {
        match rebased {
            Ok(_) if self.rebasing()? => Ok(RewriteOutcome::Stopped),
            Ok(_) => {
                self.clean_rewrite_dir();
                Ok(RewriteOutcome::Done)
            }
            Err(error) => {
                let undo = self.undoing();
                if undo.rebasing().unwrap_or(true) {
                    // Brings back stashed changes too.
                    undo.run(["rebase", "--abort"], None).ok();
                }
                self.clean_rewrite_dir();
                Err(error)
            }
        }
    }

    /// Whether a rebase is in progress.
    fn rebasing(&self) -> Result<bool> {
        let git_dir = self.git_dir()?;
        Ok(git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists())
    }

    /// The working tree's git directory, as an absolute path.
    fn git_dir(&self) -> Result<PathBuf> {
        let output = self.run(["rev-parse", "--absolute-git-dir"], None)?;
        Ok(PathBuf::from(String::from_utf8_lossy(&output).trim()))
    }

    /// Where a rewrite keeps its todo list and messages while it runs.
    fn rewrite_dir(&self) -> Result<PathBuf> {
        Ok(self.git_dir()?.join("gitez").join("rewrite"))
    }

    fn clean_rewrite_dir(&self) {
        if let Ok(dir) = self.undoing().rewrite_dir() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// Stashes every uncommitted change, untracked files too, like
    /// `git stash push --include-untracked`; returns whether there was
    /// anything to stash.
    fn stash_push(&self, message: &str) -> Result<bool> {
        self.stash(Some(message), true)
    }

    /// Stashes the uncommitted changes, like `git stash push`, with
    /// `message` or git's own "WIP on …"; untracked files too if
    /// `untracked`. Returns whether there was anything to stash.
    pub fn stash(&self, message: Option<&str>, untracked: bool) -> Result<bool> {
        let before = self
            .run(["rev-parse", "--quiet", "--verify", "refs/stash"], None)
            .ok();
        let mut args = vec!["stash", "push", "--quiet"];
        if untracked {
            args.push("--include-untracked");
        }
        if let Some(message) = message {
            args.extend(["-m", message]);
        }
        // Literal pathspecs stop `git stash` from cleaning the untracked
        // files it stashed, which it does with the pathspec `:/`.
        self.run_with_env(args, None, &[("GIT_LITERAL_PATHSPECS", "0")])?;
        // Stashed by now, so asked even after a cancel.
        let after = self
            .undoing()
            .run(["rev-parse", "--quiet", "--verify", "refs/stash"], None)
            .ok();
        Ok(after.is_some() && after != before)
    }

    /// The paths with unresolved conflicts, like `git diff --diff-filter=U`.
    pub fn conflicted(&self) -> Result<Vec<String>> {
        let output = self.run(["diff", "--name-only", "-z", "--diff-filter=U"], None)?;
        Ok(output
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect())
    }

    /// Resolves the conflicts in `paths` by taking one side's whole file,
    /// like `git checkout --ours` (or `--theirs`) and `git add`. When that
    /// side deleted the file, it is removed.
    pub fn take_side(&self, paths: &[String], side: Side) -> Result<()> {
        let flag = match side {
            Side::Ours => "--ours",
            Side::Theirs => "--theirs",
        };
        for path in paths {
            let path = std::slice::from_ref(path);
            match self.run_with_paths(["checkout", flag], path) {
                Ok(()) => self.stage(path)?,
                Err(error) if error.is_cancelled() => return Err(error),
                // That side has no version of it: it deleted the file.
                Err(error) => self
                    .run_with_paths(["rm", "--quiet"], path)
                    .map_err(|_| error)?,
            }
        }
        Ok(())
    }

    /// Commits the merge in progress with the message git prepared, once
    /// every conflict is resolved and staged.
    pub fn commit_merge(&self) -> Result<()> {
        self.run(["commit", "--quiet", "--no-edit"], None).map(drop)
    }

    /// Gives up the merge in progress, like `git merge --abort`: the
    /// working tree and index return to how they were before it.
    pub fn abort_merge(&self) -> Result<()> {
        self.run(["merge", "--abort"], None).map(drop)
    }

    /// Sets `key` in the repository's own configuration to `value`, like
    /// `git config --local`, or removes it when `value` is `None`.
    pub fn set_config(&self, key: &str, value: Option<&str>) -> Result<()> {
        match value {
            Some(value) => self.run(["config", "--local", key, value], None).map(drop),
            None => match self.run(["config", "--local", "--unset-all", key], None) {
                // Exit code 5: it was not set.
                Err(Error::Failed { .. }) if self.config_unset(key) => Ok(()),
                result => result.map(drop),
            },
        }
    }

    /// Whether `key` is not set in the repository's own configuration.
    fn config_unset(&self, key: &str) -> bool {
        self.run(["config", "--local", "--get-all", key], None)
            .is_err()
    }

    /// Replaces the last commit with one that has `message` and also the
    /// staged changes, like `git commit --amend`.
    pub fn amend(&self, message: &str) -> Result<()> {
        self.commit_with(
            message,
            CommitOptions {
                amend: true,
                ..CommitOptions::default()
            },
        )
    }

    /// Applies `patch` to the index only, like `git apply --cached`; with
    /// `reverse`, takes it back out. Stages or unstages part of a file.
    pub fn apply_to_index(&self, patch: &str, reverse: bool) -> Result<()> {
        let mut args = vec!["apply", "--cached", "--whitespace=nowarn"];
        if reverse {
            args.push("--reverse");
        }
        args.push("-");
        self.run(args, Some(patch.as_bytes())).map(drop)
    }

    /// Applies `patch` to the working tree only, like `git apply`; with
    /// `reverse`, takes it back out. Discards part of a file's unstaged
    /// changes, or brings them back.
    pub fn apply_to_worktree(&self, patch: &str, reverse: bool) -> Result<()> {
        let mut args = vec!["apply", "--whitespace=nowarn"];
        if reverse {
            args.push("--reverse");
        }
        args.push("-");
        self.run(args, Some(patch.as_bytes())).map(drop)
    }

    /// Throws away the uncommitted changes to `paths`, or to every file
    /// when `None`, staged or not, and deletes the files git does not track
    /// yet. They are kept in a stash commit that is left off the stash
    /// list; its id, returned, brings them back with
    /// [`Git::restore_discarded`]. `None` when there was nothing to discard.
    pub fn discard(&self, paths: Option<&[String]>) -> Result<Option<String>> {
        self.discard_changes(paths, true)
    }

    /// Like [`Git::discard`] for every file, but leaves the files git does
    /// not track alone, as `git reset --hard` does.
    pub fn discard_tracked(&self) -> Result<Option<String>> {
        self.discard_changes(None, false)
    }

    fn discard_changes(&self, paths: Option<&[String]>, untracked: bool) -> Result<Option<String>> {
        let top = |git: &Self| {
            git.run(["rev-parse", "--quiet", "--verify", "refs/stash"], None)
                .ok()
                .map(|id| String::from_utf8_lossy(&id).trim().to_owned())
        };
        let before = top(self);
        let mut args = vec!["stash", "push", "--quiet"];
        if untracked {
            args.push("--include-untracked");
        }
        args.extend(["-m", "GitEZ: discarded changes"]);
        // Literal pathspecs stop `git stash` from cleaning the untracked
        // files it stashed, so each path is marked literal instead.
        let literal = [("GIT_LITERAL_PATHSPECS", "0")];
        match paths {
            Some([]) => return Ok(None),
            Some(paths) => {
                let mut input = Vec::new();
                for path in paths {
                    input.extend_from_slice(b":(literal)");
                    input.extend_from_slice(path.as_bytes());
                    input.push(0);
                }
                let args = args
                    .into_iter()
                    .chain(["--pathspec-from-file=-", "--pathspec-file-nul"]);
                self.run_with_env(args, Some(&input), &literal)?;
            }
            None => {
                self.run_with_env(args, None, &literal)?;
            }
        }
        // Stashed by now, so finished even after a cancel: the changes
        // would stay on the stash list, discarded but not said to be.
        let finish = self.undoing();
        let after = top(&finish);
        if after.is_none() || after == before {
            return Ok(None);
        }
        finish.drop_stash(0)?;
        Ok(after)
    }

    /// Brings back the changes [`Git::discard`] kept in stash commit `id`,
    /// staged as they were. Fails, changing nothing, when they would
    /// overwrite changes made since.
    pub fn restore_discarded(&self, id: &str) -> Result<()> {
        self.run(["stash", "apply", "--quiet", "--index", id], None)
            .map(drop)
    }

    /// Runs `git <args>` with `paths` passed on standard input, so any
    /// number of paths fits and none is read as an option or a pattern.
    fn run_with_paths<const N: usize>(&self, args: [&str; N], paths: &[String]) -> Result<()> {
        let mut input = Vec::new();
        for path in paths {
            input.extend_from_slice(path.as_bytes());
            input.push(0);
        }
        let args = args
            .into_iter()
            .chain(["--pathspec-from-file=-", "--pathspec-file-nul"]);
        self.run(args, Some(&input)).map(drop)
    }

    /// Runs `git <args>`, writing `input` to its standard input, and returns
    /// its standard output.
    fn run<I, S>(&self, args: I, input: Option<&[u8]>) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_with_env(args, input, &[])
    }

    /// [`Git::run`] with the environment variables `env` set as well.
    fn run_with_env<I, S>(
        &self,
        args: I,
        input: Option<&[u8]>,
        env: &[(&str, &str)],
    ) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<_> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect();
        let mut command = self.command();
        command
            .args(&args)
            // Paths are file names, never patterns.
            .env("GIT_LITERAL_PATHSPECS", "1")
            // Nothing can answer a prompt.
            .env("GIT_TERMINAL_PROMPT", "0")
            .envs(env.iter().copied());
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let end = run::run(
            command,
            input,
            self.operation.as_ref(),
            self.undoing,
            |stream, bytes| match stream {
                Stream::Out => stdout.extend_from_slice(bytes),
                Stream::Err => stderr.extend_from_slice(bytes),
            },
        )
        .map_err(Error::Spawn)?;
        let status = match end {
            End::Exited(status) if status.success() => return Ok(stdout),
            End::Exited(status) => status,
            End::Cancelled => {
                return Err(Error::Cancelled {
                    command: describe(&args),
                    output: run::text(&stderr),
                });
            }
        };
        let mut text = run::text(&stderr);
        let stdout = String::from_utf8_lossy(&stdout);
        if !stdout.trim().is_empty() {
            // Hooks often write to standard output.
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(stdout.trim());
        }
        if text.is_empty() {
            text = format!("git exited with {status}");
        }
        Err(Error::Failed {
            command: describe(&args),
            output: text,
        })
    }

    /// `git` in the working tree, with this handle's environment.
    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.work_dir)
            .envs(self.env.iter().map(|(key, value)| (key, value)));
        command
    }
}

/// `git <args>`, to name a command in an error.
fn describe<S: AsRef<OsStr>>(args: &[S]) -> String {
    let mut command = String::from("git");
    for arg in args {
        command.push(' ');
        command.push_str(&arg.as_ref().to_string_lossy());
    }
    command
}

/// The working trees in `output`, what `git worktree list --porcelain`
/// wrote with each line ended by `end`: an empty line ends a working tree.
fn parse_worktrees(output: &[u8], end: u8) -> Vec<Worktree> {
    let mut worktrees: Vec<Worktree> = Vec::new();
    let mut started = false;
    for line in output.split(|byte| *byte == end) {
        let line = String::from_utf8_lossy(line);
        if line.is_empty() {
            started = false;
            continue;
        }
        let (key, value) = match line.split_once(' ') {
            Some((key, value)) => (key, Some(value)),
            None => (&*line, None),
        };
        if key == "worktree" {
            let path = PathBuf::from(value.unwrap_or_default());
            // Git says so itself (`prunable`) only when the folder's `.git`
            // file is gone too.
            let problem = (!path.exists()).then_some(WorktreeProblem::Missing);
            worktrees.push(Worktree {
                path,
                branch: None,
                problem,
            });
            started = true;
            continue;
        }
        let Some(worktree) = worktrees.last_mut().filter(|_| started) else {
            continue;
        };
        match key {
            "branch" => {
                worktree.branch = value
                    .and_then(|name| name.strip_prefix("refs/heads/"))
                    .map(str::to_owned);
            }
            "prunable" => worktree.problem = Some(WorktreeProblem::Missing),
            // Missing matters more: there is nothing to unlock.
            "locked" if worktree.problem.is_none() => {
                let reason = value.filter(|reason| !reason.is_empty());
                worktree.problem = Some(WorktreeProblem::Locked(reason.map(str::to_owned)));
            }
            _ => {}
        }
    }
    worktrees
}

/// One line of git's output, without its line ending.
fn line_of(line: &[u8]) -> String {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    String::from_utf8_lossy(line).into_owned()
}

/// The editor git runs for each commit a rewrite rewords: it puts the
/// message chosen for the commit, kept in `messages` under its id, in the
/// file git asks to edit. Commits without one keep what git proposes.
fn editor_script(git_dir: &Path, messages: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         # Written by GitEZ for one rewrite.\n\
         id=$(tail -n 1 {done} | cut -d ' ' -f 2)\n\
         message={messages}/\"$id\"\n\
         if [ -n \"$id\" ] && [ -f \"$message\" ]; then cat \"$message\" > \"$1\"; fi\n",
        done = shell_quote(&git_dir.join("rebase-merge").join("done")),
        messages = shell_quote(messages),
    )
}

fn write(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents).map_err(|source| Error::Write {
        path: path.to_owned(),
        source,
    })
}

/// `path` quoted for `sh`.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}
