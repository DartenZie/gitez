//! What merging one branch into another would do, worked out in memory
//! without touching the working tree, the index or the object database.

use gix::bstr::{BString, ByteSlice};
use gix::merge::tree::TreatAsUnresolved;
use gix::object::tree::EntryMode;
use gix::object::tree::diff::Change;

use crate::{Error, Oid, Repository, Result};

/// How a branch compares to the branch it would be merged into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePreview {
    /// Commits on the branch that the target lacks.
    pub ahead: usize,
    /// Commits on the target that the branch lacks: when there are none,
    /// the target can be fast-forwarded.
    pub behind: usize,
    /// Files the branch changes since it left the target, like
    /// `git diff --name-only target...branch`.
    pub files: usize,
    /// The files a merge would leave with conflicts, in path order.
    pub conflicts: Vec<String>,
}

impl Repository {
    /// What merging commit `head` into commit `base` would do, as
    /// `git merge` would see it.
    pub fn merge_preview(&self, base: Oid, head: Oid) -> Result<MergePreview> {
        let (ahead, behind) = self.ahead_behind(head, base)?;
        let conflicts = if ahead == 0 || behind == 0 {
            // Nothing to merge, or a fast-forward: nothing can conflict.
            Vec::new()
        } else {
            // Merged blobs and trees stay in memory.
            let repo = self.local().with_object_memory();
            let options = repo
                .tree_merge_options()
                .map_err(|error| Error::Merge(Box::new(error)))?;
            let outcome = repo
                .merge_commits(base, head, Default::default(), options.into())
                .map_err(|error| Error::Merge(Box::new(error)))?;
            let mut conflicts: Vec<String> = outcome
                .tree_merge
                .conflicts
                .iter()
                .filter(|conflict| conflict.is_unresolved(TreatAsUnresolved::git()))
                .map(|conflict| conflict.ours.location().to_str_lossy().into_owned())
                .collect();
            conflicts.sort();
            conflicts.dedup();
            conflicts
        };
        Ok(MergePreview {
            ahead,
            behind,
            files: self.changed_files(base, head)?,
            conflicts,
        })
    }

    /// How many files commit `head` changes since it left commit `base`,
    /// as [`Repository::branch_diff`] counts them.
    fn changed_files(&self, base: Oid, head: Oid) -> Result<usize> {
        // The tree compared with stays in memory.
        let repo = self.local().with_object_memory();
        let merge_base = repo.merge_base(base, head)?.map(|id| id.detach());
        let old_tree = tree_before_changes(&repo, merge_base, base, head)?;
        let new_tree = repo.find_commit(head)?.tree()?;
        let mut files = 0;
        old_tree
            .changes()?
            .for_each_to_obtain_tree(&new_tree, |change| {
                if !change.entry_mode().is_tree() {
                    files += 1;
                }
                Ok(std::ops::ControlFlow::Continue(()))
            })?;
        Ok(files)
    }
}

/// What a path holds in a tree: its mode and object, or nothing.
type Entry = Option<(EntryMode, gix::ObjectId)>;

/// The tree to compare commit `head` with to see the changes it would
/// bring into commit `base`: the tree of `merge_base`, where `head` left
/// `base`, with the changes `base` made since that `head` has as well,
/// however they got there (merged, squashed, cherry-picked or made twice).
/// Without a merge base it is the empty tree.
///
/// Every file in it is one of `base`, of `head` or of `merge_base`; only
/// trees are written, so `repo` should keep new objects
/// [in memory](gix::Repository::with_object_memory).
pub(crate) fn tree_before_changes<'repo>(
    repo: &'repo gix::Repository,
    merge_base: Option<Oid>,
    base: Oid,
    head: Oid,
) -> Result<gix::Tree<'repo>> {
    let Some(merge_base) = merge_base else {
        return Ok(repo.empty_tree());
    };
    let fork_tree = repo.find_commit(merge_base)?.tree()?;
    if merge_base == base || merge_base == head {
        // `head` has every commit of `base`, or none of its own.
        return Ok(fork_tree);
    }
    // A merge that cannot be worked out leaves the plain comparison with
    // the merge base, which shows too much rather than too little.
    Ok(tree_with_shared_changes(repo, &fork_tree, base, head).unwrap_or(fork_tree))
}

/// As [`tree_before_changes`], for commits that have both moved on from
/// `fork_tree`. Merging `base` into `head` in memory tells them apart:
///
/// - Where the merge leaves the file of `head` as it is, `head` has all of
///   `base`'s changes to it: it is compared with the file of `base`.
/// - Where the merge takes the file of `base` as it is, `head` has nothing
///   of its own in it: it is compared with itself, so it does not show.
/// - Elsewhere both changed the file, or they conflict: it is compared with
///   the file of `fork_tree`.
fn tree_with_shared_changes<'repo>(
    repo: &'repo gix::Repository,
    fork_tree: &gix::Tree<'repo>,
    base: Oid,
    head: Oid,
) -> Result<gix::Tree<'repo>> {
    let options = repo
        .tree_merge_options()
        .map_err(|error| Error::Merge(Box::new(error)))?;
    let mut merge = repo
        .merge_commits(head, base, Default::default(), options.into())
        .map_err(|error| Error::Merge(Box::new(error)))?
        .tree_merge;
    let merged_tree = merge
        .tree
        .write()
        .map_err(|error| Error::Merge(Box::new(error)))?;
    let merged_tree = repo.find_tree(merged_tree)?;
    let head_tree = repo.find_commit(head)?.tree()?;
    let base_tree = repo.find_commit(base)?.tree()?;

    // The files the merge would change in `head`, as they are in it and
    // after the merge.
    let mut differing: Vec<(BString, Entry, Entry)> = Vec::new();
    head_tree
        .changes()?
        .options(|options| {
            options.track_path();
            // Each path on its own.
            options.track_rewrites(None);
        })
        .for_each_to_obtain_tree(&merged_tree, |change| {
            match change {
                _ if change.entry_mode().is_tree() => {}
                Change::Addition {
                    location,
                    entry_mode,
                    id,
                    ..
                } => differing.push((location.to_owned(), None, Some((entry_mode, id.detach())))),
                Change::Deletion {
                    location,
                    entry_mode,
                    id,
                    ..
                } => differing.push((location.to_owned(), Some((entry_mode, id.detach())), None)),
                Change::Modification {
                    location,
                    previous_entry_mode,
                    previous_id,
                    entry_mode,
                    id,
                } => differing.push((
                    location.to_owned(),
                    Some((previous_entry_mode, previous_id.detach())),
                    Some((entry_mode, id.detach())),
                )),
                // Not tracked.
                Change::Rewrite { .. } => {}
            }
            Ok(std::ops::ControlFlow::Continue(()))
        })?;

    let mut old = base_tree
        .edit()
        .map_err(|error| Error::Merge(Box::new(error)))?;
    let mut set = |path: &BString, entry: Entry| {
        match entry {
            Some((mode, id)) => old.upsert(path, mode.kind(), id),
            None => old.remove(path),
        }
        .map(drop)
        .map_err(|error| Error::Merge(Box::new(error)))
    };
    for (path, in_head, merged) in &differing {
        let from_base =
            in_head.is_some() && merged.is_some() && *merged == entry(&base_tree, path)?;
        set(
            path,
            if from_base {
                *in_head
            } else {
                entry(fork_tree, path)?
            },
        )?;
    }
    // Whatever the merge left there, neither side has the other's changes.
    let unresolved = merge
        .conflicts
        .iter()
        .filter(|conflict| conflict.is_unresolved(TreatAsUnresolved::git()));
    for conflict in unresolved {
        for path in [conflict.ours.location(), conflict.theirs.location()] {
            let path = path.to_owned();
            set(&path, entry(fork_tree, &path)?)?;
        }
    }
    let old = old.write().map_err(|error| Error::Merge(Box::new(error)))?;
    Ok(repo.find_tree(old)?)
}

/// What `path` holds in `tree`.
fn entry(tree: &gix::Tree<'_>, path: &BString) -> Result<Entry> {
    let entry = tree.lookup_entry(path.split(|byte| *byte == b'/'))?;
    Ok(entry.map(|entry| (entry.mode(), entry.object_id())))
}

/// A merge git stopped in the middle of, to have its conflicts resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeInProgress {
    /// The commit checked out when the merge started ("ours").
    pub ours: Oid,
    /// The branch checked out, unless `HEAD` is detached.
    pub branch: Option<String>,
    /// The commit being merged in ("theirs").
    pub theirs: Oid,
    /// The local branch on `theirs`, if one is.
    pub theirs_branch: Option<String>,
    /// The merge commit's message as git prepared it.
    pub message: String,
}

impl Repository {
    /// The merge in progress in this working tree, if git stopped one to
    /// have conflicts resolved.
    pub fn merge_in_progress(&self) -> Result<Option<MergeInProgress>> {
        let read = |name: &str| {
            let path = self.git_dir().join(name);
            match std::fs::read_to_string(&path) {
                Ok(text) => Ok(Some(text)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(source) => Err(Error::ReadFile { path, source }),
            }
        };
        let Some(merge_head) = read("MERGE_HEAD")? else {
            return Ok(None);
        };
        // An octopus merge lists several; GitEZ resolves the first.
        let Some(theirs) = merge_head
            .lines()
            .next()
            .and_then(|hex| Oid::from_hex(hex.trim().as_bytes()).ok())
        else {
            return Ok(None);
        };
        let refs = self.refs()?;
        let (ours, branch) = match refs.head {
            Some(crate::Head::Branch { name, target }) => (target, Some(name)),
            Some(crate::Head::Detached { target }) => (target, None),
            _ => return Ok(None),
        };
        let theirs_branch = refs
            .branches
            .iter()
            .find(|candidate| candidate.target == theirs)
            .map(|candidate| candidate.name.clone());
        Ok(Some(MergeInProgress {
            ours,
            branch,
            theirs,
            theirs_branch,
            message: read("MERGE_MSG")?.unwrap_or_default(),
        }))
    }

    /// The conflicts in the working tree's file at `path`, read from its
    /// conflict markers; `None` when it has none git wrote, such as a
    /// binary file or one deleted on one side.
    pub fn conflict_file(&self, path: &str) -> Result<Option<crate::ConflictFile>> {
        let Some(dir) = self.work_dir() else {
            return Ok(None);
        };
        let path = dir.join(path);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(std::str::from_utf8(&bytes)
                .ok()
                .and_then(crate::ConflictFile::parse)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::ReadFile { path, source }),
        }
    }
}
