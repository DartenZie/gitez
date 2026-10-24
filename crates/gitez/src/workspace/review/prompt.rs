//! The agent prompt panel of a local pull request, as in the
//! `LocalPR.dc.html` artboard: which comments go into the prompt, how it is
//! put together, the prompt itself, and sending it to a coding agent whose
//! work streams in below.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use gitez_agent::{Agent, AgentEvent, ClaudeCode, Outcome, Stopper};
use gitez_command::Git;
use gitez_git::Repository;
use gitez_review::{PromptContext, RunHeader, Side, Store, Template, Timestamp};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Review, Row, Subject, muted};
use crate::theme::{self, color};
use crate::time;

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
    _load_run: Task<()>,
    _detect: Task<()>,
    _subscriptions: Vec<Subscription>,
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
        let subscriptions = vec![
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
        Self {
            agent,
            template,
            copied: None,
            run: None,
            test_command,
            detected_test_command: None,
            saving_test_command: Task::ready(()),
            test_command_error: None,
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

    /// Sends the prompt to the pull request's agent.
    fn send_prompt(&mut self, cx: &mut Context<Self>) {
        if self.cannot_send(cx).is_some() {
            return;
        }
        let (Some(prompt), Some(dir)) = (self.prompt_text(cx), self.repository.work_dir()) else {
            return;
        };
        let Subject::PullRequest(subject) = &self.subject else {
            return;
        };
        let Some(agent) = gitez_agent::agent(&subject.pull_request.prompt.agent) else {
            return;
        };
        let number = subject.pull_request.number;
        let key = (self.repository.common_dir(), number);
        let (store, dir) = (subject.store.clone(), dir.to_owned());
        let run = cx.new(|cx| AgentRun::start(agent.into(), prompt, dir, store, number, cx));
        cx.default_global::<AgentRuns>().0.insert(key, run.clone());
        self.show_run(run, cx);
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
        let agent = gitez_agent::agent(&pull_request.prompt.agent);
        let cannot_send = self.cannot_send(cx);
        let options = &pull_request.prompt;

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
                                        .primary()
                                        .flex_grow(2.)
                                        .flex_basis(relative(0.))
                                        .h(px(34.))
                                        .label(format!("Send to {}", agent.name()))
                                        .child(Icon::new(IconName::ArrowRight).size(px(14.)))
                                        .disabled(prompt.is_none() || cannot_send.is_some())
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.send_prompt(cx)),
                                        ),
                                )
                            }),
                    )
                    .when_some(cannot_send.filter(|_| agent.is_some()), |this, reason| {
                        this.child(
                            div()
                                .text_size(px(12.))
                                .line_height(relative(1.5))
                                .text_color(rgb(color::ACCENT))
                                .child(reason),
                        )
                    })
                    .child(
                        muted(
                            "Threads resolve themselves when a new commit on this branch \
                             changes the lines they point at. You can reopen any of them.",
                        )
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
                .when_some(panel.run.as_ref(), |this, (run, _)| {
                    this.child(self.render_run(run.read(cx), cx))
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
    fn scroll_to_thread(&mut self, id: u32, cx: &mut Context<Self>) {
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
                AgentEvent::Started { .. } | AgentEvent::Finished(_) => return None,
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

impl AgentRun {
    /// Starts `agent` on `prompt` in `dir`, saving the transcript in
    /// `store` under pull request `number`.
    fn start(
        agent: Arc<dyn Agent>,
        prompt: String,
        dir: PathBuf,
        store: Store,
        number: u32,
        cx: &mut Context<Self>,
    ) -> Self {
        let started = Timestamp::now();
        let (sender, mut messages) = futures::channel::mpsc::unbounded();
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
            let mut session = match thread_agent.start(&prompt, &dir) {
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
        let agent = gitez_agent::agent(&header.agent);
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
