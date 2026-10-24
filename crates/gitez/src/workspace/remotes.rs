//! Managing remotes: adding, editing and removing them, and setting the
//! remote branch a local branch tracks.

use gitez_git::remote_name_problem;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::select::{SearchableVec, Select, SelectState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{AddRemote, Workspace};
use crate::theme::color;

/// What the remote form edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteFormKind {
    Add,
    /// Remote `name`, which fetches from `url`.
    Edit {
        name: String,
        url: String,
    },
}

/// What a filled-in remote form asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteRequest {
    Add {
        name: String,
        url: String,
    },
    Edit {
        from: String,
        to: String,
        old_url: String,
        url: String,
    },
}

/// What a form asks of the window.
pub enum RemoteFormEvent {
    /// Enter was pressed in a field.
    Submit,
}

pub struct RemoteForm {
    kind: RemoteFormKind,
    name: Entity<InputState>,
    url: Entity<InputState>,
    /// The remotes there are, which a new name must not repeat.
    remotes: Vec<String>,
    /// Why the form cannot be used as filled in; shown once something is
    /// typed, or on submitting.
    error: Option<SharedString>,
    _events: [Subscription; 2],
}

impl EventEmitter<RemoteFormEvent> for RemoteForm {}

impl RemoteForm {
    pub fn new(
        kind: RemoteFormKind,
        remotes: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (name, url) = match &kind {
            RemoteFormKind::Add => {
                let name = if remotes.is_empty() { "origin" } else { "" };
                (name.to_owned(), String::new())
            }
            RemoteFormKind::Edit { name, url } => (name.clone(), url.clone()),
        };
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("origin")
                .default_value(name)
        });
        let url = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("git@github.com:owner/repository.git")
                .default_value(url)
        });
        let changed = |this: &mut Self, event: &InputEvent, cx: &mut Context<Self>| match event {
            InputEvent::PressEnter { .. } => cx.emit(RemoteFormEvent::Submit),
            InputEvent::Change => {
                this.error = this.problem(cx).filter(|_| !this.name(cx).is_empty());
                cx.notify();
            }
            _ => {}
        };
        let name_events = cx.subscribe_in(&name, window, move |this, _, event, _, cx| {
            changed(this, event, cx)
        });
        let url_events = cx.subscribe_in(&url, window, move |this, _, event, _, cx| {
            changed(this, event, cx)
        });
        Self {
            kind,
            name,
            url,
            remotes,
            error: None,
            _events: [name_events, url_events],
        }
    }

    pub fn title(&self) -> &'static str {
        match self.kind {
            RemoteFormKind::Add => "Add remote",
            RemoteFormKind::Edit { .. } => "Edit remote",
        }
    }

    pub fn submit_label(&self) -> &'static str {
        match self.kind {
            RemoteFormKind::Add => "Add remote",
            RemoteFormKind::Edit { .. } => "Save",
        }
    }

    /// The field focused when the dialog opens: the name to add, or the
    /// URL to change.
    pub fn first_input(&self) -> &Entity<InputState> {
        match self.kind {
            RemoteFormKind::Add if self.remotes.is_empty() => &self.url,
            RemoteFormKind::Add => &self.name,
            RemoteFormKind::Edit { .. } => &self.url,
        }
    }

    fn name(&self, cx: &App) -> String {
        self.name.read(cx).value().trim().to_owned()
    }

    fn url(&self, cx: &App) -> String {
        self.url.read(cx).value().trim().to_owned()
    }

    /// Why the form cannot be used as filled in, if it cannot.
    fn problem(&self, cx: &App) -> Option<SharedString> {
        let name = self.name(cx);
        if let Some(problem) = remote_name_problem(&name) {
            return Some(problem.into());
        }
        let unchanged =
            matches!(&self.kind, RemoteFormKind::Edit { name: from, .. } if *from == name);
        if !unchanged && self.remotes.contains(&name) {
            return Some(format!("A remote named {name} already exists.").into());
        }
        None
    }

    /// What the form asks for; `None` after showing why when it cannot be
    /// used.
    pub fn submit(&mut self, cx: &mut Context<Self>) -> Option<RemoteRequest> {
        let problem = self.problem(cx).or_else(|| {
            self.url(cx)
                .is_empty()
                .then(|| "Give the URL to fetch from.".into())
        });
        if let Some(problem) = problem {
            self.error = Some(problem);
            cx.notify();
            return None;
        }
        let (name, url) = (self.name(cx), self.url(cx));
        Some(match &self.kind {
            RemoteFormKind::Add => RemoteRequest::Add { name, url },
            RemoteFormKind::Edit {
                name: from,
                url: old_url,
            } => RemoteRequest::Edit {
                from: from.clone(),
                to: name,
                old_url: old_url.clone(),
                url,
            },
        })
    }
}

impl Render for RemoteForm {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .gap(px(14.))
            .pb(px(4.))
            .child(field("Name", Input::new(&self.name)))
            .child(field("URL", Input::new(&self.url)))
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .text_size(px(12.5))
                        .text_color(rgb(color::DIFF_REMOVED))
                        .child(error),
                )
            })
    }
}

/// A caption above a control.
fn field(caption: &'static str, control: impl IntoElement) -> impl IntoElement {
    div()
        .v_flex()
        .gap(px(6.))
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_MUTED))
                .child(caption),
        )
        .child(control)
}

/// The choice that unsets an upstream.
const NO_UPSTREAM: &str = "None";

/// Asks which remote branch local branch `branch` tracks.
pub struct UpstreamForm {
    branch: String,
    choice: Entity<SelectState<SearchableVec<String>>>,
}

impl UpstreamForm {
    /// `remote_branches` are short names such as `origin/main`; `current`
    /// is the upstream now.
    pub fn new(
        branch: String,
        remote_branches: Vec<String>,
        current: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut choices = vec![NO_UPSTREAM.to_owned()];
        choices.extend(remote_branches);
        let selected = current
            .and_then(|current| choices.iter().position(|choice| *choice == current))
            .unwrap_or(0);
        let choice = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(choices),
                Some(IndexPath::new(selected)),
                window,
                cx,
            )
            .searchable(true)
        });
        Self { branch, choice }
    }

    /// The upstream chosen; `None` to have none.
    fn upstream(&self, cx: &App) -> Option<String> {
        self.choice
            .read(cx)
            .selected_value()
            .filter(|choice| *choice != NO_UPSTREAM)
            .cloned()
    }
}

impl Render for UpstreamForm {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .gap(px(14.))
            .pb(px(4.))
            .child(div().text_color(rgb(color::TEXT_SECONDARY)).child(format!(
                "The remote branch {} pulls from and pushes to.",
                self.branch
            )))
            .child(field("Upstream", Select::new(&self.choice).w_full()))
    }
}

impl Workspace {
    /// Asks for a remote to add.
    pub(super) fn add_remote(
        &mut self,
        _: &AddRemote,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_remote_form(RemoteFormKind::Add, window, cx);
    }

    /// Asks for a new name or URL for remote `name`.
    pub(super) fn edit_remote(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let url = snapshot.remote_urls.get(&name).cloned().unwrap_or_default();
        self.open_remote_form(RemoteFormKind::Edit { name, url }, window, cx);
    }

    fn open_remote_form(
        &mut self,
        kind: RemoteFormKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let remotes = snapshot.remotes.clone();
        let form = cx.new(|cx| RemoteForm::new(kind, remotes, window, cx));
        let subscription =
            cx.subscribe_in(&form, window, |this, _, event, window, cx| match event {
                RemoteFormEvent::Submit => this.submit_remote_form(window, cx),
            });
        self.remote_form = Some((form.clone(), subscription));
        let workspace = cx.entity().downgrade();
        let focus_first = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if focus_first.replace(false) {
                let first = form.read(cx).first_input().focus_handle(cx);
                window.focus(&first, cx);
            }
            let (title, submit_label) = {
                let form = form.read(cx);
                (form.title(), form.submit_label())
            };
            let submit = workspace.clone();
            dialog
                .title(title)
                .w(px(460.))
                .child(form.clone())
                .footer(
                    div()
                        .h_flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            Button::new("remote-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("remote-submit")
                                .primary()
                                .label(submit_label)
                                .on_click(move |_, window, cx| {
                                    submit
                                        .update(cx, |this, cx| this.submit_remote_form(window, cx))
                                        .ok();
                                }),
                        ),
                )
                .on_close({
                    let workspace = workspace.clone();
                    move |_, _, cx| {
                        workspace.update(cx, |this, _| this.remote_form = None).ok();
                    }
                })
        });
    }

    /// Adds or changes the remote the open form describes. A new remote
    /// is fetched straight away, so its branches show.
    fn submit_remote_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((form, _)) = &self.remote_form else {
            return;
        };
        let Some(request) = form.update(cx, |form, cx| form.submit(cx)) else {
            return;
        };
        window.close_dialog(cx);
        self.remote_form = None;
        match request {
            RemoteRequest::Add { name, url } => {
                let remote = name.clone();
                self.run_git(
                    None,
                    move |git| {
                        git.add_remote(&remote, &url)?;
                        Ok(git.fetch_remote(&remote).err())
                    },
                    move |_, result, _, window, cx| {
                        let notification = match result {
                            Ok(None) => {
                                Notification::success(format!("Added {name} and fetched it"))
                            }
                            Ok(Some(error)) => Notification::warning(format!(
                                "Added {name}, but could not fetch it: {error}"
                            )),
                            Err(error) => {
                                Notification::error(format!("Could not add {name}: {error}"))
                            }
                        };
                        window.push_notification(notification, cx);
                    },
                    window,
                    cx,
                );
            }
            RemoteRequest::Edit {
                from,
                to,
                old_url,
                url,
            } => {
                if from == to && old_url == url {
                    return;
                }
                let (old, new) = (from.clone(), to.clone());
                self.run_git(
                    None,
                    move |git| {
                        if old != new {
                            git.rename_remote(&old, &new)?;
                        }
                        if old_url != url {
                            git.set_remote_url(&new, &url)?;
                        }
                        Ok(())
                    },
                    move |_, result, _, window, cx| {
                        let notification = match result {
                            Ok(()) if from != to => {
                                Notification::success(format!("Renamed {from} to {to}"))
                            }
                            Ok(()) => Notification::success(format!("Changed the URL of {to}")),
                            Err(error) => {
                                Notification::error(format!("Could not change {from}: {error}"))
                            }
                        };
                        window.push_notification(notification, cx);
                    },
                    window,
                    cx,
                );
            }
        }
    }

    /// Fetches remote `name` alone.
    pub(super) fn fetch_one_remote(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let remote = name.clone();
        self.run_git(
            None,
            move |git| git.fetch_remote(&remote),
            move |_, result, _, window, cx| {
                let notification = match result {
                    Ok(()) => Notification::success(format!("Fetched {name}")),
                    Err(error) => Notification::error(format!("Could not fetch {name}: {error}")),
                };
                window.push_notification(notification, cx);
            },
            window,
            cx,
        );
    }

    /// Removes remote `name` once `confirmed`; until then, asks.
    pub(super) fn remove_remote(
        &mut self,
        name: String,
        confirmed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !confirmed {
            let again = name.clone();
            self.notify_with_action(
                Notification::warning(format!(
                    "Remove {name}? Its remote-tracking branches go, and branches that track \
                     it no longer have an upstream. GitEZ cannot undo this; adding it again \
                     and fetching brings the branches back."
                )),
                "Remove",
                move |this, window, cx| this.remove_remote(again.clone(), true, window, cx),
                window,
                cx,
            );
            return;
        }
        let remote = name.clone();
        self.run_git(
            None,
            move |git| git.remove_remote(&remote),
            move |_, result, _, window, cx| {
                let notification = match result {
                    Ok(()) => Notification::success(format!("Removed {name}")),
                    Err(error) => Notification::error(format!("Could not remove {name}: {error}")),
                };
                window.push_notification(notification, cx);
            },
            window,
            cx,
        );
    }

    /// Asks which remote branch local branch `branch` tracks.
    pub(super) fn set_upstream(
        &mut self,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let remote_branches: Vec<String> = snapshot
            .refs
            .remote_branches
            .iter()
            .map(|remote| remote.short_name())
            .collect();
        if remote_branches.is_empty() {
            window.push_notification(
                Notification::info("There are no remote branches to track; fetch a remote first."),
                cx,
            );
            return;
        }
        let current = snapshot
            .tracking
            .get(&branch)
            .map(|tracking| tracking.upstream.clone());
        let form =
            cx.new(|cx| UpstreamForm::new(branch.clone(), remote_branches, current, window, cx));
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let (submit, chosen) = (workspace.clone(), form.clone());
            dialog
                .title(format!("Upstream of {branch}"))
                .w(px(460.))
                .child(form.clone())
                .footer(
                    div()
                        .h_flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            Button::new("upstream-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("upstream-submit")
                                .primary()
                                .label("Save")
                                .on_click(move |_, window, cx| {
                                    let (branch, upstream) = {
                                        let form = chosen.read(cx);
                                        (form.branch.clone(), form.upstream(cx))
                                    };
                                    window.close_dialog(cx);
                                    submit
                                        .update(cx, |this, cx| {
                                            this.save_upstream(branch, upstream, window, cx)
                                        })
                                        .ok();
                                }),
                        ),
                )
        });
    }

    fn save_upstream(
        &mut self,
        branch: String,
        upstream: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (local, to) = (branch.clone(), upstream.clone());
        self.run_git(
            None,
            move |git| git.set_upstream(&local, to.as_deref()),
            move |_, result, _, window, cx| {
                let notification = match (result, upstream) {
                    (Ok(()), Some(upstream)) => {
                        Notification::success(format!("{branch} now tracks {upstream}"))
                    }
                    (Ok(()), None) => {
                        Notification::success(format!("{branch} no longer has an upstream"))
                    }
                    (Err(error), _) => Notification::error(format!(
                        "Could not set the upstream of {branch}: {error}"
                    )),
                };
                window.push_notification(notification, cx);
            },
            window,
            cx,
        );
    }
}
