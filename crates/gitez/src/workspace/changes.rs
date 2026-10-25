//! The uncommitted changes in the right-hand pane, while their row of the
//! history is selected: the unstaged and the staged files, then a form that
//! commits the staged ones or stashes everything.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use gitez_command::{CommitOptions, Git, ResetMode};
use gitez_git::{
    ChangeKind, FileStatus, RefKind, RefLabel, Repository, WorkingTreeStatus, WorktreeChange,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::labels;
use super::stash::StashRequest;
use crate::theme::{self, color};
use crate::undo::{self, Discard, Recorded, Recorder};

/// Files listed in a section before the rest are summarised in one line.
const MAX_FILES_SHOWN: usize = 500;
/// The length git and most tools wrap a summary line at.
const SUMMARY_LENGTH: i64 = 72;

/// What the panel asks of the window.
pub enum ChangesEvent {
    /// Show the diff of the uncommitted changes, scrolled to this file.
    OpenFile(String),
    /// The index changed: read the status again.
    Staged,
    /// Uncommitted changes were thrown away; `step` is the undo step.
    Discarded {
        message: SharedString,
        step: Option<u64>,
    },
    /// A commit was made.
    Committed,
    /// Stash the uncommitted changes.
    Stash(StashRequest),
}

/// The commit `HEAD` is on, which can be amended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastCommit {
    /// The upstream that already has the commit, if any: amending it
    /// rewrites history others may have.
    pub pushed_to: Option<String>,
}

/// How the files are listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// One row per file, with its full path.
    Path,
    /// Files under their folders.
    Tree,
}

/// The two lists of files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Section {
    Unstaged,
    Staged,
}

/// The form under the files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Commit,
    Stash,
}

/// One row of a section in the tree layout.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TreeRow {
    /// A folder: its path from the root, ending in `/`, and its depth.
    Folder { path: String, depth: usize },
    /// The file at this index of the section's files, and its depth.
    File { index: usize, depth: usize },
}

pub struct ChangesPanel {
    repository: Repository,
    git: Git,
    status: Arc<WorkingTreeStatus>,
    /// The branch `HEAD` is on, or the detached `HEAD`.
    head: Option<RefLabel>,
    /// The commit `HEAD` is on, if any.
    last_commit: Option<LastCommit>,
    layout: Layout,
    /// Paths sorted Z to A instead of A to Z.
    descending: bool,
    /// The sections folded away.
    folded: HashSet<Section>,
    /// The folders folded away in the tree layout.
    folded_folders: HashSet<(Section, String)>,
    /// The file whose diff was opened last, or that is at the top of the
    /// diff now.
    selected: Option<(Section, String)>,
    /// Whether to scroll the selected file into view on the next render.
    reveal: bool,
    /// The scroll position of each section's files.
    scroll: HashMap<Section, ScrollHandle>,
    tab: Tab,
    summary: Entity<InputState>,
    /// The same summary, typed in the uncommitted-changes row of the
    /// history; the two are kept in step.
    graph_summary: Entity<InputState>,
    description: Entity<TextareaState>,
    /// Set while amending: the summary and description typed before, which
    /// come back when amending is turned off. The boxes hold the last
    /// commit's message meanwhile.
    saved: Option<(String, String)>,
    show_options: bool,
    no_verify: bool,
    sign_off: bool,
    stash_message: Entity<InputState>,
    stash_untracked: bool,
    /// Reading the last commit's message to amend it.
    reading_message: Option<Task<()>>,
    /// The `git` command running. Only one runs at a time.
    running: Option<Task<()>>,
    /// Why the last command failed: often the output of a hook.
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ChangesEvent> for ChangesPanel {}

impl ChangesPanel {
    pub fn new(
        repository: &Repository,
        head: Option<RefLabel>,
        last_commit: Option<LastCommit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let summary = cx.new(|cx| InputState::new(window, cx).placeholder("Commit summary"));
        let graph_summary = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Uncommitted changes — write a commit summary")
        });
        let description = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(4, 12)
                .placeholder("Description")
        });
        let stash_message = cx.new(|cx| InputState::new(window, cx));
        let subscriptions = vec![
            cx.subscribe_in(&summary, window, |this, _, event, window, cx| match event {
                InputEvent::PressEnter {
                    secondary: true, ..
                } => this.commit(window, cx),
                InputEvent::PressEnter { .. } => {
                    let description = this.description.focus_handle(cx);
                    window.focus(&description, cx);
                }
                InputEvent::Change => {
                    let value = this.summary.read(cx).value();
                    this.graph_summary
                        .update(cx, |input, cx| input.set_value(value, window, cx));
                    cx.notify();
                }
                _ => {}
            }),
            cx.subscribe_in(
                &graph_summary,
                window,
                |this, _, event, window, cx| match event {
                    InputEvent::PressEnter {
                        secondary: true, ..
                    } => this.commit(window, cx),
                    InputEvent::PressEnter { .. } => {
                        this.tab = Tab::Commit;
                        let description = this.description.focus_handle(cx);
                        window.focus(&description, cx);
                        cx.notify();
                    }
                    InputEvent::Change => {
                        let value = this.graph_summary.read(cx).value();
                        this.summary
                            .update(cx, |input, cx| input.set_value(value, window, cx));
                        cx.notify();
                    }
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &description,
                window,
                |this, _, event, window, cx| match event {
                    InputEvent::PressEnter {
                        secondary: true, ..
                    } => this.commit(window, cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
            cx.subscribe_in(&stash_message, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.stash(window, cx);
                }
            }),
        ];
        let mut panel = Self {
            repository: repository.clone(),
            // A bare repository has no uncommitted changes.
            git: Git::new(repository.work_dir().unwrap_or(repository.git_dir())),
            status: Arc::default(),
            head: None,
            last_commit: None,
            layout: Layout::Path,
            descending: false,
            folded: HashSet::new(),
            folded_folders: HashSet::new(),
            selected: None,
            reveal: false,
            scroll: HashMap::from([
                (Section::Unstaged, ScrollHandle::new()),
                (Section::Staged, ScrollHandle::new()),
            ]),
            tab: Tab::Commit,
            summary,
            graph_summary,
            description,
            saved: None,
            show_options: false,
            no_verify: false,
            sign_off: false,
            stash_message,
            stash_untracked: true,
            reading_message: None,
            running: None,
            error: None,
            _subscriptions: subscriptions,
        };
        panel.set_head(head, last_commit, window, cx);
        panel
    }

    /// Shows the uncommitted changes in `status`.
    pub fn set_status(&mut self, status: Arc<WorkingTreeStatus>, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    /// Updates where `HEAD` is after the refs changed.
    pub fn set_head(
        &mut self,
        head: Option<RefLabel>,
        last_commit: Option<LastCommit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let placeholder = match &head {
            Some(RefLabel {
                kind: RefKind::Branch,
                name,
                ..
            }) => format!("WIP on {name}"),
            _ => "WIP".to_owned(),
        };
        self.stash_message.update(cx, |message, cx| {
            message.set_placeholder(placeholder, window, cx)
        });
        if last_commit.is_none() && self.is_amending() {
            self.set_amending(false, window, cx);
        }
        self.head = head;
        self.last_commit = last_commit;
        cx.notify();
    }

    /// The commit summary input shown in the uncommitted-changes row of
    /// the history.
    pub fn graph_summary(&self) -> Entity<InputState> {
        self.graph_summary.clone()
    }

    /// Puts the cursor in the commit summary.
    pub fn focus_summary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = Tab::Commit;
        let summary = self.summary.focus_handle(cx);
        window.focus(&summary, cx);
        cx.notify();
    }

    fn is_amending(&self) -> bool {
        self.saved.is_some()
    }

    fn is_busy(&self) -> bool {
        self.running.is_some() || self.reading_message.is_some()
    }

    /// The commit message typed: the summary, then the description after a
    /// blank line.
    fn message(&self, cx: &App) -> String {
        let summary = self.summary.read(cx).value().trim().to_owned();
        let description = self.description.read(cx).value().trim_end().to_owned();
        let description = description.trim_start_matches('\n');
        if description.is_empty() {
            summary
        } else {
            format!("{summary}\n\n{description}")
        }
    }

    /// Whether the commit button does anything.
    fn can_commit(&self, cx: &App) -> bool {
        !self.is_busy()
            && !self.summary.read(cx).value().trim().is_empty()
            && (self.is_amending() || self.status.staged() > 0)
    }

    /// Runs `command` with `git` in the background unless another command is
    /// running, then `done` if it succeeded.
    fn run_git<T: Send + 'static>(
        &mut self,
        command: impl FnOnce(&Git) -> gitez_command::Result<T> + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.running.is_some() {
            return;
        }
        let git = self.git.clone();
        self.error = None;
        self.running = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { command(&git) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.running = None;
                match result {
                    Ok(value) => done(this, value, window, cx),
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

    /// Stages the files at `paths`, or every file when `None`.
    fn stage(&mut self, paths: Option<Vec<String>>, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git(
            move |git| match &paths {
                Some(paths) => git.stage(paths),
                None => git.stage_all(),
            },
            |_, (), _, cx| cx.emit(ChangesEvent::Staged),
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
            |_, (), _, cx| cx.emit(ChangesEvent::Staged),
            window,
            cx,
        );
    }

    /// Throws away the uncommitted changes to the files at `paths`, or to
    /// every file when `None`, as one undo step.
    fn discard(&mut self, paths: Option<Vec<String>>, window: &mut Window, cx: &mut Context<Self>) {
        let (label, message) = match paths.as_deref() {
            Some([path, ..]) => (
                format!("Discard the changes to {path}"),
                format!("Discarded the changes to {path}"),
            ),
            _ => (
                "Discard all changes".to_owned(),
                "Discarded every uncommitted change".to_owned(),
            ),
        };
        self.run_git(
            move |git| {
                let saved = git.discard(paths.as_deref())?;
                Ok(saved.and_then(|saved| {
                    Recorded::discard(git.work_dir(), Discard::Files { paths, saved })
                }))
            },
            move |_, recorded, _, cx| {
                let step = undo::record(cx, recorded, label);
                cx.emit(ChangesEvent::Discarded {
                    message: message.into(),
                    step,
                });
            },
            window,
            cx,
        );
    }

    /// Commits the staged changes with the message typed, or amends the
    /// last commit with them.
    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_commit(cx) {
            return;
        }
        let message = self.message(cx);
        let summary = message.lines().next().unwrap_or_default().to_owned();
        let options = CommitOptions {
            amend: self.is_amending(),
            no_verify: self.no_verify,
            sign_off: self.sign_off,
        };
        self.run_git(
            move |git| {
                let recorder = Recorder::start(git.work_dir());
                git.commit_with(&message, options)?;
                // Undoing a commit brings its changes back staged.
                Ok(recorder.finish(ResetMode::Soft))
            },
            move |this, recorded, window, cx| {
                let verb = if options.amend { "Amend" } else { "Commit" };
                undo::record(cx, recorded, format!("{verb} “{summary}”"));
                this.saved = None;
                this.set_message("", "", window, cx);
                cx.emit(ChangesEvent::Committed);
            },
            window,
            cx,
        );
    }

    fn set_message(
        &mut self,
        summary: &str,
        description: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (summary, description) = (summary.to_owned(), description.to_owned());
        self.graph_summary
            .update(cx, |input, cx| input.set_value(summary.clone(), window, cx));
        self.summary
            .update(cx, |input, cx| input.set_value(summary, window, cx));
        self.description
            .update(cx, |input, cx| input.set_value(description, window, cx));
    }

    /// Starts or stops amending the last commit. Starting puts its message
    /// in the boxes; stopping brings back what was typed before.
    fn set_amending(&mut self, amend: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.running.is_some() || self.is_amending() == amend {
            return;
        }
        if !amend {
            self.reading_message = None;
            if let Some((summary, description)) = self.saved.take() {
                self.set_message(&summary, &description, window, cx);
            }
            cx.notify();
            return;
        }

        let typed = (
            self.summary.read(cx).value().to_string(),
            self.description.read(cx).value().to_string(),
        );
        self.saved = Some(typed);
        self.error = None;
        let repository = self.repository.clone();
        self.reading_message = Some(cx.spawn_in(window, async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move { repository.head_message() })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.reading_message = None;
                match read {
                    Ok(last) => {
                        let last = last.unwrap_or_default();
                        let (summary, description) = split_message(&last);
                        this.set_message(summary, description, window, cx);
                    }
                    Err(error) => {
                        this.error = Some(
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

    /// Asks the window to stash the changes with the message typed.
    fn stash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() || self.status.is_clean() {
            return;
        }
        let message = self.stash_message.read(cx).value().trim().to_owned();
        let untracked = self.stash_untracked && self.new_files() > 0;
        self.stash_message
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.emit(ChangesEvent::Stash(StashRequest {
            message: (!message.is_empty()).then_some(message),
            untracked,
        }));
    }

    /// Files git does not track yet.
    fn new_files(&self) -> usize {
        self.status
            .files
            .iter()
            .filter(|file| file.unstaged == Some(WorktreeChange::Untracked))
            .count()
    }

    /// Highlights the file at `path`, which the diff has scrolled to, and
    /// scrolls it into view: in the section it was opened from, otherwise
    /// in the unstaged files if it has unstaged changes.
    pub fn show_current(&mut self, path: &str, cx: &mut Context<Self>) {
        if self
            .selected
            .as_ref()
            .is_some_and(|(_, selected)| selected == path)
        {
            return;
        }
        let Some(file) = self.status.files.iter().find(|file| file.path == path) else {
            return;
        };
        let section = if file.unstaged.is_some() {
            Section::Unstaged
        } else {
            Section::Staged
        };
        self.selected = Some((section, path.to_owned()));
        self.reveal = true;
        cx.notify();
    }

    fn open_file(&mut self, section: Section, path: String, cx: &mut Context<Self>) {
        self.selected = Some((section, path.clone()));
        cx.emit(ChangesEvent::OpenFile(path));
        cx.notify();
    }

    /// The files of `section`, sorted.
    fn files(&self, section: Section) -> Vec<&FileStatus> {
        let mut files: Vec<&FileStatus> = self
            .status
            .files
            .iter()
            .filter(|file| match section {
                Section::Unstaged => file.unstaged.is_some(),
                Section::Staged => file.staged.is_some(),
            })
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        if self.descending {
            files.reverse();
        }
        files
    }
}

impl Render for ChangesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let reveal = std::mem::take(&mut self.reveal);
        div()
            .v_flex()
            .size_full()
            .bg(rgb(color::PANEL))
            .child(self.render_header(cx))
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_section(Section::Unstaged, reveal, cx))
                    .child(self.render_section(Section::Staged, reveal, cx)),
            )
            .child(self.render_form(cx))
    }
}

impl ChangesPanel {
    /// "Discard all", and "N file changes on <branch>".
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.status.files.len();
        let changes = match count {
            1 => "1 file change on".to_owned(),
            count => format!("{count} file changes on"),
        };
        div()
            .h_flex()
            .flex_none()
            .h(px(44.))
            .px(px(10.))
            .gap(px(8.))
            .border_b_1()
            .border_color(rgb(color::BORDER))
            .child(
                Button::new("discard-all")
                    .ghost()
                    .size(px(28.))
                    .icon(
                        Icon::new(IconName::Trash)
                            .size(px(15.))
                            .text_color(rgb(color::DIFF_REMOVED)),
                    )
                    .accessibility_label("Discard all changes")
                    .tooltip("Discard all changes; Undo brings them back")
                    .disabled(self.is_busy() || count == 0)
                    .on_click(cx.listener(|this, _, window, cx| this.discard(None, window, cx))),
            )
            .child(
                div()
                    .h_flex()
                    .flex_1()
                    .min_w_0()
                    .justify_center()
                    .gap(px(6.))
                    .child(div().flex_none().child(changes))
                    .child(match &self.head {
                        Some(head) if head.kind == RefKind::Branch => {
                            labels::ref_label(head).into_any_element()
                        }
                        Some(_) => div()
                            .text_color(rgb(color::TEXT_MUTED))
                            .child("a detached HEAD")
                            .into_any_element(),
                        None => div().into_any_element(),
                    }),
            )
            // Balances the discard button so the title stays centred.
            .child(div().flex_none().w(px(28.)))
    }

    /// Sort order, and paths or a tree.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (sort_icon, sort_tooltip) = if self.descending {
            (IconName::ArrowUpZA, "Sorted Z to A")
        } else {
            (IconName::ArrowUpAZ, "Sorted A to Z")
        };
        div()
            .h_flex()
            .flex_none()
            .px(px(10.))
            .py(px(8.))
            .child(
                Button::new("sort-files")
                    .ghost()
                    .size(px(28.))
                    .icon(Icon::new(sort_icon).size(px(15.)))
                    .accessibility_label(sort_tooltip)
                    .tooltip(sort_tooltip)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.descending = !this.descending;
                        cx.notify();
                    })),
            )
            .child(
                div().flex_1().h_flex().justify_center().child(
                    ButtonGroup::new("file-layout")
                        .outline()
                        .small()
                        .child(
                            Button::new("layout-path")
                                .icon(Icon::new(IconName::List).size(px(14.)))
                                .label("Path")
                                .selected(self.layout == Layout::Path),
                        )
                        .child(
                            Button::new("layout-tree")
                                .icon(Icon::new(IconName::ListTree).size(px(14.)))
                                .label("Tree")
                                .selected(self.layout == Layout::Tree),
                        )
                        .on_click(cx.listener(|this, clicked: &Vec<usize>, _, cx| {
                            this.layout = if clicked.contains(&1) {
                                Layout::Tree
                            } else {
                                Layout::Path
                            };
                            cx.notify();
                        })),
                ),
            )
            // Balances the sort button so the switch stays centred.
            .child(div().flex_none().w(px(28.)))
    }

    /// "Unstaged Files (N)" or "Staged Files (N)", the button that stages or
    /// unstages them all, and the files unless folded. With `reveal`, the
    /// selected file is scrolled into view.
    fn render_section(
        &self,
        section: Section,
        reveal: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let files = self.files(section);
        let folded = self.folded.contains(&section);
        let (title, all_label) = match section {
            Section::Unstaged => ("Unstaged Files", "Stage All Changes"),
            Section::Staged => ("Staged Files", "Unstage All Changes"),
        };
        let id = match section {
            Section::Unstaged => "unstaged",
            Section::Staged => "staged",
        };
        let busy = self.is_busy();

        let header = div()
            .id(SharedString::from(format!("{id}-header")))
            .h_flex()
            .flex_none()
            .h(px(32.))
            .mx(px(6.))
            .px(px(4.))
            .gap(px(6.))
            .cursor_pointer()
            .when(section == Section::Staged, |this| {
                this.border_t_1().border_color(rgb(color::BORDER))
            })
            .child(
                Icon::new(if folded {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(px(14.))
                .text_color(rgb(color::TEXT_MUTED)),
            )
            .child(
                div()
                    .flex_1()
                    .text_color(rgb(color::TEXT_SECONDARY))
                    .child(format!("{title} ({})", files.len())),
            )
            .when(!files.is_empty(), |this| {
                this.child(
                    Button::new(SharedString::from(format!("{id}-all")))
                        .outline()
                        .success()
                        .xsmall()
                        .label(all_label)
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            // Folding the section is the header's click.
                            cx.stop_propagation();
                            match section {
                                Section::Unstaged => this.stage(None, window, cx),
                                Section::Staged => this.unstage(None, window, cx),
                            }
                        })),
                )
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.folded.remove(&section) {
                    this.folded.insert(section);
                }
                cx.notify();
            }));

        let selected = self
            .selected
            .as_ref()
            .filter(|(selected_section, _)| *selected_section == section)
            .map(|(_, path)| path.as_str());
        let is_selected = |index: usize| selected == Some(files[index].path.as_str());
        let mut selected_row = None;
        let rows: Vec<AnyElement> = match self.layout {
            Layout::Path => files
                .iter()
                .take(MAX_FILES_SHOWN)
                .enumerate()
                .map(|(index, file)| {
                    if is_selected(index) {
                        selected_row = Some(index);
                    }
                    self.render_file(section, index, file, 0, cx)
                })
                .collect(),
            Layout::Tree => {
                let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
                tree_rows(&paths, self.descending)
                    .into_iter()
                    .filter(|row| !self.inside_folded_folder(section, row, &files))
                    .take(MAX_FILES_SHOWN)
                    .enumerate()
                    .map(|(position, row)| match row {
                        TreeRow::Folder { path, depth } => {
                            self.render_folder(section, path, depth, cx)
                        }
                        TreeRow::File { index, depth } => {
                            if is_selected(index) {
                                selected_row = Some(position);
                            }
                            self.render_file(section, index, files[index], depth, cx)
                        }
                    })
                    .collect()
            }
        };
        let scroll = self.scroll[&section].clone();
        if reveal && let Some(row) = selected_row {
            scroll.scroll_to_item(row);
        }
        let hidden = files.len().saturating_sub(MAX_FILES_SHOWN);

        div()
            .v_flex()
            .when(!folded, |this| this.flex_1().min_h_0())
            .child(header)
            .when(!folded, |this| {
                this.child(
                    div()
                        .id(SharedString::from(format!("{id}-files")))
                        .v_flex()
                        .flex_1()
                        .min_h_0()
                        .px(px(6.))
                        .pb(px(6.))
                        .overflow_y_scroll()
                        .track_scroll(&scroll)
                        .children(rows)
                        .when(hidden > 0, |this| {
                            this.child(
                                div()
                                    .px(px(10.))
                                    .py(px(6.))
                                    .text_size(px(12.))
                                    .text_color(rgb(color::TEXT_MUTED))
                                    .child(format!("and {hidden} more files")),
                            )
                        }),
                )
            })
    }

    /// Whether `row` sits in a folder folded away.
    fn inside_folded_folder(&self, section: Section, row: &TreeRow, files: &[&FileStatus]) -> bool {
        let path = match row {
            TreeRow::Folder { path, .. } => path.trim_end_matches('/'),
            TreeRow::File { index, .. } => files[*index].path.as_str(),
        };
        self.folded_folders.iter().any(|(folded_section, folder)| {
            *folded_section == section && path.starts_with(folder.as_str())
        })
    }

    fn render_folder(
        &self,
        section: Section,
        path: String,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = (section, path.clone());
        let folded = self.folded_folders.contains(&key);
        let name = path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned();
        div()
            .id(SharedString::from(format!("{section:?}-folder-{path}")))
            .h_flex()
            .flex_none()
            .h(px(28.))
            .pl(px(10. + 14. * depth as f32))
            .pr(px(10.))
            .gap(px(6.))
            .rounded(theme::RADIUS)
            .cursor_pointer()
            .text_color(rgb(color::TEXT_SECONDARY))
            .hover(|style| style.bg(rgb(color::NAV_HOVER)))
            .child(
                Icon::new(if folded {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(px(12.))
                .text_color(rgb(color::TEXT_MUTED)),
            )
            .child(
                Icon::new(if folded {
                    IconName::Folder
                } else {
                    IconName::FolderOpen
                })
                .size(px(14.))
                .text_color(rgb(color::TEXT_MUTED)),
            )
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(name),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.folded_folders.remove(&key) {
                    this.folded_folders.insert(key.clone());
                }
                cx.notify();
            }))
            .into_any_element()
    }

    /// A file: what happened to it, its path or name, and, on hover,
    /// buttons that discard and stage or unstage it.
    fn render_file(
        &self,
        section: Section,
        index: usize,
        file: &FileStatus,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (icon, icon_color) = match section {
            Section::Unstaged => worktree_icon(file.unstaged.unwrap_or(WorktreeChange::Modified)),
            Section::Staged => staged_icon(file.staged.as_ref().unwrap_or(&ChangeKind::Modified)),
        };
        let selected = self
            .selected
            .as_ref()
            .is_some_and(|(selected_section, path)| {
                *selected_section == section && *path == file.path
            });
        let (directory, name) = match self.layout {
            Layout::Path => match file.path.rfind('/') {
                Some(slash) => file.path.split_at(slash + 1),
                None => ("", file.path.as_str()),
            },
            Layout::Tree => ("", file.path.rsplit('/').next().unwrap_or_default()),
        };
        let from = match &file.staged {
            Some(ChangeKind::Renamed { from }) if section == Section::Staged => Some(from.clone()),
            _ => None,
        };
        // Unstaging a rename unstages both of its paths.
        let mut paths = vec![file.path.clone()];
        paths.extend(from.clone());
        let group: SharedString = format!("{section:?}-file-{index}").into();
        let busy = self.is_busy();
        let path = file.path.clone();
        let (stage_label, stage_id) = match section {
            Section::Unstaged => ("Stage File", "stage-file"),
            Section::Staged => ("Unstage File", "unstage-file"),
        };
        let row_id = match section {
            Section::Unstaged => "unstaged-file",
            Section::Staged => "staged-file",
        };
        let discard_paths = paths.clone();

        div()
            .id((row_id, index))
            .group(group.clone())
            .h_flex()
            .flex_none()
            .h(px(28.))
            .pl(px(10.
                + 14. * depth as f32
                + if depth > 0 { 18. } else { 0. }))
            .pr(px(6.))
            .gap(px(8.))
            .rounded(theme::RADIUS)
            .cursor_pointer()
            .map(|this| {
                if selected {
                    this.bg(rgb(color::SELECTED))
                } else {
                    this.hover(|style| style.bg(rgb(color::NAV_HOVER)))
                }
            })
            .child(Icon::new(icon).size(px(13.)).text_color(rgb(icon_color)))
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
            .child(
                div()
                    .h_flex()
                    .flex_none()
                    .gap(px(4.))
                    .opacity(0.)
                    .group_hover(group, |style| style.opacity(1.))
                    .when(section == Section::Unstaged, |this| {
                        this.child(
                            Button::new(("discard-file", index))
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(IconName::Trash).size(px(13.)))
                                .accessibility_label("Discard changes")
                                .tooltip("Discard the changes to this file; Undo brings them back")
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.discard(Some(discard_paths.clone()), window, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new((stage_id, index))
                            .outline()
                            .map(|button| match section {
                                Section::Unstaged => button.success(),
                                Section::Staged => button.danger(),
                            })
                            .xsmall()
                            .label(stage_label)
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                match section {
                                    Section::Unstaged => {
                                        this.stage(Some(paths.clone()), window, cx)
                                    }
                                    Section::Staged => {
                                        this.unstage(Some(paths.clone()), window, cx)
                                    }
                                }
                            })),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.open_file(section, path.clone(), cx)))
            .into_any_element()
    }

    /// The tabs, and the commit or stash form.
    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = |id: &'static str, icon: IconName, label: &'static str, value: Tab| {
            let active = self.tab == value;
            div()
                .id(id)
                .h_flex()
                .gap(px(6.))
                .px(px(14.))
                .h(px(34.))
                .cursor_pointer()
                .border_1()
                .border_b_0()
                .rounded_t(theme::RADIUS)
                .map(|this| {
                    if active {
                        this.border_color(rgb(color::BORDER))
                            .bg(rgb(color::PANEL))
                            .text_color(rgb(color::TEXT))
                    } else {
                        this.border_color(rgba(0))
                            .text_color(rgb(color::TEXT_MUTED))
                            .hover(|style| style.text_color(rgb(color::TEXT)))
                    }
                })
                .child(Icon::new(icon).size(px(14.)))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.tab = value;
                    cx.notify();
                }))
        };
        div()
            .v_flex()
            .flex_none()
            .px(px(10.))
            .pt(px(10.))
            .pb(px(12.))
            .gap(px(10.))
            .border_t_1()
            .border_color(rgb(color::BORDER))
            .child(
                div()
                    .h_flex()
                    // The active tab's border joins the line under the tabs.
                    .mb(px(-10.))
                    .border_b_1()
                    .border_color(rgb(color::BORDER))
                    .child(tab(
                        "tab-commit",
                        IconName::GitCommitHorizontal,
                        "Commit",
                        Tab::Commit,
                    ))
                    .child(tab("tab-stash", IconName::Archive, "Stash", Tab::Stash)),
            )
            .child(div().h(px(2.)))
            .map(|this| match self.tab {
                Tab::Commit => this.children(self.render_commit_form(cx)),
                Tab::Stash => this.children(self.render_stash_form(cx)),
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .id("changes-error")
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

    fn render_commit_form(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let busy = self.is_busy();
        let amend = self.is_amending();
        let left = SUMMARY_LENGTH - self.summary.read(cx).value().chars().count() as i64;
        let pushed_to = self
            .last_commit
            .as_ref()
            .filter(|_| amend)
            .and_then(|last| last.pushed_to.clone());
        let mut form = vec![
            Checkbox::new("amend")
                .label("Amend previous commit")
                .checked(amend)
                .disabled(self.last_commit.is_none() || self.running.is_some())
                .on_click(cx.listener(|this, checked: &bool, window, cx| {
                    this.set_amending(*checked, window, cx)
                }))
                .into_any_element(),
        ];
        if let Some(upstream) = pushed_to {
            form.push(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(color::ACCENT))
                    .child(format!(
                        "The last commit is already on {upstream}. Amending it rewrites \
                         history others may have; pushing it then needs --force."
                    ))
                    .into_any_element(),
            );
        }
        form.push(
            div()
                .v_flex()
                .gap(px(2.))
                .p(px(6.))
                .rounded(theme::RADIUS)
                .border_1()
                .border_color(rgb(color::BUTTON_BORDER))
                .bg(rgb(color::BACKGROUND))
                .child(
                    div()
                        .h_flex()
                        .gap(px(6.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&self.summary).appearance(false).disabled(busy)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .pr(px(6.))
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_size(px(12.))
                                .text_color(rgb(if left < 0 {
                                    color::DIFF_REMOVED
                                } else {
                                    color::TEXT_MUTED
                                }))
                                .child(left.to_string()),
                        ),
                )
                .child(
                    Textarea::new(&self.description)
                        .appearance(false)
                        .disabled(busy),
                )
                .into_any_element(),
        );
        form.push(
            div()
                .id("commit-options")
                .h_flex()
                .gap(px(6.))
                .cursor_pointer()
                .text_color(rgb(color::TEXT_MUTED))
                .hover(|style| style.text_color(rgb(color::TEXT)))
                .child(
                    Icon::new(if self.show_options {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size(px(14.)),
                )
                .child("Commit options")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.show_options = !this.show_options;
                    cx.notify();
                }))
                .into_any_element(),
        );
        if self.show_options {
            form.push(
                div()
                    .v_flex()
                    .gap(px(8.))
                    .pl(px(20.))
                    .child(
                        Checkbox::new("no-verify")
                            .label("Skip commit hooks (--no-verify)")
                            .checked(self.no_verify)
                            .disabled(busy)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.no_verify = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("sign-off")
                            .label("Add Signed-off-by (--signoff)")
                            .checked(self.sign_off)
                            .disabled(busy)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.sign_off = *checked;
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        form.push(
            Button::new("commit")
                .primary()
                .w_full()
                .h(px(36.))
                .icon(Icon::new(IconName::GitCommitHorizontal).size(px(15.)))
                .label(if amend { "Amend" } else { "Commit" })
                .loading(self.running.is_some())
                .disabled(!self.can_commit(cx))
                .on_click(cx.listener(|this, _, window, cx| this.commit(window, cx)))
                .into_any_element(),
        );
        form
    }

    fn render_stash_form(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let new_files = self.new_files();
        let mut form = vec![
            Input::new(&self.stash_message)
                .disabled(self.is_busy())
                .into_any_element(),
        ];
        if new_files > 0 {
            let label = match new_files {
                1 => "Include 1 new file".to_owned(),
                count => format!("Include {count} new files"),
            };
            form.push(
                Checkbox::new("stash-untracked")
                    .label(label)
                    .checked(self.stash_untracked)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.stash_untracked = *checked;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        let stashed = if self.stash_untracked {
            self.status.files.len()
        } else {
            self.status.files.len() - new_files
        };
        form.push(
            Button::new("stash")
                .primary()
                .w_full()
                .h(px(36.))
                .icon(Icon::new(IconName::Archive).size(px(15.)))
                .label("Stash")
                .disabled(self.is_busy() || stashed == 0)
                .on_click(cx.listener(|this, _, window, cx| this.stash(window, cx)))
                .into_any_element(),
        );
        form
    }
}

/// The first line of `message`, and the rest without the blank line after
/// the first.
fn split_message(message: &str) -> (&str, &str) {
    let message = message.trim_end();
    match message.split_once('\n') {
        Some((summary, rest)) => (summary, rest.trim_start_matches('\n')),
        None => (message, ""),
    }
}

/// The rows of `paths` in the tree layout: each folder before its contents,
/// folders before files, sorted by name A to Z or Z to A.
fn tree_rows(paths: &[&str], descending: bool) -> Vec<TreeRow> {
    #[derive(Default)]
    struct Folder {
        folders: BTreeMap<String, Folder>,
        files: Vec<(String, usize)>,
    }
    fn walk(
        folder: &Folder,
        prefix: &str,
        depth: usize,
        descending: bool,
        rows: &mut Vec<TreeRow>,
    ) {
        let mut folders: Vec<_> = folder.folders.iter().collect();
        let mut files: Vec<_> = folder.files.iter().collect();
        files.sort();
        if descending {
            folders.reverse();
            files.reverse();
        }
        for (name, inner) in folders {
            let path = format!("{prefix}{name}/");
            rows.push(TreeRow::Folder {
                path: path.clone(),
                depth,
            });
            walk(inner, &path, depth + 1, descending, rows);
        }
        for (_, index) in files {
            rows.push(TreeRow::File {
                index: *index,
                depth,
            });
        }
    }

    let mut root = Folder::default();
    for (index, path) in paths.iter().enumerate() {
        let mut parts: Vec<&str> = path.split('/').collect();
        let name = parts.pop().unwrap_or_default();
        let mut folder = &mut root;
        for part in parts {
            folder = folder.folders.entry(part.to_owned()).or_default();
        }
        folder.files.push((name.to_owned(), index));
    }
    let mut rows = Vec::new();
    walk(&root, "", 0, descending, &mut rows);
    rows
}

/// The icon of an unstaged change and its colour.
fn worktree_icon(change: WorktreeChange) -> (IconName, u32) {
    match change {
        WorktreeChange::Modified => (IconName::Pencil, color::ACCENT),
        WorktreeChange::Deleted => (IconName::Minus, color::DIFF_REMOVED),
        WorktreeChange::Untracked => (IconName::Plus, color::DIFF_ADDED),
        WorktreeChange::Conflicted => (IconName::TriangleAlert, color::DIFF_REMOVED),
    }
}

/// The icon of a staged change and its colour.
fn staged_icon(kind: &ChangeKind) -> (IconName, u32) {
    match kind {
        ChangeKind::Added => (IconName::Plus, color::DIFF_ADDED),
        ChangeKind::Modified => (IconName::Pencil, color::ACCENT),
        ChangeKind::Deleted => (IconName::Minus, color::DIFF_REMOVED),
        ChangeKind::Renamed { .. } => (IconName::ArrowRight, color::BRANCH_BLUE),
        ChangeKind::Copied { .. } => (IconName::Copy, color::BRANCH_BLUE),
    }
}

#[cfg(test)]
mod tests {
    use super::{TreeRow, split_message, tree_rows};

    #[test]
    fn a_message_splits_into_summary_and_description() {
        assert_eq!(
            split_message("Fix it\n\nBecause.\n"),
            ("Fix it", "Because.")
        );
        assert_eq!(split_message("Fix it\n"), ("Fix it", ""));
        assert_eq!(split_message("Fix it\nAt once"), ("Fix it", "At once"));
    }

    #[test]
    fn tree_rows_put_folders_before_files() {
        let paths = ["src/main.rs", "README.md", "src/ui/view.rs", "Cargo.toml"];
        let folder = |path: &str, depth| TreeRow::Folder {
            path: path.to_owned(),
            depth,
        };
        let file = |index, depth| TreeRow::File { index, depth };
        assert_eq!(
            tree_rows(&paths, false),
            vec![
                folder("src/", 0),
                folder("src/ui/", 1),
                file(2, 2),
                file(0, 1),
                file(3, 0),
                file(1, 0),
            ]
        );
        assert_eq!(
            tree_rows(&paths, true),
            vec![
                folder("src/", 0),
                folder("src/ui/", 1),
                file(2, 2),
                file(0, 1),
                file(1, 0),
                file(3, 0),
            ]
        );
    }
}
