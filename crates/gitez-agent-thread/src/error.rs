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
    #[error(
        "{path} exists: another GitEZ is saving this thread, or one quit while saving it. \
         Remove the file if no other GitEZ is running."
    )]
    Locked { path: PathBuf },
    #[error("there is no thread {0}")]
    NotFound(AgentThreadId),
    #[error("{0} is not a valid thread id")]
    InvalidId(String),
    /// A change the thread's state does not allow, said for the user.
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Git(#[from] gitez_command::Error),
    #[error(transparent)]
    Read(#[from] gitez_git::Error),
    #[error(transparent)]
    Watch(#[from] notify::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Error::Io { path, source }
    }

    pub(crate) fn refused(message: impl Into<String>) -> Self {
        Error::Refused(message.into())
    }
}
