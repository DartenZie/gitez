//! The agent prompt panel of a local pull request, as in the
//! `LocalPR.dc.html` artboard: which comments go into the prompt, how it is
//! put together, the prompt itself, and sending it to a coding agent whose
//! work streams in below.
//!
//! A pull request that an agent thread created sends its prompt to that
//! thread instead (E.11): the thread continues in its own worktree, so the
//! head branch need not be checked out, and the panel follows the thread.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use gitez_agent::{
    Agent, AgentConfig, AgentEvent, AgentProcess, ClaudeCode, Conversation, Outcome, Registry,
    RunSpec, Stopper,
};
use gitez_agent_thread::{AgentThread, Ending, State as ThreadState, TurnKind};
use gitez_command::Git;
use gitez_git::Repository;
use gitez_review::{PromptContext, RunHeader, Side, Store, Template, Timestamp};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::text::TextView;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Review, Row, Subject, muted};
use crate::agent_threads::{AgentThreadKey, AgentThreads, OpenAgentThread};
use crate::theme::{self, color};
use crate::time;
use crate::workspace::agent_thread;

#[cfg(all(test, target_os = "macos"))]
mod loop_shots;

/// Width of the panel beside the diff.
pub(super) const PANEL_WIDTH: Pixels = px(380.);
/// How long "Copied" shows after copying.
const COPIED_FOR: Duration = Duration::from_secs(2);
/// Wait this long after the last keystroke before saving the test command.
const SAVE_TEST_COMMAND_AFTER: Duration = Duration::from_millis(600);
/// Where the test command is kept: the repository's own git config.
const TEST_COMMAND_KEY: &str = "gitez.testCommand";
/// The prompt options' agent id for copying only.
const NO_AGENT: &str = "none";

const AGENTS: [(&str, &str); 2] = [(ClaudeCode::ID, "Claude Code"), (NO_AGENT, "Copy only")];
const TEMPLATES: [(Template, &str); 2] = [
    (Template::Fix, "Fix review comments"),
    (Template::Explain, "Explain, don’t change"),
];

/// The agent runs of every open pull request, by common git directory and
/// number. They outlive the review that started them, so leaving a pull
/// request does not stop or lose its run.
#[derive(Default)]
struct AgentRuns(HashMap<(PathBuf, u32), Entity<AgentRun>>);

impl Global for AgentRuns {}

/// The panel's own state.
pub(super) struct PromptPanel {
    agent: Entity<SelectState<Vec<SharedString>>>,
    template: Entity<SelectState<Vec<SharedString>>>,
    /// Set while "Copied" shows.
    copied: Option<Task<()>>,
    /// The run shown: running now, or the last one.
    run: Option<(Entity<AgentRun>, Subscription)>,
    /// The command that runs the tests, as set for the repository.
    test_command: Entity<InputState>,
    /// The command guessed from the project's files, used when none is set.
    detected_test_command: Option<String>,
    /// Saving the test command, after a pause in typing.
    saving_test_command: Task<()>,
    test_command_error: Option<SharedString>,
    /// The repository's common git directory, which names its agent
    /// threads.
    common_dir: PathBuf,
    /// What the panel asked of the pull request's agent thread, while the
    /// request is under way.
    thread_busy: Option<(ThreadAction, Task<()>)>,
    /// The thread's `updated` time when a prompt was sent to it: until the
    /// thread changes, it counts as working.
    sent_at: Option<gitez_agent_thread::Timestamp>,
    /// Why the last request to the thread failed.
    thread_error: Option<SharedString>,
    /// The thread's `updated` time last seen, to redraw when it changes.
    thread_seen: Option<gitez_agent_thread::Timestamp>,
    /// Whether the thread's worktree folder is gone, as last checked, and
    /// for which `updated` time of the thread.
    worktree_gone: bool,
    worktree_checked: Option<gitez_agent_thread::Timestamp>,
    _check_worktree: Task<()>,
    _load_run: Task<()>,
    _detect: Task<()>,
    _subscriptions: Vec<Subscription>,
}

/// What the panel can ask of a pull request's agent thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThreadAction {
    Send,
    Retry,
    Stop,
}

impl PromptPanel {
    /// The panel for pull request `number` with `options`.
    pub(super) fn new(
        repository: &Repository,
        options: &gitez_review::PromptOptions,
        window: &mut Window,
        cx: &mut Context<Review>,
    ) -> Self {
        let configured = repository.config_string(TEST_COMMAND_KEY);
        let test_command = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("Such as cargo test");
            if let Some(command) = configured {
                input.set_value(command, window, cx);
            }
            input
        });
        let detect = match repository.work_dir() {
            Some(dir) => {
                let dir = dir.to_owned();
                cx.spawn_in(window, async move |this, cx| {
                    let detected = cx
                        .background_executor()
                        .spawn(async move { gitez_agent::detect_test_command(&dir) })
                        .await;
                    this.update_in(cx, |this, window, cx| {
                        if let Subject::PullRequest(subject) = &mut this.subject {
                            if let Some(command) = &detected {
                                subject.panel.test_command.update(cx, |input, cx| {
                                    input.set_placeholder(command.clone(), window, cx)
                                });
                            }
                            subject.panel.detected_test_command = detected;
                            cx.notify();
                        }
                    })
                    .ok();
                })
            }
            None => Task::ready(()),
        };
        let agent_index = AGENTS
            .iter()
            .position(|(id, _)| *id == options.agent)
            .unwrap_or(0);
        let template_index = TEMPLATES
            .iter()
            .position(|(template, _)| *template == options.template)
            .unwrap_or(0);
        let select = |names: Vec<SharedString>, index: usize, window: &mut Window, cx: &mut App| {
            cx.new(|cx| SelectState::new(names, Some(IndexPath::new(index)), window, cx))
        };
        let agent = select(
            AGENTS.iter().map(|(_, name)| (*name).into()).collect(),
            agent_index,
            window,
            cx,
        );
        let template = select(
            TEMPLATES.iter().map(|(_, name)| (*name).into()).collect(),
            template_index,
            window,
            cx,
        );
        let mut subscriptions = vec![
            cx.subscribe(&test_command, |this, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    this.save_test_command(cx);
                }
            }),
            cx.subscribe(&agent, |this, _, event: &SelectEvent<_>, cx| {
                if let SelectEvent::Confirm(Some(name)) = event
                    && let Some((id, _)) = AGENTS.iter().find(|(_, label)| name == label)
                {
                    let id = (*id).to_owned();
                    this.change_prompt(cx, |options| options.agent = id);
                }
            }),
            cx.subscribe(&template, |this, _, event: &SelectEvent<_>, cx| {
                if let SelectEvent::Confirm(Some(name)) = event
                    && let Some((template, _)) = TEMPLATES.iter().find(|(_, label)| name == label)
                {
                    let template = *template;
                    this.change_prompt(cx, |options| options.template = template);
                }
            }),
        ];
        // The pull request's agent thread, if one created it, as it runs.
        if let Some(threads) = AgentThreads::try_global(cx) {
            subscriptions.push(cx.observe(&threads, |this, _, cx| this.follow_agent_thread(cx)));
        }
        Self {
            agent,
            template,
            copied: None,
            run: None,
            test_command,
            detected_test_command: None,
            saving_test_command: Task::ready(()),
            test_command_error: None,
            common_dir: repository.common_dir(),
            thread_busy: None,
            sent_at: None,
            thread_error: None,
            thread_seen: None,
            worktree_gone: false,
            worktree_checked: None,
            _check_worktree: Task::ready(()),
            _load_run: Task::ready(()),
            _detect: detect,
            _subscriptions: subscriptions,
        }
    }
}

impl Review {
    fn panel(&self) -> Option<(&super::PullRequestSubject, &PromptPanel)> {
        match &self.subject {
            Subject::PullRequest(subject) => Some((subject, &subject.panel)),
            _ => None,
        }
    }

    /// Shows the run of this pull request still going, or else reads its
    /// last transcript, once the pull request is open.
    pub(super) fn find_run(&mut self, cx: &mut Context<Self>) {
        let Subject::PullRequest(subject) = &self.subject else {
            return;
        };
        let key = (self.repository.common_dir(), subject.pull_request.number);
        if let Some(run) = cx
            .try_global::<AgentRuns>()
            .and_then(|runs| runs.0.get(&key))
            .cloned()
        {
            self.show_run(run, cx);
            return;
        }
        let (store, number) = (subject.store.clone(), subject.pull_request.number);
        let read = cx.background_executor().spawn(async move {
            let Some(last) = store.runs(number)?.pop() else {
                return Ok(None);
            };
            store.read_run(&last).map(Some)
        });
        let load = cx.spawn(async move |this, cx| {
            let read: gitez_review::Result<_> = read.await;
            this.update(cx, |this, cx| match read {
                Ok(Some((header, lines))) => {
                    let run = cx.new(|_| AgentRun::saved(header, &lines));
                    this.show_run(run, cx);
                }
                Ok(None) => {}
                Err(error) => {
                    let run = cx.new(|_| {
                        AgentRun::failed(format!(
                            "Could not read the last agent run: {:#}",
                            anyhow::Error::from(error)
                        ))
                    });
                    this.show_run(run, cx);
                }
            })
            .ok();
        });
        if let Subject::PullRequest(subject) = &mut self.subject {
            subject.panel._load_run = load;
        }
    }

    fn show_run(&mut self, run: Entity<AgentRun>, cx: &mut Context<Self>) {
        let observe = cx.observe(&run, |_, _, cx| cx.notify());
        if let Subject::PullRequest(subject) = &mut self.subject {
            subject.panel.run = Some((run, observe));
            cx.notify();
        }
    }

    /// Changes the prompt options with `change` and saves them.
    fn change_prompt(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut gitez_review::PromptOptions),
    ) {
        let Subject::PullRequest(subject) = &mut self.subject else {
            return;
        };
        change(&mut subject.pull_request.prompt);
        self.save_pull_request(cx);
        cx.notify();
    }

    /// The prompt as it would be sent now; `None` without included
    /// comments.
    fn prompt_text(&self, cx: &App) -> Option<String> {
        let (subject, panel) = self.panel()?;
        let typed = panel.test_command.read(cx).value().trim().to_owned();
        subject.pull_request.prompt(&PromptContext {
            repository: self.repository.name(),
            commits_ahead: subject.ahead,
            test_command: if typed.is_empty() {
                panel.detected_test_command.clone()
            } else {
                Some(typed)
            },
        })
    }

    /// Saves the test command in the repository's git config once typing
    /// pauses; an empty one is removed, so the guess applies again.
    fn save_test_command(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.repository.work_dir() else {
            return;
        };
        let git = Git::new(dir);
        let Subject::PullRequest(subject) = &mut self.subject else {
            return;
        };
        let panel = &mut subject.panel;
        let command = panel.test_command.read(cx).value().trim().to_owned();
        panel.saving_test_command = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(SAVE_TEST_COMMAND_AFTER)
                .await;
            let saved = cx
                .background_executor()
                .spawn(async move {
                    git.set_config(TEST_COMMAND_KEY, (!command.is_empty()).then_some(&*command))
                })
                .await;
            this.update(cx, |this, cx| {
                if let Subject::PullRequest(subject) = &mut this.subject {
                    subject.panel.test_command_error = saved.err().map(|error| {
                        format!(
                            "Could not save the test command: {:#}",
                            anyhow::Error::from(error)
                        )
                        .into()
                    });
                    cx.notify();
                }
            })
            .ok();
        });
        // The prompt shows the command as typed.
        cx.notify();
    }

    fn copy_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.prompt_text(cx) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(prompt));
        let reset = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            this.update(cx, |this, cx| {
                if let Subject::PullRequest(subject) = &mut this.subject {
                    subject.panel.copied = None;
                    cx.notify();
                }
            })
            .ok();
        });
        if let Subject::PullRequest(subject) = &mut self.subject {
            subject.panel.copied = Some(reset);
            cx.notify();
        }
    }

    /// Why the prompt cannot be sent now, if it cannot.
    fn cannot_send(&self, cx: &App) -> Option<String> {
        let (subject, panel) = self.panel()?;
        if self.repository.work_dir().is_none() {
            return Some("A bare repository has no working tree for the agent to work in.".into());
        }
        if subject.pull_request.state != gitez_review::State::Open {
            return Some("This pull request is no longer open.".into());
        }
        // The thread works in its own worktree, whatever is checked out.
        if let Some(thread) = self.agent_thread_for_pull_request(cx) {
            if panel.thread_busy.is_some() {
                return Some("Sending to the thread…".into());
            }
            if thread.state == ThreadState::Waiting {
                return Some(
                    "The thread waits for the agent’s usage limit to reset. Send once it has \
                     finished."
                        .into(),
                );
            }
            if thread.state.is_active() || panel.sent_at == Some(thread.updated) {
                return Some("The agent is still working on this thread.".into());
            }
            if thread.session_id.is_none() {
                return Some("Retry the thread first: the agent never started.".into());
            }
            return None;
        }
        if !subject.tips.checked_out {
            return Some(format!(
                "The agent commits to the branch that is checked out. Check out {} to send.",
                subject.pull_request.head.branch
            ));
        }
        if panel
            .run
            .as_ref()
            .is_some_and(|(run, _)| run.read(cx).is_running())
        {
            return Some("The agent is still working on the last prompt.".into());
        }
        None
    }

    /// Sends the prompt to the pull request's agent, or to the agent
    /// thread that created it.
    fn send_prompt(&mut self, cx: &mut Context<Self>) {
        if self.cannot_send(cx).is_some() {
            return;
        }
        let (Some(prompt), Some(dir)) = (self.prompt_text(cx), self.repository.work_dir()) else {
            return;
        };
        if let Some(thread) = self.agent_thread_for_pull_request(cx) {
            let sent_at = thread.updated;
            self.ask_thread(ThreadAction::Send, cx, move |model, key, cx| {
                model.reply(key, TurnKind::Review, prompt, Vec::new(), cx)
            });
            if let Subject::PullRequest(subject) = &mut self.subject {
                subject.panel.sent_at = Some(sent_at);
            }
            return;
        }
        let Subject::PullRequest(subject) = &self.subject else {
            return;
        };
        let Some(launch) = Launch::of(&subject.pull_request.prompt.agent, cx) else {
            return;
        };
        let number = subject.pull_request.number;
        let key = (self.repository.common_dir(), number);
        let (store, dir) = (subject.store.clone(), dir.to_owned());
        let run = cx.new(|cx| AgentRun::start(launch, prompt, dir, store, number, cx));
        cx.default_global::<AgentRuns>().0.insert(key, run.clone());
        self.show_run(run, cx);
    }

    /// The agent thread that created the pull request reviewed, unless it
    /// is completed: a reopened pull request of a completed thread is
    /// reviewed as any other.
    pub(super) fn agent_thread_for_pull_request(&self, cx: &App) -> Option<Arc<AgentThread>> {
        let (subject, panel) = self.panel()?;
        let threads = AgentThreads::try_global(cx)?;
        let thread = threads
            .read(cx)
            .for_pull_request(&panel.common_dir, subject.pull_request.number)?;
        (thread.state != ThreadState::Completed).then(|| thread.clone())
    }

    /// Follows the pull request's agent thread: redraws when it changes,
    /// and checks whether its worktree is still there.
    pub(super) fn follow_agent_thread(&mut self, cx: &mut Context<Self>) {
        let thread = self.agent_thread_for_pull_request(cx);
        let Subject::PullRequest(subject) = &mut self.subject else {
            return;
        };
        let panel = &mut subject.panel;
        let updated = thread.as_ref().map(|thread| thread.updated);
        // An active thread's elapsed time advances with the model's redraws.
        let active = thread
            .as_ref()
            .is_some_and(|thread| thread.state.is_active());
        if panel.thread_seen == updated && !active {
            return;
        }
        panel.thread_seen = updated;
        if let Some(thread) = thread
            && panel.worktree_checked != Some(thread.updated)
        {
            panel.worktree_checked = Some(thread.updated);
            let worktree = thread.worktree.clone();
            panel._check_worktree = cx.spawn(async move |this, cx| {
                let gone = cx
                    .background_executor()
                    .spawn(async move {
                        worktree.is_some_and(|dir| std::fs::symlink_metadata(dir).is_err())
                    })
                    .await;
                this.update(cx, |this, cx| {
                    if let Subject::PullRequest(subject) = &mut this.subject {
                        subject.panel.worktree_gone = gone;
                        cx.notify();
                    }
                })
                .ok();
            });
        }
        cx.notify();
    }

    /// Runs `action` on the pull request's agent thread, showing `what`
    /// until it ends and its error if it fails.
    fn ask_thread(
        &mut self,
        what: ThreadAction,
        cx: &mut Context<Self>,
        action: impl FnOnce(
            &mut AgentThreads,
            &AgentThreadKey,
            &mut Context<AgentThreads>,
        ) -> Task<anyhow::Result<()>>,
    ) {
        let (Some(thread), Some(threads)) = (
            self.agent_thread_for_pull_request(cx),
            AgentThreads::try_global(cx),
        ) else {
            return;
        };
        let Subject::PullRequest(subject) = &mut self.subject else {
            return;
        };
        if subject.panel.thread_busy.is_some() {
            return;
        }
        let key = AgentThreadKey {
            common_dir: subject.panel.common_dir.clone(),
            id: thread.id.clone(),
        };
        let task = threads.update(cx, |threads, cx| action(threads, &key, cx));
        let task = cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if let Subject::PullRequest(subject) = &mut this.subject {
                    let panel = &mut subject.panel;
                    panel.thread_busy = None;
                    panel.thread_error = result.err().map(|error| format!("{error:#}").into());
                    if panel.thread_error.is_some() {
                        panel.sent_at = None;
                    }
                    cx.notify();
                }
            })
            .ok();
        });
        subject.panel.thread_busy = Some((what, task));
        subject.panel.thread_error = None;
        cx.notify();
    }

    fn retry_thread(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.agent_thread_for_pull_request(cx) else {
            return;
        };
        let model = AgentThreads::global(cx).read(cx);
        let network = agent_thread::suggested_retry(
            &thread,
            model.agents(),
            &model.settings().sandbox.package_hosts,
        );
        self.ask_thread(ThreadAction::Retry, cx, move |model, key, cx| {
            model.retry(key, network, cx)
        });
    }

    fn stop_thread(&mut self, cx: &mut Context<Self>) {
        self.ask_thread(ThreadAction::Stop, cx, |model, key, cx| model.stop(key, cx));
    }

    fn stop_agent(&mut self, cx: &mut Context<Self>) {
        if let Some((_, panel)) = self.panel()
            && let Some((run, _)) = &panel.run
        {
            run.read(cx).stop();
        }
    }

    /// The panel beside the diff.
    pub(super) fn render_prompt_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (subject, panel) = self.panel()?;
        let pull_request = &subject.pull_request;
        let mono = cx.theme().mono_font_family.clone();
        let open: Vec<_> = {
            let mut open: Vec<_> = pull_request
                .threads
                .iter()
                .filter(|thread| thread.is_open())
                .collect();
            open.sort_by(|a, b| {
                let key = |thread: &gitez_review::Thread| {
                    let anchor = &thread.anchor;
                    (anchor.path.clone(), anchor.lines, anchor.side == Side::Old)
                };
                key(a).cmp(&key(b))
            });
            open
        };
        let included = open.iter().filter(|thread| thread.in_prompt).count();
        let prompt = self.prompt_text(cx);
        let agent = Registry::builtin().get(&pull_request.prompt.agent);
        let cannot_send = self.cannot_send(cx);
        let options = &pull_request.prompt;
        let thread = self.agent_thread_for_pull_request(cx);
        // Made by an agent thread, completed or not: its review threads are
        // called comments throughout, also after the merge.
        let made_by_thread = AgentThreads::try_global(cx).is_some_and(|threads| {
            threads
                .read(cx)
                .for_pull_request(&panel.common_dir, pull_request.number)
                .is_some()
        });
        let sending = matches!(panel.thread_busy, Some((ThreadAction::Send, _)));

        let comments = open.iter().map(|thread| {
            let id = thread.id;
            let anchor = &thread.anchor;
            let name = anchor.path.rsplit('/').next().unwrap_or(&anchor.path);
            let (first, last) = anchor.lines;
            let lines = if first == last {
                format!("{name}:{first}")
            } else {
                format!("{name}:{first}–{last}")
            };
            let summary = thread
                .comments
                .first()
                .and_then(|comment| comment.body.lines().next())
                .unwrap_or_default()
                .to_owned();
            div()
                .id(("prompt-comment", id as usize))
                .h_flex()
                .items_start()
                .gap(px(10.))
                .px(px(12.))
                .py(px(10.))
                .rounded(px(8.))
                .border_1()
                .border_color(rgb(color::DIFF_CARD_BORDER))
                .bg(rgb(color::PROMPT_COMMENT))
                .child(
                    Checkbox::new(("prompt-include", id as usize))
                        .checked(thread.in_prompt)
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                            this.change_prompt_thread(id, *checked, cx)
                        })),
                )
                .child(
                    div()
                        .v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(3.))
                        .cursor_pointer()
                        .child(
                            div()
                                .font_family(mono.clone())
                                .text_size(theme::SMALL_TEXT_SIZE)
                                .text_color(rgb(color::TEXT_SECONDARY))
                                .child(if anchor.side == Side::Old {
                                    format!("{lines} (removed)")
                                } else {
                                    lines
                                }),
                        )
                        .child(
                            div()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(px(12.5))
                                .text_color(rgb(color::AUTHOR_TEXT))
                                .child(summary),
                        ),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.scroll_to_thread(id, cx)))
        });

        let section = || div().v_flex().gap(px(8.)).px(px(14.));
        let card = div()
            .v_flex()
            .flex_none()
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(color::DIFF_CARD_BORDER))
            .bg(rgb(color::PANEL))
            .overflow_hidden()
            .child(
                div()
                    .h_flex()
                    .gap(px(8.))
                    .px(px(14.))
                    .py(px(12.))
                    .border_b_1()
                    .border_color(rgb(color::DIFF_CARD_BORDER))
                    .child(
                        Icon::new(IconName::Terminal)
                            .size(px(16.))
                            .text_color(rgb(color::ACCENT)),
                    )
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Agent prompt"),
                    )
                    .child(muted("Updates as you comment").ml_auto().text_size(px(12.))),
            )
            .child(
                section()
                    .py(px(12.))
                    .child(
                        div()
                            .h_flex()
                            .gap(px(8.))
                            .text_size(px(12.))
                            .child(div().font_weight(FontWeight::SEMIBOLD).child("Comments"))
                            .child(muted(format!("{included} of {} included", open.len()))),
                    )
                    .when(open.is_empty(), |this| {
                        this.child(
                            muted("Hover a line of the diff and click + to comment.")
                                .text_size(px(12.5)),
                        )
                    })
                    .children(comments),
            )
            .child(
                div()
                    .px(px(14.))
                    .pb(px(12.))
                    .grid()
                    .grid_cols(2)
                    .gap(px(8.))
                    .child(labelled("Agent", Select::new(&panel.agent).small()))
                    .child(labelled("Template", Select::new(&panel.template).small()))
                    .child(
                        Checkbox::new("prompt-quote-code")
                            .label("Quote code")
                            .checked(options.quote_code)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                let checked = *checked;
                                this.change_prompt(cx, |options| options.quote_code = checked)
                            })),
                    )
                    .child(
                        Checkbox::new("prompt-run-tests")
                            .label("Run tests after")
                            .checked(options.run_tests)
                            .disabled(options.template == Template::Explain)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                let checked = *checked;
                                this.change_prompt(cx, |options| options.run_tests = checked)
                            })),
                    ),
            )
            .when(
                options.run_tests && options.template == Template::Fix,
                |this| {
                    this.child(
                        section()
                            .pb(px(12.))
                            .gap(px(4.))
                            .child(labelled(
                                "Test command",
                                Input::new(&panel.test_command).small(),
                            ))
                            .child(
                                muted(match &panel.test_command_error {
                                    Some(error) => error.clone(),
                                    None => "Kept in this repository’s git config;                                              leave it empty to use the suggestion."
                                        .into(),
                                })
                                .when(panel.test_command_error.is_some(), |this| {
                                    this.text_color(rgb(color::DIFF_REMOVED))
                                })
                                .text_size(theme::SMALL_TEXT_SIZE)
                                .line_height(relative(1.5)),
                            ),
                    )
                },
            )
            .child(
                div()
                    .id("prompt-text")
                    .mx(px(14.))
                    .max_h(px(430.))
                    .overflow_y_scroll()
                    // GPUI lets the wheel event go on to the panel's own
                    // scroll, which would move too: the box keeps it.
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .rounded(theme::RADIUS)
                    .border_1()
                    .border_color(rgb(color::DIFF_CARD_BORDER))
                    .bg(rgb(color::PROMPT))
                    .p(px(12.))
                    .font_family(mono)
                    .text_size(theme::SMALL_TEXT_SIZE)
                    .line_height(relative(1.6))
                    .text_color(rgb(color::DIFF_TEXT))
                    .map(|this| match &prompt {
                        Some(prompt) => this.child(prompt.clone()),
                        None => this.child(
                            muted("Include a comment to build the prompt.").text_size(px(12.)),
                        ),
                    }),
            )
            .child(
                section()
                    .pt(px(12.))
                    .pb(px(14.))
                    .child(
                        div()
                            .h_flex()
                            .gap(px(8.))
                            .child(
                                Button::new("prompt-copy")
                                    .flex_1()
                                    .h(px(34.))
                                    .icon(IconName::Copy)
                                    .label(if panel.copied.is_some() {
                                        "Copied"
                                    } else {
                                        "Copy"
                                    })
                                    .disabled(prompt.is_none())
                                    .on_click(cx.listener(|this, _, _, cx| this.copy_prompt(cx))),
                            )
                            .when_some(agent.as_ref(), |this, agent| {
                                this.child(
                                    Button::new("prompt-send")
                                        .debug_selector(|| "prompt-send".into())
                                        .primary()
                                        .flex_grow(2.)
                                        .flex_basis(relative(0.))
                                        .h(px(34.))
                                        .label(format!("Send to {}", agent.name()))
                                        .child(Icon::new(IconName::ArrowRight).size(px(14.)))
                                        .loading(sending)
                                        .disabled(prompt.is_none() || cannot_send.is_some())
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.send_prompt(cx)),
                                        ),
                                )
                            }),
                    )
                    .map(|this| match (&cannot_send, &thread, &agent) {
                        (_, _, None) => this,
                        (Some(reason), _, Some(_)) => this.child(
                            div()
                                .text_size(px(12.))
                                .line_height(relative(1.5))
                                .text_color(rgb(color::ACCENT))
                                .child(reason.clone()),
                        ),
                        (None, Some(_), Some(_)) => this.child(
                            muted("Runs in the thread’s worktree; your working tree is not touched.")
                                .text_size(px(12.))
                                .line_height(relative(1.5)),
                        ),
                        (None, None, Some(_)) => this,
                    })
                    .child(
                        // Beside an agent thread, review threads are called
                        // comments, to keep the two apart.
                        muted(if made_by_thread {
                            "Comments resolve themselves when a new commit on this branch \
                             changes the lines they point at. You can reopen any of them."
                        } else {
                            "Threads resolve themselves when a new commit on this branch \
                             changes the lines they point at. You can reopen any of them."
                        })
                        .text_size(theme::SMALL_TEXT_SIZE)
                        .line_height(relative(1.5)),
                    ),
            );

        Some(
            div()
                .id("prompt-panel")
                .v_flex()
                .w(PANEL_WIDTH)
                .flex_none()
                .h_full()
                .gap(px(14.))
                .overflow_y_scroll()
                .pb(px(24.))
                .child(card)
                .map(|this| match &thread {
                    Some(thread) => this.child(self.render_agent_thread(thread, cx)),
                    None => this.when_some(panel.run.as_ref(), |this, (run, _)| {
                        this.child(self.render_run(run.read(cx), cx))
                    }),
                })
                .into_any_element(),
        )
    }

    fn change_prompt_thread(&mut self, thread: u32, included: bool, cx: &mut Context<Self>) {
        let Subject::PullRequest(subject) = &mut self.subject else {
            return;
        };
        subject.pull_request.set_in_prompt(thread, included).ok();
        self.save_pull_request(cx);
        let anchor = self.scroll_anchor();
        self.rebuild_rows(anchor, cx);
        cx.notify();
    }

    /// Scrolls the diff to thread `id`.
    pub(super) fn scroll_to_thread(&mut self, id: u32, cx: &mut Context<Self>) {
        if let Some(index) = self
            .rows
            .iter()
            .position(|row| matches!(row, Row::Thread(_, thread) if *thread == id))
        {
            // The lines above it stay in sight.
            self.list.scroll_to(ListOffset {
                item_ix: index.saturating_sub(3),
                offset_in_item: px(0.),
            });
            cx.notify();
        }
    }

    /// The pull request's agent thread: what it does now or how its last
    /// turn ended, and what can be done with it.
    fn render_agent_thread(&self, thread: &AgentThread, cx: &mut Context<Self>) -> Div {
        let Some((_, panel)) = self.panel() else {
            return div();
        };
        let mono = cx.theme().mono_font_family.clone();
        let now = gitez_agent_thread::Timestamp::now();
        let busy = panel.thread_busy.as_ref().map(|(what, _)| *what);
        let reviewed = thread
            .turns
            .last()
            .is_some_and(|turn| turn.kind == TurnKind::Review);
        let text = |text: String| {
            div()
                .text_size(px(12.5))
                .line_height(relative(1.5))
                .text_color(rgb(color::TEXT))
                .child(text)
        };
        let note = |text: String| {
            muted(text)
                .text_size(theme::SMALL_TEXT_SIZE)
                .line_height(relative(1.5))
        };
        let failed = |text: String| {
            div()
                .text_size(px(12.5))
                .line_height(relative(1.5))
                .text_color(rgb(color::DIFF_REMOVED))
                .child(text)
        };
        let mut body: Vec<AnyElement> = Vec::new();
        match thread.state {
            ThreadState::Queued => body.push(
                text(if reviewed {
                    "Your comments are sent. The thread waits for a free running slot.".into()
                } else {
                    "The thread waits for a free running slot.".into()
                })
                .into_any_element(),
            ),
            ThreadState::Preparing | ThreadState::Running => {
                if let Some(activity) = &thread.activity {
                    body.push(
                        div()
                            .h_flex()
                            .gap(px(8.))
                            .font_family(mono.clone())
                            .text_size(theme::SMALL_TEXT_SIZE)
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(rgb(color::ACCENT))
                                    .child(activity.tool.clone().unwrap_or_else(|| "Setup".into())),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_color(rgb(color::TEXT_MUTED))
                                    .child(activity.detail.clone()),
                            )
                            .into_any_element(),
                    );
                } else {
                    body.push(
                        text(if thread.state == ThreadState::Preparing {
                            "Preparing the worktree…".into()
                        } else {
                            "Working on your comments…".into()
                        })
                        .into_any_element(),
                    );
                }
                body.push(note(agent_thread::summary(thread, now)).into_any_element());
            }
            ThreadState::Waiting => body.push(
                text(match thread.waiting_until {
                    Some(_) => format!(
                        "The agent’s usage limit was reached; the thread is {}.",
                        agent_thread::state_label(thread).to_lowercase()
                    ),
                    None => "The agent’s usage limit was reached; the thread waits for it to \
                             reset."
                        .into(),
                })
                .into_any_element(),
            ),
            ThreadState::Finished | ThreadState::Completed => {
                match thread.ending {
                    Some(Ending::Failed) => body.push(
                        failed(
                            thread
                                .error
                                .as_ref()
                                .map(|error| error.message.clone())
                                .unwrap_or_else(|| "The thread failed.".into()),
                        )
                        .into_any_element(),
                    ),
                    Some(Ending::Stopped) => body
                        .push(text("Stopped before the agent finished.".into()).into_any_element()),
                    _ => {
                        if let Some(message) = thread
                            .turns
                            .last()
                            .and_then(|turn| turn.result.as_ref())
                            .map(|result| {
                                // Not the `# Title` line that named the
                                // thread: its title is above.
                                gitez_agent_thread::message_without_title(
                                    &result.message,
                                    &thread.title,
                                )
                                .trim()
                                .to_owned()
                            })
                            .filter(|message| !message.is_empty())
                        {
                            body.push(
                                div()
                                    .id("thread-message")
                                    .max_h(px(260.))
                                    .overflow_y_scroll()
                                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                                    .text_size(px(12.5))
                                    .line_height(relative(1.5))
                                    .text_color(rgb(color::TEXT))
                                    .child(
                                        TextView::markdown(
                                            SharedString::from(format!(
                                                "prompt-thread-message-{}",
                                                thread.id
                                            )),
                                            message,
                                        )
                                        .selectable(true),
                                    )
                                    .into_any_element(),
                            );
                        }
                    }
                }
                body.push(note(agent_thread::summary(thread, now)).into_any_element());
                if let Some(changes) = agent_thread::uncommitted_text(thread)
                    && !panel.worktree_gone
                {
                    let why = agent_thread::commit_error_text(thread)
                        .map(|error| format!(" {error}"))
                        .unwrap_or_default();
                    body.push(
                        div()
                            .debug_selector(|| "prompt-thread-uncommitted".into())
                            .h_flex()
                            .items_start()
                            .gap(px(6.))
                            .text_size(theme::SMALL_TEXT_SIZE)
                            .line_height(relative(1.5))
                            .text_color(rgb(color::WARNING_TEXT))
                            .child(Icon::new(IconName::TriangleAlert).size(px(12.)).mt(px(2.)))
                            .child(div().flex_1().min_w_0().child(format!(
                                "{changes} left in the thread’s worktree, not in this pull \
                                 request.{why} Ask the agent in a comment to commit what it left."
                            )))
                            .into_any_element(),
                    );
                }
                if panel.worktree_gone {
                    body.push(
                        note(format!(
                            "The thread’s worktree folder is gone. Sending creates it again \
                             from {}.",
                            thread.branch_name()
                        ))
                        .text_color(rgb(color::ACCENT))
                        .into_any_element(),
                    );
                }
            }
        }
        if let Some(error) = &panel.thread_error {
            body.push(
                div()
                    .font_family(mono.clone())
                    .text_size(theme::SMALL_TEXT_SIZE)
                    .line_height(relative(1.5))
                    .text_color(rgb(color::DIFF_REMOVED))
                    .child(error.clone())
                    .into_any_element(),
            );
        }

        let retry = thread.state == ThreadState::Finished
            && matches!(thread.ending, Some(Ending::Failed | Ending::Stopped));
        let stop = thread.state.is_active();
        let open = {
            let action = OpenAgentThread {
                common_dir: panel.common_dir.clone(),
                id: thread.id.clone(),
            };
            Button::new("thread-open")
                .small()
                .label("Open thread")
                .on_click(move |_, window, cx| window.dispatch_action(Box::new(action.clone()), cx))
        };
        div()
            .debug_selector(|| "prompt-thread".into())
            .v_flex()
            .flex_none()
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(color::DIFF_CARD_BORDER))
            .bg(rgb(color::PANEL))
            .child(
                div()
                    .h_flex()
                    .gap(px(8.))
                    .px(px(14.))
                    .py(px(10.))
                    .border_b_1()
                    .border_color(rgb(color::DIFF_CARD_BORDER))
                    .child(agent_thread::agent_mark(&thread.agent))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(13.))
                            .child(thread.title.clone()),
                    )
                    .child(agent_thread::state_chip(thread)),
            )
            .child(
                div()
                    .v_flex()
                    .gap(px(8.))
                    .px(px(14.))
                    .py(px(12.))
                    .children(body),
            )
            .child(
                div()
                    .h_flex()
                    .gap(px(8.))
                    .px(px(14.))
                    .pb(px(12.))
                    .child(open)
                    .when(retry, |this| {
                        this.child(
                            Button::new("thread-retry")
                                .small()
                                .icon(IconName::RefreshCw)
                                .label("Retry")
                                .loading(busy == Some(ThreadAction::Retry))
                                .disabled(busy.is_some())
                                .on_click(cx.listener(|this, _, _, cx| this.retry_thread(cx))),
                        )
                    })
                    .when(stop, |this| {
                        this.child(
                            Button::new("thread-stop")
                                .ml_auto()
                                .small()
                                .icon(IconName::Square)
                                .label("Stop")
                                .loading(busy == Some(ThreadAction::Stop))
                                .disabled(busy.is_some())
                                .on_click(cx.listener(|this, _, _, cx| this.stop_thread(cx))),
                        )
                    }),
            )
    }

    /// The run's state and what the agent did, newest last.
    fn render_run(&self, run: &AgentRun, cx: &Context<Self>) -> Div {
        let mono = cx.theme().mono_font_family.clone();
        let (status, status_color) = run.status();
        let finished = run.events.iter().rev().find_map(|event| match event {
            AgentEvent::Finished(finished) => Some(finished),
            _ => None,
        });
        let summary = finished.map(|finished| {
            let mut parts = Vec::new();
            if let Some(turns) = finished.turns {
                parts.push(format!("{turns} turns"));
            }
            if let Some(duration) = finished.duration_ms {
                parts.push(format!("{}s", duration / 1000));
            }
            if let Some(cost) = finished.cost_usd {
                parts.push(format!("${cost:.2}"));
            }
            parts.join(" · ")
        });
        let events = run.events.iter().filter_map(|event| {
            let element = match event {
                // The result says when the usage limit stopped the run.
                AgentEvent::Started { .. }
                | AgentEvent::UsageLimit(_)
                | AgentEvent::Finished(_) => return None,
                AgentEvent::Text(text) => div()
                    .text_size(px(12.5))
                    .line_height(relative(1.5))
                    .text_color(rgb(color::TEXT))
                    .child(text.clone()),
                AgentEvent::Tool { name, detail } => div()
                    .h_flex()
                    .gap(px(8.))
                    .font_family(mono.clone())
                    .text_size(theme::SMALL_TEXT_SIZE)
                    .child(
                        div()
                            .flex_none()
                            .text_color(rgb(color::ACCENT))
                            .child(name.clone()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(detail.clone()),
                    ),
                AgentEvent::ToolFailed(why) => div()
                    .font_family(mono.clone())
                    .text_size(theme::SMALL_TEXT_SIZE)
                    .text_color(rgb(color::DIFF_REMOVED))
                    .child(why.clone()),
            };
            Some(element)
        });
        div()
            .v_flex()
            .flex_none()
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(color::DIFF_CARD_BORDER))
            .bg(rgb(color::PANEL))
            .child(
                div()
                    .h_flex()
                    .gap(px(8.))
                    .px(px(14.))
                    .py(px(10.))
                    .border_b_1()
                    .border_color(rgb(color::DIFF_CARD_BORDER))
                    .child(
                        Icon::new(IconName::Bot)
                            .size(px(15.))
                            .text_color(rgb(color::TEXT_MUTED)),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(13.))
                            .child(run.agent.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(status_color))
                            .child(status),
                    )
                    .when(run.is_running(), |this| {
                        this.child(
                            Button::new("agent-stop")
                                .ml_auto()
                                .small()
                                .icon(IconName::Square)
                                .label("Stop")
                                .on_click(cx.listener(|this, _, _, cx| this.stop_agent(cx))),
                        )
                    }),
            )
            .child(
                div()
                    .id("agent-events")
                    .v_flex()
                    .max_h(px(420.))
                    .overflow_y_scroll()
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .gap(px(8.))
                    .px(px(14.))
                    .py(px(12.))
                    .children(events)
                    .when(run.events.is_empty() && run.is_running(), |this| {
                        this.child(muted("Starting…").text_size(px(12.5)))
                    })
                    .when_some(
                        finished.filter(|finished| !finished.success),
                        |this, finished| {
                            this.child(
                                div()
                                    .text_size(px(12.5))
                                    .text_color(rgb(color::DIFF_REMOVED))
                                    .child(if finished.text.is_empty() {
                                        "The agent reported an error.".to_owned()
                                    } else {
                                        finished.text.clone()
                                    }),
                            )
                        },
                    )
                    .when_some(run.error.clone(), |this, error| {
                        this.child(
                            div()
                                .font_family(mono.clone())
                                .text_size(theme::SMALL_TEXT_SIZE)
                                .line_height(relative(1.5))
                                .text_color(rgb(color::DIFF_REMOVED))
                                .child(error),
                        )
                    }),
            )
            .when_some(
                summary.filter(|summary| !summary.is_empty()),
                |this, summary| {
                    this.child(
                        muted(summary)
                            .px(px(14.))
                            .pb(px(10.))
                            .text_size(theme::SMALL_TEXT_SIZE),
                    )
                },
            )
    }
}

/// A field with a small label above it.
fn labelled(label: &'static str, field: impl IntoElement) -> Div {
    div()
        .v_flex()
        .min_w_0()
        .gap(px(4.))
        .text_size(theme::SMALL_TEXT_SIZE)
        .text_color(rgb(color::TEXT_MUTED))
        .child(label)
        .child(field)
}

/// One run of an agent on a pull request's prompt: running, or read back
/// from its transcript.
pub struct AgentRun {
    agent: SharedString,
    started: Timestamp,
    events: Vec<AgentEvent>,
    state: RunState,
    /// Why the run failed, or its transcript could not be saved.
    error: Option<String>,
    _task: Task<()>,
}

enum RunState {
    /// Started; stoppable once the process is up.
    Running(Option<Stopper>),
    Ended(Outcome),
    /// Read from a transcript.
    Saved,
    Failed,
}

/// What the thread running the agent reports.
enum Message {
    Started(Stopper),
    Line(String),
    /// The transcript could not be written; the run goes on.
    TranscriptFailed(String),
    Ended(Result<Outcome, String>),
}

/// The agent a review run starts and how: the program, folder,
/// environment, arguments and model set for it in Settings, as an agent
/// thread gets them.
struct Launch {
    agent: Arc<dyn Agent>,
    config: AgentConfig,
    model: Option<String>,
}

impl Launch {
    /// The agent `id` as Settings configure it; the built-in agent with its
    /// defaults when the app has no agent threads model (in tests).
    fn of(id: &str, cx: &App) -> Option<Self> {
        match AgentThreads::try_global(cx) {
            Some(model) => {
                let model = model.read(cx);
                let entry = model.settings().agent(id);
                let agent = model.agents().get(id)?;
                Some(Self {
                    config: entry.config(),
                    model: Some(
                        entry
                            .model
                            .unwrap_or_else(|| agent.default_model().to_owned()),
                    )
                    .filter(|alias| !alias.is_empty()),
                    agent,
                })
            }
            None => {
                let agent = Registry::builtin().get(id)?;
                Some(Self {
                    model: Some(agent.default_model().to_owned()).filter(|alias| !alias.is_empty()),
                    agent,
                    config: AgentConfig::default(),
                })
            }
        }
    }
}

impl AgentRun {
    /// Starts the agent of `launch` on `prompt` in `dir`, saving the
    /// transcript in `store` under pull request `number`.
    fn start(
        launch: Launch,
        prompt: String,
        dir: PathBuf,
        store: Store,
        number: u32,
        cx: &mut Context<Self>,
    ) -> Self {
        let started = Timestamp::now();
        let (sender, mut messages) = futures::channel::mpsc::unbounded();
        let agent = launch.agent.clone();
        let thread_agent = agent.clone();
        // A thread of its own: it blocks on the agent's output for as long
        // as the agent works.
        std::thread::spawn(move || {
            let send = |message| sender.unbounded_send(message).is_ok();
            let header = RunHeader {
                started,
                agent: thread_agent.id().to_owned(),
                prompt: prompt.clone(),
            };
            let mut transcript = match store.start_run(number, header) {
                Ok(transcript) => Some(transcript),
                Err(error) => {
                    send(Message::TranscriptFailed(format!(
                        "{:#}",
                        anyhow::Error::from(error)
                    )));
                    None
                }
            };
            let mut session = match start(
                thread_agent.as_ref(),
                &launch.config,
                launch.model.as_deref(),
                &prompt,
                &dir,
            ) {
                Ok(session) => session,
                Err(error) => {
                    send(Message::Ended(Err(format!(
                        "{:#}",
                        anyhow::Error::from(error)
                    ))));
                    return;
                }
            };
            send(Message::Started(session.stopper()));
            loop {
                match session.next_line() {
                    Ok(Some(line)) => {
                        if let Some(writer) = &mut transcript
                            && let Err(error) = writer.append(&line)
                        {
                            send(Message::TranscriptFailed(format!(
                                "{:#}",
                                anyhow::Error::from(error)
                            )));
                            transcript = None;
                        }
                        // The window may be gone; the agent keeps going and
                        // its transcript is still written.
                        send(Message::Line(line));
                    }
                    Ok(None) => break,
                    Err(error) => {
                        session.stopper().stop();
                        send(Message::Ended(Err(format!(
                            "{:#}",
                            anyhow::Error::from(error)
                        ))));
                        return;
                    }
                }
            }
            let ended = session
                .finish()
                .map_err(|error| format!("{:#}", anyhow::Error::from(error)));
            send(Message::Ended(ended));
        });
        let parser = agent.clone();
        let task = cx.spawn(async move |this, cx| {
            while let Some(message) = messages.next().await {
                let received = this.update(cx, |run, cx| {
                    run.receive(message, parser.as_ref());
                    cx.notify();
                });
                if received.is_err() {
                    break;
                }
            }
        });
        Self {
            agent: agent.name().into(),
            started,
            events: Vec::new(),
            state: RunState::Running(None),
            error: None,
            _task: task,
        }
    }

    /// The run a transcript with `header` and output `lines` records.
    fn saved(header: RunHeader, lines: &[String]) -> Self {
        let agent = Registry::builtin().get(&header.agent);
        Self {
            agent: agent
                .as_ref()
                .map_or_else(|| header.agent.clone(), |agent| agent.name().to_owned())
                .into(),
            started: header.started,
            events: agent
                .map(|agent| lines.iter().filter_map(|line| agent.parse(line)).collect())
                .unwrap_or_default(),
            state: RunState::Saved,
            error: None,
            _task: Task::ready(()),
        }
    }

    fn failed(error: String) -> Self {
        Self {
            agent: "Agent".into(),
            started: Timestamp::now(),
            events: Vec::new(),
            state: RunState::Failed,
            error: Some(error),
            _task: Task::ready(()),
        }
    }

    fn receive(&mut self, message: Message, agent: &dyn Agent) {
        match message {
            Message::Started(stopper) => self.state = RunState::Running(Some(stopper)),
            Message::Line(line) => self.events.extend(agent.parse(&line)),
            Message::TranscriptFailed(error) => {
                self.error = Some(format!("The transcript is not being saved: {error}"))
            }
            Message::Ended(Ok(outcome)) => {
                if !outcome.success && !outcome.stopped && !outcome.stderr.is_empty() {
                    self.error = Some(outcome.stderr.clone());
                }
                self.state = RunState::Ended(outcome);
            }
            Message::Ended(Err(error)) => {
                self.error = Some(error);
                self.state = RunState::Failed;
            }
        }
    }

    fn is_running(&self) -> bool {
        matches!(self.state, RunState::Running(_))
    }

    fn stop(&self) {
        if let RunState::Running(Some(stopper)) = &self.state {
            stopper.stop();
        }
    }

    /// What the run's header says about it, and in which colour.
    fn status(&self) -> (String, u32) {
        let ago = time::relative(
            gitez_git::Timestamp {
                seconds: self.started.as_second(),
                offset: 0,
            },
            time::now(),
        );
        let reported = self.events.iter().rev().find_map(|event| match event {
            AgentEvent::Finished(finished) => Some(finished.success),
            _ => None,
        });
        match (&self.state, reported) {
            (RunState::Running(_), _) => ("Working…".to_owned(), color::ACCENT),
            (RunState::Ended(outcome), _) if outcome.stopped => {
                (format!("Stopped · started {ago}"), color::TEXT_MUTED)
            }
            (RunState::Ended(outcome), Some(true)) if outcome.success => {
                (format!("Done · started {ago}"), color::DIFF_ADDED)
            }
            (RunState::Saved, Some(true)) => (format!("Last run · {ago}"), color::TEXT_MUTED),
            (RunState::Saved, None) => (
                format!("Last run · {ago} · did not finish"),
                color::TEXT_MUTED,
            ),
            _ => (format!("Failed · started {ago}"), color::DIFF_REMOVED),
        }
    }
}

/// Starts `agent` on `prompt` in `dir`, the checked-out working tree, as
/// `config` and `model` (from Settings) say and outside GitEZ's sandbox.
fn start(
    agent: &dyn Agent,
    config: &AgentConfig,
    model: Option<&str>,
    prompt: &str,
    dir: &std::path::Path,
) -> gitez_agent::Result<AgentProcess> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let mut command = agent.command(
        config,
        &RunSpec {
            prompt,
            images: &[],
            cwd: dir,
            shared_folder: false,
            model,
            conversation: Conversation::New {
                session_id: &session_id,
            },
            system_prompt: None,
        },
    )?;
    // Not inside GitEZ's sandbox: Claude Code's own sandbox, if the user
    // turned it on, still applies, as before.
    if agent.id() == ClaudeCode::ID {
        ClaudeCode::keep_own_sandbox(&mut command);
    }
    AgentProcess::spawn(&command)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use gitez_settings::{AgentEntry, Settings};
    use gpui_kit::TestAppContext;

    use super::Launch;
    use crate::agent_threads::{Options, init_with};

    /// M5: "Send to Claude Code" runs the agent as Settings configure it,
    /// with its program, folder, environment, arguments and model.
    #[gpui_kit::test]
    fn a_review_run_uses_the_agent_settings(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp dir");
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).expect("data folder");
        let entry = AgentEntry {
            program: Some(PathBuf::from("/opt/agents/claude")),
            config_dir: Some(PathBuf::from("/opt/agents/config")),
            env: BTreeMap::from([("HTTPS_PROXY".to_owned(), "http://proxy:3128".to_owned())]),
            args: vec!["--verbose".into()],
            model: Some("haiku".into()),
        };
        Settings {
            agents: BTreeMap::from([(gitez_agent::ClaudeCode::ID.to_owned(), entry.clone())]),
            ..Settings::default()
        }
        .save(&Settings::path(&data_dir))
        .expect("save");
        cx.update(|cx| {
            gpui_kit::init(cx);
            init_with(
                Options {
                    data_dir: Some(data_dir),
                    sandbox: Arc::new(gitez_sandbox::Passthrough),
                    agents: gitez_agent::Registry::builtin(),
                    secrets: Arc::new(gitez_secret::Memory::default()),
                    exe: Err("no supervisor in tests".into()),
                    watch: false,
                },
                cx,
            );
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let launch = Launch::of(gitez_agent::ClaudeCode::ID, cx).expect("a known agent");
            assert_eq!(launch.config, entry.config());
            assert_eq!(launch.model.as_deref(), Some("haiku"));
            assert!(Launch::of("no-such-agent", cx).is_none());
        });
    }
}
