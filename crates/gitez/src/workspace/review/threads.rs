//! Comments on lines of a local pull request: choosing lines, writing a
//! comment, and the threads shown under the lines they are on, as in the
//! `LocalPR.dc.html` artboard.

use std::collections::{HashMap, HashSet};

use gitez_git::{DiffContent, FileDiff};
use gitez_review::{Anchor, Resolution, ResolvedBy, Side, Thread, Timestamp};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{GapAt, Review, Row, Subject, card_row, gap, labels, muted};
use crate::theme::{self, color};
use crate::time;

/// Space left of a thread card: the line numbers and the sign column.
const THREAD_INDENT: Pixels = px(116.);
/// Size of the "+" that starts a comment on a line.
const ADD_BUTTON_SIZE: Pixels = px(16.);

/// What the comment tools of a pull request review keep.
#[derive(Default)]
pub(super) struct Comments {
    /// The name comments are shown under.
    pub user_name: Option<String>,
    /// Lines being chosen by dragging from a "+".
    pub selecting: Option<Selecting>,
    /// The comment being written.
    pub composer: Option<Composer>,
    /// Replies being written, by thread id.
    pub replies: HashMap<u32, Draft>,
    /// The comment being edited: its thread, its id, and the new text.
    pub editing: Option<(u32, u32, Draft)>,
    /// Resolved threads shown in full, by id.
    pub shown_resolved: HashSet<u32>,
}

/// Lines of one side of a file being chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Selecting {
    pub file: usize,
    pub side: Side,
    /// Where the drag started and where it is now; either may be larger.
    pub start: u32,
    pub end: u32,
}

impl Selecting {
    fn lines(self) -> (u32, u32) {
        (self.start.min(self.end), self.start.max(self.end))
    }
}

/// A new comment on lines `first..=last` of one side of a file.
pub(super) struct Composer {
    pub path: String,
    pub side: Side,
    pub lines: (u32, u32),
    pub draft: Draft,
}

/// A text box being written in, submitted with ⌘↵.
pub(super) struct Draft {
    pub input: Entity<TextareaState>,
    _events: Subscription,
}

/// What a thread placed in the rows is: a thread, or the comment box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Placed {
    pub file: usize,
    /// The side and line it goes under; `None` for the end of the file.
    pub below: Option<(Side, u32)>,
    pub row: Row,
}

/// `rows` with each of `placed` after the row of its line, or before the
/// end of its file's card when no row has that line. `numbers` gives a
/// row's old and new line numbers. Nothing is placed in a file `hidden`
/// tells is collapsed. Also returns the row of each file's header.
pub(super) fn with_comments(
    rows: &[Row],
    placed: &[Placed],
    numbers: impl Fn(Row) -> (Option<u32>, Option<u32>),
    hidden: impl Fn(usize) -> bool,
) -> (Vec<Row>, Vec<usize>) {
    let mut left: Vec<Option<&Placed>> = placed.iter().map(Some).collect();
    let mut result = Vec::with_capacity(rows.len() + placed.len());
    for &row in rows {
        if let Row::FileEnd(file) = row
            && !hidden(file)
        {
            for item in left.iter_mut() {
                if let Some(placed) = item.filter(|placed| placed.file == file) {
                    result.push(placed.row);
                    *item = None;
                }
            }
        }
        result.push(row);
        let Some(file) = row.file() else {
            continue;
        };
        if hidden(file) {
            continue;
        }
        let (old, new) = numbers(row);
        for item in left.iter_mut() {
            let Some(placed) = *item else {
                continue;
            };
            let here = match placed.below {
                Some((Side::Old, line)) => old == Some(line),
                Some((Side::New, line)) => new == Some(line),
                None => false,
            };
            if placed.file == file && here {
                result.push(placed.row);
                *item = None;
            }
        }
    }
    let file_rows = result
        .iter()
        .enumerate()
        .filter_map(|(index, row)| matches!(row, Row::File(_)).then_some(index))
        .collect();
    (result, file_rows)
}

/// The old and new line number of a line row of `files`; `None` for other
/// rows and a side the line is not on.
pub(super) fn row_numbers(files: &[FileDiff], row: Row) -> (Option<u32>, Option<u32>) {
    let hunks = |file: usize| match files.get(file).map(|file| &file.content) {
        Some(DiffContent::Text(hunks)) => Some(hunks),
        _ => None,
    };
    match row {
        Row::Line(file, hunk, line) => hunks(file)
            .and_then(|hunks| hunks.get(hunk)?.lines.get(line))
            .map_or((None, None), |line| (line.old, line.new)),
        Row::Pair(file, hunk, old, new) => {
            let lines = hunks(file)
                .and_then(|hunks| hunks.get(hunk))
                .map(|hunk| &hunk.lines);
            let get = |index: Option<usize>| index.and_then(|index| lines?.get(index));
            (
                get(old).and_then(|line| line.old),
                get(new).and_then(|line| line.new),
            )
        }
        Row::Context(file, at, line) => hunks(file)
            .and_then(|hunks| gap(hunks, at))
            .map_or((None, None), |gap| {
                (Some(gap.old + line), Some(gap.new + line))
            }),
        _ => (None, None),
    }
}

/// "line 7", "lines 33–34", or "removed lines 33–34".
fn describe_lines(side: Side, (first, last): (u32, u32)) -> String {
    let removed = match side {
        Side::Old => "removed ",
        Side::New => "",
    };
    if first == last {
        format!("{removed}line {first}")
    } else {
        format!("{removed}lines {first}–{last}")
    }
}

/// How long ago `at` was, as the history shows times.
fn ago(at: Timestamp) -> String {
    time::relative(
        gitez_git::Timestamp {
            seconds: at.as_second(),
            offset: 0,
        },
        time::now(),
    )
}

impl Review {
    fn comments(&self) -> Option<&Comments> {
        match &self.subject {
            Subject::PullRequest(subject) => Some(&subject.comments),
            _ => None,
        }
    }

    fn comments_mut(&mut self) -> Option<&mut Comments> {
        match &mut self.subject {
            Subject::PullRequest(subject) => Some(&mut subject.comments),
            _ => None,
        }
    }

    /// The threads, the comment box and where each goes, for the files
    /// shown.
    pub(super) fn placed_comments(&self, files: &[FileDiff]) -> Vec<Placed> {
        let Subject::PullRequest(subject) = &self.subject else {
            return Vec::new();
        };
        let file_of = |path: &str| files.iter().position(|file| file.path == path);
        let head = &subject.pull_request.head.commit;
        let mut placed: Vec<Placed> = subject
            .pull_request
            .threads
            .iter()
            .filter_map(|thread| {
                let anchor = &thread.anchor;
                let file = file_of(&anchor.path)?;
                // Line numbers on the head's side are those of the commit
                // the thread was made on; on the base's side they hold.
                let current = anchor.side == Side::Old || &anchor.commit == head;
                Some(Placed {
                    file,
                    below: current.then_some((anchor.side, anchor.lines.1)),
                    row: Row::Thread(file, thread.id),
                })
            })
            .collect();
        if let Some(composer) = &subject.comments.composer
            && let Some(file) = file_of(&composer.path)
        {
            placed.push(Placed {
                file,
                below: Some((composer.side, composer.lines.1)),
                row: Row::Composer(file),
            });
        }
        placed
    }

    /// The side and lines highlighted in file `file`: being chosen, or
    /// being commented on.
    fn highlighted(&self, file: usize) -> Option<(Side, (u32, u32))> {
        let comments = self.comments()?;
        if let Some(selecting) = comments.selecting {
            return (selecting.file == file).then(|| (selecting.side, selecting.lines()));
        }
        let composer = comments.composer.as_ref()?;
        let Some(Ok(files)) = &self.files else {
            return None;
        };
        (files.get(file)?.path == composer.path).then_some((composer.side, composer.lines))
    }

    /// A line row of a pull request, with the "+" that starts a comment on
    /// it, the highlight of chosen lines, and dragging to choose more.
    pub(super) fn commentable(
        &self,
        element: AnyElement,
        index: usize,
        row: Row,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (Some(Ok(files)), Some(comments)) = (&self.files, self.comments()) else {
            return element;
        };
        let Some(file) = row.file() else {
            return element;
        };
        let (old, new) = row_numbers(files, row);
        let split = matches!(row, Row::Pair(..))
            || matches!(row, Row::Context(..))
                && super::ReviewSettings::get(cx).layout == super::DiffLayout::Split;
        let selecting = comments.selecting.is_some();
        let highlight = self.highlighted(file).and_then(|(side, (first, last))| {
            let number = match side {
                Side::Old => old,
                Side::New => new,
            };
            number
                .filter(|number| (first..=last).contains(number))
                .map(|_| side)
        });
        // In the unified layout a removed line is on the old side, every
        // other line on the new side.
        let unified_side = match (old, new) {
            (Some(old), None) => Some((Side::Old, old)),
            (_, Some(new)) => Some((Side::New, new)),
            (None, None) => None,
        };
        let add = |id: (&'static str, usize), side: Side, line: u32, cx: &mut Context<Self>| {
            div()
                .id(id)
                .absolute()
                .top(px(2.5))
                .size(ADD_BUTTON_SIZE)
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .bg(rgb(color::ACCENT))
                .text_color(rgb(color::ON_ACCENT))
                .cursor_pointer()
                .opacity(0.)
                .child(Icon::new(IconName::Plus).size(px(12.)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.start_selecting(file, side, line, cx);
                    }),
                )
        };
        let mut wrapper = div()
            .id(("commentable", index))
            .relative()
            .w_full()
            .child(element)
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                if event.dragging() {
                    this.extend_selection(file, old, new, cx);
                }
            }));
        if let Some(side) = highlight {
            let (left, width) = match (split, side) {
                (false, _) => (relative(0.), relative(1.)),
                (true, Side::Old) => (relative(0.), relative(0.5)),
                (true, Side::New) => (relative(0.5), relative(0.5)),
            };
            wrapper = wrapper.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(left)
                    .w(width)
                    .border_l_2()
                    .border_color(rgb(color::ACCENT))
                    .bg(theme::with_alpha(color::ACCENT, 0.12)),
            );
        }
        if selecting {
            return wrapper.into_any_element();
        }
        if split {
            // A "+" on each side, over its sign column.
            for (name, half_left, side, number) in [
                ("comment-old", relative(0.), Side::Old, old),
                ("comment-new", relative(0.5), Side::New, new),
            ] {
                let Some(number) = number else {
                    continue;
                };
                let group: SharedString = format!("{name}-{index}").into();
                wrapper = wrapper.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(half_left)
                        .w(relative(0.5))
                        .group(group.clone())
                        .child(
                            add((name, index), side, number, cx)
                                .left(px(50.))
                                .group_hover(group, |style| style.opacity(1.)),
                        ),
                );
            }
        } else if let Some((side, number)) = unified_side {
            let group: SharedString = format!("comment-{index}").into();
            wrapper = wrapper.group(group.clone()).child(
                add(("comment", index), side, number, cx)
                    .left(px(98.))
                    .group_hover(group, |style| style.opacity(1.)),
            );
        }
        wrapper.into_any_element()
    }

    fn start_selecting(&mut self, file: usize, side: Side, line: u32, cx: &mut Context<Self>) {
        if let Some(comments) = self.comments_mut() {
            comments.selecting = Some(Selecting {
                file,
                side,
                start: line,
                end: line,
            });
            cx.notify();
        }
    }

    fn extend_selection(
        &mut self,
        file: usize,
        old: Option<u32>,
        new: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        let Some(Selecting {
            file: chosen,
            side,
            end,
            ..
        }) = self
            .comments_mut()
            .and_then(|comments| comments.selecting.as_mut())
        else {
            return;
        };
        let number = match side {
            Side::Old => old,
            Side::New => new,
        };
        if *chosen == file
            && let Some(number) = number
            && *end != number
        {
            *end = number;
            cx.notify();
        }
    }

    /// Ends choosing lines by opening the comment box on them.
    pub(super) fn finish_selecting(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selecting) = self
            .comments_mut()
            .and_then(|comments| comments.selecting.take())
        else {
            return;
        };
        let Some(Ok(files)) = &self.files else {
            return;
        };
        let Some(path) = files.get(selecting.file).map(|file| file.path.clone()) else {
            return;
        };
        let lines = selecting.lines();
        let placeholder = format!("Comment on {}", describe_lines(selecting.side, lines));
        let draft = self.draft(placeholder, window, cx, |this, window, cx| {
            this.submit_comment(window, cx)
        });
        window.focus(&draft.input.focus_handle(cx), cx);
        if let Some(comments) = self.comments_mut() {
            comments.composer = Some(Composer {
                path,
                side: selecting.side,
                lines,
                draft,
            });
        }
        self.rebuild_keeping_scroll(cx);
    }

    /// A text box that calls `submit` on ⌘↵.
    fn draft(
        &mut self,
        placeholder: String,
        window: &mut Window,
        cx: &mut Context<Self>,
        submit: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Draft {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 12)
                .placeholder(placeholder)
        });
        let events = cx.subscribe_in(&input, window, move |this, _, event, window, cx| {
            match event {
                InputEvent::PressEnter {
                    secondary: true, ..
                } => submit(this, window, cx),
                // The buttons follow whether there is text.
                InputEvent::Change => cx.notify(),
                _ => {}
            }
        });
        Draft {
            input,
            _events: events,
        }
    }

    fn rebuild_keeping_scroll(&mut self, cx: &mut Context<Self>) {
        let anchor = self.scroll_anchor();
        self.rebuild_rows(anchor, cx);
        cx.notify();
    }

    /// Changes the pull request with `change`, then saves it and shows the
    /// change.
    fn change_threads(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut gitez_review::PullRequest, &mut Comments),
    ) {
        let Subject::PullRequest(subject) = &mut self.subject else {
            return;
        };
        change(&mut subject.pull_request, &mut subject.comments);
        self.save_pull_request(cx);
        self.rebuild_keeping_scroll(cx);
    }

    /// Starts a thread with the comment being written.
    fn submit_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(composer) = self
            .comments()
            .and_then(|comments| comments.composer.as_ref())
        else {
            return;
        };
        let body = composer.draft.input.read(cx).value().trim().to_owned();
        if body.is_empty() {
            return;
        }
        let (path, side, lines) = (composer.path.clone(), composer.side, composer.lines);
        let excerpt = self.excerpt(&path, side, lines, cx);
        self.change_threads(cx, |pull_request, comments| {
            let anchor = Anchor {
                commit: pull_request.head.commit.clone(),
                path,
                side,
                lines,
                excerpt,
            };
            pull_request.add_thread(anchor, body, Timestamp::now());
            comments.composer = None;
        });
        window.focus(&self.focus_handle, cx);
    }

    /// The text of lines `first..=last` on `side` of the file at `path`, as
    /// far as the rows show them.
    fn excerpt(&self, path: &str, side: Side, (first, last): (u32, u32), cx: &App) -> Vec<String> {
        let Some(Ok(files)) = &self.files else {
            return Vec::new();
        };
        let Some(file) = files.iter().position(|file| file.path == path) else {
            return Vec::new();
        };
        let start = self.file_rows.get(file).copied().unwrap_or(0);
        let end = self
            .file_rows
            .get(file + 1)
            .copied()
            .unwrap_or(self.rows.len());
        let mut lines: Vec<(u32, String)> = self.rows[start..end]
            .iter()
            .filter_map(|&row| self.row_text(files, row, side, cx))
            .filter(|(number, _)| (first..=last).contains(number))
            .collect();
        lines.sort_by_key(|(number, _)| *number);
        lines.dedup_by_key(|(number, _)| *number);
        lines.into_iter().map(|(_, text)| text).collect()
    }

    /// The number and text of the line on `side` in `row`, if it has one.
    fn row_text(
        &self,
        files: &[FileDiff],
        row: Row,
        side: Side,
        _cx: &App,
    ) -> Option<(u32, String)> {
        let pick = |line: &gitez_git::DiffLine| {
            let number = match side {
                Side::Old => line.old,
                Side::New => line.new,
            }?;
            Some((number, line.text.trim_end_matches('\r').to_owned()))
        };
        let DiffContent::Text(hunks) = &files.get(row.file()?)?.content else {
            return None;
        };
        match row {
            Row::Line(_, hunk, line) => pick(hunks.get(hunk)?.lines.get(line)?),
            Row::Pair(_, hunk, old, new) => {
                let index = match side {
                    Side::Old => old,
                    Side::New => new,
                }?;
                pick(hunks.get(hunk)?.lines.get(index)?)
            }
            Row::Context(_, at, line) => {
                let (GapAt::Before(index) | GapAt::After(index)) = at;
                let gap = gap(hunks, at)?;
                let source = &hunks.get(index)?.new_side.as_ref()?.source;
                let new = gap.new + line;
                let text = self.sources.get(source)?.get(new as usize - 1)?.clone();
                let number = match side {
                    Side::Old => gap.old + line,
                    Side::New => new,
                };
                Some((number, text))
            }
            _ => None,
        }
    }

    fn cancel_comment(&mut self, cx: &mut Context<Self>) {
        if let Some(comments) = self.comments_mut() {
            comments.composer = None;
        }
        self.rebuild_keeping_scroll(cx);
    }

    /// Closes what is being written, if anything: the comment box first,
    /// then the replies. Returns whether anything was open.
    pub(super) fn cancel_writing(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(comments) = self.comments_mut() else {
            return false;
        };
        let open = comments.composer.is_some()
            || !comments.replies.is_empty()
            || comments.editing.is_some();
        if comments.composer.take().is_none() && comments.editing.take().is_none() {
            comments.replies.clear();
        }
        if open {
            self.rebuild_keeping_scroll(cx);
        }
        open
    }

    fn start_reply(&mut self, thread: u32, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.draft("Reply".to_owned(), window, cx, move |this, window, cx| {
            this.submit_reply(thread, window, cx)
        });
        window.focus(&draft.input.focus_handle(cx), cx);
        if let Some(comments) = self.comments_mut() {
            comments.replies.insert(thread, draft);
        }
        self.rebuild_keeping_scroll(cx);
    }

    fn submit_reply(&mut self, thread: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self
            .comments()
            .and_then(|comments| comments.replies.get(&thread))
        else {
            return;
        };
        let body = draft.input.read(cx).value().trim().to_owned();
        if body.is_empty() {
            return;
        }
        self.change_threads(cx, |pull_request, comments| {
            // The thread exists: it is shown.
            pull_request.reply(thread, body, Timestamp::now()).ok();
            comments.replies.remove(&thread);
        });
        window.focus(&self.focus_handle, cx);
    }

    fn cancel_reply(&mut self, thread: u32, cx: &mut Context<Self>) {
        if let Some(comments) = self.comments_mut() {
            comments.replies.remove(&thread);
        }
        self.rebuild_keeping_scroll(cx);
    }

    /// Starts editing comment `comment` of thread `thread`.
    fn start_editing(
        &mut self,
        thread: u32,
        comment: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(body) = (match &self.subject {
            Subject::PullRequest(subject) => subject
                .pull_request
                .thread(thread)
                .and_then(|thread| thread.comments.iter().find(|c| c.id == comment))
                .map(|comment| comment.body.clone()),
            _ => None,
        }) else {
            return;
        };
        let draft = self.draft("Comment".to_owned(), window, cx, move |this, window, cx| {
            this.submit_edit(window, cx)
        });
        draft
            .input
            .update(cx, |input, cx| input.set_value(body, window, cx));
        window.focus(&draft.input.focus_handle(cx), cx);
        if let Some(comments) = self.comments_mut() {
            comments.editing = Some((thread, comment, draft));
        }
        self.rebuild_keeping_scroll(cx);
    }

    /// Saves the comment being edited.
    fn submit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((thread, comment, draft)) = self
            .comments()
            .and_then(|comments| comments.editing.as_ref())
        else {
            return;
        };
        let (thread, comment) = (*thread, *comment);
        let body = draft.input.read(cx).value().trim().to_owned();
        if body.is_empty() {
            return;
        }
        self.change_threads(cx, |pull_request, comments| {
            // The comment exists: it is shown.
            pull_request
                .edit_comment(thread, comment, body, Timestamp::now())
                .ok();
            comments.editing = None;
        });
        window.focus(&self.focus_handle, cx);
    }

    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(comments) = self.comments_mut() {
            comments.editing = None;
        }
        self.rebuild_keeping_scroll(cx);
    }

    /// Deletes comment `comment` of thread `thread`, and the thread with
    /// its first comment.
    fn delete_comment(&mut self, thread: u32, comment: u32, cx: &mut Context<Self>) {
        self.change_threads(cx, |pull_request, comments| {
            let gone = pull_request
                .delete_comment(thread, comment, Timestamp::now())
                .unwrap_or(false);
            if comments
                .editing
                .as_ref()
                .is_some_and(|(t, c, _)| *t == thread && (*c == comment || gone))
            {
                comments.editing = None;
            }
            if gone {
                comments.replies.remove(&thread);
                comments.shown_resolved.remove(&thread);
            }
        });
    }

    fn set_resolved(&mut self, thread: u32, resolved: bool, cx: &mut Context<Self>) {
        self.change_threads(cx, |pull_request, comments| {
            let resolution = resolved.then(|| Resolution {
                by: ResolvedBy::You,
                commit: pull_request.head.commit.clone(),
                at: Timestamp::now(),
            });
            pull_request.set_resolution(thread, resolution).ok();
            comments.shown_resolved.remove(&thread);
            comments.replies.remove(&thread);
        });
    }

    fn set_in_prompt(&mut self, thread: u32, in_prompt: bool, cx: &mut Context<Self>) {
        self.change_threads(cx, |pull_request, _| {
            pull_request.set_in_prompt(thread, in_prompt).ok();
        });
    }

    fn show_resolved(&mut self, thread: u32, shown: bool, cx: &mut Context<Self>) {
        if let Some(comments) = self.comments_mut() {
            if shown {
                comments.shown_resolved.insert(thread);
            } else {
                comments.shown_resolved.remove(&thread);
            }
        }
        self.rebuild_keeping_scroll(cx);
    }

    /// The comment box, under the lines it comments on.
    pub(super) fn render_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(composer) = self
            .comments()
            .and_then(|comments| comments.composer.as_ref())
        else {
            return div().into_any_element();
        };
        let empty = composer.draft.input.read(cx).value().trim().is_empty();
        card_row()
            .child(
                thread_card()
                    .p(px(12.))
                    .gap(px(10.))
                    .child(Textarea::new(&composer.draft.input).bg(rgb(color::BACKGROUND)))
                    .child(
                        div()
                            .h_flex()
                            .gap(px(8.))
                            .child(muted(format!(
                                "On {} · ⌘↵ to comment",
                                describe_lines(composer.side, composer.lines)
                            )))
                            .child(
                                div()
                                    .h_flex()
                                    .ml_auto()
                                    .gap(px(6.))
                                    .child(
                                        Button::new("comment-cancel")
                                            .ghost()
                                            .h(px(30.))
                                            .label("Cancel")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.cancel_comment(cx)
                                            })),
                                    )
                                    .child(
                                        Button::new("comment-submit")
                                            .primary()
                                            .h(px(30.))
                                            .label("Comment")
                                            .disabled(empty)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.submit_comment(window, cx)
                                            })),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Thread `id`: its comments and what can be done with it, or one line
    /// once it is resolved.
    pub(super) fn render_thread(&self, id: u32, cx: &mut Context<Self>) -> AnyElement {
        let Subject::PullRequest(subject) = &self.subject else {
            return div().into_any_element();
        };
        let Some(thread) = subject.pull_request.thread(id) else {
            return div().into_any_element();
        };
        let comments = &subject.comments;
        let outdated = thread.anchor.side == Side::New
            && thread.anchor.commit != subject.pull_request.head.commit;
        let card = if thread.resolved.is_some() && !comments.shown_resolved.contains(&id) {
            self.render_resolved(thread, cx).into_any_element()
        } else {
            let number = subject
                .pull_request
                .prompt_threads()
                .iter()
                .position(|included| included.id == id)
                .map(|index| index + 1);
            self.render_open(thread, number, outdated, comments, cx)
                .into_any_element()
        };
        card_row().child(card).into_any_element()
    }

    /// A resolved thread folded to one line.
    fn render_resolved(&self, thread: &Thread, cx: &mut Context<Self>) -> impl IntoElement {
        let id = thread.id;
        let first = thread
            .comments
            .first()
            .and_then(|comment| comment.body.lines().next())
            .unwrap_or_default()
            .to_owned();
        let how = match &thread.resolved {
            Some(Resolution {
                by: ResolvedBy::Auto,
                commit,
                ..
            }) => format!(
                "resolved: the lines changed in {}",
                commit.get(..7).unwrap_or(commit)
            ),
            _ => "resolved".to_owned(),
        };
        thread_card()
            .id(("thread-resolved", id as usize))
            .h_flex()
            .gap(px(8.))
            .px(px(14.))
            .py(px(8.))
            .text_size(px(12.5))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.show_resolved(id, true, cx)))
            .child(
                Icon::new(IconName::ChevronRight)
                    .size(px(12.))
                    .text_color(rgb(color::TEXT_MUTED)),
            )
            .child(
                Icon::new(IconName::Check)
                    .size(px(14.))
                    .text_color(rgb(color::DIFF_ADDED)),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(rgb(color::TEXT_MUTED))
                    .child(format!(
                        "{} {how}",
                        capitalize(&describe_lines(thread.anchor.side, thread.anchor.lines))
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(rgb(color::TEXT_SECONDARY))
                    .child(first),
            )
    }

    /// A thread shown in full: its comments, a reply being written, and
    /// its actions.
    fn render_open(
        &self,
        thread: &Thread,
        number: Option<usize>,
        outdated: bool,
        comments: &Comments,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = thread.id;
        let name = comments
            .user_name
            .clone()
            .unwrap_or_else(|| "You".to_owned());
        let resolved = thread.resolved.is_some();
        let reply = comments.replies.get(&id);
        let mut card = thread_card();
        for (index, comment) in thread.comments.iter().enumerate() {
            let edited = if comment.edited.is_some() {
                " · edited"
            } else {
                ""
            };
            let detail = if index == 0 {
                let lines = describe_lines(thread.anchor.side, thread.anchor.lines);
                let outdated = if outdated { " · outdated" } else { "" };
                format!("on {lines} · {}{edited}{outdated}", ago(comment.created))
            } else {
                format!("{}{edited}", ago(comment.created))
            };
            let comment_id = comment.id;
            let editing = comments
                .editing
                .as_ref()
                .filter(|(thread, comment, _)| *thread == id && *comment == comment_id)
                .map(|(_, _, draft)| draft);
            let group: SharedString = format!("comment-{comment_id}").into();
            let tools = div()
                .h_flex()
                .gap(px(2.))
                .opacity(0.)
                .group_hover(group.clone(), |style| style.opacity(1.))
                .child(
                    Button::new(("comment-edit", comment_id as usize))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Pencil)
                        .tooltip("Edit")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.start_editing(id, comment_id, window, cx)
                        })),
                )
                .child(
                    Button::new(("comment-delete", comment_id as usize))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Delete)
                        .tooltip(if index == 0 {
                            "Delete the thread"
                        } else {
                            "Delete"
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.delete_comment(id, comment_id, cx)
                        })),
                );
            let body = match editing {
                Some(draft) => {
                    let empty = draft.input.read(cx).value().trim().is_empty();
                    div()
                        .v_flex()
                        .gap(px(8.))
                        .child(Textarea::new(&draft.input).bg(rgb(color::BACKGROUND)))
                        .child(
                            div().h_flex().gap(px(6.)).child(muted("⌘↵ to save")).child(
                                div()
                                    .h_flex()
                                    .ml_auto()
                                    .gap(px(6.))
                                    .child(
                                        Button::new("comment-edit-cancel")
                                            .ghost()
                                            .h(px(30.))
                                            .label("Cancel")
                                            .on_click(
                                                cx.listener(|this, _, _, cx| this.cancel_edit(cx)),
                                            ),
                                    )
                                    .child(
                                        Button::new("comment-edit-save")
                                            .primary()
                                            .h(px(30.))
                                            .label("Save")
                                            .disabled(empty)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.submit_edit(window, cx)
                                            })),
                                    ),
                            ),
                        )
                }
                None => div()
                    .line_height(relative(1.55))
                    .text_color(rgb(color::TEXT))
                    .child(comment.body.clone()),
            };
            card = card.child(
                div()
                    .h_flex()
                    .items_start()
                    .group(group)
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(12.))
                    .when(index > 0, |this| {
                        this.border_t_1().border_color(rgb(color::THREAD_DIVIDER))
                    })
                    .child(labels::avatar(&name, px(22.)))
                    .child(
                        div()
                            .v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(6.))
                            .child(
                                div()
                                    .h_flex()
                                    .gap(px(8.))
                                    .text_size(px(12.))
                                    .child(div().font_weight(FontWeight::SEMIBOLD).child("You"))
                                    .child(muted(detail))
                                    .child(
                                        div()
                                            .h_flex()
                                            .ml_auto()
                                            .gap(px(6.))
                                            .when(editing.is_none(), |this| this.child(tools))
                                            .when_some(
                                                number.filter(|_| index == 0),
                                                |this, number| this.child(chip(number.to_string())),
                                            ),
                                    ),
                            )
                            .child(body),
                    ),
            );
        }
        if let Some(reply) = reply {
            let empty = reply.input.read(cx).value().trim().is_empty();
            card =
                card.child(
                    div()
                        .v_flex()
                        .gap(px(8.))
                        .px(px(14.))
                        .py(px(10.))
                        .border_t_1()
                        .border_color(rgb(color::THREAD_DIVIDER))
                        .child(Textarea::new(&reply.input).bg(rgb(color::BACKGROUND)))
                        .child(
                            div()
                                .h_flex()
                                .justify_end()
                                .gap(px(6.))
                                .child(
                                    Button::new(("reply-cancel", id as usize))
                                        .ghost()
                                        .h(px(30.))
                                        .label("Cancel")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.cancel_reply(id, cx)
                                        })),
                                )
                                .child(
                                    Button::new(("reply-submit", id as usize))
                                        .primary()
                                        .h(px(30.))
                                        .label("Reply")
                                        .disabled(empty)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.submit_reply(id, window, cx)
                                        })),
                                ),
                        ),
                );
        }
        card.child(
            div()
                .h_flex()
                .gap(px(8.))
                .px(px(14.))
                .pt(px(8.))
                .pb(px(10.))
                .border_t_1()
                .border_color(rgb(color::THREAD_DIVIDER))
                .map(|this| {
                    if resolved {
                        this.child(
                            Button::new(("thread-hide", id as usize))
                                .ghost()
                                .small()
                                .label("Hide")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.show_resolved(id, false, cx)
                                })),
                        )
                    } else {
                        this.child(
                            Checkbox::new(("in-prompt", id as usize))
                                .label("In agent prompt")
                                .checked(thread.in_prompt)
                                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                    this.set_in_prompt(id, *checked, cx)
                                })),
                        )
                    }
                })
                .child(
                    div()
                        .h_flex()
                        .ml_auto()
                        .gap(px(6.))
                        .when(reply.is_none() && !resolved, |this| {
                            this.child(
                                Button::new(("thread-reply", id as usize))
                                    .ghost()
                                    .h(px(30.))
                                    .label("Reply")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.start_reply(id, window, cx)
                                    })),
                            )
                        })
                        .child(
                            Button::new(("thread-resolve", id as usize))
                                .h(px(30.))
                                .label(if resolved { "Reopen" } else { "Resolve" })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_resolved(id, !resolved, cx)
                                })),
                        ),
                ),
        )
    }

    /// How many open threads each file has, by path.
    pub(super) fn open_threads_by_path(&self) -> HashMap<&str, usize> {
        let mut counts = HashMap::new();
        if let Subject::PullRequest(subject) = &self.subject {
            for thread in subject.pull_request.threads.iter().filter(|t| t.is_open()) {
                *counts.entry(thread.anchor.path.as_str()).or_default() += 1;
            }
        }
        counts
    }
}

/// The frame of a thread, indented past the line numbers.
fn thread_card() -> Div {
    div()
        .v_flex()
        .flex_1()
        .min_w_0()
        .mt(px(8.))
        .mb(px(10.))
        .ml(THREAD_INDENT)
        .mr(px(12.))
        .rounded(px(8.))
        .border_1()
        .border_color(rgb(color::THREAD_BORDER))
        .bg(rgb(color::THREAD))
        .text_size(theme::TEXT_SIZE)
}

/// A small amber pill with `text`: a comment count or a thread's number in
/// the prompt.
pub(super) fn chip(text: impl Into<SharedString>) -> Div {
    div()
        .h_flex()
        .flex_none()
        .h(px(18.))
        .px(px(6.))
        .rounded(px(9.))
        .bg(rgb(color::SELECTED))
        .text_color(rgb(color::CHIP_TEXT))
        .text_size(px(10.5))
        .child(text.into())
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{Placed, Row, Side, describe_lines, with_comments};

    #[test]
    fn threads_go_under_their_line_or_at_the_end_of_the_file() {
        // File 0: lines numbered (old, new) 1/1, 2/–, –/2; file 1: one line.
        let rows = [
            Row::File(0),
            Row::Line(0, 0, 0),
            Row::Line(0, 0, 1),
            Row::Line(0, 0, 2),
            Row::FileEnd(0),
            Row::File(1),
            Row::Line(1, 0, 0),
            Row::FileEnd(1),
            Row::End,
        ];
        let numbers = |row: Row| match row {
            Row::Line(0, 0, 0) => (Some(1), Some(1)),
            Row::Line(0, 0, 1) => (Some(2), None),
            Row::Line(0, 0, 2) => (None, Some(2)),
            Row::Line(1, 0, 0) => (Some(1), Some(1)),
            _ => (None, None),
        };
        let placed = [
            // On the removed line 2.
            Placed {
                file: 0,
                below: Some((Side::Old, 2)),
                row: Row::Thread(0, 1),
            },
            // On the added line 2.
            Placed {
                file: 0,
                below: Some((Side::New, 2)),
                row: Row::Thread(0, 2),
            },
            // Outdated: at the end.
            Placed {
                file: 0,
                below: None,
                row: Row::Thread(0, 3),
            },
            // On a line not shown: at the end too.
            Placed {
                file: 0,
                below: Some((Side::New, 40)),
                row: Row::Composer(0),
            },
            // In a collapsed file: not shown.
            Placed {
                file: 1,
                below: Some((Side::New, 1)),
                row: Row::Thread(1, 4),
            },
        ];
        let (rows, file_rows) = with_comments(&rows, &placed, numbers, |file| file == 1);
        assert_eq!(
            rows,
            [
                Row::File(0),
                Row::Line(0, 0, 0),
                Row::Line(0, 0, 1),
                Row::Thread(0, 1),
                Row::Line(0, 0, 2),
                Row::Thread(0, 2),
                Row::Thread(0, 3),
                Row::Composer(0),
                Row::FileEnd(0),
                Row::File(1),
                Row::Line(1, 0, 0),
                Row::FileEnd(1),
                Row::End,
            ]
        );
        assert_eq!(file_rows, [0, 9]);
    }

    #[test]
    fn lines_are_described_by_side_and_count() {
        assert_eq!(describe_lines(Side::New, (7, 7)), "line 7");
        assert_eq!(describe_lines(Side::New, (33, 34)), "lines 33–34");
        assert_eq!(describe_lines(Side::Old, (5, 6)), "removed lines 5–6");
    }
}
