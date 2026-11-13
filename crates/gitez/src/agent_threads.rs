//! The agent threads of every repository GitEZ knows of, shared by all
//! windows. An agent thread runs in a supervisor process of its own
//! (`gitez supervise …`), which writes its state to
//! `<git common dir>/gitez/threads/<id>/thread.json`; this model reads
//! those files, follows them by watching their folders, and starts,
//! continues, stops and completes agent threads.
//!
//! The repositories with agent threads are listed in `threads.json` in
//! GitEZ's data folder ([`gitez_agent_thread::Index`]), so the threads of
//! repositories that are not open are known too. Every read and write runs
//! on the background executor.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context as _, bail};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gitez_agent_thread::{
    AgentThread, AgentThreadId, Ending, IfUncommitted, Index, IndexedRepository, Kept,
    NewAgentThread, State, Step, Store, Timestamp, TurnKind,
};
use gitez_sandbox::NetworkProfile;
use gitez_secret::SecretStore;
use gitez_settings::Settings;
use gitez_supervisor::{Command, Launch, Secrets};
use gpui_kit::*;

actions!(agent_threads, [OpenSettings]);

/// Opens agent thread `id` of the repository at `common_dir` in the
/// window's detail panel (the workspace handles it).
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = agent_threads, no_json)]
pub struct OpenAgentThread {
    pub common_dir: PathBuf,
    pub id: AgentThreadId,
}

/// Opens local pull request `number`, which an agent thread of the
/// repository at `common_dir` created (the workspace handles it).
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = agent_threads, no_json)]
pub struct OpenAgentPullRequest {
    pub common_dir: PathBuf,
    pub number: u32,
}

/// Names an agent thread across repositories.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentThreadKey {
    pub common_dir: PathBuf,
    pub id: AgentThreadId,
}

/// The agent threads of one repository.
pub struct RepositoryThreads {
    pub common_dir: PathBuf,
    /// The main worktree.
    pub work_dir: PathBuf,
    pub name: SharedString,
    /// Newest first.
    pub threads: Vec<Arc<AgentThread>>,
    /// Why some threads could not be read.
    pub error: Option<SharedString>,
    /// `common_dir` with symlinks resolved, as file events name it.
    canonical: PathBuf,
}

/// How many agent threads are in each group.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Preparing or running.
    pub active: usize,
    /// Queued, or waiting for the usage limit to reset.
    pub waiting: usize,
    /// Finished with a pull request or an answer.
    pub ready: usize,
    /// Finished after failing or being stopped.
    pub failed: usize,
    pub completed: usize,
}

/// What the windows should react to.
pub enum AgentThreadsEvent {
    /// Agent threads of this repository changed (state, pull request,
    /// activity).
    Changed { common_dir: PathBuf },
    /// The transcript of this agent thread grew.
    Transcript(AgentThreadKey),
}

/// What a new agent thread starts with.
#[derive(Debug, Clone)]
pub struct StartRequest {
    pub work_dir: PathBuf,
    pub common_dir: PathBuf,
    pub repository_name: String,
    pub prompt: String,
    pub agent: String,
    pub model: Option<String>,
    pub start: gitez_agent_thread::Start,
    pub network: NetworkProfile,
    pub secrets: Vec<String>,
}

/// The repository open in the window, for Settings' "In this repository".
pub struct CurrentRepository {
    pub work_dir: PathBuf,
    pub common_dir: PathBuf,
    pub name: SharedString,
}

/// The [`AgentThreads`] of the app.
struct AgentThreadsGlobal(Entity<AgentThreads>);

impl Global for AgentThreadsGlobal {}

/// How long to keep collecting file events after the first, so a burst of
/// writes is read once.
const WATCH_BATCH: Duration = Duration::from_millis(150);
/// How often supervisors of active threads are checked for being alive.
const LIVENESS_EVERY: Duration = Duration::from_secs(5);
/// Liveness checks between redraws that advance elapsed times (30 s).
const REDRAW_EVERY_CHECKS: u32 = 6;
/// Set to log what the model reads and reaps, to standard error.
const DEBUG_ENV: &str = "GITEZ_DEBUG_THREADS";

/// The agent threads of all repositories GitEZ knows of, and what is
/// needed to start more: settings, agents and the sandbox.
pub struct AgentThreads {
    data_dir: Option<PathBuf>,
    /// `data_dir` with symlinks resolved, once read.
    canonical_data_dir: Option<PathBuf>,
    settings: Arc<Settings>,
    /// Why the settings could not be used as they are, for Settings.
    settings_error: Option<SharedString>,
    /// False when `settings.json` was written by a newer GitEZ: it is
    /// never overwritten.
    can_save_settings: bool,
    /// The last save of the settings; the next waits for it.
    saving_settings: Option<Task<()>>,
    /// Saves not yet finished; the settings file is not read back meanwhile.
    pending_saves: usize,
    agents: gitez_agent::Registry,
    sandbox: Arc<dyn gitez_sandbox::Sandbox>,
    /// Where secret values are read from to launch supervisors, and
    /// written to by Settings.
    secrets: Arc<dyn SecretStore>,
    /// Counts changes to the agents' or the sandbox's installation that the
    /// settings do not show, such as installing the sandbox, so views that
    /// checked them check again.
    environment: u64,
    /// The `gitez` program, started as `gitez supervise …`.
    exe: Result<PathBuf, String>,
    repositories: Vec<RepositoryThreads>,
    current: Option<CurrentRepository>,
    /// Threads being completed because their pull request was merged or
    /// closed, so a burst of file events completes each once.
    completing: HashSet<AgentThreadKey>,
    /// File events and reload requests, read in order by one task.
    changes: mpsc::UnboundedSender<Vec<PathBuf>>,
    watch: bool,
    watched: Vec<PathBuf>,
    _watchers: Option<Watchers>,
    _rewatch: Task<()>,
    _follow: Task<()>,
    _liveness: Task<()>,
}

impl EventEmitter<AgentThreadsEvent> for AgentThreads {}

/// What [`AgentThreads`] is made from; tests make their own.
pub(crate) struct Options {
    pub data_dir: Option<PathBuf>,
    pub sandbox: Arc<dyn gitez_sandbox::Sandbox>,
    pub agents: gitez_agent::Registry,
    pub secrets: Arc<dyn SecretStore>,
    pub exe: Result<PathBuf, String>,
    /// Watch the files and check that supervisors are alive.
    pub watch: bool,
}

/// Creates the app's [`AgentThreads`] and starts reading the threads of
/// every indexed repository. Call once, after `recent::init`.
pub fn init(cx: &mut App) {
    let data_dir = gitez_settings::data_dir();
    let sandbox = test_sandbox()
        .unwrap_or_else(|| gitez_sandbox::platform(data_dir.as_deref().unwrap_or(Path::new(""))));
    let options = Options {
        data_dir,
        sandbox,
        agents: gitez_agent::Registry::builtin(),
        secrets: Arc::new(gitez_secret::Keychain::new(
            gitez_secret::Keychain::default_service(),
        )),
        exe: std::env::current_exe()
            .map_err(|error| format!("GitEZ could not find its own program: {error}")),
        watch: true,
    };
    init_with(options, cx);
}

/// [`init`] with `options`.
pub(crate) fn init_with(options: Options, cx: &mut App) {
    let model = cx.new(|cx| AgentThreads::new(options, cx));
    cx.set_global(AgentThreadsGlobal(model));
}

/// The pass-through sandbox the supervisor uses with
/// `GITEZ_TEST_SANDBOX=passthrough` in a debug build, so the UI does not
/// ask for a sandbox the supervisor will not use.
fn test_sandbox() -> Option<Arc<dyn gitez_sandbox::Sandbox>> {
    (cfg!(debug_assertions)
        && std::env::var_os("GITEZ_TEST_SANDBOX").is_some_and(|value| value == "passthrough"))
    .then(|| Arc::new(gitez_sandbox::Passthrough) as Arc<dyn gitez_sandbox::Sandbox>)
}

/// Logs `message` when `GITEZ_DEBUG_THREADS` is set.
fn debug(message: impl FnOnce() -> String) {
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os(DEBUG_ENV).is_some_and(|value| !value.is_empty())) {
        eprintln!("gitez threads: {}", message());
    }
}

impl AgentThreads {
    fn new(options: Options, cx: &mut Context<Self>) -> Self {
        let (changes, received) = mpsc::unbounded();
        // Everything is read first, then followed.
        changes.unbounded_send(Vec::new()).ok();
        let follow = cx.spawn(async move |this, cx| follow(this, received, cx).await);
        let liveness = if options.watch {
            cx.spawn(async move |this, cx| check_liveness(this, cx).await)
        } else {
            Task::ready(())
        };
        Self {
            data_dir: options.data_dir,
            canonical_data_dir: None,
            settings: Arc::new(Settings::default()),
            settings_error: None,
            can_save_settings: true,
            saving_settings: None,
            pending_saves: 0,
            agents: options.agents,
            sandbox: options.sandbox,
            secrets: options.secrets,
            environment: 0,
            exe: options.exe,
            repositories: Vec::new(),
            current: None,
            completing: HashSet::new(),
            changes,
            watch: options.watch,
            watched: Vec::new(),
            _watchers: None,
            _rewatch: Task::ready(()),
            _follow: follow,
            _liveness: liveness,
        }
    }

    pub fn global(cx: &App) -> Entity<AgentThreads> {
        cx.global::<AgentThreadsGlobal>().0.clone()
    }

    /// [`AgentThreads::global`], or `None` before [`init`] (in tests of
    /// views that do not need it).
    pub fn try_global(cx: &App) -> Option<Entity<AgentThreads>> {
        cx.try_global::<AgentThreadsGlobal>()
            .map(|global| global.0.clone())
    }

    pub fn data_dir(&self) -> Option<&Path> {
        self.data_dir.as_deref()
    }

    pub fn repositories(&self) -> &[RepositoryThreads] {
        &self.repositories
    }

    pub fn repository(&self, common_dir: &Path) -> Option<&RepositoryThreads> {
        self.repositories.iter().find(|repository| {
            repository.common_dir == common_dir || repository.canonical == common_dir
        })
    }

    pub fn repository_by_work_dir(&self, work_dir: &Path) -> Option<&RepositoryThreads> {
        self.repositories
            .iter()
            .find(|repository| repository.work_dir == work_dir)
    }

    pub fn thread(&self, key: &AgentThreadKey) -> Option<&Arc<AgentThread>> {
        self.repository(&key.common_dir)?
            .threads
            .iter()
            .find(|thread| thread.id == key.id)
    }

    /// The agent thread that created local pull request `number`, if any.
    pub fn for_pull_request(&self, common_dir: &Path, number: u32) -> Option<&Arc<AgentThread>> {
        self.repository(common_dir)?
            .threads
            .iter()
            .find(|thread| thread.pull_request == Some(number))
    }

    /// Of one repository, or of all with `None`.
    pub fn counts(&self, common_dir: Option<&Path>) -> Counts {
        match common_dir {
            Some(common_dir) => self
                .repository(common_dir)
                .map(|repository| Counts::of(repository.threads.iter().map(Arc::as_ref)))
                .unwrap_or_default(),
            None => Counts::of(
                self.repositories
                    .iter()
                    .flat_map(|repository| repository.threads.iter().map(Arc::as_ref)),
            ),
        }
    }

    /// Agent threads holding a running-agent slot now (Preparing, Running),
    /// across repositories.
    pub fn running(&self) -> usize {
        self.counts(None).active
    }

    pub fn settings(&self) -> &Arc<Settings> {
        &self.settings
    }

    /// Why the settings could not be read or saved, if so.
    pub fn settings_error(&self) -> Option<&SharedString> {
        self.settings_error.as_ref()
    }

    pub fn agents(&self) -> &gitez_agent::Registry {
        &self.agents
    }

    pub fn sandbox(&self) -> &Arc<dyn gitez_sandbox::Sandbox> {
        &self.sandbox
    }

    /// Where secret values are kept. Every call may block (the OS
    /// keychain): use it on the background executor.
    pub fn secret_store(&self) -> &Arc<dyn SecretStore> {
        &self.secrets
    }

    /// Changes when an agent or the sandbox may have been installed; see
    /// [`AgentThreads::environment_changed`].
    pub fn environment(&self) -> u64 {
        self.environment
    }

    /// Says that what agents and the sandbox find may have changed (the
    /// sandbox was installed), so views check them again.
    pub fn environment_changed(&mut self, cx: &mut Context<Self>) {
        self.environment += 1;
        cx.notify();
    }

    pub fn current_repository(&self) -> Option<&CurrentRepository> {
        self.current.as_ref()
    }

    pub fn set_current_repository(
        &mut self,
        current: Option<CurrentRepository>,
        cx: &mut Context<Self>,
    ) {
        self.current = current;
        cx.notify();
    }

    /// Changes the settings, saves them in the background (after the
    /// previous save) and notifies.
    pub fn update_settings(&mut self, change: impl FnOnce(&mut Settings), cx: &mut Context<Self>) {
        let mut settings = (*self.settings).clone();
        change(&mut settings);
        if settings == *self.settings {
            return;
        }
        self.settings = Arc::new(settings);
        cx.notify();
        if !self.can_save_settings {
            return;
        }
        let Some(path) = self.data_dir.as_deref().map(Settings::path) else {
            return;
        };
        let settings = self.settings.clone();
        let previous = self.saving_settings.take();
        self.pending_saves += 1;
        self.saving_settings = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let saved = cx
                .background_executor()
                .spawn(async move { settings.save(&path) })
                .await;
            this.update(cx, |this, cx| {
                this.pending_saves -= 1;
                this.settings_error = saved.err().map(|error| {
                    format!(
                        "Could not save the settings: {:#}",
                        anyhow::Error::from(error)
                    )
                    .into()
                });
                cx.notify();
            })
            .ok();
        }));
    }

    /// Creates the agent thread (queued), lists its repository in the
    /// index, reads its secrets from the keychain and starts its
    /// supervisor with `gitez supervise start`. Runs in the background.
    pub fn start(
        &mut self,
        request: StartRequest,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<AgentThreadKey>> {
        let launcher = self.launcher();
        let data_dir = self.data_dir.clone();
        let changes = self.changes.clone();
        cx.background_executor().spawn(async move {
            let data_dir =
                data_dir.context("GitEZ has no data folder to keep the thread's worktree in")?;
            let repository = gitez_git::Repository::open(&request.work_dir)?;
            let store = Store::new(&request.common_dir);
            let thread = store.create(
                NewAgentThread {
                    prompt: request.prompt,
                    agent: request.agent,
                    model: request.model,
                    start: request.start,
                    network: request.network,
                    secrets: request.secrets,
                },
                &data_dir,
                &request.repository_name,
                |branch| {
                    repository
                        .resolve_commit(&format!("refs/heads/{branch}"))
                        .is_ok_and(|tip| tip.is_some())
                },
                Timestamp::now(),
            )?;
            let key = AgentThreadKey {
                common_dir: request.common_dir.clone(),
                id: thread.id.clone(),
            };
            debug(|| format!("created {}", key.id));
            let index = Index::new(&data_dir);
            let indexed = index.add(IndexedRepository {
                common_dir: request.common_dir.clone(),
                work_dir: request.work_dir,
                name: request.repository_name,
            });
            changes
                .unbounded_send(vec![index.path().to_owned(), thread_file(&key)])
                .ok();
            if let Err(error) = indexed {
                // The thread runs; only Home misses it while it is not open.
                let warning = format!(
                    "GitEZ could not list this repository among those with threads: {:#}",
                    anyhow::Error::from(error)
                );
                store.update(&key.id, |thread| {
                    thread.warnings.push(warning);
                    Ok(())
                })?;
            }
            launcher.launch(&store, Command::Start, &key, &thread)?;
            Ok(key)
        })
    }

    /// Adds a reply or review turn (`request_turn`), then continues the
    /// agent thread with `gitez supervise reply`.
    pub fn reply(
        &mut self,
        key: &AgentThreadKey,
        kind: TurnKind,
        prompt: String,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let launcher = self.launcher();
        let changes = self.changes.clone();
        let key = key.clone();
        cx.background_executor().spawn(async move {
            let store = Store::new(&key.common_dir);
            let thread = store.update(&key.id, |thread| {
                thread.request_turn(kind, prompt, Timestamp::now())?;
                Ok(thread.clone())
            })?;
            changes.unbounded_send(vec![thread_file(&key)]).ok();
            launcher.launch(&store, Command::Reply, &key, &thread)
        })
    }

    /// Prepares a retry with `network` (`request_retry`), then retries the
    /// agent thread with `gitez supervise retry`.
    pub fn retry(
        &mut self,
        key: &AgentThreadKey,
        network: NetworkProfile,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let launcher = self.launcher();
        let changes = self.changes.clone();
        let key = key.clone();
        cx.background_executor().spawn(async move {
            let store = Store::new(&key.common_dir);
            let thread = store.update(&key.id, |thread| {
                thread.request_retry(network, Timestamp::now())?;
                Ok(thread.clone())
            })?;
            changes.unbounded_send(vec![thread_file(&key)]).ok();
            launcher.launch(&store, Command::Retry, &key, &thread)
        })
    }

    /// Asks the agent thread's supervisor to stop it: a running agent's
    /// turn ends cleanly, a queued thread leaves the queue.
    pub fn stop(
        &mut self,
        key: &AgentThreadKey,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let key = key.clone();
        cx.background_executor().spawn(async move {
            let store = Store::new(&key.common_dir);
            let thread = store.load(&key.id)?;
            if !thread.state.is_active() {
                bail!("The thread is not running.");
            }
            let Some(supervisor) = &thread.supervisor else {
                bail!("The thread’s supervisor is still starting. Try again in a moment.");
            };
            match gitez_supervisor::stop(&store.thread_dir(&key.id), supervisor) {
                Ok(()) => Ok(()),
                Err(gitez_supervisor::StopError::NotRunning) => {
                    // It died: say so on the thread rather than wait for
                    // the next liveness check.
                    gitez_supervisor::reap_if_dead(&store, &key.id, Timestamp::now())?;
                    bail!("The thread’s supervisor is not running any more.")
                }
                Err(error) => Err(error.into()),
            }
        })
    }

    /// Completes a finished agent thread: removes its worktree (and its
    /// branch if it has no commits of its own), then marks it completed.
    pub fn complete(
        &mut self,
        key: &AgentThreadKey,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let key = key.clone();
        let work_dir = self
            .repository(&key.common_dir)
            .map(|repository| repository.work_dir.clone());
        let changes = self.changes.clone();
        cx.background_executor().spawn(async move {
            let store = Store::new(&key.common_dir);
            let thread = store.load(&key.id)?;
            // Refused unless it is finished; checked before anything is
            // removed.
            thread.clone().mark_completed(Timestamp::now())?;
            let work_dir = work_dir.context("GitEZ does not know this thread’s repository")?;
            // Refuse by default: an uncommitted worktree is never removed on
            // one click. The GUI confirms, then calls again with Discard or
            // Keep (see gitez_agent_thread::IfUncommitted). A tampered
            // worktree is refused the same way.
            gitez_agent_thread::complete(&work_dir, &thread, IfUncommitted::Refuse)?;
            store.update(&key.id, |thread| thread.mark_completed(Timestamp::now()))?;
            changes.unbounded_send(vec![thread_file(&key)]).ok();
            Ok(())
        })
    }

    /// What a background launch needs.
    fn launcher(&self) -> Launcher {
        Launcher {
            exe: self.exe.clone(),
            settings: self.settings.clone(),
            secrets: self.secrets.clone(),
        }
    }

    /// The agent threads that should have a live supervisor.
    fn active_threads(&self) -> Vec<AgentThreadKey> {
        self.repositories
            .iter()
            .flat_map(|repository| {
                repository
                    .threads
                    .iter()
                    .filter(|thread| thread.state.is_active())
                    .map(|thread| AgentThreadKey {
                        common_dir: repository.common_dir.clone(),
                        id: thread.id.clone(),
                    })
            })
            .collect()
    }

    /// What to read again for the changed `paths`; all of it when
    /// `everything`.
    fn plan(&self, paths: &[PathBuf], everything: bool) -> Plan {
        let mut plan = Plan {
            data_dir: self.data_dir.clone(),
            ..Plan::default()
        };
        if everything {
            plan.settings = true;
            plan.index = true;
            return plan;
        }
        let data_dirs: Vec<&Path> = self
            .data_dir
            .iter()
            .chain(&self.canonical_data_dir)
            .map(PathBuf::as_path)
            .collect();
        for path in paths {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            // Being written; the rename that follows names the file itself.
            if name.ends_with(".lock") {
                continue;
            }
            if path
                .parent()
                .is_some_and(|parent| data_dirs.contains(&parent))
            {
                match name {
                    "settings.json" => plan.settings = true,
                    "threads.json" => plan.index = true,
                    _ => {}
                }
                continue;
            }
            for repository in &self.repositories {
                let rest = [&repository.common_dir, &repository.canonical]
                    .iter()
                    .find_map(|dir| path.strip_prefix(dir.join("gitez")).ok());
                if let Some(rest) = rest {
                    plan.add(&repository.common_dir, rest);
                    break;
                }
            }
        }
        plan
    }

    /// Takes in what [`Plan::load`] read, and tells the windows what
    /// changed.
    fn apply(&mut self, loaded: Loaded, cx: &mut Context<Self>) {
        let mut changed = BTreeSet::new();
        // Read again, so perhaps finished since their pull request was
        // merged or closed: whole repositories, and single threads.
        let mut reread: Vec<PathBuf> = loaded.reviews.iter().cloned().collect();
        let mut reread_threads = Vec::new();
        if let Some(dir) = loaded.canonical_data_dir {
            self.canonical_data_dir = Some(dir);
        }
        if let Some(settings) = loaded.settings
            && self.pending_saves == 0
        {
            self.take_settings(settings);
        }
        if let Some(index) = loaded.index {
            match index {
                Ok(listed) => {
                    let mut repositories = Vec::with_capacity(listed.len());
                    for (repository, canonical) in listed {
                        let threads = match self.repository(&repository.common_dir) {
                            Some(known) => known.threads.clone(),
                            None => {
                                changed.insert(repository.common_dir.clone());
                                Vec::new()
                            }
                        };
                        repositories.push(RepositoryThreads {
                            common_dir: repository.common_dir,
                            work_dir: repository.work_dir,
                            name: repository.name.into(),
                            threads,
                            error: None,
                            canonical,
                        });
                    }
                    self.repositories = repositories;
                    self.rewatch(cx);
                }
                Err(error) => eprintln!(
                    "gitez: could not read the repositories with threads: {:#}",
                    anyhow::Error::from(error)
                ),
            }
        }
        for (common_dir, read) in loaded.repositories {
            let Some(repository) = self.repository_mut(&common_dir) else {
                continue;
            };
            let (threads, error) = match read {
                Ok((threads, errors)) => (
                    threads.into_iter().map(Arc::new).collect::<Vec<_>>(),
                    unreadable(&errors),
                ),
                Err(error) => (Vec::new(), Some(error)),
            };
            if repository.threads != threads || repository.error != error {
                repository.threads = threads;
                repository.error = error;
                changed.insert(repository.common_dir.clone());
                reread.push(repository.common_dir.clone());
            }
        }
        for (common_dir, id, read) in loaded.threads {
            let Some(repository) = self.repository_mut(&common_dir) else {
                continue;
            };
            let position = repository.threads.iter().position(|thread| thread.id == id);
            if read.is_ok() {
                reread_threads.push(AgentThreadKey {
                    common_dir: repository.common_dir.clone(),
                    id: id.clone(),
                });
            }
            match (read, position) {
                (Ok(thread), Some(position)) => {
                    if *repository.threads[position] == thread {
                        continue;
                    }
                    repository.threads[position] = Arc::new(thread);
                }
                (Ok(thread), None) => {
                    repository.threads.push(Arc::new(thread));
                    repository
                        .threads
                        .sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
                }
                (Err(gitez_agent_thread::Error::NotFound(_)), Some(position)) => {
                    repository.threads.remove(position);
                }
                (Err(gitez_agent_thread::Error::NotFound(_)), None) => continue,
                (Err(error), _) => {
                    repository.error = Some(
                        format!("Could not read {id}: {:#}", anyhow::Error::from(error)).into(),
                    );
                }
            }
            changed.insert(repository.common_dir.clone());
        }
        self.complete_merged(&reread, &reread_threads, cx);
        for common_dir in changed {
            debug(|| format!("changed {}", common_dir.display()));
            cx.emit(AgentThreadsEvent::Changed { common_dir });
        }
        for key in loaded.transcripts {
            cx.emit(AgentThreadsEvent::Transcript(key));
        }
        cx.notify();
    }

    fn repository_mut(&mut self, common_dir: &Path) -> Option<&mut RepositoryThreads> {
        self.repositories.iter_mut().find(|repository| {
            repository.common_dir == common_dir || repository.canonical == common_dir
        })
    }

    /// Uses settings read from `settings.json`.
    fn take_settings(&mut self, read: gitez_settings::Result<gitez_settings::Loaded>) {
        match read {
            Ok(gitez_settings::Loaded::Current(settings)) => {
                self.can_save_settings = true;
                self.settings_error = None;
                self.settings = Arc::new(settings);
            }
            Ok(gitez_settings::Loaded::TooNew) => {
                self.can_save_settings = false;
                self.settings_error = Some(
                    "A newer GitEZ wrote these settings. They are shown with their defaults, \
                     and changes are not saved."
                        .into(),
                );
                self.settings = Arc::new(Settings::default());
            }
            Err(error) => {
                self.can_save_settings = true;
                self.settings_error = Some(
                    format!(
                        "Could not read the settings, so the defaults are used; a change \
                         replaces them: {:#}",
                        anyhow::Error::from(error)
                    )
                    .into(),
                );
            }
        }
    }

    /// Watches the folders of the indexed repositories' threads and the
    /// data folder, again if the repositories changed.
    fn rewatch(&mut self, cx: &mut Context<Self>) {
        if !self.watch {
            return;
        }
        let dirs: Vec<PathBuf> = self
            .repositories
            .iter()
            .map(|repository| repository.common_dir.join("gitez"))
            .collect();
        if dirs == self.watched && self._watchers.is_some() {
            return;
        }
        self.watched = dirs.clone();
        let data_dir = self.data_dir.clone();
        let changes = self.changes.clone();
        let start = cx
            .background_executor()
            .spawn(async move { Watchers::start(&dirs, data_dir.as_deref(), changes) });
        self._rewatch = cx.spawn(async move |this, cx| {
            let watchers = start.await;
            this.update(cx, |this, _| this._watchers = Some(watchers))
                .ok();
        });
    }
}

impl Counts {
    /// The counts of `threads`.
    pub fn of<'a>(threads: impl IntoIterator<Item = &'a AgentThread>) -> Self {
        use gitez_agent_thread::State;
        let mut counts = Counts::default();
        for thread in threads {
            match thread.state {
                State::Preparing | State::Running => counts.active += 1,
                State::Queued | State::Waiting => counts.waiting += 1,
                State::Completed => counts.completed += 1,
                State::Finished => match thread.ending {
                    Some(Ending::Failed | Ending::Stopped) => counts.failed += 1,
                    Some(Ending::PullRequest | Ending::Answer) | None => counts.ready += 1,
                },
            }
        }
        counts
    }
}

/// `thread.json` of the agent thread `key`.
fn thread_file(key: &AgentThreadKey) -> PathBuf {
    Store::new(&key.common_dir)
        .thread_dir(&key.id)
        .join("thread.json")
}

/// "Could not read some threads: …", or `None` when every one was read.
fn unreadable(errors: &[(AgentThreadId, gitez_agent_thread::Error)]) -> Option<SharedString> {
    if errors.is_empty() {
        return None;
    }
    let errors: Vec<String> = errors
        .iter()
        .map(|(id, error)| format!("{id}: {:#}", anyhow::Error::from_ref(error)))
        .collect();
    Some(format!("Could not read some threads: {}", errors.join("; ")).into())
}

trait FromRef {
    fn from_ref(error: &gitez_agent_thread::Error) -> String;
}

impl FromRef for anyhow::Error {
    /// `error` and its causes, as `{:#}` shows an `anyhow::Error`.
    fn from_ref(error: &gitez_agent_thread::Error) -> String {
        let mut text = error.to_string();
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            text.push_str(": ");
            text.push_str(&cause.to_string());
            source = cause.source();
        }
        text
    }
}

/// Reads file events and reload requests in order: collects a burst,
/// reads what it touched in the background, then applies it.
async fn follow(
    this: WeakEntity<AgentThreads>,
    mut received: mpsc::UnboundedReceiver<Vec<PathBuf>>,
    cx: &mut AsyncApp,
) {
    while let Some(mut paths) = received.next().await {
        let mut everything = paths.is_empty();
        // An empty batch asks for everything at once; file events wait for
        // the rest of their burst.
        if !everything {
            cx.background_executor().timer(WATCH_BATCH).await;
        }
        while let Ok(more) = received.try_recv() {
            everything |= more.is_empty();
            paths.extend(more);
        }
        let Ok(plan) = this.update(cx, |this, _| this.plan(&paths, everything)) else {
            return;
        };
        if plan.is_empty() {
            continue;
        }
        debug(|| format!("reading {plan:?}"));
        let known = this
            .read_with(cx, |this, _| {
                this.repositories
                    .iter()
                    .map(|repository| repository.common_dir.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let loaded = cx
            .background_executor()
            .spawn(async move { plan.load(&known) })
            .await;
        if this.update(cx, |this, cx| this.apply(loaded, cx)).is_err() {
            return;
        }
    }
}

/// Every few seconds, marks failed the active agent threads whose
/// supervisor died, and every 30 s redraws so elapsed times advance.
async fn check_liveness(this: WeakEntity<AgentThreads>, cx: &mut AsyncApp) {
    let mut checks = 0u32;
    loop {
        cx.background_executor().timer(LIVENESS_EVERY).await;
        checks = checks.wrapping_add(1);
        let Ok(active) = this.update(cx, |this, cx| {
            let active = this.active_threads();
            if !active.is_empty() && checks.is_multiple_of(REDRAW_EVERY_CHECKS) {
                cx.notify();
            }
            active
        }) else {
            return;
        };
        if active.is_empty() {
            continue;
        }
        let reaped = cx
            .background_executor()
            .spawn(async move {
                let now = Timestamp::now();
                active
                    .into_iter()
                    .filter(|key| {
                        let store = Store::new(&key.common_dir);
                        match gitez_supervisor::reap_if_dead(&store, &key.id, now) {
                            Ok(reaped) => reaped,
                            Err(error) => {
                                debug(|| format!("could not check {}: {error}", key.id));
                                false
                            }
                        }
                    })
                    .map(|key| thread_file(&key))
                    .collect::<Vec<_>>()
            })
            .await;
        if reaped.is_empty() {
            continue;
        }
        debug(|| format!("reaped {reaped:?}"));
        let sent = this.update(cx, |this, _| {
            this.changes.unbounded_send(reaped).ok();
        });
        if sent.is_err() {
            return;
        }
    }
}

/// What to read again.
#[derive(Debug, Default)]
struct Plan {
    data_dir: Option<PathBuf>,
    settings: bool,
    /// The index, and then every repository in it.
    index: bool,
    /// Repositories whose threads are all read again.
    repositories: BTreeSet<PathBuf>,
    threads: BTreeMap<PathBuf, BTreeSet<AgentThreadId>>,
    transcripts: Vec<AgentThreadKey>,
    /// Repositories whose local pull requests changed: their threads may
    /// complete (E.11).
    reviews: BTreeSet<PathBuf>,
}

/// What [`Plan::load`] read.
struct Loaded {
    canonical_data_dir: Option<PathBuf>,
    settings: Option<gitez_settings::Result<gitez_settings::Loaded>>,
    index: Option<gitez_agent_thread::Result<Vec<(IndexedRepository, PathBuf)>>>,
    #[allow(clippy::type_complexity)]
    repositories: Vec<(
        PathBuf,
        Result<
            (
                Vec<AgentThread>,
                Vec<(AgentThreadId, gitez_agent_thread::Error)>,
            ),
            SharedString,
        >,
    )>,
    threads: Vec<(
        PathBuf,
        AgentThreadId,
        gitez_agent_thread::Result<AgentThread>,
    )>,
    transcripts: Vec<AgentThreadKey>,
    reviews: BTreeSet<PathBuf>,
}

impl Plan {
    /// Adds what a change at `rest`, a path inside `<common_dir>/gitez`,
    /// means.
    fn add(&mut self, common_dir: &Path, rest: &Path) {
        let parts: Vec<&str> = rest
            .components()
            .filter_map(|component| match component {
                Component::Normal(part) => part.to_str(),
                _ => None,
            })
            .collect();
        match parts.as_slice() {
            [] | ["threads"] => {
                self.repositories.insert(common_dir.to_owned());
            }
            ["threads", id, rest @ ..] => {
                let Ok(id) = AgentThreadId::new(*id) else {
                    return;
                };
                match rest {
                    ["runs", ..] => {
                        let key = AgentThreadKey {
                            common_dir: common_dir.to_owned(),
                            id,
                        };
                        if !self.transcripts.contains(&key) {
                            self.transcripts.push(key);
                        }
                    }
                    [] | ["thread.json"] => {
                        self.threads
                            .entry(common_dir.to_owned())
                            .or_default()
                            .insert(id);
                    }
                    // Logs and the supervisor's lock.
                    _ => {}
                }
            }
            // A local pull request saved, perhaps merged or closed; not the
            // review panel's own agent runs.
            ["reviews"] | ["reviews", _] | ["reviews", _, "review.json"] => {
                self.reviews.insert(common_dir.to_owned());
            }
            _ => {}
        }
    }

    fn is_empty(&self) -> bool {
        !self.settings
            && !self.index
            && self.repositories.is_empty()
            && self.threads.is_empty()
            && self.transcripts.is_empty()
            && self.reviews.is_empty()
    }

    /// Reads what the plan names. `known` are the repositories listed now.
    /// Blocks.
    fn load(self, known: &[PathBuf]) -> Loaded {
        let canonical_data_dir = self
            .data_dir
            .as_deref()
            .and_then(|dir| std::fs::canonicalize(dir).ok());
        let settings = match (&self.data_dir, self.settings) {
            (Some(dir), true) => Some(Settings::load(&Settings::path(dir))),
            _ => None,
        };
        let index = match (&self.data_dir, self.index) {
            (Some(dir), true) => Some(Index::new(dir).load().map(|repositories| {
                repositories
                    .into_iter()
                    .map(|repository| {
                        let canonical = std::fs::canonicalize(&repository.common_dir)
                            .unwrap_or_else(|_| repository.common_dir.clone());
                        (repository, canonical)
                    })
                    .collect::<Vec<_>>()
            })),
            _ => None,
        };
        let mut whole: Vec<PathBuf> = match &index {
            Some(Ok(listed)) => listed
                .iter()
                .map(|(repository, _)| repository.common_dir.clone())
                .collect(),
            _ => Vec::new(),
        };
        for common_dir in self.repositories {
            if !whole.contains(&common_dir) && known.contains(&common_dir) {
                whole.push(common_dir);
            }
        }
        let repositories = whole
            .iter()
            .map(|common_dir| {
                let store = Store::new(common_dir);
                let read = match store.ids() {
                    Ok(_) => Ok(store.load_all()),
                    Err(error) => Err(format!(
                        "Could not list the threads: {:#}",
                        anyhow::Error::from(error)
                    )
                    .into()),
                };
                (common_dir.clone(), read)
            })
            .collect();
        let mut threads = Vec::new();
        for (common_dir, ids) in self.threads {
            if whole.contains(&common_dir) {
                continue;
            }
            let store = Store::new(&common_dir);
            for id in ids {
                let read = store.load(&id);
                threads.push((common_dir.clone(), id, read));
            }
        }
        Loaded {
            canonical_data_dir,
            settings,
            index,
            repositories,
            threads,
            transcripts: self.transcripts,
            reviews: self.reviews,
        }
    }
}

/// Watches until dropped: the indexed repositories' `gitez` folders, and
/// GitEZ's data folder itself (not the worktrees inside it).
struct Watchers {
    _threads: Option<gitez_agent_thread::Watcher>,
    _data_dir: Option<notify::RecommendedWatcher>,
}

impl Watchers {
    /// Starts watching; what cannot be watched is logged and left out.
    fn start(
        dirs: &[PathBuf],
        data_dir: Option<&Path>,
        changes: mpsc::UnboundedSender<Vec<PathBuf>>,
    ) -> Self {
        let send = {
            let changes = changes.clone();
            move |paths: Vec<PathBuf>| {
                changes.unbounded_send(paths).ok();
            }
        };
        let threads = match gitez_agent_thread::watch(dirs, send) {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                eprintln!(
                    "gitez: changes to threads appear only after a restart: {:#}",
                    anyhow::Error::from(error)
                );
                None
            }
        };
        let data_dir = data_dir.and_then(|dir| {
            use notify::Watcher as _;
            std::fs::create_dir_all(dir).ok();
            let watcher = notify::recommended_watcher(
                move |event: notify::Result<notify::Event>| match event {
                    Ok(event) if event.need_rescan() => {
                        changes.unbounded_send(Vec::new()).ok();
                    }
                    Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => {}
                    Ok(event) => {
                        changes.unbounded_send(event.paths).ok();
                    }
                    Err(_) => {
                        changes.unbounded_send(Vec::new()).ok();
                    }
                },
            )
            .and_then(|mut watcher| {
                watcher.watch(dir, notify::RecursiveMode::NonRecursive)?;
                Ok(watcher)
            });
            match watcher {
                Ok(watcher) => Some(watcher),
                Err(error) => {
                    eprintln!(
                        "gitez: changes to the settings appear only after a restart: {error}"
                    );
                    None
                }
            }
        });
        Self {
            _threads: threads,
            _data_dir: data_dir,
        }
    }
}

/// Starts supervisors, in the background.
struct Launcher {
    exe: Result<PathBuf, String>,
    settings: Arc<Settings>,
    secrets: Arc<dyn SecretStore>,
}

impl Launcher {
    /// Reads `thread`'s secrets and starts `gitez supervise <command>` for
    /// it. If that fails, the thread is marked failed, so it does not sit
    /// queued with no supervisor. Blocks.
    fn launch(
        &self,
        store: &Store,
        command: Command,
        key: &AgentThreadKey,
        thread: &AgentThread,
    ) -> anyhow::Result<()> {
        let (values, warnings) = resolve_secrets(
            &self.settings,
            &key.common_dir,
            &thread.secrets,
            self.secrets.as_ref(),
        );
        if !warnings.is_empty() {
            store.update(&key.id, |thread| {
                for warning in warnings {
                    if !thread.warnings.contains(&warning) {
                        thread.warnings.push(warning);
                    }
                }
                Ok(())
            })?;
        }
        let launched = self
            .exe
            .clone()
            .map_err(anyhow::Error::msg)
            .and_then(|exe| {
                if !exe.exists() {
                    bail!(
                        "GitEZ’s program is no longer at {}; restart GitEZ.",
                        exe.display()
                    );
                }
                let pid = gitez_supervisor::launch(Launch {
                    exe: &exe,
                    prefix: &["supervise"],
                    command,
                    common_dir: &key.common_dir,
                    thread: &key.id,
                    secrets: &Secrets::new(values),
                })?;
                debug(|| format!("launched {} {} (pid {pid})", command.as_str(), key.id));
                Ok(())
            });
        if let Err(error) = &launched {
            let message = format!("GitEZ could not start the thread’s supervisor: {error:#}");
            store.update(&key.id, |thread| {
                if thread.state.is_active() {
                    thread.fail(Step::Supervisor, message, Timestamp::now());
                }
                Ok(())
            })?;
        }
        launched
    }
}

/// The values of the secrets `names` for a thread of the repository at
/// `common_dir`, and a warning for each one that could not be given.
fn resolve_secrets(
    settings: &Settings,
    common_dir: &Path,
    names: &[String],
    store: &dyn SecretStore,
) -> (BTreeMap<String, String>, Vec<String>) {
    let mut values = BTreeMap::new();
    let mut warnings = Vec::new();
    if names.is_empty() {
        return (values, warnings);
    }
    let entries = settings.secrets_for(common_dir);
    let mut seen = HashSet::new();
    for name in names {
        if !seen.insert(name) {
            continue;
        }
        let entry = entries.iter().find(|entry| &entry.name == name);
        let read = entry.map(|entry| store.get(&entry.account()));
        match read {
            Some(Ok(Some(value))) => {
                values.insert(name.clone(), value);
            }
            Some(Err(error)) => warnings.push(format!(
                "Secret {name} could not be read from the keychain ({error}); the thread ran \
                 without it."
            )),
            None | Some(Ok(None)) => warnings.push(format!(
                "Secret {name} was deleted; the thread ran without it."
            )),
        }
    }
    (values, warnings)
}

// Completing an agent thread when its local pull request is merged or
// closed (E.11). Checked whenever a pull request of a repository with
// threads is saved and whenever threads are read, so it covers every way a
// pull request is merged or closed: in this window, in another, or while
// GitEZ was not running.

impl AgentThreads {
    /// Completes, in the background, the finished threads among those of
    /// the repositories `repositories` and the threads `threads` whose pull
    /// request is now merged or closed.
    fn complete_merged(
        &mut self,
        repositories: &[PathBuf],
        threads: &[AgentThreadKey],
        cx: &mut Context<Self>,
    ) {
        let mut due = Vec::new();
        for repository in &self.repositories {
            let whole = repositories
                .iter()
                .any(|dir| *dir == repository.common_dir || *dir == repository.canonical);
            for thread in &repository.threads {
                let (State::Finished, Some(number)) = (thread.state, thread.pull_request) else {
                    continue;
                };
                let key = AgentThreadKey {
                    common_dir: repository.common_dir.clone(),
                    id: thread.id.clone(),
                };
                let named = threads.iter().any(|named| {
                    named.id == key.id
                        && (named.common_dir == repository.common_dir
                            || named.common_dir == repository.canonical)
                });
                if (whole || named) && !self.completing.contains(&key) {
                    due.push((key, number, repository.work_dir.clone()));
                }
            }
        }
        if due.is_empty() {
            return;
        }
        self.completing
            .extend(due.iter().map(|(key, _, _)| key.clone()));
        let changes = self.changes.clone();
        let task = cx.background_executor().spawn(async move {
            due.into_iter()
                .map(|(key, number, work_dir)| {
                    match complete_for_pull_request(&work_dir, &key, number, Timestamp::now()) {
                        Ok(true) => {
                            debug(|| format!("completed {}: #{number} merged or closed", key.id));
                            changes.unbounded_send(vec![thread_file(&key)]).ok();
                        }
                        Ok(false) => {}
                        Err(error) => eprintln!(
                            "gitez: could not complete the thread {} of pull request #{number}: \
                             {error:#}",
                            key.id
                        ),
                    }
                    key
                })
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let done = task.await;
            this.update(cx, |this, _| {
                for key in &done {
                    this.completing.remove(key);
                }
            })
            .ok();
        })
        .detach();
    }
}

/// Whether `thread` completes when the pull request it created is in
/// `state`: a finished thread does once the pull request is merged or
/// closed. One still working completes when it finishes (it is checked
/// again then); a completed one stays as it is, even when its pull request
/// is reopened.
fn completes_with(thread: &AgentThread, state: gitez_review::State) -> bool {
    thread.state == State::Finished
        && thread.pull_request.is_some()
        && matches!(
            state,
            gitez_review::State::Merged | gitez_review::State::Closed
        )
}

/// Completes the thread `key` if pull request `number`, which it created,
/// is merged or closed: removes its worktree (and its branch when it has no
/// commits of its own) and marks it completed. A worktree folder already
/// gone, or one git cannot remove, does not keep the thread from
/// completing; the thread says what happened. Returns whether it
/// completed. Blocks.
fn complete_for_pull_request(
    work_dir: &Path,
    key: &AgentThreadKey,
    number: u32,
    now: Timestamp,
) -> anyhow::Result<bool> {
    let pull_request = match gitez_review::Store::new(&key.common_dir).load(number) {
        Ok(pull_request) => pull_request,
        // A pull request whose file is gone or unreadable is left alone,
        // like any other (F).
        Err(error) => {
            debug(|| format!("pull request #{number} of {}: {error}", key.id));
            return Ok(false);
        }
    };
    let store = Store::new(&key.common_dir);
    let thread = store.load(&key.id)?;
    if !completes_with(&thread, pull_request.state) {
        return Ok(false);
    }
    let how = if pull_request.state == gitez_review::State::Merged {
        "merged"
    } else {
        "closed"
    };
    // Automatic completion after a merge or close keeps a worktree that has
    // uncommitted changes (or that GitEZ no longer trusts) rather than
    // destroying work: the user removes it later with an explicit Complete.
    let removed = gitez_agent_thread::complete(work_dir, &thread, IfUncommitted::Keep);
    let warning = match &removed {
        Ok(report) => match &report.kept {
            Some(Kept::Uncommitted { .. }) => Some(format!(
                "Pull request #{number} was {how}; GitEZ completed the thread but kept its \
                 worktree at {} because it has uncommitted changes. Remove it when you have \
                 saved what you need.",
                thread.worktree.display()
            )),
            Some(Kept::Untrusted(problem)) => Some(format!(
                "Pull request #{number} was {how}; GitEZ completed the thread but kept its \
                 worktree at {}: {problem} Remove it yourself.",
                thread.worktree.display()
            )),
            None if !report.worktree_removed => Some(format!(
                "The thread’s worktree folder was already gone when pull request #{number} was \
                 {how}."
            )),
            None => None,
        },
        Err(error) => Some(format!(
            "Pull request #{number} was {how}, but GitEZ could not remove the thread’s worktree \
             at {}: {}. Remove the folder yourself.",
            thread.worktree.display(),
            anyhow::Error::from_ref(error)
        )),
    };
    store
        .update(&key.id, |thread| {
            // Continued meanwhile: it completes when it finishes again.
            if thread.state != State::Finished {
                return Ok(false);
            }
            if let Some(warning) = warning
                && !thread.warnings.contains(&warning)
            {
                thread.warnings.push(warning);
            }
            thread.mark_completed(now)?;
            Ok(true)
        })
        .map_err(anyhow::Error::from)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    use gitez_agent_thread::{
        AgentThread, AgentThreadId, Ending, Failure, Start, State, Timestamp, Turn, TurnKind,
        TurnResult,
    };
    use gitez_sandbox::NetworkProfile;
    use gitez_secret::SecretStore as _;
    use gitez_settings::{SecretEntry, SecretScope, Settings};

    use super::{
        AgentThreadKey, Counts, Plan, complete_for_pull_request, completes_with, resolve_secrets,
    };

    /// An agent thread in `state` that ended with `ending`.
    fn thread(id: &str, state: State, ending: Option<Ending>) -> AgentThread {
        let at: Timestamp = "2026-11-13T09:00:00Z".parse().expect("a time");
        AgentThread {
            id: AgentThreadId::new(id).expect("an id"),
            title: id.to_owned(),
            agent: "claude-code".to_owned(),
            model: None,
            state,
            ending,
            created: at,
            updated: at,
            start: Start {
                branch: "main".to_owned(),
                commit: "0".repeat(40),
            },
            branch: format!("agent/{id}"),
            worktree: PathBuf::from("/nowhere").join(id),
            prepared: true,
            network: NetworkProfile::AgentOnly,
            secrets: Vec::new(),
            session_id: None,
            pull_request: None,
            turns: vec![Turn {
                kind: TurnKind::Start,
                prompt: "Do it".to_owned(),
                requested: at,
                resume: false,
                started: Some(at),
                ended: None,
                transcript: None,
                result: None::<TurnResult>,
            }],
            activity: None,
            waiting_until: None,
            error: None::<Failure>,
            warnings: Vec::new(),
            supervisor: None,
            completed: None,
            worktree_record: None,
            uncommitted: None,
        }
    }

    #[test]
    fn counts_group_threads_by_what_they_need() {
        let threads = [
            thread("a", State::Running, None),
            thread("b", State::Preparing, None),
            thread("c", State::Queued, None),
            thread("d", State::Waiting, None),
            thread("e", State::Finished, Some(Ending::PullRequest)),
            thread("f", State::Finished, Some(Ending::Answer)),
            thread("g", State::Finished, Some(Ending::Failed)),
            thread("h", State::Finished, Some(Ending::Stopped)),
            thread("i", State::Completed, Some(Ending::PullRequest)),
        ];
        assert_eq!(
            Counts::of(&threads),
            Counts {
                active: 2,
                waiting: 2,
                ready: 2,
                failed: 2,
                completed: 1,
            }
        );
        assert_eq!(Counts::of([]), Counts::default());
    }

    #[test]
    fn secrets_are_read_by_scope_and_missing_ones_are_named() {
        let common_dir = Path::new("/repo/.git");
        let settings = Settings {
            secrets: vec![
                SecretEntry {
                    name: "TOKEN".into(),
                    scope: SecretScope::All,
                    default: true,
                },
                SecretEntry {
                    name: "TOKEN".into(),
                    scope: SecretScope::Repository(common_dir.to_owned()),
                    default: false,
                },
                SecretEntry {
                    name: "EMPTY".into(),
                    scope: SecretScope::All,
                    default: false,
                },
            ],
            ..Settings::default()
        };
        let store = gitez_secret::Memory::default();
        store
            .set(
                &format!("repo:{}/TOKEN", common_dir.display()),
                "repo-value",
            )
            .expect("set");
        store.set("all/TOKEN", "all-value").expect("set");
        let names = ["TOKEN", "EMPTY", "GONE", "TOKEN"].map(str::to_owned);
        let (values, warnings) = resolve_secrets(&settings, common_dir, &names, &store);
        assert_eq!(
            values,
            BTreeMap::from([("TOKEN".to_owned(), "repo-value".to_owned())])
        );
        assert_eq!(
            warnings,
            [
                "Secret EMPTY was deleted; the thread ran without it.",
                "Secret GONE was deleted; the thread ran without it.",
            ]
        );
    }

    #[test]
    fn file_events_are_classified() {
        let mut plan = Plan::default();
        let common = Path::new("/r/.git");
        for rest in [
            "threads/fix-it/thread.json",
            "threads/fix-it/runs/2026-11-13T09-41-31Z.jsonl",
            "threads/fix-it/supervisor.log",
            "threads/other",
            "threads/Not An Id/thread.json",
            "reviews/3/review.json",
        ] {
            plan.add(common, Path::new(rest));
        }
        let fix_it = AgentThreadId::new("fix-it").expect("id");
        let other = AgentThreadId::new("other").expect("id");
        assert_eq!(
            plan.threads.get(common),
            Some(&BTreeSet::from([fix_it.clone(), other]))
        );
        assert_eq!(
            plan.transcripts,
            [AgentThreadKey {
                common_dir: common.to_owned(),
                id: fix_it
            }]
        );
        assert!(plan.repositories.is_empty() && !plan.index && !plan.settings);
        assert_eq!(plan.reviews, BTreeSet::from([common.to_owned()]));
        plan.add(common, Path::new("threads"));
        assert!(plan.repositories.contains(common));
        // The review panel's own agent runs are not the pull request.
        let mut runs = Plan::default();
        runs.add(
            common,
            Path::new("reviews/3/runs/2026-11-13T09-41-31Z.jsonl"),
        );
        runs.add(common, Path::new("reviews/3/review.json.lock"));
        assert!(runs.is_empty());
    }

    /// E.11: a finished thread completes once its pull request is merged
    /// or closed; one working, completed, or with its pull request open
    /// does not.
    #[test]
    fn a_finished_thread_completes_when_its_pull_request_is_merged_or_closed() {
        use gitez_review::State as PullRequest;
        let mut finished = thread("a", State::Finished, Some(Ending::PullRequest));
        finished.pull_request = Some(3);
        assert!(completes_with(&finished, PullRequest::Merged));
        assert!(completes_with(&finished, PullRequest::Closed));
        assert!(!completes_with(&finished, PullRequest::Open));
        // A failed review turn leaves it finished too.
        let mut failed = finished.clone();
        failed.ending = Some(Ending::Failed);
        assert!(completes_with(&failed, PullRequest::Merged));
        for state in [
            State::Queued,
            State::Preparing,
            State::Running,
            State::Waiting,
            State::Completed,
        ] {
            let mut other = finished.clone();
            other.state = state;
            assert!(!completes_with(&other, PullRequest::Merged), "{state:?}");
            assert!(!completes_with(&other, PullRequest::Closed), "{state:?}");
        }
        let no_pull_request = thread("b", State::Finished, Some(Ending::Answer));
        assert!(!completes_with(&no_pull_request, PullRequest::Merged));
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git runs");
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout)
            .expect("utf-8")
            .trim()
            .to_owned()
    }

    /// A repository with a finished thread whose branch, checked out in its
    /// worktree, has a commit, and the open pull request it created.
    fn finished_thread(dir: &Path) -> (PathBuf, AgentThreadKey, u32) {
        let main = dir.join("repo");
        std::fs::create_dir_all(&main).expect("folder");
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["config", "user.name", "Ada Lovelace"]);
        git(&main, &["config", "user.email", "ada@example.com"]);
        git(&main, &["config", "commit.gpgsign", "false"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "First"]);
        let start = git(&main, &["rev-parse", "HEAD"]);
        let common_dir = gitez_git::Repository::open(&main)
            .expect("open")
            .common_dir();
        let store = gitez_agent_thread::Store::new(&common_dir);
        let created = store
            .create(
                gitez_agent_thread::NewAgentThread {
                    prompt: "Add b.txt".into(),
                    agent: "claude-code".into(),
                    model: None,
                    start: Start {
                        branch: "main".into(),
                        commit: start.clone(),
                    },
                    network: NetworkProfile::AgentOnly,
                    secrets: Vec::new(),
                },
                &dir.join("data"),
                "repo",
                |_| false,
                Timestamp::now(),
            )
            .expect("thread");
        gitez_agent_thread::create_worktree(&main, &created.worktree, &created.branch, &start)
            .expect("worktree");
        std::fs::write(created.worktree.join("b.txt"), "b\n").expect("write");
        git(&created.worktree, &["add", "."]);
        git(&created.worktree, &["commit", "-qm", "Add b.txt"]);
        let head = git(&created.worktree, &["rev-parse", "HEAD"]);
        let pull_request = gitez_review::Store::new(&common_dir)
            .create(
                gitez_review::NewPullRequest {
                    title: created.title.clone(),
                    description: String::new(),
                    base: gitez_review::Tip {
                        branch: "main".into(),
                        commit: start,
                    },
                    head: gitez_review::Tip {
                        branch: created.branch.clone(),
                        commit: head,
                    },
                },
                Timestamp::now(),
            )
            .expect("pull request");
        store
            .update(&created.id, |thread| {
                thread.state = State::Finished;
                thread.ending = Some(Ending::PullRequest);
                thread.session_id = Some("session".into());
                thread.pull_request = Some(pull_request.number);
                Ok(())
            })
            .expect("finish");
        let key = AgentThreadKey {
            common_dir,
            id: created.id,
        };
        (main, key, pull_request.number)
    }

    fn set_state(key: &AgentThreadKey, number: u32, state: gitez_review::State) {
        let store = gitez_review::Store::new(&key.common_dir);
        let mut pull_request = store.load(number).expect("load");
        pull_request.state = state;
        store.save(&pull_request).expect("save");
    }

    #[test]
    fn merging_its_pull_request_removes_the_worktree_and_completes_the_thread() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (main, key, number) = finished_thread(dir.path());
        let store = gitez_agent_thread::Store::new(&key.common_dir);
        let worktree = store.load(&key.id).expect("load").worktree;
        let now = Timestamp::now();
        // Open: nothing happens.
        assert!(!complete_for_pull_request(&main, &key, number, now).expect("checked"));
        assert!(worktree.exists());

        set_state(&key, number, gitez_review::State::Merged);
        assert!(complete_for_pull_request(&main, &key, number, now).expect("completed"));
        let thread = store.load(&key.id).expect("load");
        assert_eq!(thread.state, State::Completed);
        assert_eq!(thread.completed, Some(now));
        assert!(thread.warnings.is_empty(), "{:?}", thread.warnings);
        assert!(!worktree.exists());
        // The branch has a commit of its own, so it stays; git forgets the
        // worktree.
        git(&main, &["rev-parse", "--verify", &thread.branch]);
        assert_eq!(git(&main, &["worktree", "list"]).lines().count(), 1);

        // Reopened: the completed thread stays as it is.
        set_state(&key, number, gitez_review::State::Open);
        assert!(!complete_for_pull_request(&main, &key, number, now).expect("checked"));
        set_state(&key, number, gitez_review::State::Closed);
        assert!(!complete_for_pull_request(&main, &key, number, now).expect("checked"));
        assert_eq!(store.load(&key.id).expect("load"), thread);
    }

    #[test]
    fn a_thread_whose_worktree_is_gone_completes_and_says_so() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (main, key, number) = finished_thread(dir.path());
        let store = gitez_agent_thread::Store::new(&key.common_dir);
        std::fs::remove_dir_all(store.load(&key.id).expect("load").worktree)
            .expect("deleted by hand");
        set_state(&key, number, gitez_review::State::Closed);
        assert!(complete_for_pull_request(&main, &key, number, Timestamp::now()).expect("done"));
        let thread = store.load(&key.id).expect("load");
        assert_eq!(thread.state, State::Completed);
        assert_eq!(
            thread.warnings,
            [format!(
                "The thread’s worktree folder was already gone when pull request #{number} was \
                 closed."
            )]
        );
        assert_eq!(git(&main, &["worktree", "list"]).lines().count(), 1);
    }

    #[test]
    fn a_pull_request_whose_file_is_gone_leaves_its_thread_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (main, key, number) = finished_thread(dir.path());
        std::fs::remove_dir_all(key.common_dir.join("gitez/reviews")).expect("remove");
        assert!(!complete_for_pull_request(&main, &key, number, Timestamp::now()).expect("ok"));
        let thread = gitez_agent_thread::Store::new(&key.common_dir)
            .load(&key.id)
            .expect("load");
        assert_eq!(thread.state, State::Finished);
    }
}
