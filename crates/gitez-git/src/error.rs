use std::path::PathBuf;

/// Errors returned when reading a repository.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not open a git repository at {path}")]
    Open {
        path: PathBuf,
        #[source]
        source: gix::Error,
    },
    #[error("could not read the reflog of {name}")]
    Reflog {
        name: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not read {path}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not compute a diff")]
    Diff(#[source] std::io::Error),
    #[error("could not watch the repository for changes")]
    Watch(#[from] notify::Error),
    #[error("could not work out the merge")]
    Merge(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error(transparent)]
    Git(#[from] gix::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
