//! The linked worktrees agent threads work in: where they live, creating
//! them and removing them when a thread is completed. Every change runs the
//! user's `git` through `gitez-command`; reads go through `gitez-git`.

use std::fs;
use std::io::{ErrorKind, Read as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};

use gitez_command::Git;
use gitez_git::{HeadPlace, Repository};

use crate::{AgentThread, AgentThreadId, Error, Result, WorktreeRecord, repository_key};

/// The largest pointer file GitEZ reads (`.git`, `commondir`, `gitdir`,
/// `config.worktree`); git's own are a path long.
const MAX_POINTER: u64 = 64 * 1024;

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

/// Rebases the branch checked out in a thread's worktree onto `onto`,
/// like `git rebase onto`, for the GUI's "Rebase and merge" of a thread's
/// local pull request. git refuses to rebase a branch checked out in a
/// linked worktree from anywhere but that worktree, so this must run
/// there; the working tree is the agent's. It is made safe without a
/// sandbox: the worktree's `.git` pointer and git dir are checked against
/// `record` with [`check_worktree`] first (so git opens the thread's own
/// repository, not one the agent redirected it to), then
/// [`Git::rebase_onto_untrusted`] replays the commits with hooks, the
/// filesystem monitor and submodule recursion off. [`Error::Tampered`]
/// when the worktree's git files are not the recorded ones; GitEZ runs no
/// git in it then.
pub fn rebase_in_worktree(
    common_dir: &Path,
    worktree: &Path,
    record: Option<&WorktreeRecord>,
    onto: &str,
) -> Result<()> {
    check_worktree(common_dir, worktree, record)?;
    Git::new(worktree).rebase_onto_untrusted(onto)?;
    Ok(())
}

/// A worktree [`create_worktree`] made, or found already there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedWorktree {
    /// The worktree's own git dir, `<common>/worktrees/<name>`.
    pub git_dir: PathBuf,
    /// Its git files as they are now: what to keep in `thread.json`
    /// ([`AgentThread::worktree_record`]) for [`check_worktree`].
    pub record: WorktreeRecord,
    /// The working tree is still empty: the branch has commits beyond the
    /// start, which may be the agent's, so git did not check them out
    /// outside the sandbox. The caller checks them out inside the sandbox
    /// (`git reset --hard --quiet`) before anything else runs there.
    pub needs_checkout: bool,
}

/// Creates branch `branch` at `start_commit` unless it exists, then
/// `git worktree add <path> <branch>`, run in `main_work_dir`. When the
/// branch exists and is not at `start_commit` its files are the agent's,
/// so they are not checked out here (`--no-checkout`; see
/// [`CreatedWorktree::needs_checkout`]): a checkout runs the user's filter
/// drivers and hooks on them, and a hook path inside the worktree
/// (`core.hooksPath = .husky`) runs the agent's own files. Fails if `path`
/// exists and is not a worktree of `branch` (an empty folder is used);
/// when it already is one and its git files pass [`check_worktree`],
/// nothing changes.
pub fn create_worktree(
    main_work_dir: &Path,
    path: &Path,
    branch: &str,
    start_commit: &str,
) -> Result<CreatedWorktree> {
    let main = Repository::open(main_work_dir)?;
    let common_dir = main.common_dir();
    if path.exists() {
        if path.join(".git").exists() {
            let record = check_worktree(&common_dir, path, None)?;
            if checked_out(&record.git_dir)?.as_deref() == Some(branch) {
                return Ok(CreatedWorktree {
                    git_dir: record.git_dir.clone(),
                    record,
                    needs_checkout: false,
                });
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

    let git = Git::new(main_work_dir);
    let tip = main.resolve_commit(&format!("refs/heads/{branch}"))?;
    let start = gitez_git::Oid::from_hex(start_commit.as_bytes()).ok();
    if tip.is_none() {
        git.create_branch(branch, start_commit, false)?;
    }
    let needs_checkout = tip.is_some() && tip != start;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(Error::io(parent))?;
    }
    if needs_checkout {
        git.worktree_add_without_checkout(path, branch)?;
    } else {
        git.worktree_add(path, branch)?;
    }
    let record = check_worktree(&common_dir, path, None)?;
    Ok(CreatedWorktree {
        git_dir: record.git_dir.clone(),
        record,
        needs_checkout,
    })
}

/// The worktree's own git dir, read from `<worktree>/.git` ("gitdir: …").
/// A relative one is taken relative to the worktree. The file is the
/// agent's to write unless the sandbox protects it: use [`check_worktree`]
/// before trusting the answer.
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

/// Checks that `worktree` is still the linked worktree of the repository
/// whose common git dir is `common_dir` that git made, before GitEZ runs
/// anything in it outside the sandbox. Everything in a thread's worktree is
/// the agent's to write, and git follows `<worktree>/.git` to find the
/// repository, its configuration (filters, `core.fsmonitor`, hooks) and
/// more: a changed pointer would make git run the agent's commands.
///
/// It must be a real folder (not a link) whose `.git` is a regular file
/// naming a git dir in `<common_dir>/worktrees/`; that git dir's
/// `commondir` must lead back to `common_dir` and its `gitdir` to this
/// `.git`; it must have no `config` and, unless `record` says git made
/// one, no `config.worktree`. With `record` (what GitEZ saw when it made
/// the worktree) every one of those files must also be exactly as
/// recorded. None of them may be a link or have another hard link. Reads
/// files only; runs nothing. Returns what the files hold now;
/// [`Error::Tampered`] says what differs.
pub fn check_worktree(
    common_dir: &Path,
    worktree: &Path,
    record: Option<&WorktreeRecord>,
) -> Result<WorktreeRecord> {
    let tampered = |problem: String| Error::Tampered {
        worktree: worktree.to_owned(),
        problem,
    };
    let common = common_dir.canonicalize().map_err(Error::io(common_dir))?;
    if !is_real_dir(worktree) {
        return Err(tampered(
            "the worktree folder is not a folder (it was replaced, perhaps by a link).".to_owned(),
        ));
    }
    let dot_git_path = worktree.join(".git");
    let dot_git =
        read_pointer(&dot_git_path).map_err(|problem| tampered(format!("its .git {problem}")))?;
    let Some(named) = dot_git
        .trim_end_matches(['\r', '\n'])
        .strip_prefix("gitdir: ")
    else {
        return Err(tampered(
            "its .git file does not name a git dir.".to_owned(),
        ));
    };
    let named = Path::new(named);
    let named = if named.is_relative() {
        worktree.join(named)
    } else {
        named.to_owned()
    };
    let worktrees = common.join("worktrees");
    let git_dir = match lexical(&named) {
        Some(dir) if is_real_dir(&dir) => dir,
        _ => {
            return Err(tampered(
                "its .git file names a git dir that is not a folder.".to_owned(),
            ));
        }
    };
    let git_dir = git_dir.canonicalize().map_err(Error::io(&git_dir))?;
    if git_dir.parent() != Some(worktrees.as_path()) {
        return Err(tampered(format!(
            "its .git file names {}, which is not one of the repository's worktrees.",
            git_dir.display()
        )));
    }
    let commondir = read_pointer(&git_dir.join("commondir"))
        .map_err(|problem| tampered(format!("the commondir file of its git dir {problem}")))?;
    let leads_to = git_dir.join(commondir.trim_end_matches(['\r', '\n']));
    if leads_to.canonicalize().ok().as_deref() != Some(common.as_path()) {
        return Err(tampered(
            "the commondir file of its git dir does not lead to the repository.".to_owned(),
        ));
    }
    let gitdir = read_pointer(&git_dir.join("gitdir"))
        .map_err(|problem| tampered(format!("the gitdir file of its git dir {problem}")))?;
    let back = git_dir.join(gitdir.trim_end_matches(['\r', '\n']));
    let own = dot_git_path
        .canonicalize()
        .map_err(Error::io(&dot_git_path))?;
    if back.canonicalize().ok().as_deref() != Some(own.as_path()) {
        return Err(tampered(
            "the gitdir file of its git dir does not lead back to this worktree.".to_owned(),
        ));
    }
    if fs::symlink_metadata(git_dir.join("config")).is_ok() {
        return Err(tampered(
            "its git dir has a config file of its own.".to_owned(),
        ));
    }
    let config_worktree_path = git_dir.join("config.worktree");
    let config_worktree = match fs::symlink_metadata(&config_worktree_path) {
        Ok(_) => Some(read_pointer(&config_worktree_path).map_err(|problem| {
            tampered(format!("the config.worktree file of its git dir {problem}"))
        })?),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(Error::io(&config_worktree_path)(error)),
    };
    let now = WorktreeRecord {
        git_dir,
        dot_git,
        commondir,
        gitdir,
        config_worktree,
    };
    match record {
        None if now.config_worktree.is_some() => Err(tampered(
            "its git dir has a config.worktree file GitEZ did not see git make.".to_owned(),
        )),
        None => Ok(now),
        Some(record) => {
            let recorded_dir = record
                .git_dir
                .canonicalize()
                .unwrap_or_else(|_| record.git_dir.clone());
            let changed = [
                (recorded_dir != now.git_dir, "its git dir"),
                (record.dot_git != now.dot_git, "its .git file"),
                (
                    record.commondir != now.commondir,
                    "the commondir file of its git dir",
                ),
                (
                    record.gitdir != now.gitdir,
                    "the gitdir file of its git dir",
                ),
                (
                    record.config_worktree != now.config_worktree,
                    "the config.worktree file of its git dir",
                ),
            ];
            match changed.iter().find(|(changed, _)| *changed) {
                Some((_, what)) => Err(tampered(format!(
                    "{what} is not what git wrote when GitEZ made the worktree."
                ))),
                None => Ok(now),
            }
        }
    }
}

/// Whether `path` is a folder itself, not a link to one.
fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// A small regular file's text, refused (with what is wrong, for the user)
/// when it is missing, a link, not a regular file, hard linked elsewhere,
/// too big or not UTF-8.
fn read_pointer(path: &Path) -> std::result::Result<String, String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == ErrorKind::NotFound => return Err("is missing.".to_owned()),
        Err(error) => return Err(format!("cannot be read: {error}.")),
    };
    if meta.file_type().is_symlink() {
        return Err("is a link.".to_owned());
    }
    if !meta.is_file() {
        return Err("is not a file.".to_owned());
    }
    if meta.nlink() > 1 {
        return Err("has another name (a hard link).".to_owned());
    }
    if meta.len() > MAX_POINTER {
        return Err("is too big.".to_owned());
    }
    let mut text = String::new();
    fs::File::open(path)
        .and_then(|file| file.take(MAX_POINTER).read_to_string(&mut text))
        .map_err(|error| format!("cannot be read: {error}."))?;
    Ok(text)
}

/// `path` with `.` and `..` resolved textually; `None` for a relative one.
fn lexical(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    Some(out)
}

/// The branch a linked worktree's `HEAD` (in its git dir) is on, if any.
/// Read as a file: `HEAD` is the agent's to write, and the answer only
/// decides whether GitEZ reuses the worktree.
fn checked_out(git_dir: &Path) -> Result<Option<String>> {
    let path = git_dir.join("HEAD");
    let text = read_pointer(&path)
        .map_err(|problem| Error::refused(format!("{} {problem}", path.display())))?;
    Ok(text
        .trim_end_matches(['\r', '\n'])
        .strip_prefix("ref: refs/heads/")
        .map(str::to_owned))
}

/// The branch checked out in the main working tree at `work_dir`, if any.
fn checked_out_branch(work_dir: &Path) -> Result<Option<String>> {
    Ok(match Repository::open(work_dir)?.ref_state()?.head {
        Some(HeadPlace::Branch(name) | HeadPlace::Unborn(name)) => Some(name),
        _ => None,
    })
}

/// What [`complete`] does with a worktree that has uncommitted changes:
/// one the supervisor recorded as having them at the end of the thread's
/// last run ([`AgentThread::uncommitted`]), or whose state it does not know
/// (no record).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IfUncommitted {
    /// Change nothing and fail with [`Error::Uncommitted`], so the user can
    /// be asked first.
    #[default]
    Refuse,
    /// Remove the worktree and the changes in it: the user agreed.
    Discard,
    /// Leave the worktree and its branch where they are (the thread can
    /// still be marked completed); a later `complete` with
    /// [`IfUncommitted::Discard`] removes them. Also leaves a worktree
    /// GitEZ does not trust ([`Error::Tampered`]) instead of failing.
    Keep,
}

/// Why [`complete`] left a worktree in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kept {
    /// It has uncommitted changes (`count` of them; `None`: not known).
    Uncommitted { count: Option<u32> },
    /// Its git files changed since GitEZ made it (the text says how, for
    /// the user); GitEZ runs no git in it.
    Untrusted(String),
}

/// What [`complete`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompleteReport {
    pub worktree_removed: bool,
    /// Set when the worktree was left in place ([`IfUncommitted::Keep`]).
    pub kept: Option<Kept>,
    pub branch_deleted: bool,
}

/// Completes `thread`: removes its worktree with `git worktree remove
/// --force` (skipped when the folder is gone), `git worktree prune`, and
/// deletes the branch when its tip is still `thread.start.commit` (no
/// commits of its own) and it is not checked out in the main worktree;
/// then removes the thread's [`ScratchDirs`] (best effort). Does not change
/// thread.json; the caller then calls `mark_completed`.
///
/// A worktree with uncommitted changes, or whose state is not known, is
/// handled as `uncommitted` says; nothing here looks into the worktree
/// (the supervisor checked it inside the sandbox). Before git touches the
/// worktree its git files must pass [`check_worktree`] against
/// `thread.worktree_record`: otherwise [`Error::Tampered`] (or, with
/// [`IfUncommitted::Keep`], it is kept), as GitEZ never runs git in a
/// worktree whose `.git` the agent may have pointed elsewhere. `git
/// worktree remove --force` reads no file of the working tree and runs no
/// hook, filter or submodule command; it deletes without following links.
///
/// Called again for a completed thread whose worktree was kept, with
/// [`IfUncommitted::Discard`], it removes the worktree then.
pub fn complete(
    main_work_dir: &Path,
    thread: &AgentThread,
    uncommitted: IfUncommitted,
) -> Result<CompleteReport> {
    let git = Git::new(main_work_dir);
    let exists = match fs::symlink_metadata(&thread.worktree) {
        Ok(_) => true,
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => return Err(Error::io(&thread.worktree)(error)),
    };
    let mut kept = None;
    if exists {
        // Only a run that recorded uncommitted changes (count > 0) is
        // protected: the supervisor records the count at the end of every
        // run, so a failed leftover commit shows as `Some(count)` here.
        // `None` means no run recorded anything (an old or never-run
        // thread), handled as today.
        let recorded = thread
            .uncommitted
            .as_ref()
            .map(|found| found.count)
            .filter(|&count| count > 0);
        if let Some(count) = recorded {
            match uncommitted {
                IfUncommitted::Refuse => {
                    return Err(Error::Uncommitted {
                        worktree: thread.worktree.clone(),
                        count: Some(count),
                    });
                }
                IfUncommitted::Keep => kept = Some(Kept::Uncommitted { count: Some(count) }),
                IfUncommitted::Discard => {}
            }
        }
        if kept.is_none() {
            let common_dir = Repository::open(main_work_dir)?.common_dir();
            match check_worktree(
                &common_dir,
                &thread.worktree,
                thread.worktree_record.as_ref(),
            ) {
                Ok(_) => {}
                Err(Error::Tampered { problem, .. }) if uncommitted == IfUncommitted::Keep => {
                    kept = Some(Kept::Untrusted(problem));
                }
                Err(error) => return Err(error),
            }
        }
        if kept.is_none() {
            git.worktree_remove(&thread.worktree, true)?;
        }
    }
    if let Some(kept) = kept {
        if let Some(dirs) = scratch_dirs_of(thread) {
            remove_scratch(&dirs.tmp);
            remove_scratch(&dirs.caches);
        }
        return Ok(CompleteReport {
            worktree_removed: false,
            kept: Some(kept),
            branch_deleted: false,
        });
    }
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
        worktree_removed: exists,
        kept: None,
        branch_deleted,
    })
}
