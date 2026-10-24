//! Reviewing one commit: its message, then the diff of every file it changed,
//! one after another, as in the `Commit.dc.html` artboard.

use std::sync::Arc;

use gitez_git::{
    ChangeKind, Commit, CommitDetails, DiffContent, DiffLine, FileDiff, LineKind, RefLabel,
    Repository,
};
use gpui_kit::component::button::*;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::labels;
use crate::theme::{self, color};
use crate::time;

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
}

/// Height of one diff line.
const LINE_HEIGHT: Pixels = px(21.);
/// Space between file cards.
const CARD_GAP: Pixels = px(18.);
/// Rows rendered beyond the visible ones, so fast scrolling stays smooth.
const OVERDRAW: Pixels = px(600.);
/// Spaces a tab is shown as.
const TAB: &str = "    ";

/// A row of the diff list.
#[derive(Debug, Clone, Copy)]
enum Row {
    /// The header of the file at this index.
    File(usize),
    /// A hunk header: file and hunk index.
    Hunk(usize, usize),
    /// A diff line: file, hunk and line index.
    Line(usize, usize, usize),
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
            | Row::Notice(file)
            | Row::FileEnd(file) => Some(file),
            Row::End => None,
        }
    }
}

pub struct Review {
    focus_handle: FocusHandle,
    commit: Commit,
    labels: Vec<RefLabel>,
    /// The commits before and after this one in the history, if any.
    newer: Option<Commit>,
    older: Option<Commit>,
    details: Option<Arc<CommitDetails>>,
    /// `None` while reading.
    files: Option<Result<Arc<Vec<FileDiff>>, SharedString>>,
    rows: Vec<Row>,
    /// Index in `rows` of each file's header.
    file_rows: Vec<usize>,
    list: ListState,
    /// The file at the top of the list.
    current_file: usize,
    _load: Task<()>,
}

impl EventEmitter<ReviewEvent> for Review {}

impl Review {
    /// Reviews `commit` of `repository`, reading its diff in the background.
    pub fn new(
        repository: &Repository,
        commit: Commit,
        labels: Vec<RefLabel>,
        (newer, older): (Option<Commit>, Option<Commit>),
        cx: &mut Context<Self>,
    ) -> Self {
        let list = ListState::new(0, ListAlignment::Top, OVERDRAW);
        let this = cx.entity().downgrade();
        list.set_scroll_handler(move |event, _, cx| {
            let first = event.visible_range.start;
            this.update(cx, |this, cx| this.scrolled_to(first, cx)).ok();
        });

        let repository = repository.clone();
        let id = commit.id;
        let load = cx.spawn(async move |this, cx| {
            let (details, files) = cx
                .background_executor()
                .spawn(async move { (repository.commit_details(id), repository.commit_diff(id)) })
                .await;
            this.update(cx, |this, cx| this.finish(details.ok(), files, cx))
                .ok();
        });

        Self {
            focus_handle: cx.focus_handle(),
            commit,
            labels,
            newer,
            older,
            details: None,
            files: None,
            rows: Vec::new(),
            file_rows: Vec::new(),
            list,
            current_file: 0,
            _load: load,
        }
    }

    fn finish(
        &mut self,
        details: Option<CommitDetails>,
        files: gitez_git::Result<Vec<FileDiff>>,
        cx: &mut Context<Self>,
    ) {
        self.details = details.map(Arc::new);
        match files {
            Ok(files) => {
                (self.rows, self.file_rows) = rows(&files);
                self.list.reset(self.rows.len());
                self.files = Some(Ok(Arc::new(files)));
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

    fn scrolled_to(&mut self, first_row: usize, cx: &mut Context<Self>) {
        let file = self
            .rows
            .get(first_row)
            .and_then(|row| row.file())
            .unwrap_or(self.file_rows.len().saturating_sub(1));
        if file != self.current_file {
            self.current_file = file;
            cx.notify();
        }
    }

    fn scroll_to_file(&mut self, file: usize, cx: &mut Context<Self>) {
        if let Some(&row) = self.file_rows.get(file) {
            self.list.scroll_to(ListOffset {
                item_ix: row,
                offset_in_item: px(0.),
            });
            self.current_file = file;
            cx.notify();
        }
    }

    fn close(&mut self, _: &Close, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ReviewEvent::Close);
    }

    fn newer(&mut self, _: &Newer, _: &mut Window, cx: &mut Context<Self>) {
        if self.newer.is_some() {
            cx.emit(ReviewEvent::Newer);
        }
    }

    fn older(&mut self, _: &Older, _: &mut Window, cx: &mut Context<Self>) {
        if self.older.is_some() {
            cx.emit(ReviewEvent::Older);
        }
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(Ok(files)) = &self.files else {
            return div().into_any_element();
        };
        let mono = cx.theme().mono_font_family.clone();
        match self.rows.get(index).copied() {
            Some(Row::File(file)) => file_header(&files[file], mono).into_any_element(),
            Some(Row::Hunk(file, hunk)) => {
                let DiffContent::Text(hunks) = &files[file].content else {
                    return div().into_any_element();
                };
                let hunk = &hunks[hunk];
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
                    .into_any_element()
            }
            Some(Row::Line(file, hunk, line)) => {
                let DiffContent::Text(hunks) = &files[file].content else {
                    return div().into_any_element();
                };
                diff_line(&hunks[hunk].lines[line], mono).into_any_element()
            }
            Some(Row::Notice(file)) => card_row()
                .px(px(14.))
                .py(px(10.))
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_MUTED))
                .child(notice(&files[file]))
                .into_any_element(),
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
            Some(Row::End) => self.render_end(cx).into_any_element(),
            None => div().into_any_element(),
        }
    }

    /// "End of commit · Next: <older commit>".
    fn render_end(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
            .child("End of commit")
            .when_some(self.older.as_ref(), |this, older| {
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
        let mono = cx.theme().mono_font_family.clone();
        let commit = &self.commit;
        let details = self.details.as_deref();
        let parent = match commit.parents.as_slice() {
            [] => "root commit".to_owned(),
            [parent, ..] => format!("parent {}", parent.to_hex_with_len(7)),
        };
        let mut when = format!("committed {}", time::absolute(commit.author.time));
        if details.is_some_and(|details| details.signed) {
            when.push_str(" · signed");
        }
        let body = details
            .map(|details| details.body.clone())
            .filter(|body| !body.is_empty());
        let totals = match &self.files {
            Some(Ok(files)) => Some(totals(files)),
            _ => None,
        };

        div()
            .v_flex()
            .flex_none()
            .gap(px(14.))
            .pt(px(14.))
            .px(px(24.))
            .pb(px(16.))
            .bg(rgb(color::PANEL))
            .border_b_1()
            .border_color(rgb(color::BORDER))
            .child(
                div()
                    .h_flex()
                    .gap(px(10.))
                    .text_size(px(12.5))
                    .child(
                        Button::new("review-back")
                            .ghost()
                            .h(px(30.))
                            .label("History")
                            .tooltip("Back to the history (Esc)")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ReviewEvent::Close))),
                    )
                    .children(self.labels.first().map(|label| {
                        div()
                            .h_flex()
                            .gap(px(10.))
                            .child(muted("/"))
                            .child(labels::ref_label(label))
                    }))
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
                                    .disabled(self.newer.is_none())
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
                                    .disabled(self.older.is_none())
                                    .on_click(
                                        cx.listener(|_, _, _, cx| cx.emit(ReviewEvent::Older)),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .v_flex()
                    .gap(px(8.))
                    .max_w(px(900.))
                    .child(
                        div()
                            .text_size(px(22.))
                            .line_height(relative(1.3))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(commit.summary.clone()),
                    )
                    .when_some(body, |this, body| {
                        this.child(
                            div()
                                .max_w(px(760.))
                                .line_height(relative(1.55))
                                .text_color(rgb(color::AUTHOR_TEXT))
                                .child(body),
                        )
                    })
                    .child(
                        div()
                            .h_flex()
                            .flex_wrap()
                            .gap(px(10.))
                            .text_size(px(12.5))
                            .child(labels::avatar(&commit.author.name, px(22.)))
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(commit.author.name.clone()),
                            )
                            .child(muted(when))
                            .when_some(totals, |this, (files, added, removed)| {
                                this.child(muted("·"))
                                    .child(match files {
                                        1 => "1 file".to_owned(),
                                        files => format!("{files} files"),
                                    })
                                    .child(
                                        div()
                                            .font_family(mono.clone())
                                            .text_color(rgb(color::DIFF_ADDED))
                                            .child(format!("+{added}")),
                                    )
                                    .child(
                                        div()
                                            .font_family(mono.clone())
                                            .text_color(rgb(color::DIFF_REMOVED))
                                            .child(format!("−{removed}")),
                                    )
                            }),
                    ),
            )
    }

    /// The changed files by directory; clicking one scrolls to it.
    fn render_files(&self, files: &[FileDiff], cx: &mut Context<Self>) -> impl IntoElement {
        let mut items: Vec<AnyElement> = Vec::new();
        let mut directory = None;
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
                            .child(name.to_owned()),
                    )
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
                    .px(px(10.))
                    .pb(px(8.))
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(color::TEXT_FAINT))
                    .child("FILES"),
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
            Some(Ok(files)) if files.is_empty() => {
                message("This commit changes no files.").into_any_element()
            }
            Some(Ok(files)) => {
                let files = files.clone();
                div()
                    .h_flex()
                    .items_start()
                    .flex_1()
                    .min_h_0()
                    .gap(px(20.))
                    .pt(px(20.))
                    .px(px(24.))
                    .child(self.render_files(&files, cx))
                    .child(
                        list(
                            self.list.clone(),
                            cx.processor(|this, index, _window, cx| this.render_row(index, cx)),
                        )
                        .flex_1()
                        .min_w_0()
                        .h_full(),
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
            .v_flex()
            .size_full()
            .bg(rgb(color::BACKGROUND))
            .child(self.render_header(cx))
            .child(body)
    }
}

/// The rows of the diff list for `files`, and the row of each file's header.
fn rows(files: &[FileDiff]) -> (Vec<Row>, Vec<usize>) {
    let mut rows = Vec::new();
    let mut file_rows = Vec::with_capacity(files.len());
    for (file, diff) in files.iter().enumerate() {
        file_rows.push(rows.len());
        rows.push(Row::File(file));
        match &diff.content {
            DiffContent::Text(hunks) => {
                for (hunk_index, hunk) in hunks.iter().enumerate() {
                    rows.push(Row::Hunk(file, hunk_index));
                    rows.extend(
                        (0..hunk.lines.len()).map(|line| Row::Line(file, hunk_index, line)),
                    );
                }
            }
            DiffContent::TooLarge(_) | DiffContent::Binary | DiffContent::Unchanged => {
                rows.push(Row::Notice(file));
            }
        }
        rows.push(Row::FileEnd(file));
    }
    rows.push(Row::End);
    (rows, file_rows)
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

/// The top of a file's card: path, what happened to it and line counts.
fn file_header(file: &FileDiff, mono: SharedString) -> impl IntoElement {
    let (directory, name) = split_path(&file.path);
    let (label, label_color) = match &file.kind {
        ChangeKind::Added => (Some("new file".to_owned()), color::DIFF_ADDED_TEXT),
        ChangeKind::Deleted => (Some("deleted".to_owned()), color::DIFF_REMOVED),
        ChangeKind::Renamed { from } => (Some(format!("renamed from {from}")), color::BRANCH_BLUE),
        ChangeKind::Copied { from } => (Some(format!("copied from {from}")), color::BRANCH_BLUE),
        ChangeKind::Modified => (None, color::TEXT),
    };
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
        .rounded_t(px(8.))
        .text_size(px(12.5))
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
        .when_some(label, |this, label| {
            this.child(
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
                    .child(label),
            )
        })
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
}

/// One line of a diff: old and new line numbers, `+` or `−`, and the text.
fn diff_line(line: &DiffLine, mono: SharedString) -> impl IntoElement {
    let (sign, tint) = match line.kind {
        LineKind::Context => ("", None),
        LineKind::Added => ("+", Some(color::DIFF_ADDED)),
        LineKind::Removed => ("−", Some(color::DIFF_REMOVED)),
    };
    let number = |number: Option<u32>| {
        div()
            .w(px(48.))
            .flex_none()
            .h_full()
            .pr(px(10.))
            .text_right()
            .text_color(rgb(color::DIFF_LINE_NUMBER))
            .when_some(tint, |this, tint| this.bg(theme::with_alpha(tint, 0.08)))
            .children(number.map(|number| number.to_string()))
    };

    card_row()
        .h(LINE_HEIGHT)
        .font_family(mono)
        .text_size(theme::MONO_TEXT_SIZE)
        .when_some(tint, |this, tint| this.bg(theme::with_alpha(tint, 0.10)))
        .child(number(line.old))
        .child(number(line.new))
        .child(
            div()
                .w(px(20.))
                .flex_none()
                .text_center()
                .text_color(rgb(tint.unwrap_or(color::DIFF_LINE_NUMBER)))
                .child(sign),
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
                .child(line.text.replace('\t', TAB)),
        )
        .when(line.no_newline, |this| {
            this.child(
                div()
                    .flex_none()
                    .pr(px(12.))
                    .text_size(theme::SMALL_TEXT_SIZE)
                    .text_color(rgb(color::TEXT_FAINT))
                    .child("no newline at end of file"),
            )
        })
}

/// Why `file` shows no lines.
fn notice(file: &FileDiff) -> String {
    match (&file.content, &file.kind) {
        (DiffContent::TooLarge(counts), _) => format!(
            "Large diff not shown: {} lines added, {} removed.",
            counts.added, counts.removed
        ),
        (DiffContent::Binary, _) => "Binary file not shown.".to_owned(),
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
