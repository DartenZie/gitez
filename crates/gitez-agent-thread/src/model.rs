//! What an agent thread holds, as stored in its `thread.json`, and the
//! changes its state allows.

use std::path::{Path, PathBuf};

use gitez_sandbox::NetworkProfile;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, ScratchDirs, TurnImage};

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

/// What an agent thread is for, chosen when it is started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// The agent changes files on its own branch in its own worktree, and
    /// its commits become a local pull request.
    #[default]
    Implement,
    /// The agent reads the user's working tree and answers: the sandbox
    /// lets it change nothing there. No branch, no worktree, no pull
    /// request.
    Discuss,
}

impl Mode {
    pub const ALL: [Mode; 2] = [Self::Discuss, Self::Implement];

    /// "Implement", "Discuss".
    pub fn label(self) -> &'static str {
        match self {
            Self::Implement => "Implement",
            Self::Discuss => "Discuss",
        }
    }

    pub fn is_implement(&self) -> bool {
        *self == Self::Implement
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
    /// GitEZ asked a Discuss thread for a brief ([`BRIEF_PROMPT`]) to start
    /// an Implement thread with.
    Brief,
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
    /// Sent with the prompt, in order; files in `images/`
    /// ([`crate::Store::image_path`]). A retry that sends the prompt again
    /// lists them again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<TurnImage>,
    /// Set when the agent of an Auto thread chose to implement: it
    /// answered with [`HANDOFF_MARKER`] and a brief (the result's message,
    /// without the marker), and GitEZ starts an Implement thread from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<Handoff>,
}

/// What became of a brief an Auto thread's agent answered with
/// ([`Turn::handoff`]). GitEZ claims it before it starts the Implement
/// thread, so each brief starts at most one, also after a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    /// When GitEZ took the brief to start an Implement thread; `None`
    /// until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed: Option<Timestamp>,
    /// The Implement thread started from the brief.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<AgentThreadId>,
    /// Why no Implement thread was started, for the user: the brief is
    /// left for "Implement…".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
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

/// The thread's branch, deleted when the thread was completed: where it
/// was, so that `git branch <branch> <commit>` brings it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletedBranch {
    /// Full hexadecimal id of the branch's tip when it was deleted.
    pub commit: String,
    pub at: Timestamp,
}

/// The Claude account a thread runs with: a configuration folder, which
/// is where Claude Code keeps its login and the thread's conversation, and
/// the name Settings gave it. Recorded when the thread is created; every
/// later turn runs with it, whatever Settings or the repository choose
/// then.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Account {
    /// A label: the account is found again by its folder, so renaming it
    /// in Settings does not lose its threads.
    pub name: String,
    /// The folder, as `CLAUDE_CONFIG_DIR` is set to it (verbatim, never
    /// made canonical). `None`: Claude Code's default folder with the
    /// variable unset, which is another login than the same folder named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
}

impl Account {
    /// The name of the account GitEZ always has: the folder GitEZ found
    /// before it had accounts (the agent entry's, else `CLAUDE_CONFIG_DIR`
    /// in GitEZ's environment or the login shell, else Claude Code's
    /// default). No account in Settings can take this name.
    pub const DEFAULT_NAME: &str = "Default";

    /// Whether this is the account GitEZ always has
    /// ([`Account::DEFAULT_NAME`]), rather than one added in Settings.
    pub fn is_default(&self) -> bool {
        self.name == Self::DEFAULT_NAME
    }

    /// Whether `name` is [`Account::DEFAULT_NAME`], in any case.
    pub fn is_default_name(name: &str) -> bool {
        name.trim().eq_ignore_ascii_case(Self::DEFAULT_NAME)
    }
}

/// A prompt given to a coding agent, and the conversation that follows: one
/// `thread.json`. An Implement thread works on its own branch in its own
/// worktree; a Discuss thread reads the user's working tree ([`Mode`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentThread {
    pub id: AgentThreadId,
    /// From the prompt ([`crate::title_from_prompt`]) until the supervisor
    /// creates the thread's pull request, then the pull request's title.
    pub title: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The account every turn runs with. `None` only in a record from
    /// before accounts: it runs with the default account, which the
    /// supervisor then records. A record naming another account is
    /// written as version 2 ([`AgentThread::format_version`]), so an older
    /// GitEZ, which would drop the field and run the next turn with the
    /// default account, refuses it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<Account>,
    #[serde(default, skip_serializing_if = "Mode::is_implement")]
    pub mode: Mode,
    /// A Discuss thread started as Auto: its agent may answer with a brief
    /// behind [`HANDOFF_MARKER`], and GitEZ then starts an Implement
    /// thread from it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto: bool,
    pub state: State,
    /// Set when `state` is Finished or Completed.
    pub ending: Option<Ending>,
    pub created: Timestamp,
    pub updated: Timestamp,
    /// For a Discuss thread: what was checked out when it started.
    pub start: Start,
    /// `agent/<id>`; `None` for a Discuss thread, which has no branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// `None` for a Discuss thread, which has no worktree of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
    /// A Discuss thread only: the working tree the agent reads, the one
    /// the user had open when they started it (the main one or a linked
    /// one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_dir: Option<PathBuf>,
    /// A Discuss thread only: its folders in GitEZ's data folder (an
    /// Implement thread's follow from where its worktree is).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch: Option<ScratchDirs>,
    /// The Discuss thread whose brief started this thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<AgentThreadId>,
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
    /// Set when completing the thread deleted its branch; `None` while the
    /// branch is kept (and for threads from before GitEZ recorded it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_branch: Option<DeletedBranch>,
}

/// What a new agent thread starts with (the GUI fills it in).
#[derive(Debug, Clone)]
pub struct NewAgentThread {
    pub prompt: String,
    pub agent: String,
    pub model: Option<String>,
    /// Resolved from the repository's choice when the thread is created;
    /// `None` leaves it to the supervisor (the default account).
    pub account: Option<Account>,
    pub mode: Mode,
    /// For [`Mode::Discuss`]: started as Auto ([`AgentThread::auto`]).
    pub auto: bool,
    pub start: Start,
    /// For [`Mode::Discuss`]: the working tree the agent reads. Unused
    /// for an Implement thread.
    pub work_dir: Option<PathBuf>,
    /// The Discuss thread whose brief this thread starts with.
    pub origin: Option<AgentThreadId>,
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

/// The prompt of a [`TurnKind::Brief`] turn: what "Implement…" asks a
/// Discuss thread for. The answer becomes the first prompt of an Implement
/// thread, whose agent has not seen the discussion.
pub const BRIEF_PROMPT: &str = "\
Write a brief for an engineer who will implement what we discussed. They have not seen this \
conversation and start with a fresh context, in a fresh checkout of the repository without \
uncommitted changes, so the brief must stand on its own.

Begin with a title for the change as a level-1 Markdown heading (\"# <title>\") in the \
imperative mood, like a good commit subject, about 60 characters at most. Then, under short \
headings: the goal and why; what we decided, with the reasons that matter; constraints and \
conventions to follow; the files and code to look at, as paths; how to verify the result; and \
what is out of scope. Leave out anything we discussed and dropped. If something needed is still \
undecided, list it under \"Open questions\".

Reply with the brief only: no preamble and no closing remarks.";

/// The first line of an Auto thread's answer that is a brief to implement
/// rather than an answer ([`AgentThread::auto`]). Never shown, and never
/// part of the brief sent on.
pub const HANDOFF_MARKER: &str = "[gitez:implement]";

/// Whether `line` is [`HANDOFF_MARKER`], spaces and backticks around it
/// allowed.
fn is_marker(line: &str) -> bool {
    line.trim().trim_matches('`').trim() == HANDOFF_MARKER
}

/// The brief in `message` if its first line that is not blank is
/// [`HANDOFF_MARKER`]: the rest, trimmed. `None` when the message is not
/// marked or nothing follows the marker.
pub fn split_handoff(message: &str) -> Option<&str> {
    let rest = message.trim_start();
    let (line, rest) = rest.split_once('\n').unwrap_or((rest, ""));
    if !is_marker(line) {
        return None;
    }
    let brief = rest.trim();
    (!brief.is_empty()).then_some(brief)
}

/// `message` without a [`HANDOFF_MARKER`] line at its start, as shown.
pub fn without_handoff_marker(message: &str) -> &str {
    let rest = message.trim_start();
    match rest.split_once('\n') {
        Some((line, rest)) if is_marker(line) => rest.trim_start(),
        None if is_marker(rest) => "",
        _ => message,
    }
}

const STILL_WORKING: &str = "The agent is still working on this thread.";
const COMPLETED: &str = "This thread is completed.";
const NEVER_STARTED: &str = "Retry this thread first: the agent never started.";

impl AgentThread {
    /// The format version its `thread.json` is written with: 2 when it
    /// names an account other than the default, which an older GitEZ
    /// would drop; 1 otherwise, so older GitEZ keep reading the rest.
    pub fn format_version(&self) -> u32 {
        match &self.account {
            Some(account) if !account.is_default() => 2,
            _ => 1,
        }
    }

    /// An Implement thread's branch and worktree; `None` for a Discuss
    /// thread, which has neither.
    pub fn own(&self) -> Option<(&str, &Path)> {
        Some((self.branch.as_deref()?, self.worktree.as_deref()?))
    }

    /// The worktree's path as said in a message about it; empty for a
    /// Discuss thread, which has none and gets no such message.
    pub fn worktree_display(&self) -> std::path::Display<'_> {
        self.worktree.as_deref().unwrap_or(Path::new("")).display()
    }

    /// The branch's name as said in a message about it; empty for a
    /// Discuss thread.
    pub fn branch_name(&self) -> &str {
        self.branch.as_deref().unwrap_or_default()
    }

    /// Why this record cannot be used, if its mode and its places do not
    /// go together.
    pub(crate) fn inconsistency(&self) -> Option<&'static str> {
        match self.mode {
            Mode::Implement if self.own().is_none() => {
                Some("an Implement thread without a branch and a worktree")
            }
            Mode::Discuss if self.work_dir.is_none() => {
                Some("a Discuss thread without a working tree")
            }
            Mode::Discuss if self.branch.is_some() || self.worktree.is_some() => {
                Some("a Discuss thread with a branch or a worktree")
            }
            Mode::Implement if self.auto => Some("an Implement thread started as Auto"),
            _ => None,
        }
    }

    /// The last turn's brief, if that turn is a [`TurnKind::Brief`] the
    /// agent answered, or one an Auto thread's agent chose to answer with
    /// ([`Turn::handoff`]): what an Implement thread can start with.
    pub fn brief(&self) -> Option<&str> {
        let turn = self.turns.last()?;
        let result = turn.result.as_ref()?;
        ((turn.kind == TurnKind::Brief || turn.handoff.is_some())
            && result.success
            && !result.message.trim().is_empty())
        .then_some(result.message.as_str())
    }

    /// The turn whose brief GitEZ should start an Implement thread with
    /// now: in an Auto thread that is not completed, the first one the
    /// agent answered with a brief that GitEZ has not claimed yet.
    pub fn pending_handoff(&self) -> Option<usize> {
        if !self.auto || self.mode != Mode::Discuss || self.state == State::Completed {
            return None;
        }
        self.turns.iter().position(|turn| {
            turn.handoff
                .as_ref()
                .is_some_and(|handoff| handoff.claimed.is_none())
                && turn
                    .result
                    .as_ref()
                    .is_some_and(|result| result.success && !result.message.trim().is_empty())
        })
    }

    /// Takes the brief of turn `index` ([`pending_handoff`]) to start an
    /// Implement thread with: marks it claimed and returns it, or
    /// Err(Refused) if it is not pending (claimed already), so each brief
    /// starts at most one thread. Call it under the thread's lock
    /// ([`crate::Store::update`]).
    ///
    /// [`pending_handoff`]: Self::pending_handoff
    pub fn claim_handoff(&mut self, index: usize, at: Timestamp) -> Result<String> {
        if self.pending_handoff() != Some(index) {
            return Err(Error::refused("This brief was handed on already."));
        }
        let turn = &mut self.turns[index];
        let brief = turn
            .result
            .as_ref()
            .map(|result| result.message.trim().to_owned())
            .unwrap_or_default();
        if let Some(handoff) = &mut turn.handoff {
            handoff.claimed = Some(at);
        }
        Ok(brief)
    }

    /// Records what came of the claimed brief of turn `index`: the
    /// Implement thread started from it, or why none was (ignored once a
    /// thread was started: a thread that fails later says so itself).
    pub fn record_handoff(
        &mut self,
        index: usize,
        outcome: std::result::Result<AgentThreadId, String>,
    ) {
        if let Some(handoff) = self
            .turns
            .get_mut(index)
            .and_then(|turn| turn.handoff.as_mut())
        {
            match outcome {
                Ok(id) => handoff.thread = Some(id),
                Err(problem) if handoff.thread.is_none() => handoff.problem = Some(problem),
                Err(_) => {}
            }
        }
    }

    /// The newest turn's final message that is not empty.
    pub fn final_message(&self) -> Option<&str> {
        self.turns
            .iter()
            .rev()
            .filter_map(|turn| turn.result.as_ref())
            .map(|result| result.message.as_str())
            .find(|message| !message.is_empty())
    }

    /// [`final_message`](Self::final_message) as shown apart from the
    /// transcript: without the `# Title` line that gave the thread its
    /// title (see [`crate::message_without_title`]). `None` when nothing
    /// else is left.
    pub fn final_message_body(&self) -> Option<&str> {
        let body = crate::message_without_title(self.final_message()?, &self.title);
        (!body.trim().is_empty()).then_some(body)
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

    /// Adds a Reply or Review turn, or a Discuss thread's Brief turn:
    /// allowed only when Finished and `session_id` is set; sets state
    /// Queued, clears `ending`, `error`, `waiting_until`. Err(Refused)
    /// otherwise.
    pub fn request_turn(&mut self, kind: TurnKind, prompt: String, at: Timestamp) -> Result<()> {
        self.request_turn_with_images(kind, prompt, Vec::new(), at)
    }

    /// [`request_turn`](Self::request_turn) with images sent along, whose
    /// files [`crate::Store::add_images`] wrote.
    pub fn request_turn_with_images(
        &mut self,
        kind: TurnKind,
        prompt: String,
        images: Vec<TurnImage>,
        at: Timestamp,
    ) -> Result<()> {
        match (kind, self.mode) {
            (TurnKind::Reply, _) | (TurnKind::Review, Mode::Implement) => {}
            (TurnKind::Brief, Mode::Discuss) => {}
            (TurnKind::Brief, Mode::Implement) => {
                return Err(Error::refused("Only a Discuss thread can write a brief."));
            }
            (TurnKind::Review, Mode::Discuss) => {
                return Err(Error::refused("A Discuss thread has no pull request."));
            }
            _ => {
                return Err(Error::refused(
                    "Only a reply or review comments can continue a thread.",
                ));
            }
        }
        self.check_finished()?;
        if self.session_id.is_none() {
            return Err(Error::refused(NEVER_STARTED));
        }
        let mut turn = Turn::requested(kind, prompt, true, at);
        turn.images = images;
        self.turns.push(turn);
        self.queue();
        Ok(())
    }

    /// Prepares a retry: allowed only when Finished with ending Failed or
    /// Stopped. Sets `network`, state Queued, clears `ending` and `error`;
    /// adds a Retry turn unless the plan is Finish (RunAgain: the first
    /// prompt, resume false; Continue: CONTINUE_PROMPT, resume true — or,
    /// when the last turn is a reply or review that never started, that
    /// turn's prompt, so the user's words are not lost). A prompt sent again
    /// takes its images along.
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
                let (prompt, images) = self
                    .turns
                    .first()
                    .map(|turn| (turn.prompt.clone(), turn.images.clone()))
                    .unwrap_or_default();
                let mut turn = Turn::requested(TurnKind::Retry, prompt, false, at);
                turn.images = images;
                self.turns.push(turn);
            }
            RetryPlan::Continue => {
                let (prompt, images) = match self.turns.last() {
                    Some(turn)
                        if turn.started.is_none()
                            && matches!(
                                turn.kind,
                                TurnKind::Reply | TurnKind::Review | TurnKind::Brief
                            ) =>
                    {
                        (turn.prompt.clone(), turn.images.clone())
                    }
                    _ => (CONTINUE_PROMPT.to_owned(), Vec::new()),
                };
                let mut turn = Turn::requested(TurnKind::Retry, prompt, true, at);
                turn.images = images;
                self.turns.push(turn);
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
            images: Vec::new(),
            handoff: None,
        }
    }

    /// A turn asked for at `at` that has not run yet.
    pub(crate) fn first(prompt: String, at: Timestamp) -> Self {
        Self::requested(TurnKind::Start, prompt, false, at)
    }
}
