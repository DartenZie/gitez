//! Writing a file safely, as git writes its own: the new content goes to
//! `<file>.lock`, created exclusively, which is then renamed over the file.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::{Error, Result};

/// How often a held lock is tried again.
const RETRY_EVERY: Duration = Duration::from_millis(20);
/// How long a held lock is waited for before giving up.
const WAIT_AT_MOST: Duration = Duration::from_secs(5);

/// An exclusively created lock file, removed on drop unless renamed over
/// its target.
pub(crate) struct Lock {
    path: PathBuf,
    file: File,
    committed: bool,
}

impl Lock {
    /// Creates `path`; while another writer holds it, tries again every
    /// 20 ms for up to 5 s, then fails with [`Error::Locked`].
    pub(crate) fn take(path: &Path) -> Result<Self> {
        Self::take_waiting(path, WAIT_AT_MOST)
    }

    fn take_waiting(path: &Path, wait: Duration) -> Result<Self> {
        let deadline = Instant::now() + wait;
        loop {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(file) => {
                    return Ok(Self {
                        path: path.to_owned(),
                        file,
                        committed: false,
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if Instant::now() >= deadline {
                        return Err(Error::Locked {
                            path: path.to_owned(),
                        });
                    }
                    std::thread::sleep(RETRY_EVERY);
                }
                Err(error) => return Err(Error::io(path)(error)),
            }
        }
    }

    /// Writes `text` to the lock file and renames it over `target`, so
    /// `target` is never half written.
    pub(crate) fn write_and_commit(mut self, text: &str, target: &Path) -> Result<()> {
        let mut file = &self.file;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(Error::io(&self.path))?;
        fs::rename(&self.path, target).map_err(Error::io(target))?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if !self.committed {
            // Best effort: a lock left behind is reported on the next save.
            let _ = fs::remove_file(&self.path);
        }
    }
}
