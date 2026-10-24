//! Details of the selected commit, or of the uncommitted changes, in the
//! right-hand pane.

use std::sync::Arc;
use std::time::Duration;

use gitez_git::{
    ChangeKind, Commit, CommitDetails, FileChange, FileStatus, LineCounts, Oid, RefLabel,
    Repository, WorkingTreeStatus, WorktreeChange,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::labels;
use crate::theme::{self, color};
use crate::time;

/// How long the selection has to rest before the details are read, so that
/// moving through the history with the arrow keys reads only where it stops.
const DEBOUNCE: Duration = Duration::from_millis(40);

/// Files listed before the rest are summarised in one line.
const MAX_FILES_SHOWN: usize = 500;

/// What the detail pane asks of the window.
pub enum DetailEvent {
    /// Show the diff of the selection, scrolled to the file at this path.
    OpenFile(String),
}

/// The selected commit: SHA, parents, message, author, refs and changed files.
/// Or, with the uncommitted-changes row selected, the files they touch.
/// Clicking a file opens its diff.
pub struct DetailPane {
    working_tree: Option<Arc<WorkingTreeStatus>>,
    commit: Option<Commit>,
    labels: Vec<RefLabel>,
    /// `None` while the details are being read.
    details: Option<Result<Arc<CommitDetails>, SharedString>>,
    /// The read in flight. Dropping it before the debounce ends skips it.
    _load: Option<Task<()>>,
}

impl EventEmitter<DetailEvent> for DetailPane {}

impl DetailPane {
    pub fn new() -> Self {
        Self {
            working_tree: None,
            commit: None,
            labels: Vec::new(),
            details: None,
            _load: None,
        }
    }

    /// Shows `selection`, a commit of `repository` and the refs pointing at
    /// it, or nothing.
    pub fn show(
        &mut self,
        repository: &Repository,
        selection: Option<(Commit, Vec<RefLabel>)>,
        cx: &mut Context<Self>,
    ) {
        let Some((commit, labels)) = selection else {
            *self = Self::new();
            cx.notify();
            return;
        };
        if self
            .commit
            .as_ref()
            .is_some_and(|current| current.id == commit.id)
        {
            // Refs may have moved to or from the commit.
            if self.labels != labels {
                self.labels = labels;
                cx.notify();
            }
            return;
        }

        let id = commit.id;
        self.commit = Some(commit);
        self.labels = labels;
        self.details = None;
        let repository = repository.clone();
        self._load = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let details = cx
                .background_executor()
                .spawn(async move { repository.commit_details(id) })
                .await;
            this.update(cx, |this, cx| this.finish(id, details, cx))
                .ok();
        }));
        cx.notify();
    }

    /// Shows the uncommitted changes in `status`.
    pub fn show_working_tree(&mut self, status: Arc<WorkingTreeStatus>, cx: &mut Context<Self>) {
        *self = Self {
            working_tree: Some(status),
            ..Self::new()
        };
        cx.notify();
    }

    fn finish(
        &mut self,
        id: Oid,
        details: gitez_git::Result<CommitDetails>,
        cx: &mut Context<Self>,
    ) {
        if self.commit.as_ref().map(|commit| commit.id) != Some(id) {
            return;
        }
        self.details = Some(details.map(Arc::new).map_err(|error| {
            format!(
                "Could not read the changes: {:#}",
                anyhow::Error::from(error)
            )
            .into()
        }));
        self._load = None;
        cx.notify();
    }
}

impl Render for DetailPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = div().v_flex().size_full().bg(rgb(color::PANEL));
        let mono = cx.theme().mono_font_family.clone();
        if let Some(status) = &self.working_tree {
            return pane
                .child(
                    div()
                        .id("working-tree-detail")
                        .v_flex()
                        .flex_1()
                        .overflow_y_scroll()
                        .child(working_tree(status, mono, cx)),
                )
                .when(!status.is_clean(), |this| this.child(commit_footer()));
        }
        let Some(commit) = &self.commit else {
            return pane.child(
                div()
                    .v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap(px(6.))
                    .p(px(24.))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("No commit selected"),
                    )
                    .child(
                        div()
                            .text_color(rgb(color::TEXT_MUTED))
                            .child("Select a commit to see its details."),
                    ),
            );
        };
        let details = match &self.details {
            Some(Ok(details)) => Some(details.clone()),
            _ => None,
        };
        let error = match &self.details {
            Some(Err(error)) => Some(error.clone()),
            _ => None,
        };

        pane.child(
            div()
                .id("commit-detail")
                .v_flex()
                .flex_1()
                .overflow_y_scroll()
                .child(header(
                    commit,
                    &self.labels,
                    details.as_deref(),
                    mono.clone(),
                ))
                .when_some(error, |this, error| {
                    this.child(
                        div()
                            .p(px(16.))
                            .text_size(px(12.))
                            .text_color(rgb(color::DIFF_REMOVED))
                            .child(error),
                    )
                })
                .when_some(details, |this, details| {
                    this.child(files(&details, mono, cx))
                }),
        )
    }
}

/// "Commit changes", which opens the uncommitted changes to stage and
/// commit them.
fn commit_footer() -> impl IntoElement {
    let enter = Keystroke::parse("enter").expect("enter is a valid keystroke");
    div()
        .v_flex()
        .flex_none()
        .gap(px(8.))
        .px(px(16.))
        .py(px(14.))
        .border_t_1()
        .border_color(rgb(color::BORDER))
        .child(
            Button::new("commit-changes")
                .primary()
                .h(px(36.))
                .w_full()
                .label("Commit changes")
                .icon(Icon::new(IconName::ArrowRight).size(px(14.)))
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(super::ReviewSelected), cx)
                }),
        )
        .child(
            div()
                .h_flex()
                .justify_center()
                .gap(px(4.))
                .text_size(theme::SMALL_TEXT_SIZE)
                .text_color(rgb(color::TEXT_MUTED))
                .child("Double-click any row or press")
                .child(Kbd::new(enter)),
        )
}

/// SHA and parents, summary and body, author, and refs.
fn header(
    commit: &Commit,
    labels: &[RefLabel],
    details: Option<&CommitDetails>,
    mono: SharedString,
) -> impl IntoElement {
    let short = |id: &Oid| id.to_hex_with_len(7).to_string();
    let parents = match commit.parents.as_slice() {
        [] => "root commit".to_owned(),
        [parent] => format!("parent {}", short(parent)),
        parents => format!(
            "parents {}",
            parents.iter().map(short).collect::<Vec<_>>().join(", ")
        ),
    };
    let full_sha = commit.id.to_string();

    let mut when = time::absolute(commit.author.time);
    if details.is_some_and(|details| details.signed) {
        when.push_str(" · signed");
    }
    let committed_by = details
        .map(|details| &details.committer)
        .filter(|committer| {
            committer.name != commit.author.name || committer.email != commit.author.email
        })
        .map(|committer| format!("Committed by {}", committer.name));
    let body = details
        .map(|details| details.body.clone())
        .filter(|body| !body.is_empty());

    div()
        .v_flex()
        .gap(px(12.))
        .pt(px(16.))
        .px(px(18.))
        .pb(px(14.))
        .border_b_1()
        .border_color(rgb(color::BORDER))
        .child(
            div()
                .h_flex()
                .gap(px(8.))
                .text_size(px(12.))
                .child(
                    div()
                        .font_family(mono.clone())
                        .text_color(rgb(color::ACCENT))
                        .child(short(&commit.id)),
                )
                .child(
                    Button::new("copy-sha")
                        .ghost()
                        .size(px(24.))
                        .icon(Icon::new(IconName::Copy).size(px(13.)))
                        .accessibility_label("Copy full SHA")
                        .tooltip("Copy full SHA")
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(full_sha.clone()));
                        }),
                )
                .child(
                    div()
                        .ml_auto()
                        .font_family(mono)
                        .text_color(rgb(color::TEXT_MUTED))
                        .child(parents),
                ),
        )
        .child(
            div()
                .text_size(px(17.))
                .line_height(relative(1.35))
                .font_weight(FontWeight::SEMIBOLD)
                .child(commit.summary.clone()),
        )
        .when_some(body, |this, body| {
            this.child(
                div()
                    .text_size(px(12.5))
                    .line_height(relative(1.55))
                    .text_color(rgb(color::AUTHOR_TEXT))
                    .child(body),
            )
        })
        .child(
            div()
                .h_flex()
                .gap(px(10.))
                .child(labels::avatar(&commit.author.name, px(28.)))
                .child(
                    div()
                        .v_flex()
                        .gap(px(1.))
                        .min_w_0()
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .child(commit.author.name.clone()),
                        )
                        .child(
                            div()
                                .text_size(theme::SMALL_TEXT_SIZE)
                                .text_color(rgb(color::TEXT_MUTED))
                                .child(when),
                        )
                        .when_some(committed_by, |this, committed_by| {
                            this.child(
                                div()
                                    .text_size(theme::SMALL_TEXT_SIZE)
                                    .text_color(rgb(color::TEXT_MUTED))
                                    .child(committed_by),
                            )
                        }),
                ),
        )
        .when(!labels.is_empty(), |this| {
            this.child(
                div()
                    .h_flex()
                    .flex_wrap()
                    .gap(px(6.))
                    .children(labels.iter().map(labels::ref_label)),
            )
        })
}

/// "N files changed +A −R" and one row per file.
fn files(
    details: &CommitDetails,
    mono: SharedString,
    cx: &Context<DetailPane>,
) -> impl IntoElement {
    let (added, removed) = details.line_totals();
    let title = match details.files.len() {
        0 => "No files changed".to_owned(),
        1 => "1 file changed".to_owned(),
        count => format!("{count} files changed"),
    };
    let hidden = details.files.len().saturating_sub(MAX_FILES_SHOWN);

    div()
        .v_flex()
        .pb(px(12.))
        .child(
            div()
                .h_flex()
                .gap(px(8.))
                .pt(px(12.))
                .px(px(16.))
                .pb(px(6.))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                .child(
                    div()
                        .h_flex()
                        .ml_auto()
                        .gap(px(8.))
                        .font_family(mono.clone())
                        .text_size(px(12.))
                        .when(added > 0, |this| {
                            this.child(
                                div()
                                    .text_color(rgb(color::DIFF_ADDED))
                                    .child(format!("+{added}")),
                            )
                        })
                        .when(removed > 0, |this| {
                            this.child(
                                div()
                                    .text_color(rgb(color::DIFF_REMOVED))
                                    .child(format!("−{removed}")),
                            )
                        }),
                ),
        )
        .children(
            details
                .files
                .iter()
                .take(MAX_FILES_SHOWN)
                .enumerate()
                .map(|(index, file)| {
                    opens_file(
                        ("commit-file", index),
                        &file.path,
                        commit_file_row(file, mono.clone()),
                        cx,
                    )
                }),
        )
        .when(hidden > 0, |this| this.child(more_files(hidden)))
}

/// "Uncommitted changes", then the staged files and the other changes.
fn working_tree(
    status: &WorkingTreeStatus,
    mono: SharedString,
    cx: &Context<DetailPane>,
) -> impl IntoElement {
    let staged: Vec<&FileStatus> = status
        .files
        .iter()
        .filter(|file| file.staged.is_some())
        .collect();
    let unstaged: Vec<&FileStatus> = status
        .files
        .iter()
        .filter(|file| file.unstaged.is_some())
        .collect();
    let group = |title: &'static str,
                 files: &[&FileStatus],
                 row: &dyn Fn(usize, &FileStatus) -> AnyElement| {
        let hidden = files.len().saturating_sub(MAX_FILES_SHOWN);
        div()
            .v_flex()
            .pb(px(8.))
            .child(
                div()
                    .h_flex()
                    .gap(px(8.))
                    .pt(px(12.))
                    .px(px(16.))
                    .pb(px(6.))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(files.len().to_string()),
                    ),
            )
            .children(
                files
                    .iter()
                    .take(MAX_FILES_SHOWN)
                    .enumerate()
                    .map(|(index, file)| row(index, file)),
            )
            .when(hidden > 0, |this| this.child(more_files(hidden)))
    };

    div()
        .v_flex()
        .child(
            div()
                .v_flex()
                .gap(px(4.))
                .pt(px(16.))
                .px(px(18.))
                .pb(px(14.))
                .border_b_1()
                .border_color(rgb(color::BORDER))
                .child(
                    div()
                        .text_size(px(17.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Uncommitted changes"),
                )
                .child(
                    div()
                        .text_size(theme::SMALL_TEXT_SIZE)
                        .text_color(rgb(color::TEXT_MUTED))
                        .child(labels::working_tree_summary(status)),
                ),
        )
        .when(!staged.is_empty(), |this| {
            this.child(group("Staged", &staged, &|index, file| {
                let kind = file.staged.as_ref().unwrap_or(&ChangeKind::Modified);
                let (letter, letter_color) = labels::change_letter(kind);
                let row = file_row(
                    letter,
                    letter_color,
                    &file.path,
                    rename_source(kind),
                    None,
                    mono.clone(),
                );
                opens_file(("staged-file", index), &file.path, row, cx).into_any_element()
            }))
        })
        .when(!unstaged.is_empty(), |this| {
            this.child(group("Changes", &unstaged, &|index, file| {
                let (letter, letter_color) =
                    worktree_letter(file.unstaged.unwrap_or(WorktreeChange::Modified));
                let row = file_row(letter, letter_color, &file.path, None, None, mono.clone());
                opens_file(("unstaged-file", index), &file.path, row, cx).into_any_element()
            }))
        })
}

/// `row`, which opens the diff of the file at `path` when clicked.
fn opens_file(
    id: impl Into<ElementId>,
    path: &str,
    row: impl IntoElement,
    cx: &Context<DetailPane>,
) -> impl IntoElement {
    let path = path.to_owned();
    div()
        .id(id)
        .cursor_pointer()
        .child(row)
        .on_click(cx.listener(move |_, _, _, cx| cx.emit(DetailEvent::OpenFile(path.clone()))))
}

fn more_files(hidden: usize) -> impl IntoElement {
    div()
        .px(px(16.))
        .py(px(6.))
        .text_size(px(12.))
        .text_color(rgb(color::TEXT_MUTED))
        .child(format!("and {hidden} more files"))
}

/// The status letter of an unstaged change: `?` untracked, `U` unmerged.
fn worktree_letter(change: WorktreeChange) -> (&'static str, u32) {
    match change {
        WorktreeChange::Modified => ("M", color::ACCENT),
        WorktreeChange::Deleted => ("D", color::DIFF_REMOVED),
        WorktreeChange::Untracked => ("?", color::DIFF_ADDED),
        WorktreeChange::Conflicted => ("U", color::DIFF_REMOVED),
    }
}

fn rename_source(kind: &ChangeKind) -> Option<String> {
    match kind {
        ChangeKind::Renamed { from } | ChangeKind::Copied { from } => Some(from.clone()),
        _ => None,
    }
}

fn commit_file_row(file: &FileChange, mono: SharedString) -> impl IntoElement {
    let (letter, letter_color) = labels::change_letter(&file.kind);
    file_row(
        letter,
        letter_color,
        &file.path,
        rename_source(&file.kind),
        file.lines,
        mono,
    )
}

/// One changed file: its status letter, the old path of a rename, the path
/// with the directory muted, and lines added and removed if known.
fn file_row(
    letter: &'static str,
    letter_color: u32,
    path: &str,
    from: Option<String>,
    lines: Option<LineCounts>,
    mono: SharedString,
) -> impl IntoElement {
    let (directory, name) = match path.rfind('/') {
        Some(slash) => path.split_at(slash + 1),
        None => ("", path),
    };
    let count = |prefix: &str, value: usize, hex: u32| {
        (value > 0).then(|| div().text_color(rgb(hex)).child(format!("{prefix}{value}")))
    };

    div()
        .h_flex()
        .h(px(30.))
        .px(px(16.))
        .gap(px(8.))
        .text_size(px(12.))
        .hover(|style| style.bg(rgb(color::NAV_HOVER)))
        .child(
            div()
                .w(px(12.))
                .flex_none()
                .font_family(mono.clone())
                .text_color(rgb(letter_color))
                .child(letter),
        )
        .child(
            div()
                .h_flex()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .when_some(from, |this, from| {
                    this.child(
                        div()
                            .flex_none()
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(format!("{from} → ")),
                    )
                })
                .child(
                    div()
                        .flex_none()
                        .text_color(rgb(color::TEXT_MUTED))
                        .child(directory.to_owned()),
                )
                .child(div().min_w_0().text_ellipsis().child(name.to_owned())),
        )
        .when_some(lines, |this, lines| {
            this.child(
                div()
                    .h_flex()
                    .flex_none()
                    .gap(px(6.))
                    .font_family(mono)
                    .text_size(px(11.))
                    .children(count("+", lines.added, color::DIFF_ADDED))
                    .children(count("−", lines.removed, color::DIFF_REMOVED)),
            )
        })
}
