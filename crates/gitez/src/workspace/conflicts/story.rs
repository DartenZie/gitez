//! "Why these lines conflict": the commits each side made since the two
//! sides split, with those that changed the current conflict's lines picked
//! out, and what a chosen commit did there.

use gitez_command::Side;
use std::collections::HashMap;
use std::sync::Arc;

use gitez_git::{
    ChangeLine, Conflict, ConflictFile, ConflictHistory, HistoryCommit, LineKind, Segment,
    SideHistory,
};
use gitez_highlight::{Highlights, Language, Span};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{CodeRow, Conflicts, ConflictsEvent, Source, code_rows, empty_note, source_tag, spans};
use crate::theme::{self, color};
use crate::time;
use crate::workspace::labels::avatar;

/// Commits that touch a conflict shown on a lane at most; older ones are
/// counted instead.
const MAX_SHOWN: usize = 6;
/// Height of a lane, and so of the fork drawn from the base to it.
const LANE_HEIGHT: Pixels = px(48.);
/// Width of every item on a lane, so the lanes line up.
const ITEM_WIDTH: Pixels = px(250.);
/// Height of every item on a lane.
const ITEM_HEIGHT: Pixels = px(38.);
/// Width of the stretch of line that stands for commits left out.
const GAP_WIDTH: Pixels = px(110.);
/// Commits listed in the tooltip of such a stretch at most.
const GAP_TOOLTIP_COMMITS: usize = 12;

/// A conflicted file's history, with what each commit did to each conflict
/// highlighted.
pub(super) struct FileHistory {
    pub history: ConflictHistory,
    /// By conflict and commit index, for our side's commits and theirs.
    ours: HashMap<(usize, usize), ChangeHighlights>,
    theirs: HashMap<(usize, usize), ChangeHighlights>,
}

/// A commit's change to a conflict, highlighted: its lines as they were
/// before and as the commit left them.
pub(super) struct ChangeHighlights {
    old: Highlights,
    new: Highlights,
}

impl ChangeHighlights {
    pub(super) fn spans(&self, line: &ChangeLine) -> Option<&[Span]> {
        match line.kind {
            LineKind::Removed => self.old.line(line.old?, &line.text),
            LineKind::Added | LineKind::Context => self.new.line(line.new?, &line.text),
        }
    }
}

impl FileHistory {
    /// Highlights the changes in `history` of the file at `path`, whose
    /// working tree version is `content`. Blocks; run it in the background.
    pub(super) fn new(path: &str, content: &ConflictFile, history: ConflictHistory) -> Self {
        let first_line = match content.segments.first() {
            Some(Segment::Clean(lines)) => lines.first().map(String::as_str),
            _ => None,
        };
        let language = Language::detect(path, first_line.unwrap_or_default());
        let highlight = |side: &SideHistory| {
            let mut changes = HashMap::new();
            let Some(language) = language else {
                return changes;
            };
            for (n, conflict) in side.conflicts.iter().enumerate() {
                for (index, lines) in &conflict.touched_by {
                    changes.insert(
                        (n, *index),
                        ChangeHighlights {
                            old: gitez_highlight::highlight_runs(language, runs(lines, |l| l.old)),
                            new: gitez_highlight::highlight_runs(language, runs(lines, |l| l.new)),
                        },
                    );
                }
            }
            changes
        };
        Self {
            ours: highlight(&history.ours),
            theirs: highlight(&history.theirs),
            history,
        }
    }

    /// What commit `index` of `side` did to conflict `n`, highlighted.
    #[cfg(test)]
    pub(super) fn change(&self, side: Side, n: usize, index: usize) -> Option<&ChangeHighlights> {
        self.changes(side).get(&(n, index))
    }

    fn changes(&self, side: Side) -> &HashMap<(usize, usize), ChangeHighlights> {
        match side {
            Side::Ours => &self.ours,
            Side::Theirs => &self.theirs,
        }
    }
}

/// The lines of a change that have a number on one side, as runs of
/// consecutive numbers.
fn runs(lines: &[ChangeLine], number: fn(&ChangeLine) -> Option<u32>) -> Vec<(u32, Vec<&str>)> {
    let mut runs: Vec<(u32, Vec<&str>)> = Vec::new();
    for line in lines {
        let Some(n) = number(line) else { continue };
        match runs.last_mut() {
            Some((first, run)) if *first + run.len() as u32 == n => run.push(&line.text),
            _ => runs.push((n, vec![line.text.as_str()])),
        }
    }
    runs
}

/// What the card shows of a side besides its history: its name, and its
/// version of the file highlighted, with where the conflict's lines start
/// in it.
pub(super) struct SideView<'a> {
    pub name: &'a str,
    pub highlights: Option<&'a Highlights>,
    pub start: usize,
}

/// The tint of a side: the accent for ours, teal for theirs.
pub(super) fn tint(side: Side) -> u32 {
    match side {
        Side::Ours => color::ACCENT,
        Side::Theirs => color::BRANCH_TEAL,
    }
}

/// Text in a side's colour, for its commit ids.
pub(super) fn tint_text(side: Side) -> u32 {
    match side {
        Side::Ours => color::CHIP_TEXT,
        Side::Theirs => color::PULL_REQUEST_OPEN_TEXT,
    }
}

pub(super) fn short(commit: &HistoryCommit) -> String {
    commit.id.to_hex_with_len(7).to_string()
}

/// One side's history and what it did to the current conflict.
struct SideStory<'a> {
    side: Side,
    name: &'a str,
    history: &'a SideHistory,
    conflict: usize,
    view: SideView<'a>,
    changes: &'a HashMap<(usize, usize), ChangeHighlights>,
}

impl SideStory<'_> {
    /// Indices of the commits that touched the conflict, newest first.
    fn touching(&self) -> impl Iterator<Item = usize> + '_ {
        self.history
            .conflicts
            .get(self.conflict)
            .into_iter()
            .flat_map(|conflict| conflict.touched_by.iter().map(|(index, _)| *index))
    }

    fn change(&self, index: usize) -> Option<&[ChangeLine]> {
        let conflict = self.history.conflicts.get(self.conflict)?;
        conflict
            .touched_by
            .iter()
            .find(|(touching, _)| *touching == index)
            .map(|(_, lines)| lines.as_slice())
    }
}

/// The small pill linking a line to the commit that last changed it.
pub(super) fn line_credit(side: Side, commit: &HistoryCommit) -> Div {
    div()
        .flex_none()
        .mr(px(10.))
        .px(px(5.))
        .rounded(px(3.))
        .text_size(px(10.5))
        .line_height(px(16.))
        .bg(theme::with_alpha(tint(side), 0.08))
        .text_color(rgb(tint_text(side)))
        .child(short(commit))
}

impl Conflicts {
    /// Shows commit `index` of `side` under the lanes, or hides it when it
    /// is shown already.
    pub(super) fn choose_story_commit(&mut self, side: Side, index: usize, cx: &mut Context<Self>) {
        self.story_commit = match self.story_commit {
            Some(chosen) if chosen == (side, index) => None,
            _ => Some((side, index)),
        };
        self.story_open = true;
        cx.notify();
    }

    /// The card above the sides of conflict `conflict`: still reading,
    /// failed, or the history read.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_story(
        &self,
        history: Option<&Result<Arc<FileHistory>, SharedString>>,
        sides: (SideView<'_>, SideView<'_>),
        n: usize,
        conflict: &Conflict,
        mono: &SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let card = div()
            .flex_none()
            .v_flex()
            .rounded(px(9.))
            .border_1()
            .border_color(rgb(color::DIFF_CARD_BORDER))
            .bg(rgb(color::DIFF_CARD))
            .overflow_hidden();
        let history = match history {
            None => {
                return card
                    .child(self.story_header(
                        "Reading the history of these lines…".into(),
                        false,
                        cx,
                    ))
                    .into_any_element();
            }
            Some(Err(error)) => {
                return card
                    .child(self.story_header(
                        format!("Could not read the history: {error}").into(),
                        false,
                        cx,
                    ))
                    .into_any_element();
            }
            Some(Ok(history)) => history,
        };
        let file = history;
        let history = &file.history;
        let (ours_view, theirs_view) = sides;
        let ours = SideStory {
            side: Side::Ours,
            name: ours_view.name,
            history: &history.ours,
            conflict: n,
            view: ours_view,
            changes: file.changes(Side::Ours),
        };
        let theirs = SideStory {
            side: Side::Theirs,
            name: theirs_view.name,
            history: &history.theirs,
            conflict: n,
            view: theirs_view,
            changes: file.changes(Side::Theirs),
        };
        let both = ours.touching().next().is_some() && theirs.touching().next().is_some();
        let base = history.base.as_ref().map(short);
        let summary: SharedString = match (&base, both) {
            (Some(base), true) => format!(
                "Both branches changed these lines since they split at {base}. \
                 Click a commit to see what it did."
            ),
            (Some(base), false) => format!(
                "These are the commits since the branches split at {base}. \
                 Click a commit to see what it did."
            ),
            (None, _) => "The branches share no history.".to_owned(),
        }
        .into();
        let header = self.story_header(summary, true, cx);
        if !self.story_open {
            return card.child(header).into_any_element();
        }
        let now = time::now();
        let fork = base.is_some();
        let lanes = div()
            .id("story-lanes")
            .h_flex()
            // The scroll area fills this height: without it, it has none,
            // and no swipe reaches it.
            .h(LANE_HEIGHT * 2. + px(24.))
            .px(px(14.))
            .py(px(12.))
            .when_some(history.base.as_ref(), |this, base| {
                this.child(base_box(base, mono))
                    .child(
                        div()
                            .flex_none()
                            .w(px(10.))
                            .h(px(2.))
                            .bg(rgb(color::REMOTE_LABEL_BORDER)),
                    )
                    .child(fork_lines())
            })
            .child(
                div()
                    .v_flex()
                    .flex_grow(1.)
                    .flex_shrink_0()
                    .child(self.lane(&ours, fork, now, mono, cx))
                    .child(self.lane(&theirs, fork, now, mono, cx)),
            )
            .overflow_x_scrollbar();
        let detail = self.story_commit.and_then(|(side, index)| {
            let story = if side == Side::Ours { &ours } else { &theirs };
            let commit = story.history.commits.get(index)?;
            let change = story.change(index)?;
            let highlights = story.changes.get(&(n, index));
            Some(self.commit_detail(story, commit, change, highlights, mono, cx))
        });
        card.child(header)
            .child(lanes)
            .when_some(detail, |this, detail| {
                this.child(detail)
                    .child(compare(&ours, &theirs, conflict, mono))
            })
            .into_any_element()
    }

    fn story_header(
        &self,
        summary: SharedString,
        collapsible: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let legend_dot = |filled: bool| {
            div().flex_none().size(px(9.)).rounded_full().map(|this| {
                if filled {
                    this.bg(rgb(color::ACCENT))
                } else {
                    this.border_2().border_color(rgb(color::TEXT_MUTED))
                }
            })
        };
        div()
            .h_flex()
            .gap(px(8.))
            .px(px(12.))
            .py(px(8.))
            .bg(rgb(color::DIFF_FILE_HEADER))
            .when(collapsible && self.story_open, |this| {
                this.border_b_1().border_color(rgb(color::DIFF_CARD_BORDER))
            })
            .text_size(px(12.5))
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Why these lines conflict"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(px(12.))
                    .text_color(rgb(color::TEXT_MUTED))
                    .child(summary),
            )
            .when(collapsible, |this| {
                this.when(self.story_open, |this| {
                    this.child(
                        div()
                            .flex_none()
                            .h_flex()
                            .gap(px(14.))
                            .whitespace_nowrap()
                            .text_size(px(11.5))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(
                                div()
                                    .h_flex()
                                    .gap(px(5.))
                                    .child(legend_dot(true))
                                    .child("touches this conflict"),
                            )
                            .child(
                                div()
                                    .h_flex()
                                    .gap(px(5.))
                                    .child(legend_dot(false))
                                    .child("other commits"),
                            ),
                    )
                })
                .child(
                    Button::new("story-toggle")
                        .ghost()
                        .xsmall()
                        .icon(if self.story_open {
                            IconName::ChevronUp
                        } else {
                            IconName::ChevronDown
                        })
                        .tooltip(if self.story_open {
                            "Hide the history"
                        } else {
                            "Show the history"
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.story_open = !this.story_open;
                            cx.notify();
                        })),
                )
            })
    }

    /// A side's lane: its commits oldest first, those that touched the
    /// conflict in full and the rest dimmed or counted, then its branch.
    fn lane(
        &self,
        story: &SideStory<'_>,
        fork: bool,
        now: i64,
        mono: &SharedString,
        cx: &mut Context<Self>,
    ) -> Div {
        let tint = tint(story.side);
        let link = |width: Pixels| div().flex_none().w(width).h(px(2.)).bg(rgb(tint));
        let node = |filled: bool| {
            div()
                .flex_none()
                .size(px(10.))
                .rounded_full()
                .border_2()
                .border_color(rgb(tint))
                .bg(rgb(if filled { tint } else { color::DIFF_CARD }))
        };
        let commits = &story.history.commits;
        let mut touching: Vec<usize> = story.touching().collect();
        let hidden = touching.len().saturating_sub(MAX_SHOWN);
        touching.truncate(MAX_SHOWN);
        // Oldest first, as the lane reads from the base.
        touching.reverse();

        // The commits between index `older` (exclusive) and `newer`
        // (exclusive, or the tip when `None`), indices counting from the
        // newest: one is shown dimmed, more are counted.
        let gap_id = match story.side {
            Side::Ours => "story-gap-ours",
            Side::Theirs => "story-gap-theirs",
        };
        // The commits between index `older` (exclusive) and `newer`
        // (exclusive, or the tip when `None`), indices counting from the
        // newest: a stretch of the line with a badge counting them, and
        // their summaries on hover.
        let gap = |older: usize, newer: Option<usize>| -> Option<AnyElement> {
            let first = newer.map_or(0, |newer| newer + 1);
            let count = older.saturating_sub(first);
            if count == 0 {
                return None;
            }
            let label = match (count, hidden > 0 && newer.is_some()) {
                (1, _) => "1 other commit".to_owned(),
                (n, true) => format!("{n} earlier commits"),
                (n, false) => format!("{n} other commits"),
            };
            let summaries: Vec<String> = commits[first..older]
                .iter()
                .take(GAP_TOOLTIP_COMMITS)
                .map(|commit| format!("{}  {}", short(commit), commit.summary))
                .chain((count > GAP_TOOLTIP_COMMITS).then(|| "…".to_owned()))
                .collect();
            let tooltip: SharedString = summaries.join("\n").into();
            Some(
                div()
                    .id((gap_id, first))
                    .flex_none()
                    .relative()
                    .w(GAP_WIDTH)
                    .h(LANE_HEIGHT)
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .top(LANE_HEIGHT / 2. - px(1.))
                            .h(px(2.))
                            .bg(rgb(tint)),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .top(LANE_HEIGHT / 2. - px(5.))
                            .h_flex()
                            .justify_center()
                            .child(node(false)),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .top(px(2.))
                            .h_flex()
                            .justify_center()
                            .child(
                                div()
                                    .px(px(6.))
                                    .rounded(px(4.))
                                    .border_1()
                                    .border_color(rgb(color::DIFF_CARD_BORDER))
                                    .bg(rgb(color::DIFF_FILE_HEADER))
                                    .text_size(px(10.5))
                                    .line_height(px(15.))
                                    .text_color(rgb(color::TEXT_MUTED))
                                    .whitespace_nowrap()
                                    .child(label),
                            ),
                    )
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .into_any_element(),
            )
        };
        let mut items: Vec<AnyElement> = Vec::new();
        let mut older = commits.len();
        for &index in &touching {
            if let Some(element) = gap(older, Some(index)) {
                items.push(element);
            }
            let side = story.side;
            let selected = self.story_commit == Some((side, index));
            let id = match side {
                Side::Ours => "story-ours",
                Side::Theirs => "story-theirs",
            };
            items.push(node(true).into_any_element());
            items.push(link(px(8.)).into_any_element());
            items.push(
                commit_chip(side, &commits[index], true, selected, now, mono)
                    .id((id, index))
                    .cursor_pointer()
                    .on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.choose_story_commit(side, index, cx)
                        }),
                    )
                    .into_any_element(),
            );
            items.push(link(px(14.)).into_any_element());
            older = index;
        }
        if let Some(element) = gap(older, None) {
            items.push(element);
        }
        let note = if touching.is_empty() {
            Some(if commits.is_empty() {
                "no commits since the split"
            } else {
                "none of these commits changed these lines directly"
            })
        } else if story.history.truncated {
            Some("only the newest commits are followed")
        } else {
            None
        };
        div()
            .h_flex()
            .h(LANE_HEIGHT)
            .when(fork, |this| this.child(link(px(14.))))
            .children(items)
            .child(
                div()
                    .flex_1()
                    .min_w(px(20.))
                    .h(px(2.))
                    .bg(theme::with_alpha(tint, 0.5)),
            )
            .when_some(note, |this, note| {
                this.child(
                    div()
                        .flex_none()
                        .px(px(8.))
                        .whitespace_nowrap()
                        .text_size(px(11.5))
                        .text_color(rgb(color::TEXT_MUTED))
                        .child(note),
                )
            })
            .child(
                div()
                    .flex_none()
                    .h(px(20.))
                    .ml(px(8.))
                    .px(px(7.))
                    .rounded(px(4.))
                    .h_flex()
                    .text_size(theme::SMALL_TEXT_SIZE)
                    .font_weight(FontWeight::MEDIUM)
                    .bg(theme::with_alpha(tint, 0.16))
                    .text_color(rgb(tint_text(story.side)))
                    .whitespace_nowrap()
                    .child(story.name.to_owned()),
            )
    }

    /// The chosen commit: its message, and what it did to the conflict.
    fn commit_detail(
        &self,
        story: &SideStory<'_>,
        commit: &HistoryCommit,
        change: &[ChangeLine],
        highlights: Option<&ChangeHighlights>,
        mono: &SharedString,
        cx: &mut Context<Self>,
    ) -> Div {
        let side = story.side;
        let id = commit.id;
        let full = id.to_hex().to_string();
        let message = div()
            .v_flex()
            .w(px(360.))
            .flex_none()
            .gap(px(8.))
            .px(px(14.))
            .py(px(12.))
            .border_r_1()
            .border_color(rgb(color::DIFF_CARD_BORDER))
            .child(
                div()
                    .h_flex()
                    .gap(px(8.))
                    .text_size(px(12.))
                    .child(match side {
                        Side::Ours => source_tag("OURS", Source::Ours),
                        Side::Theirs => source_tag("THEIRS", Source::Theirs),
                    })
                    .child(
                        div()
                            .flex_none()
                            .font_family(mono.clone())
                            .text_color(rgb(tint_text(side)))
                            .child(short(commit)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(format!(
                                "{} · {}",
                                commit.author.name,
                                time::absolute(commit.author.time)
                            )),
                    ),
            )
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(commit.summary.clone()),
            )
            .when_some(commit.body.clone(), |this, body| {
                this.child(
                    div()
                        .text_size(px(12.5))
                        .line_height(px(19.))
                        .text_color(rgb(color::TEXT_SECONDARY))
                        .child(body),
                )
            })
            .child(
                div()
                    .h_flex()
                    .gap(px(6.))
                    .mt(px(4.))
                    .child(
                        Button::new("story-show-commit")
                            .small()
                            .label("Show in history")
                            .tooltip("Back to the history with this commit selected; the merge stays in progress")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(ConflictsEvent::ShowCommit(id))
                            })),
                    )
                    .child(
                        Button::new("story-copy-sha")
                            .ghost()
                            .small()
                            .label("Copy SHA")
                            .text_color(rgb(color::TEXT_MUTED))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(full.clone()))
                            }),
                    ),
            );
        let rows = change
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let (sign, background, sign_color) = match line.kind {
                    LineKind::Context => (" ", None, color::DIFF_LINE_NUMBER),
                    LineKind::Added => ("+", Some(color::DIFF_ADDED), color::DIFF_ADDED),
                    LineKind::Removed => ("−", Some(color::DIFF_REMOVED), color::DIFF_REMOVED),
                };
                let row = CodeRow::code(
                    Some(sign_key(sign, sign_color)),
                    line.new.or(line.old).map(|n| n as usize),
                    &line.text,
                    highlights.and_then(|highlights| highlights.spans(line)),
                );
                let row = row.debug(format!("story-change-line-{i}"));
                match background {
                    Some(background) => row.bg(theme::with_alpha(background, 0.11)),
                    None => row,
                }
            })
            .collect();
        div()
            .h_flex()
            .items_start()
            .border_t_1()
            .border_color(rgb(color::DIFF_CARD_BORDER))
            .child(message)
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .px(px(12.))
                            .pt(px(8.))
                            .pb(px(2.))
                            .text_size(px(11.5))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child("What this commit did to these lines"),
                    )
                    .child(code_rows("story-change", rows, mono)),
            )
    }
}

/// A commit on a lane: avatar, summary, and its id, author and age.
fn commit_chip(
    side: Side,
    commit: &HistoryCommit,
    touching: bool,
    selected: bool,
    now: i64,
    mono: &SharedString,
) -> Div {
    let tint = tint(side);
    div()
        .flex_none()
        .h_flex()
        .gap(px(8.))
        .w(ITEM_WIDTH)
        .h(ITEM_HEIGHT)
        .pl(px(6.))
        .pr(px(10.))
        .rounded(px(7.))
        .border_1()
        .map(|this| {
            if selected {
                this.border_color(rgb(tint))
                    .bg(theme::with_alpha(tint, 0.13))
            } else if touching {
                this.border_color(theme::with_alpha(tint, 0.4))
                    .bg(theme::with_alpha(tint, 0.06))
                    .hover(|style| style.bg(theme::with_alpha(tint, 0.1)))
            } else {
                this.border_color(rgb(color::DIFF_CARD_BORDER))
                    .bg(rgb(color::DIFF_FILE_HEADER))
            }
        })
        .child(avatar(&commit.author.name, px(20.)))
        .child(
            div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_size(px(12.5))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(commit.summary.clone()),
                )
                .child(
                    div()
                        .h_flex()
                        .gap(px(4.))
                        .text_size(px(11.))
                        .text_color(rgb(color::TEXT_MUTED))
                        .whitespace_nowrap()
                        .child(
                            div()
                                .flex_none()
                                .font_family(mono.clone())
                                .text_color(rgb(tint_text(side)))
                                .child(short(commit)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(format!(
                                    "· {} · {}",
                                    commit.author.name,
                                    time::relative(commit.author.time, now)
                                )),
                        ),
                ),
        )
}

/// The commit both sides started from.
fn base_box(base: &HistoryCommit, mono: &SharedString) -> Div {
    div()
        .flex_none()
        .v_flex()
        .gap(px(2.))
        .w(px(130.))
        .px(px(9.))
        .py(px(7.))
        .rounded(px(7.))
        .border_1()
        .border_dashed()
        .border_color(rgb(color::REMOTE_LABEL_BORDER))
        .bg(rgb(color::DIFF_FILE_HEADER))
        .child(
            div()
                .text_size(px(10.5))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(color::TEXT_FAINT))
                .child("COMMON BASE"),
        )
        .child(
            div()
                .font_family(mono.clone())
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_SECONDARY))
                .child(short(base)),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(color::TEXT_MUTED))
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(base.summary.clone()),
        )
}

/// The fork from the base to the two lanes: from the middle, an elbow up
/// to our lane in our colour and one down to theirs.
fn fork_lines() -> Div {
    let half = LANE_HEIGHT / 2.;
    let elbow = || {
        div()
            .absolute()
            .left_0()
            .w(px(18.))
            .h(half + px(1.))
            .border_l_2()
    };
    div()
        .flex_none()
        .relative()
        .w(px(18.))
        .h(LANE_HEIGHT * 2.)
        .child(
            elbow()
                .top(half - px(1.))
                .border_t_2()
                .rounded_tl(px(10.))
                .border_color(rgb(color::ACCENT)),
        )
        .child(
            elbow()
                .top(LANE_HEIGHT)
                .border_b_2()
                .rounded_bl(px(10.))
                .border_color(rgb(color::BRANCH_TEAL)),
        )
}

/// What each side made of the lines, under the commit shown: its lines and
/// the commits that changed them, by their summaries.
fn compare(
    ours: &SideStory<'_>,
    theirs: &SideStory<'_>,
    conflict: &Conflict,
    mono: &SharedString,
) -> Div {
    let column = |story: &SideStory<'_>, lines: &[String]| {
        let (tag, source) = match story.side {
            Side::Ours => ("OURS", Source::Ours),
            Side::Theirs => ("THEIRS", Source::Theirs),
        };
        let reasons = story.touching().filter_map(|index| {
            let commit = story.history.commits.get(index)?;
            Some(
                div()
                    .h_flex()
                    .gap(px(6.))
                    .text_size(px(12.))
                    .child(
                        div()
                            .flex_none()
                            .font_family(mono.clone())
                            .text_color(rgb(tint_text(story.side)))
                            .child(short(commit)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(rgb(color::TEXT_SECONDARY))
                            .child(commit.summary.clone()),
                    ),
            )
        });
        div()
            .v_flex()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .h_flex()
                    .gap(px(6.))
                    .px(px(12.))
                    .pt(px(8.))
                    .pb(px(2.))
                    .text_size(px(11.5))
                    .text_color(rgb(color::TEXT_MUTED))
                    .child(source_tag(tag, source))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(format!("{} changed these lines to", story.name)),
                    ),
            )
            .child(code_rows(
                match story.side {
                    Side::Ours => "story-compare-ours",
                    Side::Theirs => "story-compare-theirs",
                },
                lines
                    .iter()
                    .enumerate()
                    .map(|(i, line)| {
                        let line_spans = spans(story.view.highlights, story.view.start + i, line);
                        CodeRow::code(
                            Some(sign_key("+", color::DIFF_ADDED)),
                            None,
                            line,
                            line_spans,
                        )
                        .bg(theme::with_alpha(color::DIFF_ADDED, 0.11))
                    })
                    .chain(
                        lines
                            .is_empty()
                            .then(|| CodeRow::note(empty_note("Removed them"))),
                    )
                    .collect(),
                mono,
            ))
            .child(
                div()
                    .v_flex()
                    .gap(px(2.))
                    .px(px(12.))
                    .pb(px(10.))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child("Why, by the commits' own words:"),
                    )
                    .children(reasons),
            )
    };
    div()
        .h_flex()
        .items_start()
        .border_t_1()
        .border_color(rgb(color::DIFF_CARD_BORDER))
        .child(column(ours, &conflict.ours))
        .child(
            column(theirs, &conflict.theirs)
                .border_l_1()
                .border_color(rgb(color::DIFF_CARD_BORDER)),
        )
}

/// The sign of a diff line, in the key column of its row.
fn sign_key(sign: &'static str, color: u32) -> AnyElement {
    div().text_color(rgb(color)).child(sign).into_any_element()
}
