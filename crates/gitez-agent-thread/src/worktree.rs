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

/// A thread's own folders in GitEZ's data folder, beside its worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchDirs {
    /// `<data_dir>/tmp/<repository key>-<id>`: the `TMPDIR` of its commands.
    pub tmp: PathBuf,
    /// `<data_dir>/caches/<repository key>-<id>`: its own package and build
    /// caches (Cargo home, npm cache and the like), so that nothing it
    /// downloads lands in the user's shared caches.
    pub caches: PathBuf,
}

/// The scratch folders of thread `id` of the repository GitEZ keys as
/// `repository_key` (the name of the folder its worktrees are in).
pub fn scratch_dirs(data_dir: &Path, repository_key: &str, id: &AgentThreadId) -> ScratchDirs {
    let name = format!("{repository_key}-{id}");
    ScratchDirs {
        tmp: data_dir.join("tmp").join(&name),
        caches: data_dir.join("caches").join(name),
    }
}

/// The scratch folders of `thread`, from where GitEZ put its worktree
/// (`<data_dir>/worktrees/<repository key>/<id>`); `None` for a worktree
/// elsewhere.
fn scratch_dirs_of(thread: &AgentThread) -> Option<ScratchDirs> {
    let key_dir = thread.worktree.parent()?;
    let worktrees = key_dir.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    let key = key_dir.file_name()?.to_str()?;
    Some(scratch_dirs(worktrees.parent()?, key, &thread.id))
}

/// Removes `dir` and everything in it, if it is a folder (never through a
/// symbolic link). Best effort: what cannot be removed stays.
fn remove_scratch(dir: &Path) {
    if fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir()) {
        // Cached downloads are read-only files in read-only folders (Go,
        // Cargo sources); make them removable first.
        make_writable(dir);
        let _ = fs::remove_dir_all(dir);
    }
}

#[cfg(unix)]
fn make_writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let Ok(meta) = fs::symlink_metadata(dir) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let mode = meta.permissions().mode();
    if mode & 0o700 != 0o700 {
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(mode | 0o700));
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                make_writable(&entry.path());
            }
        }
    }
}

#[cfg(not(unix))]
fn make_writable(_dir: &Path) {}

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
/// checked out in the main worktree; then removes the thread's
/// [`ScratchDirs`] (best effort). Does not change thread.json; the
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
    if let Some(dirs) = scratch_dirs_of(thread) {
        remove_scratch(&dirs.tmp);
        remove_scratch(&dirs.caches);
    }
    Ok(CompleteReport {
        worktree_removed,
        branch_deleted,
    })
}
