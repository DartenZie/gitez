use std::path::PathBuf;

/// Errors returned when reading or writing local pull requests.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not read or write {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not a valid local pull request")]
    Invalid {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{path} was written by a newer GitEZ (format version {version})")]
    TooNew { path: PathBuf, version: u32 },
    #[error(
        "{path} exists: another GitEZ is saving this pull request, or one quit \
         while saving it. Remove the file if no other GitEZ is running."
    )]
    Locked { path: PathBuf },
    #[error("there is no local pull request #{0}")]
    NotFound(u32),
    #[error("there is no thread {thread} in local pull request #{number}")]
    NoThread { number: u32, thread: u32 },
    #[error("there is no comment {comment} in thread {thread} of local pull request #{number}")]
    NoComment {
        number: u32,
        thread: u32,
        comment: u32,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Error::Io { path, source }
    }
}
