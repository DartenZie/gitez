//! Line-by-line diffs of a commit's files, for reviewing it.

use gix::bstr::ByteSlice;
use gix::diff::blob::platform::prepare_diff::Operation;
use gix::diff::blob::unified_diff::{ConsumeHunk, ContextSize, DiffLineKind, HunkHeader};
use gix::diff::blob::{InternedInput, UnifiedDiff, diff_with_slider_heuristics};
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
    let platform = change.diff(cache)?;
    platform
        .resource_cache
        .options
        .skip_internal_diff_if_external_is_configured = false;
    let prepared = platform
        .resource_cache
        .prepare_diff()
        .map_err(gix::Error::from)?;
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
