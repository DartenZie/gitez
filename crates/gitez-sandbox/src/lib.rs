//! Sandboxes GitEZ runs coding agents in. A sandbox wraps an agent's whole
//! process tree so that it can write only where its policy allows and reach
//! only the hosts its [`NetworkProfile`] allows.
//!
//! Everything here blocks: run it off the UI thread.

mod bwrap;
mod locate;
mod network;
mod passthrough;
mod policy;
mod seatbelt;
mod srt;

pub use network::NetworkProfile;
pub use passthrough::Passthrough;
pub use policy::{Network, Policy, WritePath};
pub use srt::{SRT_PACKAGE, Srt};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Unavailable(String),
    #[error("could not write the sandbox profile {path}")]
    Profile {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not install the sandbox: {0}")]
    Install(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Ready to wrap commands.
    Ready {
        program: std::path::PathBuf,
        version: Option<String>,
    },
    /// Cannot run; `fix` tells the user what to do, such as
    /// "Install the sandbox in Settings (needs Node.js)".
    Missing { fix: String },
}

pub trait Sandbox: Send + Sync {
    /// Shown in Settings, such as "srt (Anthropic sandbox runtime)".
    fn name(&self) -> &'static str;
    fn status(&self) -> Status;
    /// `command` wrapped so that it runs under `policy`. The wrapped command
    /// keeps `command`'s env, cwd and stdin; it must run the whole process
    /// tree inside the sandbox, and an interrupt (SIGINT) or kill sent to its
    /// process group must reach the wrapped program.
    fn wrap(&self, command: gitez_agent::CommandSpec, policy: &Policy) -> Result<Wrapped>;
}

/// A wrapped command, and what it needs until it exits (such as a
/// temporary profile file).
pub struct Wrapped {
    pub command: gitez_agent::CommandSpec,
    /// Dropped after the process exits.
    pub guard: Box<dyn std::any::Any + Send>,
    blocked: Box<dyn Fn() -> Vec<String> + Send>,
}

impl Wrapped {
    /// A wrapped command whose sandbox cannot tell which hosts it refused.
    /// For other implementations of [`Sandbox`], such as fakes in tests.
    pub fn new(command: gitez_agent::CommandSpec, guard: Box<dyn std::any::Any + Send>) -> Self {
        Self::with_blocked(command, guard, Box::new(Vec::new))
    }

    pub(crate) fn with_blocked(
        command: gitez_agent::CommandSpec,
        guard: Box<dyn std::any::Any + Send>,
        blocked: Box<dyn Fn() -> Vec<String> + Send>,
    ) -> Self {
        Self {
            command,
            guard,
            blocked,
        }
    }

    /// Hosts the sandbox refused during the run, if it can tell (best
    /// effort; empty when unknown). Call after the process exited.
    pub fn blocked_hosts(&self) -> Vec<String> {
        (self.blocked)()
    }
}

impl std::fmt::Debug for Wrapped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wrapped")
            .field("command", &self.command)
            .finish_non_exhaustive()
    }
}

/// The sandbox GitEZ uses on this machine: `srt` from the PATH, else from
/// `<data_dir>/sandbox` (where [`Srt::install`] puts it). Always returns a
/// sandbox; check `status()`.
pub fn platform(data_dir: &std::path::Path) -> std::sync::Arc<dyn Sandbox> {
    std::sync::Arc::new(Srt::new(data_dir))
}
