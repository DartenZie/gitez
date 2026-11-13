//! Running one `git` process so that it can always be ended.
//!
//! Git starts other processes (ssh, hooks, credential helpers) that share
//! its output and that it waits for. When one of them outlives git's own
//! work, git never exits, or exits and leaves its output open. So every
//! git runs in its own process group, an [`Operation`] can end the whole
//! group, and what git wrote is readable while it still runs.

use std::io::{Read, Write as _};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// How long a cancelled git, and the processes it started, have to leave
/// after being asked to (`SIGTERM`) before they are killed (`SIGKILL`).
pub const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// How long git may keep running after it last wrote to standard error
/// before what it wrote is reported while it still runs: see
/// [`Operation::on_stalled`].
pub const STALLED_AFTER: Duration = Duration::from_secs(3);

/// How long the output of a git that has exited may stay open, held by a
/// process git started, before the run ends without the rest of it.
const EXIT_DRAIN: Duration = Duration::from_millis(500);

/// How often a run checks on a git that is not writing anything.
const TICK: Duration = Duration::from_millis(100);

/// How often a run checks on a git that has closed its output and is about
/// to exit.
const EXIT_POLL: Duration = Duration::from_millis(1);

/// The most of git's standard error an [`Operation`] keeps: its end.
const SAID_LIMIT: usize = 64 * 1024;

/// A handle on the git commands of one user action: it can cancel them, and
/// it knows what the one running has written to standard error.
///
/// Give it to [`Git::with_operation`](crate::Git::with_operation) and keep a
/// clone. An operation is for one action at a time; a new action needs a
/// new one, because a cancelled operation stays cancelled.
#[derive(Clone, Default)]
pub struct Operation {
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
}

/// Told what git wrote: see [`Operation::on_stalled`].
type Notify = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
struct State {
    /// How often [`Operation::cancel`] was called.
    cancels: u64,
    /// Wakes the run in progress, if there is one.
    wake: Option<mpsc::Sender<Event>>,
    /// The end of what the git running now, or the last one, wrote to
    /// standard error.
    said: Vec<u8>,
    /// What a git still running wrote [`STALLED_AFTER`] ago or longer.
    stalled: Option<String>,
    on_stalled: Option<Notify>,
}

impl Operation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Calls `notify`, on the thread running git, with what git has written
    /// to standard error once it has written something, then neither
    /// written more nor exited for [`STALLED_AFTER`]: usually an error it
    /// cannot leave after, because a process it started is still there.
    /// The git is still running, and [`Operation::cancel`] ends it.
    #[must_use]
    pub fn on_stalled(self, notify: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.state().on_stalled = Some(Arc::new(notify));
        self
    }

    /// Ends the git running now with every process it started: they are
    /// asked to leave (`SIGTERM` to the process group) and, if git has not
    /// exited or its output is still open [`CANCEL_GRACE`] later, killed
    /// (`SIGKILL`). The command then fails with
    /// [`Error::Cancelled`](crate::Error::Cancelled), and so does every
    /// later step of the same action; steps that undo a half-done change,
    /// such as `git rebase --abort`, still run. Returns at once.
    pub fn cancel(&self) {
        let mut state = self.state();
        state.cancels += 1;
        if let Some(wake) = &state.wake {
            // The run has ended if nothing receives it.
            let _ = wake.send(Event::Cancel);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.state().cancels > 0
    }

    /// What the git running now, or the last one, has written to standard
    /// error so far, trimmed.
    pub fn said(&self) -> String {
        text(&self.state().said)
    }

    /// What [`Operation::on_stalled`] was last told, while that git is
    /// still running.
    pub fn stalled(&self) -> Option<String> {
        self.state().stalled.clone()
    }

    /// The state; a panic elsewhere while holding it does not make it
    /// wrong.
    fn state(&self) -> MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl std::fmt::Debug for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Operation")
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// One of the two streams git writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stream {
    Out,
    Err,
}

/// How a run ended.
#[derive(Debug)]
pub(crate) enum End {
    Exited(ExitStatus),
    /// Ended by [`Operation::cancel`], or never started because the
    /// operation was cancelled already.
    Cancelled,
}

enum Event {
    Wrote(Stream, Vec<u8>),
    /// One of the two streams ended.
    Closed,
    Cancel,
}

/// Takes the run out of the operation when it ends, however it ends.
struct Registered<'a>(Option<&'a Operation>);

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        if let Some(operation) = self.0 {
            let mut state = operation.state();
            state.wake = None;
            state.stalled = None;
        }
    }
}

/// Runs `command` in its own process group, writing `input` to its standard
/// input and calling `output` with what it writes as it comes, until it has
/// exited and closed its output, or `operation` is cancelled.
///
/// A run that is not `undoing` does not start once the operation is
/// cancelled. One that is undoing a half-done change starts anyway, and
/// ends only if cancelled again.
pub(crate) fn run(
    mut command: Command,
    input: Option<&[u8]>,
    operation: Option<&Operation>,
    undoing: bool,
    mut output: impl FnMut(Stream, &[u8]),
) -> std::io::Result<End> {
    let (sender, events) = mpsc::channel();
    if let Some(operation) = operation {
        let mut state = operation.state();
        if state.cancels > 0 && !undoing {
            return Ok(End::Cancelled);
        }
        // From here a cancel reaches this run, also before git has started.
        state.wake = Some(sender.clone());
        state.said.clear();
        state.stalled = None;
    }
    let _registered = Registered(operation);

    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so cancelling ends the processes git started
    // too. They share its output, and git waits for some of them.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn()?;

    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        // From a thread, so input larger than the pipe holds cannot block
        // reading the output. A command that fails early stops reading; its
        // output says why.
        let input = input.to_owned();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let mut open = 0;
    if let Some(stdout) = child.stdout.take() {
        open += 1;
        read(stdout, Stream::Out, sender.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        open += 1;
        read(stderr, Stream::Err, sender.clone());
    }
    // Held so the channel stays connected while a cancel may still come.
    let _sender = sender;

    // Reaped: from then on its process group must not be signalled, since
    // the id may belong to another group.
    let mut exited: Option<(ExitStatus, Instant)> = None;
    let mut terminated: Option<Instant> = None;
    let mut wrote_error: Option<Instant> = None;
    loop {
        let now = Instant::now();
        let mut wait = if open == 0 { EXIT_POLL } else { TICK };
        if let Some(terminated) = terminated {
            // Not reaped before the output closes: until then the group may
            // still have to be killed.
            if open == 0
                && let Some(status) = child.try_wait()?
            {
                // It may have finished before it was asked to leave.
                return Ok(if status.success() {
                    End::Exited(status)
                } else {
                    End::Cancelled
                });
            }
            let deadline = terminated + CANCEL_GRACE;
            if now >= deadline {
                kill_group(&mut child);
                child.wait()?;
                // What it wrote before it was killed.
                let closing = Instant::now() + EXIT_DRAIN;
                while open > 0 {
                    let left = closing.saturating_duration_since(Instant::now());
                    match events.recv_timeout(left) {
                        Ok(Event::Wrote(stream, bytes)) => output(stream, &bytes),
                        Ok(Event::Closed) => open -= 1,
                        Ok(Event::Cancel) => {}
                        Err(_) => break,
                    }
                }
                return Ok(End::Cancelled);
            }
            wait = wait.min(deadline - now);
        } else {
            if exited.is_none() {
                exited = child.try_wait()?.map(|status| (status, now));
            }
            match exited {
                Some((status, _)) if open == 0 => return Ok(End::Exited(status)),
                // A process git started holds the output open: see below.
                Some((_, at)) => wait = wait.min((at + EXIT_DRAIN).saturating_duration_since(now)),
                None => {
                    if let Some(at) = wrote_error {
                        let due = at + STALLED_AFTER;
                        if now >= due {
                            wrote_error = None;
                            if let Some(operation) = operation {
                                report_stalled(operation);
                            }
                        } else {
                            wait = wait.min(due - now);
                        }
                    }
                }
            }
        }
        match events.recv_timeout(wait) {
            Ok(Event::Wrote(stream, bytes)) => {
                if stream == Stream::Err && !bytes.is_empty() {
                    wrote_error = Some(Instant::now());
                    if let Some(operation) = operation {
                        let mut state = operation.state();
                        state.said.extend_from_slice(&bytes);
                        let excess = state.said.len().saturating_sub(SAID_LIMIT);
                        state.said.drain(..excess);
                    }
                }
                output(stream, &bytes);
            }
            Ok(Event::Closed) => open -= 1,
            // Nothing to end once it has exited; the run ends with what it
            // did.
            Ok(Event::Cancel) if terminated.is_none() && exited.is_none() => {
                terminate_group(&mut child);
                terminated = Some(Instant::now());
            }
            Ok(Event::Cancel) => {}
            Err(RecvTimeoutError::Timeout) => {
                // A process git started may hold the output open for as
                // long as it lives. Git's own output has arrived by now:
                // nothing is waiting to be read.
                if terminated.is_none()
                    && let Some((status, at)) = exited
                    && Instant::now() >= at + EXIT_DRAIN
                {
                    return Ok(End::Exited(status));
                }
            }
            // Cannot happen while `_sender` lives.
            Err(RecvTimeoutError::Disconnected) => open = 0,
        }
    }
}

/// Tells `operation` that the git running has written something and then
/// neither more nor exited.
fn report_stalled(operation: &Operation) {
    let (said, notify) = {
        let mut state = operation.state();
        let said = text(&state.said);
        state.stalled = Some(said.clone());
        (said, state.on_stalled.clone())
    };
    // Called without the lock, so it may use the operation.
    if let Some(notify) = notify
        && !said.is_empty()
    {
        notify(&said);
    }
}

/// Sends what `stream` yields to `events` from a thread of its own, then
/// that it closed.
fn read(mut stream: impl Read + Send + 'static, which: Stream, events: mpsc::Sender<Event>) {
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if events
                        .send(Event::Wrote(which, buffer[..read].to_vec()))
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = events.send(Event::Closed);
    });
}

/// Asks git and every process it started to leave. Only before git is
/// reaped, when the group is certainly still its own.
#[cfg(unix)]
fn terminate_group(child: &mut Child) {
    signal_group(child, rustix::process::Signal::TERM);
}

/// Ends git and every process it started, whatever they are doing. Only
/// before git is reaped.
#[cfg(unix)]
fn kill_group(child: &mut Child) {
    signal_group(child, rustix::process::Signal::KILL);
}

#[cfg(unix)]
fn signal_group(child: &Child, signal: rustix::process::Signal) {
    // Git leads the group, so the group's id is git's.
    let group = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw);
    if let Some(group) = group {
        // Fails only when the whole group has gone already.
        let _ = rustix::process::kill_process_group(group, signal);
    }
}

/// Without process groups there is no asking: git alone is ended.
#[cfg(not(unix))]
fn terminate_group(child: &mut Child) {
    let _ = child.kill();
}

#[cfg(not(unix))]
fn kill_group(child: &mut Child) {
    let _ = child.kill();
}

/// `bytes` as text, trimmed.
pub(crate) fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}
