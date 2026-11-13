//! GitEZ's own data folder (overridable with `GITEZ_DATA_DIR`) and the
//! settings kept in it, `settings.json`: agent entries, secret names and
//! scopes, sandbox defaults and the running-agent limit.
//!
//! ```text
//! <data dir>/
//!   settings.json        the settings
//!   settings.json.lock   only while they are being saved
//! ```

mod data_dir;
mod settings;

use std::path::PathBuf;

pub use data_dir::{DATA_DIR_ENV, data_dir, data_dir_from};
pub use settings::{
    AgentEntry, DEFAULT_HIDDEN_PATHS, DEFAULT_MAX_RUNNING, DEFAULT_PACKAGE_HOSTS, Loaded,
    MIN_SECRET_LEN, SandboxSettings, SecretEntry, SecretScope, Settings, ThreadSettings,
    is_env_name, secret_value_problem,
};

/// The newest `settings.json` format this crate reads and writes.
pub const VERSION: u32 = 1;

/// Errors returned when reading or writing the settings.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not read or write {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not valid GitEZ settings")]
    Invalid {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{path} was written by a newer GitEZ (format version {version})")]
    TooNew { path: PathBuf, version: u32 },
    #[error(
        "{path} exists: another GitEZ is saving the settings, or one quit while \
         saving them. Remove the file if no other GitEZ is running."
    )]
    Locked { path: PathBuf },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Error::Io { path, source }
    }
}
