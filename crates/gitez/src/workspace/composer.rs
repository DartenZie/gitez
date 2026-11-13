//! The "Ask an agent" composer: a prompt and the pickers that start an
//! agent thread (agent, model, starting branch, sandbox network, secrets),
//! as in the `C-1-Start` board. In a repository window it opens as a card
//! under the "Ask an agent…" row of the history; Home shows it as its
//! launcher (`C-5-Home`), with a Repository picker in front.
//!
//! The composer only gathers a [`StartRequest`] and emits it; whoever shows
//! it starts the thread with [`AgentThreads::start`].

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gitez_agent::{AgentStatus, Availability, LoginStatus};
use gitez_agent_thread::{Start, slug, title_from_prompt};
use gitez_git::Oid;
use gitez_sandbox::NetworkProfile;
use gitez_settings::{SecretScope, Settings};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::agent_thread::{agent_mark, branch_chip, on_accent};
use super::labels;
use crate::agent_threads::{AgentThreads, OpenSettings, StartRequest};
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

/// The "Ask an agent" composer.
pub struct Composer {
    mode: ComposerMode,
    target: Option<Target>,
    /// Home: reading the repository picked.
    _loading: Option<Task<()>>,
    target_error: Option<SharedString>,
    model: Entity<AgentThreads>,
    prompt: Entity<TextareaState>,
    focus_handle: FocusHandle,
    agent: String,
    /// `None`: the agent entry's default.
    model_alias: Option<String>,
    /// `None`: the branch checked out.
    from: Option<String>,
    /// `None`: the default of the settings.
    network: Option<NetworkProfile>,
    /// `None`: the secrets given to new threads by default.
    secrets: Option<BTreeSet<String>>,
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
                "Describe a task or ask a question about this repository"
            }
            ComposerMode::Home => "Describe a task or ask a question about a repository",
        };
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 10)
                .placeholder(placeholder)
        });
        let subscriptions = vec![
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
            focus_handle: cx.focus_handle(),
            agent,
            model_alias: None,
            from: None,
            network: None,
            secrets: None,
            environment: None,
            checked_with: None,
            _checking: None,
            _subscriptions: subscriptions,
        };
        composer.set_mode(mode, cx);
        if composer.target.is_none()
            && let Some(path) = composer
                .repository_choices(cx)
                .first()
                .map(|(path, _)| path.clone())
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
        } = &mode
        {
            let target = Target {
                work_dir: work_dir.clone(),
                common_dir: common_dir.clone(),
                name: name.clone(),
                branches: branches.clone(),
                current: current.clone(),
                bare: false,
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
            self.secrets = None;
        }
        self.from = self
            .from
            .take()
            .filter(|from| target.branches.iter().any(|(name, _)| name == from));
        self.target = Some(target);
        self.target_error = None;
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
    /// with threads; with their names.
    fn repository_choices(&self, cx: &App) -> Vec<(PathBuf, SharedString)> {
        let mut choices: Vec<(PathBuf, SharedString)> = crate::recent::paths(cx)
            .iter()
            .map(|path| (path.clone(), folder_name(path)))
            .collect();
        for repository in self.model.read(cx).repositories() {
            if !choices.iter().any(|(path, _)| *path == repository.work_dir) {
                choices.push((repository.work_dir.clone(), repository.name.clone()));
            }
        }
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

    fn settings(&self, cx: &App) -> Arc<Settings> {
        self.model.read(cx).settings().clone()
    }

    /// The model alias asked for; empty for the agent's default.
    pub(super) fn model_alias(&self, cx: &App) -> String {
        self.model_alias.clone().unwrap_or_else(|| {
            self.settings(cx)
                .agent(&self.agent)
                .model
                .unwrap_or_default()
        })
    }

    pub(crate) fn network(&self, cx: &App) -> NetworkProfile {
        self.network
            .unwrap_or_else(|| self.settings(cx).sandbox.default_network)
    }

    /// The secrets this repository's threads can have: (name, scope label,
    /// given by default).
    fn secret_choices(&self, cx: &App) -> Vec<(String, &'static str, bool)> {
        let Some(target) = &self.target else {
            return Vec::new();
        };
        self.settings(cx)
            .secrets_for(&target.common_dir)
            .into_iter()
            .map(|entry| {
                let scope = match entry.scope {
                    SecretScope::All => "All repositories",
                    SecretScope::Repository(_) => "This repository",
                };
                (entry.name.clone(), scope, entry.default)
            })
            .collect()
    }

    /// The secrets picked, of those that exist.
    pub(crate) fn secrets(&self, cx: &App) -> Vec<String> {
        self.secret_choices(cx)
            .into_iter()
            .filter(|(name, _, default)| match &self.secrets {
                Some(picked) => picked.contains(name),
                None => *default,
            })
            .map(|(name, _, _)| name)
            .collect()
    }

    fn toggle_secret(&mut self, name: &str, on: bool, cx: &mut Context<Self>) {
        let mut picked: BTreeSet<String> = self.secrets(cx).into_iter().collect();
        if on {
            picked.insert(name.to_owned());
        } else {
            picked.remove(name);
        }
        self.secrets = Some(picked);
        cx.notify();
    }

    /// The branch and tip new threads start from.
    fn start(&self) -> Option<(String, Oid)> {
        let from = self.start_branch()?;
        self.target
            .as_ref()?
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
        let mut problems = Vec::new();
        let mut add = |severity, text: String, settings| {
            problems.push(Problem {
                text: text.into(),
                severity,
                settings,
            })
        };
        match (&self.target, &self.target_error) {
            (_, Some(error)) => add(Severity::Blocking, error.to_string(), false),
            (None, None) if self._loading.is_some() => {
                add(Severity::Hint, "Reading the repository…".to_owned(), false)
            }
            (None, None) => add(
                Severity::Blocking,
                "Pick a repository to start the thread in.".to_owned(),
                false,
            ),
            (Some(target), None) if target.bare => add(
                Severity::Blocking,
                "A bare repository has no working tree to start from.".to_owned(),
                false,
            ),
            (Some(target), None) if target.branches.is_empty() => add(
                Severity::Blocking,
                "The repository has no branch to start from yet: make a first commit.".to_owned(),
                false,
            ),
            (Some(_), None) if self.start_branch().is_none() => add(
                Severity::Blocking,
                "HEAD is detached: pick a branch to start from.".to_owned(),
                false,
            ),
            (Some(_), None) => {}
        }
        match &self.environment {
            None => add(
                Severity::Hint,
                "Checking the agent and the sandbox…".to_owned(),
                false,
            ),
            Some(environment) => {
                if let gitez_sandbox::Status::Missing { fix } = &environment.sandbox {
                    add(
                        Severity::Blocking,
                        format!("No sandbox to run the agent in. {fix}"),
                        true,
                    );
                }
                let name = self.agent_name(cx);
                match self.agent_check() {
                    None => add(
                        Severity::Blocking,
                        format!("GitEZ does not know the agent {name}."),
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
                        format!("{}. Set its program in Settings.", capitalise(reason)),
                        true,
                    ),
                    Some(AgentCheck {
                        login: Some(LoginStatus::SignedOut),
                        ..
                    }) => add(
                        Severity::Warning,
                        format!(
                            "{name} is not signed in: run `claude auth login` in a terminal, or \
                             give the thread an ANTHROPIC_API_KEY secret."
                        ),
                        false,
                    ),
                    Some(_) => {}
                }
            }
        }
        if self.prompt.read(cx).value().trim().is_empty() {
            add(
                Severity::Hint,
                "Write what the agent should do, then send it.".to_owned(),
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
        let (Some(target), Some((branch, tip))) = (self.target.clone(), self.start()) else {
            return;
        };
        let prompt = self.prompt.read(cx).value().trim().to_owned();
        let model = self.model_alias(cx);
        let request = StartRequest {
            work_dir: target.work_dir,
            common_dir: target.common_dir,
            repository_name: target.name.to_string(),
            prompt,
            agent: self.agent.clone(),
            model: (!model.is_empty()).then_some(model),
            start: Start {
                branch,
                commit: tip.to_string(),
            },
            network: self.network(cx),
            secrets: self.secrets(cx),
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
        if prompt.trim().is_empty() {
            return None;
        }
        let branches = self
            .target
            .as_ref()
            .map_or(&[][..], |target| &target.branches[..]);
        Some(preview_branch(&prompt, branches))
    }
}

/// The branch a thread for `prompt` gets: `agent/<slug>`, with `-2`, `-3`, …
/// when such a branch exists, as the thread store picks it.
pub fn preview_branch(prompt: &str, branches: &[(String, Oid)]) -> String {
    let base = slug(&title_from_prompt(prompt));
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
    Ok(Target {
        work_dir: repository
            .work_dir()
            .map_or_else(|| path.to_owned(), Path::to_owned),
        common_dir: repository.common_dir(),
        name: repository.name().into(),
        bare: repository.work_dir().is_none(),
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

/// The one-line description of a network profile in its picker.
fn network_description(profile: NetworkProfile) -> &'static str {
    match profile {
        NetworkProfile::AgentOnly => "Downloads from package registries fail",
        NetworkProfile::Packages => "Agent API, package registries and origin",
        NetworkProfile::Open => "Any host",
    }
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
        let agent_name = self.agent_name(cx);
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
                    let item = if alias.is_empty() {
                        let subtitle = format!("As set in {agent_name}");
                        PopupMenuItem::element(move |_, _| two_lines(label, subtitle.clone()))
                    } else {
                        PopupMenuItem::new(label)
                    };
                    menu = menu.item(item.checked(alias == chosen).on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| {
                            this.model_alias = Some(alias.to_owned());
                            cx.notify();
                        })
                        .ok();
                    }));
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
        let chosen = self.start();
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

    fn render_sandbox_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let chosen = self.network(cx);
        let view = cx.entity().downgrade();
        picker("composer-sandbox")
            .child(
                div()
                    .h_flex()
                    .gap(px(6.))
                    .child(
                        Icon::new(IconName::ShieldCheck)
                            .size(px(13.))
                            .text_color(rgb(color::TEXT_MUTED)),
                    )
                    .child(picker_label("Sandbox"))
                    .child(chosen.label()),
            )
            .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, _| {
                menu = menu
                    .min_w(px(320.))
                    .check_side(Side::Right)
                    .label("Network for this thread");
                for profile in NetworkProfile::ALL {
                    let view = view.clone();
                    menu = menu.item(
                        PopupMenuItem::element(move |_, _| {
                            two_lines(profile.label(), network_description(profile))
                        })
                        .checked(profile == chosen)
                        .on_click(move |_, _, cx| {
                            view.update(cx, |this, cx| {
                                this.network = Some(profile);
                                cx.notify();
                            })
                            .ok();
                        }),
                    );
                }
                menu.separator()
                    .item(menu_note(
                        "Writes stay inside the worktree. ~/.ssh and other home secrets are \
                         hidden.",
                    ))
                    .item(menu_note(
                        "With Packages or Open, the agent could send out whatever it can read, \
                         its own login included.",
                    ))
                    .item(manage("Manage sandbox…"))
            })
    }

    fn render_secrets_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let choices = self.secret_choices(cx);
        let picked = self.secrets(cx);
        let value = match (picked.len(), choices.len()) {
            (_, 0) => "none".to_owned(),
            (picked, all) => format!("{picked} of {all}"),
        };
        let view = cx.entity().downgrade();
        let agent_name = self.agent_name(cx);
        Popover::new("composer-secrets-popover")
            .anchor(Anchor::TopLeft)
            .trigger(
                picker("composer-secrets").child(
                    div()
                        .h_flex()
                        .gap(px(6.))
                        .child(
                            Icon::new(IconName::KeyRound)
                                .size(px(13.))
                                .text_color(rgb(color::TEXT_MUTED)),
                        )
                        .child(picker_label("Secrets"))
                        .child(value),
                ),
            )
            .content(move |_, _, cx| {
                let Some(composer) = view.upgrade() else {
                    return div();
                };
                let mono = cx.theme().mono_font_family.clone();
                let (choices, picked) = {
                    let composer = composer.read(cx);
                    (composer.secret_choices(cx), composer.secrets(cx))
                };
                let heading = div()
                    .px(px(4.))
                    .pb(px(4.))
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(color::TEXT_FAINT))
                    .child("SECRETS FOR THIS THREAD");
                let separator = || div().my(px(4.)).h(px(1.)).bg(rgb(color::SEPARATOR));
                let note = |text: &'static str| {
                    div()
                        .px(px(4.))
                        .py(px(4.))
                        .text_size(px(11.5))
                        .line_height(relative(1.5))
                        .text_color(rgb(color::TEXT_MUTED))
                        .child(text)
                };
                let rows = choices.into_iter().map(|(name, scope, _)| {
                    let on = picked.contains(&name);
                    let composer = composer.downgrade();
                    let toggled = name.clone();
                    div()
                        .h_flex()
                        .gap(px(10.))
                        .min_h(px(32.))
                        .px(px(4.))
                        .child(
                            Checkbox::new(SharedString::from(format!("composer-secret-{name}")))
                                .checked(on)
                                .on_click(move |checked, _, cx| {
                                    composer
                                        .update(cx, |this, cx| {
                                            this.toggle_secret(&toggled, *checked, cx)
                                        })
                                        .ok();
                                }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .font_family(mono.clone())
                                .text_size(px(12.))
                                .child(name),
                        )
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(rgb(color::TEXT_MUTED))
                                .child(scope),
                        )
                });
                let mut list = div().v_flex().w(px(330.)).child(heading);
                let rows: Vec<Div> = rows.collect();
                if rows.is_empty() {
                    list = list.child(note(
                        "No secrets yet. A secret is an environment variable whose value stays \
                         in the keychain; add them in Settings.",
                    ));
                } else {
                    list = list.children(rows);
                }
                list.child(separator())
                    .child(
                        div()
                            .h_flex()
                            .items_start()
                            .gap(px(10.))
                            .px(px(4.))
                            .py(px(6.))
                            .child(
                                Icon::new(IconName::Lock)
                                    .size(px(14.))
                                    .mt(px(2.))
                                    .text_color(rgb(color::TEXT_MUTED)),
                            )
                            .child(two_lines(
                                format!("{agent_name}’s own login"),
                                "The agent signs in with it; it is not one of your secrets",
                            )),
                    )
                    .child(separator())
                    .child(note(
                        "Given as environment variables. Values never appear in the prompt or \
                         the transcript.",
                    ))
                    .child(
                        Button::new("composer-manage-secrets")
                            .ghost()
                            .compact()
                            .text_color(rgb(color::TEXT_SECONDARY))
                            .label("Manage secrets…")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(OpenSettings.boxed_clone(), cx)
                            }),
                    )
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
                    .child(initial_square(&label, px(16.)))
                    .child(label),
            )
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu
                    .min_w(px(300.))
                    .max_h(px(360.))
                    .scrollable(true)
                    .check_side(Side::Right)
                    .label("Repository");
                if choices.is_empty() {
                    menu = menu.item(menu_note("No repositories yet. Open one first."));
                }
                for (path, name) in &choices {
                    let view = view.clone();
                    let (path, name, mono) = (path.clone(), name.clone(), mono.clone());
                    let shown = crate::recent::display(&path);
                    let picked = chosen.as_ref() == Some(&path);
                    menu = menu.item(
                        PopupMenuItem::element(move |_, _| {
                            div()
                                .h_flex()
                                .gap(px(10.))
                                .child(initial_square(&name, px(20.)))
                                .child(
                                    div()
                                        .v_flex()
                                        .gap(px(2.))
                                        .child(div().text_size(px(12.5)).child(name.clone()))
                                        .child(
                                            div()
                                                .font_family(mono.clone())
                                                .text_size(px(11.))
                                                .text_color(rgb(color::TEXT_MUTED))
                                                .child(shown.clone()),
                                        ),
                                )
                        })
                        .checked(picked)
                        .on_click(move |_, _, cx| {
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
                            .child(div().flex_1().min_w_0().child(problem.text.clone()))
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
                        .child("It works in the background on its own branch and worktree"),
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
            .when(home, |this| this.child(self.render_repository_picker(cx)))
            .child(self.render_agent_picker(cx))
            .child(self.render_model_picker(cx))
            .child(self.render_from_picker(cx))
            .child(self.render_sandbox_picker(cx))
            .child(self.render_secrets_picker(cx));
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
                    .label("Send")
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
        let card = div()
            .debug_selector(|| "composer".into())
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
            .child(
                div()
                    .text_size(px(if home { 14. } else { 13. }))
                    .child(Textarea::new(&self.prompt).appearance(false)),
            )
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
    use gpui_kit::{Entity, TestAppContext, VisualTestContext, px, size};

    use super::{Composer, ComposerEvent, ComposerMode, preview_branch};
    use crate::agent_threads::{Options, StartRequest, init_with};

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
            preview_branch("Add a blame view. Reuse the diff list.", &branches),
            "agent/add-a-blame-view-3"
        );
        assert_eq!(
            preview_branch("Fix the typo in README", &branches),
            "agent/fix-the-typo-in-readme"
        );
    }

    /// A repository window's composer with two branches, `feature`
    /// checked out, and settings with three secrets, one given by default.
    fn composer(cx: &mut TestAppContext) -> (Entity<Composer>, VisualTestContext) {
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
                    agents: gitez_agent::Registry::builtin(),
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
        };
        let window = cx.open_window(size(px(900.), px(600.)), |window, cx| {
            Composer::new(mode, window, cx)
        });
        let composer = window
            .update(cx, |_, _, cx| cx.entity())
            .expect("window open");
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        // As opening it does.
        composer.update(&mut cx, |composer, cx| composer.check(cx));
        cx.run_until_parked();
        (composer, cx)
    }

    #[gpui_kit::test]
    fn sending_emits_the_picked_branch_network_and_secrets(cx: &mut TestAppContext) {
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
            composer.network = Some(NetworkProfile::Packages);
            composer.model_alias = Some("haiku".into());
            composer.toggle_secret("SENTRY_DSN", true, cx);
            composer.toggle_secret("GITHUB_TOKEN", false, cx);
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
        assert_eq!(request.network, NetworkProfile::Packages);
        assert_eq!(request.secrets, ["SENTRY_DSN"]);
        assert_eq!(request.model.as_deref(), Some("haiku"));
        assert_eq!(request.prompt, "Fix the typo in README. Keep it short.");
        assert_eq!(request.repository_name, "repo");
        // The prompt is emptied for the next thread.
        composer.read_with(&cx, |composer, cx| {
            assert!(composer.prompt.read(cx).value().is_empty());
        });
    }
}
