//! The commit history: every commit reachable from a ref, newest first, with
//! its graph, labels, author, date and SHA, in a virtual list.

use std::collections::HashMap;
use std::ops::Range;

use gitez_git::graph::{GraphLayout, Row};
use gitez_git::{Commit, Oid, Order, RefLabel};
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

/// Column widths, shared by the header and the rows.
mod columns {
    use gpui_kit::{Pixels, px};

    pub const GRAPH_MIN: Pixels = px(112.);
    pub const GRAPH_MAX: Pixels = px(360.);
    pub const AUTHOR: Pixels = px(150.);
    pub const DATE: Pixels = px(96.);
    pub const SHA: Pixels = px(70.);
    pub const GAP: Pixels = px(12.);
    pub const END_PADDING: Pixels = px(14.);
}

/// Emitted when the selected commit changes.
pub struct SelectionChanged;

pub struct History {
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
    /// Commits in history order, as far as they have been read.
    commits: Vec<Commit>,
    /// Graph layout of each commit, by index.
    rows: Vec<Row>,
    /// The widest row so far, in lanes.
    lanes: usize,
    labels: HashMap<Oid, Vec<RefLabel>>,
    head: Option<Oid>,
    selected: Option<usize>,
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
            labels: HashMap::new(),
            head: None,
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

    /// The selected commit and the refs pointing at it.
    pub fn selected(&self) -> Option<(&Commit, &[RefLabel])> {
        let commit = self.commits.get(self.selected?)?;
        let labels = self.labels.get(&commit.id).map_or(&[][..], Vec::as_slice);
        Some((commit, labels))
    }

    pub fn is_head(&self, id: Oid) -> bool {
        self.head == Some(id)
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(last) = self.commits.len().checked_sub(1) else {
            return;
        };
        let index = index.min(last);
        if self.selected != Some(index) {
            self.selected = Some(index);
            self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
            cx.emit(SelectionChanged);
            cx.notify();
        }
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        let index = self.selected.map_or(0, |index| index.saturating_sub(1));
        self.select(index, cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let index = self.selected.map_or(0, |index| index + 1);
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

    fn render_rows(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let graph_width = self.graph_width();
        let mono = cx.theme().mono_font_family.clone();
        range
            .filter_map(|index| {
                let commit = self.commits.get(index)?;
                let row = self.rows.get(index)?;
                let selected = self.selected == Some(index);
                let labels = self.labels.get(&commit.id);
                let sha = commit.id.to_hex_with_len(7).to_string();

                Some(
                    div()
                        .id(index)
                        .h_flex()
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
                        .child(graph::cell(
                            row.clone(),
                            self.is_head(commit.id),
                            graph_width,
                        ))
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
                                        .min_w_0()
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
                        .child(
                            div()
                                .w(columns::SHA)
                                .flex_none()
                                .font_family(mono.clone())
                                .text_size(theme::SMALL_TEXT_SIZE)
                                .text_color(rgb(color::TEXT_MUTED))
                                .child(sha),
                        )
                        .into_any_element(),
                )
            })
            .collect()
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

        let body = if self.commits.is_empty() && (self.complete || self.error.is_some()) {
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
                self.commits.len(),
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
                    .child(column(self.graph_width(), "GRAPH").pl(px(14.)))
                    .child(div().flex_1().min_w_0().child("DESCRIPTION"))
                    .child(column(columns::AUTHOR, "AUTHOR"))
                    .child(column(columns::DATE, "DATE"))
                    .child(column(columns::SHA, "SHA")),
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
