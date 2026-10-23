//! The main window: toolbar, repository sidebar, history, commit detail and
//! status bar, laid out as in the `Main.dc.html` artboard.

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
use sidebar::Sidebar;

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
}

impl Workspace {
    /// Creates the workspace and starts opening `path`, if given.
    pub fn new(path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let mut workspace = Self {
            focus_handle,
            snapshot: None,
            loading: None,
            error: None,
            sidebar: cx.new(|_| Sidebar::new()),
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
                                .child(detail_pane(snapshot.is_some())),
                        ),
                ),
            )
            .child(status_bar::StatusBar { snapshot })
    }
}

/// Column widths of the history list, shared by its header and its rows.
mod columns {
    use gpui_kit::{Pixels, px};

    pub const GRAPH: Pixels = px(112.);
    pub const AUTHOR: Pixels = px(150.);
    pub const DATE: Pixels = px(96.);
    pub const SHA: Pixels = px(70.);
    pub const GAP: Pixels = px(12.);
}

impl Workspace {
    /// The commit history: a title bar, the column header and the commit list.
    fn render_history(&self) -> impl IntoElement {
        let column = |width: Pixels, label: &'static str| div().w(width).flex_none().child(label);

        let body = if let Some((path, _)) = &self.loading {
            empty_state(
                "Opening repository…",
                path.display().to_string(),
                None::<Button>,
            )
            .into_any_element()
        } else if self.snapshot.is_some() {
            empty_state(
                "History is not shown yet",
                "The commit graph is the next part of GitEZ to be built.",
                None::<Button>,
            )
            .into_any_element()
        } else if let Some(error) = &self.error {
            empty_state(
                "Could not open the repository",
                error.clone(),
                Some(open_button("open-repository-retry", "Open another…")),
            )
            .into_any_element()
        } else {
            empty_state(
                "No repository open",
                "Open a repository to see its history.",
                Some(open_button("open-repository", "Open repository…")),
            )
            .into_any_element()
        };

        div()
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
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .flex_none()
                    .h(px(28.))
                    .pr(px(14.))
                    .gap(columns::GAP)
                    .border_b_1()
                    .border_color(rgb(color::BORDER))
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(color::TEXT_FAINT))
                    .child(column(columns::GRAPH, "GRAPH").pl(px(14.)))
                    .child(div().flex_1().min_w_0().child("DESCRIPTION"))
                    .child(column(columns::AUTHOR, "AUTHOR"))
                    .child(column(columns::DATE, "DATE"))
                    .child(column(columns::SHA, "SHA")),
            )
            .child(body)
    }
}

/// Details of the selected commit.
fn detail_pane(has_repository: bool) -> impl IntoElement {
    let detail = if has_repository {
        "Select a commit to see its message and changed files."
    } else {
        "Commit details appear here once a repository is open."
    };
    div()
        .v_flex()
        .size_full()
        .bg(rgb(color::PANEL))
        .child(empty_state("No commit selected", detail, None::<Button>))
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
