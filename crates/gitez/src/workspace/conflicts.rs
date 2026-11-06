//! Resolving the conflicts of a merge that stopped, as in the
//! `Conflicts.dc.html` artboard: the files that need attention, each
//! conflict with our and their lines side by side, ticking lines into the
//! result or editing it, then committing the merge or giving it up.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gitez_command::{Git, ResetMode, Side};
use gitez_git::{
    Conflict, ConflictFile, MergeInProgress, Oid, Repository, Segment, WorktreeChange,
};
use gitez_highlight::{Highlights, Span};
use gitez_review::{PullRequest, State, Store};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{InputEvent, Position, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::code;
use crate::theme::{self, color};
use crate::undo::{self, Recorder};

mod story;

/// Lines of the file shown before and after a conflict.
const CONTEXT: usize = 3;
/// Height of a line of code.
const LINE_HEIGHT: Pixels = px(22.);

/// What the editor asks of the window.
pub enum ConflictsEvent {
    /// Go back to the history; the merge stays in progress.
    Close,
    /// The index or the branch changed.
    Changed,
    /// The merge was committed, or given up when `committed` is false.
    /// `step` is the undo step.
    Finished { committed: bool, step: Option<u64> },
    /// Go back to the history with this commit selected; the merge stays
    /// in progress.
    ShowCommit(Oid),
}

/// How a conflict is resolved so far.
#[derive(Debug, Clone, Default)]
struct Resolution {
    /// Which of our lines go into the result.
    ours: Vec<bool>,
    theirs: Vec<bool>,
    /// Their picked lines come before ours: their side got its first
    /// tick while ours had none.
    theirs_first: bool,
    /// The result as typed, replacing the picked lines.
    edited: Option<Vec<String>>,
    resolved: bool,
}

/// Where a result line comes from.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Source {
    Ours,
    Theirs,
    /// Typed, or a line both sides have.
    Both,
}

impl Resolution {
    fn new(conflict: &Conflict) -> Self {
        Self {
            ours: vec![false; conflict.ours.len()],
            theirs: vec![false; conflict.theirs.len()],
            ..Self::default()
        }
    }

    /// The conflict's lines in the result, where each comes from, and its
    /// index in that side's lines of the conflict; typed lines have none.
    fn lines(&self, conflict: &Conflict) -> Vec<(String, Source, Option<usize>)> {
        if let Some(edited) = &self.edited {
            return edited
                .iter()
                .map(|line| (line.clone(), Source::Both, None))
                .collect();
        }
        let pick = |lines: &[String], picked: &[bool], source| {
            lines
                .iter()
                .zip(picked)
                .enumerate()
                .filter(|(_, (_, picked))| **picked)
                .map(|(index, (line, _))| (line.clone(), source, Some(index)))
                .collect::<Vec<_>>()
        };
        let ours = pick(&conflict.ours, &self.ours, Source::Ours);
        let theirs = pick(&conflict.theirs, &self.theirs, Source::Theirs);
        if self.theirs_first {
            [theirs, ours].concat()
        } else {
            [ours, theirs].concat()
        }
    }

    fn picked(&self, side: Side) -> &[bool] {
        match side {
            Side::Ours => &self.ours,
            Side::Theirs => &self.theirs,
        }
    }

    /// Changes which lines of `side` are picked with `change`. A side that
    /// gets its first tick while the other has none goes first; when the
    /// other has some, it stays first.
    fn pick(&mut self, side: Side, change: impl FnOnce(&mut Vec<bool>)) {
        let other = match side {
            Side::Ours => Side::Theirs,
            Side::Theirs => Side::Ours,
        };
        let had = self.picked(side).contains(&true);
        let other_has = self.picked(other).contains(&true);
        change(match side {
            Side::Ours => &mut self.ours,
            Side::Theirs => &mut self.theirs,
        });
        if !had && self.picked(side).contains(&true) {
            self.theirs_first = (side == Side::Theirs) != other_has;
        }
        self.edited = None;
        self.resolved = false;
    }

    fn is_empty(&self) -> bool {
        self.edited.is_none() && !self.ours.contains(&true) && !self.theirs.contains(&true)
    }
}

/// A file with conflicts still to resolve.
struct ConflictedFile {
    path: String,
    /// `None` when git left no conflict markers to work with: a binary
    /// file, or one deleted on one side. One side's file is taken whole.
    content: Option<ConflictFile>,
    resolutions: Vec<Resolution>,
}

impl ConflictedFile {
    fn conflict(&self, n: usize) -> Option<&Conflict> {
        self.content.as_ref()?.conflicts().nth(n)
    }

    fn conflict_count(&self) -> usize {
        self.resolutions.len()
    }

    fn unresolved(&self) -> usize {
        self.resolutions
            .iter()
            .filter(|resolution| !resolution.resolved)
            .count()
    }
}

/// Syntax highlighting of a conflicted file's versions, each numbered as
/// [`starts`] numbers it; `None` for a version that stays plain.
struct ConflictHighlights {
    /// The clean lines and our side of every conflict.
    ours: Option<Arc<Highlights>>,
    /// The clean lines and their side.
    theirs: Option<Arc<Highlights>>,
    /// The clean lines and the common ancestor's, or ours where git did
    /// not write it.
    base: Option<Arc<Highlights>>,
    /// Where each segment's lines start in the base, numbering from 1.
    base_starts: Vec<usize>,
}

impl ConflictHighlights {
    /// Highlights each version of `content`, at `path`. Blocks; run it in
    /// the background.
    fn new(path: &str, content: &ConflictFile) -> Self {
        let version = |side: fn(&Conflict) -> &[String]| {
            let mut text = String::new();
            let mut starts = Vec::with_capacity(content.segments.len());
            let mut next = 1;
            for segment in &content.segments {
                starts.push(next);
                let lines: &[String] = match segment {
                    Segment::Clean(lines) => lines,
                    Segment::Conflict(conflict) => side(conflict),
                };
                next += lines.len();
                for line in lines {
                    text.push_str(line);
                    text.push('\n');
                }
            }
            let highlights = gitez_highlight::highlight_file(path, text.as_bytes());
            (highlights.highlights().cloned(), starts)
        };
        let (ours, _) = version(|conflict| &conflict.ours);
        let (theirs, _) = version(|conflict| &conflict.theirs);
        let (base, base_starts) =
            version(|conflict| conflict.base.as_deref().unwrap_or(&conflict.ours));
        Self {
            ours,
            theirs,
            base,
            base_starts,
        }
    }
}

/// The syntax spans of line `number` (from 1) of a version highlighted as
/// `highlights`, which reads `text`.
fn spans<'a>(highlights: Option<&'a Highlights>, number: usize, text: &str) -> Option<&'a [Span]> {
    highlights?.line(u32::try_from(number).ok()?, text)
}

/// What reading the merge found.
struct Loaded {
    merge: MergeInProgress,
    files: Vec<ConflictedFile>,
    /// Files already merged, cleanly or resolved.
    merged: Vec<String>,
}

pub struct Conflicts {
    focus_handle: FocusHandle,
    repository: Repository,
    /// Where local pull requests are kept, to mark the one this merges as
    /// merged once committed.
    reviews: Store,
    pull_requests: Arc<Vec<PullRequest>>,
    /// `None` while reading; `Ok(None)` when no merge is in progress.
    loaded: Option<Result<Option<Loaded>, SharedString>>,
    current_file: usize,
    current_conflict: usize,
    show_base: bool,
    /// The result of the current conflict being typed.
    editing: Option<(Entity<TextareaState>, Subscription)>,
    /// The `git` command running. Only one runs at a time.
    running: Option<Task<()>>,
    /// Why the last command failed.
    error: Option<SharedString>,
    /// Syntax highlighting of the conflicted files, by path. Filled in the
    /// background after each read.
    highlights: HashMap<String, Arc<ConflictHighlights>>,
    /// The commits behind each conflicted file's conflicts, by path. Read
    /// in the background after each read.
    histories: HashMap<String, Result<Arc<story::FileHistory>, SharedString>>,
    /// The "Why these lines conflict" card is open.
    story_open: bool,
    /// The commit shown in that card: a side and an index into its
    /// commits.
    story_commit: Option<(Side, usize)>,
    _load: Task<()>,
    _highlight: Task<()>,
    _history: Task<()>,
}

impl EventEmitter<ConflictsEvent> for Conflicts {}

impl Focusable for Conflicts {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Conflicts {
    /// Reads the merge in progress in `repository` in the background.
    pub fn new(
        repository: Repository,
        reviews: Store,
        pull_requests: Arc<Vec<PullRequest>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            repository,
            reviews,
            pull_requests,
            loaded: None,
            current_file: 0,
            current_conflict: 0,
            show_base: false,
            editing: None,
            running: None,
            error: None,
            highlights: HashMap::new(),
            histories: HashMap::new(),
            story_open: true,
            story_commit: None,
            _load: Task::ready(()),
            _highlight: Task::ready(()),
            _history: Task::ready(()),
        };
        this.load(cx);
        this
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let repository = self.repository.clone();
        self._load = cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { read_merge(&repository) })
                .await
                .map_err(|error| format!("Could not read the merge: {error:#}").into());
            this.update(cx, |this, cx| {
                this.loaded = Some(loaded);
                this.current_file = 0;
                this.current_conflict = 0;
                this.editing = None;
                this.story_commit = None;
                this.load_highlights(cx);
                this.load_histories(cx);
                cx.notify();
            })
            .ok();
        });
    }

    /// Highlights the conflicted files just read in the background, then
    /// shows them coloured. Until then, the colours of the last read stay
    /// on the lines that did not change.
    fn load_highlights(&mut self, cx: &mut Context<Self>) {
        let files: Vec<(String, ConflictFile)> = match &self.loaded {
            Some(Ok(Some(loaded))) => loaded
                .files
                .iter()
                .filter_map(|file| Some((file.path.clone(), file.content.clone()?)))
                .collect(),
            _ => Vec::new(),
        };
        self.highlights
            .retain(|path, _| files.iter().any(|(shown, _)| shown == path));
        if files.is_empty() {
            // A run still going is for files that are gone.
            self._highlight = Task::ready(());
            return;
        }
        self._highlight = cx.spawn(async move |this, cx| {
            let highlighted = cx
                .background_executor()
                .spawn(async move {
                    files
                        .into_iter()
                        .map(|(path, content)| {
                            let highlights = ConflictHighlights::new(&path, &content);
                            (path, Arc::new(highlights))
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |this, cx| {
                this.highlights.extend(highlighted);
                cx.notify();
            })
            .ok();
        });
    }

    /// Reads, in the background, which commits made the conflicts of the
    /// files just read, then shows them.
    fn load_histories(&mut self, cx: &mut Context<Self>) {
        let Some(Ok(Some(loaded))) = &self.loaded else {
            self._history = Task::ready(());
            return;
        };
        let (ours, theirs) = (loaded.merge.ours, loaded.merge.theirs);
        let files: Vec<(String, ConflictFile)> = loaded
            .files
            .iter()
            .filter_map(|file| Some((file.path.clone(), file.content.clone()?)))
            .collect();
        self.histories
            .retain(|path, _| files.iter().any(|(shown, _)| shown == path));
        let repository = self.repository.clone();
        self._history = cx.spawn(async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move {
                    files
                        .into_iter()
                        .map(|(path, content)| {
                            let history = repository
                                .conflict_history(&path, &content, ours, theirs)
                                .map(|history| {
                                    Arc::new(story::FileHistory::new(&path, &content, history))
                                })
                                .map_err(|error| SharedString::from(error.to_string()));
                            (path, history)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |this, cx| {
                this.histories.extend(read);
                cx.notify();
            })
            .ok();
        });
    }

    fn loaded_mut(&mut self) -> Option<&mut Loaded> {
        match &mut self.loaded {
            Some(Ok(Some(loaded))) => Some(loaded),
            _ => None,
        }
    }

    fn git(&self) -> Option<Git> {
        self.repository.work_dir().map(Git::new)
    }

    /// Changes the current conflict's resolution with `change`.
    fn change(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Resolution, &Conflict)) {
        let (file, n) = (self.current_file, self.current_conflict);
        let Some(loaded) = self.loaded_mut() else {
            return;
        };
        let Some(file) = loaded.files.get_mut(file) else {
            return;
        };
        let Some(conflict) = file.content.as_ref().and_then(|c| c.conflicts().nth(n)) else {
            return;
        };
        let conflict = conflict.clone();
        if let Some(resolution) = file.resolutions.get_mut(n) {
            change(resolution, &conflict);
        }
        cx.notify();
    }

    /// Ticks every line of `side` into the result, or none of them.
    fn take_side(&mut self, side: Side, take: bool, cx: &mut Context<Self>) {
        self.editing = None;
        self.change(cx, |resolution, _| {
            resolution.pick(side, |picked| picked.fill(take));
        });
    }

    fn toggle_line(&mut self, side: Side, line: usize, cx: &mut Context<Self>) {
        self.change(cx, |resolution, _| {
            resolution.pick(side, |picked| {
                if let Some(picked) = picked.get_mut(line) {
                    *picked = !*picked;
                }
            });
        });
    }

    /// Starts typing the current conflict's result, from what it is now,
    /// with the cursor at the end of result line `line` (the last line when
    /// `None`).
    fn edit(&mut self, line: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let (file, n) = (self.current_file, self.current_conflict);
        let Some(Ok(Some(loaded))) = &self.loaded else {
            return;
        };
        let Some(file) = loaded.files.get(file) else {
            return;
        };
        let (Some(conflict), Some(resolution)) = (file.conflict(n), file.resolutions.get(n)) else {
            return;
        };
        let lines: Vec<String> = resolution
            .lines(conflict)
            .into_iter()
            .map(|(line, _, _)| line)
            .collect();
        let row = line
            .unwrap_or(usize::MAX)
            .min(lines.len().saturating_sub(1));
        let column = lines.get(row).map_or(0, |line| line.encode_utf16().count());
        let text = lines.join("\n");
        let input = cx.new(|cx| {
            let mut input = TextareaState::new(window, cx).auto_grow(3, 24);
            input.set_value(text, window, cx);
            input.set_cursor_position(Position::new(row as u32, column as u32), window, cx);
            input
        });
        let subscription = cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                let text = input.read(cx).value().to_string();
                this.change(cx, |resolution, _| {
                    resolution.edited = Some(text.split('\n').map(str::to_owned).collect());
                    resolution.resolved = false;
                });
            }
        });
        window.focus(&input.focus_handle(cx), cx);
        self.editing = Some((input, subscription));
        cx.notify();
    }

    /// Marks the current conflict resolved and moves on; the last one of
    /// a file writes it and stages it.
    fn mark_resolved(&mut self, cx: &mut Context<Self>) {
        self.editing = None;
        self.change(cx, |resolution, _| resolution.resolved = true);
        let current = self.current_file;
        let Some(loaded) = self.loaded_mut() else {
            return;
        };
        let Some(file) = loaded.files.get(current) else {
            return;
        };
        let next = (0..file.conflict_count()).find(|&n| !file.resolutions[n].resolved);
        match next {
            Some(n) => {
                self.current_conflict = n;
                self.story_commit = None;
                cx.notify();
            }
            None => self.save_file(cx),
        }
    }

    /// Writes the current file with every conflict resolved and stages it.
    fn save_file(&mut self, cx: &mut Context<Self>) {
        let current = self.current_file;
        let (Some(git), Some(dir)) = (self.git(), self.repository.work_dir().map(|d| d.to_owned()))
        else {
            return;
        };
        let Some(loaded) = self.loaded_mut() else {
            return;
        };
        let Some(file) = loaded.files.get(current) else {
            return;
        };
        let Some(content) = &file.content else {
            return;
        };
        let conflicts: Vec<&Conflict> = content.conflicts().collect();
        let text = content.resolve(|n| {
            file.resolutions[n]
                .lines(conflicts[n])
                .into_iter()
                .map(|(line, _, _)| line)
                .collect()
        });
        let path = file.path.clone();
        self.run(cx, move || {
            std::fs::write(dir.join(&path), text)
                .map_err(|error| anyhow::anyhow!("Could not write {path}: {error}"))?;
            git.stage(std::slice::from_ref(&path))?;
            Ok(())
        });
    }

    /// Resolves the current file by taking `side`'s whole file.
    fn take_file(&mut self, side: Side, cx: &mut Context<Self>) {
        let Some(git) = self.git() else {
            return;
        };
        let Some(Ok(Some(loaded))) = &self.loaded else {
            return;
        };
        let Some(file) = loaded.files.get(self.current_file) else {
            return;
        };
        let path = file.path.clone();
        self.run(cx, move || Ok(git.take_side(&[path], side)?));
    }

    /// Runs `command` in the background unless another one is running,
    /// then reads the merge again.
    fn run(
        &mut self,
        cx: &mut Context<Self>,
        command: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
    ) {
        if self.running.is_some() {
            return;
        }
        self.error = None;
        self.running = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { command() })
                .await;
            this.update(cx, |this, cx| {
                this.running = None;
                match result {
                    Ok(()) => {
                        // Keep the place: the resolved file leaves the list.
                        let file = this.current_file;
                        this.reload_keeping(file, cx);
                        cx.emit(ConflictsEvent::Changed);
                    }
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Reads the merge again, staying near file `file`; resolutions not
    /// yet saved are kept.
    fn reload_keeping(&mut self, file: usize, cx: &mut Context<Self>) {
        let repository = self.repository.clone();
        let kept: Vec<(String, Vec<Resolution>)> = match &self.loaded {
            Some(Ok(Some(loaded))) => loaded
                .files
                .iter()
                .map(|file| (file.path.clone(), file.resolutions.clone()))
                .collect(),
            _ => Vec::new(),
        };
        self._load = cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { read_merge(&repository) })
                .await;
            this.update(cx, |this, cx| {
                let loaded = loaded.map(|loaded| {
                    loaded.map(|mut loaded| {
                        for conflicted in &mut loaded.files {
                            if let Some((_, resolutions)) =
                                kept.iter().find(|(path, _)| *path == conflicted.path)
                                && resolutions.len() == conflicted.resolutions.len()
                            {
                                conflicted.resolutions = resolutions.clone();
                            }
                        }
                        loaded
                    })
                });
                let count = match &loaded {
                    Ok(Some(loaded)) => loaded.files.len(),
                    _ => 0,
                };
                this.current_file = file.min(count.saturating_sub(1));
                this.current_conflict = this.first_unresolved_conflict(&loaded);
                this.loaded = Some(
                    loaded.map_err(|error| format!("Could not read the merge: {error:#}").into()),
                );
                this.editing = None;
                this.story_commit = None;
                this.load_highlights(cx);
                this.load_histories(cx);
                cx.notify();
            })
            .ok();
        });
    }

    fn first_unresolved_conflict(&self, loaded: &anyhow::Result<Option<Loaded>>) -> usize {
        match loaded {
            Ok(Some(loaded)) => loaded
                .files
                .get(self.current_file)
                .and_then(|file| file.resolutions.iter().position(|r| !r.resolved))
                .unwrap_or(0),
            _ => 0,
        }
    }

    fn select_file(&mut self, index: usize, cx: &mut Context<Self>) {
        self.current_file = index;
        self.editing = None;
        self.story_commit = None;
        self.current_conflict = match &self.loaded {
            Some(Ok(Some(loaded))) => loaded
                .files
                .get(index)
                .and_then(|file| file.resolutions.iter().position(|r| !r.resolved))
                .unwrap_or(0),
            _ => 0,
        };
        cx.notify();
    }

    fn step_conflict(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(Ok(Some(loaded))) = &self.loaded else {
            return;
        };
        let Some(file) = loaded.files.get(self.current_file) else {
            return;
        };
        let count = file.conflict_count() as isize;
        let next = self.current_conflict as isize + delta;
        if (0..count).contains(&next) {
            self.current_conflict = next as usize;
            self.editing = None;
            self.story_commit = None;
            cx.notify();
        }
    }

    /// Commits the merge, and marks the local pull request it merges as
    /// merged.
    fn commit(&mut self, cx: &mut Context<Self>) {
        let Some(git) = self.git() else {
            return;
        };
        let Some(Ok(Some(loaded))) = &self.loaded else {
            return;
        };
        if !loaded.files.is_empty() || self.running.is_some() {
            return;
        }
        let merge = loaded.merge.clone();
        let pull_request = self
            .pull_requests
            .iter()
            .find(|pull_request| {
                pull_request.state == State::Open
                    && merge.branch.as_deref() == Some(pull_request.base.branch.as_str())
                    && merge.theirs_branch.as_deref() == Some(pull_request.head.branch.as_str())
            })
            .map(|pull_request| pull_request.number);
        let label = match (&pull_request, &merge.theirs_branch, &merge.branch) {
            (Some(number), _, Some(into)) => format!("Merge #{number} into {into}"),
            (_, Some(theirs), Some(into)) => format!("Merge {theirs} into {into}"),
            _ => "Commit the merge".to_owned(),
        };
        let (repository, store) = (self.repository.clone(), self.reviews.clone());
        self.error = None;
        self.running = Some(cx.spawn(async move |this, cx| {
            let committed = cx
                .background_executor()
                .spawn(async move {
                    let recorder = Recorder::start(git.work_dir());
                    git.commit_merge()?;
                    let mut recorded = recorder.finish(ResetMode::Keep);
                    if let Some(number) = pull_request {
                        let (_, review) =
                            super::review::record_merge(&repository, &store, number, merge.ours)?;
                        recorded = recorded.map(|recorded| recorded.with_review(review));
                    }
                    anyhow::Ok(recorded)
                })
                .await;
            this.update(cx, |this, cx| {
                this.running = None;
                match committed {
                    Ok(recorded) => {
                        let step = undo::record(cx, recorded, label);
                        cx.emit(ConflictsEvent::Finished {
                            committed: true,
                            step,
                        })
                    }
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn abort(&mut self, cx: &mut Context<Self>) {
        let Some(git) = self.git() else {
            return;
        };
        if self.running.is_some() {
            return;
        }
        self.error = None;
        self.running = Some(cx.spawn(async move |this, cx| {
            let aborted = cx
                .background_executor()
                .spawn(async move { git.abort_merge() })
                .await;
            this.update(cx, |this, cx| {
                this.running = None;
                match aborted {
                    Ok(()) => cx.emit(ConflictsEvent::Finished {
                        committed: false,
                        step: None,
                    }),
                    Err(error) => {
                        this.error = Some(format!("{:#}", anyhow::Error::from(error)).into())
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

/// The merge in progress, its conflicted files and those already merged.
/// Blocks; run it on a background thread.
fn read_merge(repository: &Repository) -> anyhow::Result<Option<Loaded>> {
    let Some(merge) = repository.merge_in_progress()? else {
        return Ok(None);
    };
    let status = repository.status()?;
    let mut files = Vec::new();
    let mut merged = Vec::new();
    for file in &status.files {
        if file.unstaged == Some(WorktreeChange::Conflicted) {
            let content = repository.conflict_file(&file.path)?;
            let resolutions = content
                .as_ref()
                .map(|content| content.conflicts().map(Resolution::new).collect())
                .unwrap_or_default();
            files.push(ConflictedFile {
                path: file.path.clone(),
                content,
                resolutions,
            });
        } else if file.staged.is_some() {
            merged.push(file.path.clone());
        }
    }
    Ok(Some(Loaded {
        merge,
        files,
        merged,
    }))
}

/// A branch chip in our colour or theirs.
fn side_chip(name: String, side: Side) -> Div {
    let tint = match side {
        Side::Ours => color::ACCENT,
        Side::Theirs => color::BRANCH_TEAL,
    };
    div()
        .flex_none()
        .h(px(20.))
        .px(px(7.))
        .rounded(px(4.))
        .flex()
        .items_center()
        .text_size(theme::SMALL_TEXT_SIZE)
        .font_weight(FontWeight::MEDIUM)
        .bg(theme::with_alpha(tint, 0.16))
        .text_color(rgb(match side {
            Side::Ours => color::CHIP_TEXT,
            Side::Theirs => color::PULL_REQUEST_OPEN_TEXT,
        }))
        .child(name)
}

/// The small tag naming a side: "OURS", "O", "T" or "=".
fn source_tag(text: &'static str, source: Source) -> Div {
    let (background, foreground) = match source {
        Source::Ours => (color::SOURCE_OURS, color::CHIP_TEXT),
        Source::Theirs => (color::SOURCE_THEIRS, color::PULL_REQUEST_OPEN_TEXT),
        Source::Both => (color::SOURCE_BOTH, color::TEXT_SECONDARY),
    };
    div()
        .flex_none()
        .px(px(4.))
        .rounded(px(3.))
        .text_size(px(10.))
        .line_height(px(15.))
        .font_weight(FontWeight::BOLD)
        .bg(rgb(background))
        .text_color(rgb(foreground))
        .child(text)
}

/// How much of a side a tick box stands for.
#[derive(Clone, Copy, PartialEq)]
enum Tick {
    None,
    Some,
    All,
}

/// A tick box in a side's colour: a tick when `All`, a dash when `Some`.
fn tick_box(id: impl Into<ElementId>, side: Side, tick: Tick) -> Stateful<Div> {
    let (fill, mark) = match side {
        Side::Ours => (color::ACCENT, color::ON_ACCENT),
        Side::Theirs => (color::BRANCH_TEAL, color::HEAD_BADGE_TEXT),
    };
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(16.))
        .rounded(px(4.))
        .border_1()
        .cursor_pointer()
        .map(|this| match tick {
            Tick::None => this.border_color(rgb(color::PICK_BORDER)),
            Tick::Some | Tick::All => this.bg(rgb(fill)).border_color(rgb(fill)).child(
                Icon::new(if tick == Tick::All {
                    IconName::Check
                } else {
                    IconName::Minus
                })
                .size(px(11.))
                .text_color(rgb(mark)),
            ),
        })
}

/// A pane around code: a header strip and lines.
fn pane() -> Div {
    div()
        .v_flex()
        .min_w_0()
        .rounded(px(9.))
        .border_1()
        .border_color(rgb(color::DIFF_CARD_BORDER))
        .bg(rgb(color::DIFF_CARD))
        .overflow_hidden()
}

fn pane_header() -> Div {
    div()
        .h_flex()
        .flex_wrap()
        .gap(px(8.))
        .px(px(12.))
        .py(px(8.))
        .bg(rgb(color::DIFF_FILE_HEADER))
        .border_b_1()
        .border_color(rgb(color::DIFF_CARD_BORDER))
        .text_size(px(12.5))
}

/// What clicking a row does.
type RowClick = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// A row of a pane: a key (a tick box or a tag), its number, and its text,
/// all on one background.
struct CodeRow {
    key: Option<AnyElement>,
    number: Option<usize>,
    text: AnyElement,
    background: Option<Rgba>,
    /// The commit that last changed the line, shown after the text.
    credit: Option<AnyElement>,
    /// An id for the row, and what clicking it does.
    click: Option<(ElementId, RowClick)>,
    /// The text cell's name for tests.
    debug: Option<SharedString>,
}

impl CodeRow {
    /// A line of code, coloured by its syntax `spans` if it has them.
    fn code(
        key: Option<AnyElement>,
        number: Option<usize>,
        text: &str,
        spans: Option<&[Span]>,
    ) -> Self {
        Self {
            key,
            number,
            text: code::styled_code(text, spans).into_any_element(),
            background: None,
            credit: None,
            click: None,
            debug: None,
        }
    }

    /// A row that is not code, such as a note that a side is empty.
    fn note(text: impl IntoElement) -> Self {
        Self {
            key: None,
            number: None,
            text: text.into_any_element(),
            background: None,
            credit: None,
            click: None,
            debug: None,
        }
    }

    fn bg(mut self, background: impl Into<Rgba>) -> Self {
        self.background = Some(background.into());
        self
    }

    fn credit(mut self, credit: Option<impl IntoElement>) -> Self {
        self.credit = credit.map(IntoElement::into_any_element);
        self
    }

    fn on_click(
        mut self,
        id: impl Into<ElementId>,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.click = Some((id.into(), Rc::new(handler)));
        self
    }

    fn debug(mut self, name: impl Into<SharedString>) -> Self {
        self.debug = Some(name.into());
        self
    }
}

/// A pane's rows: the keys and numbers stay put while the text scrolls
/// sideways, so long lines can be read whole; the scroller keeps each swipe
/// to one direction, so an upright one scrolls the page. `id` names the
/// scrolling part.
fn code_rows(id: impl Into<ElementId>, rows: Vec<CodeRow>, mono: &SharedString) -> Div {
    let has_credit = rows.iter().any(|row| row.credit.is_some());
    let height = rows.len() as f32;
    // Rows without numbers, such as a side's lines on their own, leave out
    // the number column.
    let numbered = rows.iter().any(|row| row.number.is_some());
    let mut keys = Vec::with_capacity(rows.len());
    let mut texts = Vec::with_capacity(rows.len());
    let mut credits = Vec::new();
    for row in rows {
        let background = row.background;
        let line = || {
            div()
                .h_flex()
                .h(LINE_HEIGHT)
                .when_some(background, |this, background| this.bg(background))
        };
        let key = line()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .w(px(28.))
                    .justify_center()
                    .children(row.key),
            )
            .when(numbered, |this| {
                this.child(
                    div()
                        .flex_none()
                        .w(px(40.))
                        .pr(px(10.))
                        .text_right()
                        .text_color(rgb(color::DIFF_LINE_NUMBER))
                        .children(row.number.map(|n| n.to_string())),
                )
            });
        let text = line()
            .pr(px(12.))
            .whitespace_nowrap()
            .text_color(rgb(color::DIFF_TEXT))
            .when_some(row.debug, |this, name| {
                this.debug_selector(move || name.to_string())
            })
            .child(row.text);
        match row.click {
            Some((id, handler)) => {
                let on_key = handler.clone();
                keys.push(
                    key.id(id.clone())
                        .cursor_text()
                        .on_click(move |event, window, cx| on_key(event, window, cx))
                        .into_any_element(),
                );
                texts.push(
                    text.id(id)
                        .cursor_text()
                        .on_click(move |event, window, cx| handler(event, window, cx))
                        .into_any_element(),
                );
            }
            None => {
                keys.push(key.into_any_element());
                texts.push(text.into_any_element());
            }
        }
        if has_credit {
            credits.push(line().children(row.credit).into_any_element());
        }
    }
    let id = id.into();
    div()
        .h_flex()
        .items_start()
        .py(px(6.))
        .font_family(mono.clone())
        .text_size(px(12.))
        .child(
            div()
                .id(ElementId::Name(format!("{id}-keys").into()))
                .v_flex()
                .flex_none()
                .children(keys),
        )
        .child(
            div()
                .id(id.clone())
                .flex_1()
                .min_w_0()
                // The scroll area fills this height: without it, it has
                // none, and no swipe reaches it.
                .h(LINE_HEIGHT * height)
                .h_flex()
                .items_start()
                .child(div().v_flex().flex_none().min_w_full().children(texts))
                .overflow_x_scrollbar()
                // Every pane is made here; each needs its own scroll position.
                .id(ElementId::Name(format!("{id}-scroll").into())),
        )
        .when(has_credit, |this| {
            this.child(div().v_flex().flex_none().children(credits))
        })
}

/// Where each segment's lines start in our file, their file and the
/// result, numbering from 1.
fn starts(file: &ConflictedFile) -> Vec<(usize, usize, usize)> {
    let Some(content) = &file.content else {
        return Vec::new();
    };
    let (mut ours, mut theirs, mut result) = (1, 1, 1);
    let mut n = 0;
    content
        .segments
        .iter()
        .map(|segment| {
            let start = (ours, theirs, result);
            match segment {
                Segment::Clean(lines) => {
                    ours += lines.len();
                    theirs += lines.len();
                    result += lines.len();
                }
                Segment::Conflict(conflict) => {
                    ours += conflict.ours.len();
                    theirs += conflict.theirs.len();
                    result += file.resolutions[n].lines(conflict).len();
                    n += 1;
                }
            }
            start
        })
        .collect()
}

impl Conflicts {
    fn render_header(&self, loaded: &Loaded, cx: &mut Context<Self>) -> impl IntoElement {
        let merge = &loaded.merge;
        let ours = merge.branch.clone().unwrap_or_else(|| "HEAD".into());
        let theirs = merge
            .theirs_branch
            .clone()
            .unwrap_or_else(|| merge.theirs.to_hex_with_len(7).to_string());
        let conflicts: usize = loaded.files.iter().map(|f| f.conflict_count()).sum();
        let resolved: usize = loaded
            .files
            .iter()
            .map(|f| f.conflict_count() - f.unresolved())
            .sum();
        let mut summary = vec![
            count(loaded.merged.len(), "file merged", "files merged") + " cleanly",
            match loaded.files.len() {
                0 => "nothing left to resolve".to_owned(),
                1 => "1 file needs you".to_owned(),
                n => format!("{n} files need you"),
            },
        ];
        if conflicts > 0 {
            summary.push(format!("{resolved} of {conflicts} conflicts resolved"));
        }
        let running = self.running.is_some();
        div()
            .h_flex()
            .flex_wrap()
            .gap(px(12.))
            .px(px(20.))
            .py(px(12.))
            .border_b_1()
            .border_color(rgb(color::BORDER))
            .bg(rgb(color::PANEL))
            .child(
                Button::new("conflicts-back")
                    .ghost()
                    .small()
                    .icon(IconName::ArrowLeft)
                    .tooltip("Back to the history; the merge stays in progress")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(ConflictsEvent::Close))),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(30.))
                    .rounded(px(8.))
                    .bg(rgb(color::SELECTED))
                    .child(
                        Icon::new(IconName::GitMerge)
                            .size(px(16.))
                            .text_color(rgb(color::ACCENT)),
                    ),
            )
            .child(
                div()
                    .v_flex()
                    .gap(px(2.))
                    .child(
                        div()
                            .h_flex()
                            .flex_wrap()
                            .gap(px(8.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Merging")
                            .child(side_chip(theirs, Side::Theirs))
                            .child("into")
                            .child(side_chip(ours, Side::Ours)),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(summary.join(" · ")),
                    ),
            )
            .child(
                div()
                    .ml_auto()
                    .h_flex()
                    .gap(px(6.))
                    .child(
                        Button::new("conflicts-abort")
                            .ghost()
                            .label("Abort merge")
                            .text_color(rgb(color::DIFF_REMOVED))
                            .disabled(running)
                            .tooltip("Give up the merge: everything returns to how it was")
                            .on_click(cx.listener(|this, _, _, cx| this.abort(cx))),
                    )
                    .child(
                        Button::new("conflicts-commit")
                            .primary()
                            .label("Commit merge")
                            .disabled(!loaded.files.is_empty() || running)
                            .loading(running && loaded.files.is_empty())
                            .on_click(cx.listener(|this, _, _, cx| this.commit(cx))),
                    ),
            )
    }

    fn render_files(&self, loaded: &Loaded, cx: &mut Context<Self>) -> impl IntoElement {
        let heading = |text: &'static str| {
            div()
                .flex_none()
                .px(px(10.))
                .pt(px(14.))
                .pb(px(8.))
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(color::TEXT_FAINT))
                .child(text.to_uppercase())
        };
        let name = |path: &str| path.rsplit('/').next().unwrap_or(path).to_owned();
        let needs_you = loaded.files.iter().enumerate().map(|(index, file)| {
            let current = index == self.current_file;
            let left = match &file.content {
                Some(_) => format!("{} left", file.unresolved()),
                None => "whole file".into(),
            };
            div()
                .id(("conflicted-file", index))
                .h_flex()
                .flex_none()
                .gap(px(8.))
                .h(px(32.))
                .px(px(10.))
                .rounded(px(6.))
                .text_size(px(12.5))
                .cursor_pointer()
                .when(current, |this| {
                    this.bg(rgb(color::BUTTON)).text_color(rgb(color::TEXT))
                })
                .when(!current, |this| {
                    this.text_color(rgb(color::TEXT_SECONDARY))
                        .hover(|style| style.bg(rgb(color::NAV_HOVER)))
                })
                .child(
                    Icon::new(IconName::CircleAlert)
                        .size(px(13.))
                        .text_color(rgb(color::ACCENT)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(name(&file.path)),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(rgb(color::CHIP_TEXT))
                        .child(left),
                )
                .tooltip({
                    let path: SharedString = file.path.clone().into();
                    move |window, cx| Tooltip::new(path.clone()).build(window, cx)
                })
                .on_click(cx.listener(move |this, _, _, cx| this.select_file(index, cx)))
        });
        let merged = loaded.merged.iter().map(|path| {
            div()
                .h_flex()
                .flex_none()
                .gap(px(8.))
                .h(px(32.))
                .px(px(10.))
                .text_size(px(12.5))
                .text_color(rgb(color::TEXT_MUTED))
                .child(
                    Icon::new(IconName::Check)
                        .size(px(13.))
                        .text_color(rgb(color::DIFF_ADDED)),
                )
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(name(path)),
                )
        });
        div()
            .id("conflict-files")
            .v_flex()
            .flex_none()
            .w(px(230.))
            .h_full()
            .overflow_y_scroll()
            .gap(px(2.))
            .when(!loaded.files.is_empty(), |this| {
                this.child(heading("Needs you")).children(needs_you)
            })
            .when(!loaded.merged.is_empty(), |this| {
                this.child(heading("Merged")).children(merged)
            })
    }

    fn render_file(
        &self,
        loaded: &Loaded,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(file) = loaded.files.get(self.current_file) else {
            return div()
                .v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap(px(6.))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Every conflict is resolved"),
                )
                .child(
                    div()
                        .text_color(rgb(color::TEXT_MUTED))
                        .child("Commit the merge to finish it."),
                )
                .into_any_element();
        };
        let merge = &loaded.merge;
        let ours_name = merge.branch.clone().unwrap_or_else(|| "HEAD".into());
        let theirs_name = merge
            .theirs_branch
            .clone()
            .unwrap_or_else(|| merge.theirs.to_hex_with_len(7).to_string());
        let mono = cx.theme().mono_font_family.clone();
        let (dir, name) = match file.path.rsplit_once('/') {
            Some((dir, name)) => (format!("{dir}/"), name.to_owned()),
            None => (String::new(), file.path.clone()),
        };
        let title = div()
            .h_flex()
            .font_family(mono.clone())
            .text_size(px(13.))
            .child(div().text_color(rgb(color::TEXT_MUTED)).child(dir))
            .child(name);

        let Some(content) = &file.content else {
            let running = self.running.is_some();
            return div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(12.))
                .child(title)
                .child(div().text_color(rgb(color::TEXT_SECONDARY)).child(
                    "This file cannot be merged line by line: it is binary, or one \
                             side deleted it. Keep one side's version.",
                ))
                .child(
                    div()
                        .h_flex()
                        .gap(px(6.))
                        .child(
                            Button::new("take-ours-file")
                                .label(format!("Take {ours_name}"))
                                .text_color(rgb(color::CHIP_TEXT))
                                .disabled(running)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.take_file(Side::Ours, cx)),
                                ),
                        )
                        .child(
                            Button::new("take-theirs-file")
                                .label(format!("Take {theirs_name}"))
                                .text_color(rgb(color::PULL_REQUEST_OPEN_TEXT))
                                .disabled(running)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.take_file(Side::Theirs, cx)),
                                ),
                        ),
                )
                .into_any_element();
        };

        let n = self
            .current_conflict
            .min(file.conflict_count().saturating_sub(1));
        let segment_index = content
            .segments
            .iter()
            .enumerate()
            .filter(|(_, segment)| matches!(segment, Segment::Conflict(_)))
            .nth(n)
            .map(|(index, _)| index)
            .unwrap_or(0);
        let Some(Segment::Conflict(conflict)) = content.segments.get(segment_index) else {
            return div().into_any_element();
        };
        let resolution = &file.resolutions[n];
        let starts = starts(file);
        let (ours_start, theirs_start, result_start) = starts[segment_index];
        let before: &[String] = match segment_index.checked_sub(1).map(|i| &content.segments[i]) {
            Some(Segment::Clean(lines)) => &lines[lines.len().saturating_sub(CONTEXT)..],
            _ => &[],
        };
        let after: &[String] = match content.segments.get(segment_index + 1) {
            Some(Segment::Clean(lines)) => &lines[..lines.len().min(CONTEXT)],
            _ => &[],
        };
        let highlights = self.highlights.get(&file.path);
        let ours_highlights = highlights.and_then(|highlights| highlights.ours.as_deref());
        let theirs_highlights = highlights.and_then(|highlights| highlights.theirs.as_deref());
        let base_highlights = highlights.and_then(|highlights| highlights.base.as_deref());
        let base_start = highlights
            .and_then(|highlights| highlights.base_starts.get(segment_index))
            .copied()
            .unwrap_or(0);
        // Clean lines are the same on both sides, numbered on each.
        let context_lines = |start: usize, lines: &[String], highlights: Option<&Highlights>| {
            lines
                .iter()
                .enumerate()
                .map(|(i, line)| {
                    CodeRow::code(
                        None,
                        Some(start + i),
                        line,
                        spans(highlights, start + i, line),
                    )
                })
                .collect::<Vec<_>>()
        };

        let history = self.histories.get(&file.path);
        // The commit that last changed line `i` of `side` in this conflict.
        let credit =
            |side: Side, i: usize| {
                let Some(Ok(history)) = history else {
                    return None;
                };
                let side_history = match side {
                    Side::Ours => &history.history.ours,
                    Side::Theirs => &history.history.theirs,
                };
                let index = (*side_history.conflicts.get(n)?.lines.get(i)?)?;
                let commit = side_history.commits.get(index)?;
                let summary: SharedString = commit.summary.clone().into();
                let id = match side {
                    Side::Ours => ("credit-ours", i),
                    Side::Theirs => ("credit-theirs", i),
                };
                Some(
                    story::line_credit(side, commit)
                        .debug_selector(move || format!("conflict-{}-{}", id.0, id.1))
                        .id(id)
                        .cursor_pointer()
                        .tooltip(move |window, cx| Tooltip::new(summary.clone()).build(window, cx))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.choose_story_commit(side, index, cx)
                        })),
                )
            };
        let side_lines = |side: Side, lines: &[String], picked: &[bool], start: usize| {
            let (faint, strong) = match side {
                Side::Ours => (color::CONFLICT_OURS, color::CONFLICT_OURS_PICKED),
                Side::Theirs => (color::CONFLICT_THEIRS, color::CONFLICT_THEIRS_PICKED),
            };
            let highlights = match side {
                Side::Ours => ours_highlights,
                Side::Theirs => theirs_highlights,
            };
            lines
                .iter()
                .enumerate()
                .map(|(i, line)| {
                    let on = picked.get(i).copied().unwrap_or(false);
                    let id = match side {
                        Side::Ours => ("pick-ours", i),
                        Side::Theirs => ("pick-theirs", i),
                    };
                    let tick_box = tick_box(id, side, if on { Tick::All } else { Tick::None })
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_line(side, i, cx)));
                    CodeRow::code(
                        Some(tick_box.into_any_element()),
                        Some(start + i),
                        line,
                        spans(highlights, start + i, line),
                    )
                    .bg(rgba(if on { strong } else { faint }))
                    .credit(credit(side, i))
                    .debug(match side {
                        Side::Ours => format!("conflict-ours-line-{i}"),
                        Side::Theirs => format!("conflict-theirs-line-{i}"),
                    })
                })
                .collect::<Vec<_>>()
        };

        let show_base = self.show_base && conflict.base.is_some();
        let sha = |oid: &gitez_git::Oid| oid.to_hex_with_len(7).to_string();
        // The tick box over a side's lines takes all of them, or none;
        // with lines of both sides picked, the side ticked first goes first.
        let both_picked = resolution.edited.is_none()
            && resolution.ours.contains(&true)
            && resolution.theirs.contains(&true);
        let side_tick = |side: Side, name: &str| {
            let picked = resolution.picked(side);
            let tick = if picked.is_empty() || !picked.contains(&true) {
                Tick::None
            } else if picked.iter().all(|on| *on) {
                Tick::All
            } else {
                Tick::Some
            };
            let take = tick != Tick::All;
            let tooltip: SharedString = if take {
                format!("Take every line of {name}").into()
            } else {
                format!("Take none of {name}").into()
            };
            let id = match side {
                Side::Ours => "take-ours",
                Side::Theirs => "take-theirs",
            };
            tick_box(id, side, tick)
                .when(picked.is_empty(), |this| this.opacity(0.4).cursor_default())
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                .when(!picked.is_empty(), |this| {
                    this.on_click(cx.listener(move |this, _, _, cx| this.take_side(side, take, cx)))
                })
        };
        let order = |side: Side| {
            let first = (side == Side::Theirs) == resolution.theirs_first;
            div()
                .ml_auto()
                .flex_none()
                .whitespace_nowrap()
                .text_size(px(11.5))
                .text_color(rgb(color::TEXT_MUTED))
                .child(if first { "goes first" } else { "goes second" })
        };
        let ours_pane = pane()
            .child(
                pane_header()
                    .pl(px(6.))
                    .child(side_tick(Side::Ours, &ours_name))
                    .child(source_tag("OURS", Source::Ours))
                    .child(side_chip(ours_name.clone(), Side::Ours))
                    .child(
                        div()
                            .font_family(mono.clone())
                            .text_size(theme::SMALL_TEXT_SIZE)
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(sha(&merge.ours)),
                    )
                    .when(both_picked, |this| this.child(order(Side::Ours))),
            )
            .child(code_rows(
                "ours-lines",
                [
                    context_lines(ours_start - before.len(), before, ours_highlights),
                    side_lines(Side::Ours, &conflict.ours, &resolution.ours, ours_start),
                    conflict
                        .ours
                        .is_empty()
                        .then(|| CodeRow::note(empty_note("Nothing here on this side")))
                        .into_iter()
                        .collect(),
                    context_lines(ours_start + conflict.ours.len(), after, ours_highlights),
                ]
                .into_iter()
                .flatten()
                .collect(),
                &mono,
            ));
        let theirs_pane = pane()
            .child(
                pane_header()
                    .pl(px(6.))
                    .child(side_tick(Side::Theirs, &theirs_name))
                    .child(source_tag("THEIRS", Source::Theirs))
                    .child(side_chip(theirs_name.clone(), Side::Theirs))
                    .child(
                        div()
                            .font_family(mono.clone())
                            .text_size(theme::SMALL_TEXT_SIZE)
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(sha(&merge.theirs)),
                    )
                    .when(both_picked, |this| this.child(order(Side::Theirs))),
            )
            .child(code_rows(
                "theirs-lines",
                [
                    context_lines(theirs_start - before.len(), before, theirs_highlights),
                    side_lines(
                        Side::Theirs,
                        &conflict.theirs,
                        &resolution.theirs,
                        theirs_start,
                    ),
                    conflict
                        .theirs
                        .is_empty()
                        .then(|| CodeRow::note(empty_note("Nothing here on this side")))
                        .into_iter()
                        .collect(),
                    context_lines(
                        theirs_start + conflict.theirs.len(),
                        after,
                        theirs_highlights,
                    ),
                ]
                .into_iter()
                .flatten()
                .collect(),
                &mono,
            ));
        let base_pane = conflict.base.as_ref().filter(|_| show_base).map(|base| {
            pane()
                .child(
                    pane_header().child(source_tag("BASE", Source::Both)).child(
                        div()
                            .text_color(rgb(color::TEXT_MUTED))
                            .child("Before either side changed it"),
                    ),
                )
                .child(code_rows(
                    "base-lines",
                    base.iter()
                        .enumerate()
                        .map(|(i, line)| {
                            CodeRow::code(
                                None,
                                None,
                                line,
                                spans(base_highlights, base_start + i, line),
                            )
                        })
                        .chain(
                            base.is_empty()
                                .then(|| CodeRow::note(empty_note("Nothing here before"))),
                        )
                        .collect(),
                    &mono,
                ))
        });

        let result_lines = resolution.lines(conflict);
        let from_ours = result_lines
            .iter()
            .filter(|(_, s, _)| *s == Source::Ours)
            .count();
        let from_theirs = result_lines
            .iter()
            .filter(|(_, s, _)| *s == Source::Theirs)
            .count();
        let state = if resolution.resolved {
            ("RESOLVED", color::DIFF_ADDED_TEXT)
        } else {
            ("IN PROGRESS", color::CHIP_TEXT)
        };
        let built = if resolution.edited.is_some() {
            format!("Conflict {} is edited by hand.", n + 1)
        } else if resolution.is_empty() {
            "Tick the lines to keep, or a side's box to take all of it, or edit the result."
                .to_owned()
        } else {
            let parts: Vec<String> = [(from_ours, &ours_name), (from_theirs, &theirs_name)]
                .into_iter()
                .filter(|(lines, _)| *lines > 0)
                .map(|(lines, name)| format!("{} of {name}", count(lines, "line", "lines")))
                .collect();
            format!("Conflict {} is built from {}.", n + 1, parts.join(" and "))
        };
        let tagged = |source: Source| match source {
            Source::Ours => source_tag("O", source),
            Source::Theirs => source_tag("T", source),
            Source::Both => source_tag("=", source),
        };
        let editing = self.editing.as_ref().map(|(input, _)| input.clone());
        let before_rows = || {
            before
                .iter()
                .enumerate()
                .map(|(i, line)| {
                    let ours = ours_start - before.len() + i;
                    CodeRow::code(
                        Some(tagged(Source::Both).into_any_element()),
                        Some(result_start - before.len() + i),
                        line,
                        spans(ours_highlights, ours, line),
                    )
                })
                .collect::<Vec<_>>()
        };
        let after_rows = || {
            after
                .iter()
                .enumerate()
                .map(|(i, line)| {
                    let ours = ours_start + conflict.ours.len() + i;
                    CodeRow::code(
                        Some(tagged(Source::Both).into_any_element()),
                        Some(result_start + result_lines.len() + i),
                        line,
                        spans(ours_highlights, ours, line),
                    )
                })
                .collect::<Vec<_>>()
        };
        let label = div()
            .text_size(px(10.5))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(state.1))
            .child(format!("CONFLICT {} · {}", n + 1, state.0));
        let result_body = match &editing {
            Some(input) => div()
                .v_flex()
                .child(code_rows("result-before", before_rows(), &mono))
                .child(div().px(px(12.)).child(label))
                .child(
                    div()
                        .px(px(12.))
                        .py(px(4.))
                        .child(Textarea::new(input).font_family(mono.clone())),
                )
                .child(code_rows("result-after", after_rows(), &mono)),
            None => {
                let picked = result_lines
                    .iter()
                    .enumerate()
                    .map(|(i, (line, source, origin))| {
                        // A picked line is coloured as on its side.
                        let line_spans = match (source, origin) {
                            (Source::Ours, Some(index)) => {
                                spans(ours_highlights, ours_start + index, line)
                            }
                            (Source::Theirs, Some(index)) => {
                                spans(theirs_highlights, theirs_start + index, line)
                            }
                            _ => None,
                        };
                        CodeRow::code(
                            Some(tagged(*source).into_any_element()),
                            Some(result_start + i),
                            line,
                            line_spans,
                        )
                        .on_click(
                            ("result-line", i),
                            cx.listener(move |this, _, window, cx| this.edit(Some(i), window, cx)),
                        )
                        .debug(format!("conflict-result-line-{i}"))
                        .bg(rgba(match (resolution.resolved, source) {
                            (true, _) => color::CONFLICT_RESOLVED,
                            (false, Source::Ours) => color::CONFLICT_OURS_PICKED,
                            (false, Source::Theirs) => color::CONFLICT_THEIRS_PICKED,
                            (false, Source::Both) => color::CONFLICT_RESOLVED,
                        }))
                    });
                let empty = result_lines.is_empty().then(|| {
                    CodeRow::note(empty_note("Nothing yet: the conflict's lines are removed"))
                        .on_click(
                            "result-empty",
                            cx.listener(|this, _, window, cx| this.edit(None, window, cx)),
                        )
                });
                let rows = before_rows()
                    .into_iter()
                    .chain([CodeRow::note(label)])
                    .chain(picked)
                    .chain(empty)
                    .chain(after_rows())
                    .collect();
                code_rows("result-lines", rows, &mono)
            }
        };
        let result_pane = pane()
            .child(
                pane_header()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Result"))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child("Tick lines above, or click a line to edit it."),
                    )
                    .when(editing.is_none(), |this| {
                        this.child(
                            Button::new("result-edit-header")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Pencil)
                                .label("Edit")
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.edit(None, window, cx)),
                                ),
                        )
                    })
                    .child(
                        div()
                            .ml_auto()
                            .h_flex()
                            .gap(px(10.))
                            .text_size(theme::SMALL_TEXT_SIZE)
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(
                                div()
                                    .h_flex()
                                    .gap(px(5.))
                                    .child(tagged(Source::Ours))
                                    .child(ours_name.clone()),
                            )
                            .child(
                                div()
                                    .h_flex()
                                    .gap(px(5.))
                                    .child(tagged(Source::Theirs))
                                    .child(theirs_name.clone()),
                            )
                            .child(
                                div()
                                    .h_flex()
                                    .gap(px(5.))
                                    .child(tagged(Source::Both))
                                    .child("both"),
                            ),
                    ),
            )
            .child(result_body)
            .child(
                div()
                    .h_flex()
                    .flex_wrap()
                    .gap(px(10.))
                    .px(px(12.))
                    .py(px(10.))
                    .border_t_1()
                    .border_color(rgb(color::DIFF_CARD_BORDER))
                    .bg(rgb(color::RESULT_FOOTER))
                    .debug_selector(|| "conflict-result-footer".into())
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(built),
                    )
                    .child(
                        div()
                            .ml_auto()
                            .h_flex()
                            .gap(px(6.))
                            .child(if editing.is_some() {
                                Button::new("result-done-editing")
                                    .ghost()
                                    .label("Done editing")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.editing = None;
                                        cx.notify();
                                    }))
                            } else {
                                Button::new("result-edit")
                                    .ghost()
                                    .icon(IconName::Pencil)
                                    .label("Edit")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.edit(None, window, cx)
                                    }))
                            })
                            .child(
                                Button::new("result-resolved")
                                    .primary()
                                    .label(if file.unresolved() == 1 && !resolution.resolved {
                                        "Mark resolved and save file"
                                    } else {
                                        "Mark conflict resolved"
                                    })
                                    .disabled(resolution.resolved || self.running.is_some())
                                    .loading(self.running.is_some())
                                    .on_click(cx.listener(|this, _, _, cx| this.mark_resolved(cx))),
                            ),
                    ),
            );

        let _ = window;
        let story = self.render_story(
            history,
            (
                story::SideView {
                    name: &ours_name,
                    highlights: ours_highlights,
                    start: ours_start,
                },
                story::SideView {
                    name: &theirs_name,
                    highlights: theirs_highlights,
                    start: theirs_start,
                },
            ),
            n,
            conflict,
            &mono,
            cx,
        );
        div()
            .id("conflict-file")
            .v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            // A sideways swipe over the history's lanes scrolls them, not
            // the page.
            .restrict_scroll_to_axis()
            .gap(px(12.))
            .pb(px(24.))
            // Children keep their height and the column scrolls, rather than
            // squeezing them into the window.
            .child(
                div()
                    .h_flex()
                    .flex_none()
                    .flex_wrap()
                    .gap(px(8.))
                    .child(title)
                    .child(
                        div()
                            .ml(px(8.))
                            .text_size(px(12.5))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(format!("Conflict {} of {}", n + 1, file.conflict_count())),
                    )
                    .child(
                        Button::new("conflict-previous")
                            .ghost()
                            .small()
                            .icon(IconName::ChevronUp)
                            .tooltip("Previous conflict")
                            .disabled(n == 0)
                            .on_click(cx.listener(|this, _, _, cx| this.step_conflict(-1, cx))),
                    )
                    .child(
                        Button::new("conflict-next")
                            .ghost()
                            .small()
                            .icon(IconName::ChevronDown)
                            .tooltip("Next conflict")
                            .disabled(n + 1 >= file.conflict_count())
                            .on_click(cx.listener(|this, _, _, cx| this.step_conflict(1, cx))),
                    )
                    .child(div().ml_auto().h_flex().flex_wrap().gap(px(6.)).when(
                        conflict.base.is_some(),
                        |this| {
                            this.child(
                                Checkbox::new("show-base")
                                    .label("Show base")
                                    .checked(self.show_base)
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        this.show_base = *checked;
                                        cx.notify();
                                    })),
                            )
                        },
                    )),
            )
            .child(story)
            .child(
                div()
                    .flex_none()
                    .grid()
                    .grid_cols(if base_pane.is_some() { 3 } else { 2 })
                    .gap(px(12.))
                    .child(ours_pane)
                    .children(base_pane)
                    .child(theirs_pane),
            )
            .child(result_pane.flex_none())
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .flex_none()
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(theme::RADIUS)
                        .bg(rgb(color::THREAD))
                        .font_family(mono)
                        .text_size(theme::SMALL_TEXT_SIZE)
                        .text_color(rgb(color::DIFF_REMOVED))
                        .child(error),
                )
            })
            .into_any_element()
    }
}

/// A note in place of lines, such as that a side has none.
fn empty_note(text: &'static str) -> Div {
    div()
        .text_size(px(12.))
        .text_color(rgb(color::TEXT_FAINT))
        .child(text)
}

fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

impl Render for Conflicts {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .track_focus(&self.focus_handle)
            .v_flex()
            .size_full()
            .bg(rgb(color::BACKGROUND));
        let message = |title: SharedString, detail: SharedString| {
            div()
                .v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap(px(6.))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                .child(div().text_color(rgb(color::TEXT_MUTED)).child(detail))
        };
        let loaded = match &self.loaded {
            None => return root.child(message("Reading the merge…".into(), "".into())),
            Some(Err(error)) => {
                return root.child(message("Could not read the merge".into(), error.clone()));
            }
            Some(Ok(None)) => {
                return root.child(
                    message(
                        "No merge in progress".into(),
                        "It was committed or given up.".into(),
                    )
                    .child(
                        Button::new("conflicts-close")
                            .label("Back to the history")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ConflictsEvent::Close))),
                    ),
                );
            }
            Some(Ok(Some(loaded))) => loaded,
        };
        let header = self.render_header(loaded, cx).into_any_element();
        let files = self.render_files(loaded, cx).into_any_element();
        let file = self.render_file(loaded, window, cx);
        root.child(header).child(
            div()
                .flex()
                .flex_row()
                .flex_1()
                .min_h_0()
                .gap(px(16.))
                .px(px(20.))
                .pt(px(16.))
                .child(files)
                .child(file),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use gitez_git::Repository;
    use gitez_review::Store;
    use gpui_kit::{
        Entity, Modifiers, ScrollDelta, ScrollWheelEvent, TestAppContext, TouchPhase,
        VisualTestContext, point, px, size,
    };

    use gitez_highlight::{Highlights, TokenKind};

    use super::Conflicts;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(
            status.status.success() || args[0] == "merge",
            "git {args:?}: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    /// A repository stopped in a merge whose one conflict has `lines` lines
    /// on each side.
    fn conflicted_repository(lines: usize) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        let text = |side: &str| {
            (0..lines)
                .map(|n| format!("{side} line {n}\n"))
                .collect::<String>()
        };
        git(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("file.txt"), "base\n").expect("write");
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        git(root, &["checkout", "-qb", "theirs"]);
        std::fs::write(root.join("file.txt"), text("theirs")).expect("write");
        git(root, &["commit", "-qam", "theirs"]);
        git(root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("file.txt"), text("ours")).expect("write");
        git(root, &["commit", "-qam", "ours"]);
        git(root, &["merge", "-q", "theirs"]);
        dir
    }

    fn open(dir: &Path, cx: &mut TestAppContext) -> (Entity<Conflicts>, VisualTestContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::init(cx);
        });
        let repository = Repository::open(dir).expect("open repository");
        let reviews = Store::new(&repository.common_dir());
        let window = cx.open_window(size(px(1000.), px(500.)), |_, cx| {
            Conflicts::new(repository, reviews, Default::default(), cx)
        });
        let view = window
            .update(cx, |_, _, cx| cx.entity())
            .expect("window open");
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        // Reading the merge happens in the background.
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (view, cx)
    }

    fn scroll_offset(cx: &mut VisualTestContext) -> f32 {
        let bounds = cx
            .debug_bounds("conflict-result-footer")
            .expect("result footer drawn");
        f32::from(bounds.top())
    }

    #[gpui_kit::test]
    fn conflicted_files_are_highlighted(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.rs"), "/* base */\nfn f() {}\n").expect("write");
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        git(root, &["checkout", "-qb", "theirs"]);
        std::fs::write(root.join("a.rs"), "/* base */\nfn g() {}\n").expect("write");
        git(root, &["commit", "-qam", "theirs"]);
        git(root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("a.rs"), "/* base */\nfn h() {}\n").expect("write");
        git(root, &["commit", "-qam", "ours"]);
        git(root, &["merge", "-q", "theirs"]);

        let (view, cx) = open(root, cx);
        view.read_with(&cx, |view, _| {
            let highlights = view.highlights.get("a.rs").expect("a.rs is highlighted");
            let first_kind = |highlights: Option<&Highlights>, number, text| {
                super::spans(highlights, number, text).map(|spans| spans[0].kind)
            };
            let ours = highlights.ours.as_deref();
            let theirs = highlights.theirs.as_deref();
            assert_eq!(first_kind(ours, 1, "/* base */"), Some(TokenKind::Comment));
            assert_eq!(first_kind(ours, 2, "fn h() {}"), Some(TokenKind::Keyword));
            assert_eq!(first_kind(theirs, 2, "fn g() {}"), Some(TokenKind::Keyword));

            // What each side's commit did to the lines, under the history.
            let Some(Ok(file)) = view.histories.get("a.rs") else {
                panic!("the history is read");
            };
            let side = &file.history.theirs;
            let (index, lines) = &side.conflicts[0].touched_by[0];
            let change = file
                .change(gitez_command::Side::Theirs, 0, *index)
                .expect("the change is highlighted");
            let kinds: Vec<(String, Option<TokenKind>)> = lines
                .iter()
                .map(|line| {
                    let kind = change
                        .spans(line)
                        .and_then(|spans| spans.first())
                        .map(|s| s.kind);
                    (line.text.clone(), kind)
                })
                .collect();
            assert!(
                kinds.contains(&("fn f() {}".to_owned(), Some(TokenKind::Keyword)))
                    && kinds.contains(&("fn g() {}".to_owned(), Some(TokenKind::Keyword))),
                "{kinds:?}"
            );
        });
    }

    #[test]
    fn result_lines_know_which_line_of_their_side_they_are() {
        let conflict = gitez_git::Conflict {
            ours: vec!["a".into(), "b".into()],
            base: None,
            theirs: vec!["c".into(), "d".into()],
        };
        let resolution = super::Resolution {
            ours: vec![false, true],
            theirs: vec![true, true],
            theirs_first: true,
            ..super::Resolution::default()
        };
        assert_eq!(
            resolution.lines(&conflict),
            [
                ("c".to_owned(), super::Source::Theirs, Some(0)),
                ("d".to_owned(), super::Source::Theirs, Some(1)),
                ("b".to_owned(), super::Source::Ours, Some(1)),
            ]
        );
    }

    #[test]
    fn the_side_ticked_first_goes_first() {
        use gitez_command::Side;
        let conflict = gitez_git::Conflict {
            ours: vec!["a".into(), "b".into()],
            base: None,
            theirs: vec!["c".into()],
        };
        let texts = |resolution: &super::Resolution| -> Vec<String> {
            resolution
                .lines(&conflict)
                .into_iter()
                .map(|(text, _, _)| text)
                .collect()
        };
        let mut resolution = super::Resolution::new(&conflict);
        // A line of theirs first, then all of ours: theirs stays first.
        resolution.pick(Side::Theirs, |picked| picked[0] = true);
        resolution.pick(Side::Ours, |picked| picked.fill(true));
        assert_eq!(texts(&resolution), ["c", "a", "b"]);
        // Dropping theirs and ticking it again puts it after ours.
        resolution.pick(Side::Theirs, |picked| picked.fill(false));
        resolution.pick(Side::Theirs, |picked| picked.fill(true));
        assert_eq!(texts(&resolution), ["a", "b", "c"]);
        // Starting over from nothing, ours line by line, then theirs.
        let mut resolution = super::Resolution::new(&conflict);
        resolution.pick(Side::Ours, |picked| picked[1] = true);
        resolution.pick(Side::Theirs, |picked| picked.fill(true));
        resolution.pick(Side::Ours, |picked| picked[0] = true);
        assert_eq!(texts(&resolution), ["a", "b", "c"]);
    }

    #[gpui_kit::test]
    fn a_long_conflict_scrolls_to_its_result(cx: &mut TestAppContext) {
        let dir = conflicted_repository(40);
        let (_, mut cx) = open(dir.path(), cx);
        let before = scroll_offset(&mut cx);
        assert!(
            before > 500.,
            "the footer starts below the window: {before}"
        );
        for _ in 0..40 {
            cx.simulate_event(ScrollWheelEvent {
                position: point(px(600.), px(300.)),
                delta: ScrollDelta::Pixels(point(px(0.), px(-200.))),
                touch_phase: TouchPhase::Moved,
                modifiers: Modifiers::default(),
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
        }
        let after = scroll_offset(&mut cx);
        assert!(
            after < 500.,
            "scrolling brings the footer into view: {after}"
        );
    }

    #[gpui_kit::test]
    fn the_commits_behind_a_conflict_are_shown(cx: &mut TestAppContext) {
        let dir = conflicted_repository(2);
        let (view, mut cx) = open(dir.path(), cx);
        let (ours, theirs) = view.read_with(&cx, |view, _| {
            let Some(Ok(history)) = view.histories.get("file.txt") else {
                panic!("the history is read");
            };
            let touched = |side: &gitez_git::SideHistory| -> Vec<String> {
                side.conflicts[0]
                    .touched_by
                    .iter()
                    .map(|(index, _)| side.commits[*index].summary.clone())
                    .collect()
            };
            (
                touched(&history.history.ours),
                touched(&history.history.theirs),
            )
        });
        assert_eq!(
            (ours, theirs),
            (vec!["ours".to_owned()], vec!["theirs".to_owned()])
        );
        // Each conflicting line names its commit; clicking it shows the commit.
        let credit = cx
            .debug_bounds("conflict-credit-theirs-1")
            .expect("line credit drawn");
        cx.simulate_click(credit.center(), Modifiers::default());
        let chosen = view.read_with(&cx, |view, _| view.story_commit);
        assert_eq!(chosen, Some((gitez_command::Side::Theirs, 0)));
    }

    #[gpui_kit::test]
    fn a_sideways_swipe_does_not_scroll_the_page(cx: &mut TestAppContext) {
        let dir = conflicted_repository(40);
        let (_, mut cx) = open(dir.path(), cx);
        let before = scroll_offset(&mut cx);
        for _ in 0..10 {
            cx.simulate_event(ScrollWheelEvent {
                position: point(px(600.), px(300.)),
                delta: ScrollDelta::Pixels(point(px(-200.), px(0.))),
                touch_phase: TouchPhase::Moved,
                modifiers: Modifiers::default(),
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
        }
        assert_eq!(scroll_offset(&mut cx), before);
    }

    #[gpui_kit::test]
    fn a_long_line_scrolls_sideways(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("file.txt"), "base\n").expect("write");
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        git(root, &["checkout", "-qb", "theirs"]);
        std::fs::write(root.join("file.txt"), "theirs\n").expect("write");
        git(root, &["commit", "-qam", "theirs"]);
        git(root, &["checkout", "-q", "main"]);
        let long = format!("{}\n", "ours ".repeat(80));
        std::fs::write(root.join("file.txt"), long).expect("write");
        git(root, &["commit", "-qam", "ours"]);
        git(root, &["merge", "-q", "theirs"]);

        let (view, mut cx) = open(root, cx);
        let before = cx
            .debug_bounds("conflict-ours-line-0")
            .expect("our line drawn");
        cx.simulate_event(ScrollWheelEvent {
            position: point(before.origin.x + px(50.), before.center().y),
            delta: ScrollDelta::Pixels(point(px(-300.), px(0.))),
            touch_phase: TouchPhase::Moved,
            modifiers: Modifiers::default(),
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let after = cx
            .debug_bounds("conflict-ours-line-0")
            .expect("our line drawn");
        assert!(
            after.origin.x < before.origin.x,
            "the line moves left: {before:?} -> {after:?}"
        );

        // So does what our commit did to the lines, under the history: its
        // widest line, the long one it added.
        const CHANGE_LINES: [&str; 4] = [
            "story-change-line-0",
            "story-change-line-1",
            "story-change-line-2",
            "story-change-line-3",
        ];
        view.update_in(&mut cx, |view, _, cx| {
            view.choose_story_commit(gitez_command::Side::Ours, 0, cx)
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let added = CHANGE_LINES
            .iter()
            .filter_map(|name| cx.debug_bounds(name))
            .max_by(|a, b| f32::from(a.size.width).total_cmp(&f32::from(b.size.width)))
            .expect("the commit's change drawn");
        cx.simulate_event(ScrollWheelEvent {
            position: point(added.origin.x + px(50.), added.center().y),
            delta: ScrollDelta::Pixels(point(px(-300.), px(0.))),
            touch_phase: TouchPhase::Moved,
            modifiers: Modifiers::default(),
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let moved = CHANGE_LINES
            .iter()
            .filter_map(|name| cx.debug_bounds(name))
            .max_by(|a, b| f32::from(a.size.width).total_cmp(&f32::from(b.size.width)))
            .expect("the commit's change drawn");
        assert!(
            moved.origin.x < added.origin.x,
            "the change moves left: {added:?} -> {moved:?}"
        );
    }

    #[gpui_kit::test]
    fn clicking_a_result_line_edits_it(cx: &mut TestAppContext) {
        let dir = conflicted_repository(3);
        let (view, mut cx) = open(dir.path(), cx);
        // Take our side, so the result has lines to click, and fold the
        // history away so the result fits the window.
        view.update_in(&mut cx, |view, _, cx| {
            view.story_open = false;
            view.take_side(gitez_command::Side::Ours, true, cx)
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let line = cx
            .debug_bounds("conflict-result-line-1")
            .expect("result line drawn");
        cx.simulate_click(line.center(), Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let editing = view.read_with(&cx, |view, _| {
            view.editing.as_ref().map(|(input, _)| input.clone())
        });
        let input = editing.expect("clicking a result line starts editing");
        cx.simulate_input("!");
        let value = input.read_with(&cx, |input, _| input.value().to_string());
        assert_eq!(value, "ours line 0\nours line 1!\nours line 2");
        let edited = view.read_with(&cx, |view, _| match &view.loaded {
            Some(Ok(Some(loaded))) => loaded.files[0].resolutions[0].edited.clone(),
            _ => None,
        });
        assert_eq!(
            edited,
            Some(vec![
                "ours line 0".into(),
                "ours line 1!".into(),
                "ours line 2".into()
            ])
        );
    }
}
