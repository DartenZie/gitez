//! Coding agents GitEZ runs. An [`Agent`] does not start anything itself:
//! it builds the [`CommandSpec`] for one run, says what a sandbox must allow
//! for it to work ([`AgentNeeds`]), and turns each line of its output into an
//! [`AgentEvent`]. [`AgentProcess`] runs such a command, possibly wrapped by
//! a sandbox first. The [`Registry`] lists the agents GitEZ knows; Claude
//! Code ([`ClaudeCode`]) is the first.
//!
//! Everything here blocks: run it off the UI thread.

mod claude_code;
mod command;
mod login_shell;
mod process;
mod registry;
mod test_command;

pub use claude_code::{ClaudeCode, ConfigDir, ConfigDirSource};
pub use command::CommandSpec;
pub use login_shell::login_shell_var;
pub use process::{AgentProcess, Outcome, Stopper};
pub use registry::{AgentStatus, Availability, Registry};
pub use test_command::detect_test_command;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "{name} is not installed: no program named {program} was found on the PATH or in {searched}"
    )]
    NotInstalled {
        name: &'static str,
        program: &'static str,
        searched: String,
    },
    #[error("{path} is not a program GitEZ can run")]
    NotAProgram { path: std::path::PathBuf },
    #[error("could not start {program}")]
    Start {
        program: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not read the agent's output")]
    Read(#[source] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// How an agent entry in Settings configures an agent. Converted by the
/// supervisor and the GUI from `gitez_settings::AgentEntry`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentConfig {
    /// The binary; `None`: look on the PATH and in the usual install places.
    pub program: Option<std::path::PathBuf>,
    /// Claude Code: `CLAUDE_CONFIG_DIR`. `None`: the agent's default, which
    /// for Claude Code is resolved as [`ClaudeCode::config_dir`] says.
    pub config_dir: Option<std::path::PathBuf>,
    pub env: Vec<(String, String)>,
    /// Added after GitEZ's own arguments.
    pub args: Vec<String>,
}

/// Which conversation a run belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conversation<'a> {
    /// A new conversation with this id (a UUID the supervisor made).
    New { session_id: &'a str },
    /// Continue this conversation.
    Resume { session_id: &'a str },
}

impl<'a> Conversation<'a> {
    /// The conversation's id, new or continued.
    pub fn session_id(&self) -> &'a str {
        match *self {
            Self::New { session_id } | Self::Resume { session_id } => session_id,
        }
    }
}

/// One run of an agent.
#[derive(Debug, Clone, Copy)]
pub struct RunSpec<'a> {
    pub prompt: &'a str,
    /// Where the agent works: the thread's worktree.
    pub cwd: &'a std::path::Path,
    /// A model alias such as `haiku`; `None`: the agent's default.
    pub model: Option<&'a str>,
    pub conversation: Conversation<'a>,
    /// Appended to the agent's system prompt, if it has one.
    pub system_prompt: Option<&'a str>,
}

/// What the sandbox must allow for the agent itself to work.
///
/// Paths are absolute. On macOS an entry may contain `*` (any characters
/// but `/`) or `**` (anything) glob patterns, as `srt` reads them, where
/// the agent's own file names are not known in advance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentNeeds {
    /// Folders (and their contents) the agent writes: its per-project and
    /// per-session state, caches, temporary files.
    pub write_dirs: Vec<std::path::PathBuf>,
    /// Single files the agent writes.
    pub write_files: Vec<std::path::PathBuf>,
    /// Hosts it talks to (its API, login refresh, telemetry it cannot run
    /// without).
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStatus {
    SignedIn,
    SignedOut,
    /// Could not tell, with why.
    Unknown(String),
}

/// A model the agent can be asked for: (alias, label). The empty alias is
/// "Agent default".
pub type ModelChoice = (&'static str, &'static str);

/// A coding agent GitEZ can run.
pub trait Agent: Send + Sync {
    /// Stored in thread.json and settings, such as `claude-code`.
    fn id(&self) -> &'static str;
    /// Shown to the user, such as "Claude Code".
    fn name(&self) -> &'static str;
    /// The first entry is ("", "Agent default").
    fn models(&self) -> &'static [ModelChoice];
    /// The binary to run, or why there is none.
    fn locate(&self, config: &AgentConfig) -> Result<std::path::PathBuf>;
    /// The command for one run. Does not start anything.
    fn command(&self, config: &AgentConfig, run: &RunSpec<'_>) -> Result<CommandSpec>;
    /// What the sandbox must allow for any run of the agent.
    fn needs(&self, config: &AgentConfig) -> AgentNeeds;
    /// What the sandbox must allow for this run: [`Agent::needs`] plus what
    /// depends on the run, such as Claude Code's per-project and
    /// per-session folders. Sandboxed runs use this one.
    fn needs_for_run(&self, config: &AgentConfig, run: &RunSpec<'_>) -> AgentNeeds {
        let _ = run;
        self.needs(config)
    }
    /// What a line of the agent's output reports, if anything.
    fn parse(&self, line: &str) -> Option<AgentEvent>;
    /// Asks the agent itself; blocks for up to a few seconds. Never returns
    /// or stores the login itself.
    fn login_status(&self, config: &AgentConfig) -> LoginStatus;
}

/// Something an agent did, as shown while it works.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// The run started (Claude Code: the `system`/`init` event).
    Started {
        session_id: Option<String>,
        model: Option<String>,
    },
    /// It wrote this text.
    Text(String),
    /// It used a tool: its name and what it was used on, such as a command
    /// or a file.
    Tool { name: String, detail: String },
    /// A tool failed or was refused, with the first line of why.
    ToolFailed(String),
    /// The agent's usage limit was hit (Claude Code: `rate_limit_event`
    /// with `rate_limit_info.status == "rejected"`).
    UsageLimit(UsageLimit),
    /// It finished.
    Finished(Finished),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageLimit {
    /// When it resets, in seconds since the Unix epoch, if the agent said.
    pub resets_at: Option<i64>,
}

/// How an agent run ended, as the agent reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    pub success: bool,
    /// Its final message.
    pub text: String,
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
    pub turns: Option<u64>,
    pub duration_ms: Option<u64>,
    /// Set when the run ended because of the usage limit.
    pub usage_limit: Option<UsageLimit>,
}
