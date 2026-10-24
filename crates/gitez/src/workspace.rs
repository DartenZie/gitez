//! The main window: toolbar, repository sidebar, history, commit detail and
//! status bar, laid out as in the `Main.dc.html` artboard.

mod branch;
mod cherry_pick;
mod conflicts;
mod detail;
mod graph;
mod history;
mod labels;
mod new_pull_request;
mod palette;
mod remote;
mod review;
mod rewrite;
mod sidebar;
mod status_bar;
mod toolbar;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    Disableable as _, Icon, Sizable as _, StyledExt as _, WindowExt as _, h_resizable,
    resizable_panel,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use gitez_command::{Git, RewriteOutcome};
use gitez_git::{Affected, Head, Oid, RefKind, RefLabel, Watcher, WorkingTreeStatus};
use gitez_review::{NewPullRequest as NewPullRequestFields, State, Timestamp, Tip};

use crate::repository::Snapshot;
use crate::theme::{self, color};
use branch::BranchForm;
use cherry_pick::{CherryPick, CherryPickEvent};
use conflicts::{Conflicts, ConflictsEvent};
use detail::{DetailEvent, DetailPane};
use history::{
    BranchDropped, CherryPickRequested, History, PickedChanged, Selection, SelectionChanged,
};
use new_pull_request::{FormEvent, NewPullRequestForm};
use palette::{Palette, PaletteCommand, PaletteContext, PaletteEvent};
use remote::{RemoteActions, RemoteOp};
use review::{
    CommitDraft, LastCommit, MergeDialog, MergeEvent, MergedPullRequest, Merging, Review,
    ReviewEvent, Tips,
};
use rewrite::{Rewrite, RewriteEvent};
use sidebar::{Sidebar, SidebarEvent};

actions!(
    workspace,
    [
        OpenRepository,
        ReviewSelected,
        NewPullRequest,
        OpenPalette,
        Fetch,
        Pull,
        Push,
        NewBranch
    ]
);

/// Key bindings of the views inside the workspace.
pub fn key_bindings() -> Vec<KeyBinding> {
    let mut bindings = history::key_bindings();
    bindings.extend(review::key_bindings());
    bindings.extend(palette::key_bindings());
    bindings.push(KeyBinding::new("secondary-k", OpenPalette, None));
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
    /// The conflicts of a merge being resolved, shown instead of the
    /// history or a review.
    conflicts: Option<(Entity<Conflicts>, Subscription)>,
    /// The message of the next commit, kept while the review is closed.
    commit_draft: Option<Entity<CommitDraft>>,
    /// The "New local PR" form while its dialog is open.
    new_pull_request: Option<(Entity<NewPullRequestForm>, Subscription)>,
    /// Creating a local pull request.
    _create: Option<Task<()>>,
    /// Rewriting the branch's history, shown instead of the history or a
    /// review.
    rewrite: Option<(Entity<Rewrite>, Subscription)>,
    /// Continuing, aborting or undoing a rewrite.
    rewrite_command: Option<Task<()>>,
    /// The form creating or renaming a branch, while its dialog is open.
    branch_form: Option<(Entity<BranchForm>, Subscription)>,
    /// Creating, renaming or deleting a branch.
    branch_command: Option<Task<()>>,
    /// The fetch, pull or push running, if one is.
    remote_op: Option<(RemoteOp, Task<()>)>,
    /// The command palette of the open repository, kept while hidden so a
    /// command run from it can finish.
    palette: Option<(Entity<Palette>, Subscription)>,
    /// Whether the palette is shown.
    palette_open: bool,
    /// Checking out a branch chosen in the palette.
    _switch: Option<Task<()>>,
    /// The cherry-pick panel, shown in place of the commit detail.
    cherry_pick: Option<(Entity<CherryPick>, Subscription)>,
    /// The dialog offering to merge a branch dropped on another, while
    /// open.
    merge_dialog: Option<Subscription>,
    /// The local pull request to show after the next reload.
    open_after_reload: Option<u32>,
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
        let detail = cx.new(|_| DetailPane::new());
        let sidebar = cx.new(|_| Sidebar::new());
        let subscriptions = vec![
            cx.subscribe_in(&sidebar, window, |this, _, event, window, cx| match event {
                SidebarEvent::OpenPullRequest(number) => {
                    this.open_pull_request(*number, window, cx)
                }
                SidebarEvent::CheckOut(branch) => this.switch_branch(branch.clone(), window, cx),
                SidebarEvent::NewBranchFrom(tip) => this.new_branch_at(*tip, window, cx),
                SidebarEvent::RenameBranch(branch) => {
                    this.rename_branch(branch.clone(), window, cx)
                }
                SidebarEvent::DeleteBranch(branch) => {
                    this.delete_branch(branch.clone(), false, window, cx)
                }
            }),
            // The detail pane shows the selected commit.
            cx.subscribe(&history, |this, _, _: &SelectionChanged, cx| {
                this.show_selection(cx)
            }),
            cx.subscribe_in(&history, window, |this, _, dropped, window, cx| {
                this.open_branch_merge(dropped, window, cx)
            }),
            cx.subscribe_in(
                &history,
                window,
                |this, _, _: &CherryPickRequested, window, cx| this.open_cherry_pick(window, cx),
            ),
            cx.subscribe(&history, |this, history, _: &PickedChanged, cx| {
                let Some((panel, _)) = &this.cherry_pick else {
                    return;
                };
                let commits = history.read(cx).picked();
                if commits.is_empty() {
                    this.cherry_pick = None;
                    cx.notify();
                } else {
                    panel.update(cx, |panel, cx| panel.set_commits(commits, cx));
                }
            }),
            cx.subscribe_in(&detail, window, |this, _, event, window, cx| match event {
                DetailEvent::OpenFile(path) => this.open_review(Some(path.clone()), window, cx),
                DetailEvent::CherryPick(id) => {
                    this.history
                        .update(cx, |history, cx| history.pick_only(*id, cx));
                    this.open_cherry_pick(window, cx);
                }
                DetailEvent::Reword(id) => this.open_rewrite(*id, Some(*id), false, window, cx),
                DetailEvent::NewBranch(id) => this.new_branch_at(*id, window, cx),
            }),
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
            conflicts: None,
            commit_draft: None,
            new_pull_request: None,
            _create: None,
            merge_dialog: None,
            rewrite: None,
            rewrite_command: None,
            remote_op: None,
            branch_form: None,
            branch_command: None,
            palette: None,
            palette_open: false,
            _switch: None,
            cherry_pick: None,
            open_after_reload: None,
            sidebar,
            history,
            detail,
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
        self.rewrite = None;
        self.commit_draft = None;
        self.open_palette_for(&snapshot, window, cx);
        self.watch(&snapshot, window, cx);
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.set_snapshot(snapshot.clone(), cx));
        self.history
            .update(cx, |history, cx| history.load(&snapshot, cx));
        if let Some(error) = &snapshot.pull_requests_error {
            window.push_notification(Notification::warning(error.clone()), cx);
        }
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
                            review.update(cx, |review, cx| {
                                review.set_head(head, last_commit, cx);
                                if let Some(tips) = review
                                    .pull_request_number()
                                    .and_then(|number| tips_of(&snapshot, number))
                                {
                                    review.set_tips(tips, cx);
                                }
                            });
                        }
                        this.snapshot = Some(snapshot);
                        this.refresh_status(window, cx);
                        if let Some(number) = this.open_after_reload.take() {
                            this.open_pull_request(number, window, cx);
                        }
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
        self.open_review(None, window, cx);
    }

    /// Shows the diff of the commit selected in the history, or of the
    /// uncommitted changes, scrolled to the file at `path` if given.
    fn open_review(&mut self, path: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
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
        self.show_review(review, path, window, cx);
    }

    /// Shows `review` instead of the history, scrolled to the file at
    /// `path` if given.
    fn show_review(
        &mut self,
        review: Entity<Review>,
        path: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let subscription = cx.subscribe_in(&review, window, |this, _, event, window, cx| {
            let delta = match event {
                ReviewEvent::Close => {
                    this.close_review(window, cx);
                    return;
                }
                ReviewEvent::Newer => -1,
                ReviewEvent::Older => 1,
                ReviewEvent::PullRequestSaved => {
                    // The sidebar shows its open comments.
                    this.reload(window, cx);
                    return;
                }
                ReviewEvent::Staged => {
                    this.refresh_status(window, cx);
                    return;
                }
                ReviewEvent::MergeStopped => {
                    this.reload(window, cx);
                    this.refresh_status(window, cx);
                    this.open_conflicts(window, cx);
                    return;
                }
                ReviewEvent::CheckedOut => {
                    this.reload(window, cx);
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
        if let Some(path) = path {
            review.update(cx, |review, cx| review.show_file(path, cx));
        }
        window.focus(&review.focus_handle(cx), cx);
        self.review = Some((review, subscription));
        cx.notify();
    }

    /// Shows local pull request `number` instead of the history.
    fn open_pull_request(&mut self, number: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let Some(pull_request) = snapshot
            .pull_requests
            .iter()
            .find(|pull_request| pull_request.number == number)
        else {
            return;
        };
        let Some(tips) = tips_of(snapshot, number) else {
            return;
        };
        let (repository, store) = (snapshot.repository.clone(), snapshot.reviews.clone());
        let pull_request = pull_request.clone();
        let user_name = snapshot.user_name.clone();
        let review = cx.new(|cx| {
            Review::pull_request(
                &repository,
                store,
                pull_request,
                tips,
                user_name,
                window,
                cx,
            )
        });
        self.show_review(review, None, window, cx);
    }

    /// Opens the "New local PR" dialog.
    fn new_pull_request(
        &mut self,
        _: &NewPullRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        if snapshot.refs.branches.len() < 2 {
            window.push_notification(
                Notification::info(
                    "A local pull request reviews one branch against another; this repository has fewer than two branches.",
                ),
                cx,
            );
            return;
        }
        let form = cx.new(|cx| NewPullRequestForm::new(snapshot, window, cx));
        let subscription =
            cx.subscribe_in(&form, window, |this, _, event, window, cx| match event {
                FormEvent::Submit => this.create_pull_request(window, cx),
            });
        self.new_pull_request = Some((form.clone(), subscription));
        let workspace = cx.entity().downgrade();
        let focus_title = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if focus_title.replace(false) {
                let title = form.read(cx).title().focus_handle(cx);
                window.focus(&title, cx);
            }
            let create = workspace.clone();
            dialog
                .title("New local pull request")
                .w(px(460.))
                .child(form.clone())
                .footer(
                    div()
                        .h_flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            Button::new("new-pull-request-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("new-pull-request-create")
                                .primary()
                                .label("Create")
                                .on_click(move |_, window, cx| {
                                    create
                                        .update(cx, |this, cx| this.create_pull_request(window, cx))
                                        .ok();
                                }),
                        ),
                )
                .on_close({
                    let workspace = workspace.clone();
                    move |_, _, cx| {
                        workspace
                            .update(cx, |this, _| this.new_pull_request = None)
                            .ok();
                    }
                })
        });
    }

    /// Creates the local pull request the open "New local PR" form
    /// describes, then shows it.
    fn create_pull_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(snapshot), Some((form, _))) = (&self.snapshot, &self.new_pull_request) else {
            return;
        };
        let Some(new) = form.update(cx, |form, cx| form.submit(snapshot, cx)) else {
            return;
        };
        window.close_dialog(cx);
        self.new_pull_request = None;
        self.create(new, window, cx);
    }

    /// Creates the local pull request `new`, then shows it.
    fn create(&mut self, new: NewPullRequestFields, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let store = snapshot.reviews.clone();
        let path = snapshot.path.clone();
        let create = cx
            .background_executor()
            .spawn(async move { store.create(new, Timestamp::now()) });
        self._create = Some(cx.spawn_in(window, async move |this, cx| {
            let created = create.await;
            this.update_in(cx, |this, window, cx| {
                this._create = None;
                match created {
                    Ok(pull_request) => {
                        if this
                            .snapshot
                            .as_ref()
                            .is_some_and(|snapshot| snapshot.path == path)
                        {
                            // Shown once the sidebar lists it.
                            this.open_after_reload = Some(pull_request.number);
                            this.reload(window, cx);
                        }
                    }
                    Err(error) => window.push_notification(
                        Notification::error(format!(
                            "Could not create the local pull request: {:#}",
                            anyhow::Error::from(error)
                        )),
                        cx,
                    ),
                }
            })
            .ok();
        }));
    }

    /// Offers the ways to bring branch `head` into `base`, dropped on it in
    /// the history.
    fn open_branch_merge(
        &mut self,
        dropped: &BranchDropped,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let (head, base) = (dropped.head.clone(), dropped.base.clone());
        // Merging the branches merges their pull request.
        let pull_request = snapshot
            .pull_requests
            .iter()
            .find(|pull_request| {
                pull_request.state == State::Open
                    && pull_request.head.branch == head
                    && pull_request.base.branch == base
            })
            .map(|pull_request| MergedPullRequest {
                number: pull_request.number,
                title: pull_request.title.clone(),
                description: pull_request.description.clone(),
            });
        let merging = Merging {
            pull_request,
            head: head.clone(),
            base: base.clone(),
            head_tip: dropped.head_tip,
            base_tip: dropped.base_tip,
        };
        let (repository, store) = (snapshot.repository.clone(), snapshot.reviews.clone());
        let dialog = cx.new(|cx| MergeDialog::new(repository, store, merging, true, cx));
        let (head_tip, base_tip) = (dropped.head_tip, dropped.base_tip);
        let subscription = cx.subscribe_in(&dialog, window, move |this, _, event, window, cx| {
            window.close_dialog(cx);
            this.merge_dialog = None;
            let done = match event {
                MergeEvent::Merged(Some(pull_request)) => format!(
                    "Merged #{} into {}",
                    pull_request.number, pull_request.base.branch
                ),
                MergeEvent::Merged(None) => format!("Merged {head} into {base}"),
                MergeEvent::Rebased => format!("Rebased {head} onto {base}"),
                MergeEvent::Stopped => {
                    this.reload(window, cx);
                    this.refresh_status(window, cx);
                    this.open_conflicts(window, cx);
                    return;
                }
                MergeEvent::Review(Some(number)) => {
                    this.open_pull_request(*number, window, cx);
                    return;
                }
                MergeEvent::Review(None) => {
                    let new = NewPullRequestFields {
                        title: head.clone(),
                        description: String::new(),
                        base: Tip {
                            branch: base.clone(),
                            commit: base_tip.to_string(),
                        },
                        head: Tip {
                            branch: head.clone(),
                            commit: head_tip.to_string(),
                        },
                    };
                    this.create(new, window, cx);
                    return;
                }
            };
            window.push_notification(Notification::success(done), cx);
            this.reload(window, cx);
            this.refresh_status(window, cx);
        });
        self.merge_dialog = Some(subscription);
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |modal, _, _| {
            let workspace = workspace.clone();
            modal
                .w(px(460.))
                .child(dialog.clone())
                .on_close(move |_, _, cx| {
                    workspace
                        .update(cx, |this, _| this.merge_dialog = None)
                        .ok();
                })
        });
    }

    /// Shows the cherry-pick panel for the commits picked in the history.
    fn open_cherry_pick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let commits = self.history.read(cx).picked();
        if commits.is_empty() {
            return;
        }
        let uncommitted = self.status.as_ref().map_or(0, |status| status.files.len());
        let panel = cx.new(|cx| CherryPick::new(snapshot, commits, uncommitted, window, cx));
        let subscription = cx.subscribe_in(&panel, window, |this, _, event, window, cx| {
            this.cherry_pick = None;
            if let CherryPickEvent::Done(done) = event {
                window.push_notification(Notification::success(done.clone()), cx);
                this.history
                    .update(cx, |history, cx| history.clear_picked(cx));
                this.reload(window, cx);
                this.refresh_status(window, cx);
            }
            cx.notify();
        });
        self.cherry_pick = Some((panel, subscription));
        cx.notify();
    }

    /// Shows the history of the branch checked out from commit `from` up, to
    /// rewrite it, with `reword` to be reworded and, with `autosquash`, the
    /// fixups folded in.
    fn open_rewrite(
        &mut self,
        from: Oid,
        reword: Option<Oid>,
        autosquash: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        let refusal = if snapshot.merge.is_some() {
            Some("Finish or abort the merge in progress before rewriting history.")
        } else if snapshot.rebase.is_some() {
            Some("Continue or abort the rebase in progress before rewriting history again.")
        } else {
            None
        };
        if let Some(refusal) = refusal {
            window.push_notification(Notification::warning(refusal), cx);
            return;
        }
        let Some(Head::Branch { name, target }) = snapshot.refs.head.clone() else {
            window.push_notification(
                Notification::warning("Check out a branch to rewrite its history."),
                cx,
            );
            return;
        };
        let uncommitted = self.status.as_ref().map_or(0, |status| status.files.len());
        let rewrite = cx.new(|cx| {
            Rewrite::new(
                snapshot,
                name,
                target,
                from,
                reword,
                autosquash,
                uncommitted,
                window,
                cx,
            )
        });
        let subscription = cx.subscribe_in(&rewrite, window, |this, _, event, window, cx| {
            match event {
                RewriteEvent::Close => {}
                RewriteEvent::Done {
                    branch,
                    backup,
                    message,
                } => {
                    let (branch, backup) = (branch.clone(), backup.clone());
                    this.notify_with_action(
                        Notification::success(message.clone()),
                        "Undo",
                        move |this, window, cx| this.undo_rewrite(&branch, &backup, window, cx),
                        window,
                        cx,
                    );
                }
                RewriteEvent::Stopped(message) => {
                    window.push_notification(Notification::info(message.clone()), cx);
                }
            }
            this.rewrite = None;
            window.focus(&this.history.focus_handle(cx), cx);
            this.reload(window, cx);
            this.refresh_status(window, cx);
        });
        window.focus(&rewrite.focus_handle(cx), cx);
        self.rewrite = Some((rewrite, subscription));
        cx.notify();
    }

    /// Runs `command` in the working tree off the UI thread, then reads the
    /// repository again; `done` says what to tell about its outcome.
    fn run_rewrite_command<T: Send + 'static>(
        &mut self,
        command: impl FnOnce(Git) -> gitez_command::Result<T> + Send + 'static,
        done: impl FnOnce(gitez_command::Result<T>) -> Notification + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.repository.work_dir().map(PathBuf::from))
        else {
            return;
        };
        let run = cx
            .background_executor()
            .spawn(async move { command(Git::new(dir)) });
        self.rewrite_command = Some(cx.spawn_in(window, async move |this, cx| {
            let result = run.await;
            this.update_in(cx, |this, window, cx| {
                this.rewrite_command = None;
                window.push_notification(done(result), cx);
                this.reload(window, cx);
                this.refresh_status(window, cx);
            })
            .ok();
        }));
        cx.notify();
    }

    /// Puts branch `branch` back on `backup`, where it was before a rewrite.
    fn undo_rewrite(
        &mut self,
        branch: &str,
        backup: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (branch, backup) = (branch.to_owned(), backup.to_owned());
        let name = branch.clone();
        self.run_rewrite_command(
            move |git| git.restore(&branch, &backup),
            move |result| match result {
                Ok(()) => Notification::success(format!("{name} is back as it was")),
                Err(error) => Notification::error(format!("Could not undo the rewrite: {error}")),
            },
            window,
            cx,
        );
    }

    fn continue_rebase(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_rewrite_command(
            |git| git.continue_rebase(),
            |result| match result {
                Ok(RewriteOutcome::Done) => Notification::success("Rewrite finished"),
                Ok(RewriteOutcome::Stopped) => {
                    Notification::info("Stopped at the next commit to change")
                }
                Err(error) => Notification::error(format!("Could not continue: {error}")),
            },
            window,
            cx,
        );
    }

    fn abort_rebase(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_rewrite_command(
            |git| git.abort_rebase(),
            |result| match result {
                Ok(()) => Notification::success("Rewrite aborted; everything is as it was before"),
                Err(error) => Notification::error(format!("Could not abort: {error}")),
            },
            window,
            cx,
        );
    }

    /// Makes the command palette for the repository of `snapshot`.
    fn open_palette_for(
        &mut self,
        snapshot: &Snapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (repository, history) = (snapshot.repository.clone(), self.history.clone());
        let palette = cx.new(|cx| Palette::new(repository, history, window, cx));
        let subscription =
            cx.subscribe_in(&palette, window, |this, _, event, window, cx| match event {
                PaletteEvent::Dismiss => this.close_palette(window, cx),
                PaletteEvent::Command(command) => {
                    this.close_palette(window, cx);
                    this.run_palette_command(command.clone(), window, cx);
                }
                PaletteEvent::Finished { command, ok } => {
                    if !this.palette_open {
                        window.push_notification(
                            if *ok {
                                Notification::success(format!("{command} finished"))
                            } else {
                                Notification::error(format!(
                                    "{command} failed; ⌘K shows what it wrote"
                                ))
                            },
                            cx,
                        );
                    }
                    this.reload(window, cx);
                    this.refresh_status(window, cx);
                }
            });
        self.palette = Some((palette, subscription));
        self.palette_open = false;
    }

    /// Shows the command palette.
    fn open_palette(&mut self, _: &OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(snapshot), Some((palette, _))) = (&self.snapshot, &self.palette) else {
            return;
        };
        let branch = match &snapshot.refs.head {
            Some(Head::Branch { name, .. }) => Some(name.clone()),
            _ => None,
        };
        let context = PaletteContext {
            branch,
            head: snapshot.refs.head.as_ref().and_then(Head::target),
            merging: snapshot.merge.is_some(),
            rebasing: snapshot.rebase.is_some(),
            picked: self.history.read(cx).picked().len(),
            changed: self.status.as_ref().map_or_else(Vec::new, |status| {
                status.files.iter().map(|file| file.path.clone()).collect()
            }),
            branches: snapshot
                .refs
                .branches
                .iter()
                .map(|branch| branch.name.clone())
                .collect(),
            remote: Some(RemoteActions::of(snapshot)),
        };
        let palette = palette.clone();
        palette.update(cx, |palette, cx| palette.open(context, window, cx));
        window.focus(&palette.read(cx).input_focus(cx), cx);
        self.palette_open = true;
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.palette_open {
            return;
        }
        self.palette_open = false;
        let focus = if let Some((rewrite, _)) = &self.rewrite {
            rewrite.focus_handle(cx)
        } else if let Some((review, _)) = &self.review {
            review.focus_handle(cx)
        } else {
            self.history.focus_handle(cx)
        };
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Does what was chosen in the palette.
    fn run_palette_command(
        &mut self,
        command: PaletteCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            PaletteCommand::OpenRepository => {
                self.prompt_for_repository(&OpenRepository, window, cx)
            }
            PaletteCommand::NewPullRequest => self.new_pull_request(&NewPullRequest, window, cx),
            PaletteCommand::CommitChanges => self.open_working_tree(None, window, cx),
            PaletteCommand::OpenFile(path) => self.open_working_tree(Some(path), window, cx),
            PaletteCommand::RewordLast => {
                let head = self
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.refs.head.as_ref())
                    .and_then(Head::target);
                if let Some(head) = head {
                    self.open_rewrite(head, Some(head), false, window, cx);
                }
            }
            PaletteCommand::RewriteSince { base, autosquash } => {
                self.open_rewrite_since(base, autosquash, window, cx)
            }
            PaletteCommand::ContinueRebase => self.continue_rebase(window, cx),
            PaletteCommand::AbortRebase => self.abort_rebase(window, cx),
            PaletteCommand::ResolveConflicts => self.open_conflicts(window, cx),
            PaletteCommand::CherryPick => self.open_cherry_pick(window, cx),
            PaletteCommand::NewBranch => self.new_branch(&NewBranch, window, cx),
            PaletteCommand::RenameBranch => {
                if let Some(Head::Branch { name, .. }) = self
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.refs.head.clone())
                {
                    self.rename_branch(name, window, cx);
                }
            }
            PaletteCommand::Fetch => self.fetch(&Fetch, window, cx),
            PaletteCommand::Pull => self.pull(&Pull, window, cx),
            PaletteCommand::Push => self.push(&Push, window, cx),
            PaletteCommand::SelectCommit(id) => {
                self.review = None;
                self.rewrite = None;
                let found = self
                    .history
                    .update(cx, |history, cx| history.select_commit(id, cx));
                if !found {
                    window.push_notification(
                        Notification::info("That commit is not in the history shown."),
                        cx,
                    );
                }
                window.focus(&self.history.focus_handle(cx), cx);
                cx.notify();
            }
            PaletteCommand::Switch(branch) => self.switch_branch(branch, window, cx),
        }
    }

    /// Shows the uncommitted changes to review and commit, scrolled to the
    /// file at `path` if given.
    fn open_working_tree(
        &mut self,
        path: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected = self
            .history
            .update(cx, |history, cx| history.select_working_tree(cx));
        if selected {
            self.rewrite = None;
            self.open_review(path, window, cx);
        }
    }

    /// Rewrites the commits after revision `base`, as `git rebase -i base`
    /// would.
    fn open_rewrite_since(
        &mut self,
        base: String,
        autosquash: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let Some(tip) = snapshot.refs.head.as_ref().and_then(Head::target) else {
            return;
        };
        let repository = snapshot.repository.clone();
        let find = cx.background_executor().spawn({
            let base = base.clone();
            async move {
                let Some(base) = repository.resolve_commit(&base)? else {
                    return Ok(None);
                };
                repository.first_after(tip, base)
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let found: gitez_git::Result<Option<Oid>> = find.await;
            this.update_in(cx, |this, window, cx| match found {
                Ok(Some(from)) => this.open_rewrite(from, None, autosquash, window, cx),
                Ok(None) => window.push_notification(
                    Notification::warning(format!(
                        "{base} is not a commit before the tip of the branch checked out."
                    )),
                    cx,
                ),
                Err(error) => window.push_notification(
                    Notification::error(format!(
                        "Could not find {base}: {:#}",
                        anyhow::Error::from(error)
                    )),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    /// Checks out local branch `branch`.
    fn switch_branch(&mut self, branch: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.repository.work_dir().map(PathBuf::from))
        else {
            return;
        };
        let switch = cx.background_executor().spawn({
            let branch = branch.clone();
            async move { Git::new(dir).switch(&branch) }
        });
        self._switch = Some(cx.spawn_in(window, async move |this, cx| {
            let switched = switch.await;
            this.update_in(cx, |this, window, cx| {
                this._switch = None;
                match switched {
                    Ok(()) => window.push_notification(
                        Notification::success(format!("Checked out {branch}")),
                        cx,
                    ),
                    Err(error) => window.push_notification(
                        Notification::error(format!("Could not check out {branch}: {error}")),
                        cx,
                    ),
                }
                this.reload(window, cx);
                this.refresh_status(window, cx);
            })
            .ok();
        }));
    }

    /// Shows the conflicts of the merge in progress.
    fn open_conflicts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let pull_requests = Arc::new(snapshot.pull_requests.clone());
        let (repository, reviews) = (snapshot.repository.clone(), snapshot.reviews.clone());
        let conflicts = cx.new(|cx| Conflicts::new(repository, reviews, pull_requests, cx));
        let subscription = cx.subscribe_in(&conflicts, window, |this, _, event, window, cx| {
            match event {
                ConflictsEvent::Close => this.close_conflicts(window, cx),
                ConflictsEvent::Changed => this.refresh_status(window, cx),
                ConflictsEvent::Finished { committed } => {
                    window.push_notification(
                        Notification::success(if *committed {
                            "Merge committed"
                        } else {
                            "Merge aborted; everything is as it was before"
                        }),
                        cx,
                    );
                    this.close_conflicts(window, cx);
                    // A merged pull request shows as merged.
                    this.close_review(window, cx);
                    this.reload(window, cx);
                    this.refresh_status(window, cx);
                }
            }
        });
        window.focus(&conflicts.focus_handle(cx), cx);
        self.conflicts = Some((conflicts, subscription));
        cx.notify();
    }

    fn close_conflicts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.conflicts = None;
        window.focus(&self.history.focus_handle(cx), cx);
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
            .on_action(cx.listener(Self::new_pull_request))
            .on_action(cx.listener(Self::open_palette))
            .on_action(cx.listener(Self::fetch))
            .on_action(cx.listener(Self::pull))
            .on_action(cx.listener(Self::push))
            .on_action(cx.listener(Self::new_branch))
            .relative()
            .v_flex()
            .size_full()
            .bg(rgb(color::BACKGROUND))
            .text_color(rgb(color::TEXT))
            .text_size(theme::TEXT_SIZE)
            .child(toolbar::Toolbar {
                repository_name: snapshot.as_ref().map(|snapshot| snapshot.name.clone()),
                loading: self.loading.is_some(),
                remote: snapshot.as_deref().map(RemoteActions::of),
                running: self.remote_op.as_ref().map(|(op, _)| *op),
                can_branch: snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.refs.head.as_ref())
                    .and_then(Head::target)
                    .is_some(),
            })
            .when_some(
                snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.merge.as_ref())
                    .filter(|_| self.conflicts.is_none()),
                |this, merge| this.child(merge_banner(merge, cx)),
            )
            .when_some(
                snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.rebase.as_ref()),
                |this, rebase| this.child(rebase_banner(rebase, self.rewrite_command.is_some(), cx)),
            )
            .child(div().flex_1().min_h_0().map(|this| {
                if let Some((conflicts, _)) = &self.conflicts {
                    return this.child(conflicts.clone());
                }
                if let Some((rewrite, _)) = &self.rewrite {
                    return this.child(rewrite.clone());
                }
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
                                    if let Some((cherry_pick, _)) = &self.cherry_pick {
                                        panel.child(cherry_pick.clone())
                                    } else if snapshot.is_some() {
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
            .when_some(
                self.palette
                    .as_ref()
                    .filter(|_| self.palette_open)
                    .map(|(palette, _)| palette.clone()),
                |this, palette| {
                    this.child(
                        div()
                            .id("palette-backdrop")
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .flex()
                            .justify_center()
                            .items_start()
                            .pt(px(56.))
                            .bg(rgba(color::BACKDROP))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_palette(window, cx)
                            }))
                            .child(palette),
                    )
                },
            )
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

/// The commits the branches of local pull request `number` are on in
/// `snapshot`; `None` if there is no such pull request.
fn tips_of(snapshot: &Snapshot, number: u32) -> Option<Tips> {
    let pull_request = snapshot
        .pull_requests
        .iter()
        .find(|pull_request| pull_request.number == number)?;
    Some(Tips {
        head: snapshot.branch_tip(&pull_request.head.branch),
        base: snapshot.branch_tip(&pull_request.base.branch),
        checked_out: snapshot
            .refs
            .branches
            .iter()
            .any(|branch| branch.is_head && branch.name == pull_request.head.branch),
    })
}

fn branch_label(name: &str) -> RefLabel {
    RefLabel {
        kind: RefKind::Branch,
        name: name.to_owned(),
        is_head: true,
    }
}

/// The strip saying a merge is waiting for its conflicts to be resolved.
fn merge_banner(merge: &gitez_git::MergeInProgress, cx: &mut Context<Workspace>) -> Div {
    let theirs = merge
        .theirs_branch
        .clone()
        .unwrap_or_else(|| merge.theirs.to_hex_with_len(7).to_string());
    let into = merge.branch.clone().unwrap_or_else(|| "HEAD".into());
    div()
        .h_flex()
        .flex_none()
        .gap(px(10.))
        .px(px(16.))
        .py(px(8.))
        .border_b_1()
        .border_color(rgb(color::BORDER))
        .bg(rgb(color::SELECTED))
        .text_size(px(12.5))
        .child(
            Icon::new(IconName::GitMerge)
                .size(px(15.))
                .text_color(rgb(color::ACCENT)),
        )
        .child(format!(
            "Merging {theirs} into {into} stopped with conflicts to resolve."
        ))
        .child(
            Button::new("resolve-conflicts")
                .ml_auto()
                .small()
                .primary()
                .label("Resolve conflicts")
                .on_click(cx.listener(|this, _, window, cx| this.open_conflicts(window, cx))),
        )
}

/// The strip saying a rebase stopped, with the buttons to go on or give
/// it up.
fn rebase_banner(
    rebase: &gitez_git::RebaseInProgress,
    running: bool,
    cx: &mut Context<Workspace>,
) -> Div {
    let branch = rebase.branch.clone().unwrap_or_else(|| "HEAD".into());
    let at = match (&rebase.stopped_at, &rebase.summary) {
        (Some(id), Some(summary)) => format!(" at {} “{summary}”", id.to_hex_with_len(7)),
        (Some(id), None) => format!(" at {}", id.to_hex_with_len(7)),
        _ => String::new(),
    };
    div()
        .h_flex()
        .flex_none()
        .gap(px(10.))
        .px(px(16.))
        .py(px(8.))
        .border_b_1()
        .border_color(rgb(color::BORDER))
        .bg(rgb(color::SELECTED))
        .text_size(px(12.5))
        .child(
            Icon::new(IconName::Pencil)
                .size(px(15.))
                .text_color(rgb(color::ACCENT)),
        )
        .child(div().flex_1().min_w_0().child(format!(
            "Rewriting {branch} stopped{at}. Change what you need and amend the commit, \
                 then continue."
        )))
        .child(
            Button::new("abort-rebase")
                .small()
                .ghost()
                .label("Abort")
                .disabled(running)
                .on_click(cx.listener(|this, _, window, cx| this.abort_rebase(window, cx))),
        )
        .child(
            Button::new("continue-rebase")
                .small()
                .primary()
                .label("Continue")
                .loading(running)
                .on_click(cx.listener(|this, _, window, cx| this.continue_rebase(window, cx))),
        )
}
