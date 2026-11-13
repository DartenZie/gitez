//! The global running-agent limit, shared by independent supervisor
//! processes through files in `<data dir>/queue/` (DESIGN D.4).
//!
//! A waiting supervisor holds an exclusive lock on its ticket,
//! `tickets/<nanoseconds, 20 digits>-<repo key>-<id>.ticket`. Only the
//! first ticket in name order tries the slots `slots/0.lock …
//! slots/<max-1>.lock`; the first slot it can lock is its own until it
//! drops it. A ticket nobody holds belongs to a supervisor that died and
//! is deleted by whoever sees it.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use gitez_settings::{DEFAULT_MAX_RUNNING, Loaded, Settings};

const TICKET: &str = "ticket";
/// A ticket being created, before it is locked and renamed.
const NEW_TICKET: &str = "new";
/// How often a wait checks the stop flag.
pub(crate) const STOP_CHECK: Duration = Duration::from_millis(100);

/// A running-agent slot, held until dropped.
#[derive(Debug)]
pub(crate) struct Slot {
    _file: File,
    pub(crate) number: u32,
}

#[derive(Debug)]
pub(crate) enum Waited {
    Slot(Slot),
    /// The stop flag was set while waiting.
    Stopped,
}

/// The queue in `<data_dir>/queue/`.
#[derive(Debug, Clone)]
pub(crate) struct Queue {
    tickets: PathBuf,
    slots: PathBuf,
    settings_path: PathBuf,
}

/// A waiting thread's place in the queue; deleted when dropped.
#[derive(Debug)]
struct Ticket {
    path: PathBuf,
    name: String,
    _file: File,
}

impl Queue {
    pub(crate) fn new(data_dir: &Path, settings_path: &Path) -> Self {
        let queue = data_dir.join("queue");
        Self {
            tickets: queue.join("tickets"),
            slots: queue.join("slots"),
            settings_path: settings_path.to_owned(),
        }
    }

    /// Waits until this thread (`<repo key>-<id>`) is first in the queue
    /// and a slot below the limit is free, re-reading the limit from
    /// settings.json every `poll`. Returns `Stopped` as soon as `stop` is
    /// set.
    pub(crate) fn wait(
        &self,
        thread: &str,
        poll: Duration,
        stop: &AtomicBool,
    ) -> std::io::Result<Waited> {
        fs::create_dir_all(&self.tickets)?;
        fs::create_dir_all(&self.slots)?;
        let name = format!("{:020}-{thread}.{TICKET}", now_nanos());
        let mut ticket = Ticket::create(&self.tickets, name)?;
        let mut max_running = DEFAULT_MAX_RUNNING;
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(Waited::Stopped);
            }
            if let Some(max) = self.max_running() {
                max_running = max;
            }
            let first = self.first_live_ticket(&ticket.name)?;
            if first.as_deref() == Some(ticket.name.as_str()) {
                for number in 0..max_running.max(1) {
                    if let Some(slot) = self.try_slot(number)? {
                        drop(ticket);
                        return Ok(Waited::Slot(slot));
                    }
                }
            }
            if !ticket.path.exists() {
                // Someone removed it; take the same place again.
                ticket = Ticket::create(&self.tickets, ticket.name.clone())?;
            }
            if sleep(poll, stop) {
                return Ok(Waited::Stopped);
            }
        }
    }

    fn max_running(&self) -> Option<u32> {
        match Settings::load(&self.settings_path) {
            Ok(Loaded::Current(settings)) => Some(settings.threads.max_running),
            _ => None,
        }
    }

    /// The first ticket by name whose supervisor is alive, deleting the
    /// tickets of supervisors that died. `ours` counts as alive.
    fn first_live_ticket(&self, ours: &str) -> std::io::Result<Option<String>> {
        let mut names: Vec<String> = fs::read_dir(&self.tickets)?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.ends_with(&format!(".{TICKET}")))
            .collect();
        names.sort();
        let mut first = None;
        for name in names {
            if name != ours && is_abandoned(&self.tickets.join(&name)) {
                fs::remove_file(self.tickets.join(&name)).ok();
                continue;
            }
            if first.is_none() {
                first = Some(name);
            }
        }
        Ok(first)
    }

    fn try_slot(&self, number: u32) -> std::io::Result<Option<Slot>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.slots.join(format!("{number}.lock")))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Slot {
                _file: file,
                number,
            })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }
}

impl Ticket {
    /// Creates the ticket locked: it is made under another name, locked,
    /// then renamed, so no one ever sees it unlocked and takes it for a
    /// dead supervisor's.
    fn create(dir: &Path, name: String) -> std::io::Result<Self> {
        let path = dir.join(&name);
        let new = dir.join(format!("{name}.{NEW_TICKET}"));
        let file = OpenOptions::new().create_new(true).write(true).open(&new)?;
        file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => std::io::Error::from(ErrorKind::WouldBlock),
            TryLockError::Error(error) => error,
        })?;
        fs::rename(&new, &path)?;
        Ok(Self {
            path,
            name,
            _file: file,
        })
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        fs::remove_file(&self.path).ok();
    }
}

/// Whether nobody holds the ticket at `path`.
fn is_abandoned(path: &Path) -> bool {
    match File::open(path) {
        Ok(file) => file.try_lock_shared().is_ok(),
        Err(_) => false,
    }
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default()
}

/// Sleeps for `duration`, checking `stop` every 100 ms. Returns whether
/// `stop` was set.
pub(crate) fn sleep(duration: Duration, stop: &AtomicBool) -> bool {
    let until = Instant::now() + duration;
    loop {
        if stop.load(Ordering::SeqCst) {
            return true;
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(left.min(STOP_CHECK));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn queue(dir: &Path, max_running: u32) -> Queue {
        let settings_path = dir.join("settings.json");
        let mut settings = Settings::default();
        settings.threads.max_running = max_running;
        settings.save(&settings_path).expect("save settings");
        Queue::new(dir, &settings_path)
    }

    const POLL: Duration = Duration::from_millis(10);

    #[test]
    fn threads_take_slots_in_order_up_to_the_limit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let queue = queue(dir.path(), 1);
        let stop = AtomicBool::new(false);
        let Waited::Slot(first) = queue.wait("repo-a", POLL, &stop).expect("wait") else {
            panic!("not stopped");
        };
        assert_eq!(first.number, 0);

        // The second waits until the first slot is free.
        let queue2 = queue.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let stop = AtomicBool::new(false);
            let waited = queue2.wait("repo-b", POLL, &stop).expect("wait");
            sender.send(()).expect("send");
            waited
        });
        assert!(receiver.recv_timeout(Duration::from_millis(200)).is_err());
        drop(first);
        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("second gets the slot");
        assert!(matches!(
            waiter.join().expect("join"),
            Waited::Slot(Slot { number: 0, .. })
        ));
        assert_eq!(fs::read_dir(&queue.tickets).expect("list").count(), 0);
    }

    #[test]
    fn a_higher_limit_runs_several_at_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let queue = queue(dir.path(), 2);
        let stop = AtomicBool::new(false);
        let a = queue.wait("a", POLL, &stop).expect("wait");
        let b = queue.wait("b", POLL, &stop).expect("wait");
        assert!(matches!(a, Waited::Slot(Slot { number: 0, .. })));
        assert!(matches!(b, Waited::Slot(Slot { number: 1, .. })));
    }

    #[test]
    fn stop_ends_the_wait_and_removes_the_ticket() {
        let dir = tempfile::tempdir().expect("temp dir");
        let queue = queue(dir.path(), 1);
        let stop = Arc::new(AtomicBool::new(false));
        let _held = queue.wait("a", POLL, &stop).expect("wait");
        let stop2 = stop.clone();
        let queue2 = queue.clone();
        let waiter = std::thread::spawn(move || queue2.wait("b", POLL, &stop2).expect("wait"));
        std::thread::sleep(Duration::from_millis(100));
        stop.store(true, Ordering::SeqCst);
        assert!(matches!(waiter.join().expect("join"), Waited::Stopped));
        assert_eq!(fs::read_dir(&queue.tickets).expect("list").count(), 0);
    }

    #[test]
    fn an_abandoned_ticket_is_removed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let queue = queue(dir.path(), 1);
        fs::create_dir_all(&queue.tickets).expect("tickets");
        // Older than any new ticket, and locked by nobody.
        let dead = queue.tickets.join(format!("{:020}-dead.{TICKET}", 1));
        fs::write(&dead, "").expect("write");
        let stop = AtomicBool::new(false);
        let waited = queue.wait("alive", POLL, &stop).expect("wait");
        assert!(matches!(waited, Waited::Slot(_)));
        assert!(!dead.exists());
    }
}
