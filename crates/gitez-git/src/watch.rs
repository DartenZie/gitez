//! Noticing changes made to a repository outside GitEZ: in an editor, a
//! terminal or another git client.
//!
//! [`Repository::watch`] reports changed paths as the file system announces
//! them; [`Repository::classify`] tells which of them matter, so a build that
//! writes thousands of ignored files does not make the caller re-read
//! anything.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use notify::Watcher as _;

use crate::{Repository, Result};

/// Watches a repository until dropped.
pub struct Watcher {
    _inner: notify::RecommendedWatcher,
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher").finish_non_exhaustive()
    }
}

/// What a set of changed paths affects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Affected {
    /// Branches, tags, stashes, remotes, `HEAD` or the configuration changed:
    /// read the refs and history again (and the status, which compares with
    /// `HEAD`).
    pub refs: bool,
    /// Files in the working tree or the index changed: read the status again.
    pub working_tree: bool,
}

impl Affected {
    pub fn is_empty(self) -> bool {
        !self.refs && !self.working_tree
    }

    fn everything() -> Self {
        Self {
            refs: true,
            working_tree: true,
        }
    }
}

impl Repository {
    /// Watches the working tree and the git directory, calling `on_change`
    /// from a background thread with the paths of each change. A path equal
    /// to [`git_dir`](Self::git_dir) means events were lost and anything may
    /// have changed. Pass the paths to [`classify`](Self::classify).
    pub fn watch(&self, on_change: impl Fn(Vec<PathBuf>) + Send + 'static) -> Result<Watcher> {
        let git_dir = self.git_dir().to_owned();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
                Ok(event) if event.need_rescan() => on_change(vec![git_dir.clone()]),
                Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => {}
                Ok(event) => on_change(event.paths),
                Err(_) => on_change(vec![git_dir.clone()]),
            })?;
        for dir in self.watched_dirs() {
            watcher.watch(&dir, notify::RecursiveMode::Recursive)?;
        }
        Ok(Watcher { _inner: watcher })
    }

    /// The working tree and every git directory not inside it.
    fn watched_dirs(&self) -> Vec<PathBuf> {
        let repo = self.local();
        let mut dirs: Vec<PathBuf> = Vec::new();
        let candidates = [
            repo.workdir().map(Path::to_owned),
            Some(repo.git_dir().to_owned()),
            Some(repo.common_dir().to_owned()),
        ];
        for dir in candidates.into_iter().flatten() {
            let dir = canonical(&dir);
            if !dirs.iter().any(|watched| dir.starts_with(watched)) {
                dirs.retain(|watched| !watched.starts_with(&dir));
                dirs.push(dir);
            }
        }
        dirs
    }

    /// Which parts of the repository the changed `paths` affect. Paths in
    /// `.git/objects`, ignored files and paths outside the repository affect
    /// nothing.
    pub fn classify(&self, paths: &[PathBuf]) -> Result<Affected> {
        let repo = self.local();
        let git_dirs = [canonical(repo.git_dir()), canonical(repo.common_dir())];
        let workdir = repo.workdir().map(canonical);

        let mut affected = Affected::default();
        let mut worktree_paths = BTreeSet::new();
        for path in paths {
            let path = canonical(path);
            if let Some(relative) = git_dirs.iter().find_map(|dir| path.strip_prefix(dir).ok()) {
                let part = classify_git_path(relative);
                affected.refs |= part.refs;
                affected.working_tree |= part.working_tree;
            } else if let Some(relative) =
                workdir.as_ref().and_then(|dir| path.strip_prefix(dir).ok())
            {
                if relative.as_os_str().is_empty() {
                    affected.working_tree = true;
                } else {
                    worktree_paths.insert(relative.to_owned());
                }
            }
            if affected == Affected::everything() {
                return Ok(affected);
            }
        }

        if !affected.working_tree && !worktree_paths.is_empty() {
            let index = repo.index_or_empty()?;
            let mut excludes = repo.excludes(
                &index,
                None,
                gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
            )?;
            for relative in &worktree_paths {
                let is_dir = workdir
                    .as_ref()
                    .is_some_and(|dir| dir.join(relative).is_dir());
                if !is_ignored(&mut excludes, relative, is_dir)? {
                    affected.working_tree = true;
                    break;
                }
            }
        }
        Ok(affected)
    }
}

/// What a change at `relative`, a path inside the git directory, affects.
fn classify_git_path(relative: &Path) -> Affected {
    let mut components = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        });
    let Some(first) = components.next() else {
        return Affected::everything();
    };
    // Git writes `<name>.lock` and renames it over `<name>`; the rename is
    // reported for `<name>` too.
    if relative
        .extension()
        .is_some_and(|extension| extension == "lock")
    {
        return Affected::default();
    }
    match first {
        "HEAD" | "packed-refs" | "refs" | "config" => Affected::everything(),
        "index" | "MERGE_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD" | "rebase-merge"
        | "rebase-apply" | "info" => Affected {
            refs: false,
            working_tree: true,
        },
        // Objects, logs, FETCH_HEAD, hooks and GitEZ's own data change
        // nothing shown until a ref moves.
        _ => Affected::default(),
    }
}

/// Whether `relative`, a directory if `is_dir`, or one of its parent
/// directories is ignored.
fn is_ignored(
    excludes: &mut gix::AttributeStack<'_>,
    relative: &Path,
    is_dir: bool,
) -> Result<bool> {
    // A file inside an ignored directory only matches the directory's
    // pattern when the directory itself is checked.
    let mut prefix = PathBuf::new();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        prefix.push(component);
        let mode = (is_dir || components.peek().is_some()).then_some(gix::index::entry::Mode::DIR);
        if excludes.at_path(&prefix, mode)?.is_excluded() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `path` with symlinks resolved, as the file system reports events, or as
/// given if it does not exist (any more).
fn canonical(path: &Path) -> PathBuf {
    if let Ok(path) = path.canonicalize() {
        return path;
    }
    // A deleted file: resolve its directory instead.
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonical(parent).join(name),
        _ => path.to_owned(),
    }
}
