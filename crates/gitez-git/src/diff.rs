//! Line-by-line diffs of a commit's files, or of the uncommitted changes,
//! for reviewing them.

use gix::bstr::ByteSlice;
use gix::diff::blob::pipeline::{Mode, WorktreeRoots};
use gix::diff::blob::platform::prepare_diff::Operation;
use gix::diff::blob::unified_diff::{ConsumeHunk, ContextSize, DiffLineKind, HunkHeader};
use gix::diff::blob::{
    InternedInput, Interner, ResourceKind, UnifiedDiff, diff_with_slider_heuristics,
};
use gix::object::tree::EntryKind;
use gix::object::tree::diff::Change;

use crate::details::{kind, trees};
use crate::{ChangeKind, Error, LineCounts, Oid, Repository, Result, WorktreeChange};

/// Unchanged lines shown around each change, as `git diff` does.
const CONTEXT_LINES: u32 = 3;
/// A file whose diff has more lines than this is not shown line by line.
const MAX_FILE_LINES: usize = 10_000;
/// Once a commit's diffs reach this many lines, the remaining files are not
/// shown line by line, so a huge commit stays quick to open.
const MAX_COMMIT_LINES: usize = 100_000;

/// How to compare files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffOptions {
    /// Compare lines as if they had no whitespace, as `git diff -w` does.
    /// Hunks are then never [patchable](Hunk::patchable), since their lines
    /// need not match the files exactly.
    pub ignore_whitespace: bool,
}

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
        self.content.line_counts()
    }
}

impl DiffContent {
    /// Lines added and removed, if the content is text.
    pub fn line_counts(&self) -> Option<LineCounts> {
        match self {
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
            DiffContent::Binary | DiffContent::Unchanged | DiffContent::OnlyWhitespace => None,
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
    /// Only whitespace changed, and [`DiffOptions::ignore_whitespace`] is
    /// set.
    OnlyWhitespace,
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
    /// For uncommitted changes, whether the hunk is staged; `None` in a
    /// commit.
    pub staging: Option<Staging>,
    /// Whether [`Hunk::patch`] stages or unstages exactly this hunk: the
    /// file exists on both sides and its text is UTF-8.
    pub patchable: bool,
    /// The file the hunk's new lines are from, to show the unchanged lines
    /// around it; `None` when there is none, as for a deleted file.
    pub new_side: Option<NewSide>,
}

/// The new version of a file a hunk is from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSide {
    pub source: LineSource,
    /// How many lines it has.
    pub lines: u32,
}

/// Where a version of a file is stored; see [`Repository::lines`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LineSource {
    /// A blob: committed or staged.
    Blob(Oid),
    /// The file in the working tree at this path from the repository root.
    WorkingTree(String),
}

/// Which part of the uncommitted changes a hunk belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staging {
    /// From `HEAD` to the index: part of the next commit.
    Staged,
    /// From the index to the working tree.
    Unstaged,
}

impl Hunk {
    /// A patch of this hunk alone to the file at `path`, for
    /// `git apply --cached`: as is to stage an unstaged hunk, reversed to
    /// unstage a staged one. Only meaningful when [`patchable`](Self::patchable).
    pub fn patch(&self, path: &str) -> String {
        // Without the hunks before it, the hunk starts where it does in the
        // old file.
        let new_start = match (self.old_len, self.new_len) {
            (0, 0) => self.old_start,
            (0, _) => self.old_start + 1,
            (_, 0) => self.old_start.saturating_sub(1),
            _ => self.old_start,
        };
        let (old, new) = (quote("a/", path), quote("b/", path));
        let mut patch = format!(
            "diff --git {old} {new}\n--- {old}\n+++ {new}\n@@ -{},{} +{new_start},{} @@\n",
            self.old_start, self.old_len, self.new_len
        );
        for line in &self.lines {
            patch.push(match line.kind {
                LineKind::Context => ' ',
                LineKind::Added => '+',
                LineKind::Removed => '-',
            });
            patch.push_str(&line.text);
            if line.crlf {
                patch.push('\r');
            }
            patch.push('\n');
            if line.no_newline {
                patch.push_str("\\ No newline at end of file\n");
            }
        }
        patch
    }
}

/// `prefix` and `path` as a patch names a file, quoted like git does when
/// the path has quotes, backslashes or control characters.
fn quote(prefix: &str, path: &str) -> String {
    if !path
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_control())
    {
        return format!("{prefix}{path}");
    }
    let mut quoted = format!("\"{prefix}");
    for c in path.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            c if c.is_control() => {
                let mut buffer = [0; 4];
                for byte in c.encode_utf8(&mut buffer).bytes() {
                    quoted.push_str(&format!("\\{byte:03o}"));
                }
            }
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
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
    /// The line ends with a carriage return before the newline.
    pub crlf: bool,
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
    pub fn commit_diff(&self, id: Oid, options: DiffOptions) -> Result<Vec<FileDiff>> {
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
                match content(&change, &mut cache, &mut budget, options) {
                    Ok(content) => files.push(FileDiff {
                        path: change.location().to_str_lossy().into_owned(),
                        kind: kind(&change),
                        content: with_staging(content, None, false),
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
    /// The diff of every uncommitted change, with untracked files shown as
    /// added. Each file's staged hunks (`git diff --cached`) come before its
    /// unstaged ones (`git diff`); see [`Hunk::staging`]. In path order;
    /// empty for a bare repository.
    pub fn working_tree_diff(&self, options: DiffOptions) -> Result<Vec<FileDiff>> {
        let status = self.status()?;
        let repo = self.local();
        let Some(workdir) = repo.workdir().map(ToOwned::to_owned) else {
            return Ok(Vec::new());
        };
        let head_tree = repo.find_tree(repo.head_tree_id_or_empty()?)?;
        let index = repo.index_or_empty()?;
        let null = repo.object_hash().null();
        // `HEAD` to the index: both sides are stored objects, and a null id
        // is an empty side.
        let mut staged_cache = repo.diff_resource_cache(Mode::ToGit, WorktreeRoots::default())?;
        // To the working tree: a null id on the new side reads the file.
        let mut worktree_cache = repo.diff_resource_cache(
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
            let head = head_tree
                .lookup_entry_by_path(old_path)?
                .map(|entry| (entry.object_id(), entry.mode().kind()));
            let indexed = index
                .entry_by_path(file.path.as_str().into())
                .and_then(|entry| Some((entry.id, entry.mode.to_tree_entry_mode()?.kind())));
            let metadata = std::fs::symlink_metadata(workdir.join(&file.path)).ok();
            let kind = match (&file.staged, &head, &metadata) {
                (Some(kind @ (ChangeKind::Renamed { .. } | ChangeKind::Copied { .. })), _, _) => {
                    kind.clone()
                }
                (_, None, _) => ChangeKind::Added,
                (_, Some(_), None) => ChangeKind::Deleted,
                (_, Some(_), Some(_)) => ChangeKind::Modified,
            };
            if head.is_none() && metadata.is_none() {
                // Staged and then deleted again: nothing left to compare.
                continue;
            }
            let worktree_kind = match &metadata {
                Some(metadata) if metadata.is_symlink() => EntryKind::Link,
                Some(metadata) if is_executable(metadata) => EntryKind::BlobExecutable,
                _ => EntryKind::Blob,
            };
            // The file on disk, unless it is gone or a symlink.
            let on_disk = metadata
                .as_ref()
                .filter(|metadata| metadata.is_file())
                .map(|_| LineSource::WorkingTree(file.path.clone()));
            let is_file = |kind: EntryKind| {
                matches!(
                    kind,
                    EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link
                )
            };

            let content = if metadata.as_ref().is_some_and(std::fs::Metadata::is_dir)
                || [head, indexed]
                    .into_iter()
                    .flatten()
                    .any(|(_, kind)| !is_file(kind))
            {
                // A submodule or nested repository.
                DiffContent::Unchanged
            } else if file.unstaged == Some(WorktreeChange::Conflicted) {
                // The index holds the conflict's sides, not one version:
                // compare `HEAD` with the file and its conflict markers.
                let (id, old_kind) = head.unwrap_or((null, EntryKind::Blob));
                set_sides(
                    &repo,
                    &mut worktree_cache,
                    (id, old_kind, old_path),
                    (null, worktree_kind, &file.path),
                )?;
                prepared_content(&mut worktree_cache, &mut budget, options, on_disk)?
            } else {
                let staged = match &file.staged {
                    Some(_) => {
                        let (old_id, old_kind) = head.unwrap_or((null, EntryKind::Blob));
                        let (new_id, new_kind) = indexed.unwrap_or((null, EntryKind::Blob));
                        set_sides(
                            &repo,
                            &mut staged_cache,
                            (old_id, old_kind, old_path),
                            (new_id, new_kind, &file.path),
                        )?;
                        let staged_blob = indexed
                            .filter(|&(_, kind)| kind != EntryKind::Link)
                            .map(|(id, _)| LineSource::Blob(id));
                        let content =
                            prepared_content(&mut staged_cache, &mut budget, options, staged_blob)?;
                        let patchable = head.is_some() && indexed.is_some();
                        Some(with_staging(content, Some(Staging::Staged), patchable))
                    }
                    None => None,
                };
                let unstaged = match file.unstaged {
                    Some(_) => {
                        let (old_id, old_kind) = indexed.unwrap_or((null, EntryKind::Blob));
                        set_sides(
                            &repo,
                            &mut worktree_cache,
                            (old_id, old_kind, &file.path),
                            (null, worktree_kind, &file.path),
                        )?;
                        let content = prepared_content(
                            &mut worktree_cache,
                            &mut budget,
                            options,
                            on_disk.clone(),
                        )?;
                        let patchable = indexed.is_some() && metadata.is_some();
                        Some(with_staging(content, Some(Staging::Unstaged), patchable))
                    }
                    None => None,
                };
                combine(staged, unstaged)
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

impl Repository {
    /// The lines of the file stored at `source`, without line endings, as
    /// numbered in a diff.
    pub fn lines(&self, source: &LineSource) -> Result<Vec<String>> {
        let data = match source {
            LineSource::Blob(id) => self.local().find_object(*id)?.detach().data,
            LineSource::WorkingTree(path) => {
                let path = self.work_dir().unwrap_or(self.git_dir()).join(path);
                std::fs::read(&path).map_err(|source| Error::ReadFile { path, source })?
            }
        };
        Ok(data
            .lines()
            .map(|line| line.to_str_lossy().into_owned())
            .collect())
    }
}

/// Sets the old and new side of the next diff in `cache`: object id, kind
/// and path of each.
fn set_sides(
    repo: &gix::Repository,
    cache: &mut gix::diff::blob::Platform,
    (old_id, old_kind, old_path): (Oid, EntryKind, &str),
    (new_id, new_kind, new_path): (Oid, EntryKind, &str),
) -> Result<()> {
    cache
        .set_resource(
            old_id,
            old_kind,
            old_path.into(),
            ResourceKind::OldOrSource,
            &repo.objects,
        )
        .map_err(gix::Error::from)?;
    cache
        .set_resource(
            new_id,
            new_kind,
            new_path.into(),
            ResourceKind::NewOrDestination,
            &repo.objects,
        )
        .map_err(gix::Error::from)?;
    Ok(())
}

/// `content` with its hunks marked as `staging`, and as patchable when
/// `patchable` allows and their text does.
fn with_staging(content: DiffContent, staging: Option<Staging>, patchable: bool) -> DiffContent {
    match content {
        DiffContent::Text(mut hunks) => {
            for hunk in &mut hunks {
                hunk.staging = staging;
                hunk.patchable &= patchable;
            }
            DiffContent::Text(hunks)
        }
        other => other,
    }
}

/// One file's staged and unstaged changes as one content: the staged
/// hunks, then the unstaged ones.
fn combine(staged: Option<DiffContent>, unstaged: Option<DiffContent>) -> DiffContent {
    use DiffContent::{Binary, OnlyWhitespace, Text, TooLarge, Unchanged};
    match (staged, unstaged) {
        (Some(Text(mut staged)), Some(Text(unstaged))) => {
            staged.extend(unstaged);
            Text(staged)
        }
        (Some(Binary), _) | (_, Some(Binary)) => Binary,
        (Some(TooLarge(a)), Some(other)) | (Some(other), Some(TooLarge(a))) => {
            let b = other.line_counts().unwrap_or_default();
            TooLarge(LineCounts {
                added: a.added + b.added,
                removed: a.removed + b.removed,
            })
        }
        (Some(Unchanged) | None, Some(content)) | (Some(content), Some(Unchanged) | None) => {
            content
        }
        (Some(OnlyWhitespace), Some(content)) | (Some(content), Some(OnlyWhitespace)) => content,
        (None, None) => Unchanged,
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
    options: DiffOptions,
) -> Result<DiffContent> {
    if !change.entry_mode().is_blob_or_symlink() {
        // A submodule moved to another commit.
        return Ok(DiffContent::Unchanged);
    }
    change.diff(cache)?;
    let source = match change {
        Change::Deletion { .. } => None,
        _ if change.entry_mode().is_link() => None,
        _ => Some(LineSource::Blob(change.id().detach())),
    };
    prepared_content(cache, budget, options, source)
}

/// The diff of the two resources set in `cache`, using at most `budget`
/// lines. The new resource is stored at `new_source`, if anywhere.
fn prepared_content(
    cache: &mut gix::diff::blob::Platform,
    budget: &mut usize,
    options: DiffOptions,
    new_source: Option<LineSource>,
) -> Result<DiffContent> {
    cache.options.skip_internal_diff_if_external_is_configured = false;
    let prepared = cache.prepare_diff().map_err(gix::Error::from)?;
    let content = match prepared.operation {
        Operation::InternalDiff { algorithm } => {
            // Lines keep their endings, so adding or removing the newline at
            // the end of a file is a change, as in `git diff`.
            let input =
                InternedInput::new(prepared.old.intern_source(), prepared.new.intern_source());
            let diff = if options.ignore_whitespace {
                // Diffed without whitespace, then shown from `input`: the
                // lines are the same, one for one.
                diff_with_slider_heuristics(algorithm, &without_whitespace(&input))
            } else {
                diff_with_slider_heuristics(algorithm, &input)
            };
            let changed = (diff.count_additions() + diff.count_removals()) as usize;
            if changed == 0 && input.before != input.after {
                DiffContent::OnlyWhitespace
            } else if changed == 0 {
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
                .map_err(Error::Diff)?
                .into_iter()
                .map(|mut hunk| {
                    hunk.patchable &= !options.ignore_whitespace;
                    hunk.new_side = new_source.clone().map(|source| NewSide {
                        source,
                        lines: input.after.len() as u32,
                    });
                    hunk
                })
                .collect::<Vec<_>>();
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

/// `input` with every line stripped of its whitespace, line ending included.
fn without_whitespace(input: &InternedInput<&[u8]>) -> InternedInput<Vec<u8>> {
    let mut stripped = InternedInput {
        before: Vec::new(),
        after: Vec::new(),
        interner: Interner::new(input.interner.num_tokens() as usize),
    };
    let strip = |&token: &_| -> Vec<u8> {
        input.interner[token]
            .iter()
            .copied()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect()
    };
    stripped.update_before(input.before.iter().map(strip));
    stripped.update_after(input.after.iter().map(strip));
    stripped
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
        let mut exact = true;
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
                let (text, crlf, no_newline) = match text.strip_suffix(b"\n") {
                    Some(text) => match text.strip_suffix(b"\r") {
                        Some(text) => (text, true, false),
                        None => (text, false, false),
                    },
                    None => (text, false, true),
                };
                exact &= text.to_str().is_ok();
                DiffLine {
                    kind,
                    old: old_number,
                    new: new_number,
                    text: text.to_str_lossy().into_owned(),
                    crlf,
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
            staging: None,
            // Set for uncommitted changes, where it matters.
            patchable: exact,
            new_side: None,
        });
        Ok(())
    }

    fn finish(self) -> Self::Out {
        self.0
    }
}
