use std::path::{Path, PathBuf};

use gix::bstr::ByteSlice as _;

use crate::{Error, Result};

/// Object cache per thread-local handle. Commit walks look objects up more
/// than once, so a small cache pays for itself.
const OBJECT_CACHE_BYTES: usize = 4 * 1024 * 1024;

/// An open git repository.
///
/// Cloning is cheap and clones share the underlying object database, so give
/// each background task its own clone.
#[derive(Clone)]
pub struct Repository {
    inner: gix::ThreadSafeRepository,
}

impl Repository {
    /// Opens the repository that contains `path`, searching parent directories
    /// like `git` does.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let inner = gix::ThreadSafeRepository::discover(path).map_err(|source| Error::Open {
            path: path.to_owned(),
            source,
        })?;
        Ok(Self { inner })
    }

    /// The `.git` directory.
    pub fn git_dir(&self) -> &Path {
        self.inner.git_dir()
    }

    /// The git directory shared by all worktrees of the repository: the
    /// main `.git` directory, also when opened from a linked worktree.
    pub fn common_dir(&self) -> PathBuf {
        self.local().common_dir().to_owned()
    }

    /// The working tree, or `None` for a bare repository.
    pub fn work_dir(&self) -> Option<&Path> {
        self.inner.work_dir()
    }

    /// A display name for the repository: the working tree's directory name,
    /// or the git directory's name when bare.
    pub fn name(&self) -> String {
        let dir: PathBuf = self.work_dir().unwrap_or_else(|| self.git_dir()).to_owned();
        dir.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.display().to_string())
    }

    /// The name commits are made under (`user.name`), if configured.
    pub fn user_name(&self) -> Option<String> {
        self.config_string("user.name")
    }

    /// The value of `key` in the repository's git configuration, such as
    /// `gitez.testCommand`, if set and not empty. Read when the repository
    /// was opened.
    pub fn config_string(&self, key: &str) -> Option<String> {
        let repo = self.local();
        let config = repo.config_snapshot();
        let value = config.string(key)?;
        let value = value.to_str_lossy().trim().to_owned();
        (!value.is_empty()).then_some(value)
    }

    /// The names of the configured remotes, sorted.
    pub fn remote_names(&self) -> Vec<String> {
        self.local()
            .remote_names()
            .into_iter()
            .map(|name| name.to_str_lossy().into_owned())
            .collect()
    }

    /// When this working tree last fetched, as seconds since the Unix
    /// epoch: the time `FETCH_HEAD` was written. `None` before the first
    /// fetch.
    pub fn last_fetch(&self) -> Option<i64> {
        let modified = std::fs::metadata(self.git_dir().join("FETCH_HEAD"))
            .and_then(|metadata| metadata.modified())
            .ok()?;
        let elapsed = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
        i64::try_from(elapsed.as_secs()).ok()
    }

    /// A handle for use on the current thread.
    pub(crate) fn local(&self) -> gix::Repository {
        let mut repo = self.inner.to_thread_local();
        repo.object_cache_size_if_unset(OBJECT_CACHE_BYTES);
        repo
    }
}

impl std::fmt::Debug for Repository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repository")
            .field("git_dir", &self.git_dir())
            .finish()
    }
}
