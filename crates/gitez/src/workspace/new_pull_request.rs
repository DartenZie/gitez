//! The "New local PR" form: a title, the branch to review and the branch it
//! is reviewed against.

use gitez_git::Head;
use gitez_review::{NewPullRequest, Tip};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::repository::Snapshot;
use crate::theme::color;

/// Branches a pull request is usually reviewed against, in order of
/// preference.
const DEFAULT_BASES: [&str; 2] = ["main", "master"];

/// What the form asks of the window.
pub enum FormEvent {
    /// Enter was pressed in the title.
    Submit,
}

pub struct NewPullRequestForm {
    title: Entity<InputState>,
    head: Entity<SelectState<Vec<String>>>,
    base: Entity<SelectState<Vec<String>>>,
    /// Why the form cannot be submitted as filled in.
    error: Option<SharedString>,
    _title_events: Subscription,
}

impl EventEmitter<FormEvent> for NewPullRequestForm {}

impl NewPullRequestForm {
    /// A form for the branches of `snapshot`. The base starts on `main` or
    /// `master`, the head on the current branch, or else another branch.
    pub fn new(snapshot: &Snapshot, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let branches: Vec<String> = snapshot
            .refs
            .branches
            .iter()
            .map(|branch| branch.name.clone())
            .collect();
        let position = |name: &str| branches.iter().position(|branch| branch == name);
        let base = DEFAULT_BASES
            .iter()
            .find_map(|name| position(name))
            .or((!branches.is_empty()).then_some(0));
        let current = match &snapshot.refs.head {
            Some(Head::Branch { name, .. }) => position(name),
            _ => None,
        };
        let head = current
            .filter(|&head| Some(head) != base)
            .or_else(|| (0..branches.len()).find(|&index| Some(index) != base));

        let title = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Title (the branch name if left empty)")
        });
        let title_events = cx.subscribe_in(&title, window, |_, _, event, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                cx.emit(FormEvent::Submit);
            }
        });
        let select = |selected: Option<usize>, window: &mut Window, cx: &mut Context<Self>| {
            let branches = branches.clone();
            cx.new(|cx| {
                SelectState::new(branches, selected.map(IndexPath::new), window, cx)
                    .searchable(true)
            })
        };
        let head = select(head, window, cx);
        let base = select(base, window, cx);
        Self {
            title,
            head,
            base,
            error: None,
            _title_events: title_events,
        }
    }

    /// The title box, focused when the dialog opens.
    pub fn title(&self) -> &Entity<InputState> {
        &self.title
    }

    /// The pull request the form describes, with the branches' commits in
    /// `snapshot`; `None` after showing why when it is not filled in
    /// correctly.
    pub fn submit(
        &mut self,
        snapshot: &Snapshot,
        cx: &mut Context<Self>,
    ) -> Option<NewPullRequest> {
        let head = self.head.read(cx).selected_value().cloned();
        let base = self.base.read(cx).selected_value().cloned();
        let result = match (head, base) {
            (None, _) => Err("Choose the branch to review.".to_owned()),
            (_, None) => Err("Choose the branch to review it against.".to_owned()),
            (Some(head), Some(base)) if head == base => {
                Err("Choose two different branches.".to_owned())
            }
            (Some(head), Some(base)) => {
                match (snapshot.branch_tip(&head), snapshot.branch_tip(&base)) {
                    (Some(head_tip), Some(base_tip)) => {
                        let title = self.title.read(cx).value().trim().to_owned();
                        Ok(NewPullRequest {
                            title: if title.is_empty() {
                                head.clone()
                            } else {
                                title
                            },
                            description: String::new(),
                            base: Tip {
                                branch: base,
                                commit: base_tip.to_string(),
                            },
                            head: Tip {
                                branch: head,
                                commit: head_tip.to_string(),
                            },
                        })
                    }
                    _ => Err("That branch no longer exists.".to_owned()),
                }
            }
        };
        let (new, error) = match result {
            Ok(new) => (Some(new), None),
            Err(error) => (None, Some(error.into())),
        };
        self.error = error;
        cx.notify();
        new
    }
}

impl Render for NewPullRequestForm {
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
                "Branch to review",
                Select::new(&self.head)
                    .placeholder("Choose a branch")
                    .into_any_element(),
            ))
            .child(field(
                "Reviewed against",
                Select::new(&self.base)
                    .placeholder("Choose a branch")
                    .into_any_element(),
            ))
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
