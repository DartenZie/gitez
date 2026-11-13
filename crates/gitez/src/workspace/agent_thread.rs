//! One agent thread: what it was asked, what its agent did and said, its
//! branch and pull request, and what can be done with it next (stop, retry,
//! reply, complete). Shown in the detail panel of a repository window and,
//! wide, in Home; laid out as in the `C-3-Session` and `C-6-Thread` boards.

pub mod activity;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use gitez_agent::AgentEvent;
use gitez_agent_thread::{AgentThread, Ending, State, Store, Timestamp, TurnKind};
use gitez_git::{DiffOptions, Oid};
use gitez_sandbox::NetworkProfile;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::text::TextView;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::labels;
use crate::agent_threads::{AgentThreadKey, AgentThreads, AgentThreadsEvent};
use crate::theme::{self, color};
use activity::{Entry, Row, TurnEvents};

/// What the thread view asks of the window it is in.
#[allow(dead_code, reason = "for the agent thread screens still to come")]
pub enum ThreadViewEvent {
    OpenPullRequest {
        common_dir: PathBuf,
        number: u32,
    },
    /// Home: open the thread's repository.
    OpenRepository {
        work_dir: PathBuf,
    },
}

/// What the view is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    Stopping,
    Completing,
    Replying,
    Retrying,
}

/// The transcript of one turn, as read so far.
#[derive(Default)]
struct RunEvents {
    file: String,
    events: Vec<AgentEvent>,
    /// Where to read on from.
    offset: u64,
    /// Read to the end of a turn that ended.
    done: bool,
}

/// Read from the repository for the view: who the user is, and what the
/// thread's branch holds.
#[derive(Default)]
struct Details {
    user_name: Option<String>,
    pull_request: Option<BranchStats>,
}

struct BranchStats {
    commits: usize,
    files: usize,
    added: usize,
    removed: usize,
}

/// One agent thread, followed as it changes.
pub struct ThreadView {
    key: AgentThreadKey,
    /// Home's main area rather than the detail panel.
    wide: bool,
    model: Entity<AgentThreads>,
    reply: Entity<TextareaState>,
    /// The network profile picked for a retry; the thread's own by default.
    retry_network: Option<NetworkProfile>,
    /// Whether the activity is shown; by default while the thread runs.
    activity_open: Option<bool>,
    /// Collapsed runs of steps the user expanded, by their first entry.
    expanded: HashSet<usize>,
    /// By turn, aligned with the thread's turns.
    runs: Vec<Option<RunEvents>>,
    entries: Vec<Entry>,
    reading: Option<Task<()>>,
    /// The transcripts changed while being read.
    read_again: bool,
    transcript_error: Option<SharedString>,
    details: Details,
    /// The `updated` time of the thread the details were read for.
    details_read: Option<Timestamp>,
    _details_task: Option<Task<()>>,
    /// The `updated` time of the thread last seen.
    seen: Option<Timestamp>,
    busy: Option<(Busy, Task<()>)>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ThreadViewEvent> for ThreadView {}

impl ThreadView {
    pub fn new(
        key: AgentThreadKey,
        wide: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let model = AgentThreads::global(cx);
        let reply = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 10)
                .placeholder("Reply to continue this thread")
        });
        let subscriptions = vec![
            cx.observe(&model, |this, _, cx| this.thread_changed(cx)),
            cx.subscribe(&model, |this, _, event, cx| {
                if let AgentThreadsEvent::Transcript(key) = event
                    && *key == this.key
                {
                    this.read_transcripts(cx);
                }
            }),
            cx.subscribe_in(&reply, window, |this, _, event, window, cx| match event {
                InputEvent::PressEnter {
                    secondary: true, ..
                } => this.send_reply(window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            }),
        ];
        let mut view = Self {
            key,
            wide,
            model,
            reply,
            retry_network: None,
            activity_open: None,
            expanded: HashSet::new(),
            runs: Vec::new(),
            entries: Vec::new(),
            reading: None,
            read_again: false,
            transcript_error: None,
            details: Details::default(),
            details_read: None,
            _details_task: None,
            seen: None,
            busy: None,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        view.thread_changed(cx);
        view
    }

    pub fn key(&self) -> &AgentThreadKey {
        &self.key
    }

    fn thread(&self, cx: &App) -> Option<Arc<AgentThread>> {
        self.model.read(cx).thread(&self.key).cloned()
    }

    /// Follows a change of the model: reads what the thread's new state
    /// needs.
    fn thread_changed(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.thread(cx) else {
            cx.notify();
            return;
        };
        if self.seen != Some(thread.updated) {
            self.seen = Some(thread.updated);
            if self.activity_shown(&thread) {
                self.read_transcripts(cx);
            }
            // The user's name is needed at once; commits land when a turn
            // ends, so there is nothing new to read while one runs.
            if self.details_read.is_none()
                || (!thread.state.is_active() && self.details_read != Some(thread.updated))
            {
                self.read_details(&thread, cx);
            }
        }
        cx.notify();
    }

    fn activity_shown(&self, thread: &AgentThread) -> bool {
        self.activity_open.unwrap_or(thread.state.is_active())
    }

    fn toggle_activity(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.thread(cx) else {
            return;
        };
        let open = !self.activity_shown(&thread);
        self.activity_open = Some(open);
        if open {
            self.read_transcripts(cx);
        }
        cx.notify();
    }

    /// Reads what was added to the thread's transcripts, in the
    /// background; again afterwards if they changed meanwhile.
    fn read_transcripts(&mut self, cx: &mut Context<Self>) {
        if self.reading.is_some() {
            self.read_again = true;
            return;
        }
        let Some(thread) = self.thread(cx) else {
            return;
        };
        let Some(agent) = self.model.read(cx).agents().get(&thread.agent) else {
            self.transcript_error =
                Some(format!("GitEZ does not know the agent {}.", thread.agent).into());
            return;
        };
        let runs_dir = Store::new(&self.key.common_dir)
            .thread_dir(&self.key.id)
            .join("runs");
        // What to read: each turn's transcript from where it was left.
        let mut wanted = Vec::new();
        for (index, turn) in thread.turns.iter().enumerate() {
            let Some(file) = &turn.transcript else {
                continue;
            };
            let known = self
                .runs
                .get(index)
                .and_then(Option::as_ref)
                .filter(|run| &run.file == file);
            if known.is_some_and(|run| run.done) {
                continue;
            }
            let offset = known.map_or(0, |run| run.offset);
            wanted.push((index, file.clone(), offset, turn.ended.is_some()));
        }
        if wanted.is_empty() {
            self.rebuild_entries(&thread);
            return;
        }
        let store = Store::new(&self.key.common_dir);
        let read = cx.background_executor().spawn(async move {
            wanted
                .into_iter()
                .map(|(index, file, offset, ended)| {
                    let read = store.read_run_from(&runs_dir.join(&file), offset).map(
                        |(lines, offset)| {
                            let events: Vec<AgentEvent> =
                                lines.iter().filter_map(|line| agent.parse(line)).collect();
                            (events, offset)
                        },
                    );
                    (index, file, offset, ended, read)
                })
                .collect::<Vec<_>>()
        });
        self.reading = Some(cx.spawn(async move |this, cx| {
            let read = read.await;
            this.update(cx, |this, cx| {
                this.reading = None;
                this.transcript_error = None;
                for (index, file, from, ended, read) in read {
                    if this.runs.len() <= index {
                        this.runs.resize_with(index + 1, || None);
                    }
                    let run = this.runs[index].get_or_insert_with(RunEvents::default);
                    if run.file != file || run.offset != from {
                        *run = RunEvents {
                            file,
                            ..RunEvents::default()
                        };
                        if from != 0 {
                            // Read from elsewhere meanwhile; start over.
                            this.read_again = true;
                            continue;
                        }
                    }
                    match read {
                        Ok((events, offset)) => {
                            run.events.extend(events);
                            run.offset = offset;
                            run.done = ended;
                        }
                        Err(error) => {
                            this.transcript_error = Some(
                                format!(
                                    "Could not read the transcript: {:#}",
                                    anyhow::Error::from(error)
                                )
                                .into(),
                            );
                        }
                    }
                }
                if let Some(thread) = this.thread(cx) {
                    this.rebuild_entries(&thread);
                }
                if std::mem::take(&mut this.read_again) {
                    this.read_transcripts(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn rebuild_entries(&mut self, thread: &AgentThread) {
        let turns: Vec<TurnEvents<'_>> = thread
            .turns
            .iter()
            .enumerate()
            .map(|(index, turn)| TurnEvents {
                kind: turn.kind,
                prompt: &turn.prompt,
                events: self
                    .runs
                    .get(index)
                    .and_then(Option::as_ref)
                    .map_or(&[][..], |run| &run.events[..]),
            })
            .collect();
        self.entries = activity::entries(&turns);
    }

    /// Reads the user's name and the branch's commits and line counts in
    /// the background.
    fn read_details(&mut self, thread: &AgentThread, cx: &mut Context<Self>) {
        self.details_read = Some(thread.updated);
        let work_dir = self
            .model
            .read(cx)
            .repository(&self.key.common_dir)
            .map(|repository| repository.work_dir.clone());
        let (base, branch) = (thread.start.commit.clone(), thread.branch.clone());
        let with_branch = thread.pull_request.is_some();
        let read = cx.background_executor().spawn(async move {
            let Some(repository) = work_dir.and_then(|dir| gitez_git::Repository::open(dir).ok())
            else {
                return Details::default();
            };
            let pull_request = with_branch
                .then(|| branch_stats(&repository, &base, &branch))
                .flatten();
            Details {
                user_name: repository.user_name(),
                pull_request,
            }
        });
        self._details_task = Some(cx.spawn(async move |this, cx| {
            let details = read.await;
            this.update(cx, |this, cx| {
                this.details = details;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Runs `action` on the model, showing `busy` until it ends and its
    /// error if it fails.
    fn run(
        &mut self,
        busy: Busy,
        window: &mut Window,
        cx: &mut Context<Self>,
        action: impl FnOnce(
            &mut AgentThreads,
            &AgentThreadKey,
            &mut Context<AgentThreads>,
        ) -> Task<anyhow::Result<()>>,
        done: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if self.busy.is_some() {
            return;
        }
        let key = self.key.clone();
        let task = self.model.update(cx, |model, cx| action(model, &key, cx));
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(()) => done(this, window, cx),
                    Err(error) => {
                        window.push_notification(crate::toast::error(format!("{error:#}")), cx)
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self.busy = Some((busy, task));
        cx.notify();
    }

    fn stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run(
            Busy::Stopping,
            window,
            cx,
            |model, key, cx| model.stop(key, cx),
            |_, _, _| {},
        );
    }

    fn complete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run(
            Busy::Completing,
            window,
            cx,
            |model, key, cx| model.complete(key, cx),
            |_, window, cx| {
                window.push_notification(
                    crate::toast::success("Completed; the worktree is removed"),
                    cx,
                )
            },
        );
    }

    fn retry(&mut self, network: NetworkProfile, window: &mut Window, cx: &mut Context<Self>) {
        self.run(
            Busy::Retrying,
            window,
            cx,
            move |model, key, cx| model.retry(key, network, cx),
            |this, _, _| this.retry_network = None,
        );
    }

    fn send_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.thread(cx) else {
            return;
        };
        if !can_reply(&thread) {
            return;
        }
        let text = self.reply.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.activity_open = None;
        self.run(
            Busy::Replying,
            window,
            cx,
            move |model, key, cx| model.reply(key, TurnKind::Reply, text, cx),
            |this, window, cx| {
                this.reply
                    .update(cx, |reply, cx| reply.set_value("", window, cx));
            },
        );
    }

    /// Shows the newest transcript in Finder or the file manager.
    fn open_transcript(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.thread(cx).and_then(|thread| self.newest_run(&thread)) {
            cx.reveal_path(&path);
        }
    }

    fn newest_run(&self, thread: &AgentThread) -> Option<PathBuf> {
        let file = thread
            .turns
            .iter()
            .rev()
            .find_map(|turn| turn.transcript.as_ref())?;
        Some(
            Store::new(&self.key.common_dir)
                .thread_dir(&self.key.id)
                .join("runs")
                .join(file),
        )
    }
}

/// The commits and line counts of `branch` since `base`, if both are there.
fn branch_stats(
    repository: &gitez_git::Repository,
    base: &str,
    branch: &str,
) -> Option<BranchStats> {
    let base = Oid::from_hex(base.as_bytes()).ok()?;
    let head = repository
        .resolve_commit(&format!("refs/heads/{branch}"))
        .ok()??;
    let diff = repository
        .branch_diff(base, head, DiffOptions::default())
        .ok()?;
    let (added, removed) = diff
        .files
        .iter()
        .filter_map(|file| file.line_counts())
        .fold((0, 0), |(added, removed), counts| {
            (added + counts.added, removed + counts.removed)
        });
    Some(BranchStats {
        commits: diff.ahead,
        files: diff.files.len(),
        added,
        removed,
    })
}

/// Whether a reply can continue `thread`'s conversation now.
fn can_reply(thread: &AgentThread) -> bool {
    thread.state == State::Finished && thread.session_id.is_some()
}

/// What `thread` is doing, or how it ended: "Running", "Waiting until
/// 14:30", "Ready for review", "No changes", "Failed".
pub fn state_label(thread: &AgentThread) -> String {
    match thread.state {
        State::Queued => "Queued".to_owned(),
        State::Preparing => "Preparing".to_owned(),
        State::Running => "Running".to_owned(),
        State::Waiting => match thread.waiting_until {
            Some(until) => format!("Waiting until {}", local_time(until)),
            None => "Waiting".to_owned(),
        },
        State::Finished => ending_label(thread.ending).to_owned(),
        State::Completed => "Completed".to_owned(),
    }
}

fn ending_label(ending: Option<Ending>) -> &'static str {
    match ending {
        Some(Ending::PullRequest) => "Ready for review",
        Some(Ending::Answer) | None => "No changes",
        Some(Ending::Failed) => "Failed",
        Some(Ending::Stopped) => "Stopped",
    }
}

/// The chip showing [`state_label`] in the state's colours.
pub fn state_chip(thread: &AgentThread) -> Div {
    let (background, text) = match (thread.state, thread.ending) {
        (State::Running, _) => (rgba(color::THREAD_RUNNING), color::THREAD_RUNNING_TEXT),
        (State::Preparing, _) | (State::Finished, Some(Ending::PullRequest)) => {
            (rgba(color::THREAD_PREPARING), color::THREAD_PREPARING_TEXT)
        }
        (State::Finished, Some(Ending::Failed | Ending::Stopped)) => {
            (rgba(color::THREAD_FAILED), color::THREAD_FAILED_TEXT)
        }
        _ => (rgb(color::THREAD_QUEUED), color::THREAD_QUEUED_TEXT),
    };
    let mark = if thread.state == State::Completed {
        Icon::new(IconName::Check).size(px(11.)).into_any_element()
    } else {
        div()
            .flex_none()
            .size(px(6.))
            .rounded_full()
            .bg(rgb(text))
            .into_any_element()
    };
    div()
        .h_flex()
        .flex_none()
        .gap(px(5.))
        .h(px(18.))
        .px(px(7.))
        .rounded(px(9.))
        .bg(background)
        .text_color(rgb(text))
        .text_size(px(11.))
        .font_weight(FontWeight::MEDIUM)
        .whitespace_nowrap()
        .child(mark)
        .child(state_label(thread))
}

/// `time` as hours and minutes in the local time zone.
fn local_time(time: Timestamp) -> String {
    time.to_zoned(jiff::tz::TimeZone::system())
        .strftime("%H:%M")
        .to_string()
}

/// `time` for [`crate::time::relative`], in the local time zone's offset.
fn git_time(time: Timestamp) -> gitez_git::Timestamp {
    let offset = jiff::tz::TimeZone::system().to_offset(time);
    gitez_git::Timestamp {
        seconds: time.as_second(),
        offset: offset.seconds(),
    }
}

/// "14 min", "2 h 5 min".
fn duration(seconds: i64) -> String {
    let minutes = (seconds.max(0) + 30) / 60;
    match minutes {
        0 => "under a minute".to_owned(),
        1..60 => format!("{minutes} min"),
        _ if minutes % 60 == 0 => format!("{} h", minutes / 60),
        _ => format!("{} h {} min", minutes / 60, minutes % 60),
    }
}

/// What the thread cost so far, if its agent said.
fn cost(thread: &AgentThread) -> Option<f64> {
    let costs: Vec<f64> = thread
        .turns
        .iter()
        .filter_map(|turn| turn.result.as_ref()?.cost_usd)
        .collect();
    (!costs.is_empty()).then(|| costs.iter().sum())
}

/// "14 min · 31 steps · $0.92" while active; "Finished 1 h ago · 26 min run
/// · $1.84" after.
fn summary(thread: &AgentThread, now: Timestamp) -> String {
    let run = duration(thread.run_time(now).as_secs());
    let cost = cost(thread).map(|cost| format!("${cost:.2}"));
    let steps = match thread.steps() {
        1 => "1 step".to_owned(),
        steps => format!("{steps} steps"),
    };
    let mut parts = match thread.state {
        State::Queued => vec!["Waits for a free slot".to_owned()],
        State::Preparing | State::Running | State::Waiting => vec![run, steps],
        State::Finished | State::Completed => {
            let ended = thread
                .turns
                .iter()
                .rev()
                .find_map(|turn| turn.ended)
                .unwrap_or(thread.updated);
            let when = crate::time::relative(git_time(ended), now.as_second());
            let mut parts = Vec::new();
            if thread.state == State::Completed {
                parts.push(ending_label(thread.ending).to_owned());
                parts.push(format!("finished {when}"));
            } else {
                parts.push(format!("Finished {when}"));
            }
            parts.push(format!("{run} run"));
            parts
        }
    };
    parts.extend(cost);
    parts.join(" · ")
}

/// The square with an agent's mark.
fn agent_mark(agent: &str) -> Div {
    let mark = if agent == gitez_agent::ClaudeCode::ID {
        svg()
            .path("agents/claude.svg")
            .size(px(12.))
            .text_color(rgb(color::ACCENT))
            .into_any_element()
    } else {
        Icon::new(IconName::Bot)
            .size(px(12.))
            .text_color(rgb(color::ACCENT))
            .into_any_element()
    };
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(20.))
        .rounded(px(5.))
        .bg(rgb(color::AGENT_MARK_BACKGROUND))
        .child(mark)
}

/// A branch name in its colour; cut with an ellipsis where space is short.
fn branch_chip(name: &str, small: bool) -> Div {
    let branch_color = theme::branch_color(name);
    div()
        .flex_shrink(1.)
        .min_w_0()
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap()
        .h(px(if small { 18. } else { 20. }))
        .px(px(if small { 6. } else { 7. }))
        .rounded(px(4.))
        .bg(theme::with_alpha(branch_color, 0.16))
        .text_color(rgb(branch_color))
        .text_size(px(if small { 11. } else { 11.5 }))
        .font_weight(FontWeight::MEDIUM)
        .line_height(px(if small { 18. } else { 20. }))
        .child(name.to_owned())
}

/// A keycap on an amber button: dark, as the button's label.
fn on_accent(key_cap: Div) -> Div {
    key_cap
        .border_color(theme::with_alpha(color::ON_ACCENT, 0.4))
        .text_color(rgb(color::ON_ACCENT))
}

/// The heading over a message: who wrote it, and what it is.
fn who(avatar: impl IntoElement, name: impl Into<SharedString>, what: &'static str) -> Div {
    div()
        .h_flex()
        .gap(px(8.))
        .text_size(px(12.))
        .child(avatar)
        .child(div().font_weight(FontWeight::MEDIUM).child(name.into()))
        .child(div().text_color(rgb(color::TEXT_MUTED)).child(what))
}

/// A message the user sent, in a bubble.
fn bubble(text: String, indent: bool) -> Div {
    div()
        .when(indent, |this| this.ml(px(28.)))
        .px(px(14.))
        .py(px(10.))
        .bg(rgb(color::PANEL))
        .border_1()
        .border_color(rgb(color::BORDER))
        .rounded(px(8.))
        .text_size(px(13.))
        .line_height(relative(1.6))
        .child(text)
}

impl ThreadView {
    fn render_header(
        &self,
        thread: &AgentThread,
        agent_name: SharedString,
        cx: &mut Context<Self>,
    ) -> Div {
        let busy = self.busy.as_ref().map(|(busy, _)| *busy);
        let active = thread.state.is_active();
        let can_complete = thread.state == State::Finished && thread.pull_request.is_none();
        let has_transcript = self.newest_run(thread).is_some();
        let summary = div()
            .min_w_0()
            .text_size(px(12.))
            .text_color(rgb(color::TEXT_MUTED))
            .child(summary(thread, Timestamp::now()));
        let (inline, below) = if self.wide {
            (Some(summary), None)
        } else {
            (None, Some(summary))
        };
        // The detail panel is too narrow for the summary and the buttons on
        // one line: the summary goes under it, and the transcript button
        // shows only its icon.
        let transcript = Button::new("thread-transcript")
            .ghost()
            .h(px(28.))
            .compact()
            .text_color(rgb(color::TEXT_SECONDARY))
            .on_click(cx.listener(|this, _, _, cx| this.open_transcript(cx)));
        let transcript = if self.wide {
            transcript
                .label("Open transcript")
                .tooltip("Show the agent's transcript in Finder")
        } else {
            transcript
                .icon(Icon::new(IconName::File).size(px(13.)))
                .tooltip("Open transcript: show it in Finder")
        };
        let row = div()
            .h_flex()
            .flex_wrap()
            .gap(px(8.))
            .text_size(px(12.))
            .child(agent_mark(&thread.agent))
            .child(
                div()
                    .text_color(rgb(color::TEXT_SECONDARY))
                    .child(agent_name),
            )
            .child(state_chip(thread))
            .children(inline)
            .child(
                div()
                    .h_flex()
                    .ml_auto()
                    .gap(px(4.))
                    .when(active, |this| {
                        let label = if thread.state == State::Queued {
                            "Cancel"
                        } else {
                            "Stop"
                        };
                        this.child(
                            Button::new("thread-stop")
                                .h(px(28.))
                                .compact()
                                .icon(Icon::new(IconName::Square).size(px(12.)))
                                .label(label)
                                .text_color(rgb(color::THREAD_FAILED_TEXT))
                                .loading(busy == Some(Busy::Stopping))
                                .disabled(busy.is_some())
                                .on_click(cx.listener(|this, _, window, cx| this.stop(window, cx))),
                        )
                    })
                    .when(can_complete, |this| {
                        this.child(
                            Button::new("thread-complete")
                                .h(px(28.))
                                .compact()
                                .icon(Icon::new(IconName::Check).size(px(13.)))
                                .label("Complete")
                                .tooltip("Removes the worktree. The thread stays readable.")
                                .loading(busy == Some(Busy::Completing))
                                .disabled(busy.is_some())
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.complete(window, cx)),
                                ),
                        )
                    })
                    .when(has_transcript, |this| this.child(transcript)),
            );
        div().v_flex().gap(px(4.)).child(row).children(below)
    }

    fn render_facts(&self, thread: &AgentThread, cx: &mut Context<Self>) -> Div {
        let mono = cx.theme().mono_font_family.clone();
        let key_width = px(if self.wide { 84. } else { 72. });
        let row = |label: &'static str, value: AnyElement| {
            div()
                .h_flex()
                .gap(px(10.))
                .min_h(px(20.))
                .text_size(px(12.))
                .child(
                    div()
                        .flex_none()
                        .w(key_width)
                        .text_color(rgb(color::TEXT_MUTED))
                        .child(label),
                )
                .child(div().flex_1().min_w_0().child(value))
        };
        let short_sha: String = thread.start.commit.chars().take(7).collect();
        let from = |this: Div| {
            // The base branch is short as a rule; the thread's own gives way.
            this.child(branch_chip(&thread.start.branch, false).flex_none())
                .child(
                    div()
                        .flex_none()
                        .font_family(mono.clone())
                        .text_size(px(11.))
                        .text_color(rgb(color::TEXT_MUTED))
                        .child(short_sha.clone()),
                )
        };
        let mut rows = div().v_flex().gap(px(8.));
        if self.wide
            && let Some(repository) = self.model.read(cx).repository(&self.key.common_dir)
        {
            let work_dir = repository.work_dir.clone();
            rows = rows.child(row(
                "Repository",
                div()
                    .h_flex()
                    .gap(px(8.))
                    .min_w_0()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child(repository.name.clone()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .font_family(mono.clone())
                            .text_size(px(11.5))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(crate::recent::display(&work_dir)),
                    )
                    .child(
                        Button::new("thread-open-repository")
                            .link()
                            .label(format!("Open {}", repository.name))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(ThreadViewEvent::OpenRepository {
                                    work_dir: work_dir.clone(),
                                })
                            })),
                    )
                    .into_any_element(),
            ));
        }
        if thread.ending == Some(Ending::Answer) {
            rows = rows.child(row(
                "Started from",
                from(div().h_flex().gap(px(6.)).min_w_0()).into_any_element(),
            ));
        } else {
            rows = rows.child(row(
                "Branch",
                from(
                    div()
                        .h_flex()
                        .gap(px(6.))
                        .min_w_0()
                        .child(branch_chip(&thread.branch, false))
                        .child(
                            div()
                                .flex_none()
                                .text_color(rgb(color::TEXT_MUTED))
                                .child("from"),
                        ),
                )
                .into_any_element(),
            ));
        }
        if thread.state != State::Completed {
            let worktree = thread.worktree.clone();
            rows = rows.child(row(
                "Worktree",
                div()
                    .h_flex()
                    .gap(px(4.))
                    .min_w_0()
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .font_family(mono.clone())
                            .text_size(px(11.5))
                            .text_color(rgb(color::TEXT_SECONDARY))
                            .child(crate::recent::display(&worktree)),
                    )
                    .when(worktree.exists(), |this| {
                        this.child(
                            Button::new("thread-reveal-worktree")
                                .ghost()
                                .xsmall()
                                .icon(IconName::FolderOpen)
                                .tooltip("Show in Finder")
                                .on_click(move |_, _, cx| cx.reveal_path(&worktree)),
                        )
                    })
                    .into_any_element(),
            ));
        }
        rows
    }

    /// Sandbox profile, secrets and blocked requests.
    fn render_strip(&self, thread: &AgentThread, cx: &App) -> Div {
        let mono = cx.theme().mono_font_family.clone();
        let blocked = thread
            .turns
            .iter()
            .rev()
            .find_map(|turn| turn.result.as_ref())
            .map_or(0, |result| result.blocked_hosts.len());
        div()
            .h_flex()
            .flex_wrap()
            .gap_x(px(14.))
            .gap_y(px(6.))
            .py(px(10.))
            // In the panel, the section above draws the line over it.
            .when(!self.wide, |this| this.px(px(18.)))
            .when(self.wide, |this| this.border_t_1())
            .border_b_1()
            .border_color(rgb(color::ROW_DIVIDER))
            .text_size(px(12.))
            .text_color(rgb(color::TEXT_SECONDARY))
            .child(
                div()
                    .h_flex()
                    .gap(px(6.))
                    .child(
                        Icon::new(IconName::ShieldCheck)
                            .size(px(13.))
                            .text_color(rgb(color::TEXT_MUTED)),
                    )
                    .child(div().text_color(rgb(color::TEXT_MUTED)).child("Sandbox"))
                    .child(thread.network.label()),
            )
            .when(!thread.secrets.is_empty(), |this| {
                this.child(div().h_flex().flex_wrap().gap(px(6.)).children(
                    thread.secrets.iter().map(|name| {
                        div()
                            .px(px(5.))
                            .rounded(px(4.))
                            .border_1()
                            .border_color(rgb(color::TOOL_CHIP_BORDER))
                            .bg(rgb(color::TOOL_CHIP))
                            .font_family(mono.clone())
                            .text_size(px(11.))
                            .child(name.clone())
                    }),
                ))
            })
            .child(match blocked {
                0 => div()
                    .text_color(rgb(color::TEXT_MUTED))
                    .child("No blocked requests"),
                count => div()
                    .text_color(rgb(color::THREAD_FAILED_TEXT))
                    .child(if count == 1 {
                        "1 blocked request".to_owned()
                    } else {
                        format!("{count} blocked requests")
                    }),
            })
    }

    fn render_pull_request(
        &self,
        thread: &AgentThread,
        number: u32,
        cx: &mut Context<Self>,
    ) -> Div {
        let mono = cx.theme().mono_font_family.clone();
        let model = self.model.read(cx);
        let label = match model.repository(&self.key.common_dir) {
            Some(repository) if self.wide => format!("Open PR #{number} in {}", repository.name),
            _ => format!("Review PR #{number}"),
        };
        let stats = self.details.pull_request.as_ref().map(|stats| {
            let plural = |count: usize, what: &str| {
                if count == 1 {
                    format!("1 {what}")
                } else {
                    format!("{count} {what}s")
                }
            };
            div()
                .h_flex()
                .gap(px(6.))
                .child(div().text_color(rgb(color::TEXT_MUTED)).child(format!(
                    "· {} · {}",
                    plural(stats.commits, "commit"),
                    plural(stats.files, "file")
                )))
                .child(
                    div()
                        .font_family(mono.clone())
                        .text_color(rgb(color::DIFF_ADDED))
                        .child(format!("+{}", stats.added)),
                )
                .child(
                    div()
                        .font_family(mono.clone())
                        .text_color(rgb(color::DIFF_REMOVED))
                        .child(format!("−{}", stats.removed)),
                )
        });
        let common_dir = self.key.common_dir.clone();
        let button = Button::new("thread-open-pull-request")
            .h(px(34.))
            .px(px(14.))
            .primary()
            .when(!self.wide, |this| this.w_full())
            .label(label)
            .icon(Icon::new(IconName::ArrowRight).size(px(14.)))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(ThreadViewEvent::OpenPullRequest {
                    common_dir: common_dir.clone(),
                    number,
                })
            }));
        div()
            .debug_selector(|| "thread-pull-request".into())
            .flex()
            .when(self.wide, |this| this.flex_row().items_center())
            .when(!self.wide, |this| this.flex_col())
            .gap(px(if self.wide { 16. } else { 12. }))
            .px(px(if self.wide { 16. } else { 14. }))
            .py(px(if self.wide { 14. } else { 12. }))
            .bg(rgb(color::DIFF_CARD))
            .border_1()
            .border_color(rgb(color::DIFF_CARD_BORDER))
            .rounded(px(8.))
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(8.))
                    .child(
                        div()
                            .h_flex()
                            .gap(px(8.))
                            .child(
                                Icon::new(IconName::GitPullRequestArrow)
                                    .size(px(14.))
                                    .text_color(rgb(color::THREAD_PREPARING_TEXT)),
                            )
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(format!("Local pull request #{number}")),
                            )
                            // In the panel the chip goes to the right edge;
                            // wide, it follows the title.
                            .child(
                                div()
                                    .when(!self.wide, |this| this.ml_auto())
                                    .child(state_chip(thread)),
                            ),
                    )
                    .child(
                        div()
                            .h_flex()
                            .flex_wrap()
                            .gap(px(6.))
                            .text_size(px(12.))
                            .child(branch_chip(&thread.branch, true))
                            .child(
                                Icon::new(IconName::ArrowRight)
                                    .size(px(12.))
                                    .text_color(rgb(color::TEXT_MUTED)),
                            )
                            .child(branch_chip(&thread.start.branch, true))
                            .children(stats),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .line_height(relative(1.5))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(
                                "Review comments go back to this thread and run in its worktree.",
                            ),
                    ),
            )
            .when(thread.state != State::Completed, |this| this.child(button))
    }

    fn render_prompt(&self, thread: &AgentThread) -> Option<Div> {
        let prompt = thread.turns.first()?.prompt.clone();
        let name = self
            .details
            .user_name
            .clone()
            .unwrap_or_else(|| "You".to_owned());
        Some(
            div()
                .v_flex()
                .gap(px(8.))
                .child(who(
                    labels::avatar(&name, px(20.)).into_any_element(),
                    name,
                    "Prompt",
                ))
                .child(bubble(prompt, true)),
        )
    }

    fn render_activity(&self, thread: &AgentThread, cx: &mut Context<Self>) -> Div {
        let open = self.activity_shown(thread);
        let live = thread.state == State::Running
            && thread.turns.last().is_some_and(|turn| turn.ended.is_none());
        let minutes = duration(thread.run_time(Timestamp::now()).as_secs());
        let steps = match thread.steps() {
            1 => "1 step".to_owned(),
            steps => format!("{steps} steps"),
        };
        let toggle = div()
            .id("thread-activity")
            .h_flex()
            .gap(px(8.))
            .px(px(12.))
            .py(px(9.))
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(rgb(color::TEXT_MUTED))
            .hover(|style| style.text_color(rgb(color::TEXT)))
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(px(13.)),
            )
            .child(format!("Activity · {steps} · {minutes}"))
            .when(live, |this| {
                this.child(
                    div()
                        .h_flex()
                        .gap(px(5.))
                        .text_size(px(11.5))
                        .text_color(rgb(color::THREAD_RUNNING_TEXT))
                        .child(
                            div()
                                .size(px(6.))
                                .rounded_full()
                                .bg(rgb(color::BRANCH_BLUE)),
                        )
                        .child("live"),
                )
            })
            .on_click(cx.listener(|this, _, _, cx| this.toggle_activity(cx)));
        let mut log = div()
            .v_flex()
            .when(self.wide, |this| this.ml(px(28.)))
            .bg(rgb(color::PROMPT))
            .border_1()
            .border_color(rgb(color::DIFF_CARD_BORDER))
            .rounded(px(8.))
            .child(toggle);
        if !open {
            return log;
        }
        let rows = activity::rows(&self.entries, &self.expanded, live);
        let body = div()
            .v_flex()
            .py(px(4.))
            .border_t_1()
            .border_color(rgb(color::DIFF_CARD_BORDER));
        let body = if let Some(error) = &self.transcript_error {
            body.child(
                div()
                    .px(px(12.))
                    .py(px(6.))
                    .text_size(px(12.))
                    .text_color(rgb(color::THREAD_FAILED_TEXT))
                    .child(error.clone()),
            )
        } else if rows.is_empty() {
            body.child(
                div()
                    .px(px(12.))
                    .py(px(6.))
                    .text_size(px(12.))
                    .text_color(rgb(color::TEXT_MUTED))
                    .child(if self.reading.is_some() {
                        "Reading the transcript…"
                    } else if thread.state.is_active() {
                        "No steps yet"
                    } else {
                        "The agent took no steps"
                    }),
            )
        } else {
            let user = self.details.user_name.clone();
            body.children(
                rows.into_iter()
                    .enumerate()
                    .map(|(index, row)| self.render_row(index, row, user.as_deref(), cx)),
            )
        };
        log = log.child(body);
        log
    }

    fn render_row(
        &self,
        index: usize,
        row: Row<'_>,
        user: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mono = cx.theme().mono_font_family.clone();
        match row {
            // Agents write Markdown: `code` and **bold** in their steps too.
            Row::Text(text) => div()
                .pl(px(60.))
                .pr(px(12.))
                .py(px(6.))
                .text_size(px(12.5))
                .line_height(relative(1.5))
                .text_color(rgb(color::TEXT_SECONDARY))
                .child(TextView::markdown(
                    SharedString::from(format!("thread-step-{index}")),
                    text.to_owned(),
                ))
                .into_any_element(),
            Row::Tool {
                name,
                detail,
                failure,
                live,
            } => div()
                .v_flex()
                .child(
                    div()
                        .h_flex()
                        .gap(px(8.))
                        .min_h(px(26.))
                        .px(px(10.))
                        .when(live, |this| this.bg(rgba(color::LIVE_ROW)))
                        .font_family(mono)
                        .text_size(px(11.))
                        .child(
                            div()
                                .flex_none()
                                .min_w(px(42.))
                                .text_color(rgb(if live {
                                    color::THREAD_RUNNING_TEXT
                                } else {
                                    color::TEXT_SECONDARY
                                }))
                                .child(name.to_owned()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_color(rgb(if live {
                                    color::TEXT
                                } else {
                                    color::AUTHOR_TEXT
                                }))
                                .child(detail.to_owned()),
                        )
                        .child(
                            Icon::new(if live {
                                IconName::LoaderCircle
                            } else if failure.is_some() {
                                IconName::CircleAlert
                            } else {
                                IconName::Check
                            })
                            .size(px(12.))
                            .text_color(rgb(if live {
                                color::THREAD_RUNNING_TEXT
                            } else if failure.is_some() {
                                color::THREAD_FAILED_TEXT
                            } else {
                                color::TEXT_FAINT
                            })),
                        ),
                )
                .when_some(failure, |this, failure| {
                    this.child(
                        div()
                            .pl(px(60.))
                            .pr(px(12.))
                            .pb(px(8.))
                            .pt(px(2.))
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_size(px(11.))
                            .line_height(relative(1.55))
                            .text_color(rgb(color::THREAD_FAILED_TEXT))
                            .child(failure.to_owned()),
                    )
                })
                .into_any_element(),
            Row::More {
                group,
                count,
                names,
            } => div()
                .id(("thread-more", index))
                .h_flex()
                .justify_center()
                .gap(px(6.))
                .h(px(28.))
                .cursor_pointer()
                .border_t_1()
                .border_b_1()
                .border_color(rgb(color::ROW_BORDER))
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_MUTED))
                .hover(|style| style.text_color(rgb(color::TEXT)))
                .child(format!("Show {count} more steps · {names}"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.expanded.insert(group);
                    cx.notify();
                }))
                .into_any_element(),
            Row::Prompt { kind, text } => {
                let name = user.unwrap_or("You").to_owned();
                let what = match kind {
                    TurnKind::Review => "Review comments",
                    _ => "Reply",
                };
                div()
                    .v_flex()
                    .gap(px(6.))
                    .px(px(12.))
                    .py(px(8.))
                    .child(who(
                        labels::avatar(&name, px(20.)).into_any_element(),
                        name,
                        what,
                    ))
                    .child(bubble(text.to_owned(), false))
                    .into_any_element()
            }
            Row::Continued(kind) => div()
                .h_flex()
                .gap(px(6.))
                .px(px(12.))
                .py(px(6.))
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_MUTED))
                .child(Icon::new(IconName::RotateCcw).size(px(12.)))
                .child(match kind {
                    TurnKind::Resume => "Continued after the usage limit reset",
                    _ => "Retried",
                })
                .into_any_element(),
        }
    }

    fn render_final_message(&self, thread: &AgentThread, agent_name: SharedString) -> Option<Div> {
        if !matches!(thread.state, State::Finished | State::Completed) {
            return None;
        }
        let message = thread.final_message()?.to_owned();
        let what = if thread.ending == Some(Ending::Answer) {
            "Answer"
        } else {
            "Final message"
        };
        Some(
            div()
                .v_flex()
                .gap(px(8.))
                .child(who(agent_mark(&thread.agent), agent_name, what))
                .child(
                    div()
                        .debug_selector(|| "thread-final-message".into())
                        .ml(px(28.))
                        .text_size(px(13.))
                        .line_height(relative(1.6))
                        .child(
                            TextView::markdown(
                                SharedString::from(format!("thread-final-{}", thread.id)),
                                message,
                            )
                            .selectable(true),
                        ),
                ),
        )
    }

    /// Why the thread failed or stopped, and Retry.
    fn render_failure(&self, thread: &AgentThread, cx: &mut Context<Self>) -> Option<Div> {
        let failed = thread.state == State::Finished
            && matches!(thread.ending, Some(Ending::Failed | Ending::Stopped));
        if !failed {
            return None;
        }
        let busy = self.busy.as_ref().map(|(busy, _)| *busy);
        let blocked = thread
            .turns
            .iter()
            .rev()
            .find_map(|turn| turn.result.as_ref())
            .is_some_and(|result| !result.blocked_hosts.is_empty());
        let suggested = if blocked && thread.network == NetworkProfile::AgentOnly {
            NetworkProfile::Packages
        } else {
            thread.network
        };
        let network = self.retry_network.unwrap_or(suggested);
        let label = if network != thread.network && self.retry_network.is_none() {
            format!("Retry with {}", network.label())
        } else {
            "Retry".to_owned()
        };
        let message = match &thread.error {
            Some(error) => error.message.clone(),
            None if thread.ending == Some(Ending::Stopped) => "You stopped this thread.".to_owned(),
            None => "The thread failed.".to_owned(),
        };
        let view = cx.entity().downgrade();
        let picker = Button::new("thread-retry-network")
            .h(px(28.))
            .compact()
            .icon(Icon::new(IconName::ShieldCheck).size(px(13.)))
            .label(network.label())
            .dropdown_caret(true)
            .tooltip("Network for the retry")
            .dropdown_menu(move |mut menu, _, _| {
                for profile in NetworkProfile::ALL {
                    let view = view.clone();
                    menu = menu.item(
                        PopupMenuItem::new(profile.label())
                            .checked(profile == network)
                            .on_click(move |_, _, cx| {
                                view.update(cx, |this, cx| {
                                    this.retry_network = Some(profile);
                                    cx.notify();
                                })
                                .ok();
                            }),
                    );
                }
                menu
            });
        Some(
            div()
                .v_flex()
                .gap(px(10.))
                .child(
                    div()
                        .h_flex()
                        .items_start()
                        .gap(px(8.))
                        .child(
                            Icon::new(IconName::CircleAlert)
                                .size(px(14.))
                                .mt(px(2.))
                                .text_color(rgb(color::THREAD_FAILED_TEXT)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(12.5))
                                .line_height(relative(1.5))
                                .text_color(rgb(color::THREAD_FAILED_TEXT))
                                .child(message),
                        ),
                )
                .child(
                    div().h_flex().gap(px(8.)).child(picker).child(
                        Button::new("thread-retry")
                            .debug_selector(|| "thread-retry".into())
                            .primary()
                            .h(px(28.))
                            .icon(Icon::new(IconName::RotateCcw).size(px(13.)))
                            .label(label)
                            .loading(busy == Some(Busy::Retrying))
                            .disabled(busy.is_some())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.retry(network, window, cx)
                            })),
                    ),
                ),
        )
    }

    fn render_warnings(&self, thread: &AgentThread) -> Option<Div> {
        if thread.warnings.is_empty() {
            return None;
        }
        Some(
            div()
                .v_flex()
                .gap(px(6.))
                .children(thread.warnings.iter().map(|warning| {
                    div()
                        .h_flex()
                        .items_start()
                        .gap(px(8.))
                        .text_size(px(12.))
                        .line_height(relative(1.5))
                        .text_color(rgb(color::WARNING_TEXT))
                        .child(Icon::new(IconName::TriangleAlert).size(px(13.)).mt(px(2.)))
                        .child(div().flex_1().min_w_0().child(warning.clone()))
                })),
        )
    }

    /// The reply box, or why there is none.
    fn render_footer(&self, thread: &AgentThread, cx: &mut Context<Self>) -> Option<Div> {
        let footer = div()
            .flex_none()
            .border_t_1()
            .border_color(rgb(color::BORDER))
            .bg(rgb(color::DIFF_CARD))
            .px(px(if self.wide { 32. } else { 16. }))
            .pt(px(12.))
            .pb(px(14.));
        let inner = div().w_full().max_w(px(796.)).mx_auto();
        if thread.state == State::Completed {
            return Some(
                footer.child(
                    inner
                        .h_flex()
                        .gap(px(8.))
                        .h(px(32.))
                        .text_size(px(12.))
                        .text_color(rgb(color::TEXT_MUTED))
                        .child(Icon::new(IconName::Check).size(px(13.)))
                        .child("Completed threads are read-only"),
                ),
            );
        }
        if !can_reply(thread) {
            return None;
        }
        let busy = self.busy.as_ref().map(|(busy, _)| *busy);
        let empty = self.reply.read(cx).value().trim().is_empty();
        let (continues, highlight) = match thread.pull_request {
            Some(number) => {
                let prefix = "Continues on ";
                let branch = prefix.len()..prefix.len() + thread.branch.len();
                let style = HighlightStyle {
                    color: Some(rgb(theme::branch_color(&thread.branch)).into()),
                    ..HighlightStyle::default()
                };
                (
                    format!(
                        "{prefix}{} · new commits update PR #{number}",
                        thread.branch
                    ),
                    Some((branch, style)),
                )
            }
            None => ("Continues in this thread’s worktree".to_owned(), None),
        };
        let continues = div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_size(px(11.5))
            .text_color(rgb(color::TEXT_MUTED))
            .child(StyledText::new(continues).with_highlights(highlight));
        Some(
            footer.child(
                inner.child(
                    div()
                        .debug_selector(|| "thread-reply".into())
                        .v_flex()
                        .gap(px(6.))
                        .px(px(12.))
                        .py(px(10.))
                        .border_1()
                        .border_color(rgb(color::BUTTON_BORDER))
                        .rounded(px(8.))
                        .bg(rgb(color::PANEL))
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(color::TEXT_SECONDARY))
                                .child("Reply"),
                        )
                        .child(
                            Textarea::new(&self.reply)
                                .appearance(false)
                                .disabled(busy.is_some()),
                        )
                        .child(
                            div().h_flex().gap(px(10.)).child(continues).child(
                                Button::new("thread-send")
                                    .primary()
                                    .h(px(28.))
                                    .label("Send")
                                    .child(
                                        div()
                                            .h_flex()
                                            .gap(px(2.))
                                            .child(on_accent(labels::key_cap(
                                                labels::SECONDARY_KEY,
                                                cx,
                                            )))
                                            .child(on_accent(labels::key_cap("↵", cx))),
                                    )
                                    .loading(busy == Some(Busy::Replying))
                                    .disabled(empty || busy.is_some())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.send_reply(window, cx)
                                    })),
                            ),
                        ),
                ),
            ),
        )
    }
}

impl Render for ThreadView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = div().v_flex().size_full().bg(rgb(if self.wide {
            color::BACKGROUND
        } else {
            color::PANEL
        }));
        let Some(thread) = self.thread(cx) else {
            return pane.child(
                div()
                    .v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap(px(6.))
                    .p(px(24.))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Thread not found"),
                    )
                    .child(
                        div()
                            .text_color(rgb(color::TEXT_MUTED))
                            .child("It may have been deleted, or GitEZ is still reading it."),
                    ),
            );
        };
        let agent_name: SharedString = self
            .model
            .read(cx)
            .agents()
            .get(&thread.agent)
            .map_or_else(|| thread.agent.clone(), |agent| agent.name().to_owned())
            .into();
        let header = div()
            .v_flex()
            .gap(px(if self.wide { 12. } else { 10. }))
            .child(self.render_header(&thread, agent_name.clone(), cx))
            .child(
                div()
                    .text_size(px(if self.wide { 20. } else { 17. }))
                    .line_height(relative(if self.wide { 1.3 } else { 1.35 }))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(thread.title.clone()),
            )
            .child(self.render_facts(&thread, cx));
        let pull_request = thread
            .pull_request
            .map(|number| self.render_pull_request(&thread, number, cx));
        let strip = self.render_strip(&thread, cx);
        let body = div()
            .v_flex()
            .gap(px(20.))
            .when(!self.wide, |this| this.px(px(18.)))
            .children(self.render_warnings(&thread))
            .children(self.render_failure(&thread, cx))
            .children(self.render_prompt(&thread))
            .child(self.render_activity(&thread, cx))
            .children(self.render_final_message(&thread, agent_name));
        let article = if self.wide {
            div()
                .v_flex()
                .w_full()
                .max_w(px(860.))
                .mx_auto()
                .px(px(32.))
                .pt(px(24.))
                .pb(px(32.))
                .gap(px(20.))
                .child(header)
                .children(pull_request)
                .child(strip)
                .child(body)
        } else {
            // Sections divided by lines, as in the board: no gaps between
            // them.
            div()
                .v_flex()
                .pb(px(24.))
                .child(
                    div()
                        .px(px(18.))
                        .pt(px(14.))
                        .pb(px(14.))
                        .border_b_1()
                        .border_color(rgb(color::ROW_DIVIDER))
                        .child(header),
                )
                .when_some(pull_request, |this, card| {
                    this.child(
                        div()
                            .px(px(18.))
                            .py(px(14.))
                            .border_b_1()
                            .border_color(rgb(color::ROW_DIVIDER))
                            .child(card),
                    )
                })
                .child(strip)
                .child(body.pt(px(16.)))
        };
        let footer = self.render_footer(&thread, cx);
        pane.child(
            div()
                .id("thread-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .child(article),
        )
        .children(footer)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use gitez_agent_thread::{
        AgentThread, AgentThreadId, Ending, Failure, NewAgentThread, RunHeader, Start, State, Step,
        Store, Timestamp, TurnKind, TurnResult,
    };
    use gitez_sandbox::NetworkProfile;
    use gpui_kit::{Entity, TestAppContext, VisualTestContext, px, size};

    use super::{Entry, ThreadView, duration};
    use crate::agent_threads::{AgentThreadKey, Options, init_with};

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    /// A repository with one agent thread, finished by `finish`, and the
    /// data folder listing it.
    fn repository_with_thread(
        finish: impl FnOnce(&mut AgentThread),
    ) -> (tempfile::TempDir, PathBuf, AgentThreadId) {
        repository_with(
            "Explain the layout",
            &[
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"a.txt"}}]}}"#,
            ],
            finish,
        )
    }

    /// [`repository_with_thread`] with the thread's `prompt` and the lines of
    /// its transcript.
    fn repository_with(
        prompt: &str,
        transcript: &[&str],
        finish: impl FnOnce(&mut AgentThread),
    ) -> (tempfile::TempDir, PathBuf, AgentThreadId) {
        let dir = tempfile::tempdir().expect("temp dir");
        let work_dir = dir.path().join("repo");
        std::fs::create_dir(&work_dir).expect("mkdir");
        git(&work_dir, &["init", "-q", "-b", "main"]);
        std::fs::write(work_dir.join("a.txt"), "a\n").expect("write");
        git(&work_dir, &["add", "."]);
        git(&work_dir, &["commit", "-qm", "base"]);
        let repository = gitez_git::Repository::open(&work_dir).expect("open");
        let common_dir = repository.common_dir();
        let commit = repository
            .resolve_commit("HEAD")
            .expect("resolve")
            .expect("a commit")
            .to_string();
        let data_dir = dir.path().join("data");
        let store = Store::new(&common_dir);
        let at = Timestamp::now();
        let thread = store
            .create(
                NewAgentThread {
                    prompt: prompt.into(),
                    agent: gitez_agent::ClaudeCode::ID.into(),
                    model: None,
                    start: Start {
                        branch: "main".into(),
                        commit,
                    },
                    network: NetworkProfile::AgentOnly,
                    secrets: Vec::new(),
                },
                &data_dir,
                "repo",
                |_| false,
                at,
            )
            .expect("create");
        let mut run = store
            .start_run(
                &thread.id,
                RunHeader {
                    started: at,
                    agent: gitez_agent::ClaudeCode::ID.into(),
                    prompt: prompt.into(),
                    kind: TurnKind::Start,
                    session_id: None,
                    resume: false,
                    model: None,
                    network: NetworkProfile::AgentOnly,
                },
            )
            .expect("run");
        for line in transcript {
            run.append(line).expect("append");
        }
        let transcript = run.file_name();
        store
            .update(&thread.id, |thread| {
                let turn = &mut thread.turns[0];
                turn.started = Some(at);
                turn.ended = Some(at);
                turn.transcript = Some(transcript);
                finish(thread);
                Ok(())
            })
            .expect("update");
        gitez_agent_thread::Index::new(&data_dir)
            .add(gitez_agent_thread::IndexedRepository {
                common_dir: common_dir.clone(),
                work_dir,
                name: "repo".into(),
            })
            .expect("index");
        (dir, common_dir, thread.id)
    }

    fn open(
        dir: &Path,
        key: AgentThreadKey,
        cx: &mut TestAppContext,
    ) -> (Entity<ThreadView>, VisualTestContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::init(cx);
            init_with(
                Options {
                    data_dir: Some(dir.join("data")),
                    sandbox: Arc::new(gitez_sandbox::Passthrough),
                    agents: gitez_agent::Registry::builtin(),
                    secrets: Arc::new(gitez_secret::Memory::default()),
                    exe: Err("no supervisor in tests".into()),
                    watch: false,
                },
                cx,
            );
        });
        // The model reads the threads in the background.
        cx.run_until_parked();
        let window = cx.open_window(size(px(400.), px(900.)), |window, cx| {
            ThreadView::new(key, false, window, cx)
        });
        let view = window
            .update(cx, |_, _, cx| cx.entity())
            .expect("window open");
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (view, cx)
    }

    fn result(message: &str, success: bool) -> TurnResult {
        TurnResult {
            success,
            message: message.into(),
            cost_usd: Some(0.12),
            duration_ms: Some(60_000),
            steps: 1,
            exit_code: Some(0),
            stopped: false,
            blocked_hosts: Vec::new(),
        }
    }

    #[gpui_kit::test]
    fn a_finished_thread_shows_its_answer_and_a_reply_box(cx: &mut TestAppContext) {
        let (dir, common_dir, id) = repository_with_thread(|thread| {
            thread.state = State::Finished;
            thread.ending = Some(Ending::Answer);
            thread.session_id = Some("session".into());
            thread.turns[0].result = Some(result("The layout is **cached**.", true));
        });
        let key = AgentThreadKey { common_dir, id };
        let (view, mut cx) = open(dir.path(), key, cx);
        assert!(cx.debug_bounds("thread-final-message").is_some());
        assert!(cx.debug_bounds("thread-reply").is_some());
        assert!(cx.debug_bounds("thread-retry").is_none());
        // The activity opens on a click and reads the transcript.
        view.update(&mut cx, |view, cx| view.toggle_activity(cx));
        cx.run_until_parked();
        view.read_with(&cx, |view, _| {
            assert_eq!(
                view.entries,
                [Entry::Tool {
                    name: "Read".into(),
                    detail: "a.txt".into(),
                    failure: None
                }]
            );
        });
    }

    #[gpui_kit::test]
    fn a_failed_thread_offers_retry_and_no_reply(cx: &mut TestAppContext) {
        let (dir, common_dir, id) = repository_with_thread(|thread| {
            thread.fail(
                Step::Agent,
                "The agent exited with code 1.",
                Timestamp::now(),
            );
            thread.turns[0].result = Some(result("", false));
        });
        let key = AgentThreadKey { common_dir, id };
        let (view, mut cx) = open(dir.path(), key, cx);
        assert!(cx.debug_bounds("thread-retry").is_some());
        assert!(cx.debug_bounds("thread-reply").is_none());
        assert!(cx.debug_bounds("thread-final-message").is_none());
        view.read_with(&cx, |view, cx| {
            let thread = view.thread(cx).expect("the thread is read");
            assert_eq!(thread.state, State::Finished);
            assert_eq!(thread.ending, Some(Ending::Failed));
            assert!(matches!(
                &thread.error,
                Some(Failure {
                    step: Step::Agent,
                    ..
                })
            ));
        });
    }

    /// Stream-JSON lines of a transcript: `("text", words)` for what the
    /// agent wrote, `("fail", why)` for a tool that failed, and
    /// `(tool, detail)` for a tool use.
    fn transcript(steps: &[(&str, &str)]) -> Vec<String> {
        steps
            .iter()
            .map(|(kind, value)| {
                let block = match *kind {
                    "text" => serde_json::json!({"type": "text", "text": value}),
                    "fail" => {
                        let result = serde_json::json!({"type": "tool_result", "is_error": true, "content": value});
                        return serde_json::json!({"type": "user", "message": {"content": [result]}})
                            .to_string();
                    }
                    tool => serde_json::json!({"type": "tool_use", "name": tool, "input": {"command": value}}),
                };
                serde_json::json!({"type": "assistant", "message": {"content": [block]}}).to_string()
            })
            .collect()
    }

    const PROMPT: &str = "Add a blame view to the commit detail: clicking a file in a commit \
        shows who last changed each line, with the commit summary on hover. Reuse the diff \
        virtual list. Add tests in gitez-git.";

    const STEPS: &[(&str, &str)] = &[
        (
            "text",
            "I’ll add a `blame` module to gitez-git first, then the view.",
        ),
        ("Read", "crates/gitez-git/src/diff.rs"),
        ("Grep", "VirtualList"),
        ("Read", "crates/gitez-git/src/lib.rs"),
        ("Read", "crates/gitez/src/workspace/detail.rs"),
        ("Write", "crates/gitez-git/src/blame.rs"),
        ("Bash", "cargo check -p gitez-git"),
        (
            "fail",
            "error[E0425]: cannot find value `line` in this scope\n  --> blame.rs:41",
        ),
        (
            "text",
            "The virtual list from the diff view fits; wiring it into the commit detail.",
        ),
        ("Edit", "crates/gitez/src/workspace/detail.rs"),
        (
            "Bash",
            "cargo test -p gitez-git blame -- --nocapture --test-threads=1 2>&1 | tail -n 40",
        ),
    ];

    const ANSWER: &str = "Done, in two commits on the thread's branch.\n\n\
        - `blame.rs` reads who last changed each line with gix, in the background.\n\
        - The commit detail shows it in the diff's virtual list; hovering a line shows the \
        commit summary.\n\n\
        The tests in `tests/blame.rs` cover renames and a file added in the first commit.";

    /// Shoots the main window with the thread `finish` leaves open in the
    /// detail panel, whole and the panel alone.
    fn shoot(name: &str, pull_request: bool, finish: impl FnOnce(&mut AgentThread)) {
        if !crate::screenshot::enabled() {
            return;
        }
        let lines = transcript(STEPS);
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (dir, common_dir, id) = repository_with(PROMPT, &lines, finish);
        let work_dir = dir.path().join("repo");
        git(&work_dir, &["config", "user.name", "Ada Lovelace"]);
        if pull_request {
            git(&work_dir, &["checkout", "-qb", &id.branch()]);
            for (file, message) in [
                ("blame.rs", "Read who last changed each line"),
                ("detail.rs", "Show blame in the commit detail"),
            ] {
                std::fs::write(work_dir.join(file), "fn blame() {}\n".repeat(20)).expect("write");
                git(&work_dir, &["add", "."]);
                git(&work_dir, &["commit", "-qm", message]);
            }
            git(&work_dir, &["checkout", "-q", "main"]);
        }
        let store = Store::new(&common_dir);
        let thread = store.load(&id).expect("the thread is read");
        if thread.state != State::Completed {
            std::fs::create_dir_all(&thread.worktree).expect("worktree folder");
        }
        let mut shots =
            crate::screenshot::Shots::from_env(&dir.path().join("data")).expect("enabled");
        let (window, _) = shots.open_workspace(&work_dir);
        shots.dispatch(
            window,
            crate::agent_threads::OpenAgentThread {
                common_dir: common_dir.clone(),
                id: id.clone(),
            },
        );
        shots.save(window, &format!("thread-{name}"));
        // The detail panel, at its default width, in full resolution.
        let panel = gpui_kit::Bounds {
            origin: gpui_kit::point(px(1080.), px(48.)),
            size: size(px(360.), px(824.)),
        };
        shots.save_region(window, panel, &format!("thread-{name}-panel"));
        if name == "pull-request" {
            // Type a reply, open the activity and scroll to the end.
            shots.click(window, gpui_kit::point(px(1200.), px(781.)));
            shots.type_text(window, "Also cover a file renamed twice.");
            shots.click(window, gpui_kit::point(px(1200.), px(713.)));
            shots.scroll(window, gpui_kit::point(px(1260.), px(400.)), 40.);
            shots.save_region(window, panel, "thread-pull-request-activity-panel");
            // The wide layout Home will use (`C-6-Thread`).
            let key = AgentThreadKey {
                common_dir: common_dir.clone(),
                id: thread.id.clone(),
            };
            let (wide, _) = shots.open(crate::screenshot::window_size(), |window, cx| {
                gpui_kit::AppContext::new(cx, |cx| ThreadView::new(key, true, window, cx))
            });
            shots.save(wide, "thread-pull-request-wide");
        }
    }

    fn minutes_ago(minutes: i64) -> Timestamp {
        Timestamp::now() - jiff::SignedDuration::from_mins(minutes)
    }

    #[test]
    fn screenshot_thread_running() {
        shoot("running", false, |thread| {
            thread.state = State::Running;
            thread.ending = None;
            thread.session_id = Some("session".into());
            thread.secrets = vec!["GITHUB_TOKEN".into(), "SENTRY_DSN".into()];
            thread.network = NetworkProfile::Packages;
            let turn = &mut thread.turns[0];
            turn.started = Some(minutes_ago(14));
            turn.ended = None;
            thread.activity = Some(gitez_agent_thread::Activity {
                tool: Some("Bash".into()),
                detail: "cargo test -p gitez-git blame".into(),
                steps: 9,
                updated: Timestamp::now(),
            });
        });
    }

    #[test]
    fn screenshot_thread_pull_request() {
        shoot("pull-request", true, |thread| {
            thread.state = State::Finished;
            thread.ending = Some(Ending::PullRequest);
            thread.pull_request = Some(12);
            thread.session_id = Some("session".into());
            thread.secrets = vec!["GITHUB_TOKEN".into()];
            let turn = &mut thread.turns[0];
            turn.started = Some(minutes_ago(86));
            turn.ended = Some(minutes_ago(60));
            let mut done = result(ANSWER, true);
            done.steps = 9;
            done.cost_usd = Some(1.84);
            turn.result = Some(done);
        });
    }

    #[test]
    fn screenshot_thread_answer() {
        shoot("answer", false, |thread| {
            thread.state = State::Finished;
            thread.ending = Some(Ending::Answer);
            thread.session_id = Some("session".into());
            let turn = &mut thread.turns[0];
            turn.started = Some(minutes_ago(30));
            turn.ended = Some(minutes_ago(27));
            let mut done = result(ANSWER, true);
            done.steps = 9;
            turn.result = Some(done);
        });
    }

    #[test]
    fn screenshot_thread_failed() {
        shoot("failed", false, |thread| {
            let turn = &mut thread.turns[0];
            turn.started = Some(minutes_ago(12));
            turn.ended = None;
            thread.fail(
                Step::Agent,
                "Claude Code exited with code 1: API Error: 529 Overloaded. The sandbox \
                 refused a request to registry.npmjs.org.",
                minutes_ago(5),
            );
            let mut failed = result("", false);
            failed.steps = 9;
            failed.exit_code = Some(1);
            failed.blocked_hosts = vec!["registry.npmjs.org".into()];
            thread.turns[0].result = Some(failed);
        });
    }

    #[test]
    fn screenshot_thread_completed() {
        shoot("completed", false, |thread| {
            thread.state = State::Completed;
            thread.ending = Some(Ending::Answer);
            thread.session_id = Some("session".into());
            thread.completed = Some(Timestamp::now());
            let turn = &mut thread.turns[0];
            turn.started = Some(minutes_ago(60 * 26));
            turn.ended = Some(minutes_ago(60 * 26 - 3));
            let mut done = result(ANSWER, true);
            done.steps = 9;
            turn.result = Some(done);
        });
    }

    #[test]
    fn durations_read_as_minutes_and_hours() {
        assert_eq!(duration(10), "under a minute");
        assert_eq!(duration(14 * 60), "14 min");
        assert_eq!(duration(2 * 3600), "2 h");
        assert_eq!(duration(2 * 3600 + 5 * 60), "2 h 5 min");
    }
}
