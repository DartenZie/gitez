//! Changes to git repositories for GitEZ.
//!
//! Every change runs the user's own `git` binary, so hooks, signing and
//! credential helpers behave exactly as they do in the terminal. Reading the
//! repository is `gitez-git`'s job.
//!
//! The methods of [`Git`] wait for `git` to finish, so call them off the UI
//! thread.

use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Errors from running `git`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not run git")]
    Spawn(#[source] std::io::Error),
    /// `git` ran and failed. `output` is what it wrote, often the output of a
    /// hook that rejected the change.
    #[error("{output}")]
    Failed { command: String, output: String },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

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

/// Runs `git` in one working tree.
#[derive(Debug, Clone)]
pub struct Git {
    work_dir: PathBuf,
}

impl Git {
    /// Runs `git` in `work_dir`, a repository's working tree.
    pub fn new(work_dir: impl Into<PathBuf>) -> Self {
        Self {
            work_dir: work_dir.into(),
        }
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
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
        self.run(["commit", "--quiet", "--file=-"], Some(message.as_bytes()))
            .map(drop)
    }

    /// Checks out the local branch `branch`, like `git switch`. Fails, with
    /// git's explanation, when uncommitted changes would be overwritten.
    pub fn switch(&self, branch: &str) -> Result<()> {
        self.run(["switch", "--quiet", branch], None).map(drop)
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
            self.switch(branch).ok();
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
                self.run(["rebase", "--abort"], None).ok();
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
                if merged.is_err() && !self.conflicted()?.is_empty() {
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
            self.run(["reset", "--quiet", "--merge"], None).ok();
        }
        merged.map(|()| MergeOutcome::Merged)
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
        self.run(
            ["commit", "--quiet", "--amend", "--file=-"],
            Some(message.as_bytes()),
        )
        .map(drop)
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
        let args: Vec<_> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect();
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&self.work_dir)
            .args(&args)
            // Paths are file names, never patterns.
            .env("GIT_LITERAL_PATHSPECS", "1")
            // Nothing can answer a prompt.
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(Error::Spawn)?;
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            // A command that fails early stops reading; its output says why.
            let _ = stdin.write_all(input);
        }
        let output = child.wait_with_output().map_err(Error::Spawn)?;
        if output.status.success() {
            return Ok(output.stdout);
        }
        let mut text = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.trim().is_empty() {
            // Hooks often write to standard output.
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(stdout.trim());
        }
        if text.is_empty() {
            text = format!("git exited with {}", output.status);
        }
        Err(Error::Failed {
            command: format!(
                "git {}",
                args.iter()
                    .map(|arg| arg.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            output: text,
        })
    }
}
