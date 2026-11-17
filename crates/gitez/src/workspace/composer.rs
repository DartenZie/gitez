//! The "Ask an agent" composer: a prompt, the mode of the thread it starts
//! (Discuss or Implement) and the pickers (agent, model, starting branch),
//! as in the `C-1-Start` board; the sandbox network and the secrets are
//! Settings' defaults. In a repository window it opens as a card
//! under the "Ask an agent…" row of the history; Home shows it as its
//! launcher (`C-5-Home`), with a Repository picker in front.
//!
//! The composer only gathers a [`StartRequest`] and emits it; whoever shows
//! it starts the thread with [`AgentThreads::start`]. Images pasted or
//! dropped into it go with the prompt ([`PromptImages`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gitez_agent::{AgentStatus, Availability, LoginStatus};
use gitez_agent_thread::{Mode, PreparedImage, Start, slug, title_from_request};
use gitez_git::Oid;
use gitez_sandbox::NetworkProfile;
use gitez_settings::Settings;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::agent_thread::{Handoff, agent_mark, branch_chip, on_accent};
use super::labels;
use super::prompt_images::{self, Accepts, PromptImages};
use crate::agent_threads::{AgentThreadKey, AgentThreads, OpenSettings, StartRequest};
use crate::theme::{self, color};

actions!(composer, [Cancel]);

const CONTEXT: &str = "Composer";

/// Key bindings of the composer. `⌘↵` sends: the prompt field reports it.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![KeyBinding::new("escape", Cancel, Some(CONTEXT))]
}

/// Where the composer starts threads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerMode {
    /// In the window of this repository.
    Repository {
        work_dir: PathBuf,
        common_dir: PathBuf,
        name: SharedString,
        /// The local branches and their tips.
        branches: Vec<(String, Oid)>,
        /// The branch checked out; `None` with a detached `HEAD`.
        current: Option<String>,
        /// A bare repository: nothing to start from, which the composer
        /// says.
        bare: bool,
    },
    /// Home: in a repository picked with the Repository picker.
    Home,
}

/// What the composer asks of whoever shows it.
pub enum ComposerEvent {
    /// Start this thread. The prompt field is already empty.
    Submit(Box<StartRequest>),
    /// Esc: close the composer.
    Cancel,
}

/// The repository threads start in, once known.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    work_dir: PathBuf,
    common_dir: PathBuf,
    name: SharedString,
    branches: Vec<(String, Oid)>,
    current: Option<String>,
    bare: bool,
    /// Files with uncommitted changes in the main worktree, which a thread
    /// does not start with; `None` while not known.
    uncommitted: Option<usize>,
}

/// What was found out about the agents and the sandbox, in the background.
#[derive(Debug, Clone)]
struct Environment {
    agents: Vec<AgentCheck>,
    sandbox: gitez_sandbox::Status,
}

#[derive(Debug, Clone)]
struct AgentCheck {
    status: AgentStatus,
    /// Only for an installed agent.
    login: Option<LoginStatus>,
}

/// Something that keeps Send disabled, or that the user should know.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Problem {
    text: SharedString,
    /// Parts of `text` in the mono font (a branch, a command), with the
    /// colour of a branch.
    mono: Vec<(std::ops::Range<usize>, Option<u32>)>,
    severity: Severity,
    /// Whether Settings can fix it.
    settings: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    /// Send stays disabled; said plainly.
    Blocking,
    /// Send stays disabled, but nothing is wrong (an empty prompt).
    Hint,
    /// Send works; the thread may still fail.
    Warning,
}

/// A sentence in which some parts are set in the mono font, as the composer
/// shows a branch or a command; never with Markdown backticks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Sentence {
    pub(crate) text: String,
    pub(crate) mono: Vec<(std::ops::Range<usize>, Option<u32>)>,
}

impl Sentence {
    fn plain(mut self, text: &str) -> Self {
        self.text.push_str(text);
        self
    }

    /// `text` in the mono font, in `color` (a branch's) or the sentence's.
    fn mono(mut self, text: &str, color: Option<u32>) -> Self {
        let start = self.text.len();
        self.text.push_str(text);
        self.mono.push((start..self.text.len(), color));
        self
    }
}

impl From<String> for Sentence {
    fn from(text: String) -> Self {
        Sentence::default().plain(&text)
    }
}

/// A repository in Home's Repository picker.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RepositoryChoice {
    path: PathBuf,
    name: SharedString,
    /// Why it cannot be picked (it was moved or deleted).
    unavailable: Option<SharedString>,
}

/// The "Ask an agent" composer.
pub struct Composer {
    mode: ComposerMode,
    target: Option<Target>,
    /// Home: reading the repository picked.
    _loading: Option<Task<()>>,
    target_error: Option<SharedString>,
    model: Entity<AgentThreads>,
    prompt: Entity<TextareaState>,
    /// Images pasted or dropped into the prompt.
    images: Entity<PromptImages>,
    focus_handle: FocusHandle,
    agent: String,
    /// `None`: the agent entry's default.
    model_alias: Option<String>,
    /// What the thread is for; `None`: the mode of the thread started
    /// last.
    thread_mode: Option<Mode>,
    /// The Discuss thread whose brief the prompt is, and its title.
    origin: Option<(AgentThreadKey, SharedString)>,
    /// `None`: the branch checked out.
    from: Option<String>,
    environment: Option<Environment>,
    /// The settings the environment was checked with, and
    /// [`AgentThreads::environment`] then.
    checked_with: Option<(Arc<Settings>, u64)>,
    _checking: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    pub fn new(mode: ComposerMode, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = AgentThreads::global(cx);
        let placeholder = match mode {
            ComposerMode::Repository { .. } => {
                "Say what the agent should do, or ask a question about this repository"
            }
            ComposerMode::Home => "Say what the agent should do, or ask about a repository",
        };
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 10)
                .placeholder(placeholder)
        });
        let images = cx.new(|_| PromptImages::new("composer"));
        let subscriptions = vec![
            cx.observe(&images, |_, _, cx| cx.notify()),
            cx.observe_global::<crate::project_icon::Icons>(|_, cx| cx.notify()),
            cx.subscribe_in(&prompt, window, |this, _, event, window, cx| match event {
                InputEvent::PressEnter {
                    secondary: true, ..
                } => this.send(window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            }),
            // Running counts, settings and secrets.
            cx.observe(&model, |this, model, cx| {
                let settings = model.read(cx).settings().clone();
                let environment = model.read(cx).environment();
                if this.checked_with.as_ref().is_some_and(|(checked, known)| {
                    !Arc::ptr_eq(checked, &settings) || *known != environment
                }) {
                    this.check(cx);
                }
                cx.notify();
            }),
        ];
        let agent = model.read(cx).agents().all().first().map_or_else(
            || gitez_agent::ClaudeCode::ID.to_owned(),
            |agent| agent.id().to_owned(),
        );
        let mut composer = Self {
            mode: ComposerMode::Home,
            target: None,
            _loading: None,
            target_error: None,
            model,
            prompt,
            images,
            focus_handle: cx.focus_handle(),
            agent,
            model_alias: None,
            thread_mode: None,
            origin: None,
            from: None,
            environment: None,
            checked_with: None,
            _checking: None,
            _subscriptions: subscriptions,
        };
        composer.set_mode(mode, cx);
        if composer.target.is_none()
            && let Some(path) = composer
                .repository_choices(cx)
                .into_iter()
                .find(|choice| choice.unavailable.is_none())
                .map(|choice| choice.path)
        {
            composer.choose_repository(path, cx);
        }
        composer
    }

    /// Shows `mode`: in a repository window, its branches after they were
    /// read again. Choices that still apply are kept.
    pub fn set_mode(&mut self, mode: ComposerMode, cx: &mut Context<Self>) {
        if let ComposerMode::Repository {
            work_dir,
            common_dir,
            name,
            branches,
            current,
            bare,
        } = &mode
        {
            let uncommitted = self.target.as_ref().and_then(|target| target.uncommitted);
            let target = Target {
                work_dir: work_dir.clone(),
                common_dir: common_dir.clone(),
                name: name.clone(),
                branches: branches.clone(),
                current: current.clone(),
                bare: *bare,
                uncommitted,
            };
            self.set_target(target);
        }
        self.mode = mode;
        cx.notify();
    }

    fn set_target(&mut self, target: Target) {
        let same_repository = self
            .target
            .as_ref()
            .is_some_and(|known| known.common_dir == target.common_dir);
        if !same_repository {
            self.from = None;
        }
        // A brief is for the repository it was discussed in.
        self.origin = self
            .origin
            .take()
            .filter(|(origin, _)| origin.common_dir == target.common_dir);
        self.from = self
            .from
            .take()
            .filter(|from| target.branches.iter().any(|(name, _)| name == from));
        self.target = Some(target);
        self.target_error = None;
    }

    /// How many files have uncommitted changes in the repository's main
    /// worktree, as the window read them.
    pub fn set_uncommitted(&mut self, count: Option<usize>, cx: &mut Context<Self>) {
        if let Some(target) = &mut self.target
            && target.uncommitted != count
        {
            target.uncommitted = count;
            cx.notify();
        }
    }

    /// Puts the cursor in the prompt.
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.prompt
            .update(cx, |prompt, cx| prompt.focus(window, cx));
    }

    /// Replaces the prompt, as after a thread that could not start.
    pub fn set_prompt(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt
            .update(cx, |prompt, cx| prompt.set_value(text, window, cx));
        cx.notify();
    }

    /// What the thread started is for: the mode picked, else the mode of
    /// the thread started last.
    pub(crate) fn thread_mode(&self, cx: &App) -> Mode {
        self.thread_mode
            .unwrap_or_else(|| self.settings(cx).threads.last_mode)
    }

    pub fn set_thread_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.thread_mode = Some(mode);
        cx.notify();
    }

    /// Names the Discuss thread whose brief the prompt is (with its
    /// title), or none: the thread started says which it implements.
    pub fn set_origin(
        &mut self,
        origin: Option<(AgentThreadKey, SharedString)>,
        cx: &mut Context<Self>,
    ) {
        self.origin = origin;
        cx.notify();
    }

    /// What is written in the prompt.
    #[cfg(test)]
    pub(crate) fn prompt_text(&self, cx: &App) -> String {
        self.prompt.read(cx).value().to_string()
    }

    /// The Discuss thread whose brief the prompt is.
    #[cfg(test)]
    pub(crate) fn origin(&self) -> Option<&AgentThreadKey> {
        self.origin.as_ref().map(|(origin, _)| origin)
    }

    /// Takes what a Discuss thread hands over: its brief as the prompt of
    /// an Implement thread that names it, for the user to read, change and
    /// send. Home picks the discussion's repository.
    pub fn hand_over(&mut self, handoff: &Handoff, window: &mut Window, cx: &mut Context<Self>) {
        self.thread_mode = Some(Mode::Implement);
        self.origin = Some((handoff.origin.clone(), handoff.title.clone()));
        let picked = self
            .target
            .as_ref()
            .is_some_and(|target| target.common_dir == handoff.origin.common_dir);
        if self.mode == ComposerMode::Home
            && !picked
            && let Some(work_dir) = &handoff.work_dir
        {
            self.choose_repository(work_dir.clone(), cx);
        }
        self.set_prompt(handoff.brief.clone(), window, cx);
        self.focus(window, cx);
    }

    /// Puts back the images of a thread that could not start.
    pub fn restore_images(
        &mut self,
        images: Vec<Arc<PreparedImage>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.images
            .update(cx, |attached, cx| attached.restore(images, window, cx));
    }

    /// The images attached to the prompt.
    #[cfg(test)]
    pub(crate) fn images(&self) -> &Entity<PromptImages> {
        &self.images
    }

    /// Whether the agent and model picked take images, as the agent says.
    pub(crate) fn accepts_images(&self, cx: &App) -> Accepts {
        let model = self.model_alias(cx);
        let agent = self.model.read(cx).agents().get(&self.agent);
        prompt_images::accepts(agent.as_deref(), Some(&model))
    }

    /// The branch new threads start from, if one is picked or checked out.
    pub fn start_branch(&self) -> Option<&str> {
        let target = self.target.as_ref()?;
        let from = self.from.as_ref().or(target.current.as_ref())?;
        target
            .branches
            .iter()
            .any(|(name, _)| name == from)
            .then_some(from.as_str())
    }

    /// Finds out again, in the background, which agents are installed and
    /// signed in and whether the sandbox can run.
    pub fn check(&mut self, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let settings = model.settings().clone();
        let agents = model.agents().clone();
        let sandbox = model.sandbox().clone();
        self.checked_with = Some((settings.clone(), model.environment()));
        let checked = cx.background_executor().spawn(async move {
            let statuses = agents.statuses(|id| settings.agent(id).config());
            let agents = statuses
                .into_iter()
                .map(|status| {
                    let login = match &status.availability {
                        Availability::Installed { .. } => agents
                            .get(status.id)
                            .map(|agent| agent.login_status(&settings.agent(status.id).config())),
                        Availability::Unavailable { .. } => None,
                    };
                    AgentCheck { status, login }
                })
                .collect();
            Environment {
                agents,
                sandbox: sandbox.status(),
            }
        });
        self._checking = Some(cx.spawn(async move |this, cx| {
            let environment = checked.await;
            this.update(cx, |this, cx| {
                this.environment = Some(environment);
                this._checking = None;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Home: the repositories to pick from, recent ones first, then those
    /// with threads; with their names. A listed repository that was moved
    /// or deleted is shown, last, with why it cannot be picked.
    fn repository_choices(&self, cx: &App) -> Vec<RepositoryChoice> {
        let model = self.model.read(cx);
        let missing = model.missing_repositories();
        let unavailable = |path: &Path| crate::recent::unavailable(path, model, cx);
        let mut choices: Vec<RepositoryChoice> = crate::recent::paths(cx)
            .iter()
            .map(|path| RepositoryChoice {
                path: path.clone(),
                name: folder_name(path),
                unavailable: unavailable(path),
            })
            .collect();
        let listed = model
            .repositories()
            .iter()
            .map(|repository| (&repository.work_dir, repository.name.clone()))
            .chain(
                missing
                    .iter()
                    .map(|repository| (&repository.work_dir, repository.name.clone().into())),
            );
        for (work_dir, name) in listed {
            if !choices.iter().any(|choice| choice.path == *work_dir) {
                choices.push(RepositoryChoice {
                    path: work_dir.clone(),
                    name,
                    unavailable: unavailable(work_dir),
                });
            }
        }
        // Those that can be picked first, each group in its order.
        choices.sort_by_key(|choice| choice.unavailable.is_some());
        choices
    }

    /// Home: starts threads in the repository at `path`, once its branches
    /// are read.
    pub fn choose_repository(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let read = cx.background_executor().spawn({
            let path = path.clone();
            async move { read_target(&path) }
        });
        self._loading = Some(cx.spawn(async move |this, cx| {
            let read = read.await;
            this.update(cx, |this, cx| {
                this._loading = None;
                match read {
                    Ok(target) => this.set_target(target),
                    Err(error) => {
                        this.target = None;
                        this.target_error = Some(
                            format!(
                                "Could not open {}: {error:#}",
                                crate::recent::display(&path)
                            )
                            .into(),
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Home: the work dir of the repository picked, once it is read.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn picked_repository(&self) -> Option<&std::path::Path> {
        self.target.as_ref().map(|target| target.work_dir.as_path())
    }

    /// Home: reads the picked repository again, as its branches may have
    /// moved since; picks the first repository if none is picked yet.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.mode != ComposerMode::Home {
            return;
        }
        let path = match &self.target {
            Some(target) => Some(target.work_dir.clone()),
            None if self.target_error.is_none() => self
                .repository_choices(cx)
                .into_iter()
                .find(|choice| choice.unavailable.is_none())
                .map(|choice| choice.path),
            None => None,
        };
        if let Some(path) = path {
            self.choose_repository(path, cx);
        }
    }

    fn settings(&self, cx: &App) -> Arc<Settings> {
        self.model.read(cx).settings().clone()
    }

    /// The model alias asked for: the one picked, else the one Settings
    /// name, else the agent's default.
    pub(super) fn model_alias(&self, cx: &App) -> String {
        self.model_alias.clone().unwrap_or_else(|| {
            self.settings(cx)
                .agent(&self.agent)
                .model
                .unwrap_or_else(|| {
                    self.model
                        .read(cx)
                        .agents()
                        .get(&self.agent)
                        .map(|agent| agent.default_model().to_owned())
                        .unwrap_or_default()
                })
        })
    }

    /// The network profile of new threads, as set in Settings.
    pub(crate) fn network(&self, cx: &App) -> NetworkProfile {
        self.settings(cx).sandbox.default_network
    }

    /// The secrets Settings give new threads of this repository by default.
    pub(crate) fn secrets(&self, cx: &App) -> Vec<String> {
        let Some(target) = &self.target else {
            return Vec::new();
        };
        self.settings(cx)
            .secrets_for(&target.common_dir)
            .into_iter()
            .filter(|entry| entry.default)
            .map(|entry| entry.name.clone())
            .collect()
    }

    /// The branch and tip a new thread of `mode` starts from: the branch
    /// picked, or for a discussion always the one checked out, whose
    /// working tree it reads.
    fn start(&self, mode: Mode) -> Option<(String, Oid)> {
        let target = self.target.as_ref()?;
        let from = match mode {
            Mode::Implement => self.start_branch()?,
            Mode::Discuss => target.current.as_deref()?,
        };
        target
            .branches
            .iter()
            .find(|(name, _)| name == from)
            .cloned()
    }

    fn agent_check(&self) -> Option<&AgentCheck> {
        self.environment
            .as_ref()?
            .agents
            .iter()
            .find(|check| check.status.id == self.agent)
    }

    /// What keeps Send disabled, and what the user should know; in the
    /// order to show them.
    fn problems(&self, cx: &App) -> Vec<Problem> {
        let mode = self.thread_mode(cx);
        let discuss = mode == Mode::Discuss;
        let mut problems = Vec::new();
        let mut add = |severity, text: Sentence, settings| {
            problems.push(Problem {
                text: text.text.into(),
                mono: text.mono,
                severity,
                settings,
            })
        };
        match (&self.target, &self.target_error) {
            (_, Some(error)) => add(Severity::Blocking, error.to_string().into(), false),
            (None, None) if self._loading.is_some() => add(
                Severity::Hint,
                "Reading the repository…".to_owned().into(),
                false,
            ),
            (None, None) => add(
                Severity::Blocking,
                "Pick a repository to start the thread in."
                    .to_owned()
                    .into(),
                false,
            ),
            (Some(target), None) if target.bare => add(
                Severity::Blocking,
                if discuss {
                    "A bare repository has no working tree to discuss."
                } else {
                    "A bare repository has no working tree to start from."
                }
                .to_owned()
                .into(),
                false,
            ),
            (Some(target), None) if target.branches.is_empty() => add(
                Severity::Blocking,
                "The repository has no branch to start from yet: make a first commit."
                    .to_owned()
                    .into(),
                false,
            ),
            (Some(_), None) if self.start(mode).is_none() => add(
                Severity::Blocking,
                if discuss {
                    "HEAD is detached: check out a branch to discuss it."
                } else {
                    "HEAD is detached: pick a branch to start from."
                }
                .to_owned()
                .into(),
                false,
            ),
            (Some(target), None) => {
                // The thread's worktree is made from the branch's last
                // commit; what the user has not committed stays behind. A
                // discussion reads the working tree as it is.
                if let (Some(count), Some(branch)) = (target.uncommitted, self.start_branch())
                    && count > 0
                    && !discuss
                {
                    add(Severity::Warning, uncommitted_warning(branch), false);
                }
            }
        }
        match &self.environment {
            None => add(
                Severity::Hint,
                "Checking the agent and the sandbox…".to_owned().into(),
                false,
            ),
            Some(environment) => {
                if let gitez_sandbox::Status::Missing { fix } = &environment.sandbox {
                    add(
                        Severity::Blocking,
                        format!("No sandbox to run the agent in. {fix}").into(),
                        true,
                    );
                }
                let name = self.agent_name(cx);
                match self.agent_check() {
                    None => add(
                        Severity::Blocking,
                        format!("GitEZ does not know the agent {name}.").into(),
                        true,
                    ),
                    Some(AgentCheck {
                        status:
                            AgentStatus {
                                availability: Availability::Unavailable { reason },
                                ..
                            },
                        ..
                    }) => add(
                        Severity::Blocking,
                        format!("{}. Set its program in Settings.", capitalise(reason)).into(),
                        true,
                    ),
                    Some(AgentCheck {
                        login: Some(LoginStatus::SignedOut),
                        ..
                    }) => add(
                        Severity::Warning,
                        Sentence::default()
                            .plain(&format!("{name} is not signed in: run "))
                            .mono("claude auth login", None)
                            .plain(" in a terminal, or give the thread an ")
                            .mono("ANTHROPIC_API_KEY", None)
                            .plain(" secret."),
                        false,
                    ),
                    Some(_) => {}
                }
            }
        }
        let images = self.images.read(cx);
        if !images.is_empty()
            && let Err(why) = self.accepts_images(cx)
        {
            add(
                Severity::Blocking,
                format!("{why} Remove the images, or pick another model.").into(),
                false,
            );
        }
        if images.preparing() {
            add(
                Severity::Hint,
                "Preparing the images…".to_owned().into(),
                false,
            );
        }
        if self.prompt.read(cx).value().trim().is_empty() && images.is_empty() {
            add(
                Severity::Hint,
                "Write what the agent should do, then send it."
                    .to_owned()
                    .into(),
                false,
            );
        }
        problems
    }

    fn can_send(&self, cx: &App) -> bool {
        self.problems(cx)
            .iter()
            .all(|problem| problem.severity == Severity::Warning)
    }

    fn agent_name(&self, cx: &App) -> String {
        self.model
            .read(cx)
            .agents()
            .get(&self.agent)
            .map_or_else(|| self.agent.clone(), |agent| agent.name().to_owned())
    }

    /// Emits the thread to start, and empties the prompt.
    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_send(cx) {
            return;
        }
        let mode = self.thread_mode(cx);
        let (Some(target), Some((branch, tip))) = (self.target.clone(), self.start(mode)) else {
            return;
        };
        let prompt = self.prompt.read(cx).value().trim().to_owned();
        let model = self.model_alias(cx);
        let images = self.images.update(cx, |images, cx| images.take(cx));
        let request = StartRequest {
            work_dir: target.work_dir,
            common_dir: target.common_dir,
            repository_name: target.name.to_string(),
            prompt,
            agent: self.agent.clone(),
            model: (!model.is_empty()).then_some(model),
            mode,
            start: Start {
                branch,
                commit: tip.to_string(),
            },
            // A discussion implements nothing.
            origin: self
                .origin
                .take()
                .map(|(origin, _)| origin.id)
                .filter(|_| mode == Mode::Implement),
            network: self.network(cx),
            secrets: self.secrets(cx),
            images,
        };
        self.prompt
            .update(cx, |prompt, cx| prompt.set_value("", window, cx));
        cx.emit(ComposerEvent::Submit(Box::new(request)));
        cx.notify();
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ComposerEvent::Cancel);
    }

    /// "agent/<slug>" the prompt would get, skipping branches that exist.
    fn branch_preview(&self, cx: &App) -> Option<String> {
        let prompt = self.prompt.read(cx).value();
        let images = self.images.read(cx).len();
        if prompt.trim().is_empty() && images == 0 {
            return None;
        }
        let branches = self
            .target
            .as_ref()
            .map_or(&[][..], |target| &target.branches[..]);
        Some(preview_branch(&prompt, images, branches))
    }
}

/// The branch a thread for `prompt` and `images` images gets:
/// `agent/<slug>`, with `-2`, `-3`, … when such a branch exists, as the
/// thread store picks it.
pub fn preview_branch(prompt: &str, images: usize, branches: &[(String, Oid)]) -> String {
    let base = slug(&title_from_request(prompt, images));
    let taken = |name: &str| branches.iter().any(|(branch, _)| branch == name);
    let mut attempt = 1;
    loop {
        let candidate = match attempt {
            1 => format!("agent/{base}"),
            n => format!("agent/{base}-{n}"),
        };
        if !taken(&candidate) || attempt >= 100 {
            return candidate;
        }
        attempt += 1;
    }
}

/// The repository at `path`, its branches and the branch checked out.
fn read_target(path: &Path) -> anyhow::Result<Target> {
    let repository = gitez_git::Repository::open(path)?;
    let refs = repository.refs()?;
    let current = match &refs.head {
        Some(gitez_git::Head::Branch { name, .. }) => Some(name.clone()),
        _ => None,
    };
    // The user's own working tree (never a thread's): what a thread would
    // not start with.
    let uncommitted = repository
        .work_dir()
        .and_then(|_| repository.status().ok())
        .map(|status| status.files.len());
    Ok(Target {
        work_dir: repository
            .work_dir()
            .map_or_else(|| path.to_owned(), Path::to_owned),
        common_dir: repository.common_dir(),
        name: repository.name().into(),
        bare: repository.work_dir().is_none(),
        uncommitted,
        branches: refs
            .branches
            .into_iter()
            .map(|branch| (branch.name, branch.target))
            .collect(),
        current,
    })
}

fn folder_name(path: &Path) -> SharedString {
    path.file_name()
        .map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
        .into()
}

/// `text` with its first letter in capitals.
fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// "Signed in · uses Claude Code’s own login", or why it cannot run.
fn agent_subtitle(check: &AgentCheck) -> String {
    let name = check.status.name;
    match (&check.status.availability, &check.login) {
        (Availability::Unavailable { reason }, _) => capitalise(reason),
        (_, Some(LoginStatus::SignedIn)) => format!("Signed in · uses {name}’s own login"),
        (_, Some(LoginStatus::SignedOut)) => "Not signed in · run claude auth login".to_owned(),
        _ => format!("Uses {name}’s own login"),
    }
}

/// What the composer says when the main worktree has uncommitted changes;
/// the branch is shown as the "Ask an agent to work from …" row shows it.
pub(crate) fn uncommitted_warning(branch: &str) -> Sentence {
    Sentence::default()
        .plain("Starts from the last commit on ")
        .mono(branch, Some(theme::branch_color(branch)))
        .plain("; your uncommitted changes are not included")
}

/// A picker: a small button with a dropdown caret.
fn picker(id: &'static str) -> Button {
    Button::new(id)
        .debug_selector(move || id.into())
        .compact()
        .h(px(28.))
        .px(px(8.))
        .text_size(px(12.))
        .dropdown_caret(true)
}

/// A muted label inside a picker, such as "Model".
fn picker_label(text: &'static str) -> Div {
    div().text_color(rgb(color::TEXT_MUTED)).child(text)
}

/// A menu entry with a title and a muted line under it.
fn two_lines(title: impl Into<SharedString>, subtitle: impl Into<SharedString>) -> Div {
    div()
        .v_flex()
        .gap(px(2.))
        .py(px(2.))
        .child(div().text_size(px(12.5)).child(title.into()))
        .child(
            div()
                .text_size(px(11.5))
                .text_color(rgb(color::TEXT_MUTED))
                .child(subtitle.into()),
        )
}

/// A note in a menu, muted and wrapped.
fn menu_note(text: &'static str) -> PopupMenuItem {
    PopupMenuItem::element(move |_, _| {
        div()
            .max_w(px(290.))
            .text_size(px(11.5))
            .line_height(relative(1.5))
            .text_color(rgb(color::TEXT_MUTED))
            .child(text)
    })
    .disabled(true)
}

/// "Manage …" at the end of a menu: opens Settings.
fn manage(label: &'static str) -> PopupMenuItem {
    PopupMenuItem::new(label).on_click(|_, window, cx| {
        window.dispatch_action(OpenSettings.boxed_clone(), cx);
    })
}

impl Composer {
    fn render_agent_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let checks: Vec<AgentCheck> = self
            .environment
            .as_ref()
            .map(|environment| environment.agents.clone())
            .unwrap_or_default();
        let chosen = self.agent.clone();
        let view = cx.entity().downgrade();
        picker("composer-agent")
            .child(
                div()
                    .h_flex()
                    .gap(px(6.))
                    .child(agent_mark(&self.agent).size(px(16.)).rounded(px(4.)))
                    .child(self.agent_name(cx)),
            )
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu.min_w(px(300.)).check_side(Side::Right).label("Agent");
                for check in &checks {
                    let id = check.status.id;
                    let available =
                        matches!(check.status.availability, Availability::Installed { .. });
                    let (name, subtitle) = (check.status.name, agent_subtitle(check));
                    let view = view.clone();
                    menu = menu.item(
                        PopupMenuItem::element(move |_, _| {
                            div()
                                .h_flex()
                                .gap(px(10.))
                                .child(agent_mark(id))
                                .child(two_lines(name, subtitle.clone()))
                        })
                        .checked(id == chosen)
                        .disabled(!available)
                        .on_click(move |_, _, cx| {
                            view.update(cx, |this, cx| {
                                this.agent = id.to_owned();
                                this.model_alias = None;
                                cx.notify();
                            })
                            .ok();
                        }),
                    );
                }
                menu.item(
                    PopupMenuItem::element(|_, _| {
                        div()
                            .h_flex()
                            .gap(px(10.))
                            .child(
                                div()
                                    .flex_none()
                                    .size(px(20.))
                                    .rounded(px(5.))
                                    .border_1()
                                    .border_dashed()
                                    .border_color(rgb(color::TAG_LABEL_BORDER)),
                            )
                            .child(two_lines(
                                "More agents later",
                                "Other coding agents will be listed here",
                            ))
                    })
                    .disabled(true),
                )
                .separator()
                .item(manage("Manage agents…"))
            })
    }

    fn render_model_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let models: Vec<(&'static str, &'static str)> = self
            .model
            .read(cx)
            .agents()
            .get(&self.agent)
            .map(|agent| agent.models().to_vec())
            .unwrap_or_default();
        let chosen = self.model_alias(cx);
        let label = models
            .iter()
            .find(|(alias, _)| *alias == chosen)
            .map_or_else(|| chosen.clone(), |(_, label)| (*label).to_owned());
        let view = cx.entity().downgrade();
        picker("composer-model")
            .child(
                div()
                    .h_flex()
                    .gap(px(6.))
                    .child(picker_label("Model"))
                    .child(label),
            )
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu.min_w(px(240.)).check_side(Side::Right).label("Model");
                for &(alias, label) in &models {
                    let view = view.clone();
                    menu = menu.item(PopupMenuItem::new(label).checked(alias == chosen).on_click(
                        move |_, _, cx| {
                            view.update(cx, |this, cx| {
                                this.model_alias = Some(alias.to_owned());
                                cx.notify();
                            })
                            .ok();
                        },
                    ));
                }
                menu
            })
    }

    fn render_from_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let branches = self
            .target
            .as_ref()
            .map(|target| target.branches.clone())
            .unwrap_or_default();
        let current = self
            .target
            .as_ref()
            .and_then(|target| target.current.clone());
        let chosen = self.start(Mode::Implement);
        let view = cx.entity().downgrade();
        let short = |tip: &Oid| tip.to_string().chars().take(7).collect::<String>();
        let value = match &chosen {
            Some((name, tip)) => div()
                .h_flex()
                .gap(px(6.))
                .child(branch_chip(name, true).max_w(px(180.)))
                .child(
                    div()
                        .font_family(mono.clone())
                        .text_size(px(11.))
                        .text_color(rgb(color::TEXT_MUTED))
                        .child(short(tip)),
                ),
            None => div()
                .text_color(rgb(color::TEXT_MUTED))
                .child("Pick a branch"),
        };
        picker("composer-from")
            .child(
                div()
                    .h_flex()
                    .gap(px(6.))
                    .child(picker_label("From"))
                    .child(value),
            )
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu
                    .min_w(px(330.))
                    .max_h(px(360.))
                    .scrollable(true)
                    .check_side(Side::Right)
                    .label("Start from");
                for (name, tip) in &branches {
                    let view = view.clone();
                    let picked = chosen.as_ref().is_some_and(|(chosen, _)| chosen == name);
                    let (name, sha, mono) = (name.clone(), short(tip), mono.clone());
                    let is_current = current.as_deref() == Some(name.as_str());
                    let pick = name.clone();
                    menu = menu.item(
                        PopupMenuItem::element(move |_, _| {
                            div()
                                .h_flex()
                                .gap(px(8.))
                                .child(branch_chip(&name, true).max_w(px(220.)))
                                .child(
                                    div()
                                        .font_family(mono.clone())
                                        .text_size(px(11.))
                                        .text_color(rgb(color::TEXT_MUTED))
                                        .child(sha.clone()),
                                )
                                .when(is_current, |this| {
                                    this.child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(rgb(color::TEXT_MUTED))
                                            .child("checked out"),
                                    )
                                })
                        })
                        .checked(picked)
                        .on_click(move |_, _, cx| {
                            view.update(cx, |this, cx| {
                                this.from = Some(pick.clone());
                                cx.notify();
                            })
                            .ok();
                        }),
                    );
                }
                menu
            })
    }

    /// Home: the repository threads start in.
    fn render_repository_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let choices = self.repository_choices(cx);
        let chosen = self.target.as_ref().map(|target| target.work_dir.clone());
        let label: SharedString = match &self.target {
            Some(target) => target.name.clone(),
            None if self._loading.is_some() => "Reading…".into(),
            None => "Pick a repository".into(),
        };
        let mono = cx.theme().mono_font_family.clone();
        let view = cx.entity().downgrade();
        picker("composer-repository")
            .child(
                div()
                    .h_flex()
                    .gap(px(6.))
                    .map(|this| match &self.target {
                        Some(target) => this.child({
                            let label = label.clone();
                            labels::repository_icon(
                                &target.work_dir,
                                px(16.),
                                move || initial_square(&label, px(16.)),
                                cx,
                            )
                        }),
                        None => this.child(
                            Icon::new(IconName::Folder)
                                .size(px(13.))
                                .text_color(rgb(color::TEXT_MUTED)),
                        ),
                    })
                    .child(label),
            )
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu
                    .min_w(px(300.))
                    .max_w(px(520.))
                    .max_h(px(360.))
                    .scrollable(true)
                    .check_side(Side::Right)
                    .label("Repository");
                if choices.is_empty() {
                    menu = menu.item(menu_note("No repositories yet. Open one first."));
                }
                for choice in &choices {
                    let view = view.clone();
                    let RepositoryChoice {
                        path,
                        name,
                        unavailable,
                    } = choice.clone();
                    let mono = mono.clone();
                    let shown = crate::recent::display(&path);
                    let picked = chosen.as_ref() == Some(&path);
                    let disabled = unavailable.is_some();
                    let work_dir = path.clone();
                    menu = menu.item(
                        PopupMenuItem::element(move |_, cx| {
                            div()
                                .h_flex()
                                // Long paths end in an ellipsis inside the
                                // menu's width.
                                .max_w(px(460.))
                                .overflow_hidden()
                                .gap(px(10.))
                                .when(unavailable.is_some(), |this| this.opacity(0.55))
                                .child({
                                    let name = name.clone();
                                    labels::repository_icon(
                                        &work_dir,
                                        px(20.),
                                        move || initial_square(&name, px(20.)),
                                        cx,
                                    )
                                })
                                .child(
                                    div()
                                        .v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .gap(px(2.))
                                        .child(
                                            div()
                                                .h_flex()
                                                .gap(px(6.))
                                                .text_size(px(12.5))
                                                .child(name.clone())
                                                .when_some(unavailable.clone(), |this, why| {
                                                    this.child(
                                                        div()
                                                            .text_size(px(11.))
                                                            .text_color(rgb(color::TEXT_MUTED))
                                                            .child(why),
                                                    )
                                                }),
                                        )
                                        .child(
                                            div()
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .text_ellipsis()
                                                .font_family(mono.clone())
                                                .text_size(px(11.))
                                                .text_color(rgb(color::TEXT_MUTED))
                                                .child(shown.clone()),
                                        ),
                                )
                        })
                        .checked(picked)
                        .disabled(disabled)
                        .on_click(move |_, _, cx| {
                            if disabled {
                                return;
                            }
                            view.update(cx, |this, cx| this.choose_repository(path.clone(), cx))
                                .ok();
                        }),
                    );
                }
                menu
            })
    }

    /// The line over Send: the branch the thread gets, and whether it
    /// starts now.
    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let model = self.model.read(cx);
        let running = model.running();
        let limit = model.settings().threads.max_running as usize;
        let when = if running < limit {
            format!("starts now, {running} of {limit} agents running")
        } else {
            "waits for a free slot".to_owned()
        };
        let home = self.mode == ComposerMode::Home;
        let line = div()
            .h_flex()
            .flex_1()
            .min_w_0()
            .gap(px(4.))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(11.5))
            .text_color(rgb(color::TEXT_MUTED));
        if self.thread_mode(cx) == Mode::Discuss {
            return line.child(if home {
                "Reads the repository’s working tree and changes nothing".to_owned()
            } else {
                format!("Reads your working tree, changes nothing · {when}")
            });
        }
        match self.branch_preview(cx) {
            Some(branch) => line
                .child(if home {
                    "Starts in the background on"
                } else {
                    "New worktree on"
                })
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .font_family(mono)
                        .text_color(rgb(theme::branch_color(&branch)))
                        .child(branch),
                )
                .child(div().flex_none().child(format!("· {when}"))),
            None => line.child(if home {
                "Starts in the background on its own branch and worktree".to_owned()
            } else {
                format!("New worktree on its own branch · {when}")
            }),
        }
    }

    /// Discuss | Implement: what the thread is for. Two joined buttons
    /// the height of the pickers, the mode chosen marked as on.
    fn render_mode_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let chosen = self.thread_mode(cx);
        let segment = |id: &'static str, mode: Mode, tooltip: &'static str| {
            let on = mode == chosen;
            Button::new(id)
                .debug_selector(move || id.into())
                .compact()
                .h(px(28.))
                .px(px(10.))
                .text_size(px(12.))
                .label(mode.label())
                .tooltip(tooltip)
                .selected(on)
                .when(on, |this| {
                    this.bg(rgb(color::SELECTED))
                        .border_color(rgba(color::PICKER_ON_BORDER))
                        .text_color(rgb(color::ACCENT))
                })
                // The group draws the edge between the segments as Discuss's
                // right one, in Discuss's colours. With Implement chosen the
                // edge is Implement's left one instead, so the chosen
                // segment's outline is whole and the group keeps its width.
                .when(chosen == Mode::Implement, |this| match mode {
                    Mode::Discuss => this.border_r_0(),
                    Mode::Implement => this.border_l_1(),
                })
        };
        ButtonGroup::new("composer-mode")
            .child(segment(
                "composer-mode-discuss",
                Mode::Discuss,
                "The agent reads your working tree and answers. It changes nothing.",
            ))
            .child(segment(
                "composer-mode-implement",
                Mode::Implement,
                "The agent works on its own branch and worktree. Its commits become a local pull \
                 request.",
            ))
            .on_click(cx.listener(|this, clicked: &Vec<usize>, _, cx| {
                let mode = if clicked.contains(&1) {
                    Mode::Implement
                } else {
                    Mode::Discuss
                };
                this.set_thread_mode(mode, cx)
            }))
    }

    /// "From the discussion <title>", with a button to let go of it.
    fn render_origin(&self, cx: &mut Context<Self>) -> Option<Div> {
        let (_, title) = self.origin.as_ref()?;
        Some(
            div()
                .debug_selector(|| "composer-origin".into())
                .h_flex()
                .gap(px(6.))
                .min_w_0()
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_MUTED))
                .child(Icon::new(IconName::MessageSquare).size(px(12.)))
                .child(div().flex_none().child("Brief from the discussion"))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(rgb(color::TEXT_SECONDARY))
                        .child(title.clone()),
                )
                .child(
                    Button::new("composer-origin-remove")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip("Start the thread without a link to the discussion")
                        .on_click(cx.listener(|this, _, _, cx| this.set_origin(None, cx))),
                ),
        )
    }

    fn render_problems(&self, cx: &mut Context<Self>) -> Option<Div> {
        let problems: Vec<Problem> = self
            .problems(cx)
            .into_iter()
            // An empty prompt is plain to see: it says so only alone.
            .collect();
        let shown: Vec<&Problem> = problems
            .iter()
            .filter(|problem| problem.severity != Severity::Hint)
            .collect();
        if shown.is_empty() {
            return None;
        }
        let settings = shown.iter().any(|problem| problem.settings);
        let mono = cx.theme().mono_font_family.clone();
        Some(
            div()
                .h_flex()
                .items_start()
                .gap(px(10.))
                .child(div().v_flex().flex_1().min_w_0().gap(px(4.)).children(
                    shown.into_iter().map(|problem| {
                        let (icon, text) = match problem.severity {
                            Severity::Blocking => {
                                (IconName::CircleAlert, color::THREAD_FAILED_TEXT)
                            }
                            _ => (IconName::TriangleAlert, color::WARNING_TEXT),
                        };
                        div()
                            .h_flex()
                            .items_start()
                            .gap(px(8.))
                            .text_size(px(12.))
                            .line_height(relative(1.5))
                            .text_color(rgb(text))
                            .child(Icon::new(icon).size(px(13.)).mt(px(2.)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(sentence_text(problem, &mono)),
                            )
                    }),
                ))
                .when(settings, |this| {
                    this.child(
                        Button::new("composer-open-settings")
                            .compact()
                            .h(px(26.))
                            .label("Open Settings")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(OpenSettings.boxed_clone(), cx)
                            }),
                    )
                }),
        )
    }
}

/// A problem's text with its mono parts set in `mono` (and a branch's
/// colour); it wraps like plain text.
fn sentence_text(problem: &Problem, mono: &SharedString) -> StyledText {
    let highlights = problem.mono.iter().map(|(range, color)| {
        (
            range.clone(),
            HighlightStyle {
                color: color.map(|color| rgb(color).into()),
                ..HighlightStyle::default()
            },
        )
    });
    let fonts = problem
        .mono
        .iter()
        .map(|(range, _)| (range.clone(), mono.clone()));
    StyledText::new(problem.text.clone())
        .with_highlights(highlights)
        .with_font_family_overrides(fonts)
}

/// The paste handler of a prompt with `images`: an image (or a copied image
/// file) is attached instead of inserted; text pastes as before.
pub(crate) fn paste_into(
    images: &Entity<PromptImages>,
    accepts: Accepts,
) -> impl Fn(&ClipboardItem, &mut Window, &mut App) -> bool + 'static {
    let images = images.downgrade();
    move |item, window, cx| {
        images
            .update(cx, |images, cx| images.paste(item, &accepts, window, cx))
            .unwrap_or(false)
    }
}

/// A rounded square with the first letter of `name`, for a repository.
fn initial_square(name: &str, size: Pixels) -> Div {
    let letter: String = name
        .chars()
        .find(|char| char.is_alphanumeric())
        .map(|char| char.to_uppercase().collect())
        .unwrap_or_default();
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(size)
        .rounded(size * 0.25)
        .bg(rgb(color::ACCENT))
        .text_color(rgb(color::ON_ACCENT))
        .text_size(size * 0.55)
        .font_weight(FontWeight::BOLD)
        .child(letter)
}

impl Focusable for Composer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Composer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let home = self.mode == ComposerMode::Home;
        let can_send = self.can_send(cx);
        let mode = self.thread_mode(cx);
        let discuss = mode == Mode::Discuss;
        let header = (!home).then(|| {
            div()
                .h_flex()
                .gap(px(8.))
                .h(px(22.))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(px(20.))
                        .rounded(px(5.))
                        .bg(rgb(color::AGENT_MARK_BACKGROUND))
                        .child(
                            Icon::new(IconName::Terminal)
                                .size(px(12.))
                                .text_color(rgb(color::ACCENT)),
                        ),
                )
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Ask an agent"),
                )
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(12.))
                        .text_color(rgb(color::TEXT_MUTED))
                        .child(if discuss {
                            "It reads your working tree and answers; it changes nothing"
                        } else {
                            "It works in the background on its own branch and worktree"
                        }),
                )
                .child(
                    div()
                        .h_flex()
                        .ml_auto()
                        .flex_none()
                        .gap(px(6.))
                        .text_size(px(11.5))
                        .text_color(rgb(color::TEXT_MUTED))
                        .child(labels::key_cap("esc", cx))
                        .child("close"),
                )
        });
        let pickers = div()
            .h_flex()
            .flex_wrap()
            .gap(px(6.))
            .child(self.render_mode_toggle(cx))
            .when(home, |this| this.child(self.render_repository_picker(cx)))
            .child(self.render_agent_picker(cx))
            .child(self.render_model_picker(cx))
            // A discussion reads what is checked out.
            .when(!discuss, |this| this.child(self.render_from_picker(cx)));
        let footer = div()
            .h_flex()
            .gap(px(10.))
            .pt(px(if home { 10. } else { 8. }))
            .border_t_1()
            .border_color(rgb(color::BORDER))
            .child(self.render_status(cx))
            .child(
                Button::new("composer-send")
                    .debug_selector(|| "composer-send".into())
                    .primary()
                    .h(px(30.))
                    .label(mode.label())
                    .child(
                        div()
                            .h_flex()
                            .gap(px(2.))
                            .child(on_accent(labels::key_cap(labels::SECONDARY_KEY, cx)))
                            .child(on_accent(labels::key_cap("↵", cx))),
                    )
                    .disabled(!can_send)
                    .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
            );
        let accepts = self.accepts_images(cx);
        let dropped = self.images.downgrade();
        let accepts_drop = accepts.clone();
        let card = div()
            .debug_selector(|| "composer".into())
            // Image files dropped on the card are attached too.
            .on_drop(move |paths: &ExternalPaths, window, cx| {
                dropped
                    .update(cx, |images, cx| {
                        images.drop_paths(paths.paths(), &accepts_drop, window, cx)
                    })
                    .ok();
            })
            .v_flex()
            .gap(px(if home { 10. } else { 8. }))
            .px(px(14.))
            .pt(px(if home { 14. } else { 12. }))
            .pb(px(12.))
            .rounded(px(8.))
            .border_1()
            .border_color(theme::with_alpha(color::ACCENT, 0.55))
            .bg(rgb(color::PANEL))
            .shadow(vec![
                BoxShadow {
                    color: theme::with_alpha(color::ACCENT, 0.08).into(),
                    offset: point(px(0.), px(0.)),
                    blur_radius: px(0.),
                    spread_radius: px(3.),
                    inset: false,
                },
                BoxShadow {
                    color: hsla(0., 0., 0., 0.5),
                    offset: point(px(0.), px(12.)),
                    blur_radius: px(32.),
                    spread_radius: px(0.),
                    inset: false,
                },
            ])
            .children(header)
            .children(self.render_origin(cx))
            .child(
                div().text_size(px(if home { 14. } else { 13. })).child(
                    Textarea::new(&self.prompt)
                        .appearance(false)
                        .on_paste(paste_into(&self.images, accepts.clone())),
                ),
            )
            .children(self.images.update(cx, |images, cx| images.render(cx)))
            .child(pickers)
            .child(footer)
            .children(self.render_problems(cx));
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .v_flex()
            .w_full()
            .when(home, |this| {
                this.gap(px(12.)).child(
                    div()
                        .text_size(px(20.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Ask an agent"),
                )
            })
            .child(card)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use gitez_git::Oid;
    use gitez_sandbox::NetworkProfile;
    use gitez_settings::{AgentEntry, SecretEntry, SecretScope, Settings};
    use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext, px, size};

    use gitez_agent_thread::Mode;

    use super::{Composer, ComposerEvent, ComposerMode, Handoff, preview_branch};
    use crate::agent_threads::{AgentThreadKey, Options, StartRequest, init_with};

    fn oid(n: u8) -> Oid {
        Oid::from_bytes_or_panic(&[n; 20])
    }

    #[test]
    fn the_branch_preview_skips_branches_that_exist() {
        let branches = vec![
            ("main".to_owned(), oid(1)),
            ("agent/add-a-blame-view".to_owned(), oid(2)),
            ("agent/add-a-blame-view-2".to_owned(), oid(3)),
        ];
        assert_eq!(
            preview_branch("Add a blame view. Reuse the diff list.", 0, &branches),
            "agent/add-a-blame-view-3"
        );
        assert_eq!(
            preview_branch("Fix the typo in README", 0, &branches),
            "agent/fix-the-typo-in-readme"
        );
    }

    /// A repository window's composer with two branches, `feature`
    /// checked out, and settings with three secrets, one given by default.
    fn composer(cx: &mut TestAppContext) -> (Entity<Composer>, VisualTestContext) {
        composer_with(cx, gitez_agent::Registry::builtin())
    }

    /// [`composer`] with the agents of `agents`.
    fn composer_with(
        cx: &mut TestAppContext,
        agents: gitez_agent::Registry,
    ) -> (Entity<Composer>, VisualTestContext) {
        let dir = tempfile::tempdir().expect("temp dir");
        let data_dir = dir.path().join("data");
        let common_dir = Path::new("/repo/.git");
        let settings = Settings {
            secrets: vec![
                SecretEntry {
                    name: "GITHUB_TOKEN".into(),
                    scope: SecretScope::All,
                    default: true,
                },
                SecretEntry {
                    name: "SENTRY_DSN".into(),
                    scope: SecretScope::Repository(common_dir.to_owned()),
                    default: false,
                },
                SecretEntry {
                    name: "OTHER".into(),
                    scope: SecretScope::Repository("/elsewhere/.git".into()),
                    default: true,
                },
            ],
            // Never the real `claude`: tests must not run it, not even to
            // ask whether it is signed in.
            agents: [(
                gitez_agent::ClaudeCode::ID.to_owned(),
                AgentEntry {
                    program: Some("/nonexistent/claude".into()),
                    ..AgentEntry::default()
                },
            )]
            .into(),
            ..Settings::default()
        };
        settings
            .save(&Settings::path(&data_dir))
            .expect("settings saved");
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::init(cx);
            crate::recent::init_in_memory(cx);
            init_with(
                Options {
                    data_dir: Some(data_dir),
                    sandbox: Arc::new(gitez_sandbox::Passthrough),
                    agents,
                    secrets: Arc::new(gitez_secret::Memory::default()),
                    exe: Err("no supervisor in tests".into()),
                    watch: false,
                },
                cx,
            );
        });
        cx.run_until_parked();
        // The settings are read: the temporary folder may go.
        drop(dir);
        let mode = ComposerMode::Repository {
            work_dir: "/repo".into(),
            common_dir: common_dir.to_owned(),
            name: "repo".into(),
            branches: vec![("main".into(), oid(1)), ("feature".into(), oid(2))],
            current: Some("feature".into()),
            bare: false,
        };
        // In gpui-kit's Root, as in the app: notices need it.
        let (window, composer) = cx
            .update(|cx| {
                gpui_kit::open_window(gpui_kit::WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| Composer::new(mode, window, cx))
                })
            })
            .expect("window open");
        let mut cx = VisualTestContext::from_window(window, cx);
        // As opening it does.
        composer.update(&mut cx, |composer, cx| composer.check(cx));
        cx.run_until_parked();
        (composer, cx)
    }

    /// Finding 9: Home's Repository picker shows a repository that was
    /// moved or deleted as unavailable, and never picks it: one with
    /// threads, and a recent one.
    #[gpui_kit::test]
    fn a_moved_repository_cannot_be_picked(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp dir");
        let data_dir = dir.path().join("data");
        let gone = dir.path().join("gone");
        let here = dir.path().join("here");
        let lost = dir.path().join("lost");
        std::fs::create_dir_all(here.join(".git")).expect("a git folder");
        let index = gitez_agent_thread::Index::new(&data_dir);
        for (work_dir, name) in [(&gone, "gone"), (&here, "here")] {
            index
                .add(gitez_agent_thread::IndexedRepository {
                    common_dir: work_dir.join(".git"),
                    work_dir: work_dir.clone(),
                    name: name.into(),
                })
                .expect("listed");
        }
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::init(cx);
            crate::recent::init_in_memory(cx);
            crate::recent::opened(&gone, cx);
            // A recent one without threads, moved since: the same check.
            // Opened last, so first among the recent ones.
            crate::recent::opened(&lost, cx);
            crate::recent::check(cx);
            init_with(
                Options {
                    data_dir: Some(data_dir.clone()),
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
        let window = cx.open_window(size(px(900.), px(600.)), |window, cx| {
            Composer::new(ComposerMode::Home, window, cx)
        });
        let composer = window
            .update(cx, |_, _, cx| cx.entity())
            .expect("window open");
        let cx = VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        composer.read_with(&cx, |composer, cx| {
            let choices = composer.repository_choices(cx);
            let shown: Vec<(&str, Option<&str>)> = choices
                .iter()
                .map(|choice| (choice.name.as_ref(), choice.unavailable.as_deref()))
                .collect();
            // Those that can be picked first; the gone one says why.
            assert_eq!(
                shown,
                [
                    ("here", None),
                    ("lost", Some("Moved or deleted")),
                    ("gone", Some("Moved or deleted"))
                ]
            );
        });
    }

    #[gpui_kit::test]
    fn sending_emits_the_picked_branch_and_the_default_network_and_secrets(
        cx: &mut TestAppContext,
    ) {
        let (composer, mut cx) = composer(cx);
        let sent: Arc<std::sync::Mutex<Vec<StartRequest>>> = Arc::default();
        cx.update(|_, cx| {
            let sent = sent.clone();
            cx.subscribe(&composer, move |_, event: &ComposerEvent, _| {
                if let ComposerEvent::Submit(request) = event {
                    sent.lock().expect("lock").push((**request).clone());
                }
            })
            .detach();
        });
        // The secrets given by default, of this repository only.
        composer.read_with(&cx, |composer, cx| {
            assert_eq!(composer.secrets(cx), ["GITHUB_TOKEN"]);
            assert_eq!(composer.start_branch(), Some("feature"));
        });
        composer.update_in(&mut cx, |composer, window, cx| {
            composer.from = Some("main".into());
            composer.model_alias = Some("haiku".into());
            composer.set_prompt("Fix the typo in README. Keep it short.".into(), window, cx);
        });
        cx.run_until_parked();
        // The agent's program is missing: Send stays disabled, and says why.
        composer.read_with(&cx, |composer, cx| {
            assert!(composer.environment.is_some());
            assert!(!composer.can_send(cx));
            let problems = composer.problems(cx);
            assert!(
                problems
                    .iter()
                    .any(|problem| problem.severity == super::Severity::Blocking
                        && problem.settings
                        && problem.text.contains("/nonexistent/claude")),
                "{problems:?}"
            );
        });
        // As if the agent were installed and signed in, send as `⌘↵` does.
        composer.update_in(&mut cx, |composer, window, cx| {
            composer.environment = Some(super::Environment {
                agents: vec![super::AgentCheck {
                    status: gitez_agent::AgentStatus {
                        id: gitez_agent::ClaudeCode::ID,
                        name: "Claude Code",
                        availability: gitez_agent::Availability::Installed {
                            program: "/bin/true".into(),
                        },
                    },
                    login: Some(gitez_agent::LoginStatus::SignedIn),
                }],
                sandbox: gitez_sandbox::Status::Ready {
                    program: "/bin/true".into(),
                    version: None,
                },
            });
            assert!(composer.can_send(cx));
            composer.send(window, cx);
        });
        let sent = sent.lock().expect("lock");
        let [request] = &sent[..] else {
            panic!("one request, got {}", sent.len());
        };
        assert_eq!(request.start.branch, "main");
        assert_eq!(request.start.commit, oid(1).to_string());
        assert_eq!(request.network, NetworkProfile::default());
        assert_eq!(request.secrets, ["GITHUB_TOKEN"]);
        assert_eq!(request.model.as_deref(), Some("haiku"));
        assert_eq!(request.prompt, "Fix the typo in README. Keep it short.");
        assert_eq!(request.repository_name, "repo");
        // The prompt is emptied for the next thread.
        composer.read_with(&cx, |composer, cx| {
            assert!(composer.prompt.read(cx).value().is_empty());
        });
    }

    /// What `composer` emits from now on.
    fn requests(
        composer: &Entity<Composer>,
        cx: &mut VisualTestContext,
    ) -> Arc<std::sync::Mutex<Vec<StartRequest>>> {
        let sent: Arc<std::sync::Mutex<Vec<StartRequest>>> = Arc::default();
        cx.update(|_, cx| {
            let sent = sent.clone();
            cx.subscribe(composer, move |_, event: &ComposerEvent, _| {
                if let ComposerEvent::Submit(request) = event {
                    sent.lock().expect("lock").push((**request).clone());
                }
            })
            .detach();
        });
        sent
    }

    /// Discuss starts a thread that reads what is checked out: no From
    /// picker, no warning about uncommitted changes, and the branch picked
    /// for Implement is not used. Implement is the mode until one is
    /// picked.
    #[gpui_kit::test]
    fn discuss_reads_what_is_checked_out(cx: &mut TestAppContext) {
        let (composer, mut cx) = composer(cx);
        let sent = requests(&composer, &mut cx);
        composer.update_in(&mut cx, |composer, window, cx| {
            ready(composer, gitez_agent::ClaudeCode::ID);
            composer.from = Some("main".into());
            composer.set_uncommitted(Some(2), cx);
            composer.set_prompt("How does the parser read a rename?".into(), window, cx);
            assert_eq!(composer.thread_mode(cx), Mode::Implement);
            assert!(
                composer
                    .problems(cx)
                    .iter()
                    .any(|problem| problem.severity == super::Severity::Warning)
            );
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("composer-from").is_some());
        // The toggle's first button.
        let discuss = cx
            .debug_bounds("composer-mode-discuss")
            .expect("the mode toggle is shown");
        cx.simulate_click(discuss.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("composer-from").is_none());
        assert!(cx.debug_bounds("composer-model").is_some());
        composer.update_in(&mut cx, |composer, window, cx| {
            assert_eq!(composer.thread_mode(cx), Mode::Discuss);
            assert_eq!(composer.problems(cx), []);
            composer.send(window, cx);
        });
        let sent = sent.lock().expect("lock");
        let [request] = &sent[..] else {
            panic!("one request, got {}", sent.len());
        };
        assert_eq!(request.mode, Mode::Discuss);
        assert_eq!(request.start.branch, "feature");
        assert_eq!(request.start.commit, oid(2).to_string());
        assert_eq!(request.origin, None);
    }

    /// A discussion needs a branch checked out, even when Implement could
    /// start from a picked one.
    #[gpui_kit::test]
    fn discuss_needs_a_branch_checked_out(cx: &mut TestAppContext) {
        let (composer, mut cx) = composer(cx);
        composer.update_in(&mut cx, |composer, window, cx| {
            ready(composer, gitez_agent::ClaudeCode::ID);
            composer.set_mode(
                ComposerMode::Repository {
                    work_dir: "/repo".into(),
                    common_dir: "/repo/.git".into(),
                    name: "repo".into(),
                    branches: vec![("main".into(), oid(1))],
                    current: None,
                    bare: false,
                },
                cx,
            );
            composer.from = Some("main".into());
            composer.set_prompt("Where is blame read?".into(), window, cx);
            assert!(composer.can_send(cx), "{:?}", composer.problems(cx));
            composer.set_thread_mode(Mode::Discuss, cx);
            assert!(!composer.can_send(cx));
            assert!(composer.problems(cx).iter().any(|problem| {
                problem.severity == super::Severity::Blocking
                    && problem.text == "HEAD is detached: check out a branch to discuss it."
            }));
        });
    }

    /// The brief a Discuss thread hands over becomes the prompt of an
    /// Implement thread that names the discussion; without the link, or as
    /// a discussion, the thread started names none.
    #[gpui_kit::test]
    fn a_brief_handed_over_starts_an_implement_thread(cx: &mut TestAppContext) {
        let (composer, mut cx) = composer(cx);
        let sent = requests(&composer, &mut cx);
        let origin = AgentThreadKey {
            common_dir: "/repo/.git".into(),
            id: gitez_agent_thread::AgentThreadId::new("how-should-blame-read-renames")
                .expect("an id"),
        };
        let handoff = Handoff {
            origin: origin.clone(),
            title: "How should blame read renames?".into(),
            brief: "# Follow renames in blame\n\nRead the rename from each commit’s diff.".into(),
            work_dir: Some("/repo".into()),
        };
        composer.update_in(&mut cx, |composer, window, cx| {
            ready(composer, gitez_agent::ClaudeCode::ID);
            composer.set_thread_mode(Mode::Discuss, cx);
            composer.hand_over(&handoff, window, cx);
            assert_eq!(composer.thread_mode(cx), Mode::Implement);
            assert_eq!(composer.origin(), Some(&origin));
            assert_eq!(composer.prompt.read(cx).value(), handoff.brief);
            // The thread is named by the brief's heading.
            assert_eq!(
                composer.branch_preview(cx).as_deref(),
                Some("agent/follow-renames-in-blame")
            );
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("composer-origin").is_some());
        composer.update_in(&mut cx, |composer, window, cx| composer.send(window, cx));
        {
            let sent = sent.lock().expect("lock");
            let [request] = &sent[..] else {
                panic!("one request, got {}", sent.len());
            };
            assert_eq!(request.mode, Mode::Implement);
            assert_eq!(request.origin.as_ref(), Some(&origin.id));
            assert_eq!(request.prompt, handoff.brief);
        }
        // Sent: the next thread is not linked.
        composer.read_with(&cx, |composer, _| assert_eq!(composer.origin(), None));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("composer-origin").is_none());

        // A brief sent as a discussion implements nothing.
        composer.update_in(&mut cx, |composer, window, cx| {
            composer.hand_over(&handoff, window, cx);
            composer.set_thread_mode(Mode::Discuss, cx);
            composer.send(window, cx);
        });
        assert_eq!(sent.lock().expect("lock")[1].origin, None);

        // Another repository's branches: the brief was not written for it.
        composer.update_in(&mut cx, |composer, window, cx| {
            composer.hand_over(&handoff, window, cx);
            composer.set_mode(
                ComposerMode::Repository {
                    work_dir: "/other".into(),
                    common_dir: "/other/.git".into(),
                    name: "other".into(),
                    branches: vec![("main".into(), oid(1))],
                    current: Some("main".into()),
                    bare: false,
                },
                cx,
            );
            assert_eq!(composer.origin(), None);
        });
    }

    /// M4: with uncommitted changes in the main worktree the composer says
    /// a thread does not start with them; Send still works.
    #[gpui_kit::test]
    fn uncommitted_changes_are_said_to_stay_behind(cx: &mut TestAppContext) {
        let (composer, mut cx) = composer(cx);
        let warning = super::uncommitted_warning("feature");
        // The branch is set in the mono font, not quoted with backticks.
        assert_eq!(
            warning.text,
            "Starts from the last commit on feature; your uncommitted changes are not included"
        );
        let start = "Starts from the last commit on ".len();
        assert_eq!(
            warning.mono,
            vec![(
                start..start + "feature".len(),
                Some(crate::theme::branch_color("feature"))
            )]
        );
        let has = |composer: &Composer, cx: &gpui_kit::App| {
            composer.problems(cx).iter().any(|problem| {
                problem.text == warning.text
                    && problem.mono == warning.mono
                    && problem.severity == super::Severity::Warning
            })
        };
        composer.read_with(&cx, |composer, cx| assert!(!has(composer, cx)));
        composer.update(&mut cx, |composer, cx| {
            composer.set_uncommitted(Some(2), cx)
        });
        composer.read_with(&cx, |composer, cx| assert!(has(composer, cx)));
        // Kept when the refs are read again.
        composer.update(&mut cx, |composer, cx| {
            let mode = composer.mode.clone();
            composer.set_mode(mode, cx);
        });
        composer.read_with(&cx, |composer, cx| assert!(has(composer, cx)));
        composer.update(&mut cx, |composer, cx| {
            composer.set_uncommitted(Some(0), cx)
        });
        composer.read_with(&cx, |composer, cx| assert!(!has(composer, cx)));
    }

    /// Minor 16: a bare repository keeps its composer, which says why
    /// nothing can start.
    #[gpui_kit::test]
    fn a_bare_repository_says_why_no_thread_can_start(cx: &mut TestAppContext) {
        let (composer, mut cx) = composer(cx);
        composer.update(&mut cx, |composer, cx| {
            composer.set_mode(
                ComposerMode::Repository {
                    work_dir: "/repo.git".into(),
                    common_dir: "/repo.git".into(),
                    name: "repo".into(),
                    branches: vec![("main".into(), oid(1))],
                    current: Some("main".into()),
                    bare: true,
                },
                cx,
            )
        });
        composer.read_with(&cx, |composer, cx| {
            assert!(composer.problems(cx).iter().any(|problem| {
                problem.severity == super::Severity::Blocking
                    && problem.text.contains("bare repository")
            }));
        });
    }

    /// As if the agent were installed and signed in, and the sandbox ready.
    fn ready(composer: &mut Composer, agent: &'static str) {
        composer.environment = Some(super::Environment {
            agents: vec![super::AgentCheck {
                status: gitez_agent::AgentStatus {
                    id: agent,
                    name: "Agent",
                    availability: gitez_agent::Availability::Installed {
                        program: "/bin/true".into(),
                    },
                },
                login: Some(gitez_agent::LoginStatus::SignedIn),
            }],
            sandbox: gitez_sandbox::Status::Ready {
                program: "/bin/true".into(),
                version: None,
            },
        });
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let pixels = image::RgbImage::from_pixel(width, height, image::Rgb([40, 120, 220]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(pixels)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("encode");
        bytes.into_inner()
    }

    fn clipboard_image(bytes: Vec<u8>) -> gpui_kit::ClipboardItem {
        gpui_kit::ClipboardItem {
            entries: vec![gpui_kit::ClipboardEntry::Image(
                gpui_kit::Image::from_bytes(gpui_kit::ImageFormat::Png, bytes),
            )],
        }
    }

    /// `⌘V` (`ctrl-v` elsewhere) in the composer's prompt.
    fn paste(composer: &Entity<Composer>, cx: &mut VisualTestContext) {
        composer.update_in(cx, |composer, window, cx| composer.focus(window, cx));
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-v"
        } else {
            "ctrl-v"
        });
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn a_pasted_image_is_attached_and_sent_with_the_prompt(cx: &mut TestAppContext) {
        let (composer, mut cx) = composer(cx);
        let sent: Arc<std::sync::Mutex<Vec<StartRequest>>> = Arc::default();
        cx.update(|_, cx| {
            let sent = sent.clone();
            cx.subscribe(&composer, move |_, event: &ComposerEvent, _| {
                if let ComposerEvent::Submit(request) = event {
                    sent.lock().expect("lock").push((**request).clone());
                }
            })
            .detach();
        });
        let bytes = png(40, 30);
        cx.write_to_clipboard(clipboard_image(bytes.clone()));
        paste(&composer, &mut cx);
        composer.read_with(&cx, |composer, cx| {
            let images = composer.images().read(cx);
            assert_eq!(images.len(), 1);
            assert!(!images.preparing());
            // Nothing was typed into the prompt.
            assert!(composer.prompt.read(cx).value().is_empty());
            // An image alone is a prompt: the thread is named after it.
            assert_eq!(
                composer.branch_preview(cx).as_deref(),
                Some("agent/pasted-image")
            );
        });
        assert!(cx.debug_bounds("composer-images").is_some());
        composer.update_in(&mut cx, |composer, window, cx| {
            ready(composer, gitez_agent::ClaudeCode::ID);
            assert!(composer.can_send(cx), "{:?}", composer.problems(cx));
            composer.send(window, cx);
        });
        let sent = sent.lock().expect("lock");
        let [request] = &sent[..] else {
            panic!("one request, got {}", sent.len());
        };
        assert_eq!(request.prompt, "");
        assert_eq!(request.images.len(), 1);
        assert_eq!(request.images[0].bytes, bytes);
        assert_eq!(
            request.images[0].media_type,
            gitez_agent_thread::ImageType::Png
        );
        composer.read_with(&cx, |composer, cx| {
            assert!(composer.images().read(cx).is_empty());
        });
    }

    #[gpui_kit::test]
    fn pasting_text_inserts_it_as_before(cx: &mut TestAppContext) {
        let (composer, mut cx) = composer(cx);
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
            "Fix the typo".to_owned(),
        ));
        paste(&composer, &mut cx);
        composer.read_with(&cx, |composer, cx| {
            assert_eq!(composer.prompt.read(cx).value(), "Fix the typo");
            assert!(composer.images().read(cx).is_empty());
        });
    }

    #[gpui_kit::test]
    fn a_message_takes_at_most_five_images(cx: &mut TestAppContext) {
        let (composer, mut cx) = composer(cx);
        for side in 10..17 {
            cx.write_to_clipboard(clipboard_image(png(side, side)));
            paste(&composer, &mut cx);
        }
        composer.read_with(&cx, |composer, cx| {
            assert_eq!(
                composer.images().read(cx).len(),
                gitez_agent_thread::MAX_IMAGES
            );
        });
    }

    #[gpui_kit::test]
    fn what_is_not_an_image_is_refused_with_a_notice(cx: &mut TestAppContext) {
        let (composer, mut cx) = composer(cx);
        cx.write_to_clipboard(clipboard_image(b"not an image at all".to_vec()));
        paste(&composer, &mut cx);
        composer.read_with(&cx, |composer, cx| {
            assert!(composer.images().read(cx).is_empty());
            assert!(composer.prompt.read(cx).value().is_empty());
        });
    }

    /// An agent whose models read no images.
    struct TextOnly;

    impl gitez_agent::Agent for TextOnly {
        fn id(&self) -> &'static str {
            "text-only"
        }
        fn name(&self) -> &'static str {
            "Text Only"
        }
        fn models(&self) -> &'static [gitez_agent::ModelChoice] {
            &[("text", "Text")]
        }
        fn locate(&self, _: &gitez_agent::AgentConfig) -> gitez_agent::Result<std::path::PathBuf> {
            Ok("/bin/true".into())
        }
        fn command(
            &self,
            _: &gitez_agent::AgentConfig,
            _: &gitez_agent::RunSpec<'_>,
        ) -> gitez_agent::Result<gitez_agent::CommandSpec> {
            Ok(gitez_agent::CommandSpec::default())
        }
        fn needs(&self, _: &gitez_agent::AgentConfig) -> gitez_agent::AgentNeeds {
            gitez_agent::AgentNeeds::default()
        }
        fn parse(&self, _: &str) -> Option<gitez_agent::AgentEvent> {
            None
        }
        fn login_status(&self, _: &gitez_agent::AgentConfig) -> gitez_agent::LoginStatus {
            gitez_agent::LoginStatus::SignedIn
        }
    }

    /// The trait decides: an agent that reads no images gets none, and the
    /// paste says so; Claude Code's do.
    #[gpui_kit::test]
    fn an_agent_that_reads_no_images_gets_none(cx: &mut TestAppContext) {
        let agents = gitez_agent::Registry::with(vec![
            Arc::new(TextOnly),
            Arc::new(gitez_agent::ClaudeCode),
        ]);
        let (composer, mut cx) = composer_with(cx, agents);
        composer.read_with(&cx, |composer, cx| {
            assert_eq!(composer.agent, "text-only");
            assert!(composer.accepts_images(cx).is_err());
        });
        cx.write_to_clipboard(clipboard_image(png(20, 20)));
        paste(&composer, &mut cx);
        composer.read_with(&cx, |composer, cx| {
            assert!(composer.images().read(cx).is_empty());
            assert!(composer.prompt.read(cx).value().is_empty());
        });
        // Claude Code takes them.
        composer.update(&mut cx, |composer, cx| {
            composer.agent = gitez_agent::ClaudeCode::ID.to_owned();
            cx.notify();
        });
        paste(&composer, &mut cx);
        composer.read_with(&cx, |composer, cx| {
            assert_eq!(composer.images().read(cx).len(), 1);
        });
        // Picking the other agent again keeps Send off while it has them.
        composer.update(&mut cx, |composer, cx| {
            composer.agent = "text-only".to_owned();
            ready(composer, "text-only");
            assert!(!composer.can_send(cx));
            assert!(composer.problems(cx).iter().any(|problem| {
                problem.severity == super::Severity::Blocking
                    && problem.text.contains("does not take images")
            }));
        });
    }
}
