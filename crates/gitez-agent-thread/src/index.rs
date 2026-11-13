//! The global index of the repositories that have agent threads, so Home
//! can show the threads of repositories that are not open.
//!
//! ```json
//! {
//!   "version": 1,
//!   "repositories": [
//!     { "common_dir": "/Users/me/repos/gitez/.git", "work_dir": "/Users/me/repos/gitez", "name": "gitez" }
//!   ]
//! }
//! ```

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::lock::Lock;
use crate::{Error, Result};

const INDEX_FILE: &str = "threads.json";
const LOCK_FILE: &str = "threads.json.lock";
/// The newest `threads.json` format this crate reads and writes.
const INDEX_VERSION: u32 = 1;

/// The repositories that have agent threads: `<data dir>/threads.json`.
#[derive(Debug, Clone)]
pub struct Index {
    path: PathBuf,
}

/// A repository listed in the [`Index`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedRepository {
    pub common_dir: PathBuf,
    /// The main worktree.
    pub work_dir: PathBuf,
    pub name: String,
}

/// What [`Index::load_report`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexReport {
    /// As [`Index::load`] returns them.
    pub repositories: Vec<IndexedRepository>,
    /// Listed, but their common dir is gone.
    pub missing: Vec<IndexedRepository>,
    /// The file's format version, when a newer GitEZ wrote it (and it
    /// lists nothing for this one).
    pub too_new: Option<u32>,
}

#[derive(Serialize, Deserialize)]
struct OnDisk {
    version: u32,
    #[serde(default)]
    repositories: Vec<IndexedRepository>,
}

#[derive(Deserialize)]
struct VersionOnly {
    version: u32,
}

/// What the index file holds, as far as this GitEZ can tell.
enum Contents {
    Missing,
    Known(Vec<IndexedRepository>),
    /// Written by a newer GitEZ.
    TooNew(u32),
}

impl Index {
    /// The index in GitEZ's data folder `data_dir`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(INDEX_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The listed repositories. A missing file, or one a newer GitEZ
    /// wrote, lists none. Entries whose common dir no longer exists are
    /// left out (not removed from the file).
    pub fn load(&self) -> Result<Vec<IndexedRepository>> {
        Ok(match self.read()? {
            Contents::Missing | Contents::TooNew(_) => Vec::new(),
            Contents::Known(repositories) => repositories
                .into_iter()
                .filter(|repository| repository.common_dir.is_dir())
                .collect(),
        })
    }

    /// [`Index::load`], and what it leaves out, so a view can say why
    /// threads are missing: the entries whose common dir is gone (moved or
    /// deleted), and the format version of a file a newer GitEZ wrote.
    pub fn load_report(&self) -> Result<IndexReport> {
        Ok(match self.read()? {
            Contents::Missing => IndexReport::default(),
            Contents::TooNew(version) => IndexReport {
                too_new: Some(version),
                ..IndexReport::default()
            },
            Contents::Known(repositories) => {
                let (repositories, missing) = repositories
                    .into_iter()
                    .partition(|repository| repository.common_dir.is_dir());
                IndexReport {
                    repositories,
                    missing,
                    too_new: None,
                }
            }
        })
    }

    /// Removes the entry of `common_dir` (any spelling of it), if listed
    /// (lock and rename): for a repository that was moved or deleted.
    /// Refuses ([`Error::TooNew`]) to change a file a newer GitEZ wrote.
    pub fn remove(&self, common_dir: &Path) -> Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        let lock = Lock::take(&self.path.with_file_name(LOCK_FILE))?;
        let mut repositories = match self.read()? {
            Contents::Missing => return Ok(()),
            Contents::Known(repositories) => repositories,
            Contents::TooNew(version) => {
                return Err(Error::TooNew {
                    path: self.path.clone(),
                    version,
                });
            }
        };
        let key = same_dir_key(common_dir);
        let before = repositories.len();
        repositories.retain(|listed| {
            listed.common_dir != common_dir && same_dir_key(&listed.common_dir) != key
        });
        if repositories.len() == before {
            return Ok(());
        }
        self.write(lock, repositories)
    }

    /// Adds `repository` unless its common dir is listed (lock and
    /// rename). Refuses ([`Error::TooNew`]) to overwrite a file a newer
    /// GitEZ wrote.
    pub fn add(&self, repository: IndexedRepository) -> Result<()> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(dir).map_err(Error::io(dir))?;
        let lock = Lock::take(&self.path.with_file_name(LOCK_FILE))?;
        let mut repositories = match self.read()? {
            Contents::Missing => Vec::new(),
            Contents::Known(repositories) => repositories,
            Contents::TooNew(version) => {
                return Err(Error::TooNew {
                    path: self.path.clone(),
                    version,
                });
            }
        };
        let key = same_dir_key(&repository.common_dir);
        if repositories
            .iter()
            .any(|listed| same_dir_key(&listed.common_dir) == key)
        {
            return Ok(());
        }
        repositories.push(repository);
        self.write(lock, repositories)
    }

    /// Writes `repositories` under `lock`, taken by the caller.
    fn write(&self, lock: Lock, repositories: Vec<IndexedRepository>) -> Result<()> {
        let mut text = serde_json::to_string_pretty(&OnDisk {
            version: INDEX_VERSION,
            repositories,
        })
        .map_err(|source| Error::Invalid {
            path: self.path.clone(),
            source,
        })?;
        text.push('\n');
        lock.write_and_commit(&text, &self.path)
    }

    fn read(&self) -> Result<Contents> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Contents::Missing),
            Err(error) => return Err(Error::io(&self.path)(error)),
        };
        let invalid = |source| Error::Invalid {
            path: self.path.clone(),
            source,
        };
        let VersionOnly { version } = serde_json::from_str(&text).map_err(invalid)?;
        if version > INDEX_VERSION {
            return Ok(Contents::TooNew(version));
        }
        let on_disk: OnDisk = serde_json::from_str(&text).map_err(invalid)?;
        Ok(Contents::Known(on_disk.repositories))
    }
}

/// `dir` with symlinks resolved when it exists, so two spellings of one
/// folder are one entry.
fn same_dir_key(dir: &Path) -> PathBuf {
    dir.canonicalize().unwrap_or_else(|_| dir.to_owned())
}
