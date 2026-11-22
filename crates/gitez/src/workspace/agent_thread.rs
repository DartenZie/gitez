//! One agent thread: what it was asked, what its agent did and said, its
//! branch and pull request, and what can be done with it next (stop, retry,
//! reply, complete). Shown in the detail panel of a repository window and,
//! wide, in Home; laid out as in the `C-3-Session` and `C-6-Thread` boards.

pub mod activity;
pub mod complete;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use gitez_agent::{AgentEvent, BlockedHosts, Registry};
use gitez_agent_thread::{
    AgentThread, BRIEF_PROMPT, Ending, Mode, State, Store, Timestamp, TurnImage, TurnKind,
};
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
use super::prompt_images::{self, Accepts, PromptImages};
use crate::agent_threads::{AgentThreadKey, AgentThreads, AgentThreadsEvent, OpenAgentThread};
use crate::motion;
use crate::theme::{self, color};
use activity::{Entry, Planned, Row, TurnEvents};

/// The reply "Ask the agent to commit" puts in the box.
const ASK_TO_COMMIT: &str = "You left uncommitted changes in the worktree. Commit what belongs to \
    the task with a clear message, and remove what does not. If a pre-commit hook fails, fix what \
    it reports and commit again.";

/// Activity rows from which the list is virtual: only the rows in view are
/// laid out and drawn, in a box of its own that scrolls.
const VIRTUAL_FROM: usize = 40;
/// Height of the virtual activity list.
const VIRTUAL_HEIGHT: Pixels = px(520.);

/// What the thread view asks of the window it is in.
pub enum ThreadViewEvent {
    OpenPullRequest {
        common_dir: PathBuf,
        number: u32,
    },
    /// Home: open the thread's repository.
    OpenRepository {
        work_dir: PathBuf,
    },
    /// "Implement…": open the composer with the brief a Discuss thread
    /// wrote, to start an Implement thread from it.
    Implement(Handoff),
}

/// What a Discuss thread hands to the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handoff {
    /// The Discuss thread.
    pub origin: AgentThreadKey,
    pub title: SharedString,
    /// The first prompt of the Implement thread, for the user to edit.
    pub brief: String,
    /// The working tree the discussion read.
    pub work_dir: Option<PathBuf>,
}

/// What the view is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    Stopping,
    Completing,
    Replying,
    Retrying,
    /// Asking a Discuss thread for its brief.
    Briefing,
}

/// A brief "Implement…" asked for, awaited to open the composer with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AwaitedBrief {
    /// The index its turn has in the thread.
    turn: usize,
    /// Whether the turn was added (the request did not fail).
    asked: bool,
}

/// The thumbnail of an image a turn sent, by its file name.
enum TurnThumbnail {
    Loading,
    Ready(Arc<RenderImage>),
    /// The file could not be read, with why.
    Failed(SharedString),
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
    /// Whether the thread's pull request is open, merged or closed.
    pull_request_state: Option<gitez_review::State>,
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
    /// Images pasted or dropped into the reply box.
    reply_images: Entity<PromptImages>,
    /// Thumbnails of the images the turns sent, read in the background.
    thumbnails: HashMap<String, TurnThumbnail>,
    _thumbnails_task: Option<Task<()>>,
    /// The network profile picked for a retry; the thread's own by default.
    retry_network: Option<NetworkProfile>,
    /// Whether the list of blocked hosts is shown; see
    /// [`ThreadView::blocked_shown`].
    blocked_open: Option<bool>,
    /// Whether the activity is shown; by default while the thread runs.
    activity_open: Option<bool>,
    /// Collapsed runs of steps the user expanded, by their first entry.
    expanded: HashSet<usize>,
    /// By turn, aligned with the thread's turns.
    runs: Vec<Option<RunEvents>>,
    entries: Vec<Entry>,
    /// The activity's rows as last drawn.
    plan: Vec<Planned>,
    /// The virtual list of the rows, once there are [`VIRTUAL_FROM`].
    rows: ListState,
    /// Whether the virtual list follows the newest row (while live).
    following: bool,
    /// [`VIRTUAL_FROM`]; tests change it to measure the list drawn whole.
    virtual_from: usize,
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
    /// Set from "Implement…" until the brief arrives or fails.
    awaited_brief: Option<AwaitedBrief>,
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
        let reply_images = cx.new(|_| PromptImages::new("reply"));
        let subscriptions = vec![
            cx.observe(&reply_images, |_, _, cx| cx.notify()),
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
            reply_images,
            thumbnails: HashMap::new(),
            _thumbnails_task: None,
            retry_network: None,
            blocked_open: None,
            activity_open: None,
            expanded: HashSet::new(),
            runs: Vec::new(),
            entries: Vec::new(),
            plan: Vec::new(),
            rows: ListState::new(0, ListAlignment::Top, px(400.)),
            following: false,
            virtual_from: VIRTUAL_FROM,
            reading: None,
            read_again: false,
            transcript_error: None,
            details: Details::default(),
            details_read: None,
            _details_task: None,
            seen: None,
            busy: None,
            awaited_brief: None,
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
            self.read_thumbnails(&thread, cx);
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
        self.brief_arrived(&thread, cx);
        cx.notify();
    }

    /// Hands the brief "Implement…" asked for to the composer, once its
    /// turn has ended; forgets it when the turn failed or never started.
    fn brief_arrived(&mut self, thread: &AgentThread, cx: &mut Context<Self>) {
        let Some(awaited) = self.awaited_brief else {
            return;
        };
        if thread.state.is_active() {
            return;
        }
        match thread.turns.get(awaited.turn) {
            Some(turn) if turn.kind == TurnKind::Brief => {
                self.awaited_brief = None;
                if let Some(handoff) = self.handoff(thread, cx) {
                    cx.emit(ThreadViewEvent::Implement(handoff));
                }
            }
            Some(_) => self.awaited_brief = None,
            // The model has not read the new turn yet, unless it was
            // never added.
            None if !awaited.asked && self.busy.is_none() => self.awaited_brief = None,
            None => {}
        }
    }

    /// What the composer gets from `thread`, a Discuss thread whose last
    /// turn is a brief.
    fn handoff(&self, thread: &AgentThread, _cx: &App) -> Option<Handoff> {
        let brief = thread.brief()?.trim().to_owned();
        Some(Handoff {
            origin: self.key.clone(),
            title: thread.title.clone().into(),
            brief,
            work_dir: thread.work_dir.clone(),
        })
    }

    /// "Implement…": opens the composer with the thread's brief; asks the
    /// agent for one first unless its last answer is one.
    fn implement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.thread(cx) else {
            return;
        };
        if !can_implement(&thread) || self.busy.is_some() {
            return;
        }
        if let Some(handoff) = self.handoff(&thread, cx) {
            cx.emit(ThreadViewEvent::Implement(handoff));
            return;
        }
        self.activity_open = None;
        self.awaited_brief = Some(AwaitedBrief {
            turn: thread.turns.len(),
            asked: false,
        });
        self.run(
            Busy::Briefing,
            window,
            cx,
            |model, key, cx| {
                model.reply(
                    key,
                    TurnKind::Brief,
                    BRIEF_PROMPT.to_owned(),
                    Vec::new(),
                    cx,
                )
            },
            |this, _, _| {
                if let Some(awaited) = &mut this.awaited_brief {
                    awaited.asked = true;
                }
            },
        );
    }

    /// Makes thumbnails of the turns' images not read yet, in the
    /// background: small copies only, never the images themselves.
    fn read_thumbnails(&mut self, thread: &AgentThread, cx: &mut Context<Self>) {
        if self._thumbnails_task.is_some() {
            return;
        }
        let wanted: Vec<TurnImage> = thread
            .turns
            .iter()
            .flat_map(|turn| &turn.images)
            .filter(|image| !self.thumbnails.contains_key(&image.name))
            .cloned()
            .collect();
        if wanted.is_empty() {
            return;
        }
        for image in &wanted {
            self.thumbnails
                .insert(image.name.clone(), TurnThumbnail::Loading);
        }
        let store = Store::new(&self.key.common_dir);
        let id = self.key.id.clone();
        let read = cx.background_executor().spawn(async move {
            wanted
                .into_iter()
                .map(|image| {
                    let thumbnail = store
                        .read_image(&id, &image)
                        .map_err(|error| format!("{:#}", anyhow::Error::from(error)))
                        .and_then(|bytes| {
                            gitez_agent_thread::thumbnail(&bytes, prompt_images::THUMBNAIL_PIXELS)
                                .map_err(|error| error.to_string())
                        });
                    (image.name, thumbnail)
                })
                .collect::<Vec<_>>()
        });
        self._thumbnails_task = Some(cx.spawn(async move |this, cx| {
            let read = read.await;
            this.update(cx, |this, cx| {
                this._thumbnails_task = None;
                for (name, thumbnail) in read {
                    this.thumbnails.insert(
                        name,
                        match thumbnail {
                            Ok(thumbnail) => {
                                TurnThumbnail::Ready(prompt_images::render_image(thumbnail))
                            }
                            Err(why) => TurnThumbnail::Failed(why.into()),
                        },
                    );
                }
                // Images of turns added meanwhile.
                if let Some(thread) = this.thread(cx) {
                    this.read_thumbnails(&thread, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The thumbnails of `images`, which a turn sent: a click opens the
    /// image in the system's viewer.
    fn render_turn_images(&self, turn: usize, images: &[TurnImage]) -> Option<Div> {
        if images.is_empty() {
            return None;
        }
        let store = Store::new(&self.key.common_dir);
        let tiles = images.iter().enumerate().map(|(index, image)| {
            let (thumbnail, mut tooltip) = match self.thumbnails.get(&image.name) {
                Some(TurnThumbnail::Ready(thumbnail)) => (Some(thumbnail.clone()), None),
                Some(TurnThumbnail::Failed(why)) => (None, Some(why.to_string())),
                _ => (None, None),
            };
            let failed = tooltip.is_some();
            let described = format!(
                "{} · {} × {} · {}",
                image.media_type.label(),
                image.width,
                image.height,
                gitez_agent_thread::size_text(image.size)
            );
            let tooltip: SharedString = tooltip
                .take()
                .unwrap_or_else(|| format!("{described} · click to open"))
                .into();
            let path = store.image_path(&self.key.id, image).ok();
            let id = ElementId::Name(format!("turn-{turn}-image-{index}").into());
            prompt_images::image_tile(id, thumbnail, tooltip, None)
                .when(failed, |this| {
                    this.child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                Icon::new(IconName::TriangleAlert)
                                    .size(px(14.))
                                    .text_color(rgb(color::WARNING_TEXT)),
                            ),
                    )
                })
                .when_some(path.filter(|_| !failed), |this, path| {
                    this.cursor_pointer()
                        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                            cx.open_with_system(&path)
                        })
                })
        });
        Some(
            div()
                .debug_selector(move || format!("turn-{turn}-images"))
                .h_flex()
                .flex_wrap()
                .gap(px(8.))
                .children(tiles),
        )
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
        let base = thread.start.commit.clone();
        // A deleted branch is read where it was deleted.
        let branch = match &thread.deleted_branch {
            Some(deleted) => deleted.commit.clone(),
            None => format!("refs/heads/{}", thread.branch_name()),
        };
        let number = thread.pull_request;
        let common_dir = self.key.common_dir.clone();
        let read = cx.background_executor().spawn(async move {
            let Some(repository) = work_dir.and_then(|dir| gitez_git::Repository::open(dir).ok())
            else {
                return Details::default();
            };
            let read =
                number.and_then(|number| gitez_review::Store::new(&common_dir).load(number).ok());
            // A merged or closed pull request keeps the changes it had then,
            // as its review shows them: from the commits it recorded (the
            // branch may be gone, or rebased onto the base).
            let pull_request = match &read {
                Some(read) if read.state != gitez_review::State::Open => {
                    branch_stats(&repository, &read.base.commit, &read.head.commit)
                }
                _ => number.and_then(|_| branch_stats(&repository, &base, &branch)),
            };
            let pull_request_state = read.map(|pull_request| pull_request.state);
            Details {
                user_name: repository.user_name(),
                pull_request,
                pull_request_state,
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
        if self.busy.is_some() {
            return;
        }
        complete::complete(self.key.clone(), self.completing_busy(cx), window, cx);
    }

    /// Removes the worktree the completed thread kept, after asking.
    fn remove_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        complete::remove_worktree(self.key.clone(), self.completing_busy(cx), window, cx);
    }

    /// Marks the view busy completing while [`complete`] works.
    fn completing_busy(&self, cx: &mut Context<Self>) -> complete::Busy {
        let view = cx.entity().downgrade();
        std::rc::Rc::new(move |busy, _, cx| {
            view.update(cx, |this, cx| {
                this.busy = busy.then(|| (Busy::Completing, Task::ready(())));
                cx.notify();
            })
            .ok();
        })
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

    /// Whether the thread's agent and model take images, as the agent
    /// says.
    fn accepts_images(&self, thread: &AgentThread, cx: &App) -> Accepts {
        let model = self.model.read(cx);
        let agent = model.agents().get(&thread.agent);
        let alias = thread
            .model
            .clone()
            .or_else(|| model.settings().agent(&thread.agent).model)
            .unwrap_or_default();
        prompt_images::accepts(agent.as_deref(), Some(&alias))
    }

    /// Why the reply cannot be sent now, if not for its being empty.
    fn reply_blocked(&self, thread: &AgentThread, cx: &App) -> Option<SharedString> {
        let images = self.reply_images.read(cx);
        if images.is_empty() {
            return None;
        }
        if images.preparing() {
            return Some("Preparing the images…".into());
        }
        self.accepts_images(thread, cx)
            .err()
            .map(|why| format!("{why} Remove the images to send the reply.").into())
    }

    fn send_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.thread(cx) else {
            return;
        };
        if !can_reply(&thread) || self.reply_blocked(&thread, cx).is_some() {
            return;
        }
        let text = self.reply.read(cx).value().trim().to_owned();
        let images = self.reply_images.read(cx).ready();
        if text.is_empty() && images.is_empty() {
            return;
        }
        self.activity_open = None;
        self.run(
            Busy::Replying,
            window,
            cx,
            move |model, key, cx| model.reply(key, TurnKind::Reply, text, images, cx),
            |this, window, cx| {
                this.reply
                    .update(cx, |reply, cx| reply.set_value("", window, cx));
                this.reply_images
                    .update(cx, |images, cx| images.clear(window, cx));
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
fn branch_stats(repository: &gitez_git::Repository, base: &str, head: &str) -> Option<BranchStats> {
    let base = Oid::from_hex(base.as_bytes()).ok()?;
    let head = repository.resolve_commit(head).ok()??;
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

/// What became of completed `thread`'s worktree and branch, for its pull
/// request's card: "The thread is completed, its worktree removed and its
/// branch deleted (it was at 1a2b3c4)." `worktree_present`: its worktree
/// folder is still there (kept, and the branch with it).
fn completion_note(thread: &AgentThread, worktree_present: bool) -> String {
    if worktree_present {
        return "The thread is completed; its worktree and branch are kept (see above).".into();
    }
    match &thread.deleted_branch {
        Some(deleted) => format!(
            "The thread is completed, its worktree removed and its branch deleted (it was at \
             {}).",
            deleted.commit.chars().take(7).collect::<String>()
        ),
        None if thread.branch_kept_warning().is_some() => {
            "The thread is completed and its worktree removed; its branch is kept (see below)."
                .into()
        }
        None => "The thread is completed and its worktree removed; its branch is kept.".into(),
    }
}

/// Whether a reply can continue `thread`'s conversation now.
fn can_reply(thread: &AgentThread) -> bool {
    thread.state == State::Finished && thread.session_id.is_some()
}

/// Whether "Implement…" is offered: a Discuss thread whose conversation
/// can go on, so the agent can write the brief.
fn can_implement(thread: &AgentThread) -> bool {
    thread.mode == Mode::Discuss && can_reply(thread)
}

/// Under an Auto thread's last answer, when its agent chose to implement:
/// what GitEZ did with the brief.
fn handoff_note(thread: &AgentThread) -> Option<Div> {
    let handoff = thread.turns.last()?.handoff.as_ref()?;
    let (text, colour) = match (&handoff.thread, &handoff.problem) {
        (Some(_), _) => (
            "GitEZ started an Implement thread from this brief.".to_owned(),
            color::TEXT_MUTED,
        ),
        (None, Some(problem)) => (problem.clone(), color::THREAD_FAILED_TEXT),
        (None, None) => (
            "Starting an Implement thread from this brief…".to_owned(),
            color::TEXT_MUTED,
        ),
    };
    Some(
        div()
            .debug_selector(|| "thread-handoff".into())
            .ml(px(28.))
            .h_flex()
            .gap(px(6.))
            .text_size(px(12.))
            .text_color(rgb(colour))
            .items_start()
            .child(
                div()
                    .flex_none()
                    .pt(px(2.))
                    .child(Icon::new(IconName::ArrowRight).size(px(12.))),
            )
            .child(div().flex_1().min_w_0().child(text)),
    )
}

/// Whether `thread` is finished without a pull request, so Complete is
/// offered; one with a pull request completes when it is merged or closed.
pub fn can_complete(thread: &AgentThread) -> bool {
    thread.state == State::Finished && thread.pull_request.is_none()
}

/// The model the thread runs with, as its agent names it ("Opus"), or the
/// alias stored when the agent does not know it, followed by the effort
/// level asked for ("Opus · High effort"); `None` for a thread that
/// recorded neither.
pub fn model_label(agents: &Registry, thread: &AgentThread) -> Option<SharedString> {
    let agent = agents.get(&thread.agent);
    let named = |value: &Option<String>, choices: &[(&'static str, &'static str)]| {
        let value = value.as_deref().filter(|value| !value.is_empty())?;
        Some(
            choices
                .iter()
                .find(|(known, _)| *known == value)
                .map_or(value, |(_, label)| *label)
                .to_owned(),
        )
    };
    let model = named(
        &thread.model,
        agent.as_ref().map_or(&[], |agent| agent.models()),
    );
    let effort = named(
        &thread.effort,
        agent.as_ref().map_or(&[], |agent| agent.efforts()),
    )
    .map(|effort| format!("{effort} effort"));
    let label = match (model, effort) {
        (Some(model), Some(effort)) => format!("{model} · {effort}"),
        (model, effort) => model.or(effort)?,
    };
    Some(label.into())
}

/// The hosts the sandbox refused in `thread`'s last run that ended, split
/// by its agent's [`gitez_agent::Agent::optional_hosts`]: only
/// [`BlockedHosts::problems`] count, colour a line or change a suggestion.
pub fn blocked_hosts<'a>(thread: &'a AgentThread, agents: &Registry) -> BlockedHosts<'a> {
    let hosts = thread
        .turns
        .iter()
        .rev()
        .find_map(|turn| turn.result.as_ref())
        .map_or(&[][..], |result| result.blocked_hosts.as_slice());
    BlockedHosts::classify(agents.get(&thread.agent).as_deref(), hosts)
}

/// The network a retry of `thread` should use: Packages when Agent API only
/// blocked a host in its last run that Packages allows (one of
/// `package_hosts`, the Settings list), otherwise the thread's own. Hosts
/// the agent does not need ([`blocked_hosts`]), such as Claude Code's own
/// telemetry, never widen the network, nor do hosts Packages blocks too.
pub fn suggested_retry(
    thread: &AgentThread,
    agents: &Registry,
    package_hosts: &[String],
) -> NetworkProfile {
    let packages_help = blocked_hosts(thread, agents)
        .problems
        .iter()
        .any(|host| package_host(package_hosts, host));
    if packages_help && thread.network == NetworkProfile::AgentOnly {
        NetworkProfile::Packages
    } else {
        thread.network
    }
}

/// The strip's count of blocked requests: only the hosts the run may have
/// needed count; "No blocked requests" when the agent's optional hosts
/// were the only ones blocked.
pub fn blocked_summary(blocked: &BlockedHosts<'_>) -> String {
    match blocked.problems.len() {
        0 => "No blocked requests".to_owned(),
        1 => "1 blocked request".to_owned(),
        count => format!("{count} blocked requests"),
    }
}

/// Whether `host` is one of `package_hosts`, which may name a domain and
/// its subdomains as `*.example.com`.
fn package_host(package_hosts: &[String], host: &str) -> bool {
    package_hosts.iter().any(|allowed| {
        allowed.eq_ignore_ascii_case(host)
            || allowed.strip_prefix("*.").is_some_and(|domain| {
                host.len() > domain.len()
                    && host[host.len() - domain.len()..].eq_ignore_ascii_case(domain)
                    && host[..host.len() - domain.len()].ends_with('.')
            })
    })
}

/// When `thread` last stopped: its newest turn's end, else its last change.
pub fn finished_at(thread: &AgentThread) -> Timestamp {
    thread
        .turns
        .iter()
        .rev()
        .find_map(|turn| turn.ended)
        .unwrap_or(thread.updated)
}

/// "12 min ago", "yesterday": when `time` was, from `now`.
pub fn ago(time: Timestamp, now: Timestamp) -> String {
    crate::time::relative(git_time(time), now.as_second())
}

/// The one vocabulary of thread states every view uses (thread view, Home,
/// the Agents popover, lanes, the review panel), each with its own colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    Queued,
    Preparing,
    Running,
    /// Waiting for the agent's usage limit.
    Waiting,
    /// Finished with a pull request.
    Ready,
    /// Finished with an answer and nothing left in the worktree.
    Answer,
    /// Finished without a pull request, but with uncommitted changes left
    /// in the worktree: never "no changes".
    Uncommitted,
    Failed,
    Stopped,
    Completed,
}

impl Look {
    pub fn of(thread: &AgentThread) -> Self {
        match thread.state {
            State::Queued => Look::Queued,
            State::Preparing => Look::Preparing,
            State::Running => Look::Running,
            State::Waiting => Look::Waiting,
            State::Completed => Look::Completed,
            State::Finished => match thread.ending {
                Some(Ending::Failed) => Look::Failed,
                Some(Ending::Stopped) => Look::Stopped,
                _ if thread.pull_request.is_some() => Look::Ready,
                _ if thread.has_uncommitted() == Some(true) => Look::Uncommitted,
                Some(Ending::PullRequest) => Look::Ready,
                Some(Ending::Answer) | None => Look::Answer,
            },
        }
    }

    /// The chip's background and the state's text colour.
    pub fn colors(self) -> (Rgba, u32) {
        match self {
            Look::Queued => (rgb(color::THREAD_QUEUED), color::TEXT_MUTED),
            Look::Preparing => (rgba(color::THREAD_PREPARING), color::THREAD_PREPARING_TEXT),
            Look::Running => (rgba(color::THREAD_RUNNING), color::THREAD_RUNNING_TEXT),
            Look::Waiting => (rgba(color::THREAD_WAITING), color::THREAD_WAITING_TEXT),
            Look::Ready => (rgba(color::THREAD_READY), color::THREAD_READY_TEXT),
            Look::Answer => (rgb(color::THREAD_ANSWER), color::THREAD_ANSWER_TEXT),
            Look::Uncommitted => (
                rgba(color::THREAD_UNCOMMITTED),
                color::THREAD_UNCOMMITTED_TEXT,
            ),
            Look::Failed => (rgba(color::THREAD_FAILED), color::THREAD_FAILED_TEXT),
            Look::Stopped => (rgba(color::THREAD_STOPPED), color::THREAD_STOPPED_TEXT),
            Look::Completed => (rgb(color::THREAD_QUEUED), color::TEXT_FAINT),
        }
    }

    /// The state's text colour.
    pub fn text(self) -> u32 {
        self.colors().1
    }

    /// Its word, without the time a waiting thread waits until.
    pub fn word(self) -> &'static str {
        match self {
            Look::Queued => "Queued",
            Look::Preparing => "Preparing",
            Look::Running => "Running",
            Look::Waiting => "Waiting",
            Look::Ready => "Ready for review",
            Look::Answer => "Answer",
            Look::Uncommitted => "Uncommitted changes",
            Look::Failed => "Failed",
            Look::Stopped => "Stopped",
            Look::Completed => "Completed",
        }
    }
}

/// What `thread` is doing, or how it ended: "Running", "Waiting until
/// 14:30", "Ready for review", "Answer", "Uncommitted changes", "Failed".
pub fn state_label(thread: &AgentThread) -> String {
    match (Look::of(thread), thread.waiting_until) {
        (Look::Waiting, Some(until)) => format!("Waiting until {}", local_time(until)),
        (look, _) => look.word().to_owned(),
    }
}

/// How a completed thread was completed: "Completed · PR #12 merged",
/// "Completed · PR #12 closed", "Completed by hand". `pull_request` is the
/// state of its pull request, when read.
pub fn completed_label(thread: &AgentThread, pull_request: Option<gitez_review::State>) -> String {
    match (thread.pull_request, pull_request) {
        (Some(number), Some(gitez_review::State::Merged)) => {
            format!("Completed · PR #{number} merged")
        }
        (Some(number), Some(gitez_review::State::Closed)) => {
            format!("Completed · PR #{number} closed")
        }
        (Some(number), _) => format!("Completed · PR #{number}"),
        (None, _) => "Completed by hand".to_owned(),
    }
}

/// "3 uncommitted changes" left in `thread`'s worktree, if any; for a
/// thread whose worktree was removed, nothing.
pub fn uncommitted_text(thread: &AgentThread) -> Option<String> {
    let uncommitted = thread
        .uncommitted
        .as_ref()
        .filter(|found| found.count > 0)?;
    Some(match uncommitted.count {
        1 => "1 uncommitted change".to_owned(),
        count => format!("{count} uncommitted changes"),
    })
}

/// Why the changes `thread` left are still uncommitted, as the supervisor
/// recorded it (git's or a hook's words), ending as a sentence.
pub fn commit_error_text(thread: &AgentThread) -> Option<String> {
    let error = thread.uncommitted.as_ref()?.commit_error.as_deref()?.trim();
    if error.is_empty() {
        return None;
    }
    Some(if error.ends_with(['.', '!', '?']) {
        error.to_owned()
    } else {
        format!("{error}.")
    })
}

/// The chip showing [`state_label`] in the state's colours.
pub fn state_chip(thread: &AgentThread) -> Div {
    chip(Look::of(thread), state_label(thread))
}

/// A chip saying `label` in the colours of `look`. Its dot blinks while
/// the thread runs or is prepared, as on the boards.
pub fn chip(look: Look, label: String) -> Div {
    let (background, text) = look.colors();
    let mark = if look == Look::Completed {
        Icon::new(IconName::Check).size(px(11.)).into_any_element()
    } else if matches!(look, Look::Running | Look::Preparing) {
        motion::blinking_dot(px(6.), text).into_any_element()
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
        .child(label)
}

/// The 16 px mark of a thread's state, in its colour: a ring with a dot
/// while running (dashed while preparing), a dashed ring while queued or
/// waiting, a pull-request mark when ready for review, a speech bubble for
/// an answer, a warning for uncommitted changes, a cross when it failed, a
/// square when stopped, and when completed a merge mark (merged), a crossed
/// circle (closed) or a tick (by hand). `pull_request` is the state of its
/// pull request, when read. With `moving`, a running thread's dot blinks
/// and a preparing thread's ring turns (Home's rows); without, they stand
/// still (the Agents popover, as on the boards).
pub fn state_icon(
    thread: &AgentThread,
    pull_request: Option<gitez_review::State>,
    moving: bool,
) -> AnyElement {
    let look = Look::of(thread);
    let hex = look.text();
    let ring = |dashed: bool| {
        div()
            .flex()
            .items_center()
            .justify_center()
            .size(px(12.))
            .rounded_full()
            .border_2()
            .border_color(rgb(hex))
            .when(dashed, |this| this.border_dashed())
    };
    let icon = |name: IconName| {
        Icon::new(name)
            .size(px(14.))
            .text_color(rgb(hex))
            .into_any_element()
    };
    let mark = match look {
        Look::Running if moving => ring(false)
            .child(motion::blinking_dot(px(4.), hex))
            .into_any_element(),
        Look::Running => ring(false)
            .child(div().size(px(4.)).rounded_full().bg(rgb(hex)))
            .into_any_element(),
        Look::Preparing if moving => motion::spinning_ring(px(12.), px(2.), hex).into_any_element(),
        Look::Preparing | Look::Queued | Look::Waiting => ring(true).into_any_element(),
        Look::Ready => icon(IconName::GitPullRequestArrow),
        Look::Answer => icon(IconName::MessageSquare),
        Look::Uncommitted => icon(IconName::TriangleAlert),
        Look::Failed => icon(IconName::CircleX),
        Look::Stopped => icon(IconName::CircleStop),
        Look::Completed => match (thread.pull_request, pull_request) {
            (Some(_), Some(gitez_review::State::Merged)) => icon(IconName::GitMerge),
            (Some(_), Some(gitez_review::State::Closed)) => icon(IconName::CircleX),
            _ => icon(IconName::Check),
        },
    };
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(16.))
        .mt(px(1.))
        .child(mark)
        .into_any_element()
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
pub fn duration(seconds: i64) -> String {
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
pub fn summary(thread: &AgentThread, now: Timestamp) -> String {
    let run = duration(thread.run_time(now).as_secs());
    let cost = cost(thread).map(|cost| format!("${cost:.2}"));
    let steps = match thread.steps() {
        1 => "1 step".to_owned(),
        steps => format!("{steps} steps"),
    };
    let mut parts = match thread.state {
        State::Queued => vec!["Waits for a free slot".to_owned()],
        State::Preparing | State::Running | State::Waiting => vec![run, steps],
        State::Finished | State::Completed => vec![
            format!("Finished {}", ago(finished_at(thread), now)),
            format!("{run} run"),
        ],
    };
    parts.extend(cost);
    parts.join(" · ")
}

/// The square with an agent's mark.
pub fn agent_mark(agent: &str) -> Div {
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
pub fn branch_chip(name: &str, small: bool) -> Div {
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
pub fn on_accent(key_cap: Div) -> Div {
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

/// A message the user sent, in a bubble: its images, then its text.
fn bubble(text: String, images: Option<Div>, indent: bool) -> Div {
    let has_text = !text.trim().is_empty();
    div()
        .when(indent, |this| this.ml(px(28.)))
        .v_flex()
        .gap(px(8.))
        .px(px(14.))
        .py(px(10.))
        .bg(rgb(color::PANEL))
        .border_1()
        .border_color(rgb(color::BORDER))
        .rounded(px(8.))
        .text_size(px(13.))
        .line_height(relative(1.6))
        .children(images.map(|images| images.py(px(2.))))
        .when(has_text, |this| this.child(text))
}

impl ThreadView {
    fn render_header(
        &self,
        thread: &AgentThread,
        agent_name: SharedString,
        model_name: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> Div {
        let busy = self.busy.as_ref().map(|(busy, _)| *busy);
        let active = thread.state.is_active();
        let can_complete = can_complete(thread);
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
            .children(model_name.map(|name| {
                div()
                    .debug_selector(|| "thread-model".into())
                    .text_color(rgb(color::TEXT_MUTED))
                    .child(name)
            }))
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
                                .debug_selector(|| "thread-stop".into())
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
                                .debug_selector(|| "thread-complete".into())
                                .h(px(28.))
                                .compact()
                                .icon(Icon::new(IconName::Check).size(px(13.)))
                                .label("Complete")
                                .tooltip(crate::workspace::toolbar::agents::complete_tooltip(
                                    thread,
                                ))
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
        // A thread without a branch now (a discussion, an answer, or a
        // branch deleted when it was completed) shows where it started.
        let branch = thread
            .branch
            .as_deref()
            .filter(|_| thread.ending != Some(Ending::Answer) && thread.deleted_branch.is_none());
        match branch {
            None => {
                rows = rows.child(row(
                    "Started from",
                    from(div().h_flex().gap(px(6.)).min_w_0()).into_any_element(),
                ));
            }
            Some(branch) => {
                rows = rows.child(row(
                    "Branch",
                    from(
                        div()
                            .h_flex()
                            .gap(px(6.))
                            .min_w_0()
                            .child(branch_chip(branch, false))
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
        }
        let path = |dir: &std::path::Path| {
            div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .font_family(mono.clone())
                .text_size(px(11.5))
                .text_color(rgb(color::TEXT_SECONDARY))
                .child(crate::recent::display(dir))
        };
        // Looked up with the thread in the background: never a disk read
        // while drawing.
        let present = self.model.read(cx).facts(&self.key).worktree_present;
        if let Some(worktree) = thread
            .worktree
            .clone()
            .filter(|_| thread.state != State::Completed || present)
        {
            rows = rows.child(row(
                "Worktree",
                div()
                    .h_flex()
                    .gap(px(4.))
                    .min_w_0()
                    .child(path(&worktree))
                    .when(present, |this| {
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
        // A discussion reads the user's own working tree, and changes
        // nothing in it.
        if let Some(work_dir) = thread.work_dir.as_deref().filter(|_| !self.wide) {
            rows = rows.child(row(
                "Reads",
                div()
                    .debug_selector(|| "thread-working-tree".into())
                    .h_flex()
                    .gap(px(6.))
                    .min_w_0()
                    .child(path(work_dir))
                    .child(div().flex_none().text_color(rgb(color::TEXT_MUTED)).child(
                        if thread.auto {
                            "read-only · Auto"
                        } else {
                            "read-only"
                        },
                    ))
                    .into_any_element(),
            ));
        }
        let link = |id: &'static str, index: usize, thread: &AgentThread| {
            let action = OpenAgentThread {
                common_dir: self.key.common_dir.clone(),
                id: thread.id.clone(),
            };
            div()
                .h_flex()
                .gap(px(8.))
                .min_w_0()
                .child(
                    Button::new((id, index))
                        .link()
                        .label(thread.title.clone())
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(Box::new(action.clone()), cx)
                        }),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(rgb(color::TEXT_MUTED))
                        .child(Look::of(thread).word()),
                )
        };
        if let Some(origin) = &thread.origin {
            let key = AgentThreadKey {
                common_dir: self.key.common_dir.clone(),
                id: origin.clone(),
            };
            let value = match self.model.read(cx).thread(&key) {
                Some(discussion) => link("thread-origin", 0, discussion).into_any_element(),
                // Its files are gone; its name is what is left.
                None => div()
                    .text_color(rgb(color::TEXT_MUTED))
                    .child(origin.to_string())
                    .into_any_element(),
            };
            rows = rows.child(row("Implements", value));
        }
        let implementations = self
            .model
            .read(cx)
            .implementations(&self.key.common_dir, &thread.id);
        for (index, implementation) in implementations.iter().enumerate() {
            rows = rows.child(row(
                if index == 0 { "Handed to" } else { "" },
                link("thread-implementation", index, implementation).into_any_element(),
            ));
        }
        rows
    }

    /// Whether the list of blocked hosts is shown: by default when a host
    /// the run may have needed was blocked.
    fn blocked_shown(&self, blocked: &BlockedHosts<'_>) -> bool {
        self.blocked_open.unwrap_or(!blocked.problems.is_empty())
    }

    /// Sandbox profile, secrets and blocked requests. Only the hosts the
    /// run may have needed are counted, in the warning colour; the hosts
    /// the agent contacts on its own and does not need are listed apart,
    /// muted, with what they are for.
    fn render_strip(&self, thread: &AgentThread, cx: &mut Context<Self>) -> Div {
        let mono = cx.theme().mono_font_family.clone();
        let model = self.model.read(cx);
        let blocked = blocked_hosts(thread, model.agents());
        let open = self.blocked_shown(&blocked);
        let any = !blocked.problems.is_empty() || !blocked.optional.is_empty();
        let summary = div()
            .id("thread-blocked")
            .debug_selector(|| "thread-blocked".into())
            .h_flex()
            .gap(px(5.))
            .text_color(rgb(if blocked.problems.is_empty() {
                color::TEXT_MUTED
            } else {
                color::THREAD_FAILED_TEXT
            }))
            .child(blocked_summary(&blocked))
            .when(!blocked.optional.is_empty(), |this| {
                this.child(
                    div()
                        .text_color(rgb(color::TEXT_FAINT))
                        .child(format!("· {} not needed", blocked.optional.len())),
                )
            })
            .when(any, |this| {
                this.cursor_pointer()
                    .hover(|style| style.text_color(rgb(color::TEXT)))
                    .child(
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(px(12.))
                        .text_color(rgb(color::TEXT_FAINT)),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.blocked_open = Some(!open);
                        cx.notify();
                    }))
            });
        let row = div()
            .h_flex()
            .flex_wrap()
            .gap_x(px(14.))
            .gap_y(px(6.))
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
            .child(summary);
        let host = |host: &str, hex: u32| {
            div()
                .flex_none()
                .font_family(mono.clone())
                .text_size(px(11.5))
                .text_color(rgb(hex))
                .child(host.to_owned())
        };
        let list = (any && open).then(|| {
            div()
                .id("thread-blocked-hosts")
                .v_flex()
                .gap(px(4.))
                .pl(px(19.))
                .children(blocked.problems.iter().map(|name| {
                    div()
                        .h_flex()
                        .flex_wrap()
                        .gap_x(px(8.))
                        .child(host(name, color::THREAD_FAILED_TEXT))
                        .child(
                            div()
                                .text_color(rgb(color::TEXT_MUTED))
                                .child(format!("blocked by {}", thread.network.label())),
                        )
                }))
                .children(blocked.optional.iter().map(|(name, purpose)| {
                    div()
                        .h_flex()
                        .flex_wrap()
                        .gap_x(px(8.))
                        .text_color(rgb(color::TEXT_FAINT))
                        .child(host(name, color::TEXT_FAINT))
                        .child(*purpose)
                }))
        });
        div()
            .v_flex()
            .gap(px(8.))
            .py(px(10.))
            // In the panel, the section above draws the line over it.
            .when(!self.wide, |this| this.px(px(18.)))
            .when(self.wide, |this| this.border_t_1())
            .border_b_1()
            .border_color(rgb(color::ROW_DIVIDER))
            .text_size(px(12.))
            .text_color(rgb(color::TEXT_SECONDARY))
            .child(row)
            .children(list)
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
        let completed = thread.state == State::Completed;
        // How the pull request ended, once read, for a completed thread.
        let completion = completion_note(
            thread,
            self.model.read(cx).facts(&self.key).worktree_present,
        );
        let note = match (completed, self.details.pull_request_state) {
            (false, _) => "Review comments go back to this thread and run in its worktree.".into(),
            (true, Some(gitez_review::State::Merged)) => {
                format!("Merged into {}. {completion}", thread.start.branch)
            }
            (true, Some(gitez_review::State::Closed)) => {
                format!("Closed without merging. {completion}")
            }
            (true, Some(gitez_review::State::Open)) => {
                "Reopened after the thread was completed; it is reviewed like any other pull \
                 request."
                    .into()
            }
            (true, None) => completion,
        };
        let button = Button::new("thread-open-pull-request")
            .h(px(34.))
            .px(px(14.))
            .when(!completed, |this| this.primary())
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
                            .child(branch_chip(thread.branch_name(), true))
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
                            .child(note),
                    ),
            )
            .child(button)
    }

    fn render_prompt(&self, thread: &AgentThread) -> Option<Div> {
        let first = thread.turns.first()?;
        let prompt = first.prompt.clone();
        let images = self.render_turn_images(0, &first.images);
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
                .child(bubble(prompt, images, true)),
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
                        .child(motion::blinking_dot(px(6.), color::BRANCH_BLUE))
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
        let rows = self.plan.len();
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
        } else if rows == 0 {
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
        } else if rows >= self.virtual_from {
            // Only the rows in view are laid out: a transcript of thousands
            // of steps draws as fast as a short one.
            body.child(
                // GPUI's list scrolls itself but lets the wheel event go on
                // to the thread's own scroll, which then moves too: the
                // list keeps the events it gets.
                div()
                    .id("thread-activity-list")
                    .debug_selector(|| "thread-activity-list".into())
                    .w_full()
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .child(
                        list(
                            self.rows.clone(),
                            // Each row takes the list's width, as when drawn
                            // whole.
                            cx.processor(|this, index: usize, _, cx| {
                                div()
                                    .w_full()
                                    .child(this.render_planned(index, cx))
                                    .into_any_element()
                            }),
                        )
                        .h(VIRTUAL_HEIGHT)
                        .w_full(),
                    ),
            )
        } else {
            body.children((0..rows).map(|index| self.render_planned(index, cx)))
        };
        log = log.child(body);
        log
    }

    /// Plans the activity's rows for this frame and keeps the virtual list
    /// in step: the rows that changed are measured again, the others keep
    /// their place; while the agent works the list follows the newest.
    fn plan_rows(&mut self, thread: &AgentThread) {
        let live = thread.state == State::Running
            && thread.turns.last().is_some_and(|turn| turn.ended.is_none());
        let plan = activity::plan(&self.entries, &self.expanded, live);
        if plan != self.plan {
            let (old, count) = activity::changed(&self.plan, &plan);
            if self.rows.item_count() == self.plan.len() {
                self.rows.splice(old, count);
            } else {
                self.rows.reset(plan.len());
            }
            self.plan = plan;
        }
        if live != self.following {
            self.following = live;
            self.rows.set_follow_mode(if live {
                FollowMode::Tail
            } else {
                FollowMode::Normal
            });
        }
    }

    /// Row `index` of the activity.
    fn render_planned(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(planned) = self.plan.get(index) else {
            return div().into_any_element();
        };
        let row = activity::row_of(&self.entries, planned);
        self.render_row(index, row, self.details.user_name.as_deref(), cx)
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
                        .child(if live {
                            // Turning, as the board's `spin`.
                            motion::spinner(
                                IconName::LoaderCircle.path(),
                                px(12.),
                                color::THREAD_RUNNING_TEXT,
                            )
                            .into_any_element()
                        } else {
                            Icon::new(if failure.is_some() {
                                IconName::CircleAlert
                            } else {
                                IconName::Check
                            })
                            .size(px(12.))
                            .text_color(rgb(if failure.is_some() {
                                color::THREAD_FAILED_TEXT
                            } else {
                                color::TEXT_FAINT
                            }))
                            .into_any_element()
                        }),
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
            Row::Prompt { kind, text, turn } => {
                let name = user.unwrap_or("You").to_owned();
                let images = self.thread(cx).and_then(|thread| {
                    let images = thread.turns.get(turn)?.images.clone();
                    self.render_turn_images(turn, &images)
                });
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
                    .child(bubble(text.to_owned(), images, false))
                    .into_any_element()
            }
            Row::Continued(kind) => div()
                .h_flex()
                .gap(px(6.))
                .px(px(12.))
                .py(px(6.))
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_MUTED))
                .child(
                    Icon::new(match kind {
                        TurnKind::Brief => IconName::ArrowRight,
                        _ => IconName::RotateCcw,
                    })
                    .size(px(12.)),
                )
                .child(match kind {
                    TurnKind::Resume => "Continued after the usage limit reset",
                    TurnKind::Brief => "Asked for a brief to implement from",
                    _ => "Retried",
                })
                .into_any_element(),
        }
    }

    fn render_final_message(&self, thread: &AgentThread, agent_name: SharedString) -> Option<Div> {
        if !matches!(thread.state, State::Finished | State::Completed) {
            return None;
        }
        // The `# Title` line that named the thread is its title above.
        let message = thread.final_message_body()?.to_owned();
        let what = if thread.brief().is_some() {
            "Brief"
        } else if thread.ending == Some(Ending::Answer) {
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
                )
                .children(handoff_note(thread)),
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
        let model = AgentThreads::global(cx).read(cx);
        let network = self.retry_network.unwrap_or_else(|| {
            suggested_retry(
                thread,
                model.agents(),
                &model.settings().sandbox.package_hosts,
            )
        });
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
                            .tooltip(crate::workspace::toolbar::agents::retry_tooltip(
                                thread, network,
                            ))
                            .loading(busy == Some(Busy::Retrying))
                            .disabled(busy.is_some())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.retry(network, window, cx)
                            })),
                    ),
                )
                .children(
                    crate::workspace::toolbar::agents::wider_network_warning(
                        thread.network,
                        network,
                    )
                    .map(|warning| {
                        div()
                            .debug_selector(|| "thread-retry-warning".into())
                            .h_flex()
                            .items_start()
                            .gap(px(8.))
                            .text_size(px(12.))
                            .line_height(relative(1.5))
                            .text_color(rgb(color::WARNING_TEXT))
                            .child(Icon::new(IconName::TriangleAlert).size(px(13.)).mt(px(2.)))
                            .child(div().flex_1().min_w_0().child(warning))
                    }),
                ),
        )
    }

    /// What the agent left uncommitted in the worktree, or that GitEZ no
    /// longer trusts it, and the ways forward: ask the agent to commit,
    /// complete (which asks first), or remove a worktree a completed
    /// thread kept. Known only from what the supervisor recorded; GitEZ
    /// never looks into the worktree itself.
    fn render_uncommitted(&self, thread: &AgentThread, cx: &mut Context<Self>) -> Option<Div> {
        let facts = self.model.read(cx).facts(&self.key);
        let completed = thread.state == State::Completed;
        if thread.state.is_active() || (completed && !facts.worktree_present) {
            return None;
        }
        let uncommitted = thread.uncommitted.as_ref().filter(|found| found.count > 0);
        if uncommitted.is_none() && facts.untrusted.is_none() {
            return None;
        }
        let busy = self.busy.as_ref().map(|(busy, _)| *busy);
        let mono = cx.theme().mono_font_family.clone();
        let title = match (completed, uncommitted_text(thread)) {
            (true, Some(text)) => format!("Worktree kept: {text}"),
            (true, None) => "Worktree kept".to_owned(),
            (false, Some(text)) => format!("{text} in the worktree"),
            (false, None) => "GitEZ no longer trusts the worktree".to_owned(),
        };
        let mut card = div()
            .debug_selector(|| "thread-uncommitted".into())
            .v_flex()
            .gap(px(8.))
            .px(px(14.))
            .py(px(12.))
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(color::WARNING_BORDER))
            .bg(rgba(color::THREAD_UNCOMMITTED))
            .child(
                div()
                    .h_flex()
                    .gap(px(8.))
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(Look::Uncommitted.text()))
                    .child(Icon::new(IconName::TriangleAlert).size(px(14.)))
                    .child(title),
            )
            .child(
                div()
                    .font_family(mono.clone())
                    .text_size(px(11.5))
                    .text_color(rgb(color::TEXT_SECONDARY))
                    .child(thread.worktree_display().to_string()),
            );
        if let Some(uncommitted) = uncommitted {
            if !uncommitted.paths.is_empty() {
                let more = (uncommitted.count as usize).saturating_sub(uncommitted.paths.len());
                card = card.child(
                    div()
                        .v_flex()
                        .gap(px(2.))
                        .font_family(mono.clone())
                        .text_size(px(11.5))
                        .text_color(rgb(color::TEXT_SECONDARY))
                        .children(
                            uncommitted
                                .paths
                                .iter()
                                .map(|path| div().child(path.trim().to_owned())),
                        )
                        .when(more > 0, |this| {
                            this.child(
                                div()
                                    .text_color(rgb(color::TEXT_MUTED))
                                    .child(format!("and {more} more")),
                            )
                        }),
                );
            }
            if let Some(error) = commit_error_text(thread) {
                card = card.child(
                    div()
                        .text_size(px(12.))
                        .line_height(relative(1.5))
                        .text_color(rgb(color::WARNING_TEXT))
                        .child(error),
                );
            }
        }
        let explanation = match (&facts.untrusted, completed) {
            (Some(problem), _) => complete::untrusted_text(thread, problem),
            (None, true) => "The thread is completed, but its worktree was kept so these \
                 changes are not lost. Remove it when you have saved what you need."
                .to_owned(),
            (None, false) if thread.pull_request.is_some() => {
                "They are not in the pull request. Ask the agent to commit them; merging or \
                 closing the pull request keeps the worktree while they are there."
                    .to_owned()
            }
            (None, false) => "They are not on the thread’s branch. Ask the agent to commit them, \
                 or Complete to decide whether to keep or discard them."
                .to_owned(),
        };
        card = card.child(
            div()
                .text_size(px(12.))
                .line_height(relative(1.5))
                .text_color(rgb(color::TEXT_SECONDARY))
                .child(explanation),
        );
        let mut buttons = div().h_flex().gap(px(8.));
        if !completed && can_reply(thread) && uncommitted.is_some() {
            buttons = buttons.child(
                Button::new("thread-ask-commit")
                    .debug_selector(|| "thread-ask-commit".into())
                    .h(px(28.))
                    .compact()
                    .icon(Icon::new(IconName::GitCommitHorizontal).size(px(13.)))
                    .label("Ask the agent to commit")
                    .tooltip("Puts a reply asking for a commit in the box below")
                    .disabled(busy.is_some())
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.reply.update(cx, |reply, cx| {
                            reply.set_value(ASK_TO_COMMIT, window, cx);
                            reply.focus(window, cx);
                        });
                        cx.notify();
                    })),
            );
        }
        if completed && facts.untrusted.is_none() {
            buttons = buttons.child(
                Button::new("thread-remove-worktree")
                    .debug_selector(|| "thread-remove-worktree".into())
                    .danger()
                    .h(px(28.))
                    .compact()
                    .icon(Icon::new(IconName::Delete).size(px(13.)))
                    .label("Remove worktree")
                    .tooltip("Asks first: removing it discards what is in it")
                    .loading(busy == Some(Busy::Completing))
                    .disabled(busy.is_some())
                    .on_click(cx.listener(|this, _, window, cx| this.remove_worktree(window, cx))),
            );
        }
        if let Some(worktree) = thread.worktree.clone() {
            buttons = buttons.child(
                Button::new("thread-uncommitted-reveal")
                    .ghost()
                    .h(px(28.))
                    .compact()
                    .icon(Icon::new(IconName::FolderOpen).size(px(13.)))
                    .label("Show in Finder")
                    .on_click(move |_, _, cx| cx.reveal_path(&worktree)),
            );
        }
        Some(card.child(buttons))
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
        let empty =
            self.reply.read(cx).value().trim().is_empty() && self.reply_images.read(cx).is_empty();
        let blocked = self.reply_blocked(thread, cx);
        let accepts = self.accepts_images(thread, cx);
        let dropped = self.reply_images.downgrade();
        let accepts_drop = accepts.clone();
        let implement = can_implement(thread);
        let (continues, highlight) = match (thread.pull_request, thread.branch.as_deref()) {
            (Some(number), Some(name)) => {
                let prefix = "Continues on ";
                let branch = prefix.len()..prefix.len() + name.len();
                let style = HighlightStyle {
                    color: Some(rgb(theme::branch_color(name)).into()),
                    ..HighlightStyle::default()
                };
                (
                    format!("{prefix}{name} · new commits update PR #{number}"),
                    Some((branch, style)),
                )
            }
            // The detail panel has room for a word or two beside the
            // buttons.
            _ if thread.mode == Mode::Discuss => (
                if self.wide && thread.auto {
                    "Continues the discussion · the agent answers, or hands off to implement"
                } else if self.wide {
                    "Continues the discussion · the agent changes nothing"
                } else {
                    "Changes nothing"
                }
                .to_owned(),
                None,
            ),
            _ => ("Continues in this thread’s worktree".to_owned(), None),
        };
        let implement_tooltip = if thread.brief().is_some() {
            "Opens the brief in the composer, to start an agent that implements it"
        } else {
            "Asks the agent for a brief of what you discussed, then opens it in the composer"
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
                        .on_drop(move |paths: &ExternalPaths, window, cx| {
                            dropped
                                .update(cx, |images, cx| {
                                    images.drop_paths(paths.paths(), &accepts_drop, window, cx)
                                })
                                .ok();
                        })
                        .child(
                            Textarea::new(&self.reply)
                                .appearance(false)
                                .disabled(busy.is_some())
                                .on_paste(super::composer::paste_into(&self.reply_images, accepts)),
                        )
                        .children(self.reply_images.update(cx, |images, cx| images.render(cx)))
                        .children(blocked.clone().map(|why| {
                            div()
                                .text_size(px(11.5))
                                .text_color(rgb(color::WARNING_TEXT))
                                .child(why)
                        }))
                        .child(
                            div()
                                .h_flex()
                                .gap(px(10.))
                                .child(continues)
                                .when(implement, |this| {
                                    this.child(
                                        Button::new("thread-implement")
                                            .debug_selector(|| "thread-implement".into())
                                            .h(px(28.))
                                            .label("Implement…")
                                            .tooltip(implement_tooltip)
                                            .loading(busy == Some(Busy::Briefing))
                                            .disabled(busy.is_some())
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.implement(window, cx)
                                            })),
                                    )
                                })
                                .child(
                                    Button::new("thread-send")
                                        .debug_selector(|| "thread-send".into())
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
                                        .disabled(empty || blocked.is_some() || busy.is_some())
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
        if let Some(thread) = self.thread(cx) {
            self.plan_rows(&thread);
        }
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
        let model_name = model_label(self.model.read(cx).agents(), &thread);
        let header = div()
            .v_flex()
            .gap(px(if self.wide { 12. } else { 10. }))
            .child(self.render_header(&thread, agent_name.clone(), model_name, cx))
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
            .children(self.render_uncommitted(&thread, cx))
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
        AgentThread, AgentThreadId, BRIEF_PROMPT, Ending, Failure, Mode, NewAgentThread, RunHeader,
        Start, State, Step, Store, Timestamp, TurnKind, TurnResult,
    };
    use gitez_sandbox::NetworkProfile;
    use gpui_kit::{Entity, TestAppContext, VisualTestContext, px, size};

    use super::{AwaitedBrief, Entry, Handoff, ThreadView, ThreadViewEvent, duration, model_label};
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
        repository_in_mode(Mode::Implement, prompt, transcript, finish)
    }

    /// A repository with one Discuss thread, finished by `finish`.
    fn repository_with_discussion(
        finish: impl FnOnce(&mut AgentThread),
    ) -> (tempfile::TempDir, PathBuf, AgentThreadId) {
        repository_in_mode(
            Mode::Discuss,
            "How should blame read renames?",
            &[
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"a.txt"}}]}}"#,
            ],
            finish,
        )
    }

    /// [`repository_with`] with a thread of `mode`.
    fn repository_in_mode(
        mode: Mode,
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
                    model: Some("opus".into()),
                    effort: None,
                    start: Start {
                        branch: "main".into(),
                        commit,
                    },
                    network: NetworkProfile::AgentOnly,
                    secrets: Vec::new(),
                    mode,
                    auto: false,
                    work_dir: (mode == Mode::Discuss).then(|| work_dir.clone()),
                    origin: None,
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
                    effort: None,
                    network: NetworkProfile::AgentOnly,
                    images: Vec::new(),
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
        // Only a discussion is handed over to an Implement thread.
        assert!(cx.debug_bounds("thread-implement").is_none());
        assert!(cx.debug_bounds("thread-working-tree").is_none());
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

    /// A finished Discuss thread: the working tree it reads instead of a
    /// branch and a worktree, a reply box, and "Implement…".
    #[gpui_kit::test]
    fn a_discussion_shows_what_it_reads_and_offers_implement(cx: &mut TestAppContext) {
        let (dir, common_dir, id) = repository_with_discussion(|thread| {
            thread.state = State::Finished;
            thread.ending = Some(Ending::Answer);
            thread.session_id = Some("session".into());
            thread.turns[0].result = Some(result("Follow the rename in the diff.", true));
        });
        let thread = Store::new(&common_dir).load(&id).expect("load");
        assert_eq!(
            (thread.branch.as_deref(), thread.worktree.as_deref()),
            (None, None)
        );
        let key = AgentThreadKey { common_dir, id };
        let (view, mut cx) = open(dir.path(), key, cx);
        assert!(cx.debug_bounds("thread-working-tree").is_some());
        assert!(cx.debug_bounds("thread-implement").is_some());
        assert!(cx.debug_bounds("thread-reply").is_some());
        assert!(cx.debug_bounds("thread-final-message").is_some());
        // Nothing of a worktree: none was made.
        assert!(cx.debug_bounds("thread-uncommitted").is_none());
        view.read_with(&cx, |view, cx| {
            let thread = view.thread(cx).expect("the thread is read");
            assert!(super::can_implement(&thread));
            assert_eq!(
                crate::workspace::toolbar::agents::complete_tooltip(&thread),
                "Ends the discussion. The thread stays readable; nothing in your working tree \
                 changes."
            );
        });
    }

    /// A discussion the agent cannot continue (it never answered) is not
    /// handed over: there is no one to write the brief.
    #[gpui_kit::test]
    fn a_failed_discussion_does_not_offer_implement(cx: &mut TestAppContext) {
        let (dir, common_dir, id) = repository_with_discussion(|thread| {
            thread.state = State::Finished;
            thread.ending = Some(Ending::Failed);
        });
        let key = AgentThreadKey { common_dir, id };
        let (_view, mut cx) = open(dir.path(), key, cx);
        assert!(cx.debug_bounds("thread-implement").is_none());
        assert!(cx.debug_bounds("thread-working-tree").is_some());
    }

    /// An Auto thread whose agent chose to implement shows its brief and
    /// what GitEZ did with it; "Implement…" opens the same brief.
    #[gpui_kit::test]
    fn an_auto_discussion_says_what_became_of_its_brief(cx: &mut TestAppContext) {
        let brief = "# Follow renames in blame\n\nRead the rename from the diff.";
        let (dir, common_dir, id) = repository_with_discussion(|thread| {
            thread.auto = true;
            thread.state = State::Finished;
            thread.ending = Some(Ending::Answer);
            thread.session_id = Some("session".into());
            thread.turns[0].result = Some(result(brief, true));
            thread.turns[0].handoff = Some(gitez_agent_thread::Handoff {
                claimed: Some(Timestamp::now()),
                thread: None,
                problem: Some("The branch main is gone.".into()),
            });
        });
        let key = AgentThreadKey { common_dir, id };
        let (view, mut cx) = open(dir.path(), key, cx);
        assert!(cx.debug_bounds("thread-handoff").is_some());
        assert!(cx.debug_bounds("thread-final-message").is_some());
        let seen = handoffs(&view, &mut cx);
        view.update_in(&mut cx, |view, window, cx| view.implement(window, cx));
        cx.run_until_parked();
        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].brief, brief);
    }

    /// The events `view` emits from now on.
    fn handoffs(
        view: &Entity<ThreadView>,
        cx: &mut VisualTestContext,
    ) -> Arc<std::sync::Mutex<Vec<Handoff>>> {
        let seen: Arc<std::sync::Mutex<Vec<Handoff>>> = Arc::default();
        cx.update(|_, cx| {
            let seen = seen.clone();
            cx.subscribe(view, move |_, event: &ThreadViewEvent, _| {
                if let ThreadViewEvent::Implement(handoff) = event {
                    seen.lock().expect("lock").push(handoff.clone());
                }
            })
            .detach();
        });
        seen
    }

    /// "Implement…" asks the agent for a brief in a turn of its own; when
    /// the turn ends, the brief goes to the composer with the discussion
    /// it came from.
    #[gpui_kit::test]
    fn implement_asks_for_a_brief_and_hands_it_over(cx: &mut TestAppContext) {
        let (dir, common_dir, id) = repository_with_discussion(|thread| {
            thread.state = State::Finished;
            thread.ending = Some(Ending::Answer);
            thread.session_id = Some("session".into());
            thread.turns[0].result = Some(result("Follow the rename in the diff.", true));
        });
        let key = AgentThreadKey {
            common_dir: common_dir.clone(),
            id: id.clone(),
        };
        let (view, mut cx) = open_in_root(dir.path(), key.clone(), cx);
        let seen = handoffs(&view, &mut cx);
        view.update_in(&mut cx, |view, window, cx| view.implement(window, cx));
        view.read_with(&cx, |view, _| {
            assert_eq!(
                view.awaited_brief,
                Some(AwaitedBrief {
                    turn: 1,
                    asked: false
                })
            );
        });
        cx.run_until_parked();
        // The turn is recorded. Tests have no supervisor to launch, so it
        // fails at that step, and nothing is handed over.
        let store = Store::new(&common_dir);
        let thread = store.load(&id).expect("load");
        let [_, brief] = &thread.turns[..] else {
            panic!("two turns, got {}", thread.turns.len());
        };
        assert_eq!(brief.kind, TurnKind::Brief);
        assert_eq!(brief.prompt, BRIEF_PROMPT);
        assert_eq!(thread.ending, Some(Ending::Failed));
        assert!(seen.lock().expect("lock").is_empty());

        // As the supervisor leaves it when the agent has answered.
        let answered = store
            .update(&id, |thread| {
                thread.state = State::Finished;
                thread.ending = Some(Ending::Answer);
                thread.turns[1].result = Some(result(
                    "# Follow renames in blame\n\nRead the rename from the diff of each commit.\n",
                    true,
                ));
                Ok(thread.clone())
            })
            .expect("update");
        view.update(&mut cx, |view, cx| {
            view.model.update(cx, |model, _| {
                model.replace_threads(&common_dir, vec![Arc::new(answered)])
            });
            view.awaited_brief = Some(AwaitedBrief {
                turn: 1,
                asked: true,
            });
            view.thread_changed(cx);
            assert_eq!(view.awaited_brief, None);
        });
        let expected = Handoff {
            origin: key,
            title: "How should blame read renames?".into(),
            brief: "# Follow renames in blame\n\nRead the rename from the diff of each commit."
                .into(),
            work_dir: Some(dir.path().join("repo")),
        };
        assert_eq!(*seen.lock().expect("lock"), std::slice::from_ref(&expected));
        // With the brief as the last answer, "Implement…" opens it again
        // without asking the agent.
        view.update_in(&mut cx, |view, window, cx| view.implement(window, cx));
        cx.run_until_parked();
        assert_eq!(*seen.lock().expect("lock"), [expected.clone(), expected]);
        assert_eq!(store.load(&id).expect("load").turns.len(), 2);
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

    /// A thread whose commit a hook refused says what it left, why, and
    /// offers to ask the agent to commit.
    #[gpui_kit::test]
    fn uncommitted_changes_show_a_card_and_complete_asks(cx: &mut TestAppContext) {
        let (dir, common_dir, id) = repository_with_thread(|thread| {
            thread.state = State::Finished;
            thread.ending = Some(Ending::Answer);
            thread.session_id = Some("session".into());
            thread.turns[0].result = Some(result("Wrote draft.md.", true));
            thread.uncommitted = Some(gitez_agent_thread::Uncommitted {
                checked: Timestamp::now(),
                count: 1,
                paths: vec!["?? draft.md".into()],
                commit_error: Some(
                    "GitEZ could not commit the changes the agent left:\npre-commit: lint first"
                        .into(),
                ),
            });
        });
        let made = Store::new(&common_dir).load(&id).expect("load");
        gitez_agent_thread::create_worktree(
            &dir.path().join("repo"),
            crate::agent_threads::thread_worktree(&made),
            crate::agent_threads::thread_branch(&made),
            &made.start.commit,
        )
        .expect("worktree");
        std::fs::write(
            crate::agent_threads::thread_worktree(&made).join("draft.md"),
            "a draft\n",
        )
        .expect("write");
        let key = AgentThreadKey { common_dir, id };
        let (view, mut cx) = open(dir.path(), key, cx);
        assert!(cx.debug_bounds("thread-uncommitted").is_some());
        assert!(cx.debug_bounds("thread-ask-commit").is_some());
        assert!(cx.debug_bounds("thread-remove-worktree").is_none());
        view.read_with(&cx, |view, cx| {
            let thread = view.thread(cx).expect("the thread is read");
            assert_eq!(super::state_label(&thread), "Uncommitted changes");
            assert_eq!(
                super::commit_error_text(&thread).as_deref(),
                Some("GitEZ could not commit the changes the agent left:\npre-commit: lint first.")
            );
        });
        assert!(cx.debug_bounds("thread-complete").is_some());
        // Clicking Complete asks first: `e2e_c1_uncommitted_keep_then_remove`
        // (it needs gpui-kit's Root for the dialog).
        assert!(
            crate::agent_threads::thread_worktree(&made)
                .join("draft.md")
                .exists()
        );
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
        shoot_then(name, pull_request, finish, |_, _| {});
    }

    /// [`shoot`], then `then` with the window, to change and save more.
    fn shoot_then(
        name: &str,
        pull_request: bool,
        finish: impl FnOnce(&mut AgentThread),
        then: impl FnOnce(&mut crate::screenshot::Shots, gpui_kit::AnyWindowHandle),
    ) {
        shoot_in_mode(Mode::Implement, PROMPT, name, pull_request, finish, then);
    }

    /// [`shoot_then`] with a thread of `mode` started with `prompt`.
    fn shoot_in_mode(
        mode: Mode,
        prompt: &str,
        name: &str,
        pull_request: bool,
        finish: impl FnOnce(&mut AgentThread),
        then: impl FnOnce(&mut crate::screenshot::Shots, gpui_kit::AnyWindowHandle),
    ) {
        if !crate::screenshot::enabled() {
            return;
        }
        let lines = transcript(STEPS);
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (dir, common_dir, id) = repository_in_mode(mode, prompt, &lines, finish);
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
        // A real worktree, as the supervisor makes it: a bare folder would
        // rightly be shown as one GitEZ no longer trusts. A discussion has
        // none.
        if thread.state != State::Completed && thread.own().is_some() {
            if pull_request {
                let path = crate::agent_threads::thread_worktree(&thread)
                    .to_str()
                    .expect("a UTF-8 path");
                git(
                    &work_dir,
                    &[
                        "worktree",
                        "add",
                        "-q",
                        path,
                        crate::agent_threads::thread_branch(&thread),
                    ],
                );
            } else {
                gitez_agent_thread::create_worktree(
                    &work_dir,
                    crate::agent_threads::thread_worktree(&thread),
                    crate::agent_threads::thread_branch(&thread),
                    &thread.start.commit,
                )
                .expect("worktree");
            }
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
        then(&mut shots, window);
    }

    /// The detail panel, at its default width.
    fn panel() -> gpui_kit::Bounds<gpui_kit::Pixels> {
        gpui_kit::Bounds {
            origin: gpui_kit::point(px(1080.), px(48.)),
            size: size(px(360.), px(824.)),
        }
    }

    /// Hosts the real Claude Code contacts in every run and does not need.
    const OPTIONAL_HOSTS: [&str; 2] = [
        "mcp-proxy.anthropic.com",
        "http-intake.logs.us5.datadoghq.com",
    ];

    /// Every real run: only Claude Code's optional hosts were blocked. No
    /// count in the warning colour; expanded, they are listed muted with
    /// what they are for.
    #[test]
    fn screenshot_thread_optional_hosts() {
        shoot_then(
            "optional-hosts",
            false,
            |thread| {
                thread.state = State::Finished;
                thread.ending = Some(Ending::Answer);
                thread.session_id = Some("session".into());
                let turn = &mut thread.turns[0];
                turn.started = Some(minutes_ago(30));
                turn.ended = Some(minutes_ago(27));
                let mut done = result(ANSWER, true);
                done.steps = 9;
                done.blocked_hosts = OPTIONAL_HOSTS.map(str::to_owned).into();
                turn.result = Some(done);
            },
            |shots, window| {
                // "No blocked requests · 2 not needed", in the panel.
                shots.click(window, gpui_kit::point(px(1150.), px(265.)));
                shots.save_region(window, panel(), "thread-optional-hosts-open-panel");
            },
        );
    }

    /// A host the build needed was blocked besides the optional ones: the
    /// count in the warning colour is that one only, and the list is open.
    #[test]
    fn screenshot_thread_blocked_and_optional_hosts() {
        shoot("blocked-and-optional", false, |thread| {
            let turn = &mut thread.turns[0];
            turn.started = Some(minutes_ago(12));
            turn.ended = None;
            thread.fail(
                Step::Agent,
                "cargo could not download the crates it needs.\n\nThe sandbox refused \
                 connections to: static.crates.io.",
                minutes_ago(5),
            );
            let mut failed = result("", false);
            failed.steps = 9;
            failed.exit_code = Some(1);
            failed.blocked_hosts = vec![
                OPTIONAL_HOSTS[0].into(),
                "static.crates.io".into(),
                OPTIONAL_HOSTS[1].into(),
            ];
            thread.turns[0].result = Some(failed);
        });
    }

    /// The header names the model as the agent does, an unknown alias as
    /// stored, and nothing for a thread without one.
    #[test]
    fn model_label_names_the_model() {
        let (_dir, common_dir, id) = repository_with_discussion(|_| {});
        let mut thread = Store::new(&common_dir).load(&id).expect("load");
        let agents = gitez_agent::Registry::builtin();
        assert_eq!(
            model_label(&agents, &thread).as_deref(),
            Some("Opus"),
            "a known alias"
        );
        thread.model = Some("claude-opus-5-5".into());
        assert_eq!(
            model_label(&agents, &thread).as_deref(),
            Some("claude-opus-5-5")
        );
        thread.model = Some(String::new());
        assert_eq!(model_label(&agents, &thread), None);
        thread.model = None;
        assert_eq!(model_label(&agents, &thread), None);
        // The effort asked for follows the model.
        thread.effort = Some("xhigh".into());
        assert_eq!(
            model_label(&agents, &thread).as_deref(),
            Some("Extra high effort")
        );
        thread.model = Some("opus".into());
        assert_eq!(
            model_label(&agents, &thread).as_deref(),
            Some("Opus · Extra high effort")
        );
    }

    fn minutes_ago(minutes: i64) -> Timestamp {
        Timestamp::now() - jiff::SignedDuration::from_mins(minutes)
    }

    /// A thread with a long prompt and a long answer, in the detail panel
    /// and in Home's wide thread view, each in a 700-point-tall window:
    /// both scroll to their end.
    #[test]
    fn screenshot_scroll_thread_long() {
        if !crate::screenshot::enabled() {
            return;
        }
        let prompt = [PROMPT; 6].join("\n\n");
        let answer = [ANSWER; 8].join("\n\n");
        let lines = transcript(STEPS);
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (dir, common_dir, id) = repository_with(&prompt, &lines, |thread| {
            thread.state = State::Finished;
            thread.ending = Some(Ending::Answer);
            thread.session_id = Some("session".into());
            let turn = &mut thread.turns[0];
            turn.started = Some(minutes_ago(30));
            turn.ended = Some(minutes_ago(27));
            turn.result = Some(result(&answer, true));
        });
        let work_dir = dir.path().join("repo");
        git(&work_dir, &["config", "user.name", "Ada Lovelace"]);
        let thread = Store::new(&common_dir)
            .load(&id)
            .expect("the thread is read");
        gitez_agent_thread::create_worktree(
            &work_dir,
            crate::agent_threads::thread_worktree(&thread),
            crate::agent_threads::thread_branch(&thread),
            &thread.start.commit,
        )
        .expect("worktree");
        let mut shots =
            crate::screenshot::Shots::from_env(&dir.path().join("data")).expect("enabled");
        let tall = size(px(1440.), px(700.));
        let (window, _) = shots.open_workspace_sized(&work_dir, tall);
        shots.dispatch(
            window,
            crate::agent_threads::OpenAgentThread {
                common_dir: common_dir.clone(),
                id: id.clone(),
            },
        );
        shots.save(window, "scroll-thread-panel-top");
        for _ in 0..10 {
            shots.scroll(window, gpui_kit::point(px(1260.), px(400.)), 100.);
        }
        shots.save(window, "scroll-thread-panel-end");
        let key = AgentThreadKey {
            common_dir,
            id: thread.id.clone(),
        };
        let (wide, _) = shots.open(tall, |window, cx| {
            gpui_kit::AppContext::new(cx, |cx| ThreadView::new(key, true, window, cx))
        });
        shots.save(wide, "scroll-thread-wide-top");
        for _ in 0..10 {
            shots.scroll(wide, gpui_kit::point(px(720.), px(350.)), 100.);
        }
        shots.save(wide, "scroll-thread-wide-end");
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

    /// A thread whose first prompt and reply sent screenshots, and an image
    /// pasted into the reply box.
    #[test]
    fn screenshot_thread_images() {
        if !crate::screenshot::enabled() {
            return;
        }
        let lines = transcript(STEPS);
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (dir, common_dir, id) = repository_with(PROMPT, &lines, |thread| {
            thread.state = State::Finished;
            thread.ending = Some(Ending::Answer);
            thread.session_id = Some("session".into());
            let turn = &mut thread.turns[0];
            turn.started = Some(minutes_ago(30));
            turn.ended = Some(minutes_ago(27));
            turn.result = Some(result(ANSWER, true));
        });
        let store = Store::new(&common_dir);
        let add = |images: &[Vec<u8>]| {
            let prepared: Vec<_> = images
                .iter()
                .map(|bytes| gitez_agent_thread::prepare_image(bytes).expect("prepared"))
                .collect();
            store
                .add_images(&id, &prepared, minutes_ago(30))
                .expect("images added")
        };
        let first = add(&[
            crate::screenshot::sample_image(1600, 1000, [95, 168, 245]),
            crate::screenshot::sample_image(600, 1400, [232, 176, 75]),
        ]);
        let second = add(&[crate::screenshot::sample_image(1200, 800, [79, 198, 176])]);
        store
            .update(&id, |thread| {
                thread.turns[0].images = first;
                thread.request_turn_with_images(
                    TurnKind::Reply,
                    "The blame column is cut off here.".into(),
                    second,
                    minutes_ago(20),
                )?;
                let turn = &mut thread.turns[1];
                turn.started = Some(minutes_ago(20));
                turn.ended = Some(minutes_ago(18));
                turn.result = Some(result("Widened the column; it fits now.", true));
                thread.state = State::Finished;
                thread.ending = Some(Ending::Answer);
                Ok(())
            })
            .expect("update");
        let work_dir = dir.path().join("repo");
        let thread = store.load(&id).expect("the thread is read");
        gitez_agent_thread::create_worktree(
            &work_dir,
            crate::agent_threads::thread_worktree(&thread),
            crate::agent_threads::thread_branch(&thread),
            &thread.start.commit,
        )
        .expect("worktree");
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
        shots.save_region(window, panel(), "thread-images-panel");
        // The reply's text, then the activity with the reply's image.
        shots.click(window, gpui_kit::point(px(1200.), px(781.)));
        shots.paste_image(
            window,
            crate::screenshot::sample_image(900, 900, [192, 140, 240]),
        );
        shots.save_region(window, panel(), "thread-images-reply-panel");
        shots.click(window, gpui_kit::point(px(1200.), px(535.)));
        shots.save_region(window, panel(), "thread-images-activity-panel");
        // The wide layout of Home.
        let key = AgentThreadKey { common_dir, id };
        let (wide, _) = shots.open(crate::screenshot::window_size(), |window, cx| {
            gpui_kit::AppContext::new(cx, |cx| ThreadView::new(key, true, window, cx))
        });
        shots.save(wide, "thread-images-wide");
        shots.click(wide, gpui_kit::point(px(720.), px(455.)));
        shots.scroll(wide, gpui_kit::point(px(720.), px(500.)), 40.);
        shots.save(wide, "thread-images-wide-activity");
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

    const DISCUSSION: &str = "How should blame follow a file that was renamed? I want to \
        keep it fast on big files.";

    const DISCUSSED: &str = "Blame can follow a rename without a second pass.\n\n\
        - gix reports renames in each commit’s diff; `blame.rs` would carry the old path \
        back with the lines it is still looking for.\n\
        - The cost is one diff per commit that touches the file, which the view already \
        pays.\n\n\
        I changed nothing. Use **Implement…** when you want this built.";

    const BRIEF: &str = "# Follow renames in blame\n\n\
        **Goal.** `blame` in gitez-git follows a file across renames.\n\n\
        **Agreed.** Take the rename from each commit’s diff; no second pass over the \
        history.\n\n\
        **Look at.** `crates/gitez-git/src/blame.rs`, `tests/blame.rs`.\n\n\
        **Verify.** A test with a file renamed twice.\n\n\
        **Out of scope.** Copies, and the view.";

    /// A Discuss thread that answered: what it reads instead of a branch
    /// and a worktree, and "Implement…" beside Send.
    #[test]
    fn screenshot_thread_discussion() {
        shoot_in_mode(
            Mode::Discuss,
            DISCUSSION,
            "discussion",
            false,
            |thread| {
                thread.state = State::Finished;
                thread.ending = Some(Ending::Answer);
                thread.session_id = Some("session".into());
                let turn = &mut thread.turns[0];
                turn.started = Some(minutes_ago(12));
                turn.ended = Some(minutes_ago(10));
                let mut done = result(DISCUSSED, true);
                done.steps = 9;
                turn.result = Some(done);
            },
            |_, _| {},
        );
    }

    /// A Discuss thread whose last answer is its brief, and the composer
    /// "Implement…" opens with it.
    #[test]
    fn screenshot_thread_discussion_brief() {
        shoot_in_mode(
            Mode::Discuss,
            DISCUSSION,
            "discussion-brief",
            false,
            |thread| {
                thread.state = State::Finished;
                thread.ending = Some(Ending::Answer);
                thread.session_id = Some("session".into());
                let turn = &mut thread.turns[0];
                turn.started = Some(minutes_ago(12));
                turn.ended = Some(minutes_ago(10));
                turn.result = Some(result(DISCUSSED, true));
                thread
                    .request_turn(TurnKind::Brief, BRIEF_PROMPT.into(), minutes_ago(3))
                    .expect("a brief can be asked for");
                thread.state = State::Finished;
                thread.ending = Some(Ending::Answer);
                let turn = &mut thread.turns[1];
                turn.started = Some(minutes_ago(3));
                turn.ended = Some(minutes_ago(2));
                turn.result = Some(result(BRIEF, true));
            },
            |shots, window| {
                shots.click_id(window, "thread-implement");
                assert!(shots.has_id(window, "composer-send"), "the composer opens");
                assert!(
                    shots.has_id(window, "composer-origin-remove"),
                    "the brief names its discussion"
                );
                shots.save(window, "thread-discussion-handoff");
            },
        );
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

    /// A running thread whose transcript has `steps` steps, half of them
    /// text, so none collapse: as many rows as steps.
    fn long_thread(steps: usize) -> (tempfile::TempDir, AgentThreadKey) {
        let steps: Vec<(String, String)> = (0..steps)
            .map(|step| {
                if step % 2 == 0 {
                    (
                        "text".to_owned(),
                        format!("Step {step}: checking `the layout` again."),
                    )
                } else {
                    (
                        "Bash".to_owned(),
                        format!("cargo test -p gitez-git step_{step}"),
                    )
                }
            })
            .collect();
        let steps: Vec<(&str, &str)> = steps
            .iter()
            .map(|(kind, value)| (kind.as_str(), value.as_str()))
            .collect();
        let lines = transcript(&steps);
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (dir, common_dir, id) = repository_with(PROMPT, &lines, |thread| {
            thread.state = State::Running;
            thread.turns[0].ended = None;
        });
        (dir, AgentThreadKey { common_dir, id })
    }

    /// Opens the activity of `key` and times drawing it `frames` times.
    fn draw_activity(
        dir: &Path,
        key: AgentThreadKey,
        virtual_from: usize,
        cx: &mut TestAppContext,
    ) -> (usize, std::time::Duration, bool) {
        let (view, mut cx) = open(dir, key, cx);
        view.update(&mut cx, |view, cx| {
            view.virtual_from = virtual_from;
            cx.notify();
        });
        cx.run_until_parked();
        let frames = 5;
        let started = std::time::Instant::now();
        for _ in 0..frames {
            view.update(&mut cx, |_, cx| cx.notify());
            cx.update(|window, cx| window.draw(cx).clear(cx));
        }
        let each = started.elapsed() / frames;
        let rows = view.read_with(&cx, |view, _| view.plan.len());
        let following = view.read_with(&cx, |view, _| view.rows.is_following_tail());
        (rows, each, following)
    }

    /// M1: a long transcript is a virtual list that follows the newest
    /// step while the agent works.
    #[gpui_kit::test]
    fn a_long_activity_is_virtual_and_follows_the_newest_step(cx: &mut TestAppContext) {
        let (dir, key) = long_thread(400);
        let (rows, _, following) = draw_activity(dir.path(), key, super::VIRTUAL_FROM, cx);
        assert_eq!(rows, 400);
        assert!(following, "a live list follows its newest row");
    }

    /// Scrolling over a long, virtual activity scrolls only the activity,
    /// never the thread around it.
    #[gpui_kit::test]
    fn scrolling_a_long_activity_leaves_the_thread_in_place(cx: &mut TestAppContext) {
        let (dir, key) = long_thread(400);
        let (view, mut cx) = open(dir.path(), key, &mut *cx);
        let bounds = cx
            .debug_bounds("thread-activity-list")
            .expect("the activity is a virtual list");
        let position = bounds.origin + gpui_kit::point(px(40.), px(20.));
        let scroll = |cx: &mut VisualTestContext, lines: f32| {
            cx.simulate_event(gpui_kit::ScrollWheelEvent {
                position,
                delta: gpui_kit::ScrollDelta::Lines(gpui_kit::point(0., lines)),
                ..Default::default()
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
        };
        let at = |cx: &VisualTestContext| {
            let top = view.read_with(cx, |view, _| view.rows.logical_scroll_top());
            (top.item_ix, top.offset_in_item)
        };
        let top = at(&cx);
        // Up, away from the newest step: the list moves, the thread does not.
        scroll(&mut cx, 5.);
        assert_ne!(at(&cx), top, "the activity scrolls");
        assert_eq!(
            view.read_with(&cx, |view, _| view.scroll.offset().y),
            px(0.)
        );
        // Down past the newest step: the list stops, and so does the thread.
        scroll(&mut cx, -50.);
        assert_eq!(
            view.read_with(&cx, |view, _| view.scroll.offset().y),
            px(0.)
        );
    }

    /// Times drawing the activity of 2,000 and 20,000 steps whole and
    /// virtual: `GITEZ_MEASURE=1 cargo test -p gitez measure_activity --
    /// --nocapture`.
    #[gpui_kit::test]
    fn measure_activity(cx: &mut TestAppContext) {
        if std::env::var_os("GITEZ_MEASURE").is_none() {
            return;
        }
        for steps in [2_000, 20_000] {
            for (name, virtual_from) in [("whole", usize::MAX), ("virtual", super::VIRTUAL_FROM)] {
                let (dir, key) = long_thread(steps);
                let (rows, each, _) = draw_activity(dir.path(), key, virtual_from, cx);
                eprintln!("activity {steps} steps ({rows} rows), {name}: {each:?} a frame");
            }
        }
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let pixels = image::RgbImage::from_pixel(width, height, image::Rgb([200, 90, 40]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(pixels)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("encode");
        bytes.into_inner()
    }

    /// [`open`], in gpui-kit's Root as in the app: a failed reply shows a
    /// notice, which needs it.
    fn open_in_root(
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
        cx.run_until_parked();
        let options = gpui_kit::WindowOptions {
            window_bounds: Some(gpui_kit::WindowBounds::Windowed(gpui_kit::Bounds {
                origin: gpui_kit::point(px(0.), px(0.)),
                size: size(px(400.), px(900.)),
            })),
            ..gpui_kit::WindowOptions::default()
        };
        let (window, view) = cx
            .update(|cx| {
                gpui_kit::open_window(options, cx, |window, cx| {
                    gpui_kit::AppContext::new(cx, |cx| ThreadView::new(key, false, window, cx))
                })
            })
            .expect("window open");
        let cx = VisualTestContext::from_window(window, cx);
        cx.run_until_parked();
        (view, cx)
    }

    /// The images a turn sent are shown as thumbnails under its prompt,
    /// read in the background from the thread's folder.
    #[gpui_kit::test]
    fn a_turn_shows_the_images_it_sent(cx: &mut TestAppContext) {
        let (dir, common_dir, id) = repository_with_thread(|thread| {
            thread.state = State::Finished;
            thread.ending = Some(Ending::Answer);
            thread.session_id = Some("session".into());
            thread.turns[0].result = Some(result("A red square.", true));
        });
        let store = Store::new(&common_dir);
        let prepared = gitez_agent_thread::prepare_image(&png(300, 200)).expect("prepared");
        let listed = store
            .add_images(&id, &[prepared], Timestamp::now())
            .expect("images added");
        store
            .update(&id, |thread| {
                thread.turns[0].images = listed.clone();
                Ok(())
            })
            .expect("update");
        let key = AgentThreadKey { common_dir, id };
        let (view, mut cx) = open(dir.path(), key, cx);
        assert!(cx.debug_bounds("turn-0-images").is_some());
        view.read_with(&cx, |view, _| {
            assert!(matches!(
                view.thumbnails.get(&listed[0].name),
                Some(super::TurnThumbnail::Ready(_))
            ));
        });
    }

    /// An image pasted into the reply box is attached, not typed; Send
    /// records the reply with it, and with no text.
    #[gpui_kit::test]
    fn a_pasted_image_is_sent_with_the_reply(cx: &mut TestAppContext) {
        let (dir, common_dir, id) = repository_with_thread(|thread| {
            thread.state = State::Finished;
            thread.ending = Some(Ending::Answer);
            thread.session_id = Some("session".into());
            thread.turns[0].result = Some(result("Done.", true));
        });
        let key = AgentThreadKey {
            common_dir: common_dir.clone(),
            id: id.clone(),
        };
        let (view, mut cx) = open_in_root(dir.path(), key, cx);
        let bytes = png(40, 30);
        cx.write_to_clipboard(gpui_kit::ClipboardItem {
            entries: vec![gpui_kit::ClipboardEntry::Image(
                gpui_kit::Image::from_bytes(gpui_kit::ImageFormat::Png, bytes.clone()),
            )],
        });
        view.update_in(&mut cx, |view, window, cx| {
            view.reply.update(cx, |reply, cx| reply.focus(window, cx));
        });
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-v"
        } else {
            "ctrl-v"
        });
        cx.run_until_parked();
        view.read_with(&cx, |view, cx| {
            assert_eq!(view.reply_images.read(cx).len(), 1);
            assert!(view.reply.read(cx).value().is_empty());
            let thread = view.thread(cx).expect("the thread is read");
            assert_eq!(view.reply_blocked(&thread, cx), None);
        });
        assert!(cx.debug_bounds("reply-images").is_some());
        view.update_in(&mut cx, |view, window, cx| view.send_reply(window, cx));
        cx.run_until_parked();
        // Tests have no supervisor to launch: the reply is recorded, then
        // fails at that step.
        let thread = Store::new(&common_dir).load(&id).expect("load");
        let [_, reply] = &thread.turns[..] else {
            panic!("two turns, got {}", thread.turns.len());
        };
        assert_eq!(reply.kind, TurnKind::Reply);
        assert_eq!(reply.prompt, "");
        let [image] = &reply.images[..] else {
            panic!("one image, got {}", reply.images.len());
        };
        assert_eq!(image.media_type, gitez_agent_thread::ImageType::Png);
        assert_eq!((image.width, image.height), (40, 30));
        assert_eq!(
            Store::new(&common_dir)
                .read_image(&id, image)
                .expect("the image is kept"),
            bytes
        );
    }

    #[test]
    fn durations_read_as_minutes_and_hours() {
        assert_eq!(duration(10), "under a minute");
        assert_eq!(duration(14 * 60), "14 min");
        assert_eq!(duration(2 * 3600), "2 h");
        assert_eq!(duration(2 * 3600 + 5 * 60), "2 h 5 min");
    }
}
