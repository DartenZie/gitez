//! Editing a local pull request's title and description, and closing or
//! reopening it.

use gitez_review::{State, Timestamp};
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::*;
use gpui_kit::*;

use super::{Review, Subject};
use crate::theme::color;

/// What the form asks of the review.
pub(super) enum EditEvent {
    /// Enter in the title, or ⌘↵ in the description.
    Submit,
}

/// The "Edit pull request" form.
pub(super) struct EditForm {
    title: Entity<InputState>,
    description: Entity<TextareaState>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<EditEvent> for EditForm {}

impl EditForm {
    fn new(title: &str, description: &str, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let title_input = cx.new(|cx| {
            let mut input =
                InputState::new(window, cx).placeholder("Title (the branch name if left empty)");
            input.set_value(title.to_owned(), window, cx);
            input
        });
        let description_input = cx.new(|cx| {
            let mut input = TextareaState::new(window, cx)
                .auto_grow(4, 14)
                .placeholder("What the branch does and why (optional)");
            input.set_value(description.to_owned(), window, cx);
            input
        });
        let subscriptions = vec![
            cx.subscribe_in(&title_input, window, |_, _, event, _, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    cx.emit(EditEvent::Submit);
                }
            }),
            cx.subscribe_in(&description_input, window, |_, _, event, _, cx| {
                if let InputEvent::PressEnter {
                    secondary: true, ..
                } = event
                {
                    cx.emit(EditEvent::Submit);
                }
            }),
        ];
        Self {
            title: title_input,
            description: description_input,
            _subscriptions: subscriptions,
        }
    }

    /// The title and description as filled in, trimmed.
    fn values(&self, cx: &App) -> (String, String) {
        (
            self.title.read(cx).value().trim().to_owned(),
            self.description.read(cx).value().trim().to_owned(),
        )
    }
}

impl Render for EditForm {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let field = |label: &'static str, input: AnyElement| {
            div()
                .v_flex()
                .gap(px(6.))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(color::TEXT_MUTED))
                        .child(label),
                )
                .child(input)
        };
        div()
            .v_flex()
            .gap(px(14.))
            .pb(px(4.))
            .child(field("Title", Input::new(&self.title).into_any_element()))
            .child(field(
                "Description",
                Textarea::new(&self.description).into_any_element(),
            ))
    }
}

impl Review {
    /// Opens the dialog to edit the pull request's title and description.
    pub(super) fn open_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Subject::PullRequest(subject) = &self.subject else {
            return;
        };
        let pull_request = &subject.pull_request;
        let (title, description) = (pull_request.title.clone(), pull_request.description.clone());
        let form = cx.new(|cx| EditForm::new(&title, &description, window, cx));
        let subscription = cx.subscribe_in(&form, window, |this, form, event, window, cx| {
            let EditEvent::Submit = event;
            this.save_edit(form.clone(), window, cx);
        });
        if let Subject::PullRequest(subject) = &mut self.subject {
            subject.edit_dialog = Some(subscription);
        }
        let review = cx.entity().downgrade();
        let focus_title = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if focus_title.replace(false) {
                let title = form.read(cx).title.focus_handle(cx);
                window.focus(&title, cx);
            }
            let (save, form_to_save) = (review.clone(), form.clone());
            dialog
                .title("Edit local pull request")
                .w(px(520.))
                .child(form.clone())
                .footer(
                    div()
                        .h_flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            Button::new("edit-pull-request-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("edit-pull-request-save")
                                .primary()
                                .label("Save")
                                .on_click(move |_, window, cx| {
                                    let form = form_to_save.clone();
                                    save.update(cx, |this, cx| this.save_edit(form, window, cx))
                                        .ok();
                                }),
                        ),
                )
                .on_close({
                    let review = review.clone();
                    move |_, _, cx| {
                        review
                            .update(cx, |this, _| {
                                if let Subject::PullRequest(subject) = &mut this.subject {
                                    subject.edit_dialog = None;
                                }
                            })
                            .ok();
                    }
                })
        });
    }

    /// Saves the title and description from `form` and closes the dialog.
    fn save_edit(&mut self, form: Entity<EditForm>, window: &mut Window, cx: &mut Context<Self>) {
        let (title, description) = form.read(cx).values(cx);
        let Subject::PullRequest(subject) = &mut self.subject else {
            return;
        };
        let pull_request = &mut subject.pull_request;
        let title = if title.is_empty() {
            pull_request.head.branch.clone()
        } else {
            title
        };
        if title != pull_request.title || description != pull_request.description {
            pull_request.title = title;
            pull_request.description = description;
            pull_request.updated = Timestamp::now();
            self.save_pull_request(cx);
        }
        window.close_dialog(cx);
        cx.notify();
    }

    /// Closes the pull request, or reopens it when `open`. A closed pull
    /// request keeps the changes it had; a reopened one follows its
    /// branches again.
    pub(super) fn set_open(&mut self, open: bool, cx: &mut Context<Self>) {
        let Subject::PullRequest(subject) = &mut self.subject else {
            return;
        };
        let pull_request = &mut subject.pull_request;
        let state = if open { State::Open } else { State::Closed };
        if pull_request.state == state || pull_request.state == State::Merged {
            return;
        }
        pull_request.state = state;
        pull_request.updated = Timestamp::now();
        self.save_pull_request(cx);
        if open {
            // Read again on the branches as they are now, once saved.
            self.after_saving(cx, |this, cx| {
                let Subject::PullRequest(subject) = &this.subject else {
                    return;
                };
                let (number, tips) = (subject.pull_request.number, subject.tips);
                let store = subject.store.clone();
                this._load = Self::load_pull_request(&this.repository, &store, number, tips, cx);
            });
        }
        cx.notify();
    }

    /// Runs `then` once the saves started so far are done.
    fn after_saving(
        &mut self,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        let Subject::PullRequest(subject) = &mut self.subject else {
            return;
        };
        let previous = std::mem::replace(&mut subject.saving, Task::ready(()));
        subject.saving = cx.spawn(async move |this, cx| {
            previous.await;
            this.update(cx, then).ok();
        });
    }
}
