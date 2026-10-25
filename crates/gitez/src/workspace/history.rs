//! The commit history: every commit reachable from a ref, newest first, with
//! its graph, labels, author and date, in a virtual list. Uncommitted changes
//! get a row of their own right above the commit `HEAD` points at. A local
//! branch's label can be dragged onto another branch to merge it there, as
//! in the `Merge.dc.html` artboard. ⌘-click and ⇧-click pick several
//! commits, to cherry-pick them, as in the `CherryPick.dc.html` artboard.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use gitez_git::graph::{GraphLayout, Row};
use gitez_git::{Commit, Oid, Order, RefKind, RefLabel, Repository, WorkingTreeStatus};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{graph, labels};
use crate::repository::Snapshot;
use crate::theme::{self, color};
use crate::time;

actions!(
    history,
    [
        SelectPrevious,
        SelectNext,
        SelectFirst,
        SelectLast,
        CancelDrag
    ]
);

/// Key bindings of the history list.
pub fn key_bindings() -> Vec<KeyBinding> {
    // Not while typing the commit summary in the uncommitted-changes row.
    const CONTEXT: Option<&str> = Some("History && !Input");
    vec![
        KeyBinding::new("up", SelectPrevious, CONTEXT),
        KeyBinding::new("k", SelectPrevious, CONTEXT),
        KeyBinding::new("down", SelectNext, CONTEXT),
        KeyBinding::new("j", SelectNext, CONTEXT),
        KeyBinding::new("home", SelectFirst, CONTEXT),
        KeyBinding::new("end", SelectLast, CONTEXT),
        KeyBinding::new("enter", super::ReviewSelected, CONTEXT),
        KeyBinding::new("escape", CancelDrag, CONTEXT),
    ]
}

/// Commits in the first batch, small so the first screen appears at once.
const FIRST_BATCH: usize = 200;
/// Commits in each later batch.
const BATCH: usize = 5_000;

pub const ROW_HEIGHT: Pixels = px(36.);
/// Room kept for a commit's summary beside its ref labels.
const SUMMARY_MIN_WIDTH: Pixels = px(160.);
/// The most commits highlighted as coming in with a dragged branch.
const MAX_INCOMING: usize = 2_000;

/// Column widths, shared by the header and the rows.
mod columns {
    use gpui_kit::{Pixels, px};

    pub const GRAPH_MIN: Pixels = px(112.);
    /// About ten lanes; wider graphs scroll sideways.
    pub const GRAPH_MAX: Pixels = px(240.);
    pub const AUTHOR: Pixels = px(150.);
    pub const DATE: Pixels = px(96.);
    pub const GAP: Pixels = px(12.);
    pub const END_PADDING: Pixels = px(14.);
}

/// Emitted when the selected row changes, or the uncommitted changes while
/// their row is selected.
pub struct SelectionChanged;

/// Emitted when the commits picked with ⌘-click or ⇧-click change.
pub struct PickedChanged;

/// Emitted when the picked commits are to be cherry-picked.
pub struct CherryPickRequested;

/// Emitted when a branch is dropped on another, to merge it there.
pub struct BranchDropped {
    pub head: String,
    pub head_tip: Oid,
    pub base: String,
    pub base_tip: Oid,
}

/// A local branch being dragged.
pub struct DraggedBranch {
    name: SharedString,
    tip: Oid,
}

/// The branch being dragged and where it would land.
struct Drag {
    branch: SharedString,
    tip: Oid,
    /// The commit under the pointer, by index into `commits`.
    over_commit: Option<usize>,
    /// The branch label under the pointer.
    over_label: Option<SharedString>,
    /// The branch it would be dropped on, and the index of its commit.
    target: Option<(SharedString, usize)>,
    /// The commits the dragged branch would bring into the target, once
    /// read.
    incoming: HashSet<Oid>,
    _incoming: Option<Task<()>>,
}

/// How a row looks while a branch is dragged.
enum DragRole {
    /// The commit of the branch it would be dropped on.
    Target,
    /// A commit it would bring in.
    Incoming,
    /// Any other row, dimmed.
    Other,
}

/// What the selected row shows.
pub enum Selection<'a> {
    WorkingTree(&'a Arc<WorkingTreeStatus>),
    Commit(&'a Commit, &'a [RefLabel]),
}

/// A row of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    WorkingTree,
    /// The commit at this index of `commits`.
    Commit(usize),
}

pub struct History {
    focus_handle: FocusHandle,
    /// The repository shown, once loaded.
    repository: Option<Repository>,
    scroll: UniformListScrollHandle,
    /// Commits in history order, as far as they have been read.
    commits: Vec<Commit>,
    /// Graph layout of each commit, by index.
    rows: Vec<Row>,
    /// The widest row so far, in lanes.
    lanes: usize,
    /// How far the graph is scrolled to the left, when wider than its column.
    graph_scroll: Pixels,
    labels: HashMap<Oid, Vec<RefLabel>>,
    head: Option<Oid>,
    /// Index of the `HEAD` commit in `commits`, once read.
    head_index: Option<usize>,
    /// Uncommitted changes; the list shows their row while not clean.
    working_tree: Option<Arc<WorkingTreeStatus>>,
    /// The commit summary typed in the uncommitted-changes row, shared with
    /// the changes panel.
    summary: Option<(Entity<InputState>, Subscription)>,
    selected: Option<Entry>,
    /// The branch being dragged onto another, if any.
    drag: Option<Drag>,
    /// Commits picked with ⌘-click or ⇧-click, in the order picked.
    picked: Vec<Oid>,
    /// Index of the commit a ⇧-click picks from.
    pick_anchor: Option<usize>,
    /// Whether the whole history has been read.
    complete: bool,
    error: Option<SharedString>,
    /// Reference time for relative dates, so they do not shift while scrolling.
    now: i64,
    /// The walk in flight. Dropping it stops the walk.
    _load: Option<Task<()>>,
}

impl EventEmitter<SelectionChanged> for History {}
impl EventEmitter<BranchDropped> for History {}
impl EventEmitter<PickedChanged> for History {}
impl EventEmitter<CherryPickRequested> for History {}

impl History {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            repository: None,
            scroll: UniformListScrollHandle::new(),
            commits: Vec::new(),
            rows: Vec::new(),
            lanes: 1,
            graph_scroll: px(0.),
            labels: HashMap::new(),
            head: None,
            head_index: None,
            working_tree: None,
            summary: None,
            selected: None,
            drag: None,
            picked: Vec::new(),
            pick_anchor: None,
            complete: false,
            error: None,
            now: time::now(),
            _load: None,
        }
    }

    /// Replaces the history with that of `snapshot`, read in batches on a
    /// background thread.
    pub fn load(&mut self, snapshot: &Snapshot, cx: &mut Context<Self>) {
        let had_selection = self.selected.is_some();
        *self = Self {
            focus_handle: self.focus_handle.clone(),
            scroll: self.scroll.clone(),
            summary: self.summary.take(),
            repository: Some(snapshot.repository.clone()),
            labels: snapshot.refs.labels_by_commit(),
            head: snapshot.refs.head.as_ref().and_then(|head| head.target()),
            ..Self::new(cx)
        };
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);

        let repository = snapshot.repository.clone();
        let tips = snapshot.refs.tips(true);
        self._load = Some(cx.spawn(async move |this, cx| {
            let walk = cx
                .background_executor()
                .spawn(async move { repository.walk(tips, Order::Date) })
                .await;
            let mut walk = match walk {
                Ok(walk) => walk,
                Err(error) => {
                    this.update(cx, |this, cx| this.fail(error, cx)).ok();
                    return;
                }
            };
            let mut layout = GraphLayout::new();
            let mut batch_size = FIRST_BATCH;
            loop {
                let (returned_walk, returned_layout, batch) = cx
                    .background_executor()
                    .spawn(async move {
                        let batch = walk.next_batch(batch_size).map(|commits| {
                            let rows: Vec<Row> = commits
                                .iter()
                                .map(|commit| layout.push(commit.id, &commit.parents))
                                .collect();
                            (commits, rows)
                        });
                        (walk, layout, batch)
                    })
                    .await;
                (walk, layout) = (returned_walk, returned_layout);
                let Ok(still_open) = this.update(cx, |this, cx| match batch {
                    Ok((commits, rows)) => this.append(commits, rows, cx),
                    Err(error) => {
                        this.fail(error, cx);
                        false
                    }
                }) else {
                    return;
                };
                if !still_open {
                    return;
                }
                batch_size = BATCH;
            }
        }));
        if had_selection {
            cx.emit(SelectionChanged);
        }
        cx.notify();
    }

    /// Reads the history of `snapshot`, a newer state of the repository on
    /// screen, in full on a background thread and then replaces the list in
    /// one step, keeping the selected commit and the scroll position.
    pub fn reload(&mut self, snapshot: &Snapshot, cx: &mut Context<Self>) {
        self.repository = Some(snapshot.repository.clone());
        let repository = snapshot.repository.clone();
        let tips = snapshot.refs.tips(true);
        let labels = snapshot.refs.labels_by_commit();
        let head = snapshot.refs.head.as_ref().and_then(|head| head.target());
        self._load = Some(cx.spawn(async move |this, cx| {
            let history = cx
                .background_executor()
                .spawn(async move {
                    let mut layout = GraphLayout::new();
                    let commits = repository
                        .walk(tips, Order::Date)?
                        .collect::<gitez_git::Result<Vec<_>>>()?;
                    let rows = commits
                        .iter()
                        .map(|commit| layout.push(commit.id, &commit.parents))
                        .collect::<Vec<_>>();
                    gitez_git::Result::Ok((commits, rows))
                })
                .await;
            this.update(cx, |this, cx| match history {
                Ok((commits, rows)) => this.replace(commits, rows, labels, head, cx),
                Err(error) => this.fail(error, cx),
            })
            .ok();
        }));
    }

    fn replace(
        &mut self,
        commits: Vec<Commit>,
        rows: Vec<Row>,
        labels: HashMap<Oid, Vec<RefLabel>>,
        head: Option<Oid>,
        cx: &mut Context<Self>,
    ) {
        let selected_commit = match self.selected {
            Some(Entry::Commit(index)) => self.commits.get(index).map(|commit| commit.id),
            _ => None,
        };
        self.lanes = rows.iter().map(|row| row.width).fold(1, usize::max);
        self.head_index = head.and_then(|head| commits.iter().position(|commit| commit.id == head));
        self.commits = commits;
        self.rows = rows;
        self.labels = labels;
        // Its rows may have moved.
        if let Some(drag) = &mut self.drag {
            drag.over_commit = None;
            drag.target = None;
            drag.incoming.clear();
            drag._incoming = None;
        }
        self.head = head;
        self.complete = true;
        self.error = None;
        self.now = time::now();
        self._load = None;
        self.set_graph_scroll(self.graph_scroll);
        let picked = self.picked.len();
        let commits = &self.commits;
        self.picked
            .retain(|id| commits.iter().any(|commit| commit.id == *id));
        self.pick_anchor = None;
        if self.picked.len() != picked {
            cx.emit(PickedChanged);
        }
        if let Some(id) = selected_commit {
            self.selected = self
                .commits
                .iter()
                .position(|commit| commit.id == id)
                .map(Entry::Commit);
            // Its refs may have changed, or it may be gone.
            cx.emit(SelectionChanged);
        }
        cx.notify();
    }

    /// Adds a batch of commits; returns whether more may follow.
    fn append(&mut self, commits: Vec<Commit>, rows: Vec<Row>, cx: &mut Context<Self>) -> bool {
        if commits.is_empty() {
            self.complete = true;
            self._load = None;
            cx.notify();
            return false;
        }
        self.lanes = rows
            .iter()
            .map(|row| row.width)
            .fold(self.lanes, usize::max);
        if self.head_index.is_none()
            && let Some(head) = self.head
            && let Some(offset) = commits.iter().position(|commit| commit.id == head)
        {
            self.head_index = Some(self.commits.len() + offset);
            // The uncommitted-changes row moves from the top to above `HEAD`.
            if self.working_tree_index().is_some() {
                self.scroll_to_selection();
            }
        }
        self.commits.extend(commits);
        self.rows.extend(rows);
        cx.notify();
        true
    }

    fn fail(&mut self, error: gitez_git::Error, cx: &mut Context<Self>) {
        self.error = Some(
            format!(
                "Could not read the history: {:#}",
                anyhow::Error::from(error)
            )
            .into(),
        );
        self._load = None;
        cx.notify();
    }

    /// Shows `status` as the uncommitted-changes row, or removes the row
    /// when `status` is clean or `None`.
    pub fn set_working_tree(
        &mut self,
        status: Option<Arc<WorkingTreeStatus>>,
        cx: &mut Context<Self>,
    ) {
        let status = status.filter(|status| !status.is_clean());
        if status == self.working_tree {
            return;
        }
        self.working_tree = status;
        if self.selected == Some(Entry::WorkingTree) {
            if self.working_tree.is_none() {
                self.selected = None;
            }
            cx.emit(SelectionChanged);
        }
        cx.notify();
    }

    /// Shows `input` in the uncommitted-changes row, to type the commit
    /// summary there; the row is selected when it gets the cursor.
    pub fn set_summary_input(&mut self, input: Entity<InputState>, cx: &mut Context<Self>) {
        let subscription = cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Focus) && this.selected != Some(Entry::WorkingTree) {
                this.clear_picked(cx);
                this.select_working_tree(cx);
            }
        });
        self.summary = Some((input, subscription));
        cx.notify();
    }

    /// What the selected row shows.
    pub fn selected(&self) -> Option<Selection<'_>> {
        match self.selected? {
            Entry::WorkingTree => self.working_tree.as_ref().map(Selection::WorkingTree),
            Entry::Commit(index) => {
                let commit = self.commits.get(index)?;
                let labels = self.labels.get(&commit.id).map_or(&[][..], Vec::as_slice);
                Some(Selection::Commit(commit, labels))
            }
        }
    }

    /// The commits read so far, newest first.
    pub fn commits(&self) -> &[Commit] {
        &self.commits
    }

    /// The refs pointing at commit `id`.
    pub fn labels_of(&self, id: Oid) -> &[RefLabel] {
        self.labels.get(&id).map_or(&[][..], Vec::as_slice)
    }

    /// Selects commit `id`, if the history has read it; returns whether it
    /// did.
    pub fn select_commit(&mut self, id: Oid, cx: &mut Context<Self>) -> bool {
        let Some(index) = self.commits.iter().position(|commit| commit.id == id) else {
            return false;
        };
        let Some(row) = self.index_of(Entry::Commit(index)) else {
            return false;
        };
        self.select(row, cx);
        true
    }

    /// Selects the uncommitted changes, if there are any; returns whether
    /// it did.
    pub fn select_working_tree(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(row) = self.index_of(Entry::WorkingTree) else {
            return false;
        };
        self.select(row, cx);
        true
    }

    /// The commits right before and after the selected one in the history:
    /// newer, then older.
    pub fn neighbors(&self) -> (Option<&Commit>, Option<&Commit>) {
        let Some(Entry::Commit(index)) = self.selected else {
            return (None, None);
        };
        let newer = index
            .checked_sub(1)
            .and_then(|index| self.commits.get(index));
        (newer, self.commits.get(index + 1))
    }

    /// Selects the commit `delta` places from the selected one, newer for a
    /// negative `delta`. Returns whether the selection moved.
    pub fn select_adjacent_commit(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let Some(Entry::Commit(index)) = self.selected else {
            return false;
        };
        let Some(index) = index.checked_add_signed(delta) else {
            return false;
        };
        if index >= self.commits.len() {
            return false;
        }
        self.selected = Some(Entry::Commit(index));
        self.scroll_to_selection();
        cx.emit(SelectionChanged);
        cx.notify();
        true
    }

    /// Where the uncommitted-changes row is, if shown: right above the `HEAD`
    /// commit, or at the top until that commit has been read.
    fn working_tree_index(&self) -> Option<usize> {
        self.working_tree.as_ref()?;
        Some(self.head_index.unwrap_or(0))
    }

    /// Number of rows in the list.
    fn len(&self) -> usize {
        self.commits.len() + usize::from(self.working_tree_index().is_some())
    }

    fn entry(&self, index: usize) -> Entry {
        match self.working_tree_index() {
            Some(wip) if index == wip => Entry::WorkingTree,
            Some(wip) if index > wip => Entry::Commit(index - 1),
            _ => Entry::Commit(index),
        }
    }

    fn index_of(&self, entry: Entry) -> Option<usize> {
        match (entry, self.working_tree_index()) {
            (Entry::WorkingTree, wip) => wip,
            (Entry::Commit(index), Some(wip)) if index >= wip => Some(index + 1),
            (Entry::Commit(index), _) => Some(index),
        }
    }

    /// The graph row of an entry; the uncommitted changes take `HEAD`'s.
    fn graph_row(&self, entry: Entry) -> Option<&Row> {
        match entry {
            Entry::WorkingTree => self.rows.get(self.head_index?),
            Entry::Commit(index) => self.rows.get(index),
        }
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(last) = self.len().checked_sub(1) else {
            return;
        };
        let entry = self.entry(index.min(last));
        if self.selected != Some(entry) {
            self.selected = Some(entry);
            self.scroll_to_selection();
            cx.emit(SelectionChanged);
            cx.notify();
        }
    }

    /// Scrolls the list, and the graph sideways, to show the selected row.
    fn scroll_to_selection(&mut self) {
        let Some(entry) = self.selected else {
            return;
        };
        if let Some(index) = self.index_of(entry) {
            self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
        }
        if let Some(lane) = self.graph_row(entry).map(|row| row.lane) {
            let scroll = graph::scroll_to_lane(lane, self.graph_scroll, self.graph_width());
            self.set_graph_scroll(scroll);
        }
    }

    fn selected_index(&self) -> Option<usize> {
        self.index_of(self.selected?)
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        let index = self
            .selected_index()
            .map_or(0, |index| index.saturating_sub(1));
        self.select(index, cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let index = self.selected_index().map_or(0, |index| index + 1);
        self.select(index, cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select(0, cx);
    }

    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.select(usize::MAX, cx);
    }

    fn graph_width(&self) -> Pixels {
        graph::column_width(self.lanes, columns::GRAPH_MIN, columns::GRAPH_MAX)
    }

    /// How far the graph can scroll: zero when it fits its column.
    fn max_graph_scroll(&self) -> Pixels {
        (graph::content_width(self.lanes) - self.graph_width()).max(px(0.))
    }

    fn set_graph_scroll(&mut self, scroll: Pixels) -> bool {
        let scroll = scroll.clamp(px(0.), self.max_graph_scroll());
        let changed = scroll != self.graph_scroll;
        self.graph_scroll = scroll;
        changed
    }

    /// Scrolls the graph sideways on a horizontal scroll, or a vertical one
    /// with Shift held, over the graph column. Other scrolls go to the list.
    fn scroll_graph(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(window.line_height());
        let sideways = if event.modifiers.shift && delta.x == px(0.) {
            delta.y
        } else if delta.x.abs() > delta.y.abs() {
            delta.x
        } else {
            return;
        };
        cx.stop_propagation();
        if self.set_graph_scroll(self.graph_scroll - sideways) {
            cx.notify();
        }
    }

    fn start_drag(&mut self, dragged: &DraggedBranch, window: &mut Window, cx: &mut Context<Self>) {
        // For Esc to cancel it.
        window.focus(&self.focus_handle, cx);
        self.drag = Some(Drag {
            branch: dragged.name.clone(),
            tip: dragged.tip,
            over_commit: None,
            over_label: None,
            target: None,
            incoming: HashSet::new(),
            _incoming: None,
        });
        cx.notify();
    }

    /// The pointer moved, inside the row of commit `index` or not, while a
    /// branch is dragged.
    fn drag_over_commit(&mut self, index: usize, inside: bool, cx: &mut Context<Self>) {
        let Some(drag) = &mut self.drag else {
            return;
        };
        if inside {
            drag.over_commit = Some(index);
        } else if drag.over_commit == Some(index) {
            drag.over_commit = None;
        } else {
            return;
        }
        self.update_target(cx);
    }

    /// The pointer moved, inside the label of branch `name` or not, while a
    /// branch is dragged.
    fn drag_over_label(&mut self, name: &SharedString, inside: bool, cx: &mut Context<Self>) {
        let Some(drag) = &mut self.drag else {
            return;
        };
        if inside {
            drag.over_label = Some(name.clone());
        } else if drag.over_label.as_ref() == Some(name) {
            drag.over_label = None;
        } else {
            return;
        }
        self.update_target(cx);
    }

    /// The branch the dragged one would land on: the one under the pointer,
    /// or else the first other local branch on the commit under it.
    fn drop_target(&self, drag: &Drag) -> Option<(SharedString, usize)> {
        let index = drag.over_commit?;
        let commit = self.commits.get(index)?;
        let mut branches = self
            .labels
            .get(&commit.id)?
            .iter()
            .filter(|label| label.kind == RefKind::Branch && label.name != *drag.branch);
        let first = branches.clone().next()?;
        let label = branches
            .find(|label| drag.over_label.as_deref() == Some(label.name.as_str()))
            .unwrap_or(first);
        Some((label.name.clone().into(), index))
    }

    /// Works out the drop target again, and when it changed, which commits
    /// the dragged branch would bring into it.
    fn update_target(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = &self.drag else {
            return;
        };
        let target = self.drop_target(drag);
        if drag.target == target {
            return;
        }
        let base = target
            .as_ref()
            .and_then(|(_, index)| self.commits.get(*index))
            .map(|commit| commit.id);
        let repository = self.repository.clone();
        let Some(drag) = &mut self.drag else {
            return;
        };
        drag.target = target;
        drag.incoming.clear();
        drag._incoming = None;
        if let (Some(base), Some(repository)) = (base, repository) {
            let head = drag.tip;
            drag._incoming = Some(cx.spawn(async move |this, cx| {
                let incoming = cx
                    .background_executor()
                    .spawn(async move {
                        repository
                            .walk_excluding([head], [base], Order::Date)?
                            .take(MAX_INCOMING)
                            .map(|commit| commit.map(|commit| commit.id))
                            .collect::<gitez_git::Result<HashSet<Oid>>>()
                    })
                    .await;
                this.update(cx, |this, cx| {
                    if let Some(drag) = &mut this.drag {
                        // Only the highlight is lost if the walk fails.
                        drag.incoming = incoming.unwrap_or_default();
                        cx.notify();
                    }
                })
                .ok();
            }));
        }
        cx.notify();
    }

    /// The dragged branch was dropped on a row.
    fn drop_branch(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        cx.notify();
        let Some((base, index)) = drag.target else {
            return;
        };
        let Some(base_tip) = self.commits.get(index).map(|commit| commit.id) else {
            return;
        };
        cx.emit(BranchDropped {
            head: drag.branch.to_string(),
            head_tip: drag.tip,
            base: base.to_string(),
            base_tip,
        });
    }

    /// Esc: stops dragging a branch, or else unpicks the picked commits.
    fn cancel_drag(&mut self, _: &CancelDrag, window: &mut Window, cx: &mut Context<Self>) {
        let dragging = cx.stop_active_drag(window);
        if self.drag.take().is_some() || dragging {
            cx.notify();
        } else if !self.picked.is_empty() {
            self.clear_picked(cx);
        } else {
            cx.propagate();
        }
    }

    /// Picks or unpicks the commit at list position `index`, or with
    /// `range`, picks every commit from the last one picked to it. The
    /// first commit picked brings the selected one along.
    fn pick(&mut self, index: usize, range: bool, cx: &mut Context<Self>) {
        let Entry::Commit(commit) = self.entry(index) else {
            return;
        };
        let Some(id) = self.commits.get(commit).map(|commit| commit.id) else {
            return;
        };
        let selected = match self.selected {
            Some(Entry::Commit(selected)) => Some(selected),
            _ => None,
        };
        if range && let Some(anchor) = self.pick_anchor.or(selected) {
            let (first, last) = (anchor.min(commit), anchor.max(commit));
            for other in &self.commits[first..=last] {
                if !self.picked.contains(&other.id) {
                    self.picked.push(other.id);
                }
            }
        } else if let Some(position) = self.picked.iter().position(|picked| *picked == id) {
            self.picked.remove(position);
        } else {
            if self.picked.is_empty()
                && let Some(selected) = selected.filter(|selected| *selected != commit)
                && let Some(other) = self.commits.get(selected)
            {
                self.picked.push(other.id);
            }
            self.picked.push(id);
        }
        self.pick_anchor = Some(commit);
        cx.emit(PickedChanged);
        cx.notify();
    }

    /// Unpicks every picked commit.
    pub fn clear_picked(&mut self, cx: &mut Context<Self>) {
        self.pick_anchor = None;
        if self.picked.is_empty() {
            return;
        }
        self.picked.clear();
        cx.emit(PickedChanged);
        cx.notify();
    }

    /// Picks commit `id` alone.
    pub fn pick_only(&mut self, id: Oid, cx: &mut Context<Self>) {
        self.pick_anchor = None;
        self.picked = vec![id];
        cx.emit(PickedChanged);
        cx.notify();
    }

    /// The picked commits, oldest first, as far as the history has been
    /// read.
    pub fn picked(&self) -> Vec<Commit> {
        self.commits
            .iter()
            .rev()
            .filter(|commit| self.picked.contains(&commit.id))
            .cloned()
            .collect()
    }

    /// The tick box column shown while commits are picked.
    fn pick_cell(&self, commit: Option<usize>) -> Option<Div> {
        if self.picked.is_empty() {
            return None;
        }
        let picked = commit
            .and_then(|index| self.commits.get(index))
            .is_some_and(|commit| self.picked.contains(&commit.id));
        Some(div().w(px(22.)).flex_none().h_flex().justify_center().when(
            commit.is_some(),
            |this| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(15.))
                        .rounded(px(4.))
                        .border_1()
                        .map(|this| {
                            if picked {
                                this.bg(rgb(color::ACCENT)).border_color(rgb(color::ACCENT))
                            } else {
                                this.border_color(rgb(color::PICK_BORDER))
                            }
                        })
                        .when(picked, |this| {
                            this.child(
                                Icon::new(IconName::Check)
                                    .size(px(11.))
                                    .text_color(rgb(color::ON_ACCENT)),
                            )
                        }),
                )
            },
        ))
    }

    /// The bar over the bottom of the list while commits are picked.
    fn render_pick_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.picked.len();
        div()
            .absolute()
            .bottom(px(22.))
            .left_0()
            .right_0()
            .h_flex()
            .justify_center()
            .child(
                div()
                    .h_flex()
                    .gap(px(6.))
                    .pl(px(16.))
                    .pr(px(8.))
                    .py(px(8.))
                    .rounded(px(12.))
                    .bg(rgb(color::BUTTON))
                    .border_1()
                    .border_color(rgb(color::PICK_BAR_BORDER))
                    .shadow_lg()
                    .child(div().mr(px(6.)).font_weight(FontWeight::SEMIBOLD).child(
                        if count == 1 {
                            "1 commit selected".to_owned()
                        } else {
                            format!("{count} commits selected")
                        },
                    ))
                    .child(
                        Button::new("cherry-pick")
                            .label("Cherry-pick onto…")
                            .bg(rgb(color::SELECTED))
                            .border_color(rgb(color::PICK_TARGET_BORDER))
                            .text_color(rgb(color::CHIP_TEXT))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(CherryPickRequested))),
                    )
                    .child(
                        Button::new("clear-picked")
                            .ghost()
                            .icon(IconName::Close)
                            .tooltip("Clear selection")
                            .on_click(cx.listener(|this, _, _, cx| this.clear_picked(cx))),
                    ),
            )
    }

    /// How the row of commit `index` looks while a branch is dragged onto
    /// a target; `None` otherwise.
    fn drag_role(&self, index: Option<usize>) -> Option<DragRole> {
        let drag = self.drag.as_ref()?;
        let (_, target) = drag.target.as_ref()?;
        Some(match index {
            Some(index) if index == *target => DragRole::Target,
            Some(index)
                if self
                    .commits
                    .get(index)
                    .is_some_and(|commit| drag.incoming.contains(&commit.id)) =>
            {
                DragRole::Incoming
            }
            _ => DragRole::Other,
        })
    }

    /// The label of `label`, on commit `index`; a local branch's can be
    /// dragged onto another branch.
    fn render_label(&self, label: &RefLabel, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let chip = labels::ref_label(label);
        let Some(tip) = self.commits.get(index).map(|commit| commit.id) else {
            return chip.into_any_element();
        };
        if label.kind != RefKind::Branch {
            return chip.into_any_element();
        }
        let name: SharedString = label.name.clone().into();
        let drag = self.drag.as_ref();
        let lifted = drag.is_some_and(|drag| drag.branch == name);
        let target = drag
            .and_then(|drag| drag.target.as_ref())
            .is_some_and(|(target, at)| *target == name && *at == index);
        let history = cx.weak_entity();
        let over = name.clone();
        chip.id(SharedString::from(format!("branch-{name}")))
            .cursor_grab()
            .when(lifted, |this| this.opacity(0.35))
            .when(target, |this| {
                this.bg(rgb(color::ACCENT))
                    .text_color(rgb(color::ON_ACCENT))
            })
            .on_drag(
                DraggedBranch {
                    name: name.clone(),
                    tip,
                },
                move |dragged, _, window, cx| {
                    history
                        .update(cx, |this, cx| this.start_drag(dragged, window, cx))
                        .ok();
                    cx.new(|_| DraggedLabel(dragged.name.clone()))
                },
            )
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<DraggedBranch>, _, cx| {
                    let inside = event.bounds.contains(&event.event.position);
                    this.drag_over_label(&over, inside, cx);
                }),
            )
            .into_any_element()
    }

    fn render_rows(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        range
            .filter_map(|index| {
                let entry = self.entry(index);
                let picked = match entry {
                    Entry::Commit(commit) => self
                        .commits
                        .get(commit)
                        .is_some_and(|commit| self.picked.contains(&commit.id)),
                    Entry::WorkingTree => false,
                };
                let row = self.row_shell(index, picked || self.selected == Some(entry), cx);
                match entry {
                    Entry::WorkingTree => self.render_working_tree(row, cx),
                    Entry::Commit(commit) => self.render_commit(row, commit, cx),
                }
            })
            .collect()
    }

    /// An empty row at list position `index`, selected on click.
    fn row_shell(&self, index: usize, selected: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id(index)
            .h_flex()
            // Rows are laid out on their own and would otherwise be as wide
            // as their content, misaligning the columns.
            .w_full()
            .relative()
            .h(ROW_HEIGHT)
            .pr(columns::END_PADDING)
            .gap(columns::GAP)
            // The divider is the first child, not a border: a border is
            // painted over the children and would cut through the graph,
            // which is painted over the divider instead.
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .h(px(1.))
                    .bg(rgb(color::ROW_BORDER)),
            )
            .map(|this| {
                if selected {
                    this.bg(rgb(color::SELECTED))
                } else {
                    this.hover(|style| style.bg(rgb(color::ROW_HOVER)))
                }
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                // A click in the commit summary keeps the cursor there.
                let typing = this
                    .summary
                    .as_ref()
                    .is_some_and(|(input, _)| input.focus_handle(cx).is_focused(window));
                if !typing {
                    window.focus(&this.focus_handle, cx);
                }
                let modifiers = event.modifiers();
                if modifiers.secondary() || modifiers.shift {
                    this.pick(index, modifiers.shift, cx);
                    return;
                }
                this.clear_picked(cx);
                this.select(index, cx);
                if event.click_count() == 2 {
                    window.dispatch_action(Box::new(super::ReviewSelected), cx);
                }
            }))
    }

    /// The graph column of a row: scrolls the graph sideways.
    fn graph_column(&self, cell: impl IntoElement, cx: &mut Context<Self>) -> Div {
        div()
            .h_full()
            .flex_none()
            .on_scroll_wheel(cx.listener(Self::scroll_graph))
            .child(cell)
    }

    fn render_working_tree(
        &self,
        row: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let status = self.working_tree.as_ref()?;
        let head_row = self.head_index.and_then(|index| self.rows.get(index));
        let cell =
            graph::working_tree_cell(head_row.cloned(), self.graph_width(), self.graph_scroll);
        Some(
            row.when(self.drag_role(None).is_some(), |this| this.opacity(0.45))
                .child(self.graph_column(cell, cx))
                .children(self.pick_cell(None))
                .child(match &self.summary {
                    Some((input, _)) => div()
                        .h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(columns::GAP)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(input).appearance(false).small()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .whitespace_nowrap()
                                .italic()
                                .text_size(px(12.))
                                .text_color(rgb(color::WORKING_TREE_TEXT))
                                .child(labels::working_tree_summary(status)),
                        ),
                    None => div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .italic()
                        .text_color(rgb(color::WORKING_TREE_TEXT))
                        .child(format!(
                            "Uncommitted changes — {}",
                            labels::working_tree_summary(status)
                        )),
                })
                .child(div().w(columns::AUTHOR).flex_none())
                .child(
                    div()
                        .w(columns::DATE)
                        .flex_none()
                        .text_size(px(12.))
                        .text_color(rgb(color::TEXT_MUTED))
                        .child("now"),
                )
                .into_any_element(),
        )
    }

    fn render_commit(
        &self,
        row: Stateful<Div>,
        index: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let commit = self.commits.get(index)?;
        let graph_row = self.rows.get(index)?;
        let labels = self.labels.get(&commit.id);
        let head = if self.head != Some(commit.id) {
            graph::Head::No
        } else if self.working_tree.is_some() && self.head_index == Some(index) {
            graph::Head::BelowWorkingTree
        } else {
            graph::Head::Yes
        };
        let cell = graph::cell(
            graph_row.clone(),
            head,
            self.graph_width(),
            self.graph_scroll,
        );

        let row = match self.drag_role(Some(index)) {
            Some(DragRole::Target) => row.bg(rgba(color::TARGET_ROW)),
            Some(DragRole::Incoming) => row.bg(rgba(color::INCOMING_ROW)),
            Some(DragRole::Other) => row.opacity(0.45),
            None => row,
        };

        Some(
            row.on_drag_move(cx.listener(
                move |this, event: &DragMoveEvent<DraggedBranch>, _, cx| {
                    let inside = event.bounds.contains(&event.event.position);
                    this.drag_over_commit(index, inside, cx);
                },
            ))
            .on_drop(cx.listener(|this, _: &DraggedBranch, _, cx| this.drop_branch(cx)))
            .child(self.graph_column(cell, cx))
            .children(self.pick_cell(Some(index)))
            .child(
                div()
                    .h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(6.))
                    .overflow_hidden()
                    .when_some(labels, |this, labels| {
                        this.children(
                            labels
                                .iter()
                                .map(|label| self.render_label(label, index, cx)),
                        )
                    })
                    .child(
                        div()
                            // Labels shrink before the summary goes below
                            // this.
                            .min_w(SUMMARY_MIN_WIDTH)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(commit.summary.clone()),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .w(columns::AUTHOR)
                    .flex_none()
                    .gap(px(7.))
                    .text_color(rgb(color::AUTHOR_TEXT))
                    .child(labels::avatar(&commit.author.name, px(20.)))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(commit.author.name.clone()),
                    ),
            )
            .child(
                div()
                    .w(columns::DATE)
                    .flex_none()
                    .text_size(px(12.))
                    .text_color(rgb(color::TEXT_MUTED))
                    .whitespace_nowrap()
                    .child(time::relative(commit.author.time, self.now)),
            )
            .into_any_element(),
        )
    }
}

impl History {
    /// The "GRAPH" heading, with a scroll indicator along its bottom edge
    /// when the graph is wider than its column.
    fn render_graph_heading(&self) -> impl IntoElement {
        let width = self.graph_width();
        let content = graph::content_width(self.lanes);
        div()
            .relative()
            .h_full()
            .h_flex()
            .w(width)
            .flex_none()
            .pl(px(14.))
            .child("GRAPH")
            .when(self.max_graph_scroll() > px(0.), |this| {
                this.child(
                    div()
                        .absolute()
                        .bottom(px(2.))
                        .left(width * (self.graph_scroll / content))
                        .w(width * (width / content))
                        .h(px(3.))
                        .rounded_full()
                        .bg(theme::with_alpha(color::TEXT_FAINT, 0.5)),
                )
            })
    }
}

impl Focusable for History {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for History {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Released away from any branch: GPUI ends the drag on its own.
        if self.drag.is_some() && !cx.has_active_drag() {
            self.drag = None;
        }
        let column = |width: Pixels, label: &'static str| div().w(width).flex_none().child(label);
        let count = format_count(self.commits.len());
        let summary = match (self.complete, self.commits.len()) {
            (true, 1) => "· 1 commit".to_owned(),
            (true, _) => format!("· {count} commits"),
            (false, 0) => "· reading history…".to_owned(),
            (false, _) => format!("· {count} commits so far…"),
        };

        let body = if self.len() == 0 && (self.complete || self.error.is_some()) {
            let message = self
                .error
                .clone()
                .unwrap_or_else(|| "This repository has no commits yet.".into());
            div()
                .flex_1()
                .p(px(24.))
                .text_color(rgb(color::TEXT_MUTED))
                .child(message)
                .into_any_element()
        } else {
            uniform_list(
                "history",
                self.len(),
                cx.processor(|this, range, _window, cx| this.render_rows(range, cx)),
            )
            .flex_1()
            .track_scroll(&self.scroll)
            .into_any_element()
        };

        div()
            .key_context("History")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::cancel_drag))
            .relative()
            .v_flex()
            .size_full()
            .min_w_0()
            .child(
                div()
                    .h_flex()
                    .flex_none()
                    .h(px(46.))
                    .px(px(14.))
                    .gap(px(8.))
                    .border_b_1()
                    .border_color(rgb(color::BORDER))
                    .when_some(self.drag.as_ref(), |this, drag| {
                        this.child(moving(&drag.branch))
                    })
                    .when(self.drag.is_none(), |this| {
                        this.child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("All branches"),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(color::TEXT_MUTED))
                                .child(summary),
                        )
                        // A walk that failed part-way keeps what it read.
                        .when(!self.commits.is_empty(), |this| {
                            this.when_some(self.error.clone(), |this, error| {
                                this.child(
                                    div()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_size(px(12.))
                                        .text_color(rgb(color::DIFF_REMOVED))
                                        .child(error),
                                )
                            })
                        })
                    })
                    .when(self.drag.is_none() && !self.picked.is_empty(), |this| {
                        this.child(pick_hint(cx))
                    }),
            )
            .child(
                div()
                    .h_flex()
                    .flex_none()
                    .h(px(28.))
                    .pr(columns::END_PADDING)
                    .gap(columns::GAP)
                    .border_b_1()
                    .border_color(rgb(color::BORDER))
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(color::TEXT_FAINT))
                    .child(self.render_graph_heading())
                    .when(!self.picked.is_empty(), |this| {
                        this.child(div().w(px(22.)).flex_none())
                    })
                    .child(div().flex_1().min_w_0().child("DESCRIPTION"))
                    .child(column(columns::AUTHOR, "AUTHOR"))
                    .child(column(columns::DATE, "DATE")),
            )
            .child(body)
            .when(!self.picked.is_empty(), |this| {
                this.child(self.render_pick_bar(cx))
            })
    }
}

/// The strip saying which branch is being dragged, in place of the list's
/// title.
fn moving(branch: &SharedString) -> impl IntoElement {
    let escape = Keystroke::parse("escape").expect("escape is a valid keystroke");
    div()
        .h_flex()
        .flex_1()
        .min_w_0()
        .gap(px(8.))
        .child(
            div()
                .h_flex()
                .min_w_0()
                .gap(px(8.))
                .h(px(30.))
                .px(px(12.))
                .rounded(px(15.))
                .bg(rgb(color::MOVING))
                .text_color(rgb(color::MOVING_TEXT))
                .text_size(px(12.5))
                .child(Icon::new(IconName::Move).size(px(14.)))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(
                            StyledText::new(format!("Moving {branch} — drop it on any branch"))
                                .with_highlights([(
                                    "Moving ".len().."Moving ".len() + branch.len(),
                                    HighlightStyle {
                                        font_weight: Some(FontWeight::SEMIBOLD),
                                        ..Default::default()
                                    },
                                )]),
                        ),
                ),
        )
        .child(
            div()
                .h_flex()
                .ml_auto()
                .flex_none()
                .gap(px(6.))
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_MUTED))
                .child(Kbd::new(escape))
                .child("cancel"),
        )
}

/// How to pick commits, shown while some are picked.
fn pick_hint(cx: &App) -> impl IntoElement {
    let key = |key: &'static str| labels::key_cap(key, cx);
    let secondary = labels::SECONDARY_KEY;
    div()
        .h_flex()
        .ml_auto()
        .flex_none()
        .gap(px(6.))
        .text_size(px(12.))
        .text_color(rgb(color::TEXT_MUTED))
        .child(key(secondary))
        .child("click to select several ·")
        .child(key("⇧"))
        .child("click for a range")
}

/// What follows the pointer while a branch is dragged: its label, lifted.
struct DraggedLabel(SharedString);

impl Render for DraggedLabel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let branch_color = theme::branch_color(&self.0);
        div()
            .rounded(px(5.))
            .bg(rgb(color::PANEL))
            .shadow_lg()
            .font_family(cx.theme().font_family.clone())
            .child(
                div()
                    .h_flex()
                    .h(px(24.))
                    .px(px(10.))
                    .rounded(px(5.))
                    .bg(theme::with_alpha(branch_color, 0.3))
                    .text_color(rgb(branch_color))
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .whitespace_nowrap()
                    .child(self.0.clone()),
            )
    }
}

/// `count` with thousands separated by commas, as in the design: "1,284".
fn format_count(count: usize) -> String {
    let digits = count.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(digit);
    }
    formatted
}

#[cfg(test)]
mod tests {
    use super::format_count;

    #[test]
    fn counts_have_thousands_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_284), "1,284");
        assert_eq!(format_count(1_234_567), "1,234,567");
    }
}
