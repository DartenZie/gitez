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
    #[error("{name} cannot be given images")]
    NoImages { name: &'static str },
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

/// An image given to the agent with the prompt: its bytes as they are sent,
/// in a format [`Agent::accepts_images`] promised the model reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageInput<'a> {
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub media_type: &'a str,
    pub data: &'a [u8],
}

/// One run of an agent.
#[derive(Debug, Clone, Copy)]
pub struct RunSpec<'a> {
    /// May be empty when the run has images.
    pub prompt: &'a str,
    /// Sent with the prompt, in order. Empty for most runs, which are then
    /// exactly as they were before agents took images.
    pub images: &'a [ImageInput<'a>],
    /// Where the agent works: the thread's worktree, or for a Discuss
    /// thread the user's working tree.
    pub cwd: &'a std::path::Path,
    /// Whether `cwd` is the user's own folder, where their own sessions of
    /// the agent run too. [`Agent::needs_for_run`] then lets the run write
    /// only this conversation's files among what the agent keeps per
    /// folder, never what those other sessions wrote or will read.
    pub shared_folder: bool,
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
    /// Folders that must exist before the run and that the agent cannot
    /// make itself, their parent not being writable. Whoever starts the
    /// run makes them.
    pub create_dirs: Vec<std::path::PathBuf>,
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

/// A model the agent can be asked for: (alias, label).
pub type ModelChoice = (&'static str, &'static str);

/// A host an agent contacts on its own that it does not need to do its
/// work. The sandbox keeps blocking it; a blocked request to it is recorded
/// but is not a problem ([`BlockedHosts`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionalHost {
    /// The exact host name, such as `mcp-proxy.anthropic.com`.
    pub host: &'static str,
    /// What the agent uses it for, in a few words for the user, such as
    /// "Claude Code's telemetry, not needed".
    pub purpose: &'static str,
}

/// The hosts a sandbox refused in a run, split into those that may have
/// mattered and those the agent does not need ([`Agent::optional_hosts`]).
/// Every count, colour, suggestion and hint about blocked requests uses
/// [`BlockedHosts::problems`] only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockedHosts<'a> {
    /// Hosts the run may have needed, in the order they were blocked.
    pub problems: Vec<&'a str>,
    /// Hosts the agent contacts on its own and does not need, each with
    /// what it is for.
    pub optional: Vec<(&'a str, &'static str)>,
}

impl<'a> BlockedHosts<'a> {
    /// Splits `hosts` by `agent`'s [`Agent::optional_hosts`]. With no
    /// agent (one GitEZ no longer knows) every host counts as a problem.
    pub fn classify(agent: Option<&dyn Agent>, hosts: &'a [String]) -> Self {
        let mut blocked = Self::default();
        for host in hosts {
            match agent.and_then(|agent| agent.optional_host(host)) {
                Some(optional) => blocked.optional.push((host, optional.purpose)),
                None => blocked.problems.push(host),
            }
        }
        blocked
    }
}

/// A coding agent GitEZ can run.
pub trait Agent: Send + Sync {
    /// Stored in thread.json and settings, such as `claude-code`.
    fn id(&self) -> &'static str;
    /// Shown to the user, such as "Claude Code".
    fn name(&self) -> &'static str;
    /// The models offered, in the order shown.
    fn models(&self) -> &'static [ModelChoice];
    /// The alias used when Settings name no model: the first of
    /// [`Agent::models`], or empty (no model asked for) without any.
    fn default_model(&self) -> &'static str {
        self.models().first().map_or("", |(alias, _)| alias)
    }
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
    /// Hosts the agent contacts on its own that it does not need to do its
    /// work, such as its own telemetry. They are never allowed by the
    /// sandbox: a blocked request to one is recorded, shown apart and
    /// muted, and never counted as a problem. Exact host names only.
    fn optional_hosts(&self) -> &'static [OptionalHost] {
        &[]
    }
    /// The entry of [`Agent::optional_hosts`] that is `host`, if any.
    /// Host names are compared without regard to ASCII case.
    fn optional_host(&self, host: &str) -> Option<&'static OptionalHost> {
        self.optional_hosts()
            .iter()
            .find(|optional| optional.host.eq_ignore_ascii_case(host))
    }
    /// Whether `model` (an alias as in [`Agent::models`]; `None` or empty:
    /// the agent's default) reads images given with the prompt
    /// ([`RunSpec::images`]). The UI asks this before it takes an image;
    /// an agent that says no is never given one.
    fn accepts_images(&self, model: Option<&str>) -> bool {
        let _ = model;
        false
    }
    /// A line of the agent's output as GitEZ keeps it in the transcript:
    /// the line itself, or a copy without bulky data such as the bytes of
    /// an image the agent echoes. Lines are parsed before this is applied.
    fn transcript_line<'l>(&self, line: &'l str) -> std::borrow::Cow<'l, str> {
        std::borrow::Cow::Borrowed(line)
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
