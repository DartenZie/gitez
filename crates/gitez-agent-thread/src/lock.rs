//! Writing a file safely, as git writes its own: the new content goes to
//! `<file>.lock`, created exclusively, which is then renamed over the file.
//!
//! The writer also holds an advisory lock (`flock`) on the lock file for as
//! long as it exists. The kernel releases that lock when the writer exits,
//! however it exits, so a lock file left behind by a writer that died
//! (power loss, `kill -9`) is recognised and removed by the next writer
//! instead of blocking the thread forever.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::{Error, Result};

/// How often a held lock is tried again.
const RETRY_EVERY: Duration = Duration::from_millis(20);
/// How long a held lock is waited for before giving up.
const WAIT_AT_MOST: Duration = Duration::from_secs(5);
/// How old an unlocked lock file must be before it is taken for a dead
/// writer's: a live writer locks its file microseconds after creating it.
const STALE_AFTER: Duration = Duration::from_secs(2);

/// An exclusively created lock file, removed on drop unless renamed over
/// its target.
pub(crate) struct Lock {
    path: PathBuf,
    file: File,
    committed: bool,
}

impl Lock {
    /// Creates `path`; while another writer holds it, tries again every
    /// 20 ms for up to 5 s, then fails with [`Error::Locked`]. A lock file
    /// whose writer died is removed and taken.
    pub(crate) fn take(path: &Path) -> Result<Self> {
        Self::take_waiting(path, WAIT_AT_MOST, STALE_AFTER)
    }

    fn take_waiting(path: &Path, wait: Duration, stale_after: Duration) -> Result<Self> {
        let deadline = Instant::now() + wait;
        loop {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(file) => {
                    // Nobody else can have it yet: the file is new.
                    file.try_lock().map_err(|error| match error {
                        TryLockError::WouldBlock => Error::Locked {
                            path: path.to_owned(),
                        },
                        TryLockError::Error(source) => Error::io(path)(source),
                    })?;
                    return Ok(Self {
                        path: path.to_owned(),
                        file,
                        committed: false,
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if break_if_stale(path, stale_after).map_err(Error::io(path))? {
                        continue;
                    }
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
            // Best effort: a lock left behind is removed by the next writer
            // once nobody holds it.
            let _ = fs::remove_file(&self.path);
        }
        // Closing the file releases the advisory lock.
    }
}

/// Removes the lock file at `path` if its writer is gone: nobody holds its
/// advisory lock and it is older than `stale_after`. Returns whether it
/// removed it. While this holds the advisory lock no other writer can take
/// the file for stale, and no writer can create a new one at `path` while
/// this one is there, so the file removed is the one judged stale.
fn break_if_stale(path: &Path, stale_after: Duration) -> std::io::Result<bool> {
    let file = match File::open(path) {
        Ok(file) => file,
        // Committed or removed meanwhile: try to create it again.
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(false),
        Err(TryLockError::Error(error)) => return Err(error),
    }
    let held = file.metadata()?;
    let old_enough = held
        .modified()
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= stale_after);
    if !old_enough {
        return Ok(false);
    }
    match fs::symlink_metadata(path) {
        Ok(now) if now.dev() == held.dev() && now.ino() == held.ino() => {
            fs::remove_file(path)?;
            Ok(true)
        }
        // Another file is there now: not the one judged stale.
        Ok(_) => Ok(false),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_lock_is_waited_for_then_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("thread.json.lock");
        let held = Lock::take(&path).expect("first");
        let started = Instant::now();
        let error = Lock::take_waiting(&path, Duration::from_millis(100), Duration::ZERO)
            .err()
            .expect("held by a live writer");
        assert!(matches!(error, Error::Locked { .. }), "{error}");
        assert!(started.elapsed() >= Duration::from_millis(100));
        drop(held);
        assert!(!path.exists(), "an unused lock is removed");
        Lock::take(&path).expect("free again");
    }

    #[test]
    fn a_lock_left_by_a_dead_writer_is_broken() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("thread.json.lock");
        // What a writer that died between creating and renaming leaves: the
        // file, with nobody holding it.
        fs::write(&path, "half written").expect("write");
        let lock = Lock::take_waiting(&path, Duration::from_secs(1), Duration::ZERO)
            .expect("the dead writer's lock is broken");
        let target = dir.path().join("thread.json");
        lock.write_and_commit("{}\n", &target).expect("commit");
        assert_eq!(fs::read_to_string(&target).expect("read"), "{}\n");
        assert!(!path.exists());
    }

    #[test]
    fn a_fresh_unlocked_lock_is_not_broken_at_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("thread.json.lock");
        // A writer between creating its file and locking it.
        fs::write(&path, "").expect("write");
        let error = Lock::take_waiting(&path, Duration::from_millis(50), Duration::from_secs(60))
            .err()
            .expect("too new to be stale");
        assert!(matches!(error, Error::Locked { .. }), "{error}");
        assert!(path.exists());
    }

    #[test]
    fn a_lock_of_a_killed_process_is_broken() {
        // A real process that takes the lock and is killed with SIGKILL.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("thread.json.lock");
        // macOS has no flock(1); perl takes the same advisory lock.
        let mut child = std::process::Command::new("perl")
            .args([
                "-e",
                "use Fcntl qw(:flock); open(my $f, '>', $ARGV[0]) or die; flock($f, LOCK_EX) or die; \
                 print \"locked\\n\"; $| = 1; sleep 30;",
                &path.to_string_lossy(),
            ])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("perl runs");
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().expect("stdout")),
            &mut line,
        )
        .expect("read");
        assert_eq!(line.trim(), "locked");
        let error = Lock::take_waiting(&path, Duration::from_millis(50), Duration::ZERO)
            .err()
            .expect("held while the process lives");
        assert!(matches!(error, Error::Locked { .. }), "{error}");
        child.kill().expect("kill -9");
        child.wait().expect("reaped");
        Lock::take_waiting(&path, Duration::from_secs(1), Duration::ZERO)
            .expect("the kernel released the dead process's lock");
    }
}
