//! The supervisor's work for one agent thread (DESIGN D.5): check, queue,
//! worktree, prepare, run the agent, check refs, commit leftovers, pull
//! request, final state.

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
    RunSpec, Stopper,
};
use gitez_agent_thread::{
    Activity, AgentThread, AgentThreadId, CONTINUE_PROMPT, Ending, PrepareStep, RunHeader, State,
    Step, Store, SupervisorInfo, Timestamp, Turn, TurnKind, TurnResult,
};
use gitez_git::{HeadPlace, RefState, Repository};
use gitez_sandbox::{Network, NetworkProfile, Sandbox as _, Status};
use gitez_settings::{Loaded, Settings};
use jiff::SignedDuration;

use crate::liveness::SupervisorLock;
use crate::policy::{self, Place};
use crate::queue::{Queue, Slot, Waited};
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
const SETUP_LOG: &str = "setup.log";
const SETUP_COMMAND_KEY: &str = "gitez.setupCommand";
const PASSTHROUGH_WARNING: &str = "Ran without a sandbox (GITEZ_TEST_SANDBOX)";
const USAGE_LIMIT: &str = "The agent's usage limit was reached. Retry when it has reset.";

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

    /// Step 3: create the worktree, or check it is the thread's.
    fn worktree(&mut self) -> Result<(), End> {
        let failed = |message: String| End::Failed(Step::Worktree, message);
        let main_dir = self.main_dir().map_err(failed)?;
        let worktree = self.thread.worktree.clone();
        let exists = worktree.exists();
        if !exists || !self.thread.prepared {
            self.set_preparing("creating worktree");
        }
        if !exists {
            // A folder deleted by hand is still registered with git.
            gitez_command::Git::new(&main_dir).worktree_prune().ok();
        }
        let worktree_git_dir = gitez_agent_thread::create_worktree(
            &main_dir,
            &worktree,
            &self.thread.branch,
            &self.thread.start.commit,
        )
        .map_err(|error| failed(error.to_string()))?;
        if !exists && self.thread.prepared {
            // A new folder needs preparing again.
            self.save(|thread| thread.prepared = false);
        }
        let tmp_dir =
            self.env
                .data_dir
                .join("tmp")
                .join(format!("{}-{}", self.repository_key(), self.id));
        create_private_dir(&tmp_dir).map_err(|error| {
            failed(format!(
                "GitEZ could not create {}: {error}",
                tmp_dir.display()
            ))
        })?;
        self.place = Some(Place {
            common_dir: self.common_dir.clone(),
            worktree,
            worktree_git_dir,
            branch: self.thread.branch.clone(),
            tmp_dir,
            data_dir: self.env.data_dir.clone(),
        });
        self.worktree_ready = true;
        Ok(())
    }

    /// Step 4: copy includes, clone caches, run the setup command.
    fn prepare(&mut self, context: &Context) -> Result<(), End> {
        if self.thread.prepared {
            return Ok(());
        }
        let failed = |message: String| End::Failed(Step::Prepare, message);
        let started = Instant::now();
        self.set_preparing("copying files");
        let main_dir = self.main_dir().map_err(failed)?;
        let main = Repository::open(&main_dir).map_err(|error| failed(error.to_string()))?;
        let worktree = self.thread.worktree.clone();
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
        let network = self.network(NetworkProfile::Packages, &needs, &context.settings);
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
            network,
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
        let system_prompt = system_prompt(&self.thread.branch, &self.thread.start.branch);
        let model = self
            .thread
            .model
            .clone()
            .filter(|model| !model.is_empty())
            .or_else(|| context.model.clone());
        let worktree = self.thread.worktree.clone();
        let spec = RunSpec {
            prompt: &turn.prompt,
            cwd: &worktree,
            model: model.as_deref(),
            conversation,
            system_prompt: Some(&system_prompt),
        };
        let mut command = agent
            .command(&context.config, &spec)
            .map_err(|error| failed(error.to_string()))?;
        let needs = agent.needs_for_run(&context.config, &spec);
        self.add_env(&mut command);
        let network = self.network(self.thread.network, &needs, &context.settings);

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
                    network: self.thread.network,
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
        let ran = self.run_command(
            Kind::Agent,
            command,
            &needs,
            &context.settings,
            network,
            |this, line| {
                if let Err(error) = transcript.append(line) {
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
        let result = TurnResult {
            success,
            message: finished
                .as_ref()
                .map(|done| done.text.clone())
                .unwrap_or_default(),
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
        if !ran.blocked_hosts.is_empty() {
            message.push_str(&format!(
                "\n\nThe sandbox refused connections to: {}.",
                ran.blocked_hosts.join(", ")
            ));
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
            let prompt = if resume {
                CONTINUE_PROMPT.to_owned()
            } else {
                thread
                    .turns
                    .last()
                    .map(|turn| turn.prompt.clone())
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
            .map(|thread| thread.branch)
            .collect();
        exempt.insert(self.thread.branch.clone());
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

    /// Step 7: commit what the agent left uncommitted, in the sandbox
    /// without network, so hooks run sandboxed. Never fails the thread.
    fn commit_leftovers(&mut self) {
        let worktree = self.thread.worktree.clone();
        let Ok(repository) = Repository::open(&worktree) else {
            return;
        };
        let status = match repository.status() {
            Ok(status) => status,
            Err(error) => {
                self.warn(format!(
                    "Could not read the worktree's status: {error}. Changes the agent left \
                     uncommitted, if any, are still in the worktree."
                ));
                return;
            }
        };
        if status.is_clean() {
            return;
        }
        let branch = self.thread.branch.clone();
        let on_branch = matches!(
            repository.ref_state().map(|state| state.head),
            Ok(Some(HeadPlace::Branch(name))) if name == branch
        );
        if !on_branch {
            self.warn(format!(
                "The worktree is no longer on {branch}, so GitEZ did not commit the changes the \
                 agent left. They are still in the worktree."
            ));
            return;
        }
        if self.place.is_none() {
            return;
        }
        self.set_activity_detail("committing leftovers");
        let tip_before = tip(&worktree);
        let message = format!(
            "{}\n\nChanges the agent left uncommitted, committed by GitEZ.\n",
            self.thread.title
        );
        let command = self.command(
            "/bin/sh",
            vec![
                "-c".to_owned(),
                "git add --all && git commit --quiet --file=-".to_owned(),
            ],
            Some(message),
        );
        let settings = match Settings::load(&self.env.settings_path) {
            Ok(Loaded::Current(settings)) => settings,
            _ => Settings::default(),
        };
        let mut output = VecDeque::new();
        let ran = self.run_command(
            Kind::Commit,
            command,
            &AgentNeeds::default(),
            &settings,
            Network::Hosts(Vec::new()),
            |_, line| push_tail(&mut output, line.to_owned()),
        );
        let tip_after = tip(&worktree);
        let committed = tip_after.is_some() && tip_after != tip_before;
        let problem = match ran {
            Err(End::Failed(_, message)) => Some(message),
            Err(_) => Some("it was stopped".to_owned()),
            Ok(ran) if !ran.outcome.success || !committed => {
                let mut lines: Vec<String> = output.into();
                if !ran.outcome.stderr.is_empty() {
                    lines.push(self.secrets.redact(&ran.outcome.stderr));
                }
                let text = lines.join("\n");
                Some(if text.trim().is_empty() {
                    "git did not commit them".to_owned()
                } else {
                    text.trim().to_owned()
                })
            }
            Ok(_) => None,
        };
        if let Some(problem) = problem {
            self.warn(format!(
                "Could not commit the changes the agent left: {problem}. They are still in the \
                 worktree."
            ));
        }
    }

    /// Step 8: a local pull request once the branch has commits of its own.
    fn pull_request(&mut self) -> Result<(), End> {
        if self.thread.pull_request.is_some() {
            return Ok(());
        }
        let failed = |message: String| End::Failed(Step::PullRequest, message);
        let main_dir = self.main_dir().map_err(failed)?;
        let main = Repository::open(&main_dir).map_err(|error| failed(error.to_string()))?;
        let read = |error: gitez_git::Error| failed(error.to_string());
        let Some(head) = main
            .resolve_commit(&format!("refs/heads/{}", self.thread.branch))
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
        let pull_request = gitez_review::Store::new(&self.common_dir)
            .create(
                gitez_review::NewPullRequest {
                    title: self.thread.title.clone(),
                    description: self.thread.final_message().unwrap_or_default().to_owned(),
                    base: gitez_review::Tip {
                        branch: self.thread.start.branch.clone(),
                        commit: base,
                    },
                    head: gitez_review::Tip {
                        branch: self.thread.branch.clone(),
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
        self.try_save(|thread| thread.pull_request = Some(pull_request.number))
            .map_err(|error| failed(error.to_string()))?;
        Ok(())
    }

    /// Runs `command` wrapped in the sandbox, in its own process group
    /// recorded as `child_group`, passing each redacted output line to
    /// `on_line`. What a stop does depends on `kind`.
    fn run_command(
        &mut self,
        kind: Kind,
        command: CommandSpec,
        needs: &AgentNeeds,
        settings: &Settings,
        network: Network,
        mut on_line: impl FnMut(&mut Self, &str),
    ) -> Result<Ran, End> {
        let step = kind.step();
        let Some(place) = self.place.clone() else {
            return Err(End::Failed(
                Step::Worktree,
                "The worktree is not ready.".to_owned(),
            ));
        };
        let policy = policy::policy(&place, needs, settings, network);
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

    /// A command run in the worktree, with GitEZ's environment.
    fn command(&self, program: &str, args: Vec<String>, stdin: Option<String>) -> CommandSpec {
        let mut command = CommandSpec {
            program: program.into(),
            args,
            env: Vec::new(),
            env_remove: Vec::new(),
            cwd: self.thread.worktree.clone(),
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
        policy::network(profile, &needs.hosts, settings, &remotes)
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

    /// `<repo key>`: the worktree's parent folder, as the GUI named it.
    fn repository_key(&self) -> String {
        self.thread
            .worktree
            .parent()
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
            Command::Reply => matches!(pending, Some(TurnKind::Reply | TurnKind::Review)),
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

/// D.8.
fn system_prompt(branch: &str, start_branch: &str) -> String {
    format!(
        "You are working in a git worktree that GitEZ created for this task, on the branch \
         {branch}, started from {start_branch}. Commit your work on this branch as you go, with \
         clear messages. Do not push, do not switch branches, do not create other branches or \
         tags, and do not change the git configuration. When you are done, reply with a short \
         summary of what you did and anything left to do."
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

/// The commit `HEAD` of the repository at `work_dir` is on.
fn tip(work_dir: &Path) -> Option<gitez_git::Oid> {
    Repository::open(work_dir)
        .ok()?
        .resolve_commit("HEAD")
        .ok()
        .flatten()
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

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    if path.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
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
    fn last_lines_keeps_the_end() {
        assert_eq!(last_lines("a\nb\nc\n", 2), "b\nc");
        assert_eq!(last_lines("", 2), "");
    }

    #[test]
    fn the_system_prompt_names_the_branches() {
        let prompt = system_prompt("agent/x", "main");
        assert!(prompt.contains("on the branch agent/x, started from main."));
        assert!(prompt.contains("Do not push"));
    }
}
