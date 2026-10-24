//! The commit history: every commit reachable from a ref, newest first, with
//! its graph, labels, author and date, in a virtual list. Uncommitted changes
//! get a row of their own right above the commit `HEAD` points at.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use gitez_git::graph::{GraphLayout, Row};
use gitez_git::{Commit, Oid, Order, RefLabel, WorkingTreeStatus};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{graph, labels};
use crate::repository::Snapshot;
use crate::theme::{self, color};
use crate::time;

actions!(
    history,
    [SelectPrevious, SelectNext, SelectFirst, SelectLast]
);

/// Key bindings of the history list.
pub fn key_bindings() -> Vec<KeyBinding> {
    const CONTEXT: Option<&str> = Some("History");
    vec![
        KeyBinding::new("up", SelectPrevious, CONTEXT),
        KeyBinding::new("k", SelectPrevious, CONTEXT),
        KeyBinding::new("down", SelectNext, CONTEXT),
        KeyBinding::new("j", SelectNext, CONTEXT),
        KeyBinding::new("home", SelectFirst, CONTEXT),
        KeyBinding::new("end", SelectLast, CONTEXT),
    ]
}

/// Commits in the first batch, small so the first screen appears at once.
const FIRST_BATCH: usize = 200;
/// Commits in each later batch.
const BATCH: usize = 5_000;

pub const ROW_HEIGHT: Pixels = px(36.);
/// Room kept for a commit's summary beside its ref labels.
const SUMMARY_MIN_WIDTH: Pixels = px(160.);

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
    selected: Option<Entry>,
    /// Whether the whole history has been read.
    complete: bool,
    error: Option<SharedString>,
    /// Reference time for relative dates, so they do not shift while scrolling.
    now: i64,
    /// The walk in flight. Dropping it stops the walk.
    _load: Option<Task<()>>,
}

impl EventEmitter<SelectionChanged> for History {}

impl History {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            commits: Vec::new(),
            rows: Vec::new(),
            lanes: 1,
            graph_scroll: px(0.),
            labels: HashMap::new(),
            head: None,
            head_index: None,
            working_tree: None,
            selected: None,
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
        self.head = head;
        self.complete = true;
        self.error = None;
        self.now = time::now();
        self._load = None;
        self.set_graph_scroll(self.graph_scroll);
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

    fn render_rows(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        range
            .filter_map(|index| {
                let entry = self.entry(index);
                let row = self.row_shell(index, self.selected == Some(entry), cx);
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
            .h(ROW_HEIGHT)
            .pr(columns::END_PADDING)
            .gap(columns::GAP)
            .border_b_1()
            .border_color(rgb(color::ROW_BORDER))
            .map(|this| {
                if selected {
                    this.bg(rgb(color::SELECTED))
                } else {
                    this.hover(|style| style.bg(rgb(color::ROW_HOVER)))
                }
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                window.focus(&this.focus_handle, cx);
                this.select(index, cx);
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
            row.child(self.graph_column(cell, cx))
                .child(
                    div()
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
                )
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

        Some(
            row.child(self.graph_column(cell, cx))
                .child(
                    div()
                        .h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(6.))
                        .overflow_hidden()
                        .when_some(labels, |this, labels| {
                            this.children(labels.iter().map(labels::ref_label))
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
                    .child(
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
                    .child(div().flex_1().min_w_0().child("DESCRIPTION"))
                    .child(column(columns::AUTHOR, "AUTHOR"))
                    .child(column(columns::DATE, "DATE")),
            )
            .child(body)
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
