//! Coding agents GitEZ sends a review prompt to. An [`Agent`] runs as a
//! child process in the repository's working tree and reports what it does
//! as lines of output; [`Agent::parse`] turns each line into an
//! [`AgentEvent`] to show. Claude Code ([`ClaudeCode`]) is the first agent.
//!
//! Everything here blocks: run it off the UI thread.

mod claude_code;
mod session;

use std::path::{Path, PathBuf};

pub use claude_code::ClaudeCode;
pub use session::{Outcome, Session, Stopper};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{name} is not installed: no `{program}` was found on the PATH or in {searched}")]
    NotInstalled {
        name: &'static str,
        program: &'static str,
        searched: String,
    },
    #[error("could not start {program}")]
    Start {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not read the agent's output")]
    Read(#[source] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A coding agent that can be sent a prompt.
pub trait Agent: Send + Sync {
    /// Stored with a pull request's prompt options, such as `claude-code`.
    fn id(&self) -> &'static str;
    /// Shown to the user, such as "Claude Code".
    fn name(&self) -> &'static str;
    /// Starts working on `prompt` in `dir`, the repository's working tree.
    fn start(&self, prompt: &str, dir: &Path) -> Result<Session>;
    /// What a line of the agent's output reports, if anything worth
    /// showing.
    fn parse(&self, line: &str) -> Option<AgentEvent>;
}

/// The agent with id `id`, if GitEZ knows it.
pub fn agent(id: &str) -> Option<Box<dyn Agent>> {
    match id {
        ClaudeCode::ID => Some(Box::new(ClaudeCode::new())),
        _ => None,
    }
}

/// Something an agent did, as shown while it works.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// It started, with this model if known.
    Started { model: Option<String> },
    /// It wrote this text.
    Text(String),
    /// It used a tool: its name and what it was used on, such as a command
    /// or a file.
    Tool { name: String, detail: String },
    /// A tool failed or was refused, with the first line of why.
    ToolFailed(String),
    /// It finished.
    Finished(Finished),
}

/// How an agent run ended, as the agent reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    pub success: bool,
    /// Its final message.
    pub text: String,
    pub cost_usd: Option<f64>,
    pub turns: Option<u64>,
    pub duration_ms: Option<u64>,
}
