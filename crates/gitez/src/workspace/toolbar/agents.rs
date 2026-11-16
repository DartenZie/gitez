//! The toolbar's Agents indicator and its popover, as in the
//! `C-2-Sessions` board: how many agent threads run and how many wait for
//! the user, and, on a click, the threads of the open repository (or of
//! every repository, in Home) grouped by what they need, each with the
//! action that fits it.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use gitez_agent::Registry;
use gitez_agent_thread::{AgentThread, Ending, State, Timestamp};
use gitez_sandbox::NetworkProfile;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::popover::{Popover, PopoverState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::agent_threads::{
    AgentThreadKey, AgentThreads, Counts, Notice, OpenAgentPullRequest, OpenAgentThread,
    OpenSettings, ThreadFacts,
};
use crate::motion;
use crate::theme::{self, color};
use crate::workspace::ShowHome;
use crate::workspace::agent_thread::{
    Look, ago, blocked_hosts, can_complete, completed_label, duration, finished_at, state_icon,
    state_label, suggested_retry, uncommitted_text,
};

/// Height of the indicator, as the toolbar's other buttons.
const BUTTON_HEIGHT: Pixels = px(30.);
/// Width of the popover.
const POPOVER_WIDTH: Pixels = px(360.);
/// How tall the list of threads grows before it scrolls.
const LIST_MAX_HEIGHT: Pixels = px(600.);
/// Height of the buttons in a row.
const ROW_BUTTON_HEIGHT: Pixels = px(26.);
/// Completed threads listed; the rest are in Home.
const COMPLETED_SHOWN: usize = 3;

/// Whose agent threads the indicator counts and lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentsScope {
    /// The repository's git common dir, which its threads are kept in.
    pub common_dir: PathBuf,
    /// Its name, as the toolbar shows it.
    pub name: SharedString,
}

/// One part of the indicator's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Part {
    Active,
    Waiting,
    Ready,
    Failed,
    Stopped,
}

impl Part {
    /// In the colour of the state it counts, as rows show it.
    pub(crate) fn color(self) -> u32 {
        match self {
            Part::Active => Look::Running.text(),
            Part::Waiting => Look::Waiting.text(),
            Part::Ready => Look::Ready.text(),
            Part::Failed => Look::Failed.text(),
            Part::Stopped => Look::Stopped.text(),
        }
    }
}

/// What the indicator (and Home's heading) says of `counts`: "2 active",
/// "1 waiting", "1 ready", "1 failed", "1 stopped", only the parts that
/// are not zero.
pub(crate) fn indicator_parts(counts: Counts) -> Vec<(Part, String)> {
    [
        (Part::Active, counts.active, "active"),
        (Part::Waiting, counts.waiting, "waiting"),
        (Part::Ready, counts.ready, "ready"),
        (Part::Failed, counts.failed, "failed"),
        (Part::Stopped, counts.stopped, "stopped"),
    ]
    .into_iter()
    .filter(|(_, count, _)| *count > 0)
    .map(|(part, count, word)| (part, format!("{count} {word}")))
    .collect()
}

/// The indicator as one line, for its tooltip and screen readers:
/// "Agents: 2 active, 1 ready", or "Agents" when nothing is to say.
pub(crate) fn indicator_label(counts: Counts) -> String {
    let parts: Vec<String> = indicator_parts(counts)
        .into_iter()
        .map(|(_, text)| text)
        .collect();
    if parts.is_empty() {
        "Agents".to_owned()
    } else {
        format!("Agents: {}", parts.join(", "))
    }
}

/// A group of the popover, in the order they are listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Group {
    /// Failed or stopped: retry or complete.
    NeedsAttention,
    /// Finished with a pull request or an answer: review, read, complete.
    ReadyForReview,
    /// Preparing or running.
    Active,
    /// Queued, or waiting for the usage limit.
    Waiting,
    /// Done with; listed last and dimmed.
    Completed,
}

impl Group {
    fn title(self) -> &'static str {
        match self {
            Group::NeedsAttention => "Needs attention",
            Group::ReadyForReview => "Ready for review",
            Group::Active => "Active",
            Group::Waiting => "Waiting",
            Group::Completed => "Completed",
        }
    }
}

/// The group `thread` is listed in.
pub(crate) fn group_of(thread: &AgentThread) -> Group {
    match thread.state {
        State::Preparing | State::Running => Group::Active,
        State::Queued | State::Waiting => Group::Waiting,
        State::Completed => Group::Completed,
        State::Finished => match thread.ending {
            Some(Ending::Failed | Ending::Stopped) => Group::NeedsAttention,
            Some(Ending::PullRequest | Ending::Answer) | None => Group::ReadyForReview,
        },
    }
}

/// A thread in the popover, with its repository.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Item {
    pub common_dir: PathBuf,
    /// Shown on the row only when the popover lists every repository.
    pub repository: SharedString,
    pub thread: Arc<AgentThread>,
    /// What else is known of it.
    pub facts: ThreadFacts,
}

impl Item {
    pub(crate) fn key(&self) -> AgentThreadKey {
        AgentThreadKey {
            common_dir: self.common_dir.clone(),
            id: self.thread.id.clone(),
        }
    }
}

/// `items` in their groups, the groups in order and only those with
/// threads: what needs the user and what just finished newest first,
/// what runs and waits in the order it started, completed newest first.
pub(crate) fn grouped(items: Vec<Item>) -> Vec<(Group, Vec<Item>)> {
    let mut groups: Vec<(Group, Vec<Item>)> = Vec::new();
    for item in items {
        let group = group_of(&item.thread);
        match groups.iter_mut().find(|(known, _)| *known == group) {
            Some((_, items)) => items.push(item),
            None => groups.push((group, vec![item])),
        }
    }
    groups.sort_by_key(|(group, _)| *group);
    for (group, items) in &mut groups {
        match group {
            Group::NeedsAttention | Group::ReadyForReview => {
                items.sort_by_key(|item| std::cmp::Reverse(finished_at(&item.thread)))
            }
            Group::Active | Group::Waiting => items.sort_by_key(|item| item.thread.created),
            Group::Completed => items.sort_by_key(|item| {
                std::cmp::Reverse(item.thread.completed.unwrap_or(item.thread.updated))
            }),
        }
    }
    groups
}

/// What a row offers besides opening its thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowAction {
    /// Ends a running agent's turn.
    Stop,
    /// Takes a queued thread out of the queue.
    Cancel,
    /// Runs a failed or stopped thread again, with this network.
    Retry(NetworkProfile),
    /// Removes the worktree of a finished thread without a pull request.
    Complete,
    /// Opens the thread's local pull request.
    PullRequest(u32),
}

/// The actions a row of `thread` offers, in the order shown.
pub(crate) fn row_actions(
    thread: &AgentThread,
    agents: &Registry,
    package_hosts: &[String],
) -> Vec<RowAction> {
    let mut actions = Vec::new();
    match thread.state {
        State::Queued => actions.push(RowAction::Cancel),
        State::Preparing | State::Running | State::Waiting => actions.push(RowAction::Stop),
        State::Finished => {
            if matches!(thread.ending, Some(Ending::Failed | Ending::Stopped)) {
                actions.push(RowAction::Retry(suggested_retry(
                    thread,
                    agents,
                    package_hosts,
                )));
            }
            if let Some(number) = thread.pull_request {
                actions.push(RowAction::PullRequest(number));
            }
            if can_complete(thread) {
                actions.push(RowAction::Complete);
            }
        }
        State::Completed => actions.extend(thread.pull_request.map(RowAction::PullRequest)),
    }
    actions
}

impl RowAction {
    /// The button's label for a thread whose network is `network`.
    pub(crate) fn label(self, network: NetworkProfile) -> String {
        match self {
            RowAction::Stop => "Stop".to_owned(),
            RowAction::Cancel => "Cancel".to_owned(),
            RowAction::Retry(profile) if profile != network => {
                format!("Retry with {}", profile.label())
            }
            RowAction::Retry(_) => "Retry".to_owned(),
            RowAction::Complete => "Complete".to_owned(),
            RowAction::PullRequest(_) => "Review".to_owned(),
        }
    }
}

/// What Complete does to `thread`, for its tooltip.
pub(crate) fn complete_tooltip(thread: &AgentThread) -> &'static str {
    if thread.own().is_none() {
        "Ends the discussion. The thread stays readable; nothing in your working tree changes."
    } else if thread.has_uncommitted() == Some(true) {
        "Asks first: the worktree has uncommitted changes, which removing it discards"
    } else {
        "Removes the worktree, and the branch if it has no commits. The thread stays readable."
    }
}

/// What Retry with `network` does to `thread`, for its tooltip: run the
/// first prompt again, continue the conversation, or only commit and
/// create the pull request; and what a wider network allows.
pub(crate) fn retry_tooltip(thread: &AgentThread, network: NetworkProfile) -> String {
    let plan = thread.clone().request_retry(network, Timestamp::now());
    let mut text = match plan {
        Ok(gitez_agent_thread::RetryPlan::RunAgain) => {
            "Run the first prompt again in a new conversation (the agent never started)"
        }
        Ok(gitez_agent_thread::RetryPlan::Finish) => {
            "Commit and create the pull request again; the agent does not run"
        }
        Ok(gitez_agent_thread::RetryPlan::Continue) | Err(_) => {
            "Run the thread again, continuing its conversation"
        }
    }
    .to_owned();
    if let Some(warning) = wider_network_warning(thread.network, network) {
        text.push_str(". ");
        text.push_str(warning);
    }
    text
}

/// The known limit of a wider network, when a retry moves `from` to `to`
/// and `to` is Packages or Open.
pub(crate) fn wider_network_warning(
    from: NetworkProfile,
    to: NetworkProfile,
) -> Option<&'static str> {
    (from != to && to != NetworkProfile::AgentOnly).then_some(
        "With Packages or Open the sandbox cannot separate the agent from the commands it runs, \
         so the agent could send out whatever it can read, including its own login.",
    )
}

/// A piece of a row's second line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Span {
    pub text: String,
    pub color: u32,
    pub mono: bool,
}

impl Span {
    fn new(text: impl Into<String>, color: u32) -> Self {
        Self {
            text: text.into(),
            color,
            mono: false,
        }
    }

    fn mono(text: impl Into<String>, color: u32) -> Self {
        Self {
            text: text.into(),
            color,
            mono: true,
        }
    }
}

/// A row's second line: its state in the state's colour, then when and
/// what, as "Running · Bash cargo test", "PR #12 · finished 1 h ago",
/// "Failed after 6 min · 40 min ago · static.crates.io blocked by Agent API
/// only" (the hosts `thread`'s agent does not need are left out).
/// `repository` leads it when the popover lists every repository; `limit`
/// is how many agents may run at once.
pub(crate) fn status_line(
    thread: &AgentThread,
    agents: &Registry,
    facts: &ThreadFacts,
    repository: Option<&str>,
    limit: usize,
    now: Timestamp,
) -> Vec<Span> {
    let muted = color::TEXT_MUTED;
    let mut spans = Vec::new();
    if let Some(repository) = repository {
        spans.push(Span::new(repository, color::TEXT_SECONDARY));
        spans.push(Span::new(" · ", muted));
    }
    let run = thread.run_time(now).as_secs();
    let after = |what: &str| {
        if run >= 60 {
            format!("{what} after {}", duration(run))
        } else {
            what.to_owned()
        }
    };
    match thread.state {
        State::Queued => {
            spans.push(Span::new("Queued", Look::Queued.text()));
            let agents = if limit == 1 {
                "the agent is".to_owned()
            } else {
                format!("one of the {limit} agents is")
            };
            spans.push(Span::new(format!(" · starts when {agents} free"), muted));
        }
        State::Waiting => {
            spans.push(Span::new(state_label(thread), Look::Waiting.text()));
            spans.push(Span::new(" · usage limit", muted));
        }
        State::Preparing | State::Running => {
            let preparing = thread.state == State::Preparing;
            let look = Look::of(thread);
            spans.push(Span::new(look.word(), look.text()));
            let (tool, detail) = match &thread.activity {
                Some(activity) => (activity.tool.clone(), activity.detail.clone()),
                None if preparing => (None, "creating the worktree".to_owned()),
                None => (None, "starting the agent".to_owned()),
            };
            spans.push(Span::new(" · ", muted));
            if let Some(tool) = tool.filter(|tool| tool != "Setup") {
                spans.push(Span::new(format!("{tool} "), muted));
                spans.push(Span::mono(detail, muted));
            } else {
                spans.push(Span::new(detail, muted));
            }
        }
        State::Finished => {
            let when = ago(finished_at(thread), now);
            match thread.ending {
                Some(Ending::Failed) => {
                    spans.push(Span::new(after("Failed"), Look::Failed.text()));
                    spans.push(Span::new(format!(" · {when}"), muted));
                    // Only hosts the run may have needed: the agent's
                    // optional ones are no reason for the failure.
                    let blocked = blocked_hosts(thread, agents);
                    if let Some(host) = blocked.problems.first() {
                        let more = blocked.problems.len() - 1;
                        spans.push(Span::new(" · ", muted));
                        spans.push(Span::mono(*host, muted));
                        let more = if more > 0 {
                            format!(" and {more} more")
                        } else {
                            String::new()
                        };
                        spans.push(Span::new(
                            format!("{more} blocked by {}", thread.network.label()),
                            muted,
                        ));
                    }
                }
                Some(Ending::Stopped) => {
                    spans.push(Span::new(after("Stopped"), Look::Stopped.text()));
                    spans.push(Span::new(format!(" · {when}"), muted));
                }
                Some(Ending::PullRequest) | Some(Ending::Answer) | None => {
                    let look = Look::of(thread);
                    match (thread.pull_request, look) {
                        (Some(number), _) => {
                            spans.push(Span::new(format!("PR #{number}"), look.text()))
                        }
                        (None, Look::Uncommitted) => {
                            let text = uncommitted_text(thread).unwrap_or_default();
                            spans.push(Span::new(text, look.text()));
                        }
                        (None, _) => spans.push(Span::new(look.word(), look.text())),
                    }
                    spans.push(Span::new(format!(" · finished {when}"), muted));
                }
            }
            // Left in the worktree: said on every row, never hidden behind
            // the state.
            if Look::of(thread) != Look::Uncommitted
                && let Some(text) = uncommitted_text(thread)
            {
                spans.insert(
                    spans.len().min(repository.map_or(1, |_| 3)),
                    Span::new(format!(" · {text}"), Look::Uncommitted.text()),
                );
            }
        }
        State::Completed => {
            let when = ago(thread.completed.unwrap_or(thread.updated), now);
            spans.push(Span::new(
                completed_label(thread, facts.pull_request),
                Look::Completed.text(),
            ));
            if facts.worktree_present {
                let kept = match uncommitted_text(thread) {
                    Some(text) => format!(" · worktree kept: {text}"),
                    None => " · worktree kept".to_owned(),
                };
                spans.push(Span::new(kept, Look::Uncommitted.text()));
            }
            spans.push(Span::new(format!(" · {when}"), Look::Completed.text()));
        }
    }
    spans
}

/// How long an active thread has run: "just now", "14 min", as the lanes
/// say it.
pub(crate) fn elapsed(thread: &AgentThread, now: Timestamp) -> String {
    let seconds = thread.run_time(now).as_secs();
    if seconds < 60 {
        "just now".to_owned()
    } else {
        duration(seconds)
    }
}

/// The threads of the popover's scope, and the other repositories with
/// threads running, with how many, for its footer.
fn items(
    scope: Option<&AgentsScope>,
    threads: &AgentThreads,
) -> (Vec<Item>, Vec<(SharedString, usize)>) {
    let mut items = Vec::new();
    let mut elsewhere = Vec::new();
    for repository in threads.repositories() {
        let inside = scope.is_none_or(|scope| {
            threads
                .repository(&scope.common_dir)
                .is_some_and(|known| known.common_dir == repository.common_dir)
        });
        if !inside {
            let active = Counts::of(repository.threads.iter().map(Arc::as_ref)).active;
            if active > 0 {
                elsewhere.push((repository.name.clone(), active));
            }
            continue;
        }
        items.extend(repository.threads.iter().map(|thread| {
            Item {
                common_dir: repository.common_dir.clone(),
                repository: repository.name.clone(),
                thread: thread.clone(),
                facts: repository
                    .facts
                    .get(&thread.id)
                    .cloned()
                    .unwrap_or_default(),
            }
        }));
    }
    (items, elsewhere)
}

/// The threads an action runs for now, so their buttons wait.
#[derive(Default)]
pub(crate) struct Busy(HashSet<AgentThreadKey>);

/// The Agents button and, on a click, the popover of agent threads. With
/// `scope` it counts and lists that repository's threads; with `None`
/// every repository's (Home). Its actions go to `focus`, the view that
/// handles [`OpenAgentThread`], [`OpenAgentPullRequest`] and [`ShowHome`].
pub(crate) fn agents_indicator(
    scope: Option<AgentsScope>,
    focus: FocusHandle,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let busy = window.use_keyed_state("agents-busy", cx, |_, _| Busy::default());
    let model = AgentThreads::global(cx);
    let counts = model
        .read(cx)
        .counts(scope.as_ref().map(|scope| scope.common_dir.as_path()));
    let parts = indicator_parts(counts);
    let label = indicator_label(counts);
    let trigger = Button::new("agents-indicator")
        .debug_selector(|| "agents-indicator".into())
        .h(BUTTON_HEIGHT)
        .gap(px(8.))
        .accessibility_label(label.clone())
        .tooltip(label)
        .icon(
            Icon::new(IconName::Terminal)
                .size(px(14.))
                .text_color(rgb(color::ACCENT)),
        )
        .label("Agents")
        .children(parts.into_iter().map(|(part, text)| {
            div()
                .h_flex()
                .gap(px(5.))
                .text_size(px(12.))
                .text_color(rgb(part.color()))
                .when(part == Part::Active, |this| {
                    this.child(motion::blinking_dot(px(6.), color::BRANCH_BLUE))
                })
                .child(text)
        }));
    Popover::new("agents-popover")
        .anchor(Anchor::TopRight)
        .offset(px(8.))
        .trigger(trigger)
        .w(POPOVER_WIDTH)
        .p(px(0.))
        .bg(rgb(color::PALETTE))
        .border_color(rgb(color::PALETTE_BORDER))
        .rounded(px(12.))
        .content(move |_, window, cx| render_popover(scope.as_ref(), &focus, &busy, window, cx))
}

/// The popover's content: heading, groups of rows, footer.
fn render_popover(
    scope: Option<&AgentsScope>,
    focus: &FocusHandle,
    busy: &Entity<Busy>,
    _window: &mut Window,
    cx: &mut Context<PopoverState>,
) -> Div {
    let popover = cx.entity().downgrade();
    let model = AgentThreads::global(cx);
    let (items, elsewhere) = items(scope, model.read(cx));
    let limit = model.read(cx).settings().threads.max_running as usize;
    let running = model.read(cx).running();
    let now = Timestamp::now();
    let groups = grouped(items);
    let notices = model
        .read(cx)
        .notices(scope.map(|scope| scope.common_dir.as_path()));

    let gear = Button::new("agents-settings")
        .ghost()
        .size(ROW_BUTTON_HEIGHT)
        .ml_auto()
        .text_color(rgb(color::TEXT_SECONDARY))
        .icon(Icon::new(IconName::Settings).size(px(14.)))
        .accessibility_label("Agents, sandbox and secrets settings")
        .tooltip("Agents, sandbox and secrets settings")
        .on_click({
            let (focus, popover) = (focus.clone(), popover.clone());
            move |_, window, cx| {
                dismiss(&popover, window, cx);
                focus.dispatch_action(&OpenSettings, window, cx);
            }
        });
    let heading = div()
        .h_flex()
        .gap(px(8.))
        .pl(px(16.))
        .pr(px(12.))
        .pt(px(12.))
        .pb(px(8.))
        .border_b_1()
        .border_color(rgb(color::SEPARATOR))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child("Agent threads"),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_MUTED))
                .child(match scope {
                    Some(scope) => format!("in {}", scope.name),
                    None => "in all repositories".to_owned(),
                }),
        )
        .child(gear);

    let mut list = div()
        .id("agents-popover-list")
        .v_flex()
        .max_h(LIST_MAX_HEIGHT)
        .overflow_y_scroll()
        .pb(px(4.));
    if !notices.is_empty() {
        list = list.child(render_notices(notices, "agents-notice", cx));
    }
    if groups.is_empty() {
        list = list.child(empty(scope));
    }
    let busy_now = busy.read(cx).0.clone();
    for (group, items) in groups {
        let total = items.len();
        let shown = if group == Group::Completed {
            COMPLETED_SHOWN
        } else {
            total
        };
        list = list.child(group_heading(group));
        for item in items.into_iter().take(shown) {
            let key = item.key();
            let row = render_row(
                &item,
                scope.is_none(),
                limit,
                now,
                busy_now.contains(&key),
                RowContext {
                    focus: focus.clone(),
                    popover: popover.clone(),
                    busy: busy.downgrade(),
                },
                cx,
            );
            list = list.child(row);
        }
        if total > shown {
            list = list.child(
                div()
                    .px(px(16.))
                    .py(px(4.))
                    .text_size(px(11.5))
                    .text_color(rgb(color::TEXT_FAINT))
                    .child(format!("{} more in All threads", total - shown)),
            );
        }
    }

    let mut running_line = format!("{running} of {limit} agents running");
    if scope.is_some() {
        match elsewhere.as_slice() {
            [] => {}
            [(name, count)] => running_line.push_str(&format!(" · {count} in {name}")),
            more => {
                let count: usize = more.iter().map(|(_, count)| count).sum();
                running_line.push_str(&format!(" · {count} in other repositories"));
            }
        }
    }
    let footer = div()
        .h_flex()
        .gap(px(8.))
        .mt(px(4.))
        .pl(px(16.))
        .pr(px(12.))
        .pt(px(10.))
        .pb(px(10.))
        .border_t_1()
        .border_color(rgb(color::SEPARATOR))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(11.5))
                .text_color(rgb(color::TEXT_MUTED))
                .child(running_line),
        )
        .when(scope.is_some(), |this| {
            let (focus, popover) = (focus.clone(), popover.clone());
            this.child(
                Button::new("agents-all-threads")
                    .debug_selector(|| "agents-all-threads".into())
                    .h(px(28.))
                    .compact()
                    .icon(Icon::new(IconName::House).size(px(13.)))
                    .label("All threads")
                    .tooltip("Agent threads of every repository, in Home")
                    .on_click(move |_, window, cx| {
                        dismiss(&popover, window, cx);
                        focus.dispatch_action(&ShowHome, window, cx);
                    }),
            )
        });

    div()
        .v_flex()
        .w_full()
        .text_size(theme::TEXT_SIZE)
        .text_color(rgb(color::TEXT))
        .child(heading)
        .child(list)
        .child(footer)
}

/// What could not be read, watched or completed, each with its reason,
/// and for a repository that was moved or deleted a button to stop listing
/// it. Shared with Home; `id` prefixes the buttons' ids.
pub(crate) fn render_notices(notices: Vec<Notice>, id: &'static str, cx: &App) -> Div {
    let model = AgentThreads::global(cx).downgrade();
    div()
        .v_flex()
        .gap(px(6.))
        .mx(px(8.))
        .mt(px(8.))
        .children(notices.into_iter().enumerate().map(|(index, notice)| {
            let forget = notice.forget.clone().map(|common_dir| {
                let model = model.clone();
                Button::new((id, index))
                    .debug_selector(move || format!("{id}-forget-{index}"))
                    .compact()
                    .h(px(22.))
                    .text_size(px(11.5))
                    .label("Remove from list")
                    .tooltip("Stop listing this repository; its threads stay in its git folder")
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        model
                            .update(cx, |model, cx| {
                                model.forget_repository(common_dir.clone(), cx)
                            })
                            .ok();
                    })
            });
            div()
                .h_flex()
                .items_start()
                .gap(px(8.))
                .px(px(10.))
                .py(px(7.))
                .rounded(px(8.))
                .border_1()
                .border_color(rgb(color::WARNING_BORDER))
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .size(px(13.))
                        .mt(px(2.))
                        .text_color(rgb(color::ACCENT)),
                )
                .child(
                    div()
                        .v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(6.))
                        .child(
                            div()
                                .text_size(px(11.5))
                                .line_height(relative(1.45))
                                .text_color(rgb(color::WARNING_TEXT))
                                .child(notice.text.clone()),
                        )
                        .children(forget),
                )
        }))
}

/// What a row's handlers need.
struct RowContext {
    focus: FocusHandle,
    popover: WeakEntity<PopoverState>,
    busy: WeakEntity<Busy>,
}

fn dismiss(popover: &WeakEntity<PopoverState>, window: &mut Window, cx: &mut App) {
    popover
        .update(cx, |popover, cx| popover.dismiss(window, cx))
        .ok();
}

fn group_heading(group: Group) -> Div {
    div()
        .pt(px(10.))
        .pb(px(4.))
        .px(px(16.))
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(color::TEXT_FAINT))
        .child(group.title().to_uppercase())
}

/// What the popover says with no threads.
fn empty(scope: Option<&AgentsScope>) -> Div {
    let title = match scope {
        Some(scope) => format!("No agent threads in {} yet", scope.name),
        None => "No agent threads yet".to_owned(),
    };
    let hint = match scope {
        Some(_) => {
            "Ask an agent from the row under the uncommitted changes in the history, \
                    or press ⌘⇧A. Discuss with it, or let it implement on its own branch in \
                    its own worktree."
        }
        None => {
            "Start one from the launcher. Discuss with an agent, or let it implement on its \
                 own branch in its own worktree."
        }
    };
    div()
        .v_flex()
        .gap(px(6.))
        .px(px(16.))
        .py(px(16.))
        .child(div().font_weight(FontWeight::MEDIUM).child(title))
        .child(
            div()
                .text_size(px(12.))
                .line_height(relative(1.5))
                .text_color(rgb(color::TEXT_MUTED))
                .child(hint),
        )
}

/// The second line of a row, cut with an ellipsis. GPUI measures a line
/// for its ellipsis in one font, so the line is one text in the UI font,
/// and only a monospaced span at its end gets an element (and an
/// ellipsis) of its own; a monospaced span inside the line keeps the UI
/// font.
pub(crate) fn status_line_element(mut spans: Vec<Span>, cx: &App) -> Div {
    let ellipsis = || {
        div()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
    };
    let tail = spans
        .last()
        .is_some_and(|span| span.mono && spans.len() > 1)
        .then(|| spans.pop())
        .flatten();
    let mut text = String::new();
    let mut highlights = Vec::new();
    for span in spans {
        let start = text.len();
        text.push_str(&span.text);
        highlights.push((
            start..text.len(),
            HighlightStyle {
                color: Some(rgb(span.color).into()),
                ..HighlightStyle::default()
            },
        ));
    }
    let head = StyledText::new(text).with_highlights(highlights);
    let line = div().h_flex().min_w_0().text_size(px(11.5));
    match tail {
        None => line.child(ellipsis().flex_1().child(head)),
        Some(tail) => line
            .child(ellipsis().flex_shrink_0().max_w(relative(0.7)).child(head))
            .child(
                ellipsis()
                    .flex_1()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(px(11.))
                    .text_color(rgb(tail.color))
                    .child(tail.text),
            ),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_row(
    item: &Item,
    all_repositories: bool,
    limit: usize,
    now: Timestamp,
    busy: bool,
    context: RowContext,
    cx: &mut Context<PopoverState>,
) -> Stateful<Div> {
    let thread = &item.thread;
    let key = item.key();
    let completed = thread.state == State::Completed;
    let model = AgentThreads::global(cx).read(cx);
    let actions = row_actions(
        thread,
        model.agents(),
        &model.settings().sandbox.package_hosts,
    );
    let repository = all_repositories.then_some(item.repository.as_ref());
    let spans = status_line(thread, model.agents(), &item.facts, repository, limit, now);
    let failed = group_of(thread) == Group::NeedsAttention;
    let id = SharedString::from(format!("agents-row-{}", thread.id.as_str()));

    let buttons: Vec<Button> = actions
        .iter()
        .enumerate()
        .map(|(index, &action)| {
            row_button(
                action, index, thread, &key, busy, completed, &context, failed,
            )
        })
        .collect();

    let title = div()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .text_size(px(12.5))
        .map(|this| {
            if completed {
                this.text_color(rgb(color::TEXT_MUTED))
            } else {
                this.font_weight(FontWeight::MEDIUM)
            }
        })
        .child(thread.title.clone());
    let line = status_line_element(spans, cx);
    let text = div()
        .v_flex()
        .flex_1()
        .min_w_0()
        .gap(px(2.))
        .child(title)
        .child(line);
    let active = matches!(thread.state, State::Preparing | State::Running);
    let trailing = div().h_flex().flex_none().gap(px(6.)).when(active, |this| {
        this.child(
            div()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(px(11.5))
                .text_color(rgb(color::TEXT_SECONDARY))
                .child(elapsed(thread, now)),
        )
    });
    let (body, trailing) = if failed {
        // Retry and Complete go under the text, as on the board.
        (
            div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(6.))
                .child(text)
                .child(div().h_flex().gap(px(6.)).children(buttons)),
            trailing,
        )
    } else {
        (text, trailing.children(buttons))
    };

    let open = {
        let (focus, popover) = (context.focus.clone(), context.popover.clone());
        let action = OpenAgentThread {
            common_dir: key.common_dir.clone(),
            id: key.id.clone(),
        };
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            dismiss(&popover, window, cx);
            focus.dispatch_action(&action, window, cx);
        }
    };
    div()
        .id(id.clone())
        .debug_selector(move || id.to_string())
        .h_flex()
        .items_start()
        .gap(px(10.))
        .mx(px(4.))
        .px(px(12.))
        .py(px(7.))
        .rounded(px(8.))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(color::PALETTE_HOVER)))
        .on_click(open)
        .child(state_icon(thread, item.facts.pull_request, false))
        .child(body)
        .child(trailing)
}

/// The button of `action` on the row of `thread`.
#[allow(clippy::too_many_arguments)]
fn row_button(
    action: RowAction,
    index: usize,
    thread: &AgentThread,
    key: &AgentThreadKey,
    busy: bool,
    completed: bool,
    context: &RowContext,
    failed: bool,
) -> Button {
    let id = SharedString::from(format!("agents-action-{}-{index}", thread.id.as_str()));
    let selector = format!(
        "agents-action-{}-{}",
        thread.id.as_str(),
        action_name(action)
    );
    let button = Button::new(id)
        .debug_selector(move || selector.clone())
        .h(ROW_BUTTON_HEIGHT)
        .compact()
        .text_size(px(12.))
        .label(action.label(thread.network))
        .disabled(busy);
    let button = match action {
        RowAction::Retry(network) => button
            .primary()
            .icon(Icon::new(IconName::RotateCcw).size(px(12.)))
            .loading(busy)
            .tooltip(retry_tooltip(thread, network)),
        RowAction::Stop | RowAction::Cancel => button
            .ghost()
            .text_color(rgb(color::THREAD_FAILED_TEXT))
            .loading(busy)
            .tooltip(if action == RowAction::Stop {
                "End the agent's turn; the thread stays and can be retried"
            } else {
                "Take the thread out of the queue"
            }),
        RowAction::Complete => button
            .when(!failed, |this| this.ghost())
            .icon(Icon::new(IconName::Check).size(px(12.)))
            .loading(busy)
            .tooltip(complete_tooltip(thread)),
        RowAction::PullRequest(number) => button
            .when(completed, |this| {
                this.ghost().label(format!("PR #{number}"))
            })
            .icon(Icon::new(IconName::GitPullRequestArrow).size(px(12.)))
            .tooltip(format!("Open local pull request #{number}")),
    };
    let (key, focus, popover, busy) = (
        key.clone(),
        context.focus.clone(),
        context.popover.clone(),
        context.busy.clone(),
    );
    button.on_click(move |_, window, cx| {
        cx.stop_propagation();
        run(action, &key, &focus, &popover, &busy, window, cx);
    })
}

/// A stable name of `action`, for tests to find its button.
fn action_name(action: RowAction) -> &'static str {
    match action {
        RowAction::Stop => "stop",
        RowAction::Cancel => "cancel",
        RowAction::Retry(_) => "retry",
        RowAction::Complete => "complete",
        RowAction::PullRequest(_) => "pull-request",
    }
}

/// Does `action` for thread `key`: opens its pull request, or asks the
/// model to stop, retry or complete it, marking the row busy until that
/// ends and showing its error if it fails.
fn run(
    action: RowAction,
    key: &AgentThreadKey,
    focus: &FocusHandle,
    popover: &WeakEntity<PopoverState>,
    busy: &WeakEntity<Busy>,
    window: &mut Window,
    cx: &mut App,
) {
    let model = AgentThreads::global(cx);
    let task = match action {
        RowAction::PullRequest(number) => {
            dismiss(popover, window, cx);
            let action = OpenAgentPullRequest {
                common_dir: key.common_dir.clone(),
                number,
            };
            focus.dispatch_action(&action, window, cx);
            return;
        }
        RowAction::Stop | RowAction::Cancel => model.update(cx, |model, cx| model.stop(key, cx)),
        RowAction::Retry(network) => model.update(cx, |model, cx| model.retry(key, network, cx)),
        RowAction::Complete => {
            // The question it may ask is a dialog of the window, which the
            // popover would cover.
            dismiss(popover, window, cx);
            let (busy, busy_key) = (busy.clone(), key.clone());
            let busy: crate::workspace::agent_thread::complete::Busy =
                std::rc::Rc::new(move |on, _, cx| {
                    busy.update(cx, |busy, cx| {
                        if on {
                            busy.0.insert(busy_key.clone());
                        } else {
                            busy.0.remove(&busy_key);
                        }
                        cx.notify();
                    })
                    .ok();
                });
            crate::workspace::agent_thread::complete::complete(key.clone(), busy, window, cx);
            return;
        }
    };
    busy.update(cx, |busy, cx| {
        busy.0.insert(key.clone());
        cx.notify();
    })
    .ok();
    let (key, busy) = (key.clone(), busy.clone());
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            cx.update(|window, cx| {
                busy.update(cx, |busy, cx| {
                    busy.0.remove(&key);
                    cx.notify();
                })
                .ok();
                if let Err(error) = result {
                    window.push_notification(crate::toast::error(format!("{error:#}")), cx)
                }
            })
            .ok();
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use gitez_agent_thread::{AgentThread, Ending, State, Timestamp};
    use gitez_sandbox::NetworkProfile;

    use super::{
        Group, Item, Part, RowAction, ThreadFacts, grouped, indicator_label, indicator_parts,
        row_actions, status_line,
    };
    use crate::agent_threads::Counts;
    use crate::workspace::agent_thread::{blocked_hosts, blocked_summary};

    fn agents() -> gitez_agent::Registry {
        gitez_agent::Registry::builtin()
    }

    fn thread(state: State, ending: Option<Ending>) -> AgentThread {
        let json = r#"{
            "id": "fix-the-parser",
            "title": "Fix the parser",
            "agent": "claude-code",
            "state": "queued",
            "ending": null,
            "created": "2026-11-13T10:00:00Z",
            "updated": "2026-11-13T10:00:00Z",
            "start": {"branch": "main", "commit": "0123456789abcdef0123456789abcdef01234567"},
            "branch": "agent/fix-the-parser",
            "worktree": "/tmp/worktree",
            "prepared": false,
            "network": "agent_only"
        }"#;
        let mut thread: AgentThread = serde_json::from_str(json).expect("a thread");
        thread.state = state;
        thread.ending = ending;
        thread
    }

    #[test]
    fn the_indicator_says_only_what_is_not_zero() {
        assert!(indicator_parts(Counts::default()).is_empty());
        assert_eq!(indicator_label(Counts::default()), "Agents");
        let counts = Counts {
            active: 2,
            waiting: 0,
            ready: 1,
            failed: 1,
            stopped: 1,
            completed: 7,
        };
        assert_eq!(
            indicator_parts(counts),
            [
                (Part::Active, "2 active".to_owned()),
                (Part::Ready, "1 ready".to_owned()),
                (Part::Failed, "1 failed".to_owned()),
                (Part::Stopped, "1 stopped".to_owned()),
            ]
        );
        assert_eq!(
            indicator_label(counts),
            "Agents: 2 active, 1 ready, 1 failed, 1 stopped"
        );
        let waiting = Counts {
            waiting: 3,
            ..Counts::default()
        };
        assert_eq!(indicator_label(waiting), "Agents: 3 waiting");
    }

    #[test]
    fn rows_offer_the_action_that_fits_the_state() {
        assert_eq!(
            row_actions(&thread(State::Queued, None), &agents(), &[]),
            [RowAction::Cancel]
        );
        assert_eq!(
            row_actions(&thread(State::Running, None), &agents(), &[]),
            [RowAction::Stop]
        );
        assert_eq!(
            row_actions(&thread(State::Preparing, None), &agents(), &[]),
            [RowAction::Stop]
        );
        assert_eq!(
            row_actions(
                &thread(State::Finished, Some(Ending::Answer)),
                &agents(),
                &[]
            ),
            [RowAction::Complete]
        );
        let mut pull_request = thread(State::Finished, Some(Ending::PullRequest));
        pull_request.pull_request = Some(12);
        assert_eq!(
            row_actions(&pull_request, &agents(), &[]),
            [RowAction::PullRequest(12)]
        );
        assert_eq!(
            row_actions(
                &thread(State::Finished, Some(Ending::Failed)),
                &agents(),
                &[]
            ),
            [
                RowAction::Retry(NetworkProfile::AgentOnly),
                RowAction::Complete
            ]
        );
        assert!(
            row_actions(
                &thread(State::Completed, Some(Ending::Answer)),
                &agents(),
                &[]
            )
            .is_empty()
        );
        pull_request.state = State::Completed;
        assert_eq!(
            row_actions(&pull_request, &agents(), &[]),
            [RowAction::PullRequest(12)]
        );
    }

    #[test]
    fn a_failed_thread_blocked_by_the_network_retries_with_packages() {
        let mut failed = thread(State::Finished, Some(Ending::Failed));
        failed.turns.push(
            serde_json::from_str(
                r#"{
                    "kind": "start",
                    "prompt": "Fix the parser",
                    "requested": "2026-11-13T10:00:00Z",
                    "resume": false,
                    "started": "2026-11-13T10:00:00Z",
                    "ended": "2026-11-13T10:06:00Z",
                    "transcript": null,
                    "result": {
                        "success": false, "message": "", "cost_usd": null, "duration_ms": null,
                        "steps": 3, "exit_code": 1, "stopped": false,
                        "blocked_hosts": ["static.crates.io", "index.crates.io"]
                    }
                }"#,
            )
            .expect("a turn"),
        );
        let package_hosts: Vec<String> = gitez_settings::DEFAULT_PACKAGE_HOSTS
            .iter()
            .map(|host| (*host).to_owned())
            .collect();
        let actions = row_actions(&failed, &agents(), &package_hosts);
        assert_eq!(actions[0], RowAction::Retry(NetworkProfile::Packages));
        assert_eq!(
            actions[0].label(failed.network),
            "Retry with Packages",
            "the label names the network it changes to"
        );
        let now: Timestamp = "2026-11-13T10:46:00Z".parse().expect("a time");
        let line: String = status_line(&failed, &agents(), &ThreadFacts::default(), None, 3, now)
            .into_iter()
            .map(|span| span.text)
            .collect();
        assert_eq!(
            line,
            "Failed after 6 min · 40 min ago · static.crates.io and 1 more blocked by Agent API only"
        );
    }

    /// The real Claude Code tries its own telemetry and connectors hosts in
    /// every run; they are its optional hosts, so they never make Retry
    /// widen the network, even when the Packages list would allow them. A
    /// host Packages allows still does, also by `*.` rule.
    #[test]
    fn only_hosts_packages_allows_suggest_packages() {
        let mut stopped = thread(State::Finished, Some(Ending::Stopped));
        let mut turn: gitez_agent_thread::Turn = serde_json::from_str(
            r#"{
                "kind": "start", "prompt": "Wait", "requested": "2026-11-13T10:00:00Z",
                "resume": false, "started": null, "ended": null, "transcript": null,
                "result": {
                    "success": false, "message": "", "cost_usd": null, "duration_ms": null,
                    "steps": 1, "exit_code": null, "stopped": true,
                    "blocked_hosts": ["mcp-proxy.anthropic.com", "http-intake.logs.us5.datadoghq.com"]
                }
            }"#,
        )
        .expect("a turn");
        stopped.turns.push(turn.clone());
        let package_hosts: Vec<String> = gitez_settings::DEFAULT_PACKAGE_HOSTS
            .iter()
            .map(|host| (*host).to_owned())
            .collect();
        assert_eq!(
            super::suggested_retry(&stopped, &agents(), &package_hosts),
            NetworkProfile::AgentOnly
        );
        assert_eq!(
            super::suggested_retry(
                &stopped,
                &agents(),
                &["*.anthropic.com".to_owned(), "*.datadoghq.com".to_owned()]
            ),
            NetworkProfile::AgentOnly,
            "the agent's optional hosts never count, whatever Packages allows"
        );
        if let Some(result) = turn.result.as_mut() {
            result.blocked_hosts.push("cdn.example.org".into());
        }
        stopped.turns[0] = turn;
        assert_eq!(
            super::suggested_retry(&stopped, &agents(), &["*.example.org".to_owned()]),
            NetworkProfile::Packages
        );
        assert_eq!(
            super::suggested_retry(&stopped, &agents(), &["*.ample.org".to_owned()]),
            NetworkProfile::AgentOnly
        );
    }

    /// A failed row names only the hosts the run may have needed; with
    /// only the agent's optional hosts blocked it names none.
    #[test]
    fn a_failed_row_leaves_out_the_agents_optional_hosts() {
        let failed = |blocked: &[&str]| {
            let mut failed = thread(State::Finished, Some(Ending::Failed));
            let mut turn: gitez_agent_thread::Turn = serde_json::from_str(
                r#"{
                    "kind": "start", "prompt": "Fix it", "requested": "2026-11-13T10:00:00Z",
                    "resume": false, "started": "2026-11-13T10:00:00Z",
                    "ended": "2026-11-13T10:06:00Z", "transcript": null,
                    "result": {
                        "success": false, "message": "", "cost_usd": null, "duration_ms": null,
                        "steps": 3, "exit_code": 1, "stopped": false, "blocked_hosts": []
                    }
                }"#,
            )
            .expect("a turn");
            if let Some(result) = turn.result.as_mut() {
                result.blocked_hosts = blocked.iter().map(|host| (*host).to_owned()).collect();
            }
            failed.turns.push(turn);
            failed
        };
        let now: Timestamp = "2026-11-13T10:46:00Z".parse().expect("a time");
        let line = |thread: &AgentThread| -> String {
            status_line(thread, &agents(), &ThreadFacts::default(), None, 3, now)
                .into_iter()
                .map(|span| span.text)
                .collect()
        };
        let optional = [
            "mcp-proxy.anthropic.com",
            "http-intake.logs.us5.datadoghq.com",
        ];
        // The thread view's count: only the other hosts.
        let count = |thread: &AgentThread| blocked_summary(&blocked_hosts(thread, &agents()));
        assert_eq!(count(&failed(&[])), "No blocked requests");
        assert_eq!(count(&failed(&optional)), "No blocked requests");
        assert_eq!(
            count(&failed(&[optional[0], "static.crates.io", optional[1]])),
            "1 blocked request"
        );
        assert_eq!(
            count(&failed(&[
                "static.crates.io",
                "index.crates.io",
                optional[1]
            ])),
            "2 blocked requests"
        );
        let only_optional = failed(&optional);
        let split = blocked_hosts(&only_optional, &agents());
        assert!(split.problems.is_empty());
        assert_eq!(
            split
                .optional
                .iter()
                .map(|(host, _)| *host)
                .collect::<Vec<_>>(),
            optional
        );
        assert_eq!(line(&failed(&optional)), "Failed after 6 min · 40 min ago");
        assert_eq!(
            line(&failed(&[optional[0], "static.crates.io", optional[1]])),
            "Failed after 6 min · 40 min ago · static.crates.io blocked by Agent API only"
        );
    }

    #[test]
    fn groups_put_what_needs_the_user_first_and_completed_last() {
        let item = |id: &str, state: State, ending: Option<Ending>, minute: u32| {
            let mut thread = thread(state, ending);
            thread.id = gitez_agent_thread::AgentThreadId::new(id).expect("an id");
            thread.created = format!("2026-11-13T10:{minute:02}:00Z")
                .parse()
                .expect("a time");
            thread.updated = thread.created;
            Item {
                common_dir: PathBuf::from("/repo/.git"),
                repository: "repo".into(),
                thread: Arc::new(thread),
                facts: ThreadFacts::default(),
            }
        };
        let groups = grouped(vec![
            item("done", State::Completed, Some(Ending::Answer), 1),
            item("late", State::Running, None, 9),
            item("early", State::Running, None, 2),
            item("queued", State::Queued, None, 3),
            item("answer", State::Finished, Some(Ending::Answer), 4),
            item("failed", State::Finished, Some(Ending::Failed), 5),
            item("newer-answer", State::Finished, Some(Ending::Answer), 6),
        ]);
        let names: Vec<(Group, Vec<&str>)> = groups
            .iter()
            .map(|(group, items)| {
                (
                    *group,
                    items.iter().map(|item| item.thread.id.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            names,
            [
                (Group::NeedsAttention, vec!["failed"]),
                (Group::ReadyForReview, vec!["newer-answer", "answer"]),
                (Group::Active, vec!["early", "late"]),
                (Group::Waiting, vec!["queued"]),
                (Group::Completed, vec!["done"]),
            ]
        );
    }

    #[test]
    fn a_queued_thread_says_when_it_starts() {
        let now = Timestamp::now();
        let line: String = status_line(
            &thread(State::Queued, None),
            &agents(),
            &ThreadFacts::default(),
            Some("gitez"),
            3,
            now,
        )
        .into_iter()
        .map(|span| span.text)
        .collect();
        assert_eq!(
            line,
            "gitez · Queued · starts when one of the 3 agents is free"
        );
    }

    /// C1: a thread that left uncommitted changes never reads "No changes",
    /// on any row; a completed one whose worktree was kept says so.
    #[test]
    fn uncommitted_changes_are_said_on_every_row() {
        let now: Timestamp = "2026-11-13T10:46:00Z".parse().expect("a time");
        let line = |thread: &AgentThread, facts: &ThreadFacts| -> String {
            status_line(thread, &agents(), facts, None, 3, now)
                .into_iter()
                .map(|span| span.text)
                .collect()
        };
        let mut answer = thread(State::Finished, Some(Ending::Answer));
        answer.updated = "2026-11-13T10:40:00Z".parse().expect("a time");
        assert!(line(&answer, &ThreadFacts::default()).starts_with("Answer · finished"));
        answer.uncommitted = Some(gitez_agent_thread::Uncommitted {
            checked: now,
            count: 3,
            paths: vec!["?? notes.txt".into()],
            commit_error: Some("pre-commit hook failed".into()),
        });
        let text = line(&answer, &ThreadFacts::default());
        assert!(
            text.starts_with("3 uncommitted changes · finished"),
            "{text}"
        );
        assert!(!text.contains("No changes"));
        assert_eq!(
            crate::workspace::agent_thread::Look::of(&answer),
            crate::workspace::agent_thread::Look::Uncommitted
        );
        // A failed one says both.
        let mut failed = answer.clone();
        failed.ending = Some(Ending::Failed);
        assert!(line(&failed, &ThreadFacts::default()).contains("Failed · 3 uncommitted changes"));
        // Completed by hand with the worktree kept.
        let mut completed = answer.clone();
        completed.state = State::Completed;
        let kept = ThreadFacts {
            worktree_present: true,
            ..ThreadFacts::default()
        };
        let text = line(&completed, &kept);
        assert!(
            text.starts_with("Completed by hand · worktree kept: 3 uncommitted changes"),
            "{text}"
        );
        // Completed with its pull request merged.
        let mut merged = thread(State::Completed, Some(Ending::PullRequest));
        merged.pull_request = Some(7);
        let facts = ThreadFacts {
            pull_request: Some(gitez_review::State::Merged),
            ..ThreadFacts::default()
        };
        assert!(line(&merged, &facts).starts_with("Completed · PR #7 merged"));
        // Uncommitted changes count as ready for review.
        assert_eq!(Counts::of([&answer]).ready, 1);
    }
}
