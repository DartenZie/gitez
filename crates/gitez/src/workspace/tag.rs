//! Tags: the form that creates one, and deleting and pushing them.

use gitez_git::{Head, Oid, tag_name_problem};
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{NewTag, Workspace};
use crate::theme::color;

/// What a filled-in form asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRequest {
    pub name: String,
    pub commit: Oid,
    /// Makes an annotated tag; `None` makes a lightweight one.
    pub message: Option<String>,
}

/// What the form asks of the window.
pub enum TagFormEvent {
    /// Enter was pressed in a field.
    Submit,
}

pub struct TagForm {
    commit: Oid,
    /// The commit as the form shows it: "main · a1b2c3d Add the parser".
    on: String,
    name: Entity<InputState>,
    message: Entity<InputState>,
    /// The tags there are, which a new name must not repeat.
    tags: Vec<String>,
    /// Why the name cannot be used; shown once something is typed, or on
    /// submitting.
    error: Option<SharedString>,
    _events: [Subscription; 2],
}

impl EventEmitter<TagFormEvent> for TagForm {}

impl TagForm {
    pub fn new(
        commit: Oid,
        on: String,
        tags: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("v1.0.0"));
        let message = cx
            .new(|cx| InputState::new(window, cx).placeholder("Optional; makes an annotated tag"));
        let name_events = cx.subscribe_in(&name, window, |this, _, event, _, cx| match event {
            InputEvent::PressEnter { .. } => cx.emit(TagFormEvent::Submit),
            InputEvent::Change => {
                let name = this.name(cx);
                this.error = if name.is_empty() {
                    None
                } else {
                    this.problem(&name).map(Into::into)
                };
                cx.notify();
            }
            _ => {}
        });
        let message_events = cx.subscribe_in(&message, window, |_, _, event, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                cx.emit(TagFormEvent::Submit);
            }
        });
        Self {
            commit,
            on,
            name,
            message,
            tags,
            error: None,
            _events: [name_events, message_events],
        }
    }

    /// The name box, focused when the dialog opens.
    pub fn name_input(&self) -> &Entity<InputState> {
        &self.name
    }

    fn name(&self, cx: &App) -> String {
        self.name.read(cx).value().trim().to_owned()
    }

    /// Why `name` cannot be used, if it cannot.
    fn problem(&self, name: &str) -> Option<String> {
        if let Some(problem) = tag_name_problem(name) {
            return Some(problem.to_owned());
        }
        self.tags
            .iter()
            .any(|tag| tag == name)
            .then(|| format!("A tag named {name} already exists."))
    }

    /// What the form asks for; `None` after showing why when the name
    /// cannot be used.
    pub fn submit(&mut self, cx: &mut Context<Self>) -> Option<TagRequest> {
        let name = self.name(cx);
        if let Some(problem) = self.problem(&name) {
            self.error = Some(problem.into());
            cx.notify();
            return None;
        }
        let message = self.message.read(cx).value().trim().to_owned();
        Some(TagRequest {
            name,
            commit: self.commit,
            message: (!message.is_empty()).then_some(message),
        })
    }
}

impl Render for TagForm {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let caption = |text: &'static str| {
            div()
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_MUTED))
                .child(text)
        };
        div()
            .v_flex()
            .gap(px(14.))
            .pb(px(4.))
            .child(
                div()
                    .v_flex()
                    .gap(px(6.))
                    .child(caption("Name"))
                    .child(Input::new(&self.name))
                    .when_some(self.error.clone(), |this, error| {
                        this.child(
                            div()
                                .text_size(px(12.5))
                                .text_color(rgb(color::DIFF_REMOVED))
                                .child(error),
                        )
                    }),
            )
            .child(
                div()
                    .v_flex()
                    .gap(px(6.))
                    .child(caption("Message"))
                    .child(Input::new(&self.message)),
            )
            .child(
                div().v_flex().gap(px(6.)).child(caption("On")).child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(rgb(color::TEXT_SECONDARY))
                        .child(self.on.clone()),
                ),
            )
    }
}

impl Workspace {
    /// Asks for a tag to create on the commit selected in the history, or
    /// where `HEAD` is.
    pub(super) fn new_tag(&mut self, _: &NewTag, window: &mut Window, cx: &mut Context<Self>) {
        let selected = match self.history.read(cx).selected() {
            Some(super::history::Selection::Commit(commit, _)) => Some(commit.id),
            _ => None,
        };
        let head = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.refs.head.as_ref())
            .and_then(Head::target);
        if let Some(commit) = selected.or(head) {
            self.new_tag_at(commit, window, cx);
        }
    }

    /// Asks for a tag to create on `commit`.
    pub(super) fn new_tag_at(&mut self, commit: Oid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let tags = snapshot
            .refs
            .tags
            .iter()
            .map(|tag| tag.name.clone())
            .collect();
        let on = self.describe_commit(commit, cx);
        let form = cx.new(|cx| TagForm::new(commit, on, tags, window, cx));
        let subscription =
            cx.subscribe_in(&form, window, |this, _, event, window, cx| match event {
                TagFormEvent::Submit => this.submit_tag_form(window, cx),
            });
        self.tag_form = Some((form.clone(), subscription));
        let workspace = cx.entity().downgrade();
        let focus_name = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if focus_name.replace(false) {
                let name = form.read(cx).name_input().focus_handle(cx);
                window.focus(&name, cx);
            }
            let submit = workspace.clone();
            dialog
                .title("New tag")
                .w(px(420.))
                .child(form.clone())
                .footer(
                    div()
                        .h_flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            Button::new("tag-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("tag-submit")
                                .primary()
                                .label("Create tag")
                                .on_click(move |_, window, cx| {
                                    submit
                                        .update(cx, |this, cx| this.submit_tag_form(window, cx))
                                        .ok();
                                }),
                        ),
                )
                .on_close({
                    let workspace = workspace.clone();
                    move |_, _, cx| {
                        workspace.update(cx, |this, _| this.tag_form = None).ok();
                    }
                })
        });
    }

    /// Creates the tag the open form describes.
    fn submit_tag_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((form, _)) = &self.tag_form else {
            return;
        };
        let Some(request) = form.update(cx, |form, cx| form.submit(cx)) else {
            return;
        };
        window.close_dialog(cx);
        self.tag_form = None;
        let TagRequest {
            name,
            commit,
            message,
        } = request;
        let tag = name.clone();
        self.run_git(
            Some(format!("Create tag {name}")),
            move |git| git.create_tag(&tag, &commit.to_string(), message.as_deref()),
            move |this, result, step, window, cx| {
                let notification = match result {
                    Ok(()) => Notification::success(format!("Created tag {name}")),
                    Err(error) => {
                        Notification::error(format!("Could not create tag {name}: {error}"))
                    }
                };
                this.notify_done(notification, step, window, cx);
            },
            window,
            cx,
        );
    }

    /// Deletes tag `name`, offering to bring it back.
    pub(super) fn delete_tag(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let tag = name.clone();
        self.run_git(
            Some(format!("Delete tag {name}")),
            move |git| git.delete_tag(&tag),
            move |this, result, step, window, cx| {
                let notification = match result {
                    Ok(()) => Notification::success(format!("Deleted tag {name}")),
                    Err(error) => {
                        Notification::error(format!("Could not delete tag {name}: {error}"))
                    }
                };
                this.notify_done(notification, step, window, cx);
            },
            window,
            cx,
        );
    }

    /// The remote tags go to: `origin` when there is one, else the first.
    pub(super) fn tag_remote(&self) -> Option<String> {
        let remotes = &self.snapshot.as_ref()?.remotes;
        remotes
            .iter()
            .find(|remote| *remote == "origin")
            .or(remotes.first())
            .cloned()
    }

    /// Sends tag `name` to the remote.
    pub(super) fn push_tag(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(remote) = self.tag_remote() else {
            window.push_notification(Notification::info("There is no remote to push to."), cx);
            return;
        };
        let (to, tag) = (remote.clone(), name.clone());
        self.run_git(
            None,
            move |git| git.push_tag(&to, &tag),
            move |_, result, _, window, cx| {
                let notification = match result {
                    Ok(()) => Notification::success(format!("Pushed tag {name} to {remote}")),
                    Err(error) => {
                        Notification::error(format!("Could not push tag {name}: {error}"))
                    }
                };
                window.push_notification(notification, cx);
            },
            window,
            cx,
        );
    }

    /// Deletes tag `name` on the remote once `confirmed`; until then, asks.
    pub(super) fn delete_remote_tag(
        &mut self,
        name: String,
        confirmed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(remote) = self.tag_remote() else {
            return;
        };
        if !confirmed {
            let again = name.clone();
            self.notify_with_action(
                Notification::warning(format!(
                    "Delete tag {name} from {remote}? Everyone who fetches from {remote} loses \
                     it, and GitEZ cannot undo this."
                )),
                "Delete",
                move |this, window, cx| this.delete_remote_tag(again.clone(), true, window, cx),
                window,
                cx,
            );
            return;
        }
        let (on, tag) = (remote.clone(), name.clone());
        self.run_git(
            None,
            move |git| git.delete_remote_tag(&on, &tag),
            move |_, result, _, window, cx| {
                let notification = match result {
                    Ok(()) => Notification::success(format!("Deleted tag {name} from {remote}")),
                    Err(error) => Notification::error(format!(
                        "Could not delete tag {name} from {remote}: {error}"
                    )),
                };
                window.push_notification(notification, cx);
            },
            window,
            cx,
        );
    }
}
