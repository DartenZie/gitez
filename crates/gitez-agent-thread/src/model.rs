//! What an agent thread holds, as stored in its `thread.json`, and the
//! changes its state allows.

use std::path::PathBuf;

use gitez_sandbox::NetworkProfile;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// An agent thread's id: its slug, unique in its repository. Also its
/// folder name, its branch `agent/<id>` and its worktree folder name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentThreadId(String);

/// The longest id: a 40-character slug with room for a `-<n>` suffix.
const MAX_ID_LEN: usize = 48;

impl AgentThreadId {
    /// `[a-z0-9]` and single `-` between them, 1 to 48 characters.
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        let valid = !id.is_empty()
            && id.len() <= MAX_ID_LEN
            && id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && !id.starts_with('-')
            && !id.ends_with('-')
            && !id.contains("--");
        if valid {
            Ok(Self(id))
        } else {
            Err(Error::InvalidId(id))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `agent/<id>`.
    pub fn branch(&self) -> String {
        format!("agent/{}", self.0)
    }
}

impl std::fmt::Display for AgentThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for AgentThreadId {
    type Error = Error;

    fn try_from(id: String) -> Result<Self> {
        Self::new(id)
    }
}

impl From<AgentThreadId> for String {
    fn from(id: AgentThreadId) -> Self {
        id.0
    }
}

/// Where an agent thread is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Waiting for a free running-agent slot.
    Queued,
    /// Creating and preparing the worktree.
    Preparing,
    Running,
    /// Waiting for the agent's usage limit to reset.
    Waiting,
    /// The agent stopped; see [`Ending`].
    Finished,
    /// Done with: the worktree is removed and the thread is read-only.
    Completed,
}

impl State {
    /// Queued, Preparing, Running or Waiting: a supervisor should be alive.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Preparing | Self::Running | Self::Waiting
        )
    }
}

/// How a finished (or completed) agent thread ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ending {
    /// The branch has commits, and a local pull request shows them.
    PullRequest,
    /// The agent answered without changing anything.
    Answer,
    Failed,
    /// Stopped by the user.
    Stopped,
}

/// Where an agent thread started: a branch and its tip at the time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Start {
    pub branch: String,
    /// Full hexadecimal id, the branch's tip when the thread was started.
    pub commit: String,
}

/// Why a turn was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnKind {
    /// The thread's first prompt.
    Start,
    /// The user replied in the thread view.
    Reply,
    /// Review comments sent from the thread's local pull request.
    Review,
    /// Retry after a failure or a stop.
    Retry,
    /// Continue after the usage limit reset (made by the supervisor).
    Resume,
}

/// One prompt sent to the agent, and how its run went.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub kind: TurnKind,
    pub prompt: String,
    pub requested: Timestamp,
    /// Whether it continues the thread's conversation (`--resume`).
    pub resume: bool,
    pub started: Option<Timestamp>,
    pub ended: Option<Timestamp>,
    /// File name in `runs/`.
    pub transcript: Option<String>,
    pub result: Option<TurnResult>,
}

/// How a turn's run ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnResult {
    pub success: bool,
    /// The agent's final message ("" if none).
    pub message: String,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    /// Tool uses during the turn.
    pub steps: u32,
    pub exit_code: Option<i32>,
    pub stopped: bool,
    /// Hosts the sandbox refused, if it could tell.
    #[serde(default)]
    pub blocked_hosts: Vec<String>,
}

/// What an agent thread is doing now, for live lanes. Written at most once
/// a second.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    /// Tool name such as `Bash`, or `Setup` while preparing.
    pub tool: Option<String>,
    /// First line of what the tool works on, or the preparing step.
    pub detail: String,
    /// Tool uses across all turns so far.
    pub steps: u32,
    pub updated: Timestamp,
}

/// The step of the supervisor's pipeline that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Queue,
    Worktree,
    Prepare,
    Sandbox,
    Agent,
    Commit,
    PullRequest,
    Supervisor,
}

/// Why an agent thread failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub step: Step,
    /// Said for the user; may span lines (git's or the agent's own words).
    pub message: String,
    pub at: Timestamp,
}

/// The supervisor process running an agent thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorInfo {
    pub pid: u32,
    /// The agent's (or setup command's) process group while one runs.
    pub child_group: Option<u32>,
    pub started: Timestamp,
}

/// What GitEZ recorded about a thread's worktree when it created it: the
/// files git trusts to find the repository. The sandbox keeps the agent
/// from changing them, and GitEZ checks them against this record before it
/// runs anything in the worktree outside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    /// The worktree's own git dir, `<common dir>/worktrees/<name>`.
    pub git_dir: PathBuf,
    /// The content of `<worktree>/.git`.
    pub dot_git: String,
    /// The content of `<git_dir>/commondir`.
    pub commondir: String,
    /// The content of `<git_dir>/gitdir`.
    pub gitdir: String,
    /// The content of `<git_dir>/config.worktree`, if git made one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_worktree: Option<String>,
}

/// What the supervisor found in the worktree at the end of the thread's
/// last run, from `git status` run inside the sandbox. The paths are the
/// agent's: untrusted text, to show and never to act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uncommitted {
    pub checked: Timestamp,
    /// Changed, deleted and untracked paths (0: the worktree is clean).
    pub count: u32,
    /// The first few of them, as `git status --porcelain` printed them.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Why GitEZ's own commit of the changes the agent left failed, when
    /// it tried (git's or a hook's words).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_error: Option<String>,
}

/// A prompt given to a coding agent working on its own branch in its own
/// worktree, and the conversation that follows: one `thread.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentThread {
    pub id: AgentThreadId,
    pub title: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub state: State,
    /// Set when `state` is Finished or Completed.
    pub ending: Option<Ending>,
    pub created: Timestamp,
    pub updated: Timestamp,
    pub start: Start,
    /// `agent/<id>`.
    pub branch: String,
    pub worktree: PathBuf,
    /// Whether the worktree was prepared (includes copied, caches cloned,
    /// setup command run).
    pub prepared: bool,
    pub network: NetworkProfile,
    /// Names only; values are in the keychain.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// The agent's conversation, once the agent confirmed it started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The local pull request this thread created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<u32>,
    #[serde(default)]
    pub turns: Vec<Turn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<Activity>,
    /// While Waiting: when the usage limit resets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_until: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Failure>,
    /// Things the user should know that did not fail the thread.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// The supervisor while one runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor: Option<SupervisorInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<Timestamp>,
    /// The worktree's git files as GitEZ made them; `None` before the
    /// worktree exists (and for threads from before GitEZ recorded it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_record: Option<WorktreeRecord>,
    /// Uncommitted changes in the worktree at the end of the last run;
    /// `None` when unknown (never checked, or the check failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncommitted: Option<Uncommitted>,
}

/// What a new agent thread starts with (the GUI fills it in).
#[derive(Debug, Clone)]
pub struct NewAgentThread {
    pub prompt: String,
    pub agent: String,
    pub model: Option<String>,
    pub start: Start,
    pub network: NetworkProfile,
    pub secrets: Vec<String>,
}

/// What a retry will do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryPlan {
    /// The agent never started: run the first prompt again in a new
    /// conversation.
    RunAgain,
    /// The agent had started: continue its conversation.
    Continue,
    /// Only the steps after the agent failed (commit, pull request).
    Finish,
}

/// The prompt a Retry or Resume turn sends when it continues the
/// conversation.
pub const CONTINUE_PROMPT: &str =
    "GitEZ stopped your last run before it finished. Continue the task where you left off.";

const STILL_WORKING: &str = "The agent is still working on this thread.";
const COMPLETED: &str = "This thread is completed.";
const NEVER_STARTED: &str = "Retry this thread first: the agent never started.";

impl AgentThread {
    /// The newest turn's final message that is not empty.
    pub fn final_message(&self) -> Option<&str> {
        self.turns
            .iter()
            .rev()
            .filter_map(|turn| turn.result.as_ref())
            .map(|result| result.message.as_str())
            .find(|message| !message.is_empty())
    }

    /// Whether the worktree had uncommitted changes at the end of the last
    /// run: `None` when unknown.
    pub fn has_uncommitted(&self) -> Option<bool> {
        self.uncommitted
            .as_ref()
            .map(|uncommitted| uncommitted.count > 0)
    }

    /// Whether GitEZ tried to commit the changes the agent left and
    /// failed, so they are still uncommitted in the worktree.
    pub fn leftover_commit_failed(&self) -> bool {
        self.uncommitted
            .as_ref()
            .is_some_and(|uncommitted| uncommitted.count > 0 && uncommitted.commit_error.is_some())
    }

    /// The turn the supervisor should run next: the last one with
    /// `started == None`.
    pub fn pending_turn(&self) -> Option<usize> {
        self.turns.iter().rposition(|turn| turn.started.is_none())
    }

    /// Refuses a change unless the thread is Finished.
    fn check_finished(&self) -> Result<()> {
        match self.state {
            State::Finished => Ok(()),
            State::Completed => Err(Error::refused(COMPLETED)),
            _ => Err(Error::refused(STILL_WORKING)),
        }
    }

    /// Adds a Reply or Review turn: allowed only when Finished and
    /// `session_id` is set; sets state Queued, clears `ending`, `error`,
    /// `waiting_until`. Err(Refused) otherwise.
    pub fn request_turn(&mut self, kind: TurnKind, prompt: String, at: Timestamp) -> Result<()> {
        if !matches!(kind, TurnKind::Reply | TurnKind::Review) {
            return Err(Error::refused(
                "Only a reply or review comments can continue a thread.",
            ));
        }
        self.check_finished()?;
        if self.session_id.is_none() {
            return Err(Error::refused(NEVER_STARTED));
        }
        self.turns.push(Turn::requested(kind, prompt, true, at));
        self.queue();
        Ok(())
    }

    /// Prepares a retry: allowed only when Finished with ending Failed or
    /// Stopped. Sets `network`, state Queued, clears `ending` and `error`;
    /// adds a Retry turn unless the plan is Finish (RunAgain: the first
    /// prompt, resume false; Continue: CONTINUE_PROMPT, resume true — or,
    /// when the last turn is a reply or review that never started, that
    /// turn's prompt, so the user's words are not lost).
    pub fn request_retry(&mut self, network: NetworkProfile, at: Timestamp) -> Result<RetryPlan> {
        self.check_finished()?;
        if !matches!(self.ending, Some(Ending::Failed | Ending::Stopped)) {
            return Err(Error::refused(
                "Only a thread that failed or was stopped can be retried.",
            ));
        }
        let failed_after_agent = self.ending == Some(Ending::Failed)
            && self
                .error
                .as_ref()
                .is_some_and(|error| matches!(error.step, Step::Commit | Step::PullRequest));
        let plan = if failed_after_agent {
            RetryPlan::Finish
        } else if self.session_id.is_some() {
            RetryPlan::Continue
        } else {
            RetryPlan::RunAgain
        };
        match plan {
            RetryPlan::Finish => {}
            RetryPlan::RunAgain => {
                let prompt = self
                    .turns
                    .first()
                    .map(|turn| turn.prompt.clone())
                    .unwrap_or_default();
                self.turns
                    .push(Turn::requested(TurnKind::Retry, prompt, false, at));
            }
            RetryPlan::Continue => {
                let prompt = match self.turns.last() {
                    Some(turn)
                        if turn.started.is_none()
                            && matches!(turn.kind, TurnKind::Reply | TurnKind::Review) =>
                    {
                        turn.prompt.clone()
                    }
                    _ => CONTINUE_PROMPT.to_owned(),
                };
                self.turns
                    .push(Turn::requested(TurnKind::Retry, prompt, true, at));
            }
        }
        self.network = network;
        self.queue();
        Ok(plan)
    }

    /// Back to Queued for the supervisor to pick up.
    fn queue(&mut self) {
        self.state = State::Queued;
        self.ending = None;
        self.error = None;
        self.waiting_until = None;
    }

    /// State Finished, ending Failed, `error` set, `supervisor` cleared.
    /// Also clears `activity` and `waiting_until`, and ends a turn that
    /// started and did not end, so its run time stops growing.
    pub fn fail(&mut self, step: Step, message: impl Into<String>, at: Timestamp) {
        self.state = State::Finished;
        self.ending = Some(Ending::Failed);
        self.error = Some(Failure {
            step,
            message: message.into(),
            at,
        });
        self.supervisor = None;
        self.activity = None;
        self.waiting_until = None;
        for turn in &mut self.turns {
            if turn.started.is_some() && turn.ended.is_none() {
                turn.ended = Some(at);
            }
        }
    }

    /// State Completed, `completed` set. Allowed when Finished.
    pub fn mark_completed(&mut self, at: Timestamp) -> Result<()> {
        self.check_finished()?;
        self.state = State::Completed;
        self.completed = Some(at);
        Ok(())
    }

    /// Total tool uses (from `activity.steps`, else the turns' sums).
    pub fn steps(&self) -> u32 {
        match &self.activity {
            Some(activity) => activity.steps,
            None => self
                .turns
                .iter()
                .filter_map(|turn| turn.result.as_ref())
                .map(|result| result.steps)
                .sum(),
        }
    }

    /// Sum of the turns' run times so far. A turn still running counts up
    /// to `now` while the thread is active.
    pub fn run_time(&self, now: Timestamp) -> SignedDuration {
        self.turns
            .iter()
            .filter_map(|turn| {
                let started = turn.started?;
                let ended = turn.ended.or(self.state.is_active().then_some(now))?;
                Some(ended.duration_since(started).max(SignedDuration::ZERO))
            })
            .fold(SignedDuration::ZERO, |total, run| total.saturating_add(run))
    }
}

impl Turn {
    fn requested(kind: TurnKind, prompt: String, resume: bool, at: Timestamp) -> Self {
        Self {
            kind,
            prompt,
            requested: at,
            resume,
            started: None,
            ended: None,
            transcript: None,
            result: None,
        }
    }

    /// A turn asked for at `at` that has not run yet.
    pub(crate) fn first(prompt: String, at: Timestamp) -> Self {
        Self::requested(TurnKind::Start, prompt, false, at)
    }
}
