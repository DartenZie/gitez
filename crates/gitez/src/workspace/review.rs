//! The diff of one commit, of the uncommitted changes or of a local pull
//! request: every changed file, one after another, as in the
//! `Commit.dc.html` and `LocalPR.dc.html` artboards. Files and hunks of the
//! uncommitted changes can be staged here; they are committed from the
//! changes panel beside the history.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::sync::Arc;

use anyhow::Context as _;
use gitez_command::{Git, ResetMode};
use gitez_git::{
    BranchDiff, ChangeKind, Commit, DiffContent, DiffLine, DiffOptions, FileDiff, FileStatus,
    Followed, Hunk, LineKind, LineSource, Oid, RefKind, RefLabel, Repository, Staging,
    WorkingTreeStatus, WorktreeChange,
};
use gitez_highlight::{Highlights, Language, Outcome, Span};
use gitez_review::{FileUpdate, LineChange, PullRequest, State, Store, Timestamp};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

mod edit;
mod edit_file;
mod merge;

pub(crate) use merge::{MergeDialog, MergeEvent, MergedPullRequest, Merging, record_merge};
mod prompt;
mod threads;

use super::{code, labels};
use crate::theme::{self, color};
use crate::undo::{self, Discard, Recorded, Recorder};

actions!(review, [Close, Newer, Older]);

/// Key bindings of the review.
pub fn key_bindings() -> Vec<KeyBinding> {
    const CONTEXT: Option<&str> = Some("Review");
    vec![
        KeyBinding::new("escape", Close, CONTEXT),
        KeyBinding::new("[", Newer, CONTEXT),
        KeyBinding::new("]", Older, CONTEXT),
    ]
}

/// What the review asks of the window.
pub enum ReviewEvent {
    /// Go back to the history.
    Close,
    /// Review the next newer commit.
    Newer,
    /// Review the next older commit.
    Older,
    /// The index changed: read the status again.
    Staged,
    /// The file at this path is now at the top of the diff of a commit or
    /// of the uncommitted changes.
    FileShown(String),
    /// The local pull request was saved.
    PullRequestSaved,
    /// Another branch was checked out.
    CheckedOut,
    /// A merge stopped with conflicts to resolve.
    MergeStopped,
    /// Uncommitted changes were thrown away; `step` is the undo step.
    Discarded {
        message: SharedString,
        step: Option<u64>,
    },
    /// The local pull request was merged, which moved the branches and
    /// may have checked out another; `step` is the undo step.
    Merged {
        message: SharedString,
        step: Option<u64>,
    },
    /// A file of the head branch was edited, and committed when
    /// `committed`; `step` is the undo step of the commit.
    FileEdited { committed: bool, step: Option<u64> },
}

/// Height of one diff line.
const LINE_HEIGHT: Pixels = px(21.);
/// Space between file cards.
const CARD_GAP: Pixels = px(18.);
/// Rows rendered beyond the visible ones, so fast scrolling stays smooth.
const OVERDRAW: Pixels = px(600.);

/// A row of the diff list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// The header of the file at this index.
    File(usize),
    /// A hunk header: file and hunk index.
    Hunk(usize, usize),
    /// A review thread under its lines: file and thread id.
    Thread(usize, u32),
    /// The box a new comment is written in: file.
    Composer(usize),
    /// A diff line: file, hunk and line index.
    Line(usize, usize, usize),
    /// In the split layout, a line's old side beside a line's new side:
    /// file, hunk, and the indices of the two lines, if any.
    Pair(usize, usize, Option<usize>, Option<usize>),
    /// Unchanged lines left out of a file's diff, which can be shown: file
    /// and where.
    Gap(usize, GapAt),
    /// One of those lines, shown: file, where, and the index in the gap.
    Context(usize, GapAt, u32),
    /// Why a file has no lines to show: binary, too large, unchanged.
    Notice(usize),
    /// The bottom edge of a file's card.
    FileEnd(usize),
    /// After the last file.
    End,
}

impl Row {
    fn file(self) -> Option<usize> {
        match self {
            Row::File(file)
            | Row::Hunk(file, _)
            | Row::Line(file, _, _)
            | Row::Pair(file, _, _, _)
            | Row::Gap(file, _)
            | Row::Context(file, _, _)
            | Row::Notice(file)
            | Row::Thread(file, _)
            | Row::Composer(file)
            | Row::FileEnd(file) => Some(file),
            Row::End => None,
        }
    }

    /// The same row in the file at index `file`.
    fn in_file(self, file: usize) -> Self {
        match self {
            Row::File(_) => Row::File(file),
            Row::Hunk(_, hunk) => Row::Hunk(file, hunk),
            Row::Line(_, hunk, line) => Row::Line(file, hunk, line),
            Row::Pair(_, hunk, old, new) => Row::Pair(file, hunk, old, new),
            Row::Notice(_) => Row::Notice(file),
            Row::Thread(_, id) => Row::Thread(file, id),
            Row::Composer(_) => Row::Composer(file),
            Row::FileEnd(_) => Row::FileEnd(file),
            Row::Gap(_, at) => Row::Gap(file, at),
            Row::Context(_, at, line) => Row::Context(file, at, line),
            Row::End => Row::End,
        }
    }
}

/// Where unchanged lines are left out of a file's diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum GapAt {
    /// Before the hunk at this index.
    Before(usize),
    /// After the hunk at this index, to the end of the file.
    After(usize),
}

/// Unchanged lines left out of a diff: `len` lines from line `new` of the
/// new file, which is line `old` of the old file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Gap {
    old: u32,
    new: u32,
    len: u32,
}

/// The lines left out at `at` in `hunks`, if any. Hunks from different
/// files, as staged and unstaged hunks are, have no gap between them.
fn gap(hunks: &[Hunk], at: GapAt) -> Option<Gap> {
    // The first line of a range; an empty one starts after the line before.
    let first = |start: u32, len: u32| if len == 0 { start + 1 } else { start };
    let after = |hunk: &Hunk| {
        (
            first(hunk.old_start, hunk.old_len) + hunk.old_len,
            first(hunk.new_start, hunk.new_len) + hunk.new_len,
        )
    };
    let same_file = |a: &Hunk, b: &Hunk| a.staging == b.staging && a.new_side == b.new_side;
    let gap = match at {
        GapAt::Before(index) => {
            let hunk = hunks.get(index)?;
            hunk.new_side.as_ref()?;
            let (old, new) = match index.checked_sub(1).and_then(|index| hunks.get(index)) {
                Some(previous) if same_file(previous, hunk) => after(previous),
                _ => (1, 1),
            };
            let end = first(hunk.new_start, hunk.new_len);
            Gap {
                old,
                new,
                len: end.saturating_sub(new),
            }
        }
        GapAt::After(index) => {
            let hunk = hunks.get(index)?;
            let side = hunk.new_side.as_ref()?;
            if hunks
                .get(index + 1)
                .is_some_and(|next| same_file(hunk, next))
            {
                // Shown as the gap before the next hunk.
                return None;
            }
            let (old, new) = after(hunk);
            Gap {
                old,
                new,
                len: (side.lines + 1).saturating_sub(new),
            }
        }
    };
    (gap.len > 0).then_some(gap)
}

/// How reviews show diffs. One setting for the whole app, so it carries
/// over to the next review.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ReviewSettings {
    layout: DiffLayout,
    hide_whitespace: bool,
}

impl Global for ReviewSettings {}

impl ReviewSettings {
    fn get(cx: &App) -> Self {
        cx.try_global::<Self>().copied().unwrap_or_default()
    }

    fn diff_options(self) -> DiffOptions {
        DiffOptions {
            ignore_whitespace: self.hide_whitespace,
        }
    }
}

/// How diffs are laid out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DiffLayout {
    /// Removed lines above the added lines that replace them.
    #[default]
    Unified,
    /// The old file on the left, the new file on the right.
    Split,
}

/// Where the list is scrolled to, kept across changes to its rows.
struct Anchor {
    /// The file at the top of the list.
    path: String,
    /// The row at the top.
    row: Row,
    /// The index of that row within the file's rows.
    index: usize,
    offset_in_item: Pixels,
}

/// What is being reviewed.
enum Subject {
    Commit(Box<CommitSubject>),
    PullRequest(Box<PullRequestSubject>),
    WorkingTree {
        status: Arc<WorkingTreeStatus>,
        /// The branch `HEAD` is on, or the detached `HEAD`.
        head: Option<RefLabel>,
        git: Git,
        /// The `git` command running. Only one runs at a time.
        running: Option<Task<()>>,
    },
}

struct CommitSubject {
    commit: Commit,
    labels: Vec<RefLabel>,
    /// The commits before and after this one in the history, if any.
    newer: Option<Commit>,
    older: Option<Commit>,
}

/// The commits a local pull request's branches are on now; `None` for a
/// branch that no longer exists, which is then taken as it was last seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tips {
    pub head: Option<Oid>,
    pub base: Option<Oid>,
    /// Whether the head branch is checked out, so an agent's commits land
    /// on it.
    pub checked_out: bool,
}

struct PullRequestSubject {
    store: Store,
    /// As last read or changed here.
    pull_request: PullRequest,
    tips: Tips,
    /// How many commits the head has that the base lacks, once read.
    ahead: Option<usize>,
    /// Saving the pull request; each save waits for the one before.
    saving: Task<()>,
    /// Why the last save failed.
    save_error: Option<SharedString>,
    comments: threads::Comments,
    panel: prompt::PromptPanel,
    /// Checking out the head branch.
    checking_out: Option<Task<()>>,
    /// Set while the merge dialog is open.
    merge_dialog: Option<Subscription>,
    /// Set while the edit dialog is open.
    edit_dialog: Option<Subscription>,
    /// Set while the dialog editing a file of the head branch is open.
    file_edit_dialog: Option<Subscription>,
}

pub struct Review {
    focus_handle: FocusHandle,
    repository: Repository,
    subject: Subject,
    /// `None` while reading.
    files: Option<Result<Arc<Vec<FileDiff>>, SharedString>>,
    rows: Vec<Row>,
    /// Index in `rows` of each file's header.
    file_rows: Vec<usize>,
    list: ListState,
    /// The file at the top of the list.
    current_file: usize,
    /// The files of a local pull request marked viewed, by path, with the
    /// id of their new version then (empty when deleted): a file that
    /// changes since counts as not viewed again.
    viewed: HashMap<String, String>,
    /// The gaps shown in full, by file path, with the [`fingerprint`] of
    /// the file's diff then.
    expanded: HashMap<String, (u64, HashSet<GapAt>)>,
    /// The lines of the files read to fill gaps.
    sources: HashMap<LineSource, Arc<Vec<String>>>,
    /// Why reading the lines of a file failed.
    sources_error: Option<SharedString>,
    /// Syntax highlighting of the versions of files shown, by where each
    /// is stored and then by the path it is shown at, which picks the
    /// language; `None` for a version that stays plain. Filled in the
    /// background, the files nearest the one shown first.
    highlights: HashMap<LineSource, HashMap<String, Option<Arc<Highlights>>>>,
    /// The file to scroll to once the diff is read.
    pending_file: Option<String>,
    _load: Task<()>,
    _load_sources: Task<()>,
    _highlight: Task<()>,
}

/// Which version of a file a hunk's lines are from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Version {
    /// The removed lines'.
    Old,
    /// The added and unchanged lines'.
    New,
}

/// A version of a file to highlight.
struct HighlightJob {
    /// The index of the file in the diff.
    file: usize,
    /// Its path in that version, which picks the language.
    path: String,
    source: LineSource,
}

impl EventEmitter<ReviewEvent> for Review {}

impl Review {
    /// Reviews `commit` of `repository`, reading its diff in the background.
    pub fn commit(
        repository: &Repository,
        commit: Commit,
        labels: Vec<RefLabel>,
        (newer, older): (Option<Commit>, Option<Commit>),
        cx: &mut Context<Self>,
    ) -> Self {
        let load = Self::load_commit(repository, commit.id, cx);
        let subject = Subject::Commit(Box::new(CommitSubject {
            commit,
            labels,
            newer,
            older,
        }));
        Self::with_subject(repository, subject, load, cx)
    }

    /// Reads the diff of commit `id` in the background.
    fn load_commit(repository: &Repository, id: Oid, cx: &mut Context<Self>) -> Task<()> {
        let repository = repository.clone();
        let options = ReviewSettings::get(cx).diff_options();
        cx.spawn(async move |this, cx| {
            let files = cx
                .background_executor()
                .spawn(async move { repository.commit_diff(id, options) })
                .await;
            this.update(cx, |this, cx| this.finish(files, cx)).ok();
        })
    }

    /// Reviews the uncommitted changes of `repository`, which has `status`,
    /// reading their diff in the background. `head` is where `HEAD` is.
    pub fn working_tree(
        repository: &Repository,
        status: Arc<WorkingTreeStatus>,
        head: Option<RefLabel>,
        cx: &mut Context<Self>,
    ) -> Self {
        let load = Self::load_working_tree(repository, cx);
        let subject = Subject::WorkingTree {
            status,
            head,
            // A bare repository has no uncommitted changes to review.
            git: Git::new(repository.work_dir().unwrap_or(repository.git_dir())),
            running: None,
        };
        Self::with_subject(repository, subject, load, cx)
    }

    /// Whether this reviews the uncommitted changes.
    pub fn is_working_tree(&self) -> bool {
        matches!(self.subject, Subject::WorkingTree { .. })
    }

    /// The commit reviewed, if one is.
    pub fn commit_id(&self) -> Option<Oid> {
        match &self.subject {
            Subject::Commit(subject) => Some(subject.commit.id),
            _ => None,
        }
    }

    /// Reviews `pull_request`, stored in `store`, with its branches on
    /// `tips`. It is read again from `store` with its diff in the
    /// background, and its threads follow the head if it moved.
    pub fn pull_request(
        repository: &Repository,
        store: Store,
        pull_request: PullRequest,
        tips: Tips,
        user_name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let load = Self::load_pull_request(repository, &store, pull_request.number, tips, cx);
        let panel = prompt::PromptPanel::new(repository, &pull_request.prompt, window, cx);
        let subject = Subject::PullRequest(Box::new(PullRequestSubject {
            store,
            pull_request,
            tips,
            ahead: None,
            saving: Task::ready(()),
            save_error: None,
            comments: threads::Comments {
                user_name,
                ..Default::default()
            },
            panel,
            checking_out: None,
            merge_dialog: None,
            edit_dialog: None,
            file_edit_dialog: None,
        }));
        let mut review = Self::with_subject(repository, subject, load, cx);
        review.find_run(cx);
        review
    }

    /// Reads pull request `number` and its diff in the background.
    fn load_pull_request(
        repository: &Repository,
        store: &Store,
        number: u32,
        tips: Tips,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let (repository, store) = (repository.clone(), store.clone());
        let options = ReviewSettings::get(cx).diff_options();
        cx.spawn(async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move { read_pull_request(&repository, &store, number, tips, options) })
                .await;
            this.update(cx, |this, cx| match read {
                Ok((pull_request, diff)) => this.finish_pull_request(pull_request, diff, cx),
                Err(error) => {
                    this.files = Some(Err(format!(
                        "Could not read local pull request #{number}: {error:#}"
                    )
                    .into()));
                    cx.notify();
                }
            })
            .ok();
        })
    }

    fn finish_pull_request(
        &mut self,
        pull_request: PullRequest,
        diff: BranchDiff,
        cx: &mut Context<Self>,
    ) {
        let Subject::PullRequest(subject) = &mut self.subject else {
            return;
        };
        self.viewed = pull_request
            .viewed
            .iter()
            .map(|(path, key)| (path.clone(), key.clone()))
            .collect();
        subject.pull_request = pull_request;
        subject.ahead = Some(diff.ahead);
        self.finish(Ok(diff.files), cx);
        // The viewed files may differ while the diff does not.
        let anchor = self.scroll_anchor();
        self.rebuild_rows(anchor, cx);
    }

    /// Shows the pull request's branches on `tips` after the refs changed.
    /// Does nothing for other reviews.
    pub fn set_tips(&mut self, tips: Tips, cx: &mut Context<Self>) {
        let Subject::PullRequest(subject) = &mut self.subject else {
            return;
        };
        if subject.tips == tips {
            return;
        }
        subject.tips = tips;
        let number = subject.pull_request.number;
        let store = subject.store.clone();
        self._load = Self::load_pull_request(&self.repository, &store, number, tips, cx);
    }

    /// Checks out the pull request's head branch, so an agent works on it.
    fn checkout_head(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.repository.work_dir() else {
            return;
        };
        let Subject::PullRequest(subject) = &mut self.subject else {
            return;
        };
        if subject.checking_out.is_some() {
            return;
        }
        let git = Git::new(dir);
        let branch = subject.pull_request.head.branch.clone();
        subject.checking_out = Some(cx.spawn_in(window, async move |this, cx| {
            let label = format!("Check out {branch}");
            let switched = cx
                .background_executor()
                .spawn(async move {
                    let recorder = Recorder::start(git.work_dir());
                    git.switch(&branch)
                        .map(|()| recorder.finish(ResetMode::Keep))
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                if let Subject::PullRequest(subject) = &mut this.subject {
                    subject.checking_out = None;
                }
                match switched {
                    Ok(recorded) => {
                        undo::record(cx, recorded, label);
                        cx.emit(ReviewEvent::CheckedOut);
                    }
                    Err(error) => window.push_notification(
                        Notification::error(format!("{:#}", anyhow::Error::from(error)))
                            .title("Could not check out the branch"),
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The number of the local pull request reviewed, if one is.
    pub fn pull_request_number(&self) -> Option<u32> {
        match &self.subject {
            Subject::PullRequest(subject) => Some(subject.pull_request.number),
            _ => None,
        }
    }

    /// Saves the pull request in the background, after any save still
    /// running.
    fn save_pull_request(&mut self, cx: &mut Context<Self>) {
        let Subject::PullRequest(subject) = &mut self.subject else {
            return;
        };
        let previous = std::mem::replace(&mut subject.saving, Task::ready(()));
        let (store, pull_request) = (subject.store.clone(), subject.pull_request.clone());
        subject.saving = cx.spawn(async move |this, cx| {
            previous.await;
            let saved = cx
                .background_executor()
                .spawn(async move { store.save(&pull_request) })
                .await;
            this.update(cx, |this, cx| {
                if saved.is_ok() {
                    cx.emit(ReviewEvent::PullRequestSaved);
                }
                if let Subject::PullRequest(subject) = &mut this.subject {
                    subject.save_error = saved.err().map(|error| {
                        format!(
                            "Could not save the review: {:#}",
                            anyhow::Error::from(error)
                        )
                        .into()
                    });
                    cx.notify();
                }
            })
            .ok();
        });
    }

    fn with_subject(
        repository: &Repository,
        subject: Subject,
        load: Task<()>,
        cx: &mut Context<Self>,
    ) -> Self {
        let list = ListState::new(0, ListAlignment::Top, OVERDRAW);
        let this = cx.entity().downgrade();
        list.set_scroll_handler(move |event, _, cx| {
            let first = event.visible_range.start;
            this.update(cx, |this, cx| this.scrolled_to(first, cx)).ok();
        });
        Self {
            focus_handle: cx.focus_handle(),
            repository: repository.clone(),
            subject,
            files: None,
            rows: Vec::new(),
            file_rows: Vec::new(),
            list,
            current_file: 0,
            viewed: HashMap::new(),
            expanded: HashMap::new(),
            sources: HashMap::new(),
            sources_error: None,
            highlights: HashMap::new(),
            pending_file: None,
            _load: load,
            _load_sources: Task::ready(()),
            _highlight: Task::ready(()),
        }
    }

    fn load_working_tree(repository: &Repository, cx: &mut Context<Self>) -> Task<()> {
        let repository = repository.clone();
        let options = ReviewSettings::get(cx).diff_options();
        cx.spawn(async move |this, cx| {
            let files = cx
                .background_executor()
                .spawn(async move { repository.working_tree_diff(options) })
                .await;
            this.update(cx, |this, cx| this.finish(files, cx)).ok();
        })
    }

    /// Updates where `HEAD` is after the refs changed.
    pub fn set_head(&mut self, head: Option<RefLabel>, cx: &mut Context<Self>) {
        if let Subject::WorkingTree { head: shown, .. } = &mut self.subject {
            *shown = head;
            cx.notify();
        }
    }

    /// Shows the uncommitted changes again after they changed to `status`,
    /// keeping the scroll position. Does nothing when reviewing a commit.
    pub fn refresh_working_tree(
        &mut self,
        repository: &Repository,
        status: Arc<WorkingTreeStatus>,
        cx: &mut Context<Self>,
    ) {
        let Subject::WorkingTree { status: shown, .. } = &mut self.subject else {
            return;
        };
        *shown = status;
        self._load = Self::load_working_tree(repository, cx);
        cx.notify();
    }

    fn finish(&mut self, files: gitez_git::Result<Vec<FileDiff>>, cx: &mut Context<Self>) {
        match files {
            Ok(files) => {
                if matches!(&self.files, Some(Ok(shown)) if **shown == files) {
                    return;
                }
                let anchor = self.scroll_anchor();
                self.files = Some(Ok(Arc::new(files)));
                // Files on disk may have changed; blobs cannot.
                self.sources
                    .retain(|source, _| matches!(source, LineSource::Blob(_)));
                self.rebuild_rows(anchor, cx);
                self.load_sources(cx);
                self.load_highlights(cx);
                if let Some(path) = self.pending_file.take() {
                    self.show_file(path, cx);
                }
            }
            Err(error) => {
                self.files = Some(Err(format!(
                    "Could not read the changes: {:#}",
                    anyhow::Error::from(error)
                )
                .into()));
            }
        }
        cx.notify();
    }

    /// Lays out the rows of the files again, then scrolls back to `anchor`,
    /// from [`Self::scroll_anchor`] before the change.
    fn rebuild_rows(&mut self, anchor: Option<Anchor>, cx: &App) {
        let Some(Ok(files)) = &self.files else {
            return;
        };
        let files = files.clone();
        (self.rows, self.file_rows) = rows(
            &files,
            |file| self.is_viewed(file),
            |file, at| self.is_expanded(file, at),
            ReviewSettings::get(cx).layout,
        );
        let placed = self.placed_comments(&files);
        if !placed.is_empty() {
            (self.rows, self.file_rows) = threads::with_comments(
                &self.rows,
                &placed,
                |row| threads::row_numbers(&files, row),
                |file| self.is_viewed(&files[file]),
            );
        }
        self.list.reset(self.rows.len());
        if let Some(Anchor {
            path,
            row,
            index,
            offset_in_item,
        }) = anchor
            && let Some(file) = files.iter().position(|file| file.path == path)
        {
            // The same row of the same file; or the row at the same place
            // in the file, or its last row if it got shorter.
            let start = self.file_rows[file];
            let end = self
                .file_rows
                .get(file + 1)
                .copied()
                .unwrap_or(self.rows.len());
            let row = row.in_file(file);
            let item_ix = match self.rows[start..end].iter().position(|&other| other == row) {
                Some(found) => start + found,
                None => (start + index).min(end.saturating_sub(1)),
            };
            self.list.scroll_to(ListOffset {
                item_ix,
                offset_in_item,
            });
            self.current_file = file;
        } else {
            self.current_file = 0;
        }
    }

    /// Whether the lines left out at `at` in `file` are shown: asked for,
    /// read, and the file has not changed since.
    fn is_expanded(&self, file: &FileDiff, at: GapAt) -> bool {
        let DiffContent::Text(hunks) = &file.content else {
            return false;
        };
        let index = match at {
            GapAt::Before(index) | GapAt::After(index) => index,
        };
        let loaded = hunks
            .get(index)
            .and_then(|hunk| hunk.new_side.as_ref())
            .is_some_and(|side| self.sources.contains_key(&side.source));
        loaded
            && self
                .expanded
                .get(&file.path)
                .is_some_and(|(print, gaps)| *print == fingerprint(file) && gaps.contains(&at))
    }

    /// Every gap in `files`, by file index.
    fn all_gaps(files: &[FileDiff]) -> impl Iterator<Item = (usize, GapAt)> + '_ {
        files.iter().enumerate().flat_map(|(file, diff)| {
            let hunks: &[Hunk] = match &diff.content {
                DiffContent::Text(hunks) => hunks,
                _ => &[],
            };
            (0..hunks.len())
                .flat_map(|index| [GapAt::Before(index), GapAt::After(index)])
                .filter(move |&at| gap(hunks, at).is_some())
                .map(move |at| (file, at))
        })
    }

    /// Shows the lines left out at each of `gaps`, reading the files they
    /// are from first.
    fn expand(&mut self, gaps: Vec<(usize, GapAt)>, cx: &mut Context<Self>) {
        let Some(Ok(files)) = &self.files else {
            return;
        };
        for (file, at) in gaps {
            let Some(file) = files.get(file) else {
                continue;
            };
            let print = fingerprint(file);
            let (shown_print, shown) = self
                .expanded
                .entry(file.path.clone())
                .or_insert_with(|| (print, HashSet::new()));
            if *shown_print != print {
                *shown_print = print;
                shown.clear();
            }
            shown.insert(at);
        }
        self.load_sources(cx);
        let anchor = self.scroll_anchor();
        self.rebuild_rows(anchor, cx);
        cx.notify();
    }

    /// Hides every line shown by [`Self::expand`].
    fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.expanded.clear();
        let anchor = self.scroll_anchor();
        self.rebuild_rows(anchor, cx);
        cx.notify();
    }

    /// Reads, in the background, the files that expanded gaps need and
    /// are not read yet, then shows their lines.
    fn load_sources(&mut self, cx: &mut Context<Self>) {
        let Some(Ok(files)) = &self.files else {
            return;
        };
        let mut needed = HashSet::new();
        for file in files.iter() {
            let (Some((print, gaps)), DiffContent::Text(hunks)) =
                (self.expanded.get(&file.path), &file.content)
            else {
                continue;
            };
            if *print != fingerprint(file) {
                continue;
            }
            for at in gaps {
                let (GapAt::Before(index) | GapAt::After(index)) = *at;
                if let Some(side) = hunks.get(index).and_then(|hunk| hunk.new_side.as_ref())
                    && !self.sources.contains_key(&side.source)
                {
                    needed.insert(side.source.clone());
                }
            }
        }
        if needed.is_empty() {
            return;
        }
        let repository = self.repository.clone();
        self._load_sources = cx.spawn(async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move {
                    needed
                        .into_iter()
                        .map(|source| {
                            let lines = repository.lines(&source);
                            (source, lines)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |this, cx| {
                this.sources_error = None;
                for (source, lines) in read {
                    match lines {
                        Ok(lines) => {
                            this.sources.insert(source, Arc::new(lines));
                        }
                        Err(error) => {
                            this.sources_error = Some(
                                format!(
                                    "Could not read the unchanged lines: {:#}",
                                    anyhow::Error::from(error)
                                )
                                .into(),
                            );
                        }
                    }
                }
                let anchor = this.scroll_anchor();
                this.rebuild_rows(anchor, cx);
                cx.notify();
            })
            .ok();
        });
    }

    /// Highlights, in the background, the versions of the files shown that
    /// are not highlighted yet, then shows them coloured. Files in the
    /// working tree are highlighted again, since they may have changed;
    /// until then their old colours stay, on the lines still as long.
    fn load_highlights(&mut self, cx: &mut Context<Self>) {
        let Some(Ok(files)) = &self.files else {
            return;
        };
        let files = files.clone();
        let mut jobs = Vec::new();
        let mut shown = HashSet::new();
        for (index, file) in files.iter().enumerate() {
            let DiffContent::Text(hunks) = &file.content else {
                continue;
            };
            for hunk in hunks {
                let sides = [
                    (hunk.old_side.as_ref(), old_path(file)),
                    (
                        hunk.new_side.as_ref().map(|side| &side.source),
                        file.path.as_str(),
                    ),
                ];
                for (source, path) in sides {
                    let Some(source) = source else {
                        continue;
                    };
                    let stale = matches!(source, LineSource::WorkingTree(_))
                        || !self
                            .highlights
                            .get(source)
                            .is_some_and(|paths| paths.contains_key(path));
                    if shown.insert((source.clone(), path.to_owned())) && stale {
                        jobs.push(HighlightJob {
                            file: index,
                            path: path.to_owned(),
                            source: source.clone(),
                        });
                    }
                }
            }
        }
        // Files no longer in the diff; blobs cannot change, so they stay
        // in case they come back.
        self.highlights.retain(|source, paths| {
            if !matches!(source, LineSource::Blob(_)) {
                paths.retain(|path, _| shown.contains(&(source.clone(), path.clone())));
            }
            !paths.is_empty()
        });
        if jobs.is_empty() {
            // A run still going is for files no longer shown.
            self._highlight = Task::ready(());
            return;
        }
        let repository = self.repository.clone();
        self._highlight = cx.spawn(async move |this, cx| {
            let parallel = std::thread::available_parallelism()
                .map_or(1, |threads| threads.get())
                .min(4);
            while !jobs.is_empty() {
                let Ok(current) = this.update(cx, |this, _| this.current_file) else {
                    return;
                };
                // The nearest files last, to be taken first.
                jobs.sort_by_key(|job| std::cmp::Reverse(job.file.abs_diff(current)));
                let batch = jobs.split_off(jobs.len().saturating_sub(parallel));
                let tasks: Vec<_> = batch
                    .into_iter()
                    .map(|job| {
                        let (repository, files) = (repository.clone(), files.clone());
                        cx.background_executor().spawn(async move {
                            let highlights = highlight(&repository, &files, &job);
                            (job.source, job.path, highlights)
                        })
                    })
                    .collect();
                let done = futures::future::join_all(tasks).await;
                let updated = this.update(cx, |this, cx| {
                    for (source, path, highlights) in done {
                        this.highlights
                            .entry(source)
                            .or_default()
                            .insert(path, highlights);
                    }
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
            }
        });
    }

    /// The syntax spans of `line` of `hunk` of `file`: a removed line's
    /// from the old file, the others' from the new one.
    fn line_spans(&self, file: &FileDiff, hunk: &Hunk, line: &DiffLine) -> Option<&[Span]> {
        let (source, number, version) = line_source(hunk, line)?;
        let path = match version {
            Version::Old => old_path(file),
            Version::New => &file.path,
        };
        self.source_spans(source, path, number, &line.text)
    }

    /// The syntax spans of line `number` of the file at `source`, shown at
    /// `path`, which reads `text`.
    fn source_spans(
        &self,
        source: &LineSource,
        path: &str,
        number: u32,
        text: &str,
    ) -> Option<&[Span]> {
        self.highlights
            .get(source)?
            .get(path)?
            .as_ref()?
            .line(number, text)
    }

    /// Whether this reviews a local pull request, whose files can be
    /// marked viewed.
    /// The button that edits `file` of a local pull request, for a file
    /// still there on its head branch. It needs the branch checked out.
    fn edit_file_button(&self, file: &FileDiff, cx: &mut Context<Self>) -> Option<Button> {
        let Subject::PullRequest(subject) = &self.subject else {
            return None;
        };
        if matches!(file.kind, ChangeKind::Deleted) || self.repository.work_dir().is_none() {
            return None;
        }
        let checked_out = subject.tips.checked_out;
        let path = file.path.clone();
        let tooltip: SharedString = if checked_out {
            "Edit this file on the branch".into()
        } else {
            format!(
                "Check out {} to edit its files",
                subject.pull_request.head.branch
            )
            .into()
        };
        Some(
            Button::new(SharedString::from(format!("edit-file-{path}")))
                .ghost()
                .xsmall()
                .icon(IconName::Pencil)
                .label("Edit")
                .tooltip(tooltip)
                .disabled(!checked_out)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_file_edit(path.clone(), window, cx)
                })),
        )
    }

    fn is_pull_request(&self) -> bool {
        matches!(self.subject, Subject::PullRequest(_))
    }

    /// Whether `file` of the local pull request is marked viewed and has
    /// not changed since.
    fn is_viewed(&self, file: &FileDiff) -> bool {
        self.viewed.get(&file.path) == Some(&view_key(file))
    }

    /// Marks the file at `index` of the local pull request viewed, which
    /// collapses it, or not viewed.
    fn set_viewed(&mut self, index: usize, viewed: bool, cx: &mut Context<Self>) {
        let Some(Ok(files)) = &self.files else {
            return;
        };
        let Some(file) = files.get(index) else {
            return;
        };
        let (file_path, key) = (file.path.clone(), view_key(file));
        let mut anchor = self.scroll_anchor();
        let Subject::PullRequest(subject) = &mut self.subject else {
            return;
        };
        let saved = &mut subject.pull_request.viewed;
        if viewed {
            saved.insert(file_path.clone(), key.clone());
            self.viewed.insert(file_path.clone(), key);
        } else {
            saved.remove(&file_path);
            self.viewed.remove(&file_path);
        }
        self.save_pull_request(cx);
        if let Some(anchor) = &mut anchor
            && anchor.path == file_path
        {
            // Scrolled into the file being collapsed: keep its header in
            // view, with the next file below it.
            anchor.row = Row::File(index);
            anchor.index = 0;
            anchor.offset_in_item = px(0.);
        }
        self.rebuild_rows(anchor, cx);
        cx.notify();
    }

    /// Lays out the diffs as `layout`, here and in later reviews.
    fn set_layout(&mut self, layout: DiffLayout, cx: &mut Context<Self>) {
        let settings = ReviewSettings::get(cx);
        if settings.layout == layout {
            return;
        }
        cx.set_global(ReviewSettings { layout, ..settings });
        let anchor = self.scroll_anchor();
        self.rebuild_rows(anchor, cx);
        cx.notify();
    }

    /// Compares lines without their whitespace, or with it, here and in
    /// later reviews.
    fn set_hide_whitespace(&mut self, hide_whitespace: bool, cx: &mut Context<Self>) {
        let settings = ReviewSettings::get(cx);
        if settings.hide_whitespace == hide_whitespace {
            return;
        }
        cx.set_global(ReviewSettings {
            hide_whitespace,
            ..settings
        });
        self._load = match &self.subject {
            Subject::Commit(subject) => Self::load_commit(&self.repository, subject.commit.id, cx),
            Subject::WorkingTree { .. } => Self::load_working_tree(&self.repository, cx),
            Subject::PullRequest(subject) => Self::load_pull_request(
                &self.repository,
                &subject.store,
                subject.pull_request.number,
                subject.tips,
                cx,
            ),
        };
        cx.notify();
    }

    /// The row at the top of the list.
    fn scroll_anchor(&self) -> Option<Anchor> {
        let Some(Ok(files)) = &self.files else {
            return None;
        };
        let top = self.list.logical_scroll_top();
        let row = *self.rows.get(top.item_ix)?;
        let file = row.file()?;
        Some(Anchor {
            path: files[file].path.clone(),
            row,
            index: top.item_ix - self.file_rows[file],
            offset_in_item: top.offset_in_item,
        })
    }

    fn scrolled_to(&mut self, first_row: usize, cx: &mut Context<Self>) {
        let file = self
            .rows
            .get(first_row)
            .and_then(|row| row.file())
            .unwrap_or(self.file_rows.len().saturating_sub(1));
        if file != self.current_file {
            self.set_current_file(file, cx);
        }
    }

    /// Makes the file at `file` the one at the top, and tells the panel
    /// beside that lists the files, if any.
    fn set_current_file(&mut self, file: usize, cx: &mut Context<Self>) {
        self.current_file = file;
        if !self.is_pull_request()
            && let Some(Ok(files)) = &self.files
            && let Some(shown) = files.get(file)
        {
            cx.emit(ReviewEvent::FileShown(shown.path.clone()));
        }
        cx.notify();
    }

    /// Scrolls to the file at `path`, now or once the diff is read.
    pub fn show_file(&mut self, path: String, cx: &mut Context<Self>) {
        match &self.files {
            Some(Ok(files)) => {
                if let Some(index) = files.iter().position(|file| file.path == path) {
                    self.scroll_to_file(index, cx);
                }
            }
            Some(Err(_)) => {}
            None => self.pending_file = Some(path),
        }
    }

    fn scroll_to_file(&mut self, file: usize, cx: &mut Context<Self>) {
        if let Some(&row) = self.file_rows.get(file) {
            self.list.scroll_to(ListOffset {
                item_ix: row,
                offset_in_item: px(0.),
            });
            self.set_current_file(file, cx);
        }
    }

    fn close(&mut self, _: &Close, _: &mut Window, cx: &mut Context<Self>) {
        if !self.cancel_writing(cx) {
            cx.emit(ReviewEvent::Close);
        }
    }

    fn newer(&mut self, _: &Newer, window: &mut Window, cx: &mut Context<Self>) {
        // Typed into a text box.
        if !self.focus_handle.is_focused(window) {
            cx.propagate();
            return;
        }
        if let Subject::Commit(subject) = &self.subject
            && subject.newer.is_some()
        {
            cx.emit(ReviewEvent::Newer);
        }
    }

    fn older(&mut self, _: &Older, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_handle.is_focused(window) {
            cx.propagate();
            return;
        }
        if let Subject::Commit(subject) = &self.subject
            && subject.older.is_some()
        {
            cx.emit(ReviewEvent::Older);
        }
    }

    /// Runs `command` with `git` in the background unless another command is
    /// running, then `done` if it succeeded.
    fn run_git<T: Send + 'static>(
        &mut self,
        command: impl FnOnce(&Git) -> gitez_command::Result<T> + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Subject::WorkingTree { git, running, .. } = &mut self.subject else {
            return;
        };
        if running.is_some() {
            return;
        }
        let git = git.clone();
        *running = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { command(&git) })
                .await;
            this.update_in(cx, |this, window, cx| {
                if let Subject::WorkingTree { running, .. } = &mut this.subject {
                    *running = None;
                }
                match result {
                    Ok(value) => done(this, value, window, cx),
                    Err(error) => window.push_notification(
                        Notification::error(format!("{:#}", anyhow::Error::from(error))),
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Stages the changes to the uncommitted files at `paths`, or to every
    /// file when `None`.
    fn stage(&mut self, paths: Option<Vec<String>>, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git(
            move |git| match &paths {
                Some(paths) => git.stage(paths),
                None => git.stage_all(),
            },
            |_, (), _, cx| cx.emit(ReviewEvent::Staged),
            window,
            cx,
        );
    }

    /// Unstages the files at `paths`, or every file when `None`.
    fn unstage(&mut self, paths: Option<Vec<String>>, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git(
            move |git| match &paths {
                Some(paths) => git.unstage(paths),
                None => git.unstage_all(),
            },
            |_, (), _, cx| cx.emit(ReviewEvent::Staged),
            window,
            cx,
        );
    }

    /// Throws away the uncommitted changes to the files at `paths`, or to
    /// every file when `None`, as one undo step.
    fn discard(&mut self, paths: Option<Vec<String>>, window: &mut Window, cx: &mut Context<Self>) {
        let (label, message) = match paths.as_deref() {
            Some([path, ..]) => (
                format!("Discard the changes to {path}"),
                format!("Discarded the changes to {path}"),
            ),
            _ => (
                "Discard all changes".to_owned(),
                "Discarded every uncommitted change".to_owned(),
            ),
        };
        self.run_git(
            move |git| {
                let saved = git.discard(paths.as_deref())?;
                Ok(saved.and_then(|saved| {
                    Recorded::discard(git.work_dir(), Discard::Files { paths, saved })
                }))
            },
            move |_, recorded, _, cx| {
                let step = undo::record(cx, recorded, label);
                cx.emit(ReviewEvent::Discarded {
                    message: message.into(),
                    step,
                });
            },
            window,
            cx,
        );
    }

    /// "Stage" and "Unstage" for the file at `index` of the list, as far as
    /// they apply to it.
    fn staging_buttons(
        &self,
        index: usize,
        status: &FileStatus,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let running = self.is_running();
        let mut unstage_paths = vec![status.path.clone()];
        if let Some(ChangeKind::Renamed { from }) = &status.staged {
            // The rename's other half.
            unstage_paths.push(from.clone());
        }
        let stage_paths = vec![status.path.clone()];
        let discard_paths = unstage_paths.clone();
        div()
            .h_flex()
            .flex_none()
            .gap(px(4.))
            .child(
                Button::new(("discard-file", index))
                    .ghost()
                    .xsmall()
                    .label("Discard")
                    .tooltip("Throw away the changes to this file; Undo brings them back")
                    .disabled(running)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.discard(Some(discard_paths.clone()), window, cx)
                    })),
            )
            .when(status.staged.is_some(), |this| {
                this.child(
                    Button::new(("unstage-file", index))
                        .ghost()
                        .xsmall()
                        .label("Unstage")
                        .disabled(running)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.unstage(Some(unstage_paths.clone()), window, cx)
                        })),
                )
            })
            .when(status.unstaged.is_some(), |this| {
                this.child(
                    Button::new(("stage-file", index))
                        .outline()
                        .xsmall()
                        .label("Stage")
                        .disabled(running)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.stage(Some(stage_paths.clone()), window, cx)
                        })),
                )
            })
    }

    /// Whether a hunk is staged, and the button that stages or unstages it
    /// alone.
    fn hunk_staging(
        &self,
        file: usize,
        hunk: usize,
        staging: Staging,
        patchable: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let (label, id) = match staging {
            Staging::Staged => ("Unstage hunk", "unstage-hunk"),
            Staging::Unstaged => ("Stage hunk", "stage-hunk"),
        };
        div()
            .h_flex()
            .ml_auto()
            .pr(px(8.))
            .gap(px(8.))
            .font_family(cx.theme().font_family.clone())
            .when(staging == Staging::Staged, |this| {
                this.child(
                    div()
                        .text_size(theme::SMALL_TEXT_SIZE)
                        .text_color(rgb(color::ACCENT))
                        .child("staged"),
                )
            })
            .when(patchable && staging == Staging::Unstaged, |this| {
                this.child(
                    Button::new(("discard-hunk", file * 100_000 + hunk))
                        .ghost()
                        .xsmall()
                        .h(px(18.))
                        .label("Discard hunk")
                        .disabled(self.is_running())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.discard_hunk(file, hunk, window, cx)
                        })),
                )
            })
            .when(patchable, |this| {
                this.child(
                    Button::new((id, file * 100_000 + hunk))
                        .ghost()
                        .xsmall()
                        .h(px(18.))
                        .label(label)
                        .disabled(self.is_running())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.apply_hunk(file, hunk, window, cx)
                        })),
                )
            })
    }

    /// Stages an unstaged hunk alone, or unstages a staged one.
    fn apply_hunk(
        &mut self,
        file: usize,
        hunk: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Ok(files)) = &self.files else {
            return;
        };
        let Some(file) = files.get(file) else {
            return;
        };
        let DiffContent::Text(hunks) = &file.content else {
            return;
        };
        let Some(hunk) = hunks.get(hunk).filter(|hunk| hunk.patchable) else {
            return;
        };
        let Some(staging) = hunk.staging else {
            return;
        };
        let patch = hunk.patch(&file.path);
        let reverse = staging == Staging::Staged;
        self.run_git(
            move |git| git.apply_to_index(&patch, reverse),
            |_, (), _, cx| cx.emit(ReviewEvent::Staged),
            window,
            cx,
        );
    }

    /// Throws away an unstaged hunk alone, as one undo step.
    fn discard_hunk(
        &mut self,
        file: usize,
        hunk: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Ok(files)) = &self.files else {
            return;
        };
        let Some(file) = files.get(file) else {
            return;
        };
        let DiffContent::Text(hunks) = &file.content else {
            return;
        };
        let Some(hunk) = hunks
            .get(hunk)
            .filter(|hunk| hunk.patchable && hunk.staging == Some(Staging::Unstaged))
        else {
            return;
        };
        let patch = hunk.patch(&file.path);
        let path = file.path.clone();
        self.run_git(
            move |git| {
                git.apply_to_worktree(&patch, true)?;
                Ok(Recorded::discard(git.work_dir(), Discard::Hunk { patch }))
            },
            move |_, recorded, _, cx| {
                let step = undo::record(cx, recorded, format!("Discard a hunk of {path}"));
                cx.emit(ReviewEvent::Discarded {
                    message: format!("Discarded a hunk of {path}").into(),
                    step,
                });
            },
            window,
            cx,
        );
    }

    /// Whether a `git` command is running.
    fn is_running(&self) -> bool {
        matches!(&self.subject, Subject::WorkingTree { running, .. } if running.is_some())
    }

    /// How the uncommitted changes to `path` stand, when reviewing them.
    fn file_status(&self, path: &str) -> Option<&FileStatus> {
        let Subject::WorkingTree { status, .. } = &self.subject else {
            return None;
        };
        status.files.iter().find(|file| file.path == path)
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let element = self.render_plain_row(index, cx);
        match self.rows.get(index).copied() {
            Some(row @ (Row::Line(..) | Row::Pair(..) | Row::Context(..)))
                if self.is_pull_request() =>
            {
                self.commentable(element, index, row, cx)
            }
            _ => element,
        }
    }

    fn render_plain_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(Ok(files)) = &self.files else {
            return div().into_any_element();
        };
        let mono = cx.theme().mono_font_family.clone();
        match self.rows.get(index).copied() {
            Some(Row::File(index)) => {
                let file = &files[index];
                let status = self.file_status(&file.path);
                let viewed = self.is_viewed(file);
                let open = self
                    .open_threads_by_path()
                    .get(file.path.as_str())
                    .copied()
                    .unwrap_or(0);
                file_header(file, status, viewed, mono)
                    .when(open > 0, |this| {
                        this.child(threads::chip(match open {
                            1 => "1 comment".to_owned(),
                            open => format!("{open} comments"),
                        }))
                    })
                    .child(
                        div()
                            .h_flex()
                            .flex_none()
                            .ml_auto()
                            .gap(px(10.))
                            .when_some(status, |this, status| {
                                this.child(self.staging_buttons(index, status, cx))
                            })
                            .when_some(self.edit_file_button(file, cx), |this, button| {
                                this.child(button)
                            })
                            .when(self.is_pull_request(), |this| {
                                this.child(
                                    Checkbox::new(("viewed", index))
                                        .label("Viewed")
                                        .checked(viewed)
                                        .on_click(cx.listener(
                                            move |this, checked: &bool, _, cx| {
                                                this.set_viewed(index, *checked, cx)
                                            },
                                        )),
                                )
                            }),
                    )
                    .into_any_element()
            }
            Some(Row::Hunk(file, hunk)) => {
                let DiffContent::Text(hunks) = &files[file].content else {
                    return div().into_any_element();
                };
                let index = hunk;
                let hunk = &hunks[index];
                card_row()
                    .h(LINE_HEIGHT)
                    .bg(rgb(color::DIFF_HUNK))
                    .font_family(mono)
                    .text_size(theme::MONO_TEXT_SIZE)
                    .text_color(rgb(color::DIFF_HUNK_TEXT))
                    .child(div().w(px(116.)).flex_none())
                    .child(format!(
                        "@@ -{},{} +{},{} @@",
                        hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
                    ))
                    .when_some(hunk.staging, |this, staging| {
                        this.child(self.hunk_staging(file, index, staging, hunk.patchable, cx))
                    })
                    .into_any_element()
            }
            Some(Row::Line(file, hunk, line)) => {
                let DiffContent::Text(hunks) = &files[file].content else {
                    return div().into_any_element();
                };
                let (hunk, line) = (&hunks[hunk], &hunks[hunk].lines[line]);
                diff_line(line, self.line_spans(&files[file], hunk, line), mono).into_any_element()
            }
            Some(Row::Pair(file, hunk, old, new)) => {
                let DiffContent::Text(hunks) = &files[file].content else {
                    return div().into_any_element();
                };
                let hunk = &hunks[hunk];
                let side = |index: Option<usize>| {
                    let line = hunk.lines.get(index?)?;
                    Some((line, self.line_spans(&files[file], hunk, line)))
                };
                split_line(side(old), side(new), mono).into_any_element()
            }
            Some(Row::Gap(file, at)) => {
                let DiffContent::Text(hunks) = &files[file].content else {
                    return div().into_any_element();
                };
                let Some(gap) = gap(hunks, at) else {
                    return div().into_any_element();
                };
                let (index, label) = match (at, gap.len) {
                    (GapAt::Before(index), 1) => (index * 2, "Show 1 more line".to_owned()),
                    (GapAt::Before(index), len) => (index * 2, format!("Show {len} more lines")),
                    (GapAt::After(index), 1) => {
                        (index * 2 + 1, "Show 1 line to end of file".to_owned())
                    }
                    (GapAt::After(index), len) => {
                        (index * 2 + 1, format!("Show {len} lines to end of file"))
                    }
                };
                card_row()
                    .id(("gap", file * 100_000 + index))
                    .h(px(30.))
                    .justify_center()
                    .bg(rgb(color::DIFF_GAP))
                    .border_t_1()
                    .border_b_1()
                    .text_size(px(12.))
                    .text_color(rgb(color::TEXT_MUTED))
                    .cursor_pointer()
                    .hover(|style| style.text_color(rgb(color::TEXT)))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| this.expand(vec![(file, at)], cx)))
                    .into_any_element()
            }
            Some(Row::Context(file, at, line)) => {
                let DiffContent::Text(hunks) = &files[file].content else {
                    return div().into_any_element();
                };
                let (GapAt::Before(index) | GapAt::After(index)) = at;
                let (Some(gap), Some(side)) = (
                    gap(hunks, at),
                    hunks.get(index).and_then(|hunk| hunk.new_side.as_ref()),
                ) else {
                    return div().into_any_element();
                };
                let new = gap.new + line;
                let text = self
                    .sources
                    .get(&side.source)
                    .and_then(|lines| lines.get(new as usize - 1))
                    .cloned()
                    .unwrap_or_default();
                let spans = self.source_spans(&side.source, &files[file].path, new, &text);
                let line = DiffLine {
                    kind: LineKind::Context,
                    old: Some(gap.old + line),
                    new: Some(new),
                    text,
                    crlf: false,
                    no_newline: false,
                };
                match ReviewSettings::get(cx).layout {
                    DiffLayout::Unified => diff_line(&line, spans, mono).into_any_element(),
                    DiffLayout::Split => {
                        split_line(Some((&line, spans)), Some((&line, spans)), mono)
                            .into_any_element()
                    }
                }
            }
            Some(Row::Notice(file)) => card_row()
                .px(px(14.))
                .py(px(10.))
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_MUTED))
                .child(notice(&files[file]))
                .into_any_element(),
            Some(Row::FileEnd(file)) if self.is_viewed(&files[file]) => {
                div().h(CARD_GAP).into_any_element()
            }
            Some(Row::FileEnd(_)) => div()
                .v_flex()
                .w_full()
                .child(
                    div()
                        .h(px(4.))
                        .bg(rgb(color::DIFF_CARD))
                        .border_l_1()
                        .border_r_1()
                        .border_b_1()
                        .border_color(rgb(color::DIFF_CARD_BORDER))
                        .rounded_b(px(8.)),
                )
                .child(div().h(CARD_GAP))
                .into_any_element(),
            Some(Row::Thread(_, id)) => self.render_thread(id, cx),
            Some(Row::Composer(_)) => self.render_composer(cx),
            Some(Row::End) => self.render_end(cx).into_any_element(),
            None => div().into_any_element(),
        }
    }

    /// "End of commit · Next: <older commit>", or "End of changes".
    fn render_end(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (end, older) = match &self.subject {
            Subject::Commit(subject) => ("End of commit", subject.older.as_ref()),
            Subject::WorkingTree { .. } | Subject::PullRequest(_) => ("End of changes", None),
        };
        div()
            .h_flex()
            .w_full()
            .justify_center()
            .gap(px(8.))
            .p(px(18.))
            .mb(px(60.))
            .rounded(px(8.))
            .border_1()
            .border_dashed()
            .border_color(rgb(color::BUTTON_BORDER))
            .text_size(px(12.5))
            .text_color(rgb(color::TEXT_MUTED))
            .child(end)
            .when_some(older, |this, older| {
                this.child("·").child(
                    div()
                        .id("review-next")
                        .text_color(rgb(color::ACCENT))
                        .hover(|style| style.text_color(rgb(color::ACCENT_HOVER)))
                        .cursor_pointer()
                        .child(format!(
                            "Next: {} {}",
                            older.id.to_hex_with_len(7),
                            older.summary
                        ))
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(ReviewEvent::Older))),
                )
            })
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let totals = match &self.files {
            Some(Ok(files)) => Some(totals(files)),
            _ => None,
        };
        let header = div()
            .v_flex()
            .flex_none()
            .gap(px(14.))
            .pt(px(14.))
            .px(px(24.))
            .pb(px(16.))
            .bg(rgb(color::PANEL))
            .border_b_1()
            .border_color(rgb(color::BORDER));
        let back = Button::new("review-back")
            .ghost()
            .h(px(30.))
            .label("History")
            .tooltip("Back to the history (Esc)")
            .on_click(cx.listener(|_, _, _, cx| cx.emit(ReviewEvent::Close)));
        let breadcrumb = div().h_flex().gap(px(10.)).text_size(px(12.5)).child(back);
        let mono = cx.theme().mono_font_family.clone();

        match &self.subject {
            Subject::Commit(subject) => {
                let CommitSubject {
                    commit,
                    labels,
                    newer,
                    older,
                    ..
                } = &**subject;
                let parent = match commit.parents.as_slice() {
                    [] => "root commit".to_owned(),
                    [parent, ..] => format!("parent {}", parent.to_hex_with_len(7)),
                };
                header.child(
                    breadcrumb
                        .children(labels.first().map(crumb))
                        .child(muted("/"))
                        .child(
                            div()
                                .font_family(mono.clone())
                                .text_color(rgb(color::ACCENT))
                                .child(commit.id.to_hex_with_len(7).to_string()),
                        )
                        .child(
                            div()
                                .font_family(mono.clone())
                                .text_size(theme::SMALL_TEXT_SIZE)
                                .text_color(rgb(color::TEXT_MUTED))
                                .child(format!("({parent})")),
                        )
                        .when_some(totals, |this, totals| {
                            this.child(
                                div()
                                    .h_flex()
                                    .gap(px(10.))
                                    .text_color(rgb(color::TEXT_MUTED))
                                    .child("·")
                                    .children(total_parts(totals, mono.clone())),
                            )
                        })
                        .child(
                            div()
                                .h_flex()
                                .ml_auto()
                                .gap(px(6.))
                                .child(
                                    Button::new("review-newer")
                                        .ghost()
                                        .h(px(30.))
                                        .label("Newer")
                                        .tooltip("Review the next newer commit ([)")
                                        .disabled(newer.is_none())
                                        .on_click(
                                            cx.listener(|_, _, _, cx| cx.emit(ReviewEvent::Newer)),
                                        ),
                                )
                                .child(
                                    Button::new("review-older")
                                        .ghost()
                                        .h(px(30.))
                                        .label("Older")
                                        .tooltip("Review the next older commit (])")
                                        .disabled(older.is_none())
                                        .on_click(
                                            cx.listener(|_, _, _, cx| cx.emit(ReviewEvent::Older)),
                                        ),
                                ),
                        ),
                )
            }
            Subject::PullRequest(subject) => header
                .pb(px(18.))
                .child(
                    breadcrumb
                        .child(muted("/"))
                        .child(muted("Local pull requests"))
                        .child(muted("/"))
                        .child(format!("#{}", subject.pull_request.number)),
                )
                .child(self.render_pull_request_title(subject, totals, mono, cx)),
            Subject::WorkingTree { status, head, .. } => {
                let on = match head {
                    Some(RefLabel {
                        kind: RefKind::Branch,
                        name,
                        ..
                    }) => {
                        format!("On {name}")
                    }
                    Some(_) => "On a detached HEAD".to_owned(),
                    None => "No commits yet".to_owned(),
                };
                header
                    .child(
                        breadcrumb
                            .children(head.as_ref().map(crumb))
                            .child(muted("/"))
                            .child(
                                div()
                                    .text_color(rgb(color::ACCENT))
                                    .child("Uncommitted changes"),
                            ),
                    )
                    .child(
                        title("Uncommitted changes").child(
                            byline()
                                .child(muted(format!(
                                    "{on} · {}",
                                    labels::working_tree_summary(status)
                                )))
                                .when_some(totals, |this, totals| {
                                    this.children(total_parts(totals, mono).into_iter().skip(1))
                                }),
                        ),
                    )
            }
        }
    }

    /// The title, state, branches and totals of a pull request, and what
    /// can be done with it, as in the `LocalPR.dc.html` artboard.
    fn render_pull_request_title(
        &self,
        subject: &PullRequestSubject,
        totals: Option<(usize, usize, usize)>,
        mono: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pull_request = &subject.pull_request;
        let branch = |name: &str| {
            labels::ref_label(&RefLabel {
                kind: RefKind::Branch,
                name: name.to_owned(),
                is_head: false,
            })
            .into_any_element()
        };
        let commits = subject.ahead.map(|ahead| match ahead {
            1 => "1 commit".to_owned(),
            ahead => format!("{ahead} commits"),
        });
        let missing = [
            (subject.tips.head.is_none(), &pull_request.head.branch),
            (subject.tips.base.is_none(), &pull_request.base.branch),
        ]
        .into_iter()
        .filter(|(missing, _)| *missing)
        .map(|(_, name)| format!("{name} no longer exists"))
        .collect::<Vec<_>>();
        div()
            .h_flex()
            .items_start()
            .gap(px(20.))
            .child(
                div()
                    .v_flex()
                    .gap(px(8.))
                    .max_w(px(900.))
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .h_flex()
                            .items_baseline()
                            .gap(px(8.))
                            .text_size(px(22.))
                            .line_height(relative(1.3))
                            .child(
                                div()
                                    .min_w_0()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(pull_request.title.clone()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(rgb(color::TEXT_MUTED))
                                    .child(format!("#{}", pull_request.number)),
                            ),
                    )
                    .child(
                    byline()
                        .child(state_chip(pull_request.state))
                        .child(branch(&pull_request.head.branch))
                        .child(
                            Icon::new(IconName::ArrowRight)
                                .size(px(16.))
                                .text_color(rgb(color::TEXT_MUTED)),
                        )
                        .child(branch(&pull_request.base.branch))
                        .children(commits.map(|commits| muted(format!("· {commits}"))))
                        .when_some(totals, |this, totals| {
                            let [files, added, removed] = total_parts(totals, mono);
                            this.child(muted("·"))
                                .child(div().text_color(rgb(color::TEXT_MUTED)).child(files))
                                .child(added)
                                .child(removed)
                        })
                        .children(
                            (!missing.is_empty())
                                .then(|| muted(format!("· {}", missing.join(", ")))),
                        ),
                    )
                    .when(!pull_request.description.is_empty(), |this| {
                        this.child(
                            div()
                                .pt(px(2.))
                                .text_size(px(13.))
                                .line_height(relative(1.55))
                                .text_color(rgb(color::TEXT_SECONDARY))
                                .child(pull_request.description.clone()),
                        )
                    }),
            )
            .child(
                div()
                    .h_flex()
                    .flex_none()
                    .gap(px(6.))
                    .child(
                        Button::new("pull-request-edit")
                            .ghost()
                            .h(px(30.))
                            .icon(IconName::Pencil)
                            .label("Edit")
                            .on_click(cx.listener(|this, _, window, cx| this.open_edit(window, cx))),
                    )
                    .when(pull_request.state != State::Merged, |this| {
                        let open = pull_request.state == State::Open;
                        this.child(
                            Button::new("pull-request-close")
                                .ghost()
                                .h(px(30.))
                                .label(if open { "Close" } else { "Reopen" })
                                .tooltip(if open {
                                    "Close without merging; it keeps its comments and can be reopened"
                                } else {
                                    "Reopen to review the branch as it is now"
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_open(!open, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new("pull-request-checkout")
                            .h(px(30.))
                            .label(if subject.tips.checked_out {
                                "Checked out"
                            } else {
                                "Checkout branch"
                            })
                            .loading(subject.checking_out.is_some())
                            .disabled(
                                subject.tips.checked_out
                                    || subject.tips.head.is_none()
                                    || self.repository.work_dir().is_none(),
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.checkout_head(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("pull-request-merge")
                            .h(px(30.))
                            .label(format!("Merge into {}…", pull_request.base.branch))
                            .disabled(
                                pull_request.state != State::Open
                                    || subject.tips.head.is_none()
                                    || subject.tips.base.is_none()
                                    || self.repository.work_dir().is_none(),
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_merge(window, cx)),
                            ),
                    ),
            )
    }

    /// The strip above the diff: the layout, whitespace and context
    /// options, and in a local pull request how many files are marked
    /// viewed.
    fn render_toolbar(&self, files: &[FileDiff], cx: &mut Context<Self>) -> impl IntoElement {
        let viewed = files.iter().filter(|file| self.is_viewed(file)).count();
        let total = files.len();
        let settings = ReviewSettings::get(cx);
        let layout = settings.layout;
        div()
            .h_flex()
            .flex_none()
            .gap(px(10.))
            .px(px(24.))
            .py(px(10.))
            .bg(rgb(color::DIFF_CARD))
            .border_b_1()
            .border_color(rgb(color::BORDER))
            .text_size(px(12.5))
            .child(
                ButtonGroup::new("diff-layout")
                    .outline()
                    .small()
                    .child(
                        Button::new("unified")
                            .label("Unified")
                            .selected(layout == DiffLayout::Unified),
                    )
                    .child(
                        Button::new("split")
                            .label("Split")
                            .selected(layout == DiffLayout::Split),
                    )
                    .on_click(cx.listener(|this, clicked: &Vec<usize>, _, cx| {
                        let layout = if clicked.contains(&1) {
                            DiffLayout::Split
                        } else {
                            DiffLayout::Unified
                        };
                        this.set_layout(layout, cx)
                    })),
            )
            .child(
                Checkbox::new("hide-whitespace")
                    .label("Hide whitespace")
                    .checked(settings.hide_whitespace)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.set_hide_whitespace(*checked, cx)
                    })),
            )
            .child(self.expand_all_button(files, cx))
            .when_some(
                self.save_error().or(self.sources_error.clone()),
                |this, error| {
                    this.child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(rgb(color::DIFF_REMOVED))
                            .child(error),
                    )
                },
            )
            .when(self.is_pull_request(), |this| {
                this.child(
                    div()
                        .h_flex()
                        .ml_auto()
                        .gap(px(10.))
                        .child(muted("Viewed"))
                        .child(
                            div()
                                .w(px(120.))
                                .h(px(6.))
                                .rounded(px(3.))
                                .bg(rgb(color::BORDER))
                                .overflow_hidden()
                                .child(
                                    div()
                                        .h_full()
                                        .w(relative(viewed as f32 / total.max(1) as f32))
                                        .bg(rgb(color::DIFF_ADDED)),
                                ),
                        )
                        .child(format!("{viewed} of {total}")),
                )
            })
    }

    fn save_error(&self) -> Option<SharedString> {
        match &self.subject {
            Subject::PullRequest(subject) => subject.save_error.clone(),
            _ => None,
        }
    }

    /// "Expand all context", or "Collapse context" once every gap is shown.
    fn expand_all_button(&self, files: &[FileDiff], cx: &mut Context<Self>) -> Button {
        let mut gaps = Self::all_gaps(files).peekable();
        let any = gaps.peek().is_some();
        let all_shown = any && gaps.all(|(file, at)| self.is_expanded(&files[file], at));
        let button = Button::new("expand-all").ghost().small();
        if all_shown {
            button
                .label("Collapse context")
                .on_click(cx.listener(|this, _, _, cx| this.collapse_all(cx)))
        } else {
            button
                .label("Expand all context")
                .disabled(!any)
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(Ok(files)) = &this.files {
                        let gaps = Self::all_gaps(files).collect();
                        this.expand(gaps, cx);
                    }
                }))
        }
    }

    /// The changed files by directory; clicking one scrolls to it.
    fn render_files(&self, files: &[FileDiff], cx: &mut Context<Self>) -> impl IntoElement {
        let mut items: Vec<AnyElement> = Vec::new();
        let mut directory = None;
        let open_threads = self.open_threads_by_path();
        for (index, file) in files.iter().enumerate() {
            let (dir, name) = split_path(&file.path);
            if directory != Some(dir) {
                directory = Some(dir);
                if !dir.is_empty() {
                    items.push(
                        div()
                            .px(px(10.))
                            .pt(px(if index == 0 { 0. } else { 8. }))
                            .pb(px(6.))
                            .text_size(px(12.))
                            .text_color(rgb(color::TEXT_MUTED))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(dir.trim_end_matches('/').to_owned())
                            .into_any_element(),
                    );
                }
            }
            let (letter, letter_color) = labels::change_letter(&file.kind);
            let active = index == self.current_file;
            let viewed = self.is_viewed(file);
            items.push(
                div()
                    .id(("review-file", index))
                    .h_flex()
                    .flex_none()
                    .h(px(30.))
                    .px(px(10.))
                    .gap(px(8.))
                    .rounded(theme::RADIUS)
                    .text_size(px(12.5))
                    .map(|this| {
                        if active {
                            this.bg(rgb(color::FILE_ACTIVE))
                                .text_color(rgb(color::TEXT))
                        } else if viewed {
                            this.text_color(rgb(color::TEXT_FAINT)).hover(|style| {
                                style.bg(rgb(color::NAV_HOVER)).text_color(rgb(color::TEXT))
                            })
                        } else {
                            this.text_color(rgb(color::TEXT_SECONDARY)).hover(|style| {
                                style.bg(rgb(color::NAV_HOVER)).text_color(rgb(color::TEXT))
                            })
                        }
                    })
                    .child(
                        div()
                            .w(px(10.))
                            .flex_none()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_color(rgb(letter_color))
                            .child(letter),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .when(viewed, |this| this.line_through())
                            .child(name.to_owned()),
                    )
                    .when_some(open_threads.get(file.path.as_str()), |this, open| {
                        this.child(threads::chip(open.to_string()))
                    })
                    .when(viewed, |this| {
                        this.child(
                            Icon::new(IconName::Check)
                                .size(px(13.))
                                .text_color(rgb(color::DIFF_ADDED)),
                        )
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.scroll_to_file(index, cx)))
                    .into_any_element(),
            );
        }

        div()
            .id("review-files")
            .v_flex()
            .w(px(250.))
            .flex_none()
            .h_full()
            .gap(px(2.))
            .overflow_y_scroll()
            .pb(px(24.))
            .child(
                div()
                    .h_flex()
                    .h(px(24.))
                    .pl(px(10.))
                    .mb(px(4.))
                    .gap(px(2.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(color::TEXT_FAINT))
                            .child("FILES"),
                    ),
            )
            .children(items)
    }
}

impl Focusable for Review {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Review {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.files {
            None => message("Reading changes…").into_any_element(),
            Some(Err(error)) => message(error.clone()).into_any_element(),
            Some(Ok(files)) if files.is_empty() => match self.subject {
                Subject::Commit(_) => message("This commit changes no files."),
                Subject::WorkingTree { .. } => message("No uncommitted changes."),
                Subject::PullRequest(_) => message("This branch changes no files."),
            }
            .into_any_element(),
            Some(Ok(files)) => {
                let files = files.clone();
                div()
                    .v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_toolbar(&files, cx))
                    .child(
                        div()
                            .h_flex()
                            .items_start()
                            .flex_1()
                            .min_h_0()
                            .gap(px(20.))
                            .pt(px(20.))
                            .px(px(24.))
                            // Beside the history, the changes panel or the
                            // commit detail lists the files.
                            .when(self.is_pull_request(), |this| {
                                this.child(self.render_files(&files, cx))
                            })
                            .child(
                                list(
                                    self.list.clone(),
                                    cx.processor(|this, index, _window, cx| {
                                        this.render_row(index, cx)
                                    }),
                                )
                                .flex_1()
                                .min_w_0()
                                .h_full(),
                            )
                            .children(self.render_prompt_panel(cx)),
                    )
                    .into_any_element()
            }
        };

        div()
            .key_context("Review")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close))
            .on_action(cx.listener(Self::newer))
            .on_action(cx.listener(Self::older))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.finish_selecting(window, cx)),
            )
            .v_flex()
            .size_full()
            .bg(rgb(color::BACKGROUND))
            .child(self.render_header(cx))
            .child(body)
    }
}

/// Pull request `number` read from `store` with its branches moved to
/// `tips`, saved again if that changed it, and its diff. Blocks; run it on
/// a background thread.
fn read_pull_request(
    repository: &Repository,
    store: &Store,
    number: u32,
    tips: Tips,
    options: DiffOptions,
) -> anyhow::Result<(PullRequest, BranchDiff)> {
    let mut pull_request = store.load(number)?;
    let read = pull_request.clone();
    let commit = |hex: &str| {
        Oid::from_hex(hex.as_bytes()).with_context(|| format!("{hex} is not a commit id"))
    };
    let last_head = commit(&pull_request.head.commit)?;
    let last_base = commit(&pull_request.base.commit)?;
    // A merged or closed pull request keeps the changes it had then.
    let (head, base) = if pull_request.state == State::Open {
        (
            tips.head.unwrap_or(last_head),
            tips.base.unwrap_or(last_base),
        )
    } else {
        (last_head, last_base)
    };
    follow_head(repository, &mut pull_request, head, Timestamp::now())?;
    pull_request.base.commit = base.to_string();
    let diff = repository.branch_diff(base, head, options)?;
    if pull_request != read {
        store.save(&pull_request)?;
    }
    Ok((pull_request, diff))
}

/// Moves `pull_request`'s head to commit `head`, if it is not there yet:
/// threads whose lines changed resolve themselves and the others follow
/// their lines. Blocks; run it on a background thread.
fn follow_head(
    repository: &Repository,
    pull_request: &mut PullRequest,
    head: Oid,
    now: Timestamp,
) -> anyhow::Result<()> {
    let last_head = Oid::from_hex(pull_request.head.commit.as_bytes())
        .with_context(|| format!("{} is not a commit id", pull_request.head.commit))?;
    if head == last_head {
        return Ok(());
    }
    pull_request.move_head(&head.to_string(), now, |path| {
        Ok::<_, gitez_git::Error>(match repository.follow_lines(path, last_head, head)? {
            Followed::Unchanged => FileUpdate::Unchanged,
            Followed::Changed { path, lines } => FileUpdate::Changed {
                path,
                lines: lines
                    .into_iter()
                    .map(|lines| LineChange {
                        old: lines.old,
                        new: lines.new,
                    })
                    .collect(),
            },
            Followed::Gone => FileUpdate::Gone,
        })
    })?;
    Ok(())
}

/// "In review", "Merged" or "Closed", with a dot.
fn state_chip(state: State) -> impl IntoElement {
    let (label, dot, text) = match state {
        State::Open => (
            "In review",
            color::BRANCH_TEAL,
            color::PULL_REQUEST_OPEN_TEXT,
        ),
        State::Merged => ("Merged", color::BRANCH_VIOLET, color::BRANCH_VIOLET),
        State::Closed => ("Closed", color::TEXT_FAINT, color::TEXT_MUTED),
    };
    div()
        .h_flex()
        .h(px(22.))
        .px(px(8.))
        .gap(px(5.))
        .rounded(px(11.))
        .bg(theme::with_alpha(dot, 0.16))
        .text_color(rgb(text))
        .text_size(theme::SMALL_TEXT_SIZE)
        .child(div().size(px(7.)).rounded_full().bg(rgb(dot)))
        .child(label)
}

/// The rows of the diff list for `files`, and the row of each file's header.
/// A file that is `viewed` shows only its header.
/// The lines left out at a gap are shown where `expanded`.
fn rows(
    files: &[FileDiff],
    viewed: impl Fn(&FileDiff) -> bool,
    expanded: impl Fn(&FileDiff, GapAt) -> bool,
    layout: DiffLayout,
) -> (Vec<Row>, Vec<usize>) {
    let gap_rows = |rows: &mut Vec<Row>, file: usize, hunks: &[Hunk], at: GapAt| {
        if let Some(gap) = gap(hunks, at) {
            if expanded(&files[file], at) {
                rows.extend((0..gap.len).map(|line| Row::Context(file, at, line)));
            } else {
                rows.push(Row::Gap(file, at));
            }
        }
    };
    let mut rows = Vec::new();
    let mut file_rows = Vec::with_capacity(files.len());
    for (file, diff) in files.iter().enumerate() {
        file_rows.push(rows.len());
        rows.push(Row::File(file));
        match &diff.content {
            _ if viewed(diff) => {}
            DiffContent::Text(hunks) => {
                for (hunk_index, hunk) in hunks.iter().enumerate() {
                    gap_rows(&mut rows, file, hunks, GapAt::Before(hunk_index));
                    rows.push(Row::Hunk(file, hunk_index));
                    match layout {
                        DiffLayout::Unified => rows.extend(
                            (0..hunk.lines.len()).map(|line| Row::Line(file, hunk_index, line)),
                        ),
                        DiffLayout::Split => rows.extend(
                            split_pairs(&hunk.lines)
                                .into_iter()
                                .map(|(old, new)| Row::Pair(file, hunk_index, old, new)),
                        ),
                    }
                    gap_rows(&mut rows, file, hunks, GapAt::After(hunk_index));
                }
            }
            DiffContent::TooLarge(_)
            | DiffContent::Binary
            | DiffContent::Unchanged
            | DiffContent::OnlyWhitespace => {
                rows.push(Row::Notice(file));
            }
        }
        rows.push(Row::FileEnd(file));
    }
    rows.push(Row::End);
    (rows, file_rows)
}

/// The rows of a hunk in the split layout, as indices into `lines`: each
/// context line on both sides, and each run of removed lines beside the
/// added lines that follow it.
fn split_pairs(lines: &[DiffLine]) -> Vec<(Option<usize>, Option<usize>)> {
    fn flush(
        pairs: &mut Vec<(Option<usize>, Option<usize>)>,
        removed: &mut Vec<usize>,
        added: &mut Vec<usize>,
    ) {
        for row in 0..removed.len().max(added.len()) {
            pairs.push((removed.get(row).copied(), added.get(row).copied()));
        }
        removed.clear();
        added.clear();
    }

    let mut pairs = Vec::with_capacity(lines.len());
    let (mut removed, mut added) = (Vec::new(), Vec::new());
    for (index, line) in lines.iter().enumerate() {
        match line.kind {
            LineKind::Context => {
                flush(&mut pairs, &mut removed, &mut added);
                pairs.push((Some(index), Some(index)));
            }
            LineKind::Removed => {
                if !added.is_empty() {
                    flush(&mut pairs, &mut removed, &mut added);
                }
                removed.push(index);
            }
            LineKind::Added => added.push(index),
        }
    }
    flush(&mut pairs, &mut removed, &mut added);
    pairs
}

/// What a file of a local pull request marked viewed is compared with
/// later: the id of its new version, empty when deleted.
fn view_key(file: &FileDiff) -> String {
    file.new_id.map(|id| id.to_string()).unwrap_or_default()
}

/// What a file's diff looks like, to tell whether it changed since its
/// gaps were expanded. Where the lines are and whether they are staged is
/// left out, so staging a hunk does not count as a change.
fn fingerprint(file: &FileDiff) -> u64 {
    let mut hasher = DefaultHasher::new();
    match &file.content {
        DiffContent::Text(hunks) => {
            // Hunks hashed apart and summed, as staging reorders them.
            let sum = hunks.iter().fold(0_u64, |sum, hunk| {
                let mut hunk_hasher = DefaultHasher::new();
                for line in &hunk.lines {
                    (line.kind as u8, &line.text, line.crlf, line.no_newline)
                        .hash(&mut hunk_hasher);
                }
                sum.wrapping_add(hunk_hasher.finish())
            });
            (0_u8, sum).hash(&mut hasher);
        }
        DiffContent::TooLarge(counts) => (1_u8, counts.added, counts.removed).hash(&mut hasher),
        DiffContent::Binary => 2_u8.hash(&mut hasher),
        DiffContent::Unchanged => 3_u8.hash(&mut hasher),
        DiffContent::OnlyWhitespace => 4_u8.hash(&mut hasher),
    }
    hasher.finish()
}

/// Files, lines added and lines removed.
fn totals(files: &[FileDiff]) -> (usize, usize, usize) {
    files
        .iter()
        .filter_map(FileDiff::line_counts)
        .fold((files.len(), 0, 0), |(files, added, removed), counts| {
            (files, added + counts.added, removed + counts.removed)
        })
}

/// A row inside a file's card: its side borders.
fn card_row() -> Div {
    div()
        .h_flex()
        // List rows are laid out on their own and would otherwise be only as
        // wide as their content.
        .w_full()
        .bg(rgb(color::DIFF_CARD))
        .border_l_1()
        .border_r_1()
        .border_color(rgb(color::DIFF_CARD_BORDER))
}

/// The top of a file's card: path, what happened to it, how much of it is
/// staged (for uncommitted changes) and line counts. A file marked `viewed`
/// is collapsed to this header.
fn file_header(
    file: &FileDiff,
    status: Option<&FileStatus>,
    viewed: bool,
    mono: SharedString,
) -> Div {
    let (directory, name) = split_path(&file.path);
    let untracked = status.is_some_and(|status| status.unstaged == Some(WorktreeChange::Untracked));
    let kind = match &file.kind {
        ChangeKind::Added if untracked => None,
        ChangeKind::Added => Some(("new file".to_owned(), color::DIFF_ADDED_TEXT)),
        ChangeKind::Deleted => Some(("deleted".to_owned(), color::DIFF_REMOVED)),
        ChangeKind::Renamed { from } => Some((format!("renamed from {from}"), color::BRANCH_BLUE)),
        ChangeKind::Copied { from } => Some((format!("copied from {from}"), color::BRANCH_BLUE)),
        ChangeKind::Modified => None,
    };
    let staging = status.and_then(|status| match (&status.staged, status.unstaged) {
        (_, Some(WorktreeChange::Conflicted)) => Some(("conflict".to_owned(), color::DIFF_REMOVED)),
        (_, Some(WorktreeChange::Untracked)) => Some(("untracked".to_owned(), color::TEXT_MUTED)),
        (Some(_), None) => Some(("staged".to_owned(), color::ACCENT)),
        (Some(_), Some(_)) => Some(("partly staged".to_owned(), color::ACCENT)),
        (None, _) => None,
    });
    let counts = file.line_counts();

    div()
        .h_flex()
        .w_full()
        .gap(px(10.))
        .px(px(12.))
        .py(px(8.))
        .bg(rgb(color::DIFF_FILE_HEADER))
        .border_1()
        .border_color(rgb(color::DIFF_CARD_BORDER))
        .text_size(px(12.5))
        .map(|this| {
            if viewed {
                this.rounded(px(8.))
                    .text_color(rgb(color::TEXT_MUTED))
                    .child(Icon::new(IconName::ChevronRight).size(px(12.)))
            } else {
                this.rounded_t(px(8.))
            }
        })
        .child(
            div()
                .h_flex()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .font_family(mono.clone())
                .child(
                    div()
                        .flex_none()
                        .text_color(rgb(color::TEXT_MUTED))
                        .child(directory.to_owned()),
                )
                .child(div().min_w_0().text_ellipsis().child(name.to_owned())),
        )
        .children(kind.into_iter().chain(staging).map(|(label, label_color)| {
            div()
                .flex_none()
                .h(px(20.))
                .px(px(7.))
                .rounded(px(4.))
                .h_flex()
                .text_size(theme::SMALL_TEXT_SIZE)
                .font_weight(FontWeight::MEDIUM)
                .bg(theme::with_alpha(label_color, 0.14))
                .text_color(rgb(label_color))
                .child(label)
        }))
        .when_some(counts, |this, counts| {
            this.child(
                div()
                    .h_flex()
                    .flex_none()
                    .gap(px(6.))
                    .font_family(mono)
                    .text_size(px(12.))
                    .when(counts.added > 0, |this| {
                        this.child(
                            div()
                                .text_color(rgb(color::DIFF_ADDED))
                                .child(format!("+{}", counts.added)),
                        )
                    })
                    .when(counts.removed > 0, |this| {
                        this.child(
                            div()
                                .text_color(rgb(color::DIFF_REMOVED))
                                .child(format!("−{}", counts.removed)),
                        )
                    }),
            )
        })
        .when(viewed, |this| {
            this.child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(12.))
                    .child("Collapsed because you marked it viewed"),
            )
        })
}

/// The version of a file `line` of `hunk` is highlighted in, its number
/// there, and which version that is: a removed line's is the old file, the
/// others' the new one.
fn line_source<'a>(hunk: &'a Hunk, line: &DiffLine) -> Option<(&'a LineSource, u32, Version)> {
    let new = hunk.new_side.as_ref().map(|side| &side.source);
    let old = (hunk.old_side.as_ref(), line.old, Version::Old);
    let (source, number, version) = match line.kind {
        LineKind::Removed => old,
        LineKind::Added => (new, line.new, Version::New),
        // A context line is the same in both; the new file is the one gaps
        // are filled from.
        LineKind::Context => match new {
            Some(new) => (Some(new), line.new, Version::New),
            None => old,
        },
    };
    Some((source?, number?, version))
}

/// The path of `file` before the change, which differs from its path for a
/// renamed or copied file.
fn old_path(file: &FileDiff) -> &str {
    match &file.kind {
        ChangeKind::Renamed { from } | ChangeKind::Copied { from } => from,
        _ => &file.path,
    }
}

/// The syntax highlighting of `job`'s version of a file: of the whole file,
/// or when it is too large or cannot be read, of the lines its hunks show
/// of it, as the old side of some and the new side of others. `None` when
/// it stays plain. Blocks; run it in the background.
fn highlight(
    repository: &Repository,
    files: &[FileDiff],
    job: &HighlightJob,
) -> Option<Arc<Highlights>> {
    if let Ok(bytes) = repository.source_bytes(&job.source) {
        // Bytes skipped as binary may still diff as text, decoded by a
        // filter, so those fall back to the hunks' lines too.
        match gitez_highlight::highlight_file(&job.path, &bytes) {
            Outcome::Highlighted(highlights) => return Some(highlights),
            Outcome::Plain => return None,
            Outcome::Skipped => {}
        }
    }
    let DiffContent::Text(hunks) = &files.get(job.file)?.content else {
        return None;
    };
    // The file's lines by number; a line two hunks show is taken once.
    let mut lines = std::collections::BTreeMap::<u32, &str>::new();
    for hunk in hunks {
        let sides = [
            (hunk.old_side.as_ref(), Version::Old),
            (
                hunk.new_side.as_ref().map(|side| &side.source),
                Version::New,
            ),
        ];
        for (source, version) in sides {
            if source != Some(&job.source) {
                continue;
            }
            for line in &hunk.lines {
                let number = match version {
                    Version::Old => line.old,
                    Version::New => line.new,
                };
                if let Some(number) = number {
                    lines.entry(number).or_insert(&line.text);
                }
            }
        }
    }
    // Consecutive numbers make one run.
    let mut runs: Vec<(u32, Vec<&str>)> = Vec::new();
    for (number, text) in lines {
        match runs.last_mut() {
            Some((first, run))
                if u32::try_from(run.len())
                    .is_ok_and(|len| first.checked_add(len) == Some(number)) =>
            {
                run.push(text);
            }
            _ => runs.push((number, vec![text])),
        }
    }
    // Only a run from the top of the file has its first line.
    let first_line = runs
        .first()
        .filter(|(first, _)| *first == 1)
        .and_then(|(_, lines)| lines.first().copied())
        .unwrap_or_default();
    let language = Language::detect(&job.path, first_line)?;
    Some(Arc::new(gitez_highlight::highlight_runs(language, runs)))
}

/// One line of a diff: old and new line numbers, `+` or `−`, and the text,
/// coloured by its syntax `spans` if it has them.
fn diff_line(line: &DiffLine, spans: Option<&[Span]>, mono: SharedString) -> impl IntoElement {
    let (sign, tint) = line_style(line.kind);
    card_row()
        .h(LINE_HEIGHT)
        .font_family(mono)
        .text_size(theme::MONO_TEXT_SIZE)
        .when_some(tint, |this, tint| this.bg(theme::with_alpha(tint, 0.10)))
        .child(line_number(line.old, tint))
        .child(line_number(line.new, tint))
        .child(line_sign(sign, tint))
        .child(line_text(line, spans))
        .when(line.no_newline, |this| this.child(no_newline_note()))
}

/// A row of the split layout: the `old` side of a line on the left, the
/// `new` side of a line on the right, each with its syntax spans.
fn split_line(
    old: Option<(&DiffLine, Option<&[Span]>)>,
    new: Option<(&DiffLine, Option<&[Span]>)>,
    mono: SharedString,
) -> impl IntoElement {
    card_row()
        .h(LINE_HEIGHT)
        .font_family(mono)
        .text_size(theme::MONO_TEXT_SIZE)
        .child(split_half(old, |line| line.old))
        .child(
            div()
                .w(px(1.))
                .h_full()
                .flex_none()
                .bg(rgb(color::DIFF_CARD_BORDER)),
        )
        .child(split_half(new, |line| line.new))
}

/// One side of a split row, numbered by `number`; blank without a line.
fn split_half(
    line: Option<(&DiffLine, Option<&[Span]>)>,
    number: fn(&DiffLine) -> Option<u32>,
) -> Div {
    let half = div().h_flex().flex_1().min_w_0().h_full();
    let Some((line, spans)) = line else {
        return half.bg(rgb(color::DIFF_FILE_HEADER));
    };
    let (sign, tint) = line_style(line.kind);
    half.when_some(tint, |this, tint| this.bg(theme::with_alpha(tint, 0.10)))
        .child(line_number(number(line), tint))
        .child(line_sign(sign, tint))
        .child(line_text(line, spans))
        .when(line.no_newline, |this| this.child(no_newline_note()))
}

/// The sign of a line of `kind` and the colour it is tinted with.
fn line_style(kind: LineKind) -> (&'static str, Option<u32>) {
    match kind {
        LineKind::Context => ("", None),
        LineKind::Added => ("+", Some(color::DIFF_ADDED)),
        LineKind::Removed => ("−", Some(color::DIFF_REMOVED)),
    }
}

fn line_number(number: Option<u32>, tint: Option<u32>) -> Div {
    div()
        .w(px(48.))
        .flex_none()
        .h_full()
        .pr(px(10.))
        .text_right()
        .text_color(rgb(color::DIFF_LINE_NUMBER))
        .when_some(tint, |this, tint| this.bg(theme::with_alpha(tint, 0.08)))
        .children(number.map(|number| number.to_string()))
}

fn line_sign(sign: &'static str, tint: Option<u32>) -> Div {
    div()
        .w(px(20.))
        .flex_none()
        .text_center()
        .text_color(rgb(tint.unwrap_or(color::DIFF_LINE_NUMBER)))
        .child(sign)
}

fn line_text(line: &DiffLine, spans: Option<&[Span]>) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .pr(px(12.))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .text_color(rgb(color::DIFF_TEXT))
        // A carriage return is shown only as part of the text when no
        // newline follows it.
        .child(code::styled_code(&line.text, spans))
}

fn no_newline_note() -> Div {
    div()
        .flex_none()
        .pr(px(12.))
        .text_size(theme::SMALL_TEXT_SIZE)
        .text_color(rgb(color::TEXT_FAINT))
        .child("no newline at end of file")
}

/// Why `file` shows no lines.
fn notice(file: &FileDiff) -> String {
    match (&file.content, &file.kind) {
        (DiffContent::TooLarge(counts), _) => format!(
            "Large diff not shown: {} lines added, {} removed.",
            counts.added, counts.removed
        ),
        (DiffContent::Binary, _) => "Binary file not shown.".to_owned(),
        (DiffContent::OnlyWhitespace, _) => "Only whitespace changed.".to_owned(),
        (_, ChangeKind::Renamed { .. }) => "Renamed without changes.".to_owned(),
        (_, ChangeKind::Copied { .. }) => "Copied without changes.".to_owned(),
        (_, ChangeKind::Added | ChangeKind::Deleted) => "Empty file.".to_owned(),
        _ => "Only the file mode or submodule commit changed.".to_owned(),
    }
}

/// `path` split after its last `/`: the directory (with the slash) and name.
fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(slash) => path.split_at(slash + 1),
        None => ("", path),
    }
}

/// A ref label in the breadcrumb, after a "/".
fn crumb(label: &RefLabel) -> impl IntoElement {
    div()
        .h_flex()
        .gap(px(10.))
        .child(muted("/"))
        .child(labels::ref_label(label))
}

/// The title of the review, with room for more lines below it.
fn title(text: impl Into<SharedString>) -> Div {
    div().v_flex().gap(px(8.)).max_w(px(900.)).child(
        div()
            .text_size(px(22.))
            .line_height(relative(1.3))
            .font_weight(FontWeight::SEMIBOLD)
            .child(text.into()),
    )
}

/// The line under the title: author, date and totals.
fn byline() -> Div {
    div().h_flex().flex_wrap().gap(px(10.)).text_size(px(12.5))
}

/// "3 files", "+12" and "−4".
fn total_parts(
    (files, added, removed): (usize, usize, usize),
    mono: SharedString,
) -> [AnyElement; 3] {
    [
        match files {
            1 => "1 file".to_owned(),
            files => format!("{files} files"),
        }
        .into_any_element(),
        div()
            .font_family(mono.clone())
            .text_color(rgb(color::DIFF_ADDED))
            .child(format!("+{added}"))
            .into_any_element(),
        div()
            .font_family(mono)
            .text_color(rgb(color::DIFF_REMOVED))
            .child(format!("−{removed}"))
            .into_any_element(),
    ]
}

fn muted(text: impl Into<SharedString>) -> Div {
    div().text_color(rgb(color::TEXT_MUTED)).child(text.into())
}

/// A centred message filling the body.
fn message(text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .p(px(24.))
        .text_color(rgb(color::TEXT_MUTED))
        .child(text.into())
}

#[cfg(test)]
mod tests {
    use gitez_git::{
        ChangeKind, DiffContent, DiffLine, FileDiff, Hunk, LineKind, LineSource, NewSide, Staging,
    };

    use super::{
        DiffLayout, Gap, GapAt, Row, Version, fingerprint, gap, line_source, rows, split_pairs,
    };

    fn line(kind: LineKind, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old: None,
            new: None,
            text: text.to_owned(),
            crlf: false,
            no_newline: false,
        }
    }

    fn hunk(start: u32, staging: Staging, lines: Vec<DiffLine>) -> Hunk {
        Hunk {
            old_start: start,
            old_len: 1,
            new_start: start,
            new_len: 1,
            lines,
            staging: Some(staging),
            patchable: true,
            new_side: None,
            old_side: None,
        }
    }

    /// A hunk from `old_start,old_len` to `new_start,new_len` of a file
    /// with `lines` lines, which `staged` tells apart.
    fn ranged(
        (old_start, old_len): (u32, u32),
        (new_start, new_len): (u32, u32),
        staged: bool,
        lines: u32,
    ) -> Hunk {
        Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
            lines: Vec::new(),
            staging: Some(if staged {
                Staging::Staged
            } else {
                Staging::Unstaged
            }),
            patchable: true,
            new_side: Some(NewSide {
                source: LineSource::WorkingTree(format!("{staged}")),
                lines,
            }),
            old_side: None,
        }
    }

    #[test]
    fn removed_lines_are_highlighted_in_the_old_file_and_others_in_the_new() {
        let old = LineSource::Blob(
            gitez_git::Oid::from_hex(b"1111111111111111111111111111111111111111").expect("an id"),
        );
        let new = LineSource::WorkingTree("a.rs".to_owned());
        let hunk = Hunk {
            old_side: Some(old.clone()),
            ..ranged((3, 2), (3, 2), false, 9)
        };
        let hunk = Hunk {
            new_side: hunk.new_side.map(|side| NewSide {
                source: new.clone(),
                ..side
            }),
            ..hunk
        };
        let numbered = |kind, old, new| DiffLine {
            old,
            new,
            ..line(kind, "x")
        };
        assert_eq!(
            line_source(&hunk, &numbered(LineKind::Removed, Some(3), None)),
            Some((&old, 3, Version::Old))
        );
        assert_eq!(
            line_source(&hunk, &numbered(LineKind::Added, None, Some(4))),
            Some((&new, 4, Version::New))
        );
        assert_eq!(
            line_source(&hunk, &numbered(LineKind::Context, Some(5), Some(6))),
            Some((&new, 6, Version::New))
        );
        // A deleted file has only its old version.
        let deleted = Hunk {
            new_side: None,
            ..hunk.clone()
        };
        assert_eq!(
            line_source(&deleted, &numbered(LineKind::Context, Some(5), None)),
            Some((&old, 5, Version::Old))
        );
        assert_eq!(
            line_source(&deleted, &numbered(LineKind::Added, None, Some(4))),
            None
        );
    }

    #[gpui_kit::test]
    fn changed_files_are_highlighted_in_the_background(cx: &mut gpui_kit::TestAppContext) {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
                .args(args)
                .current_dir(root)
                .output()
                .expect("run git");
            assert!(output.status.success(), "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.rs"), "fn old() {}\n/* a\n   b */\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "-qm", "start"]);
        std::fs::write(root.join("a.rs"), "fn new() {}\n/* a\n   b */\n").expect("write");

        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::init(cx);
        });
        let repository = gitez_git::Repository::open(root).expect("open repository");
        let status = std::sync::Arc::new(repository.status().expect("status"));
        let window = cx.open_window(
            gpui_kit::size(gpui_kit::px(1000.), gpui_kit::px(800.)),
            |_, cx| super::Review::working_tree(&repository, status, None, cx),
        );
        let review = window
            .update(cx, |_, _, cx| cx.entity())
            .expect("window open");
        let cx = gpui_kit::VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();

        review.read_with(&cx, |review, _| {
            let Some(Ok(files)) = &review.files else {
                panic!("the diff is read");
            };
            let DiffContent::Text(hunks) = &files[0].content else {
                panic!("a text diff");
            };
            let hunk = &hunks[0];
            let kinds = |line: &DiffLine| {
                review
                    .line_spans(&files[0], hunk, line)
                    .map(|spans| spans.iter().map(|span| span.kind).collect::<Vec<_>>())
            };
            // `fn` is a keyword in both versions, and the comment's second
            // line, unchanged, is a comment because the whole file is read.
            for line in &hunk.lines {
                let kinds = kinds(line).expect("every line is highlighted");
                match line.text.as_str() {
                    "fn old() {}" | "fn new() {}" => {
                        assert_eq!(kinds.first(), Some(&gitez_highlight::TokenKind::Keyword))
                    }
                    "   b */" => assert_eq!(kinds, [gitez_highlight::TokenKind::Comment]),
                    _ => {}
                }
            }
            assert!(hunk.lines.iter().any(|line| line.kind == LineKind::Removed));
        });
    }

    #[test]
    fn gaps_lie_between_hunks_of_the_same_file() {
        let hunks = [
            // Staged: two lines added at the start of a 10-line file.
            ranged((0, 0), (1, 2), true, 10),
            // Unstaged, in a 30-line file.
            ranged((4, 3), (4, 4), false, 30),
            ranged((15, 3), (16, 3), false, 30),
        ];
        let gap = |at| gap(&hunks, at);
        let at = |old, new, len| Some(Gap { old, new, len });
        assert_eq!(gap(GapAt::Before(0)), None);
        assert_eq!(gap(GapAt::After(0)), at(1, 3, 8));
        assert_eq!(gap(GapAt::Before(1)), at(1, 1, 3));
        assert_eq!(gap(GapAt::After(1)), None);
        assert_eq!(gap(GapAt::Before(2)), at(7, 8, 8));
        assert_eq!(gap(GapAt::After(2)), at(18, 19, 12));
    }

    #[test]
    fn an_expanded_gap_shows_its_lines() {
        let files = [FileDiff {
            path: "f".to_owned(),
            kind: ChangeKind::Modified,
            content: DiffContent::Text(vec![ranged((3, 1), (3, 1), false, 3)]),
            new_id: None,
        }];
        let (rows, _) = rows(
            &files,
            |_| false,
            |_, at| at == GapAt::Before(0),
            DiffLayout::Unified,
        );
        assert_eq!(
            rows,
            [
                Row::File(0),
                Row::Context(0, GapAt::Before(0), 0),
                Row::Context(0, GapAt::Before(0), 1),
                Row::Hunk(0, 0),
                Row::FileEnd(0),
                Row::End,
            ]
        );
    }

    fn file(path: &str, hunks: Vec<Hunk>) -> FileDiff {
        FileDiff {
            path: path.to_owned(),
            kind: ChangeKind::Modified,
            content: DiffContent::Text(hunks),
            new_id: None,
        }
    }

    #[test]
    fn staging_a_hunk_keeps_the_fingerprint_but_editing_does_not() {
        let first = || vec![line(LineKind::Removed, "a"), line(LineKind::Added, "b")];
        let second = || vec![line(LineKind::Added, "z")];
        let unstaged = file(
            "f",
            vec![
                hunk(1, Staging::Unstaged, first()),
                hunk(9, Staging::Unstaged, second()),
            ],
        );
        // The second hunk staged: it moves first, as a staged hunk.
        let partly_staged = file(
            "f",
            vec![
                hunk(8, Staging::Staged, second()),
                hunk(1, Staging::Unstaged, first()),
            ],
        );
        assert_eq!(fingerprint(&unstaged), fingerprint(&partly_staged));

        let edited = file(
            "f",
            vec![
                hunk(1, Staging::Unstaged, first()),
                hunk(9, Staging::Unstaged, vec![line(LineKind::Added, "y")]),
            ],
        );
        assert_ne!(fingerprint(&unstaged), fingerprint(&edited));

        let mut crlf = unstaged.clone();
        if let DiffContent::Text(hunks) = &mut crlf.content {
            hunks[1].lines[0].crlf = true;
        }
        assert_ne!(fingerprint(&unstaged), fingerprint(&crlf));
    }

    #[test]
    fn a_viewed_file_shows_only_its_header() {
        let files = [
            file(
                "a",
                vec![hunk(1, Staging::Unstaged, vec![line(LineKind::Added, "x")])],
            ),
            file(
                "b",
                vec![hunk(1, Staging::Unstaged, vec![line(LineKind::Added, "y")])],
            ),
        ];
        let (rows, file_rows) = rows(
            &files,
            |file| file.path == "a",
            |_, _| false,
            DiffLayout::Unified,
        );
        let rows: Vec<String> = rows.iter().map(|row| format!("{row:?}")).collect();
        assert_eq!(
            rows,
            [
                "File(0)",
                "FileEnd(0)",
                "File(1)",
                "Hunk(1, 0)",
                "Line(1, 0, 0)",
                "FileEnd(1)",
                "End"
            ]
        );
        assert_eq!(file_rows, [0, 2]);
    }

    #[test]
    fn split_rows_pair_removed_lines_with_the_added_lines_after_them() {
        use LineKind::{Added, Context, Removed};
        let lines: Vec<DiffLine> = [Context, Removed, Removed, Added, Context, Added, Removed]
            .into_iter()
            .map(|kind| line(kind, ""))
            .collect();
        assert_eq!(
            split_pairs(&lines),
            [
                (Some(0), Some(0)),
                (Some(1), Some(3)),
                (Some(2), None),
                (Some(4), Some(4)),
                (None, Some(5)),
                (Some(6), None),
            ]
        );
    }
}
