//! The linked worktrees agent threads work in: where they live, creating
//! them and removing them when a thread is completed. Every change runs the
//! user's `git` through `gitez-command`; reads go through `gitez-git`.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use gitez_command::Git;
use gitez_git::{HeadPlace, Repository};

use crate::{AgentThread, AgentThreadId, Error, Result, repository_key};

/// `<data_dir>/worktrees/<repository_key>/<id>`.
pub fn worktree_path(
    data_dir: &Path,
    repository_name: &str,
    common_dir: &Path,
    id: &AgentThreadId,
) -> PathBuf {
    data_dir
        .join("worktrees")
        .join(repository_key(repository_name, common_dir))
        .join(id.as_str())
}

/// Creates branch `branch` at `start_commit` unless it exists, then
/// `git worktree add <path> <branch>`, run in `main_work_dir`. Fails if
/// `path` exists and is not a worktree of `branch` (an empty folder is
/// used); when it already is one, nothing changes. Returns the worktree's
/// own git dir (`<common>/worktrees/<name>`), read from `<path>/.git`.
pub fn create_worktree(
    main_work_dir: &Path,
    path: &Path,
    branch: &str,
    start_commit: &str,
) -> Result<PathBuf> {
    if path.exists() {
        if path.join(".git").exists() {
            if checked_out_branch(path)?.as_deref() == Some(branch) {
                return worktree_git_dir(path);
            }
            return Err(Error::refused(format!(
                "{} exists and is not the worktree of {branch}.",
                path.display()
            )));
        }
        let empty = fs::read_dir(path)
            .map_err(Error::io(path))?
            .next()
            .is_none();
        if !empty {
            return Err(Error::refused(format!(
                "{} exists and is not the worktree of {branch}.",
                path.display()
            )));
        }
    }

    let main = Repository::open(main_work_dir)?;
    let git = Git::new(main_work_dir);
    if main
        .resolve_commit(&format!("refs/heads/{branch}"))?
        .is_none()
    {
        git.create_branch(branch, start_commit, false)?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(Error::io(parent))?;
    }
    git.worktree_add(path, branch)?;
    worktree_git_dir(path)
}

/// The worktree's own git dir, read from `<worktree>/.git` ("gitdir: …").
/// A relative one is taken relative to the worktree.
pub fn worktree_git_dir(worktree: &Path) -> Result<PathBuf> {
    let dot_git = worktree.join(".git");
    let text = fs::read_to_string(&dot_git).map_err(Error::io(&dot_git))?;
    let Some(dir) = text.trim_end_matches(['\r', '\n']).strip_prefix("gitdir: ") else {
        return Err(Error::refused(format!(
            "{} is not a linked worktree.",
            worktree.display()
        )));
    };
    let dir = Path::new(dir);
    Ok(if dir.is_relative() {
        let joined = worktree.join(dir);
        joined.canonicalize().unwrap_or(joined)
    } else {
        dir.to_owned()
    })
}

/// The branch checked out in the working tree at `work_dir`, if any.
fn checked_out_branch(work_dir: &Path) -> Result<Option<String>> {
    Ok(match Repository::open(work_dir)?.ref_state()?.head {
        Some(HeadPlace::Branch(name) | HeadPlace::Unborn(name)) => Some(name),
        _ => None,
    })
}

/// What [`complete`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompleteReport {
    pub worktree_removed: bool,
    pub branch_deleted: bool,
}

/// Completes `thread`: `git worktree remove --force` (skipped when the
/// folder is gone), `git worktree prune`, and deletes the branch when its
/// tip is still `thread.start.commit` (no commits of its own) and it is not
/// checked out in the main worktree. Does not change thread.json; the
/// caller then calls `mark_completed`. Fails, with git's words, when the
/// folder exists but git does not know it as a worktree: GitEZ never
/// deletes a folder git does not vouch for.
pub fn complete(main_work_dir: &Path, thread: &AgentThread) -> Result<CompleteReport> {
    let git = Git::new(main_work_dir);
    let worktree_removed = match fs::symlink_metadata(&thread.worktree) {
        Ok(_) => {
            git.worktree_remove(&thread.worktree, true)?;
            true
        }
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => return Err(Error::io(&thread.worktree)(error)),
    };
    git.worktree_prune()?;

    let main = Repository::open(main_work_dir)?;
    let tip = main.resolve_commit(&format!("refs/heads/{}", thread.branch))?;
    let start = gitez_git::Oid::from_hex(thread.start.commit.as_bytes()).ok();
    let own_commits = tip.is_none() || tip != start;
    let checked_out = checked_out_branch(main_work_dir)?.as_deref() == Some(thread.branch.as_str());
    let branch_deleted = if own_commits || checked_out {
        false
    } else {
        git.delete_branch(&thread.branch, true)?;
        true
    };
    Ok(CompleteReport {
        worktree_removed,
        branch_deleted,
    })
}
