//! A child process described by a [`CommandSpec`]: its output line by line,
//! a way to stop it, and how it ended. It knows nothing about agents; an
//! agent only builds the command.

use std::io::{BufReader, Read as _, Write as _};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::{CommandSpec, Error, Result};

/// Keep this much of the end of the process's error output, to explain a
/// failure.
const STDERR_TAIL: usize = 8 * 1024;
/// The longest output line kept whole. Claude Code's stream-JSON lines are
/// far shorter (a tool result is cut long before this); a longer line is
/// cut here, so a program that prints without line ends cannot make GitEZ
/// hold all of it in memory.
pub const MAX_LINE: usize = 8 * 1024 * 1024;
/// Put at the end of a line cut at [`MAX_LINE`].
pub const CUT_MARK: &str = " [GitEZ cut this line: it was longer than 8 MiB]";
/// How often to check whether the process has exited once its output ended.
const EXIT_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// A running process, in a process group of its own.
pub struct AgentProcess {
    child: Arc<Mutex<Child>>,
    id: u32,
    stdout: BufReader<ChildStdout>,
    /// Collects the end of the error output; joined in [`AgentProcess::finish`].
    stderr: Option<JoinHandle<String>>,
    /// Writes the standard input; joined in [`AgentProcess::finish`].
    stdin: Option<JoinHandle<()>>,
    stopped: Arc<Mutex<bool>>,
}

/// Stops an [`AgentProcess`] from another thread.
#[derive(Clone)]
pub struct Stopper {
    child: Arc<Mutex<Child>>,
    stopped: Arc<Mutex<bool>>,
}

/// How the process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Whether it exited successfully. For an agent that is not whether its
    /// run succeeded: read that from its own output.
    pub success: bool,
    /// Whether [`Stopper::interrupt`] or [`Stopper::stop`] was called.
    pub stopped: bool,
    /// Its exit code, if it exited rather than being killed.
    pub code: Option<i32>,
    /// The end of its error output, trimmed.
    pub stderr: String,
}

impl AgentProcess {
    /// Spawns `command` in a new process group, stdout and stderr piped,
    /// stdin as `command.stdin` says.
    pub fn spawn(command: &CommandSpec) -> Result<Self> {
        let start_error = |source| Error::Start {
            program: command.program.clone(),
            source,
        };
        let mut process = Command::new(&command.program);
        process
            .args(&command.args)
            .stdin(if command.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !command.cwd.as_os_str().is_empty() {
            process.current_dir(&command.cwd);
        }
        for name in &command.env_remove {
            process.env_remove(name);
        }
        for (name, value) in &command.env {
            process.env(name, value);
        }
        // Its own process group, so stopping it also stops the commands it
        // runs, which share its output.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut process, 0);
        let mut child = process.spawn().map_err(start_error)?;
        let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
            // Piped above, so always there; stop the process rather than
            // leave it running unread.
            child.kill().ok();
            child.wait().ok();
            return Err(start_error(std::io::Error::other("no pipe to the process")));
        };
        // Written from a thread, so an input larger than the pipe's buffer
        // cannot block reading the output. Closing stdin ends the input.
        let stdin = match (command.stdin.clone(), child.stdin.take()) {
            (Some(input), Some(mut pipe)) => Some(std::thread::spawn(move || {
                // The process may exit without reading it all.
                pipe.write_all(input.as_bytes()).ok();
            })),
            _ => None,
        };
        let stderr = std::thread::spawn(move || {
            let mut tail = Vec::new();
            let mut buffer = [0_u8; 4096];
            while let Ok(read) = stderr.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                tail.extend_from_slice(&buffer[..read]);
                if tail.len() > STDERR_TAIL {
                    tail.drain(..tail.len() - STDERR_TAIL);
                }
            }
            String::from_utf8_lossy(&tail).trim().to_owned()
        });
        Ok(Self {
            id: child.id(),
            child: Arc::new(Mutex::new(child)),
            stdout: BufReader::new(stdout),
            stderr: Some(stderr),
            stdin,
            stopped: Arc::new(Mutex::new(false)),
        })
    }

    /// The process id, which is also the process group id.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The next line of output, without its line ending; `None` once the
    /// process and everything it started closed their output. A line longer
    /// than [`MAX_LINE`] bytes is cut there and ends with [`CUT_MARK`]; the
    /// rest of it is read and dropped.
    pub fn next_line(&mut self) -> Result<Option<String>> {
        read_line(&mut self.stdout, MAX_LINE).map_err(Error::Read)
    }

    pub fn stopper(&self) -> Stopper {
        Stopper {
            child: self.child.clone(),
            stopped: self.stopped.clone(),
        }
    }

    /// Waits for the process to exit.
    pub fn finish(mut self) -> Result<Outcome> {
        if let Some(stdin) = self.stdin.take() {
            stdin.join().ok();
        }
        // Polled rather than waited on, so the process stays free for the
        // `Stopper` to signal.
        let status = loop {
            if let Some(status) = lock(&self.child).try_wait().map_err(Error::Read)? {
                break status;
            }
            std::thread::sleep(EXIT_POLL);
        };
        let stderr = self
            .stderr
            .take()
            .and_then(|stderr| stderr.join().ok())
            .unwrap_or_default();
        Ok(Outcome {
            success: status.success(),
            stopped: *lock(&self.stopped),
            code: status.code(),
            stderr,
        })
    }
}

impl Stopper {
    /// Sends SIGINT to the whole process group: Claude Code ends its turn
    /// cleanly and reports a result. The process decides when it exits.
    pub fn interrupt(&self) {
        self.signal(Signal::Interrupt);
    }

    /// Kills the whole process group: the process and the commands it
    /// started. Its output then ends.
    pub fn stop(&self) {
        self.signal(Signal::Kill);
    }

    fn signal(&self, signal: Signal) {
        *lock(&self.stopped) = true;
        let mut child = lock(&self.child);
        // Already exited and reaped: its id may belong to another process
        // by now, so nothing is sent.
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        #[cfg(unix)]
        {
            use rustix::process::{Pid, Signal as Unix, kill_process_group};
            let pid = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
            if let Some(pid) = pid {
                let signal = match signal {
                    Signal::Interrupt => Unix::INT,
                    Signal::Kill => Unix::KILL,
                };
                // Fails only when the group is gone already.
                kill_process_group(pid, signal).ok();
            }
        }
        if signal == Signal::Kill {
            child.kill().ok();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    Interrupt,
    Kill,
}

/// The next line of `reader`, at most `max` bytes of it kept (see
/// [`AgentProcess::next_line`]).
fn read_line(reader: &mut impl std::io::BufRead, max: usize) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    let mut cut = false;
    let mut any = false;
    loop {
        let buffer = match reader.fill_buf() {
            Ok(buffer) => buffer,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if buffer.is_empty() {
            break;
        }
        any = true;
        let (chunk, ended) = match buffer.iter().position(|&byte| byte == b'\n') {
            Some(at) => (&buffer[..at], Some(at + 1)),
            None => (buffer, None),
        };
        let room = max.saturating_sub(line.len());
        if chunk.len() > room {
            cut = true;
        }
        line.extend_from_slice(&chunk[..chunk.len().min(room)]);
        let used = ended.unwrap_or(buffer.len());
        reader.consume(used);
        if ended.is_some() {
            break;
        }
    }
    if !any {
        return Ok(None);
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    let mut text = String::from_utf8_lossy(&line).into_owned();
    if cut {
        text.push_str(CUT_MARK);
    }
    Ok(Some(text))
}

/// `mutex` locked; a panic elsewhere while holding it does not matter for
/// a process handle or a flag.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `program` in the first directory of `path` (a `PATH` value) that has
/// it, or else the first of `candidates` that exists.
pub(crate) fn find_program(
    program: &str,
    path: Option<&std::ffi::OsStr>,
    candidates: &[PathBuf],
) -> Option<PathBuf> {
    path.into_iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(program))
        .chain(candidates.iter().cloned())
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_read_whole_and_without_their_ends() {
        let mut input = &b"one\r\ntwo\n\nlast"[..];
        let mut lines = Vec::new();
        while let Some(line) = read_line(&mut input, 100).expect("read") {
            lines.push(line);
        }
        assert_eq!(lines, ["one", "two", "", "last"]);
    }

    /// m2: an endless line is cut, and the lines after it still come.
    #[test]
    fn an_over_long_line_is_cut_and_the_next_one_still_read() {
        let mut text = vec![b'x'; 10_000];
        text.extend_from_slice(b"\nnext\n");
        let mut reader = std::io::BufReader::with_capacity(64, &text[..]);
        let first = read_line(&mut reader, 1000).expect("read").expect("a line");
        assert_eq!(first.len(), 1000 + CUT_MARK.len());
        assert!(first.ends_with(CUT_MARK));
        assert_eq!(
            read_line(&mut reader, 1000).expect("read").as_deref(),
            Some("next")
        );
        assert_eq!(read_line(&mut reader, 1000).expect("read"), None);
    }
}
