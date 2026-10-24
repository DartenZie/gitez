//! Line-by-line diffs of a commit's files, or of the uncommitted changes,
//! for reviewing them.

use gix::bstr::ByteSlice;
use gix::diff::blob::pipeline::{Mode, WorktreeRoots};
use gix::diff::blob::platform::prepare_diff::Operation;
use gix::diff::blob::unified_diff::{ConsumeHunk, ContextSize, DiffLineKind, HunkHeader};
use gix::diff::blob::{InternedInput, ResourceKind, UnifiedDiff, diff_with_slider_heuristics};
use gix::object::tree::EntryKind;
use gix::object::tree::diff::Change;

use crate::details::{kind, trees};
use crate::{ChangeKind, Error, LineCounts, Oid, Repository, Result};

/// Unchanged lines shown around each change, as `git diff` does.
const CONTEXT_LINES: u32 = 3;
/// A file whose diff has more lines than this is not shown line by line.
const MAX_FILE_LINES: usize = 10_000;
/// Once a commit's diffs reach this many lines, the remaining files are not
/// shown line by line, so a huge commit stays quick to open.
const MAX_COMMIT_LINES: usize = 100_000;

/// The diff of one changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Path from the repository root, after the change.
    pub path: String,
    pub kind: ChangeKind,
    pub content: DiffContent,
}

impl FileDiff {
    /// Lines added and removed, if the content is text.
    pub fn line_counts(&self) -> Option<LineCounts> {
        match &self.content {
            DiffContent::Text(hunks) => Some(hunks.iter().flat_map(|hunk| &hunk.lines).fold(
                LineCounts::default(),
                |mut counts, line| {
                    match line.kind {
                        LineKind::Added => counts.added += 1,
                        LineKind::Removed => counts.removed += 1,
                        LineKind::Context => {}
                    }
                    counts
                },
            )),
            DiffContent::TooLarge(lines) => Some(*lines),
            DiffContent::Binary | DiffContent::Unchanged => None,
        }
    }
}

/// What changed in a file's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffContent {
    /// Changed lines with some unchanged ones around them, in order.
    Text(Vec<Hunk>),
    /// Too many changed lines to show; only the counts.
    TooLarge(LineCounts),
    /// One side is binary.
    Binary,
    /// The content is the same: a rename, copy or mode change.
    Unchanged,
}

/// A run of changed lines and their context. Ranges follow `git diff`: an
/// empty range starts at the line before it (0 at the start of the file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// First line in the old file, 1-based.
    pub old_start: u32,
    pub old_len: u32,
    /// First line in the new file, 1-based.
    pub new_start: u32,
    pub new_len: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// Line number in the old file; `None` for added lines.
    pub old: Option<u32>,
    /// Line number in the new file; `None` for removed lines.
    pub new: Option<u32>,
    /// The line without its line ending.
    pub text: String,
    /// The line is the last of its file and has no line ending, which
    /// `git diff` marks with "\\ No newline at end of file".
    pub no_newline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

impl Repository {
    /// The diff of every file commit `id` changed, against its first parent
    /// (or the empty tree), in path order.
    pub fn commit_diff(&self, id: Oid) -> Result<Vec<FileDiff>> {
        let repo = self.local();
        let commit = repo.find_commit(id)?;
        let (old_tree, new_tree) = trees(&repo, &commit)?;

        let mut cache = repo.diff_resource_cache_for_tree_diff()?;
        let mut files = Vec::new();
        let mut budget = MAX_COMMIT_LINES;
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
                match content(&change, &mut cache, &mut budget) {
                    Ok(content) => files.push(FileDiff {
                        path: change.location().to_str_lossy().into_owned(),
                        kind: kind(&change),
                        content,
                    }),
                    Err(err) => {
                        error = Some(err);
                        return Ok(std::ops::ControlFlow::Break(()));
                    }
                }
                Ok(std::ops::ControlFlow::Continue(()))
            })?;
        if let Some(error) = error {
            return Err(error);
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }
}

impl Repository {
    /// The diff of every uncommitted change, from `HEAD` to the files in the
    /// working tree, like `git diff HEAD` with untracked files shown as
    /// added. Staged and unstaged changes to a file are shown together. In
    /// path order; empty for a bare repository.
    pub fn working_tree_diff(&self) -> Result<Vec<FileDiff>> {
        let status = self.status()?;
        let repo = self.local();
        let Some(workdir) = repo.workdir().map(ToOwned::to_owned) else {
            return Ok(Vec::new());
        };
        let head_tree = repo.find_tree(repo.head_tree_id_or_empty()?)?;
        let mut cache = repo.diff_resource_cache(
            Mode::ToGit,
            WorktreeRoots {
                old_root: None,
                new_root: Some(workdir.clone()),
            },
        )?;

        let mut budget = MAX_COMMIT_LINES;
        let mut files = Vec::with_capacity(status.files.len());
        for file in &status.files {
            let from = match &file.staged {
                Some(ChangeKind::Renamed { from } | ChangeKind::Copied { from }) => Some(from),
                _ => None,
            };
            let old_path = from.unwrap_or(&file.path);
            let old = head_tree
                .lookup_entry_by_path(old_path)?
                .map(|entry| (entry.object_id(), entry.mode().kind()));
            let metadata = std::fs::symlink_metadata(workdir.join(&file.path)).ok();
            let kind = match (&file.staged, &old, &metadata) {
                (Some(kind @ (ChangeKind::Renamed { .. } | ChangeKind::Copied { .. })), _, _) => {
                    kind.clone()
                }
                (_, None, _) => ChangeKind::Added,
                (_, Some(_), None) => ChangeKind::Deleted,
                (_, Some(_), Some(_)) => ChangeKind::Modified,
            };
            if old.is_none() && metadata.is_none() {
                // Staged and then deleted again: nothing left to compare.
                continue;
            }
            let content = if metadata.as_ref().is_some_and(std::fs::Metadata::is_dir)
                || old.is_some_and(|(_, kind)| {
                    !matches!(
                        kind,
                        EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link
                    )
                }) {
                // A submodule or nested repository.
                DiffContent::Unchanged
            } else {
                let (old_id, old_kind) =
                    old.unwrap_or((repo.object_hash().null(), EntryKind::Blob));
                let new_kind = match &metadata {
                    Some(metadata) if metadata.is_symlink() => EntryKind::Link,
                    Some(metadata) if is_executable(metadata) => EntryKind::BlobExecutable,
                    _ => EntryKind::Blob,
                };
                cache
                    .set_resource(
                        old_id,
                        old_kind,
                        old_path.as_str().into(),
                        ResourceKind::OldOrSource,
                        &repo.objects,
                    )
                    .map_err(gix::Error::from)?;
                cache
                    .set_resource(
                        repo.object_hash().null(),
                        new_kind,
                        file.path.as_str().into(),
                        ResourceKind::NewOrDestination,
                        &repo.objects,
                    )
                    .map_err(gix::Error::from)?;
                prepared_content(&mut cache, &mut budget)?
            };
            files.push(FileDiff {
                path: file.path.clone(),
                kind,
                content,
            });
        }
        Ok(files)
    }
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &std::fs::Metadata) -> bool {
    false
}

/// The diff of `change`'s content, using at most `budget` lines.
fn content(
    change: &Change<'_, '_, '_>,
    cache: &mut gix::diff::blob::Platform,
    budget: &mut usize,
) -> Result<DiffContent> {
    if !change.entry_mode().is_blob_or_symlink() {
        // A submodule moved to another commit.
        return Ok(DiffContent::Unchanged);
    }
    change.diff(cache)?;
    prepared_content(cache, budget)
}

/// The diff of the two resources set in `cache`, using at most `budget`
/// lines.
fn prepared_content(
    cache: &mut gix::diff::blob::Platform,
    budget: &mut usize,
) -> Result<DiffContent> {
    cache.options.skip_internal_diff_if_external_is_configured = false;
    let prepared = cache.prepare_diff().map_err(gix::Error::from)?;
    let content = match prepared.operation {
        Operation::InternalDiff { algorithm } => {
            // Lines keep their endings, so adding or removing the newline at
            // the end of a file is a change, as in `git diff`.
            let input =
                InternedInput::new(prepared.old.intern_source(), prepared.new.intern_source());
            let diff = diff_with_slider_heuristics(algorithm, &input);
            let changed = (diff.count_additions() + diff.count_removals()) as usize;
            if changed == 0 {
                DiffContent::Unchanged
            } else if changed > MAX_FILE_LINES || changed > *budget {
                DiffContent::TooLarge(LineCounts {
                    added: diff.count_additions() as usize,
                    removed: diff.count_removals() as usize,
                })
            } else {
                let hunks = UnifiedDiff::new(
                    &diff,
                    &input,
                    Hunks::default(),
                    ContextSize::symmetrical(CONTEXT_LINES),
                )
                .consume()
                .map_err(Error::Diff)?;
                *budget = budget.saturating_sub(hunks.iter().map(|hunk| hunk.lines.len()).sum());
                DiffContent::Text(hunks)
            }
        }
        Operation::SourceOrDestinationIsBinary => DiffContent::Binary,
        Operation::ExternalCommand { .. } => {
            unreachable!("external diff commands are disabled above")
        }
    };
    // The cache only grows; one file's contents are all it needs to hold.
    cache.clear_resource_cache_keep_allocation();
    Ok(content)
}

/// Collects the hunks of a unified diff with numbered lines.
#[derive(Default)]
struct Hunks(Vec<Hunk>);

impl ConsumeHunk for Hunks {
    type Out = Vec<Hunk>;

    fn consume_hunk(
        &mut self,
        header: HunkHeader,
        lines: &[(DiffLineKind, &[u8])],
    ) -> std::io::Result<()> {
        let mut old = header.before_hunk_start;
        let mut new = header.after_hunk_start;
        let lines = lines
            .iter()
            .map(|&(line_kind, text)| {
                let (kind, old_number, new_number) = match line_kind {
                    DiffLineKind::Context => {
                        old += 1;
                        new += 1;
                        (LineKind::Context, Some(old - 1), Some(new - 1))
                    }
                    DiffLineKind::Remove => {
                        old += 1;
                        (LineKind::Removed, Some(old - 1), None)
                    }
                    DiffLineKind::Add => {
                        new += 1;
                        (LineKind::Added, None, Some(new - 1))
                    }
                };
                let (text, no_newline) = match text.strip_suffix(b"\n") {
                    Some(text) => (text.strip_suffix(b"\r").unwrap_or(text), false),
                    None => (text, true),
                };
                DiffLine {
                    kind,
                    old: old_number,
                    new: new_number,
                    text: text.to_str_lossy().into_owned(),
                    no_newline,
                }
            })
            .collect();
        // gix starts an empty range at the line after it, git at the one
        // before.
        let start = |start: u32, len: u32| {
            if len == 0 {
                start.saturating_sub(1)
            } else {
                start
            }
        };
        self.0.push(Hunk {
            old_start: start(header.before_hunk_start, header.before_hunk_len),
            old_len: header.before_hunk_len,
            new_start: start(header.after_hunk_start, header.after_hunk_len),
            new_len: header.after_hunk_len,
            lines,
        });
        Ok(())
    }

    fn finish(self) -> Self::Out {
        self.0
    }
}
