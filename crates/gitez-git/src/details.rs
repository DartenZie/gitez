//! Everything about one commit beyond its history row: the full message and
//! the files it changed.

use gix::bstr::ByteSlice;
use gix::object::tree::diff::Change;

use crate::{Oid, Repository, Result, Signature};

/// Files after this many are listed without line counts, so a commit that
/// touches a whole vendored tree does not stall on diffing every file.
const MAX_FILES_WITH_LINE_COUNTS: usize = 2_000;

/// The full description of one commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDetails {
    pub id: Oid,
    /// The message after the summary line, without surrounding blank lines.
    /// Empty when the message is a single line.
    pub body: String,
    pub committer: Signature,
    /// Whether the commit carries a signature. It is not verified.
    pub signed: bool,
    /// Files changed relative to the first parent (or added, for a root
    /// commit), in path order.
    pub files: Vec<FileChange>,
}

impl CommitDetails {
    /// Lines added and removed across all files that have line counts.
    pub fn line_totals(&self) -> (usize, usize) {
        self.files
            .iter()
            .filter_map(|file| file.lines)
            .fold((0, 0), |(added, removed), lines| {
                (added + lines.added, removed + lines.removed)
            })
    }
}

/// How a file changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    /// Moved from `from`, possibly with edits.
    Renamed {
        from: String,
    },
    /// Copied from `from`, possibly with edits.
    Copied {
        from: String,
    },
}

/// One changed file of a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Path from the repository root, after the change.
    pub path: String,
    pub kind: ChangeKind,
    /// `None` for binary files, and for files past the line-count limit.
    pub lines: Option<LineCounts>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LineCounts {
    pub added: usize,
    pub removed: usize,
}

impl Repository {
    /// Reads the message and changed files of commit `id`. Merges are compared
    /// with their first parent, like the history's first-parent line.
    pub fn commit_details(&self, id: Oid) -> Result<CommitDetails> {
        let repo = self.local();
        let commit = repo.find_commit(id)?;
        let decoded = commit.decode()?;

        let message = decoded
            .message()
            .body()
            .map(|body| body.trim().to_str_lossy().into_owned());
        let committer = decoded.committer()?.trim();
        let committer = Signature {
            name: committer.name.to_str_lossy().into_owned(),
            email: committer.email.to_str_lossy().into_owned(),
            time: committer.time().unwrap_or_default().into(),
        };
        let signed = decoded.extra_headers().pgp_signature().is_some();
        drop(decoded);

        let (old_tree, new_tree) = trees(&repo, &commit)?;
        let mut line_cache = repo.diff_resource_cache_for_tree_diff()?;
        let mut files = Vec::new();
        let mut error = None;
        old_tree
            .changes()?
            .options(|options| {
                options.track_path();
            })
            .for_each_to_obtain_tree(&new_tree, |change| {
                if change.entry_mode().is_tree() {
                    return Ok(std::ops::ControlFlow::Continue(()));
                }
                let lines = if files.len() < MAX_FILES_WITH_LINE_COUNTS {
                    match line_counts(&change, &mut line_cache) {
                        Ok(lines) => lines,
                        Err(err) => {
                            error = Some(err);
                            return Ok(std::ops::ControlFlow::Break(()));
                        }
                    }
                } else {
                    None
                };
                files.push(FileChange {
                    path: change.location().to_str_lossy().into_owned(),
                    kind: kind(&change),
                    lines,
                });
                Ok(std::ops::ControlFlow::Continue(()))
            })?;
        if let Some(error) = error {
            return Err(error);
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(CommitDetails {
            id,
            body: message.unwrap_or_default(),
            committer,
            signed,
            files,
        })
    }
}

/// The trees commit `commit` compares: its first parent's and its own. A root
/// commit, or the boundary of a shallow clone, compares with the empty tree.
pub(crate) fn trees<'repo>(
    repo: &'repo gix::Repository,
    commit: &gix::Commit<'repo>,
) -> Result<(gix::Tree<'repo>, gix::Tree<'repo>)> {
    let first_parent = commit.parent_ids().next().map(|id| id.detach());
    // The boundary of a shallow clone has no parent to compare with.
    let is_shallow_boundary = repo
        .shallow_commits()?
        .is_some_and(|shallow| shallow.contains(&commit.id));
    let old_tree = match first_parent {
        Some(parent) if !is_shallow_boundary => repo.find_commit(parent)?.tree()?,
        _ => repo.empty_tree(),
    };
    Ok((old_tree, commit.tree()?))
}

pub(crate) fn kind(change: &Change<'_, '_, '_>) -> ChangeKind {
    match change {
        Change::Addition { .. } => ChangeKind::Added,
        Change::Deletion { .. } => ChangeKind::Deleted,
        Change::Modification { .. } => ChangeKind::Modified,
        Change::Rewrite {
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
    }
}

/// Lines added and removed by `change`; `None` for binary content and for
/// entries that are not files, such as submodules.
fn line_counts(
    change: &Change<'_, '_, '_>,
    cache: &mut gix::diff::blob::Platform,
) -> Result<Option<LineCounts>> {
    if !change.entry_mode().is_blob_or_symlink() {
        return Ok(None);
    }
    let stats = change.diff(cache)?.line_counts()?;
    // The cache only grows; one file's contents are all it needs to hold.
    cache.clear_resource_cache_keep_allocation();
    Ok(stats.map(|stats| LineCounts {
        added: stats.insertions as usize,
        removed: stats.removals as usize,
    }))
}
