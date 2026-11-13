use std::path::PathBuf;

use crate::AgentThreadId;

/// Errors returned when reading or changing agent threads.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not read or write {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not a valid agent thread")]
    Invalid {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{path} was written by a newer GitEZ (format version {version})")]
    TooNew { path: PathBuf, version: u32 },
    #[error("{path} is held: another GitEZ is saving this thread. Try again.")]
    Locked { path: PathBuf },
    #[error("there is no thread {0}")]
    NotFound(AgentThreadId),
    #[error("{0} is not a valid thread id")]
    InvalidId(String),
    /// A change the thread's state does not allow, said for the user.
    #[error("{0}")]
    Refused(String),
    /// The worktree's git files are not what GitEZ made: its `.git` file,
    /// or the `commondir`, `gitdir` or `config.worktree` file of its own
    /// git dir, was changed, replaced or linked elsewhere. GitEZ runs no
    /// git command in it; the user must look at it and remove it by hand.
    /// The text says what differs, for the user.
    #[error(
        "GitEZ does not trust the worktree at {worktree}: {problem} GitEZ did not run git in it. \
         Look at it and remove it yourself."
    )]
    Tampered { worktree: PathBuf, problem: String },
    /// [`complete`](crate::complete) with [`IfUncommitted::Refuse`](crate::IfUncommitted::Refuse)
    /// on a worktree the supervisor recorded as having uncommitted changes
    /// (`count` of them), or whose state is not known (`count` None).
    #[error("{}", uncommitted_message(*count))]
    Uncommitted {
        worktree: PathBuf,
        count: Option<u32>,
    },
    #[error(transparent)]
    Git(#[from] gitez_command::Error),
    #[error(transparent)]
    Read(#[from] gitez_git::Error),
    #[error(transparent)]
    Watch(#[from] notify::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

fn uncommitted_message(count: Option<u32>) -> String {
    match count {
        Some(1) => "The worktree has 1 uncommitted change.".to_owned(),
        Some(count) => format!("The worktree has {count} uncommitted changes."),
        None => "GitEZ does not know whether the worktree has uncommitted changes.".to_owned(),
    }
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Error::Io { path, source }
    }

    pub(crate) fn refused(message: impl Into<String>) -> Self {
        Error::Refused(message.into())
    }
}
