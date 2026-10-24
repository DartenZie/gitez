//! The state of the working tree: what is staged, what is changed but not
//! staged, and what git does not track yet.

use std::collections::BTreeMap;

use gix::bstr::ByteSlice;
use gix::status::index_worktree::iter::Summary;

use crate::{ChangeKind, Repository, Result};

/// Uncommitted changes, one entry per path, in path order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkingTreeStatus {
    pub files: Vec<FileStatus>,
}

impl WorkingTreeStatus {
    /// Whether there is nothing to commit and nothing untracked.
    pub fn is_clean(&self) -> bool {
        self.files.is_empty()
    }

    /// Files with staged changes.
    pub fn staged(&self) -> usize {
        self.files
            .iter()
            .filter(|file| file.staged.is_some())
            .count()
    }

    /// Files with a merge conflict.
    pub fn conflicted(&self) -> usize {
        self.files
            .iter()
            .filter(|file| file.unstaged == Some(WorktreeChange::Conflicted))
            .count()
    }
}

/// One path with uncommitted changes. At least one side is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStatus {
    /// Path from the repository root.
    pub path: String,
    /// The change from `HEAD` to the index.
    pub staged: Option<ChangeKind>,
    /// The change from the index to the working tree.
    pub unstaged: Option<WorktreeChange>,
}

/// How a file in the working tree differs from the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeChange {
    /// Edited, or changed type (for example a file replaced by a symlink).
    Modified,
    Deleted,
    /// Not in the index, or added with `git add --intent-to-add`.
    Untracked,
    /// Unmerged after a merge, rebase, cherry-pick or stash pop.
    Conflicted,
}

impl Repository {
    /// Reads the working tree's status like `git status --untracked-files=all`:
    /// staged and unstaged changes and every untracked file that is not
    /// ignored. Empty for a bare repository.
    ///
    /// The index is not refreshed on disk, so files whose timestamps changed
    /// without a content change are hashed on every call until `git` itself
    /// updates the index.
    pub fn status(&self) -> Result<WorkingTreeStatus> {
        let repo = self.local();
        if repo.workdir().is_none() {
            return Ok(WorkingTreeStatus::default());
        }

        let mut files = BTreeMap::<String, FileStatus>::new();

        let items = repo
            .status(gix::progress::Discard)?
            .untracked_files(gix::status::UntrackedFiles::Files)
            .into_iter(None)?;
        for item in items {
            match item? {
                gix::status::Item::TreeIndex(change) => {
                    use gix::diff::index::ChangeRef;
                    let kind = match &change {
                        ChangeRef::Addition { .. } => ChangeKind::Added,
                        ChangeRef::Deletion { .. } => ChangeKind::Deleted,
                        ChangeRef::Modification { .. } => ChangeKind::Modified,
                        ChangeRef::Rewrite {
                            source_location,
                            copy,
                            ..
                        } => {
                            let from = source_location.to_str_lossy().into_owned();
                            if *copy {
                                ChangeKind::Copied { from }
                            } else {
                                ChangeKind::Renamed { from }
                            }
                        }
                    };
                    entry(&mut files, change.location()).staged = Some(kind);
                }
                gix::status::Item::IndexWorktree(item) => {
                    let change = match item.summary() {
                        None => continue,
                        Some(Summary::Modified | Summary::TypeChange) => WorktreeChange::Modified,
                        Some(Summary::Removed) => WorktreeChange::Deleted,
                        Some(Summary::Added | Summary::IntentToAdd) => WorktreeChange::Untracked,
                        Some(Summary::Conflict) => WorktreeChange::Conflicted,
                        // Rename tracking between the index and the working
                        // tree is off, so these do not occur.
                        Some(Summary::Renamed | Summary::Copied) => WorktreeChange::Modified,
                    };
                    entry(&mut files, item.rela_path()).unstaged = Some(change);
                }
            }
        }

        Ok(WorkingTreeStatus {
            files: files.into_values().collect(),
        })
    }
}

/// The status of `path` in `files`, added with neither side set if new.
fn entry<'a>(files: &'a mut BTreeMap<String, FileStatus>, path: &[u8]) -> &'a mut FileStatus {
    let path = path.to_str_lossy().into_owned();
    files.entry(path.clone()).or_insert(FileStatus {
        path,
        staged: None,
        unstaged: None,
    })
}
