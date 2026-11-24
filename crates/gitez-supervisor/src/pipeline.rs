//! The supervisor's work for one agent thread (DESIGN D.5): check, queue,
//! worktree, prepare, run the agent, check refs, commit leftovers, pull
//! request, final state.
//!
//! A Discuss thread has no worktree of its own: the agent runs in the
//! user's working tree, which the sandbox lets it only read. Nothing is
//! prepared, committed or turned into a pull request for it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gitez_agent::{
    Agent, AgentConfig, AgentEvent, AgentNeeds, AgentProcess, CommandSpec, Conversation, Finished,
    ImageInput, RunSpec, Stopper,
};
use gitez_agent_thread::{
    Activity, AgentThread, AgentThreadId, BRIEF_PROMPT, CONTINUE_PROMPT, Ending, HANDOFF_MARKER,
    Handoff, Mode, PrepareStep, RunHeader, ScratchDirs, State, Step, Store, SupervisorInfo,
    Timestamp, Turn, TurnKind, TurnResult, Uncommitted, split_handoff, title_from_message,
};
use gitez_git::{RefState, Repository};
use gitez_sandbox::{Network, NetworkProfile, Sandbox as _, Status};
use gitez_settings::{Loaded, Settings};
use jiff::SignedDuration;

use crate::liveness::SupervisorLock;
use crate::policy::{self, Place, Work};
use crate::queue::{Queue, Slot, Waited};
use crate::toolchains;
use crate::{Command, Error, Result, Secrets};

/// How long an interrupted agent may take to end its turn before it is
/// killed.
const STOP_GRACE: Duration = Duration::from_secs(20);
/// How long GitEZ's own commit of the agent's leftovers may take (hooks
/// included).
const COMMIT_TIMEOUT: Duration = Duration::from_secs(300);
/// How often the activity is written while the agent works.
const ACTIVITY_EVERY: Duration = Duration::from_secs(1);
/// How many polls to wait after the usage limit's reset time before
/// continuing: 60 s in production (`poll` = 1 s).
const RESET_MARGIN_POLLS: u32 = 60;
/// Lines of output kept to explain a failure.
const TAIL_LINES: usize = 20;
/// How many of the worktree's uncommitted paths are recorded (for the UI).
const UNCOMMITTED_PATHS: usize = 10;
const SETUP_LOG: &str = "setup.log";
const SETUP_COMMAND_KEY: &str = "gitez.setupCommand";
const PASSTHROUGH_WARNING: &str = "Ran without a sandbox (GITEZ_TEST_SANDBOX)";
const USAGE_LIMIT: &str = "The agent's usage limit was reached. Retry when it has reset.";
/// The body of GitEZ's commit of the agent's leftovers, which marks it.
const LEFTOVERS_NOTE: &str = "Changes the agent left uncommitted, committed by GitEZ.";

pub struct Environment {
    pub data_dir: std::path::PathBuf,
    /// Re-read from settings.json at each queue poll (for max_running).
    pub settings_path: std::path::PathBuf,
    pub agents: gitez_agent::Registry,
    pub sandbox: std::sync::Arc<dyn gitez_sandbox::Sandbox>,
    /// Set by SIGTERM; tests set it themselves.
    pub stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Queue and wait polling interval; 1 s in production, 20 ms in tests.
    /// After a usage limit the supervisor also waits 60 polls past the
    /// reset time.
    pub poll: std::time::Duration,
}

/// Runs the pipeline of D.5 for thread `id` of the repository at
/// `common_dir`, holding the supervisor lock throughout. Returns the
/// thread's final state. Errors only when the thread cannot be read or
/// locked ([`Error::AlreadyRunning`] when another supervisor holds it), or
/// when `command` does not match what the thread waits for
/// ([`Error::Usage`]; an active thread is then marked failed, step
/// Supervisor); every later failure is recorded in thread.json and
/// returns Ok.
pub fn run(
    env: &Environment,
    common_dir: &std::path::Path,
    id: &gitez_agent_thread::AgentThreadId,
    command: Command,
    secrets: Secrets,
) -> Result<gitez_agent_thread::AgentThread> {
    let store = Store::new(common_dir);
    // Read first, so a missing thread is not given a lock file.
    store.load(id)?;
    let _lock = SupervisorLock::take(&store.thread_dir(id))?;
    let thread = store.load(id)?;
    let mut pipeline = Pipeline {
        env,
        common_dir: common_dir.to_owned(),
        store,
        id: id.clone(),
        secrets,
        thread,
        main_dir: None,
        place: None,
        worktree_ready: false,
        last_activity: None,
    };
    pipeline.save(|thread| {
        thread.supervisor = Some(SupervisorInfo {
            pid: std::process::id(),
            child_group: None,
            started: Timestamp::now(),
        });
    });
    if let Err(message) = check_command(&pipeline.thread, command) {
        log(id, &message);
        pipeline.try_save(|thread| {
            if thread.state.is_active() {
                thread.fail(Step::Supervisor, message.clone(), Timestamp::now());
            } else {
                thread.supervisor = None;
            }
        })?;
        return Err(Error::Usage(message));
    }
    let end = pipeline.pipeline();
    pipeline.finish(end)
}

/// How the pipeline ended before its post-run steps.
#[derive(Debug)]
enum End {
    /// The stop flag was set.
    Stopped,
    Failed(Step, String),
    /// The run ended on the usage limit (handled inside the pipeline).
    UsageLimit(Option<i64>),
}

/// What step 1 resolved.
struct Context {
    settings: Settings,
    agent: Option<Arc<dyn Agent>>,
    config: AgentConfig,
    /// The agent entry's default model, when the thread has none.
    model: Option<String>,
    /// The agent entry's default effort, when the thread has none.
    effort: Option<String>,
}

struct Pipeline<'a> {
    env: &'a Environment,
    common_dir: PathBuf,
    store: Store,
    id: AgentThreadId,
    secrets: Secrets,
    /// The thread as last saved.
    thread: AgentThread,
    /// The main worktree, once read.
    main_dir: Option<PathBuf>,
    place: Option<Place>,
    /// Whether step 3 checked or created the worktree.
    worktree_ready: bool,
    last_activity: Option<Instant>,
}

impl Pipeline<'_> {
    /// Steps 1 to 5 (5b loops back to 2). Ok: the agent's turn, if any,
    /// succeeded.
    fn pipeline(&mut self) -> Result<(), End> {
        let context = self.context()?;
        loop {
            let slot = self.queue()?;
            self.worktree()?;
            self.stopped()?;
            self.prepare(&context)?;
            self.stopped()?;
            let ran = self.run_turn(&context);
            drop(slot);
            match ran {
                Err(End::UsageLimit(Some(resets_at))) => self.wait_for_reset(resets_at)?,
                Err(End::UsageLimit(None)) => {
                    return Err(End::Failed(Step::Agent, USAGE_LIMIT.to_owned()));
                }
                other => return other,
            }
        }
    }

    /// Steps 6 to 9 after `end`, then the final state.
    fn finish(mut self, end: Result<(), End>) -> Result<AgentThread> {
        let mut end = end;
        if self.worktree_ready && self.thread.prepared {
            self.commit_leftovers();
        }
        if end.is_ok()
            && let Err(failed) = self.pull_request()
        {
            end = Err(failed);
        }
        let now = Timestamp::now();
        let thread = self.store.update(&self.id, |thread| {
            match &end {
                Ok(()) => {
                    thread.state = State::Finished;
                    thread.ending = Some(if thread.pull_request.is_some() {
                        Ending::PullRequest
                    } else {
                        Ending::Answer
                    });
                }
                Err(End::Stopped) => {
                    thread.state = State::Finished;
                    thread.ending = Some(Ending::Stopped);
                    // A stop is not an error, but a turn cut short ends.
                    for turn in &mut thread.turns {
                        if turn.started.is_some() && turn.ended.is_none() {
                            turn.ended = Some(now);
                        }
                    }
                }
                Err(End::Failed(step, message)) => thread.fail(*step, message.clone(), now),
                Err(End::UsageLimit(_)) => {
                    thread.fail(Step::Agent, USAGE_LIMIT, now);
                }
            }
            thread.activity = None;
            thread.supervisor = None;
            thread.waiting_until = None;
            Ok(thread.clone())
        })?;
        log(
            &self.id,
            &format!(
                "finished: {}",
                thread
                    .ending
                    .map(|ending| name_of(&ending))
                    .unwrap_or_default()
            ),
        );
        Ok(thread)
    }

    /// Step 1: the agent, its configuration and the sandbox.
    fn context(&mut self) -> Result<Context, End> {
        let settings = match Settings::load(&self.env.settings_path) {
            Ok(Loaded::Current(settings)) => settings,
            Ok(Loaded::TooNew) => Settings::default(),
            Err(error) => {
                log(&self.id, &format!("using default settings: {error}"));
                Settings::default()
            }
        };
        let entry = settings.agent(&self.thread.agent);
        let config = entry.config();
        let agent = if self.thread.pending_turn().is_some() {
            let agent = self.env.agents.get(&self.thread.agent).ok_or_else(|| {
                End::Failed(
                    Step::Agent,
                    format!("GitEZ does not know the agent “{}”.", self.thread.agent),
                )
            })?;
            agent
                .locate(&config)
                .map_err(|error| End::Failed(Step::Agent, error.to_string()))?;
            Some(agent)
        } else {
            None
        };
        if let Status::Missing { fix } = self.env.sandbox.status() {
            return Err(End::Failed(Step::Sandbox, fix));
        }
        if self.env.sandbox.name() == gitez_sandbox::Passthrough.name() {
            self.save(|thread| {
                if !thread.warnings.iter().any(|w| w == PASSTHROUGH_WARNING) {
                    thread.warnings.push(PASSTHROUGH_WARNING.to_owned());
                }
            });
        }
        Ok(Context {
            settings,
            agent,
            config,
            model: entry.model.filter(|model| !model.is_empty()),
            effort: entry.effort.filter(|effort| !effort.is_empty()),
        })
    }

    /// Step 2: wait for a running-agent slot.
    fn queue(&mut self) -> Result<Slot, End> {
        self.save(|thread| {
            thread.state = State::Queued;
            thread.activity = None;
        });
        let queue = Queue::new(&self.env.data_dir, &self.env.settings_path);
        let name = format!("{}-{}", self.repository_key(), self.id);
        match queue.wait(&name, self.env.poll, &self.env.stop) {
            Ok(Waited::Slot(slot)) => {
                log(&self.id, &format!("took running slot {}", slot.number));
                Ok(slot)
            }
            Ok(Waited::Stopped) => Err(End::Stopped),
            Err(error) => Err(End::Failed(
                Step::Queue,
                format!("GitEZ could not use its queue: {error}"),
            )),
        }
    }

    /// Step 3: create the worktree, or check it is the thread's: its git
    /// files must be as GitEZ recorded them (see
    /// [`gitez_agent_thread::check_worktree`]) before anything runs there.
    /// A branch with commits beyond the start is checked out inside the
    /// sandbox, never here.
    ///
    /// For a Discuss thread: check the working tree it reads
    /// ([`Self::working_tree`]).
    fn worktree(&mut self) -> Result<(), End> {
        let failed = |message: String| End::Failed(Step::Worktree, message);
        if self.thread.mode == Mode::Discuss {
            return self.working_tree();
        }
        let Some((branch, worktree)) = self
            .thread
            .own()
            .map(|(branch, worktree)| (branch.to_owned(), worktree.to_owned()))
        else {
            return Err(failed(
                "The thread has no branch and no worktree.".to_owned(),
            ));
        };
        let main_dir = self.main_dir().map_err(failed)?;
        let exists = std::fs::symlink_metadata(&worktree).is_ok();
        if !exists || !self.thread.prepared {
            self.set_preparing("creating worktree");
        }
        // Git runs here, outside the sandbox and in a process group of its
        // own, with the user's hooks (`post-checkout`): a stop ends it.
        let operation = gitez_command::Operation::new();
        let cancel = CancelOnStop::start(operation.clone(), self.env.stop.clone());
        let git = gitez_command::Git::new(&main_dir).with_operation(&operation);
        if exists {
            gitez_agent_thread::check_worktree(
                &self.common_dir,
                &worktree,
                self.thread.worktree_record.as_ref(),
            )
            .map_err(|error| failed(error.to_string()))?;
        } else {
            // A folder deleted by hand is still registered with git.
            git.worktree_prune().ok();
        }
        let created = gitez_agent_thread::create_worktree_with(
            &git,
            &worktree,
            &branch,
            &self.thread.start.commit,
        );
        cancel.finish();
        let created = created.map_err(|error| match error {
            gitez_agent_thread::Error::Git(error) if error.is_cancelled() => End::Stopped,
            error => failed(error.to_string()),
        })?;
        if !exists || self.thread.worktree_record.is_none() {
            let record = created.record.clone();
            self.save(|thread| {
                thread.worktree_record = Some(record);
                // A new folder needs preparing again.
                if !exists {
                    thread.prepared = false;
                }
            });
        }
        let scratch =
            gitez_agent_thread::scratch_dirs(&self.env.data_dir, &self.repository_key(), &self.id);
        self.own_scratch(&scratch)?;
        self.place = Some(Place {
            common_dir: self.common_dir.clone(),
            work: Work::Own {
                worktree,
                worktree_git_dir: created.git_dir,
                branch,
            },
            tmp_dir: scratch.tmp,
            caches_dir: scratch.caches,
            data_dir: self.env.data_dir.clone(),
        });
        self.worktree_ready = true;
        if created.needs_checkout {
            self.checkout()?;
        }
        Ok(())
    }

    /// Step 3 for a Discuss thread: nothing is made. The working tree the
    /// thread reads must still be one of this repository; the thread's
    /// scratch folders are created. No git runs here.
    fn working_tree(&mut self) -> Result<(), End> {
        let failed = |message: String| End::Failed(Step::Worktree, message);
        let Some(work_dir) = self.thread.work_dir.clone() else {
            return Err(failed(
                "The thread does not say which working tree to read.".to_owned(),
            ));
        };
        let gone = || {
            failed(format!(
                "The working tree {} is gone, or is no longer one of this repository.",
                work_dir.display()
            ))
        };
        let repository = Repository::open(&work_dir).map_err(|_| gone())?;
        let same = |a: &Path, b: &Path| policy::real(a) == policy::real(b);
        if !same(&repository.common_dir(), &self.common_dir)
            || !repository
                .work_dir()
                .is_some_and(|found| same(found, &work_dir))
        {
            return Err(gone());
        }
        let Some(scratch) = self.thread.scratch_dirs() else {
            return Err(failed(
                "The thread does not say where its temporary folder is.".to_owned(),
            ));
        };
        self.own_scratch(&scratch)?;
        self.place = Some(Place {
            common_dir: self.common_dir.clone(),
            work: Work::ReadOnly { work_dir },
            tmp_dir: scratch.tmp,
            caches_dir: scratch.caches,
            data_dir: self.env.data_dir.clone(),
        });
        Ok(())
    }

    /// Makes the thread's scratch folders, real folders of its own.
    fn own_scratch(&mut self, scratch: &ScratchDirs) -> Result<(), End> {
        for dir in [&scratch.tmp, &scratch.caches] {
            match own_dir(dir) {
                Ok(false) => {}
                Ok(true) => self.warn(format!(
                    "{} was not a folder (perhaps a link the agent made); GitEZ replaced it with \
                     an empty one.",
                    dir.display()
                )),
                Err(error) => {
                    return Err(End::Failed(
                        Step::Worktree,
                        format!("GitEZ could not create {}: {error}", dir.display()),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Checks out the branch in a worktree made without its files, inside
    /// the sandbox without network: the files may be the agent's, and a
    /// checkout runs filter drivers and can run hooks.
    fn checkout(&mut self) -> Result<(), End> {
        self.set_preparing("checking out the branch");
        let command = self.command(
            "/bin/sh",
            vec![
                "-c".to_owned(),
                "exec 2>&1\ngit reset --hard --quiet".to_owned(),
            ],
            None,
        );
        let settings = self.settings();
        let mut output = VecDeque::new();
        let ran = self.run_command(
            Kind::Setup,
            command,
            &AgentNeeds::default(),
            &settings,
            None,
            |_, line| push_tail(&mut output, line.to_owned()),
        )?;
        if self.env.stop.load(Ordering::SeqCst) {
            return Err(End::Stopped);
        }
        if !ran.outcome.success {
            let lines: Vec<String> = output.into();
            return Err(End::Failed(
                Step::Worktree,
                format!(
                    "GitEZ could not check out {} in the worktree:\n{}",
                    self.thread.branch.as_deref().unwrap_or_default(),
                    lines.join("\n")
                ),
            ));
        }
        Ok(())
    }

    /// The settings, or the defaults when they cannot be read.
    fn settings(&self) -> Settings {
        match Settings::load(&self.env.settings_path) {
            Ok(Loaded::Current(settings)) => settings,
            _ => Settings::default(),
        }
    }

    /// Step 4: copy includes, clone caches, run the setup command. Not
    /// for a Discuss thread, whose working tree is the user's as it is.
    fn prepare(&mut self, context: &Context) -> Result<(), End> {
        if self.thread.prepared {
            return Ok(());
        }
        let Some(worktree) = self.thread.worktree.clone() else {
            return Ok(());
        };
        let failed = |message: String| End::Failed(Step::Prepare, message);
        let started = Instant::now();
        self.set_preparing("copying files");
        let main_dir = self.main_dir().map_err(failed)?;
        let main = Repository::open(&main_dir).map_err(|error| failed(error.to_string()))?;
        let report = gitez_agent_thread::prepare(&main, &worktree, |step| match step {
            PrepareStep::CopyingIncludes { .. } => self.set_preparing("copying files"),
            PrepareStep::Cloning { dir } => self.set_preparing(&format!("cloning {dir}/")),
        })
        .map_err(|error| failed(error.to_string()))?;
        log(
            &self.id,
            &format!(
                "prepared the worktree in {} ms: copied {}, cloned {:?}, not cloned {:?}",
                started.elapsed().as_millis(),
                report.copied.len(),
                report.cloned,
                report.not_cloned
            ),
        );
        if let Some(command) = main.config_string(SETUP_COMMAND_KEY) {
            let started = Instant::now();
            self.setup(context, &command)?;
            log(
                &self.id,
                &format!(
                    "ran the setup command in {} ms",
                    started.elapsed().as_millis()
                ),
            );
        }
        self.save(|thread| thread.prepared = true);
        Ok(())
    }

    /// The setup command, `sh -c <command>` in the sandbox with the
    /// Packages network, its output in setup.log.
    fn setup(&mut self, context: &Context, setup: &str) -> Result<(), End> {
        let failed = |message: String| End::Failed(Step::Prepare, message);
        self.set_preparing("running the setup command");
        let needs = context
            .agent
            .as_ref()
            .map(|agent| agent.needs(&context.config))
            .unwrap_or_default();
        let command = self.command(
            "/bin/sh",
            // Its errors go to the log too, in order with its output.
            vec!["-c".to_owned(), format!("exec 2>&1\n{setup}")],
            None,
        );
        let log_path = self.store.thread_dir(&self.id).join(SETUP_LOG);
        let mut log_file = File::create(&log_path).map_err(|error| {
            failed(format!(
                "GitEZ could not write {}: {error}",
                log_path.display()
            ))
        })?;
        let ran = self.run_command(
            Kind::Setup,
            command,
            &needs,
            &context.settings,
            Some(NetworkProfile::Packages),
            |_, line| {
                writeln!(log_file, "{line}").ok();
            },
        )?;
        if self.env.stop.load(Ordering::SeqCst) {
            return Err(End::Stopped);
        }
        if !ran.outcome.success {
            let code = ran
                .outcome
                .code
                .map_or_else(|| "killed".to_owned(), |code| code.to_string());
            let mut tail: Vec<String> = ran.tail.into();
            if !ran.outcome.stderr.is_empty() {
                tail.push(self.secrets.redact(&ran.outcome.stderr));
            }
            return Err(failed(format!(
                "The setup command failed (exit {code}):\n{}",
                tail.join("\n")
            )));
        }
        Ok(())
    }

    /// Step 5 for the pending turn, and step 6 after it. Ok when there is
    /// no turn (a retry that only finishes) or the agent succeeded.
    fn run_turn(&mut self, context: &Context) -> Result<(), End> {
        let Some(index) = self.thread.pending_turn() else {
            return Ok(());
        };
        let Some(agent) = context.agent.clone() else {
            return Ok(());
        };
        let failed = |message: String| End::Failed(Step::Agent, message);
        let turn = self.thread.turns[index].clone();
        let main_dir = self.main_dir().map_err(failed)?;
        let refs_before = ref_state(&main_dir);

        let session_id = if turn.resume {
            self.thread.session_id.clone().ok_or_else(|| {
                failed(
                    "There is no conversation to continue: the agent never started. \
                     Retry to start it again."
                        .to_owned(),
                )
            })?
        } else {
            uuid::Uuid::new_v4().to_string()
        };
        let conversation = if turn.resume {
            Conversation::Resume {
                session_id: &session_id,
            }
        } else {
            Conversation::New {
                session_id: &session_id,
            }
        };
        let Some(place) = self.place.clone() else {
            return Err(End::Failed(
                Step::Worktree,
                "The worktree is not ready.".to_owned(),
            ));
        };
        let mut system_prompt = match &place.work {
            Work::Own { branch, .. } => {
                system_prompt(branch, &self.thread.start.branch, self.thread.network)
            }
            Work::ReadOnly { .. } => discuss_system_prompt(
                self.thread.network,
                self.thread.auto,
                &self.thread.start.branch,
            ),
        };
        system_prompt.push_str(local_ports_note(
            self.thread.network,
            self.thread.local_ports,
        ));
        let model = self
            .thread
            .model
            .clone()
            .filter(|model| !model.is_empty())
            .or_else(|| context.model.clone());
        let effort = self
            .thread
            .effort
            .clone()
            .filter(|effort| !effort.is_empty())
            .or_else(|| context.effort.clone());
        // The images go with the prompt, read from the thread's folder.
        if !turn.images.is_empty() && !agent.accepts_images(model.as_deref()) {
            return Err(failed(format!(
                "{} cannot be given images with this model. Reply without them, or pick \
                 another model.",
                agent.name()
            )));
        }
        let image_bytes = turn
            .images
            .iter()
            .map(|image| {
                self.store.read_image(&self.id, image).map_err(|error| {
                    failed(format!(
                        "GitEZ could not read the image {}: {error}",
                        image.name
                    ))
                })
            })
            .collect::<Result<Vec<Vec<u8>>, End>>()?;
        let images: Vec<ImageInput<'_>> = turn
            .images
            .iter()
            .zip(&image_bytes)
            .map(|(image, data)| ImageInput {
                media_type: image.media_type.media_type(),
                data,
            })
            .collect();
        let spec = RunSpec {
            prompt: &turn.prompt,
            images: &images,
            cwd: place.dir(),
            // The user's own sessions of the agent run in their working
            // tree too.
            shared_folder: place.read_only(),
            model: model.as_deref(),
            effort: effort.as_deref(),
            conversation,
            system_prompt: Some(&system_prompt),
        };
        let mut command = agent
            .command(&context.config, &spec)
            .map_err(|error| failed(error.to_string()))?;
        let needs = agent.needs_for_run(&context.config, &spec);
        for dir in &needs.create_dirs {
            if let Err(error) = std::fs::create_dir_all(dir) {
                log(
                    &self.id,
                    &format!("could not create {}: {error}", dir.display()),
                );
            }
        }
        self.add_env(&mut command);
        if place.read_only() {
            // git would refresh the index of the tree it only reads, and
            // report that it cannot.
            command
                .env
                .push(("GIT_OPTIONAL_LOCKS".to_owned(), "0".to_owned()));
        }

        let started = Timestamp::now();
        let mut transcript = self
            .store
            .start_run(
                &self.id,
                RunHeader {
                    started,
                    agent: self.thread.agent.clone(),
                    prompt: turn.prompt.clone(),
                    kind: turn.kind,
                    session_id: Some(session_id.clone()),
                    resume: turn.resume,
                    model: model.clone(),
                    effort: effort.clone(),
                    network: self.thread.network,
                    images: turn.images.iter().map(|image| image.name.clone()).collect(),
                },
            )
            .map_err(|error| failed(format!("GitEZ could not start the transcript: {error}")))?;
        let transcript_name = transcript.file_name();
        let base_steps = finished_steps(&self.thread);
        self.save(|thread| {
            thread.state = State::Running;
            thread.activity = Some(Activity {
                tool: None,
                detail: "starting".to_owned(),
                steps: base_steps,
                updated: started,
            });
            if let Some(turn) = thread.turns.get_mut(index) {
                turn.started = Some(started);
                turn.transcript = Some(transcript_name.clone());
            }
        });
        log(
            &self.id,
            &format!(
                "running {} ({} turn, {})",
                agent.name(),
                name_of(&turn.kind),
                if turn.resume {
                    format!("--resume {session_id}")
                } else {
                    format!("--session-id {session_id}")
                }
            ),
        );

        let launched = Instant::now();
        let mut first_event: Option<Duration> = None;
        let mut steps = 0_u32;
        let mut finished: Option<Finished> = None;
        let mut usage_limit = None;
        let mut pending_activity: Option<Activity> = None;
        let mut session_known = self.thread.session_id.is_some();
        let mut transcript_error = None;
        let profile = self.thread.network;
        let ran = self.run_command(
            Kind::Agent,
            command,
            &needs,
            &context.settings,
            Some(profile),
            |this, line| {
                if let Err(error) = transcript.append(&agent.transcript_line(line)) {
                    transcript_error.get_or_insert(error);
                }
                let Some(event) = agent.parse(line) else {
                    return;
                };
                first_event.get_or_insert_with(|| launched.elapsed());
                match event {
                    AgentEvent::Started {
                        session_id: started,
                        ..
                    } => {
                        if !session_known {
                            session_known = true;
                            let id = started.unwrap_or_else(|| session_id.clone());
                            this.save(|thread| {
                                if thread.session_id.is_none() {
                                    thread.session_id = Some(id);
                                }
                            });
                        }
                    }
                    AgentEvent::Tool { name, detail } => {
                        steps += 1;
                        pending_activity = Some(Activity {
                            tool: Some(name),
                            detail,
                            steps: base_steps + steps,
                            updated: Timestamp::now(),
                        });
                    }
                    AgentEvent::UsageLimit(limit) => usage_limit = Some(limit),
                    AgentEvent::Finished(done) => {
                        if !session_known && let Some(id) = done.session_id.clone() {
                            session_known = true;
                            this.save(|thread| {
                                if thread.session_id.is_none() {
                                    thread.session_id = Some(id);
                                }
                            });
                        }
                        finished = Some(done);
                    }
                    AgentEvent::Text(_) | AgentEvent::ToolFailed(_) => {}
                }
                if pending_activity.is_some()
                    && this
                        .last_activity
                        .is_none_or(|last| last.elapsed() >= ACTIVITY_EVERY)
                    && let Some(activity) = pending_activity.take()
                {
                    this.last_activity = Some(Instant::now());
                    this.save(|thread| thread.activity = Some(activity));
                }
            },
        )?;
        drop(transcript);
        if let Some(error) = transcript_error {
            self.warn(format!(
                "GitEZ could not write the whole transcript: {error}"
            ));
        }
        log(
            &self.id,
            &format!(
                "agent ended after {} ms (first event after {}), exit {:?}",
                launched.elapsed().as_millis(),
                first_event.map_or_else(|| "none".to_owned(), |t| format!("{} ms", t.as_millis())),
                ran.outcome.code
            ),
        );

        let stopped = self.env.stop.load(Ordering::SeqCst);
        let success = finished.as_ref().is_some_and(|done| done.success) && !stopped;
        let ended = Timestamp::now();
        let mut message = finished
            .as_ref()
            .map(|done| done.text.clone())
            .unwrap_or_default();
        // An Auto thread's agent chose to implement: the brief, without
        // the marker, goes on to an Implement thread GitEZ starts.
        let mut handoff = None;
        if self.thread.auto
            && self.thread.mode == Mode::Discuss
            && let Some(brief) = split_handoff(&message)
        {
            message = brief.to_owned();
            if success {
                log(&self.id, "the agent answered with a brief to implement");
                handoff = Some(Handoff::default());
            }
        }
        let result = TurnResult {
            success,
            message,
            cost_usd: finished.as_ref().and_then(|done| done.cost_usd),
            duration_ms: finished.as_ref().and_then(|done| done.duration_ms),
            steps,
            exit_code: ran.outcome.code,
            stopped: ran.outcome.stopped || stopped,
            blocked_hosts: ran.blocked_hosts.clone(),
        };
        self.save(|thread| {
            if let Some(turn) = thread.turns.get_mut(index) {
                turn.ended = Some(ended);
                turn.result = Some(result);
                turn.handoff = handoff;
            }
            if let Some(supervisor) = &mut thread.supervisor {
                supervisor.child_group = None;
            }
            thread.activity = None;
        });
        self.check_refs(refs_before.as_ref());

        if stopped {
            return Err(End::Stopped);
        }
        if success {
            return Ok(());
        }
        let limit = usage_limit.or(finished.as_ref().and_then(|done| done.usage_limit));
        if let Some(limit) = limit {
            return Err(End::UsageLimit(limit.resets_at));
        }
        let mut message = finished
            .map(|done| done.text)
            .filter(|text| !text.trim().is_empty())
            .or_else(|| {
                let stderr = self.secrets.redact(&ran.outcome.stderr);
                let tail = last_lines(&stderr, TAIL_LINES);
                (!tail.is_empty()).then_some(tail)
            })
            .unwrap_or_else(|| match ran.outcome.code {
                Some(code) => format!("The agent exited with code {code}."),
                None => "The agent was ended by a signal.".to_owned(),
            });
        if let Some(hint) = refused_hint(agent.as_ref(), &ran.blocked_hosts) {
            message.push_str(&hint);
        }
        Err(failed(message))
    }

    /// Step 5b: wait until the usage limit has reset, then queue a Resume
    /// turn.
    fn wait_for_reset(&mut self, resets_at: i64) -> Result<(), End> {
        let margin = SignedDuration::try_from(self.env.poll * RESET_MARGIN_POLLS)
            .unwrap_or(SignedDuration::from_secs(60));
        let reset = Timestamp::from_second(resets_at).unwrap_or_else(|_| Timestamp::now());
        let until = reset.checked_add(margin).unwrap_or(reset);
        log(&self.id, &format!("usage limit: waiting until {until}"));
        self.save(|thread| {
            thread.state = State::Waiting;
            thread.waiting_until = Some(until);
            thread.activity = None;
        });
        loop {
            let left = until.duration_since(Timestamp::now());
            if left <= SignedDuration::ZERO {
                break;
            }
            let wait = Duration::try_from(left)
                .unwrap_or_default()
                .min(self.env.poll);
            if crate::queue::sleep(wait, &self.env.stop) {
                return Err(End::Stopped);
            }
        }
        let now = Timestamp::now();
        self.save(|thread| {
            let resume = thread.session_id.is_some();
            // A prompt sent again takes its images along.
            let (prompt, images) = if resume {
                (CONTINUE_PROMPT.to_owned(), Vec::new())
            } else {
                thread
                    .turns
                    .last()
                    .map(|turn| (turn.prompt.clone(), turn.images.clone()))
                    .unwrap_or_default()
            };
            thread.turns.push(Turn {
                kind: TurnKind::Resume,
                prompt,
                requested: now,
                resume,
                started: None,
                ended: None,
                transcript: None,
                result: None,
                images,
                handoff: None,
            });
            thread.waiting_until = None;
            thread.state = State::Queued;
        });
        Ok(())
    }

    /// Step 6: warn about refs other than the thread's own that moved.
    fn check_refs(&mut self, before: Option<&RefState>) {
        let Some(before) = before else {
            return;
        };
        let Ok(main_dir) = self.main_dir() else {
            return;
        };
        let Some(after) = ref_state(&main_dir) else {
            return;
        };
        let (others, _) = self.store.load_all();
        let mut exempt: BTreeSet<String> = others
            .into_iter()
            .filter(|thread| thread.id != self.id && thread.state.is_active())
            .filter_map(|thread| thread.branch)
            .collect();
        exempt.extend(self.thread.branch.clone());
        let mut moved: Vec<String> = changed(&before.branches, &after.branches)
            .into_iter()
            .filter(|branch| !exempt.contains(branch))
            .collect();
        moved.extend(changed(&before.tags, &after.tags));
        if before.head != after.head {
            moved.push("HEAD".to_owned());
        }
        if !moved.is_empty() {
            self.warn(format!(
                "Moved while the agent ran: {}. If you did not move them, check what the agent did.",
                moved.join(", ")
            ));
        }
    }

    /// Step 7: inside the sandbox without network, read the worktree's
    /// status, commit what the agent left uncommitted (so hooks run
    /// sandboxed), and read the status again; record what remains on the
    /// thread ([`AgentThread::uncommitted`]). Everything that reads the
    /// worktree's files runs in the sandbox: its content is the agent's,
    /// and its output is untrusted text here. Never fails the thread.
    ///
    /// `check_place` already made sure, this run, that the worktree's git
    /// files are the ones GitEZ recorded, so the git this runs in the
    /// sandbox is the thread's own repository.
    fn commit_leftovers(&mut self) {
        if self.place.is_none() {
            return;
        }
        let Some(branch) = self.thread.branch.clone() else {
            return;
        };
        let checked = Timestamp::now();
        self.set_activity_detail("committing leftovers");
        // Before the pull request exists, the title the agent gave its
        // work is the one the pull request will take.
        let title = self
            .thread
            .pull_request
            .is_none()
            .then(|| self.thread.final_message().and_then(title_from_message))
            .flatten()
            .unwrap_or_else(|| self.thread.title.clone());
        let message = format!("{title}\n\n{LEFTOVERS_NOTE}\n");
        // One sandboxed shell run: status, commit if on the branch and
        // dirty, status again. Sentinels mark the sections; the status
        // lines are `git status --porcelain` output, untrusted here.
        let script = "\
status() { git status --porcelain 2>&1; }\n\
branch=$(git symbolic-ref --short -q HEAD || true)\n\
printf '%s %s\\n' GITEZ-HEAD \"$branch\"\n\
dirty=$(status)\n\
if [ -n \"$dirty\" ]; then\n\
  if [ \"$branch\" = \"$GITEZ_BRANCH\" ]; then\n\
    git add --all >&2 || true\n\
    if printf '%s' \"$GITEZ_COMMIT_MESSAGE\" | git commit --quiet --file=- >&2; then\n\
      echo GITEZ-COMMIT-OK\n\
    else\n\
      echo \"GITEZ-COMMIT-FAILED rc=$?\"\n\
    fi\n\
  else\n\
    echo GITEZ-COMMIT-WRONG-BRANCH\n\
  fi\n\
fi\n\
echo GITEZ-STATUS\n\
status\n";
        let mut command = self.command("/bin/sh", vec!["-c".to_owned(), script.to_owned()], None);
        command
            .env
            .push(("GITEZ_BRANCH".to_owned(), branch.clone()));
        command
            .env
            .push(("GITEZ_COMMIT_MESSAGE".to_owned(), message));
        let settings = self.settings();
        let mut lines: Vec<String> = Vec::new();
        let ran = self.run_command(
            Kind::Commit,
            command,
            &AgentNeeds::default(),
            &settings,
            None,
            |_, line| lines.push(line.to_owned()),
        );
        let stderr = match &ran {
            Ok(ran) => self.secrets.redact(&ran.outcome.stderr),
            Err(_) => String::new(),
        };
        let run_problem = match &ran {
            Err(End::Failed(_, message)) => Some(message.clone()),
            Err(_) => Some("GitEZ was stopped before it finished.".to_owned()),
            Ok(ran) if !ran.outcome.success => Some(
                last_lines(&stderr, TAIL_LINES)
                    .trim()
                    .to_owned()
                    .pipe(|tail| {
                        if tail.is_empty() {
                            "the sandbox command did not succeed".to_owned()
                        } else {
                            tail
                        }
                    }),
            ),
            Ok(_) => None,
        };
        let outcome = CommitOutcome::parse(&lines);
        if !outcome.checked {
            // The worktree could not be read (the run failed, or was
            // stopped): its state is unknown, not "no changes".
            let problem =
                run_problem.unwrap_or_else(|| "GitEZ could not read the worktree.".to_owned());
            self.warn(format!(
                "GitEZ could not check whether the agent left uncommitted changes: {problem} If \
                 it did, they are still in the worktree."
            ));
            return;
        }
        let paths: Vec<String> = outcome
            .status
            .iter()
            .take(UNCOMMITTED_PATHS)
            .map(|line| self.secrets.redact(line))
            .collect();
        let count = outcome.status.len() as u32;
        // Why the agent's changes are still uncommitted, if they are.
        let commit_error = match &outcome.commit {
            _ if count == 0 => None,
            Some(CommitStep::Ok) => Some(format!(
                "GitEZ committed, but {count} change(s) are still uncommitted: {}",
                paths.join("; ")
            )),
            Some(CommitStep::WrongBranch) => Some(format!(
                "The worktree is no longer on {branch}, so GitEZ did not commit the changes the \
                 agent left."
            )),
            Some(CommitStep::Failed) => Some(commit_failure(&stderr)),
            // Dirty, but the commit step did not run (the run itself failed).
            None => Some(run_problem.clone().unwrap_or_else(|| {
                "GitEZ could not commit the changes the agent left.".to_owned()
            })),
        };
        let uncommitted = Uncommitted {
            checked,
            count,
            paths,
            commit_error: commit_error.clone(),
        };
        let too_short: Vec<String> = self
            .secrets
            .too_short()
            .iter()
            .map(|&n| n.to_owned())
            .collect();
        self.save(|thread| thread.uncommitted = Some(uncommitted.clone()));
        if let Some(problem) = run_problem.filter(|_| outcome.commit.is_none()) {
            self.warn(format!(
                "GitEZ could not check or commit the worktree: {problem}"
            ));
        }
        if let Some(error) = &commit_error {
            self.warn(error.clone());
        }
        if !too_short.is_empty() {
            self.warn(format!(
                "These secrets' values are too short for GitEZ to keep out of transcripts, so \
                 GitEZ did not redact them: {}.",
                too_short.join(", ")
            ));
        }
    }

    /// Step 8: a local pull request once the branch has commits of its own.
    /// Never for a Discuss thread, which has no branch.
    fn pull_request(&mut self) -> Result<(), End> {
        if self.thread.pull_request.is_some() {
            return Ok(());
        }
        let Some(branch) = self.thread.branch.clone() else {
            return Ok(());
        };
        let failed = |message: String| End::Failed(Step::PullRequest, message);
        let main_dir = self.main_dir().map_err(failed)?;
        let main = Repository::open(&main_dir).map_err(|error| failed(error.to_string()))?;
        let read = |error: gitez_git::Error| failed(error.to_string());
        let Some(head) = main
            .resolve_commit(&format!("refs/heads/{branch}"))
            .map_err(read)?
        else {
            return Ok(());
        };
        let start = gitez_git::Oid::from_hex(self.thread.start.commit.as_bytes())
            .map_err(|error| failed(format!("The thread's start commit is invalid: {error}")))?;
        let (ahead, _) = main.ahead_behind(head, start).map_err(read)?;
        if ahead == 0 {
            return Ok(());
        }
        let base = main
            .resolve_commit(&format!("refs/heads/{}", self.thread.start.branch))
            .map_err(read)?
            .map_or_else(|| self.thread.start.commit.clone(), |oid| oid.to_string());
        // The title the agent gave its work, else its first commit's
        // subject, else the thread's title (from the prompt).
        let (heading, description) =
            gitez_agent_thread::split_title(self.thread.final_message().unwrap_or_default());
        let found = heading.or_else(|| first_commit_title(&main, head, start));
        let title = found.clone().unwrap_or_else(|| self.thread.title.clone());
        let description = description.to_owned();
        let pull_request = gitez_review::Store::new(&self.common_dir)
            .create(
                gitez_review::NewPullRequest {
                    title,
                    description,
                    base: gitez_review::Tip {
                        branch: self.thread.start.branch.clone(),
                        commit: base,
                    },
                    head: gitez_review::Tip {
                        branch,
                        commit: head.to_string(),
                    },
                },
                Timestamp::now(),
            )
            .map_err(|error| failed(error.to_string()))?;
        log(
            &self.id,
            &format!("created local pull request #{}", pull_request.number),
        );
        // The thread takes the pull request's name; its id and branch stay.
        self.try_save(|thread| {
            thread.pull_request = Some(pull_request.number);
            if let Some(title) = found {
                thread.title = title;
            }
        })
        .map_err(|error| failed(error.to_string()))?;
        Ok(())
    }

    /// Runs `command` wrapped in the sandbox with the network of `profile`
    /// (`None`: no network), in its own process group recorded as
    /// `child_group`, passing each redacted output line to `on_line`. The
    /// command gets the thread's toolchain caches (see `toolchains`) unless
    /// its own environment sets them. What a stop does depends on `kind`.
    fn run_command(
        &mut self,
        kind: Kind,
        mut command: CommandSpec,
        needs: &AgentNeeds,
        settings: &Settings,
        profile: Option<NetworkProfile>,
        mut on_line: impl FnMut(&mut Self, &str),
    ) -> Result<Ran, End> {
        let step = kind.step();
        let Some(place) = self.place.clone() else {
            return Err(End::Failed(
                Step::Worktree,
                "The worktree is not ready.".to_owned(),
            ));
        };
        let network = match profile {
            Some(profile) => self.network(profile, needs, settings),
            None => Network::Hosts(Vec::new()),
        };
        // Every run, as the last run may have changed them: GitEZ's own
        // folders must still be what they were, never links, before GitEZ
        // writes caches in them and the sandbox makes them writable.
        self.check_place(&place)?;
        // A Discuss thread is not given the folders of Settings.
        let writable: Vec<PathBuf> = if place.read_only() {
            Vec::new()
        } else {
            settings
                .writable_paths()
                .iter()
                .map(|path| policy::real(path))
                .collect()
        };
        let caches = toolchains::prepare(
            &place.caches_dir,
            policy::downloads(profile),
            &writable,
            &toolchains::Lookup::system(),
        );
        for line in &caches.log {
            log(&self.id, line);
        }
        for (name, value) in caches.env {
            if !command.env.iter().any(|(set, _)| *set == name) {
                command.env.push((name, value));
            }
        }
        // Only the agent gets the local ports the user opened, to run and
        // test servers; the setup command and GitEZ's commit never do.
        let local_ports = kind == Kind::Agent && self.thread.local_ports;
        let (policy, refused) =
            policy::policy(&place, needs, settings, network, profile, local_ports);
        for path in refused {
            let warning = format!(
                "{} is a symbolic link, so the sandbox did not make it writable. Make the folder \
                 it points to writable in Settings instead if the agent needs it.",
                path.display()
            );
            if !self.thread.warnings.contains(&warning) {
                self.warn(warning);
            }
        }
        let wrapped = self
            .env
            .sandbox
            .wrap(command, &policy)
            .map_err(|error| End::Failed(Step::Sandbox, error.to_string()))?;
        let mut process = AgentProcess::spawn(&wrapped.command)
            .map_err(|error| End::Failed(step, error.to_string()))?;
        let group = process.id();
        self.save(|thread| {
            if let Some(supervisor) = &mut thread.supervisor {
                supervisor.child_group = Some(group);
            }
        });
        let watcher = Watcher::start(process.stopper(), self.env.stop.clone(), kind);
        let mut tail = VecDeque::new();
        let mut read_error = None;
        loop {
            match process.next_line() {
                Ok(Some(line)) => {
                    let line = self.secrets.redact(&line);
                    on_line(self, &line);
                    push_tail(&mut tail, line);
                }
                Ok(None) => break,
                Err(error) => {
                    // Nothing more can be read; do not leave it running.
                    process.stopper().stop();
                    read_error = Some(error);
                    break;
                }
            }
        }
        let outcome = process.finish();
        watcher.finish();
        let blocked_hosts = wrapped.blocked_hosts();
        drop(wrapped);
        self.save(|thread| {
            if let Some(supervisor) = &mut thread.supervisor {
                supervisor.child_group = None;
            }
        });
        let outcome = outcome.map_err(|error| End::Failed(step, error.to_string()))?;
        if let Some(error) = read_error {
            log(&self.id, &format!("could not read the output: {error}"));
        }
        Ok(Ran {
            outcome,
            tail,
            blocked_hosts,
        })
    }

    /// Fails unless the worktree's git files are as recorded and the
    /// thread's own folders are real folders. A Discuss thread's working
    /// tree is the user's, which no agent could write: only the folders
    /// are checked.
    fn check_place(&mut self, place: &Place) -> Result<(), End> {
        if let Work::Own { worktree, .. } = &place.work {
            gitez_agent_thread::check_worktree(
                &place.common_dir,
                worktree,
                self.thread.worktree_record.as_ref(),
            )
            .map_err(|error| End::Failed(Step::Worktree, error.to_string()))?;
        }
        for dir in [&place.tmp_dir, &place.caches_dir] {
            if own_dir(dir).map_err(|error| {
                End::Failed(
                    Step::Worktree,
                    format!("GitEZ could not create {}: {error}", dir.display()),
                )
            })? {
                self.warn(format!(
                    "{} was not a folder (perhaps a link the agent made); GitEZ replaced it with \
                     an empty one.",
                    dir.display()
                ));
            }
        }
        Ok(())
    }

    /// A command run in the worktree, with GitEZ's environment.
    fn command(&self, program: &str, args: Vec<String>, stdin: Option<String>) -> CommandSpec {
        let cwd = match &self.place {
            Some(place) => place.dir().to_owned(),
            None => self.thread.worktree.clone().unwrap_or_default(),
        };
        let mut command = CommandSpec {
            program: program.into(),
            args,
            env: Vec::new(),
            env_remove: Vec::new(),
            cwd,
            stdin,
        };
        self.add_env(&mut command);
        command
    }

    /// D.7's environment and the secrets, after the command's own.
    fn add_env(&self, command: &mut CommandSpec) {
        let tmp_dir = self
            .place
            .as_ref()
            .map(|place| place.tmp_dir.clone())
            .unwrap_or_else(std::env::temp_dir);
        command
            .env_remove
            .extend(policy::ENV_REMOVE.map(str::to_owned));
        command.env.extend(policy::env(&tmp_dir, &self.secrets));
    }

    fn network(&self, profile: NetworkProfile, needs: &AgentNeeds, settings: &Settings) -> Network {
        let remotes: Vec<String> = self
            .main_dir
            .as_ref()
            .and_then(|dir| Repository::open(dir).ok())
            .map(|main| {
                main.remote_names()
                    .iter()
                    .filter_map(|name| main.remote_url(name))
                    .collect()
            })
            .unwrap_or_default();
        policy::network(
            profile,
            &needs.hosts,
            settings,
            &remotes,
            &policy::ssh_hostname,
        )
    }

    /// The main worktree of the repository.
    fn main_dir(&mut self) -> Result<PathBuf, String> {
        if let Some(dir) = &self.main_dir {
            return Ok(dir.clone());
        }
        let repository = Repository::open(&self.common_dir).map_err(|error| error.to_string())?;
        let dir = repository
            .work_dir()
            .map(Path::to_owned)
            .ok_or_else(|| "The repository has no working tree.".to_owned())?;
        self.main_dir = Some(dir.clone());
        Ok(dir)
    }

    /// `<repo key>`: the worktree's parent folder, as the GUI named it; for
    /// a thread without a worktree, from the repository's folder name.
    fn repository_key(&self) -> String {
        self.thread
            .worktree
            .as_deref()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| {
                let name = self
                    .common_dir
                    .parent()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                gitez_agent_thread::repository_key(&name, &self.common_dir)
            })
    }

    fn stopped(&self) -> Result<(), End> {
        if self.env.stop.load(Ordering::SeqCst) {
            Err(End::Stopped)
        } else {
            Ok(())
        }
    }

    fn set_preparing(&mut self, detail: &str) {
        let steps = self.thread.steps();
        let detail = detail.to_owned();
        self.save(|thread| {
            thread.state = State::Preparing;
            thread.activity = Some(Activity {
                tool: Some("Setup".to_owned()),
                detail,
                steps,
                updated: Timestamp::now(),
            });
        });
    }

    /// Shows `detail` as a Setup activity without changing the state.
    fn set_activity_detail(&mut self, detail: &str) {
        let steps = finished_steps(&self.thread);
        let detail = detail.to_owned();
        self.save(|thread| {
            thread.activity = Some(Activity {
                tool: Some("Setup".to_owned()),
                detail,
                steps,
                updated: Timestamp::now(),
            });
        });
    }

    fn warn(&mut self, warning: String) {
        log(&self.id, &format!("warning: {warning}"));
        self.save(|thread| thread.warnings.push(warning));
    }

    /// Changes thread.json; a failure is logged and the run goes on.
    fn save(&mut self, change: impl FnOnce(&mut AgentThread)) {
        if let Err(error) = self.try_save(change) {
            log(&self.id, &format!("could not save the thread: {error}"));
        }
    }

    fn try_save(
        &mut self,
        change: impl FnOnce(&mut AgentThread),
    ) -> Result<(), gitez_agent_thread::Error> {
        self.thread = self.store.update(&self.id, |thread| {
            change(thread);
            Ok(thread.clone())
        })?;
        Ok(())
    }
}

/// What the sandboxed leftover-commit script reported.
#[derive(Debug, Default, PartialEq, Eq)]
struct CommitOutcome {
    /// The commit step, if it ran (the worktree was dirty).
    commit: Option<CommitStep>,
    /// `git status --porcelain` lines after the commit: the worktree's
    /// remaining changes, untrusted text.
    status: Vec<String>,
    /// Whether the final status section was reached: without it the
    /// worktree's state is not known.
    checked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitStep {
    Ok,
    Failed,
    WrongBranch,
}

impl CommitOutcome {
    /// Reads the sentinel-marked sections the script printed. Unknown
    /// lines before `GITEZ-STATUS` (a hook's own output on stdout, say) are
    /// ignored; everything after it is the status.
    fn parse(lines: &[String]) -> Self {
        let mut outcome = CommitOutcome::default();
        let mut in_status = false;
        for line in lines {
            if in_status {
                if !line.is_empty() {
                    outcome.status.push(line.clone());
                }
                continue;
            }
            match line.as_str() {
                "GITEZ-STATUS" => {
                    in_status = true;
                    outcome.checked = true;
                }
                "GITEZ-COMMIT-OK" => outcome.commit = Some(CommitStep::Ok),
                "GITEZ-COMMIT-WRONG-BRANCH" => outcome.commit = Some(CommitStep::WrongBranch),
                _ if line.starts_with("GITEZ-COMMIT-FAILED") => {
                    outcome.commit = Some(CommitStep::Failed);
                }
                _ => {}
            }
        }
        outcome
    }
}

/// Why GitEZ's commit of the leftovers failed, from the run's error output.
fn commit_failure(stderr: &str) -> String {
    let tail = last_lines(stderr, TAIL_LINES);
    if tail.trim().is_empty() {
        "GitEZ could not commit the changes the agent left (a failing hook, perhaps).".to_owned()
    } else {
        format!(
            "GitEZ could not commit the changes the agent left:\n{}",
            tail.trim()
        )
    }
}

/// Lets a value be post-processed inline, for readability.
trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}

impl<T> Pipe for T {}

/// A command that ran.
struct Ran {
    outcome: gitez_agent::Outcome,
    /// Its last output lines, redacted.
    tail: VecDeque<String>,
    blocked_hosts: Vec<String>,
}

/// What a command run for a thread is; it says what a stop does to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The setup command: killed at once on stop.
    Setup,
    /// The agent: interrupted on stop, killed 20 s later.
    Agent,
    /// GitEZ's commit of the leftovers: runs to its end after a stop, but
    /// is killed after 5 minutes.
    Commit,
}

impl Kind {
    fn step(self) -> Step {
        match self {
            Self::Setup => Step::Prepare,
            Self::Agent => Step::Agent,
            Self::Commit => Step::Commit,
        }
    }
}

/// Cancels the git commands of an [`Operation`](gitez_command::Operation)
/// from a thread of its own once the stop flag is set: those GitEZ runs
/// itself, each in a process group of its own, which a stop reaches no
/// other way.
struct CancelOnStop {
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl CancelOnStop {
    fn start(operation: gitez_command::Operation, stop: Arc<AtomicBool>) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let finished = done.clone();
        let handle = std::thread::spawn(move || {
            while !finished.load(Ordering::SeqCst) {
                if stop.load(Ordering::SeqCst) {
                    operation.cancel();
                    return;
                }
                std::thread::sleep(crate::queue::STOP_CHECK);
            }
        });
        Self {
            done,
            handle: Some(handle),
        }
    }

    fn finish(mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            handle.join().ok();
        }
    }
}

impl Drop for CancelOnStop {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
    }
}

/// Stops a process from a thread of its own when the stop flag is set or
/// its time is up, as its [`Kind`] says.
struct Watcher {
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Watcher {
    fn start(stopper: Stopper, stop: Arc<AtomicBool>, kind: Kind) -> Self {
        let (watch_stop, grace, timeout) = match kind {
            Kind::Setup => (true, None, None),
            Kind::Agent => (true, Some(STOP_GRACE), None),
            Kind::Commit => (false, None, Some(COMMIT_TIMEOUT)),
        };
        let done = Arc::new(AtomicBool::new(false));
        let finished = done.clone();
        let handle = std::thread::spawn(move || {
            let started = Instant::now();
            let mut interrupted: Option<Instant> = None;
            while !finished.load(Ordering::SeqCst) {
                if timeout.is_some_and(|timeout| started.elapsed() >= timeout) {
                    stopper.stop();
                    return;
                }
                if watch_stop && stop.load(Ordering::SeqCst) {
                    match (grace, interrupted) {
                        (None, _) => {
                            stopper.stop();
                            return;
                        }
                        (Some(_), None) => {
                            stopper.interrupt();
                            interrupted = Some(Instant::now());
                        }
                        (Some(grace), Some(at)) if at.elapsed() >= grace => {
                            stopper.stop();
                            return;
                        }
                        _ => {}
                    }
                }
                std::thread::sleep(crate::queue::STOP_CHECK);
            }
        });
        Self {
            done,
            handle: Some(handle),
        }
    }

    fn finish(mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            handle.join().ok();
        }
    }
}

/// Whether `command` is what the thread waits for (D.1).
fn check_command(thread: &AgentThread, command: Command) -> Result<(), String> {
    let pending = thread.pending_turn().map(|index| thread.turns[index].kind);
    let matches = thread.state == State::Queued
        && match command {
            Command::Start => pending == Some(TurnKind::Start),
            Command::Reply => matches!(
                pending,
                Some(TurnKind::Reply | TurnKind::Review | TurnKind::Brief)
            ),
            Command::Retry => matches!(pending, Some(TurnKind::Retry) | None),
        };
    if matches {
        return Ok(());
    }
    let waits_for = match (thread.state, pending) {
        (State::Queued, Some(kind)) => format!("a {} turn", name_of(&kind)),
        (State::Queued, None) => "a retry".to_owned(),
        (state, _) => format!("nothing (it is {})", name_of(&state)),
    };
    Err(format!(
        "GitEZ started the supervisor to {} this thread, but the thread waits for {waits_for}.",
        command.as_str()
    ))
}

/// The cleaned subject of the oldest commit from `start` to `head` that is
/// not GitEZ's commit of the agent's leftovers, if one has a subject. Read
/// failures leave the title to the caller's fallback.
fn first_commit_title(
    main: &Repository,
    head: gitez_git::Oid,
    start: gitez_git::Oid,
) -> Option<String> {
    let commits: Vec<gitez_git::Commit> = main
        .walk_excluding([head], [start], gitez_git::Order::Topo)
        .ok()?
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    // Newest first: parents come after their children.
    commits.iter().rev().find_map(|commit| {
        let message = main.commit_message(commit.id).ok()?;
        if is_leftovers_commit(&message) {
            return None;
        }
        gitez_agent_thread::clean_title(message.lines().next().unwrap_or_default())
    })
}

/// Whether `message` is GitEZ's commit of the agent's leftovers: a line of
/// its body is [`LEFTOVERS_NOTE`] (hooks may add trailers after it).
fn is_leftovers_commit(message: &str) -> bool {
    message
        .lines()
        .skip(1)
        .any(|line| line.trim() == LEFTOVERS_NOTE)
}

/// D.8, and what the sandbox does not allow that an agent would otherwise
/// try first: `/tmp` and the system temporary folder (macOS `mktemp`
/// without a template uses the latter), `git stash` (its ref is shared
/// with every worktree), and, with less network, downloads.
fn system_prompt(branch: &str, start_branch: &str, network: NetworkProfile) -> String {
    let mut prompt = format!(
        "You are working in a git worktree that GitEZ created for this task, on the branch \
         {branch}, started from {start_branch}. Commit your work on this branch as you go, with \
         clear messages. Do not push, do not switch branches, do not create other branches or \
         tags, and do not change the git configuration. When you are done, reply with a short \
         summary of what you did and anything left to do. If you changed files, begin that reply \
         with a title for your change as a level-1 Markdown heading (\"# <title>\") and a blank \
         line: GitEZ names the pull request with it. The title says what the change does, not \
         what was asked, in the imperative mood like a good commit subject (\"Cache parsed \
         config files\"), about 60 characters at most, with no trailing period and no quotes or \
         backticks around it. If you changed nothing, answer without a heading. \
         You run in a sandbox. You can write only in this worktree and in $TMPDIR, which is \
         yours: put temporary files there, not in /tmp, and give mktemp a template in it \
         (mktemp \"$TMPDIR/name.XXXXXX\"). git stash does not work here; commit work in \
         progress on your branch instead."
    );
    prompt.push_str(network_note(network));
    prompt
}

/// What a network profile keeps the agent from, and what the user can do
/// about it ("" for Open).
fn network_note(network: NetworkProfile) -> &'static str {
    match network {
        NetworkProfile::AgentOnly => {
            " The network reaches only your own API: package registries cannot be reached, and \
             Cargo and Go run offline from the packages already on this computer. If the task \
             needs a download, say so in your summary: the user can retry the thread with the \
             Packages network profile."
        }
        NetworkProfile::Packages => {
            " The network reaches only package registries, GitHub and the repository's \
             remotes. If the task needs another host, name it in your summary: the user can \
             retry the thread with the Open network profile."
        }
        NetworkProfile::Open => "",
    }
}

/// What the agent is told about local ports ("" for Open, which allows
/// them anyway, and on Linux, where the sandbox has a loopback of its own).
fn local_ports_note(network: NetworkProfile, local_ports: bool) -> &'static str {
    if network == NetworkProfile::Open {
        ""
    } else if local_ports {
        " Local ports are open: you may run a server on localhost (a dev server, for example) \
         and connect to it to check your work. Stop the servers you started before you finish."
    } else if cfg!(target_os = "macos") {
        " The sandbox refuses listening on ports and connecting to localhost, so a dev server, \
         or a test that starts one, fails here. If checking your work needs one, say so in your \
         summary: the user can continue the thread with Local ports on."
    } else {
        ""
    }
}

/// What a Discuss thread's agent is told: where it is, that it changes
/// nothing, and what the sandbox does about it. In an Auto thread
/// (`auto`), it is also told how to hand off to an Implement thread, which
/// starts from the branch `start_branch`.
fn discuss_system_prompt(network: NetworkProfile, auto: bool, start_branch: &str) -> String {
    let mut prompt = "You are in a discussion that GitEZ started in the user's own working tree, \
         with their uncommitted changes as they are. The user wants to ask, explore or plan, not \
         to have anything changed yet. Read the code, run commands that only read, and answer: \
         explain, compare options, recommend, or write a plan. Do not change, create or delete \
         files in the repository, and do not commit, stash, check out or otherwise change its \
         git state. You run in a sandbox that refuses every write outside $TMPDIR, so an attempt \
         fails anyway. $TMPDIR is yours for notes and scratch files: use it, not /tmp, and give \
         mktemp a template in it (mktemp \"$TMPDIR/name.XXXXXX\"). A build or test that writes \
         into the working tree fails here; if you need one, send its output to $TMPDIR (for \
         example CARGO_TARGET_DIR=\"$TMPDIR/target\") or say what you could not run."
        .to_owned();
    if auto {
        prompt.push_str(&auto_rule(start_branch));
    } else {
        prompt.push_str(
            " If the user asks you to make the change, say what you would do and that \
             \"Implement…\" in GitEZ starts an agent that does it on a branch of its own, from a \
             brief you write then.",
        );
    }
    prompt.push_str(network_note(network));
    prompt
}

/// The rule an Auto thread's agent gets on top of a discussion's: it
/// decides, after reading the code, whether a message asks for a change,
/// and if so hands off with [`HANDOFF_MARKER`] and a brief.
fn auto_rule(start_branch: &str) -> String {
    format!(
        " The user started this thread in Auto: you decide whether they want an answer or a \
         change. Read the code before you decide. If a message (the first or a later reply) is \
         a clear request to change code and it is clear what to do, do not make the change and \
         do not ask for confirmation: reply with a line holding only {HANDOFF_MARKER}, then a \
         brief, and GitEZ starts another agent that implements it on a branch of its own, from \
         the last commit of {start_branch}. That agent sees only the brief, not this \
         conversation and not the uncommitted changes in this working tree: if the change \
         depends on them, say so in the brief. Write the brief as follows. {BRIEF_PROMPT} \
         Otherwise, when the user asks, explores or plans, or when it is unclear what they \
         want, answer as a discussion does, without {HANDOFF_MARKER}, and ask what you need to \
         know. Never write {HANDOFF_MARKER} anywhere else."
    )
}

/// How a state, ending or turn kind is written in thread.json.
fn name_of(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Tool uses of the turns that ended.
fn finished_steps(thread: &AgentThread) -> u32 {
    thread
        .turns
        .iter()
        .filter_map(|turn| turn.result.as_ref())
        .map(|result| result.steps)
        .sum()
}

fn ref_state(work_dir: &Path) -> Option<RefState> {
    Repository::open(work_dir).ok()?.ref_state().ok()
}

/// Names whose value differs between `before` and `after`, including
/// names only one of them has.
fn changed<V: PartialEq>(before: &BTreeMap<String, V>, after: &BTreeMap<String, V>) -> Vec<String> {
    before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|name| before.get(*name) != after.get(*name))
        .cloned()
        .collect()
}

fn push_tail(tail: &mut VecDeque<String>, line: String) {
    if tail.len() == TAIL_LINES {
        tail.pop_front();
    }
    tail.push_back(line);
}

fn last_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.trim().lines().collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

/// The sentence a failure message ends with when the sandbox refused hosts
/// the run may have needed. Hosts the agent contacts on its own and does
/// not need ([`Agent::optional_hosts`]) are no reason for a failure, so
/// they are left out; `None` when only those were refused.
fn refused_hint(agent: &dyn Agent, blocked_hosts: &[String]) -> Option<String> {
    let blocked = gitez_agent::BlockedHosts::classify(Some(agent), blocked_hosts);
    (!blocked.problems.is_empty()).then(|| {
        format!(
            "\n\nThe sandbox refused connections to: {}.",
            blocked.problems.join(", ")
        )
    })
}

/// Makes `path` a private folder of the thread's own, if it is not one: a
/// link or file the agent put in its place (it may write in the folder's
/// parent's entry for it) is removed, never followed. The sandbox makes
/// the folder writable, so it must not lead anywhere else. Returns whether
/// something else was there.
fn own_dir(path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::DirBuilderExt as _;
    let replaced = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => return Ok(false),
        Ok(_) => {
            std::fs::remove_file(path)?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::DirBuilder::new().mode(0o700).create(path)?;
    Ok(replaced)
}

/// A line for supervisor.log (the supervisor's standard error). Never
/// holds a secret.
pub(crate) fn log(id: &AgentThreadId, message: &str) {
    eprintln!(
        "{} gitez-supervisor [{id}]: {message}",
        Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_names_include_added_and_removed() {
        let before: BTreeMap<String, u32> = [("a".into(), 1), ("b".into(), 2)].into();
        let after: BTreeMap<String, u32> =
            [("a".into(), 1), ("b".into(), 3), ("c".into(), 1)].into();
        assert_eq!(changed(&before, &after), ["b", "c"]);
        assert_eq!(changed(&after, &before), ["b", "c"]);
    }

    #[test]
    fn the_failure_hint_leaves_out_optional_hosts() {
        let agent = gitez_agent::ClaudeCode;
        let hosts = |hosts: &[&str]| -> Vec<String> {
            hosts.iter().map(|host| (*host).to_owned()).collect()
        };
        let optional = hosts(&[
            "mcp-proxy.anthropic.com",
            "http-intake.logs.us5.datadoghq.com",
        ]);
        assert_eq!(refused_hint(&agent, &optional), None);
        assert_eq!(refused_hint(&agent, &[]), None);
        assert_eq!(
            refused_hint(
                &agent,
                &hosts(&[
                    "mcp-proxy.anthropic.com",
                    "static.crates.io",
                    "http-intake.logs.us5.datadoghq.com",
                ])
            )
            .as_deref(),
            Some("\n\nThe sandbox refused connections to: static.crates.io.")
        );
    }

    #[test]
    fn last_lines_keeps_the_end() {
        assert_eq!(last_lines("a\nb\nc\n", 2), "b\nc");
        assert_eq!(last_lines("", 2), "");
    }

    #[test]
    fn the_system_prompt_names_the_branches() {
        let prompt = system_prompt("agent/x", "main", NetworkProfile::Open);
        assert!(prompt.contains("on the branch agent/x, started from main."));
        assert!(prompt.contains("Do not push"));
    }

    #[test]
    fn the_system_prompt_asks_for_a_title_only_for_changes() {
        let prompt = system_prompt("agent/x", "main", NetworkProfile::Open);
        assert!(
            prompt.contains("If you changed files, begin that reply with a title"),
            "{prompt}"
        );
        assert!(prompt.contains("level-1 Markdown heading (\"# <title>\")"));
        assert!(prompt.contains("If you changed nothing, answer without a heading."));
    }

    #[test]
    fn the_leftovers_commit_is_recognised_by_its_body() {
        assert!(is_leftovers_commit(&format!(
            "Make the greeting friendlier\n\n{LEFTOVERS_NOTE}"
        )));
        assert!(is_leftovers_commit(&format!(
            "Title\n\n{LEFTOVERS_NOTE}\n\nSigned-off-by: A <a@example.com>"
        )));
        assert!(!is_leftovers_commit(LEFTOVERS_NOTE));
        assert!(!is_leftovers_commit("Make the greeting friendlier"));
    }

    #[test]
    fn a_discussion_is_told_it_changes_nothing_and_how_to_hand_over() {
        let prompt = discuss_system_prompt(NetworkProfile::Open, false, "main");
        assert!(prompt.starts_with("You are in a discussion"));
        assert!(prompt.contains("Do not change, create or delete files"));
        assert!(prompt.contains("refuses every write outside $TMPDIR"));
        assert!(prompt.contains("CARGO_TARGET_DIR=\"$TMPDIR/target\""));
        assert!(prompt.contains("\"Implement…\""));
        // Nothing of an Implement thread's instructions.
        assert!(!prompt.contains("Commit your work"));
        assert!(!prompt.contains("level-1 Markdown heading"));
        // The same words about the network as any thread.
        assert!(!prompt.contains("network"));
        let offline = discuss_system_prompt(NetworkProfile::AgentOnly, false, "main");
        assert!(offline.contains("retry the thread with the Packages network profile"));
        assert!(
            system_prompt("agent/x", "main", NetworkProfile::AgentOnly)
                .ends_with(network_note(NetworkProfile::AgentOnly))
        );
    }

    #[test]
    fn an_auto_discussion_is_told_how_to_hand_off_and_a_plain_one_is_not() {
        let plain = discuss_system_prompt(NetworkProfile::Open, false, "main");
        assert!(!plain.contains(HANDOFF_MARKER), "{plain}");
        assert!(!plain.contains("Auto"), "{plain}");

        let auto = discuss_system_prompt(NetworkProfile::AgentOnly, true, "feature/blame");
        assert!(auto.starts_with("You are in a discussion"));
        assert!(auto.contains("Do not change, create or delete files"));
        assert!(auto.contains(&format!("a line holding only {HANDOFF_MARKER}")));
        // The brief is the one "Implement…" asks for.
        assert!(auto.contains(BRIEF_PROMPT));
        // Where the Implement thread starts, and what it does not see.
        assert!(auto.contains("the last commit of feature/blame"));
        assert!(auto.contains("uncommitted changes"));
        // "Implement…" is the plain discussion's way to hand over.
        assert!(!auto.contains("\"Implement…\" in GitEZ"));
        assert!(auto.ends_with(network_note(NetworkProfile::AgentOnly)));
    }

    #[test]
    fn the_system_prompt_says_what_the_sandbox_refuses_and_what_fixes_it() {
        for profile in NetworkProfile::ALL {
            let prompt = system_prompt("agent/x", "main", profile);
            assert!(
                prompt.contains("mktemp \"$TMPDIR/name.XXXXXX\""),
                "{prompt}"
            );
            assert!(prompt.contains("git stash does not work"), "{prompt}");
        }
        let offline = system_prompt("agent/x", "main", NetworkProfile::AgentOnly);
        assert!(offline.contains("retry the thread with the Packages network profile"));
        let packages = system_prompt("agent/x", "main", NetworkProfile::Packages);
        assert!(packages.contains("retry the thread with the Open network profile"));
        let open = system_prompt("agent/x", "main", NetworkProfile::Open);
        assert!(!open.contains("network profile"));
    }

    #[test]
    fn the_agent_is_told_about_local_ports() {
        assert_eq!(local_ports_note(NetworkProfile::Open, false), "");
        assert_eq!(local_ports_note(NetworkProfile::Open, true), "");
        for profile in [NetworkProfile::AgentOnly, NetworkProfile::Packages] {
            assert!(local_ports_note(profile, true).contains("Local ports are open"));
            let closed = local_ports_note(profile, false);
            if cfg!(target_os = "macos") {
                assert!(closed.contains("with Local ports on"), "{closed}");
            } else {
                assert_eq!(closed, "");
            }
        }
    }
}
