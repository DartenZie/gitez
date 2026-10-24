//! The main window: toolbar, repository sidebar, history, commit detail and
//! status bar, laid out as in the `Main.dc.html` artboard.

mod detail;
mod graph;
mod history;
mod labels;
mod review;
mod sidebar;
mod status_bar;
mod toolbar;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;

use gpui_kit::component::button::*;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{StyledExt as _, WindowExt as _, h_resizable, resizable_panel};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use gitez_git::{Affected, Head, RefKind, RefLabel, Watcher, WorkingTreeStatus};

use crate::repository::Snapshot;
use crate::theme::{self, color};
use detail::DetailPane;
use history::{History, Selection};
use review::{CommitDraft, LastCommit, Review, ReviewEvent};
use sidebar::Sidebar;

actions!(workspace, [OpenRepository, ReviewSelected]);

/// Key bindings of the views inside the workspace.
pub fn key_bindings() -> Vec<KeyBinding> {
    let mut bindings = history::key_bindings();
    bindings.extend(review::key_bindings());
    bindings
}

/// How long to keep collecting file system events after the first one, so a
/// burst (a checkout, a build) is handled once.
const WATCH_BATCH: Duration = Duration::from_millis(150);

/// The root view of a GitEZ window: one repository at a time.
pub struct Workspace {
    focus_handle: FocusHandle,
    /// The open repository, if any.
    snapshot: Option<Arc<Snapshot>>,
    /// The repository being opened. Dropping the task cancels the load.
    loading: Option<(PathBuf, Task<()>)>,
    /// Why the last attempt to open a repository failed, shown while no
    /// repository is open.
    error: Option<SharedString>,
    /// Uncommitted changes of the open repository, once read.
    status: Option<Arc<WorkingTreeStatus>>,
    /// The status read in flight. Dropping it ignores its result.
    status_load: Option<Task<()>>,
    /// The last status read error shown, so it is not repeated on every
    /// refresh.
    status_error: Option<String>,
    /// Reports changes made outside GitEZ while the repository is open.
    _watcher: Option<Watcher>,
    /// Handles the watcher's reports.
    _watch: Option<Task<()>>,
    /// Re-reading the refs and history after they changed.
    _reload: Option<Task<()>>,
    /// The commit being reviewed, shown instead of the history.
    review: Option<(Entity<Review>, Subscription)>,
    /// The message of the next commit, kept while the review is closed.
    commit_draft: Option<Entity<CommitDraft>>,
    sidebar: Entity<Sidebar>,
    history: Entity<History>,
    detail: Entity<DetailPane>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    /// Creates the workspace and starts opening `path`, if given.
    pub fn new(path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let history = cx.new(History::new);
        let subscriptions = vec![
            // The detail pane shows the selected commit.
            cx.subscribe(&history, |this, _, _, cx| this.show_selection(cx)),
            // Files change outside GitEZ; read the status again on return.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.refresh_status(window, cx);
                }
            }),
        ];
        let mut workspace = Self {
            focus_handle,
            snapshot: None,
            loading: None,
            error: None,
            status: None,
            status_load: None,
            status_error: None,
            _watcher: None,
            _watch: None,
            _reload: None,
            review: None,
            commit_draft: None,
            sidebar: cx.new(|_| Sidebar::new()),
            history,
            detail: cx.new(|_| DetailPane::new()),
            _subscriptions: subscriptions,
        };
        if let Some(path) = path {
            workspace.open(path, window, cx);
        }
        workspace
    }

    /// Asks for a directory and opens the repository containing it.
    fn prompt_for_repository(
        &mut self,
        _: &OpenRepository,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let path = match paths.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                // Cancelled, or the dialog went away with the window.
                Ok(Ok(None)) | Err(_) => None,
                Ok(Err(error)) => {
                    this.update_in(cx, |_, window, cx| {
                        window.push_notification(
                            Notification::error(format!(
                                "Could not show the folder picker: {error:#}"
                            )),
                            cx,
                        );
                    })
                    .ok();
                    None
                }
            };
            if let Some(path) = path {
                this.update_in(cx, |this, window, cx| this.open(path, window, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Opens the repository containing `path` on a background thread. The
    /// current repository stays on screen until the new one has loaded.
    pub fn open(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let load = cx.background_executor().spawn({
            let path = path.clone();
            async move { Snapshot::load(&path) }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = load.await;
            this.update_in(cx, |this, window, cx| {
                this.loading = None;
                match result {
                    Ok(snapshot) => this.show(Arc::new(snapshot), window, cx),
                    Err(error) => this.show_error(error, window, cx),
                }
            })
            .ok();
        });
        self.loading = Some((path, task));
        cx.notify();
    }

    fn show(&mut self, snapshot: Arc<Snapshot>, window: &mut Window, cx: &mut Context<Self>) {
        window.set_window_title(&format!("{} — GitEZ", snapshot.name));
        self.review = None;
        self.commit_draft = None;
        self.watch(&snapshot, window, cx);
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.set_snapshot(snapshot.clone(), cx));
        self.history
            .update(cx, |history, cx| history.load(&snapshot, cx));
        window.focus(&self.history.focus_handle(cx), cx);
        self.snapshot = Some(snapshot);
        self.error = None;
        self.set_status(None, cx);
        self.status_error = None;
        self.refresh_status(window, cx);
        cx.notify();
    }

    /// Starts watching the repository of `snapshot` for changes made outside
    /// GitEZ, replacing any earlier watch.
    fn watch(&mut self, snapshot: &Snapshot, window: &mut Window, cx: &mut Context<Self>) {
        self._watcher = None;
        let (sender, mut changes) = futures::channel::mpsc::unbounded::<Vec<PathBuf>>();
        let repository = snapshot.repository.clone();
        // Watching a large tree recursively can take a while on Linux.
        let start = cx.background_executor().spawn(async move {
            repository.watch(move |paths| {
                sender.unbounded_send(paths).ok();
            })
        });
        let repository = snapshot.repository.clone();
        self._watch = Some(cx.spawn_in(window, async move |this, cx| {
            match start.await {
                Ok(watcher) => {
                    if this.update(cx, |this, _| this._watcher = Some(watcher)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    this.update_in(cx, |_, window, cx| {
                        window.push_notification(
                            Notification::warning(format!(
                                "Changes made outside GitEZ appear only when its window is focused again: {:#}",
                                anyhow::Error::from(error)
                            )),
                            cx,
                        );
                    })
                    .ok();
                    return;
                }
            }
            while let Some(mut paths) = changes.next().await {
                cx.background_executor().timer(WATCH_BATCH).await;
                while let Ok(more) = changes.try_recv() {
                    paths.extend(more);
                }
                let repository = repository.clone();
                let affected = cx
                    .background_executor()
                    .spawn(async move { repository.classify(&paths) })
                    .await
                    // Unsure what changed: read everything again.
                    .unwrap_or(Affected {
                        refs: true,
                        working_tree: true,
                    });
                if affected.is_empty() {
                    continue;
                }
                let updated = this.update_in(cx, |this, window, cx| {
                    if affected.refs {
                        this.reload(window, cx);
                    } else {
                        this.refresh_status(window, cx);
                    }
                });
                if updated.is_err() {
                    return;
                }
            }
        }));
    }

    /// Reads the refs and history of the open repository again, keeping the
    /// selection and scroll position, then the status.
    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.snapshot.as_ref().map(|snapshot| snapshot.path.clone()) else {
            return;
        };
        let load = cx.background_executor().spawn({
            let path = path.clone();
            async move { Snapshot::load(&path) }
        });
        self._reload = Some(cx.spawn_in(window, async move |this, cx| {
            let result = load.await;
            this.update_in(cx, |this, window, cx| {
                this._reload = None;
                if this
                    .snapshot
                    .as_ref()
                    .is_none_or(|snapshot| snapshot.path != path)
                {
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        let snapshot = Arc::new(snapshot);
                        this.sidebar
                            .update(cx, |sidebar, cx| sidebar.set_snapshot(snapshot.clone(), cx));
                        this.history
                            .update(cx, |history, cx| history.reload(&snapshot, cx));
                        if let Some((review, _)) = &this.review {
                            let (head, last_commit) = head_of(&snapshot);
                            review.update(cx, |review, cx| review.set_head(head, last_commit, cx));
                        }
                        this.snapshot = Some(snapshot);
                        this.refresh_status(window, cx);
                        cx.notify();
                    }
                    Err(error) => this.show_error(error, window, cx),
                }
            })
            .ok();
        }));
    }

    /// Reads the working tree's status on a background thread.
    fn refresh_status(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let repository = snapshot.repository.clone();
        let path = snapshot.path.clone();
        let read = cx
            .background_executor()
            .spawn(async move { repository.status() });
        self.status_load = Some(cx.spawn_in(window, async move |this, cx| {
            let result = read.await;
            this.update_in(cx, |this, window, cx| {
                // The repository changed while reading.
                if this
                    .snapshot
                    .as_ref()
                    .is_none_or(|snapshot| snapshot.path != path)
                {
                    return;
                }
                this.status_load = None;
                match result {
                    Ok(status) => {
                        this.status_error = None;
                        this.set_status(Some(Arc::new(status)), cx);
                    }
                    Err(error) => {
                        let message = format!(
                            "Could not read the working tree: {:#}",
                            anyhow::Error::from(error)
                        );
                        if this.status_error.as_ref() != Some(&message) {
                            window.push_notification(Notification::error(message.clone()), cx);
                            this.status_error = Some(message);
                        }
                    }
                }
            })
            .ok();
        }));
    }

    fn set_status(&mut self, status: Option<Arc<WorkingTreeStatus>>, cx: &mut Context<Self>) {
        if status == self.status {
            return;
        }
        self.status = status.clone();
        let changed = status.as_ref().map(|status| status.files.len());
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.set_changed_files(changed, cx));
        self.history.update(cx, |history, cx| {
            history.set_working_tree(status.clone(), cx)
        });
        if let (Some(snapshot), Some((review, _)), Some(status)) =
            (&self.snapshot, &self.review, status)
        {
            review.update(cx, |review, cx| {
                review.refresh_working_tree(&snapshot.repository, status, cx)
            });
        }
        cx.notify();
    }

    /// Reviews the commit or the uncommitted changes selected in the
    /// history.
    fn review_selected(&mut self, _: &ReviewSelected, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let repository = snapshot.repository.clone();
        let history = self.history.read(cx);
        let review = match history.selected() {
            Some(Selection::Commit(commit, labels)) => {
                let (newer, older) = history.neighbors();
                let (commit, labels) = (commit.clone(), labels.to_vec());
                let neighbors = (newer.cloned(), older.cloned());
                cx.new(|cx| Review::commit(&repository, commit, labels, neighbors, cx))
            }
            Some(Selection::WorkingTree(status)) => {
                let status = status.clone();
                let (head, last_commit) = head_of(snapshot);
                let draft = self
                    .commit_draft
                    .get_or_insert_with(|| cx.new(|cx| CommitDraft::new(window, cx)))
                    .clone();
                cx.new(|cx| {
                    Review::working_tree(&repository, status, head, last_commit, draft, window, cx)
                })
            }
            None => return,
        };
        let subscription = cx.subscribe_in(&review, window, |this, _, event, window, cx| {
            let delta = match event {
                ReviewEvent::Close => {
                    this.close_review(window, cx);
                    return;
                }
                ReviewEvent::Newer => -1,
                ReviewEvent::Older => 1,
                ReviewEvent::Staged => {
                    this.refresh_status(window, cx);
                    return;
                }
                ReviewEvent::Committed { everything } => {
                    // The watcher would notice too, but later.
                    this.reload(window, cx);
                    this.refresh_status(window, cx);
                    if *everything {
                        this.close_review(window, cx);
                    }
                    return;
                }
            };
            let moved = this
                .history
                .update(cx, |history, cx| history.select_adjacent_commit(delta, cx));
            if moved {
                this.review_selected(&ReviewSelected, window, cx);
            }
        });
        window.focus(&review.focus_handle(cx), cx);
        self.review = Some((review, subscription));
        cx.notify();
    }

    fn close_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.review = None;
        window.focus(&self.history.focus_handle(cx), cx);
        cx.notify();
    }

    /// Shows the commit selected in the history in the detail pane.
    fn show_selection(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let selection = match self.history.read(cx).selected() {
            Some(Selection::WorkingTree(status)) => {
                let status = status.clone();
                self.detail
                    .update(cx, |detail, cx| detail.show_working_tree(status, cx));
                return;
            }
            Some(Selection::Commit(commit, labels)) => Some((commit.clone(), labels.to_vec())),
            None => None,
        };
        self.detail.update(cx, |detail, cx| {
            detail.show(&snapshot.repository, selection, cx)
        });
    }

    fn show_error(&mut self, error: anyhow::Error, window: &mut Window, cx: &mut Context<Self>) {
        let message: SharedString = format!("{error:#}").into();
        if self.snapshot.is_some() {
            window.push_notification(Notification::error(message), cx);
        } else {
            self.error = Some(message);
        }
        cx.notify();
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.snapshot.clone();

        div()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::prompt_for_repository))
            .on_action(cx.listener(Self::review_selected))
            .v_flex()
            .size_full()
            .bg(rgb(color::BACKGROUND))
            .text_color(rgb(color::TEXT))
            .text_size(theme::TEXT_SIZE)
            .child(toolbar::Toolbar {
                repository_name: snapshot.as_ref().map(|snapshot| snapshot.name.clone()),
                loading: self.loading.is_some(),
            })
            .child(div().flex_1().min_h_0().map(|this| {
                if let Some((review, _)) = &self.review {
                    return this.child(review.clone());
                }
                this.child(
                    h_resizable("workspace-panes")
                        .child(
                            resizable_panel()
                                .size(px(240.))
                                .size_range(px(180.)..px(400.))
                                .fixed()
                                .child(self.sidebar.clone()),
                        )
                        .child(resizable_panel().child(self.render_history()))
                        .child(
                            resizable_panel()
                                .size(px(360.))
                                .size_range(px(280.)..px(560.))
                                .fixed()
                                .map(|panel| {
                                    if snapshot.is_some() {
                                        panel.child(self.detail.clone())
                                    } else {
                                        panel.child(
                                            div().v_flex().size_full().bg(rgb(color::PANEL)).child(
                                                empty_state(
                                                    "No commit selected",
                                                    "Commit details appear here once a repository is open.",
                                                    None::<Button>,
                                                ),
                                            ),
                                        )
                                    }
                                }),
                        ),
                )
            }))
            .child(status_bar::StatusBar {
                snapshot,
                status: self.status.clone(),
            })
    }
}

impl Workspace {
    /// The history list, or what to show while no repository is open.
    fn render_history(&self) -> AnyElement {
        if self.snapshot.is_some() {
            return self.history.clone().into_any_element();
        }
        if let Some((path, _)) = &self.loading {
            return empty_state(
                "Opening repository…",
                path.display().to_string(),
                None::<Button>,
            )
            .into_any_element();
        }
        if let Some(error) = &self.error {
            return empty_state(
                "Could not open the repository",
                error.clone(),
                Some(open_button("open-repository-retry", "Open another…")),
            )
            .into_any_element();
        }
        empty_state(
            "No repository open",
            "Open a repository to see its history.",
            Some(open_button("open-repository", "Open repository…")),
        )
        .into_any_element()
    }
}

fn open_button(id: &'static str, label: &'static str) -> Button {
    Button::new(id)
        .primary()
        .h(px(30.))
        .label(label)
        .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenRepository), cx))
}

/// A centred title, an explanation and an optional action, filling the rest
/// of a pane.
fn empty_state(
    title: &'static str,
    detail: impl Into<SharedString>,
    action: Option<impl IntoElement>,
) -> impl IntoElement {
    div()
        .v_flex()
        .flex_1()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .p(px(24.))
        .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
        .child(
            div()
                .max_w(px(420.))
                .text_center()
                .text_color(rgb(color::TEXT_MUTED))
                .child(detail.into()),
        )
        .when_some(action, |this, action| {
            this.child(div().mt(px(10.)).child(action))
        })
}

/// The label of where `HEAD` is in `snapshot`, and the commit it is on.
fn head_of(snapshot: &Snapshot) -> (Option<RefLabel>, Option<LastCommit>) {
    match &snapshot.refs.head {
        Some(Head::Branch { name, .. }) => {
            let pushed_to = snapshot
                .tracking
                .get(name)
                .filter(|tracking| tracking.ahead == 0)
                .map(|tracking| tracking.upstream.clone());
            (Some(branch_label(name)), Some(LastCommit { pushed_to }))
        }
        Some(Head::Unborn { name }) => (Some(branch_label(name)), None),
        Some(Head::Detached { .. }) => (
            Some(RefLabel {
                kind: RefKind::Head,
                name: "HEAD".to_owned(),
                is_head: true,
            }),
            Some(LastCommit { pushed_to: None }),
        ),
        None => (None, None),
    }
}

fn branch_label(name: &str) -> RefLabel {
    RefLabel {
        kind: RefKind::Branch,
        name: name.to_owned(),
        is_head: true,
    }
}
