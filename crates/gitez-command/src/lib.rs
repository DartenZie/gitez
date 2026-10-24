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
