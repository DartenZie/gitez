//! The supervisor: one process per agent thread that waits in the global
//! queue, prepares the thread's worktree, runs the agent inside the sandbox,
//! writes its transcript, and commits and opens a local pull request when
//! the agent is done. It outlives the GUI, which launches it, watches its
//! files and can stop it.
//!
//! ```text
//! <git common dir>/gitez/threads/<id>/
//!   supervisor.lock   held (flock) by the live supervisor
//!   supervisor.log    the supervisor's standard output and error
//!   setup.log         output of the last setup command
//! <data dir>/queue/slots/<n>.lock                        running-agent slots
//! <data dir>/queue/tickets/<nanos>-<repo key>-<id>.ticket threads waiting
//! <data dir>/tmp/<repo key>-<id>/                        the thread's TMPDIR
//! ```
//!
//! The GUI uses [`launch`], [`is_running`], [`stop`] and [`reap_if_dead`];
//! `gitez supervise …` runs [`cli::main`], which runs [`run`].
//!
//! Everything here blocks; call it off the UI thread.

pub mod cli;
mod launch;
mod liveness;
mod pipeline;
mod policy;
mod queue;
mod secrets;

pub use launch::{Launch, LaunchError, launch};
pub use liveness::{StopError, is_running, reap_if_dead, stop};
pub use pipeline::{Environment, run};
pub use secrets::Secrets;

/// The subcommands: `gitez supervise <start|reply|retry> --git-dir <common dir> --thread <id>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Start,
    Reply,
    Retry,
}

impl Command {
    pub const ALL: [Command; 3] = [Self::Start, Self::Reply, Self::Retry];

    /// `start`, `reply` or `retry`, as on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Reply => "reply",
            Self::Retry => "retry",
        }
    }

    /// The subcommand named `name`, if there is one.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|command| command.as_str() == name)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Thread(#[from] gitez_agent_thread::Error),
    #[error(transparent)]
    Agent(#[from] gitez_agent::Error),
    #[error(transparent)]
    Sandbox(#[from] gitez_sandbox::Error),
    #[error(transparent)]
    Review(#[from] gitez_review::Error),
    #[error(transparent)]
    Git(#[from] gitez_command::Error),
    #[error(transparent)]
    Read(#[from] gitez_git::Error),
    #[error(transparent)]
    Settings(#[from] gitez_settings::Error),
    #[error("another supervisor is already running this thread")]
    AlreadyRunning,
    #[error("{0}")]
    Usage(String),
    #[error("could not read or write {path}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn io(path: impl Into<std::path::PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Error::Io { path, source }
    }
}

/// The folder of agent thread `id` in the repository at `common_dir`.
pub(crate) fn thread_dir(
    common_dir: &std::path::Path,
    id: &gitez_agent_thread::AgentThreadId,
) -> std::path::PathBuf {
    gitez_agent_thread::Store::new(common_dir).thread_dir(id)
}

#[cfg(test)]
mod tests {
    use super::Command;

    #[test]
    fn subcommands_round_trip() {
        for command in Command::ALL {
            assert_eq!(Command::from_name(command.as_str()), Some(command));
        }
        assert_eq!(Command::from_name("stop"), None);
    }
}
