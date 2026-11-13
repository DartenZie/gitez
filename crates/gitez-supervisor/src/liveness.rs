//! Whether a thread's supervisor is alive, stopping it, and noticing that
//! it died. A live supervisor holds an exclusive `flock` on
//! `<thread dir>/supervisor.lock`; the kernel releases it when the process
//! ends, however it ends.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use gitez_agent_thread::{AgentThreadId, Step, Store, SupervisorInfo, Timestamp};
use jiff::SignedDuration;

use crate::{Error, Result};

const LOCK_FILE: &str = "supervisor.lock";

/// How long a thread may wait for its supervisor to take the lock after it
/// was created or a turn was asked for.
const LAUNCH_GRACE: SignedDuration = SignedDuration::from_secs(15);

const DIED: &str = "GitEZ's supervisor for this thread stopped unexpectedly. Retry to continue.";

/// The lock a live supervisor holds; released when dropped.
#[derive(Debug)]
pub(crate) struct SupervisorLock {
    _file: File,
}

impl SupervisorLock {
    /// Takes `<thread_dir>/supervisor.lock`; [`Error::AlreadyRunning`] when
    /// another supervisor holds it.
    pub(crate) fn take(thread_dir: &Path) -> Result<Self> {
        let path = thread_dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(Error::io(&path))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(Error::AlreadyRunning),
            Err(TryLockError::Error(source)) => Err(Error::Io { path, source }),
        }
    }
}

/// Whether a supervisor holds `<thread dir>/supervisor.lock`: a shared
/// `try_lock_shared` that succeeds means none does (the lock is released
/// right away).
pub fn is_running(thread_dir: &Path) -> bool {
    let Ok(file) = File::open(thread_dir.join(LOCK_FILE)) else {
        return false;
    };
    matches!(file.try_lock_shared(), Err(TryLockError::WouldBlock))
}

#[derive(Debug, thiserror::Error)]
pub enum StopError {
    #[error("the thread is not running")]
    NotRunning,
    #[error("could not signal the supervisor")]
    Signal(#[source] std::io::Error),
}

/// SIGTERM to the supervisor, after checking it holds its lock. The
/// supervisor then ends the agent's turn (SIGINT, then SIGKILL after 20 s)
/// and finishes the thread with ending `stopped`.
pub fn stop(thread_dir: &Path, supervisor: &SupervisorInfo) -> Result<(), StopError> {
    if !is_running(thread_dir) {
        return Err(StopError::NotRunning);
    }
    let pid = pid(supervisor.pid).ok_or(StopError::NotRunning)?;
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).map_err(|errno| {
        if errno == rustix::io::Errno::SRCH {
            StopError::NotRunning
        } else {
            StopError::Signal(errno.into())
        }
    })
}

/// If `id` is active but no supervisor holds its lock (and it is not a
/// thread launched less than 15 s ago with no `supervisor` yet), marks it
/// failed (step Supervisor, "GitEZ's supervisor for this thread stopped
/// unexpectedly. Retry to continue.") and SIGKILLs `child_group` if that
/// group still exists. Returns whether it marked it.
///
/// "Launched" is the latest of the thread's `created`, `updated` and its
/// turns' `requested` times (`updated` covers a retry that adds no turn).
pub fn reap_if_dead(store: &Store, id: &AgentThreadId, now: Timestamp) -> Result<bool> {
    let thread_dir = store.thread_dir(id);
    let thread = store.load(id)?;
    if !thread.state.is_active() || is_running(&thread_dir) {
        return Ok(false);
    }
    let launched = thread
        .turns
        .iter()
        .map(|turn| turn.requested)
        .chain([thread.created, thread.updated])
        .max()
        .unwrap_or(thread.created);
    if thread.supervisor.is_none() && now.duration_since(launched) < LAUNCH_GRACE {
        return Ok(false);
    }
    // Checked again under the thread's lock: the supervisor may have
    // finished, or a new one started, since.
    let child_group = store.update(id, |thread| {
        if !thread.state.is_active() || is_running(&thread_dir) {
            return Ok(None);
        }
        let group = thread
            .supervisor
            .as_ref()
            .and_then(|supervisor| supervisor.child_group);
        thread.fail(Step::Supervisor, DIED, now);
        Ok(Some(group))
    })?;
    let Some(group) = child_group else {
        return Ok(false);
    };
    if let Some(group) = group.and_then(pid) {
        // Fails only when the group is gone already.
        rustix::process::kill_process_group(group, rustix::process::Signal::KILL).ok();
    }
    Ok(true)
}

fn pid(raw: u32) -> Option<rustix::process::Pid> {
    i32::try_from(raw)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
}
