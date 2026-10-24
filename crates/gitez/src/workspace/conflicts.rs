//! Resolving the conflicts of a merge that stopped, as in the
//! `Conflicts.dc.html` artboard: the files that need attention, each
//! conflict with our and their lines side by side, ticking lines into the
//! result or editing it, then committing the merge or giving it up.

use std::sync::Arc;

use gitez_command::{Git, Side};
use gitez_git::{Conflict, ConflictFile, MergeInProgress, Repository, Segment, WorktreeChange};
use gitez_review::{PullRequest, State, Store};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::theme::{self, color};

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
    Finished { committed: bool },
}

/// How a conflict is resolved so far.
#[derive(Debug, Clone, Default)]
struct Resolution {
    /// Which of our lines go into the result.
    ours: Vec<bool>,
    theirs: Vec<bool>,
    /// Their picked lines come before ours.
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

    /// The conflict's lines in the result, and where each comes from.
    fn lines(&self, conflict: &Conflict) -> Vec<(String, Source)> {
        if let Some(edited) = &self.edited {
            return edited
                .iter()
                .map(|line| (line.clone(), Source::Both))
                .collect();
        }
        let pick = |lines: &[String], picked: &[bool], source| {
            lines
                .iter()
                .zip(picked)
                .filter(|(_, picked)| **picked)
                .map(|(line, _)| (line.clone(), source))
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
    _load: Task<()>,
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
            _load: Task::ready(()),
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

    fn take(&mut self, ours: bool, theirs: bool, theirs_first: bool, cx: &mut Context<Self>) {
        self.editing = None;
        self.change(cx, |resolution, conflict| {
            resolution.ours = vec![ours; conflict.ours.len()];
            resolution.theirs = vec![theirs; conflict.theirs.len()];
            resolution.theirs_first = theirs_first;
            resolution.edited = None;
        });
    }

    fn toggle_line(&mut self, side: Side, line: usize, cx: &mut Context<Self>) {
        self.change(cx, |resolution, _| {
            let picked = match side {
                Side::Ours => &mut resolution.ours,
                Side::Theirs => &mut resolution.theirs,
            };
            if let Some(picked) = picked.get_mut(line) {
                *picked = !*picked;
            }
            resolution.edited = None;
            resolution.resolved = false;
        });
    }

    /// Starts typing the current conflict's result, from what it is now.
    fn edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let text = resolution
            .lines(conflict)
            .into_iter()
            .map(|(line, _)| line)
            .collect::<Vec<_>>()
            .join("\n");
        let input = cx.new(|cx| {
            let mut input = TextareaState::new(window, cx).auto_grow(3, 24);
            input.set_value(text, window, cx);
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
                .map(|(line, _)| line)
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
        let (repository, store) = (self.repository.clone(), self.reviews.clone());
        self.error = None;
        self.running = Some(cx.spawn(async move |this, cx| {
            let committed = cx
                .background_executor()
                .spawn(async move {
                    git.commit_merge()?;
                    if let Some(number) = pull_request {
                        super::review::record_merge(&repository, &store, number, merge.ours)?;
                    }
                    anyhow::Ok(())
                })
                .await;
            this.update(cx, |this, cx| {
                this.running = None;
                match committed {
                    Ok(()) => cx.emit(ConflictsEvent::Finished { committed: true }),
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
                    Ok(()) => cx.emit(ConflictsEvent::Finished { committed: false }),
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

/// A line of code: a key column (a tick box or a tag), its number, its
/// text.
fn code_line(key: Option<AnyElement>, number: Option<usize>, text: &str) -> Div {
    div()
        .h_flex()
        .h(LINE_HEIGHT)
        .child(
            div()
                .flex()
                .flex_none()
                .w(px(28.))
                .justify_center()
                .children(key),
        )
        .child(
            div()
                .flex_none()
                .w(px(40.))
                .pr(px(10.))
                .text_right()
                .text_color(rgb(color::DIFF_LINE_NUMBER))
                .children(number.map(|n| n.to_string())),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .pr(px(12.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(rgb(color::DIFF_TEXT))
                // An empty line keeps its height.
                .child(if text.is_empty() {
                    " ".to_owned()
                } else {
                    text.replace('\t', "    ")
                }),
        )
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
        let context_lines = |start: usize, lines: &[String]| {
            lines
                .iter()
                .enumerate()
                .map(|(i, line)| code_line(None, Some(start + i), line))
                .collect::<Vec<_>>()
        };

        let side_lines = |side: Side, lines: &[String], picked: &[bool], start: usize| {
            let (faint, strong, tick, on_tick) = match side {
                Side::Ours => (
                    color::CONFLICT_OURS,
                    color::CONFLICT_OURS_PICKED,
                    color::ACCENT,
                    color::ON_ACCENT,
                ),
                Side::Theirs => (
                    color::CONFLICT_THEIRS,
                    color::CONFLICT_THEIRS_PICKED,
                    color::BRANCH_TEAL,
                    color::HEAD_BADGE_TEXT,
                ),
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
                    let tick_box = div()
                        .id(id)
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(16.))
                        .rounded(px(4.))
                        .border_1()
                        .cursor_pointer()
                        .map(|this| {
                            if on {
                                this.bg(rgb(tick)).border_color(rgb(tick)).child(
                                    Icon::new(IconName::Check)
                                        .size(px(11.))
                                        .text_color(rgb(on_tick)),
                                )
                            } else {
                                this.border_color(rgb(color::PICK_BORDER))
                            }
                        })
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_line(side, i, cx)));
                    code_line(Some(tick_box.into_any_element()), Some(start + i), line)
                        .bg(rgba(if on { strong } else { faint }))
                })
                .collect::<Vec<_>>()
        };

        let show_base = self.show_base && conflict.base.is_some();
        let sha = |oid: &gitez_git::Oid| oid.to_hex_with_len(7).to_string();
        let ours_pane = pane()
            .child(
                pane_header()
                    .child(source_tag("OURS", Source::Ours))
                    .child(side_chip(ours_name.clone(), Side::Ours))
                    .child(
                        div()
                            .font_family(mono.clone())
                            .text_size(theme::SMALL_TEXT_SIZE)
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(sha(&merge.ours)),
                    ),
            )
            .child(
                div()
                    .v_flex()
                    .py(px(6.))
                    .font_family(mono.clone())
                    .text_size(px(12.))
                    .children(context_lines(ours_start - before.len(), before))
                    .children(side_lines(
                        Side::Ours,
                        &conflict.ours,
                        &resolution.ours,
                        ours_start,
                    ))
                    .when(conflict.ours.is_empty(), |this| {
                        this.child(empty_side("Nothing here on this side"))
                    })
                    .children(context_lines(ours_start + conflict.ours.len(), after)),
            );
        let theirs_pane = pane()
            .child(
                pane_header()
                    .child(source_tag("THEIRS", Source::Theirs))
                    .child(side_chip(theirs_name.clone(), Side::Theirs))
                    .child(
                        div()
                            .font_family(mono.clone())
                            .text_size(theme::SMALL_TEXT_SIZE)
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(sha(&merge.theirs)),
                    ),
            )
            .child(
                div()
                    .v_flex()
                    .py(px(6.))
                    .font_family(mono.clone())
                    .text_size(px(12.))
                    .children(context_lines(theirs_start - before.len(), before))
                    .children(side_lines(
                        Side::Theirs,
                        &conflict.theirs,
                        &resolution.theirs,
                        theirs_start,
                    ))
                    .when(conflict.theirs.is_empty(), |this| {
                        this.child(empty_side("Nothing here on this side"))
                    })
                    .children(context_lines(theirs_start + conflict.theirs.len(), after)),
            );
        let base_pane = conflict.base.as_ref().filter(|_| show_base).map(|base| {
            pane()
                .child(
                    pane_header().child(source_tag("BASE", Source::Both)).child(
                        div()
                            .text_color(rgb(color::TEXT_MUTED))
                            .child("Before either side changed it"),
                    ),
                )
                .child(
                    div()
                        .v_flex()
                        .py(px(6.))
                        .font_family(mono.clone())
                        .text_size(px(12.))
                        .children(base.iter().map(|line| code_line(None, None, line)))
                        .when(base.is_empty(), |this| {
                            this.child(empty_side("Nothing here before"))
                        }),
                )
        });

        let result_lines = resolution.lines(conflict);
        let from_ours = result_lines
            .iter()
            .filter(|(_, s)| *s == Source::Ours)
            .count();
        let from_theirs = result_lines
            .iter()
            .filter(|(_, s)| *s == Source::Theirs)
            .count();
        let state = if resolution.resolved {
            ("RESOLVED", color::DIFF_ADDED_TEXT)
        } else {
            ("IN PROGRESS", color::CHIP_TEXT)
        };
        let built = if resolution.edited.is_some() {
            format!("Conflict {} is edited by hand.", n + 1)
        } else if resolution.is_empty() {
            "Tick the lines to keep, take one side, or edit the result.".to_owned()
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
        let result_pane = pane()
            .child(
                pane_header()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Result"))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child("Tick lines above or edit it."),
                    )
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
            .child(
                div()
                    .v_flex()
                    .py(px(6.))
                    .font_family(mono.clone())
                    .text_size(px(12.))
                    .children(before.iter().enumerate().map(|(i, line)| {
                        code_line(
                            Some(tagged(Source::Both).into_any_element()),
                            Some(result_start - before.len() + i),
                            line,
                        )
                    }))
                    .child(
                        div()
                            .px(px(12.))
                            .py(px(3.))
                            .text_size(px(10.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(state.1))
                            .child(format!("CONFLICT {} · {}", n + 1, state.0)),
                    )
                    .map(|this| match &editing {
                        Some(input) => this.child(
                            div()
                                .px(px(12.))
                                .py(px(4.))
                                .child(Textarea::new(input).font_family(mono.clone())),
                        ),
                        None => this
                            .children(result_lines.iter().enumerate().map(|(i, (line, source))| {
                                code_line(
                                    Some(tagged(*source).into_any_element()),
                                    Some(result_start + i),
                                    line,
                                )
                                .bg(rgba(
                                    match (resolution.resolved, source) {
                                        (true, _) => color::CONFLICT_RESOLVED,
                                        (false, Source::Ours) => color::CONFLICT_OURS_PICKED,
                                        (false, Source::Theirs) => color::CONFLICT_THEIRS_PICKED,
                                        (false, Source::Both) => color::CONFLICT_RESOLVED,
                                    },
                                ))
                            }))
                            .when(result_lines.is_empty(), |this| {
                                this.child(empty_side(
                                    "Nothing yet: the conflict's lines are removed",
                                ))
                            }),
                    })
                    .children(after.iter().enumerate().map(|(i, line)| {
                        code_line(
                            Some(tagged(Source::Both).into_any_element()),
                            Some(result_start + result_lines.len() + i),
                            line,
                        )
                    })),
            )
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
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.edit(window, cx)),
                                    )
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
        div()
            .id("conflict-file")
            .v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .gap(px(12.))
            .pb(px(24.))
            .child(
                div()
                    .h_flex()
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
                    .child(
                        div()
                            .ml_auto()
                            .h_flex()
                            .flex_wrap()
                            .gap(px(6.))
                            .child(
                                Button::new("take-ours")
                                    .small()
                                    .label(format!("Take {ours_name}"))
                                    .text_color(rgb(color::CHIP_TEXT))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.take(true, false, false, cx)
                                    })),
                            )
                            .child(
                                Button::new("take-theirs")
                                    .small()
                                    .label(format!("Take {theirs_name}"))
                                    .text_color(rgb(color::PULL_REQUEST_OPEN_TEXT))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.take(false, true, false, cx)
                                    })),
                            )
                            .child(
                                Button::new("take-both-ours-first")
                                    .small()
                                    .label(format!("Both, {ours_name} first"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.take(true, true, false, cx)
                                    })),
                            )
                            .child(
                                Button::new("take-both-theirs-first")
                                    .small()
                                    .label(format!("Both, {theirs_name} first"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.take(true, true, true, cx)
                                    })),
                            )
                            .when(conflict.base.is_some(), |this| {
                                this.child(
                                    Checkbox::new("show-base")
                                        .label("Show base")
                                        .checked(self.show_base)
                                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                            this.show_base = *checked;
                                            cx.notify();
                                        })),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .grid()
                    .grid_cols(if base_pane.is_some() { 3 } else { 2 })
                    .gap(px(12.))
                    .child(ours_pane)
                    .children(base_pane)
                    .child(theirs_pane),
            )
            .child(result_pane)
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
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

fn empty_side(text: &'static str) -> Div {
    div()
        .px(px(12.))
        .h(LINE_HEIGHT)
        .flex()
        .items_center()
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
                .h_flex()
                .items_start()
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
