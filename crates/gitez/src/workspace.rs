//! The main window: toolbar, repository sidebar, history, commit detail and
//! status bar, laid out as in the `Main.dc.html` artboard.

mod detail;
mod graph;
mod history;
mod labels;
mod sidebar;
mod status_bar;
mod toolbar;

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::button::*;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{StyledExt as _, WindowExt as _, h_resizable, resizable_panel};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::repository::Snapshot;
use crate::theme::{self, color};
use detail::CommitDetail;
use history::History;
use sidebar::Sidebar;

pub use history::key_bindings as history_key_bindings;

actions!(workspace, [OpenRepository]);

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
    sidebar: Entity<Sidebar>,
    history: Entity<History>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    /// Creates the workspace and starts opening `path`, if given.
    pub fn new(path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let history = cx.new(History::new);
        // The detail pane shows the selected commit.
        let subscriptions = vec![cx.subscribe(&history, |_, _, _, cx| cx.notify())];
        let mut workspace = Self {
            focus_handle,
            snapshot: None,
            loading: None,
            error: None,
            sidebar: cx.new(|_| Sidebar::new()),
            history,
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
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.set_snapshot(snapshot.clone(), cx));
        self.history
            .update(cx, |history, cx| history.load(&snapshot, cx));
        window.focus(&self.history.focus_handle(cx), cx);
        self.snapshot = Some(snapshot);
        self.error = None;
        cx.notify();
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
            .v_flex()
            .size_full()
            .bg(rgb(color::BACKGROUND))
            .text_color(rgb(color::TEXT))
            .text_size(theme::TEXT_SIZE)
            .child(toolbar::Toolbar {
                repository_name: snapshot.as_ref().map(|snapshot| snapshot.name.clone()),
                loading: self.loading.is_some(),
            })
            .child(
                div().flex_1().min_h_0().child(
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
                                .child(self.render_detail(cx)),
                        ),
                ),
            )
            .child(status_bar::StatusBar { snapshot })
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

    /// Details of the selected commit.
    fn render_detail(&self, cx: &App) -> impl IntoElement {
        let selected = self
            .snapshot
            .as_ref()
            .and_then(|_| self.history.read(cx).selected())
            .map(|(commit, labels)| CommitDetail {
                commit: commit.clone(),
                labels: labels.to_vec(),
            });
        let pane = div().v_flex().size_full().bg(rgb(color::PANEL));
        match selected {
            Some(detail) => pane.child(detail),
            None => {
                let detail = if self.snapshot.is_some() {
                    "Select a commit to see its details."
                } else {
                    "Commit details appear here once a repository is open."
                };
                pane.child(empty_state("No commit selected", detail, None::<Button>))
            }
        }
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
