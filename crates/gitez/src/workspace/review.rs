//! Reviewing one commit, or the uncommitted changes: the commit's message,
//! then the diff of every changed file, one after another, as in the
//! `Commit.dc.html` artboard. The uncommitted changes can be staged and
//! committed from here.

use std::sync::Arc;

use gitez_command::Git;
use gitez_git::{
    ChangeKind, Commit, CommitDetails, DiffContent, DiffLine, FileDiff, FileStatus, LineKind,
    RefKind, RefLabel, Repository, WorkingTreeStatus, WorktreeChange,
};
use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::labels;
use crate::theme::{self, color};
use crate::time;

actions!(review, [Close, Newer, Older, CommitStaged]);

/// Key bindings of the review.
pub fn key_bindings() -> Vec<KeyBinding> {
    const CONTEXT: Option<&str> = Some("Review");
    vec![
        KeyBinding::new("escape", Close, CONTEXT),
        KeyBinding::new("[", Newer, CONTEXT),
        KeyBinding::new("]", Older, CONTEXT),
        KeyBinding::new("secondary-enter", CommitStaged, CONTEXT),
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
    /// A commit was made. `everything` is set when no uncommitted changes
    /// were left out of it.
    Committed { everything: bool },
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

/// What is being reviewed.
enum Subject {
    Commit(Box<CommitSubject>),
    WorkingTree {
        status: Arc<WorkingTreeStatus>,
        /// The branch `HEAD` is on, or the detached `HEAD`.
        head: Option<RefLabel>,
        /// The commit `HEAD` is on, if any.
        last_commit: Option<LastCommit>,
        form: Box<CommitForm>,
    },
}

/// The commit `HEAD` is on, which can be amended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastCommit {
    /// The upstream that already has the commit, if any: amending it
    /// rewrites history others may have.
    pub pushed_to: Option<String>,
}

/// The message of the next commit. It belongs to the window, so it survives
/// leaving the review.
pub struct CommitDraft {
    message: Entity<TextareaState>,
    /// Set while amending: the message typed before, which comes back when
    /// amending is turned off. The message box holds the last commit's
    /// message meanwhile.
    saved: Option<String>,
}

impl CommitDraft {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            message: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 10)
                    .placeholder("Commit message")
            }),
            saved: None,
        }
    }

    fn is_amending(&self) -> bool {
        self.saved.is_some()
    }
}

/// Staging and committing the uncommitted changes.
struct CommitForm {
    git: Git,
    repository: Repository,
    draft: Entity<CommitDraft>,
    /// The message box of `draft`.
    message: Entity<TextareaState>,
    /// Reading the last commit's message to amend it.
    reading_message: Option<Task<()>>,
    /// The `git` command running. Only one runs at a time.
    running: Option<Task<()>>,
    /// Why the last command failed: often the output of a hook.
    error: Option<SharedString>,
    _message_events: Subscription,
}

struct CommitSubject {
    commit: Commit,
    labels: Vec<RefLabel>,
    /// The commits before and after this one in the history, if any.
    newer: Option<Commit>,
    older: Option<Commit>,
    details: Option<Arc<CommitDetails>>,
}

pub struct Review {
    focus_handle: FocusHandle,
    subject: Subject,
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
    pub fn commit(
        repository: &Repository,
        commit: Commit,
        labels: Vec<RefLabel>,
        (newer, older): (Option<Commit>, Option<Commit>),
        cx: &mut Context<Self>,
    ) -> Self {
        let repository = repository.clone();
        let id = commit.id;
        let load = cx.spawn(async move |this, cx| {
            let (details, files) = cx
                .background_executor()
                .spawn(async move { (repository.commit_details(id), repository.commit_diff(id)) })
                .await;
            this.update(cx, |this, cx| {
                if let Subject::Commit(subject) = &mut this.subject {
                    subject.details = details.ok().map(Arc::new);
                }
                this.finish(files, cx);
            })
            .ok();
        });
        let subject = Subject::Commit(Box::new(CommitSubject {
            commit,
            labels,
            newer,
            older,
            details: None,
        }));
        Self::with_subject(subject, load, cx)
    }

    /// Reviews the uncommitted changes of `repository`, which has `status`,
    /// reading their diff in the background. `message` holds the commit
    /// message.
    pub fn working_tree(
        repository: &Repository,
        status: Arc<WorkingTreeStatus>,
        head: Option<RefLabel>,
        last_commit: Option<LastCommit>,
        draft: Entity<CommitDraft>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let load = Self::load_working_tree(repository, cx);
        let message = draft.read(cx).message.clone();
        let message_events =
            cx.subscribe_in(&message, window, |this, _, event, window, cx| match event {
                InputEvent::PressEnter {
                    secondary: true, ..
                } => this.commit_staged(&CommitStaged, window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            });
        let form = CommitForm {
            // A bare repository has no uncommitted changes to review.
            git: Git::new(repository.work_dir().unwrap_or(repository.git_dir())),
            repository: repository.clone(),
            draft,
            message,
            reading_message: None,
            running: None,
            error: None,
            _message_events: message_events,
        };
        let subject = Subject::WorkingTree {
            status,
            head,
            last_commit,
            form: Box::new(form),
        };
        Self::with_subject(subject, load, cx)
    }

    fn with_subject(subject: Subject, load: Task<()>, cx: &mut Context<Self>) -> Self {
        let list = ListState::new(0, ListAlignment::Top, OVERDRAW);
        let this = cx.entity().downgrade();
        list.set_scroll_handler(move |event, _, cx| {
            let first = event.visible_range.start;
            this.update(cx, |this, cx| this.scrolled_to(first, cx)).ok();
        });
        Self {
            focus_handle: cx.focus_handle(),
            subject,
            files: None,
            rows: Vec::new(),
            file_rows: Vec::new(),
            list,
            current_file: 0,
            _load: load,
        }
    }

    fn load_working_tree(repository: &Repository, cx: &mut Context<Self>) -> Task<()> {
        let repository = repository.clone();
        cx.spawn(async move |this, cx| {
            let files = cx
                .background_executor()
                .spawn(async move { repository.working_tree_diff() })
                .await;
            this.update(cx, |this, cx| this.finish(files, cx)).ok();
        })
    }

    /// Updates where `HEAD` is after the refs changed.
    pub fn set_head(
        &mut self,
        head: Option<RefLabel>,
        last_commit: Option<LastCommit>,
        cx: &mut Context<Self>,
    ) {
        if let Subject::WorkingTree {
            head: shown,
            last_commit: shown_commit,
            ..
        } = &mut self.subject
        {
            *shown = head;
            *shown_commit = last_commit;
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
                (self.rows, self.file_rows) = rows(&files);
                self.list.reset(self.rows.len());
                if let Some((path, row, offset_in_item)) = anchor
                    && let Some(file) = files.iter().position(|file| file.path == path)
                {
                    // The same row of the same file, or the file's last row
                    // if it got shorter.
                    let end = self
                        .file_rows
                        .get(file + 1)
                        .copied()
                        .unwrap_or(self.rows.len());
                    let item_ix = (self.file_rows[file] + row).min(end.saturating_sub(1));
                    self.list.scroll_to(ListOffset {
                        item_ix,
                        offset_in_item,
                    });
                    self.current_file = file;
                } else {
                    self.current_file = 0;
                }
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

    /// The file at the top of the list, the row within it and the offset
    /// into that row.
    fn scroll_anchor(&self) -> Option<(String, usize, Pixels)> {
        let Some(Ok(files)) = &self.files else {
            return None;
        };
        let top = self.list.logical_scroll_top();
        let file = self.rows.get(top.item_ix)?.file()?;
        Some((
            files[file].path.clone(),
            top.item_ix - self.file_rows[file],
            top.offset_in_item,
        ))
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
        if let Subject::Commit(subject) = &self.subject
            && subject.newer.is_some()
        {
            cx.emit(ReviewEvent::Newer);
        }
    }

    fn older(&mut self, _: &Older, _: &mut Window, cx: &mut Context<Self>) {
        if let Subject::Commit(subject) = &self.subject
            && subject.older.is_some()
        {
            cx.emit(ReviewEvent::Older);
        }
    }

    /// Runs `command` with `git` in the background unless another command is
    /// running, then `done` if it succeeded.
    fn run_git(
        &mut self,
        command: impl FnOnce(&Git) -> gitez_command::Result<()> + Send + 'static,
        done: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Subject::WorkingTree { form, .. } = &mut self.subject else {
            return;
        };
        if form.running.is_some() {
            return;
        }
        let git = form.git.clone();
        form.error = None;
        form.running = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { command(&git) })
                .await;
            let error: Option<SharedString> = result
                .err()
                .map(|error| format!("{:#}", anyhow::Error::from(error)).into());
            this.update_in(cx, |this, window, cx| {
                let succeeded = error.is_none();
                if let Subject::WorkingTree { form, .. } = &mut this.subject {
                    form.running = None;
                    form.error = error;
                }
                if succeeded {
                    done(this, window, cx);
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
            |_, _, cx| cx.emit(ReviewEvent::Staged),
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
            |_, _, cx| cx.emit(ReviewEvent::Staged),
            window,
            cx,
        );
    }

    /// Commits the staged changes with the message typed, or amends the
    /// last commit with them.
    fn commit_staged(&mut self, _: &CommitStaged, window: &mut Window, cx: &mut Context<Self>) {
        let Subject::WorkingTree { status, form, .. } = &self.subject else {
            return;
        };
        let message = form.message.read(cx).value().trim().to_owned();
        let amend = form.draft.read(cx).is_amending();
        if message.is_empty() || form.reading_message.is_some() || (!amend && status.staged() == 0)
        {
            return;
        }
        let everything = status.files.iter().all(|file| file.unstaged.is_none());
        self.run_git(
            move |git| {
                if amend {
                    git.amend(&message)
                } else {
                    git.commit(&message)
                }
            },
            move |this, window, cx| {
                if let Subject::WorkingTree { form, .. } = &this.subject {
                    form.draft.update(cx, |draft, _| draft.saved = None);
                    form.message
                        .update(cx, |message, cx| message.set_value("", window, cx));
                }
                cx.emit(ReviewEvent::Committed { everything });
            },
            window,
            cx,
        );
    }

    /// Starts or stops amending the last commit. Starting puts its message
    /// in the message box; stopping brings back what was typed before.
    fn set_amending(&mut self, amend: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Subject::WorkingTree { form, .. } = &mut self.subject else {
            return;
        };
        if form.running.is_some() || form.draft.read(cx).is_amending() == amend {
            return;
        }
        let message = form.message.clone();
        if !amend {
            form.reading_message = None;
            let saved = form.draft.update(cx, |draft, _| draft.saved.take());
            message.update(cx, |message, cx| {
                message.set_value(saved.unwrap_or_default(), window, cx)
            });
            cx.notify();
            return;
        }

        let typed = message.read(cx).value().to_string();
        form.draft.update(cx, |draft, _| draft.saved = Some(typed));
        form.error = None;
        let repository = form.repository.clone();
        form.reading_message = Some(cx.spawn_in(window, async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move { repository.head_message() })
                .await;
            this.update_in(cx, |this, window, cx| {
                let Subject::WorkingTree { form, .. } = &mut this.subject else {
                    return;
                };
                form.reading_message = None;
                match read {
                    Ok(last) => message.update(cx, |message, cx| {
                        message.set_value(last.unwrap_or_default(), window, cx)
                    }),
                    Err(error) => {
                        form.error = Some(
                            format!(
                                "Could not read the last commit: {:#}",
                                anyhow::Error::from(error)
                            )
                            .into(),
                        )
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
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
        div()
            .h_flex()
            .flex_none()
            .ml_auto()
            .gap(px(4.))
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

    /// "Unstage all" and "Stage all" above the files, when reviewing the
    /// uncommitted changes.
    fn stage_all_buttons(&self, cx: &mut Context<Self>) -> Option<[AnyElement; 2]> {
        let Subject::WorkingTree { status, form, .. } = &self.subject else {
            return None;
        };
        let running = form.running.is_some();
        let anything_staged = status.staged() > 0;
        let anything_unstaged = status.files.iter().any(|file| file.unstaged.is_some());
        Some([
            Button::new("unstage-all")
                .ghost()
                .xsmall()
                .label("Unstage all")
                .disabled(running || !anything_staged)
                .on_click(cx.listener(|this, _, window, cx| this.unstage(None, window, cx)))
                .into_any_element(),
            Button::new("stage-all")
                .ghost()
                .xsmall()
                .label("Stage all")
                .disabled(running || !anything_unstaged)
                .on_click(cx.listener(|this, _, window, cx| this.stage(None, window, cx)))
                .into_any_element(),
        ])
    }

    /// Whether a `git` command is running.
    fn is_running(&self) -> bool {
        matches!(&self.subject, Subject::WorkingTree { form, .. } if form.running.is_some())
    }

    /// The commit message, the button and why the last command failed.
    fn render_commit_form(
        &self,
        status: &WorkingTreeStatus,
        last_commit: Option<&LastCommit>,
        form: &CommitForm,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let staged = status.staged();
        let running = form.running.is_some() || form.reading_message.is_some();
        let empty = form.message.read(cx).value().trim().is_empty();
        let amend = form.draft.read(cx).is_amending();
        let files = match staged {
            1 => "1 staged file".to_owned(),
            staged => format!("{staged} staged files"),
        };
        let label = match (amend, staged) {
            (true, 0) => "Amend last commit".to_owned(),
            (true, _) => format!("Amend with {files}"),
            (false, 0) => "Commit".to_owned(),
            (false, _) => format!("Commit {files}"),
        };
        let hint = if !amend && staged == 0 {
            "Stage the changes to commit."
        } else if empty && form.reading_message.is_none() {
            "Write a message to commit."
        } else {
            ""
        };
        let pushed_to = last_commit
            .filter(|_| amend)
            .and_then(|last| last.pushed_to.clone());
        let commit = Keystroke::parse("secondary-enter").expect("a valid keystroke");

        div()
            .v_flex()
            .gap(px(10.))
            .max_w(px(760.))
            .child(
                Textarea::new(&form.message)
                    .bg(rgb(color::BACKGROUND))
                    .disabled(running),
            )
            .child(
                div()
                    .h_flex()
                    .gap(px(10.))
                    .text_size(px(12.))
                    .child(
                        Button::new("commit-staged")
                            .primary()
                            .h(px(30.))
                            .label(label)
                            .loading(running)
                            .disabled((!amend && staged == 0) || empty || running)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.commit_staged(&CommitStaged, window, cx)
                            })),
                    )
                    .child(Kbd::new(commit))
                    .child(muted(hint))
                    .when(last_commit.is_some(), |this| {
                        this.child(
                            div().ml_auto().child(
                                Checkbox::new("amend")
                                    .label("Amend last commit")
                                    .checked(amend)
                                    .disabled(running)
                                    .on_click(cx.listener(|this, checked: &bool, window, cx| {
                                        this.set_amending(*checked, window, cx)
                                    })),
                            ),
                        )
                    }),
            )
            .when_some(pushed_to, |this, upstream| {
                this.child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(color::ACCENT))
                        .child(format!(
                            "The last commit is already on {upstream}. Amending it rewrites \
                             history others may have; pushing it then needs --force."
                        )),
                )
            })
            .when_some(form.error.clone(), |this, error| {
                this.child(
                    div()
                        .id("commit-error")
                        .max_h(px(160.))
                        .overflow_y_scroll()
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(theme::RADIUS)
                        .bg(theme::with_alpha(color::DIFF_REMOVED, 0.10))
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(px(12.))
                        .text_color(rgb(color::DIFF_REMOVED))
                        .child(error),
                )
            })
    }

    /// How the uncommitted changes to `path` stand, when reviewing them.
    fn file_status(&self, path: &str) -> Option<&FileStatus> {
        let Subject::WorkingTree { status, .. } = &self.subject else {
            return None;
        };
        status.files.iter().find(|file| file.path == path)
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(Ok(files)) = &self.files else {
            return div().into_any_element();
        };
        let mono = cx.theme().mono_font_family.clone();
        match self.rows.get(index).copied() {
            Some(Row::File(index)) => {
                let file = &files[index];
                let status = self.file_status(&file.path);
                file_header(file, status, mono)
                    .when_some(status, |this, status| {
                        this.child(self.staging_buttons(index, status, cx))
                    })
                    .into_any_element()
            }
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

    /// "End of commit · Next: <older commit>", or "End of changes".
    fn render_end(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (end, older) = match &self.subject {
            Subject::Commit(subject) => ("End of commit", subject.older.as_ref()),
            Subject::WorkingTree { .. } => ("End of changes", None),
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
                    details,
                } = &**subject;
                let parent = match commit.parents.as_slice() {
                    [] => "root commit".to_owned(),
                    [parent, ..] => format!("parent {}", parent.to_hex_with_len(7)),
                };
                let mut when = format!("committed {}", time::absolute(commit.author.time));
                if details.as_ref().is_some_and(|details| details.signed) {
                    when.push_str(" · signed");
                }
                let body = details
                    .as_ref()
                    .map(|details| details.body.clone())
                    .filter(|body| !body.is_empty());
                header
                    .child(
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
                                            .on_click(cx.listener(|_, _, _, cx| {
                                                cx.emit(ReviewEvent::Newer)
                                            })),
                                    )
                                    .child(
                                        Button::new("review-older")
                                            .ghost()
                                            .h(px(30.))
                                            .label("Older")
                                            .tooltip("Review the next older commit (])")
                                            .disabled(older.is_none())
                                            .on_click(cx.listener(|_, _, _, cx| {
                                                cx.emit(ReviewEvent::Older)
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        title(commit.summary.clone())
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
                                byline()
                                    .child(labels::avatar(&commit.author.name, px(22.)))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(commit.author.name.clone()),
                                    )
                                    .child(muted(when))
                                    .when_some(totals, |this, totals| {
                                        this.child(muted("·")).children(total_parts(totals, mono))
                                    }),
                            ),
                    )
            }
            Subject::WorkingTree {
                status,
                head,
                last_commit,
                form,
            } => {
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
                    .child(self.render_commit_form(status, last_commit.as_ref(), form, cx))
            }
        }
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
                    )
                    .when_some(self.stage_all_buttons(cx), |this, buttons| {
                        this.children(buttons)
                    }),
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
            }
            .into_any_element(),
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
            .on_action(cx.listener(Self::commit_staged))
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

/// The top of a file's card: path, what happened to it, how much of it is
/// staged (for uncommitted changes) and line counts.
fn file_header(file: &FileDiff, status: Option<&FileStatus>, mono: SharedString) -> Div {
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
