//! An agent running as a child process: its output line by line, a way to
//! stop it, and how it ended.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::{Error, Result};

/// Keep this much of the end of the agent's error output, to explain a
/// failure.
const STDERR_TAIL: usize = 8 * 1024;
/// How often to check whether the agent has exited once its output ended.
const EXIT_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// A running agent.
pub struct Session {
    child: Arc<Mutex<Child>>,
    stdout: BufReader<ChildStdout>,
    /// Collects the end of the error output; joined in [`Session::finish`].
    stderr: Option<JoinHandle<String>>,
    /// Writes the prompt; joined in [`Session::finish`].
    stdin: Option<JoinHandle<()>>,
    stopped: Arc<Mutex<bool>>,
}

/// Stops a [`Session`] from another thread.
#[derive(Clone)]
pub struct Stopper {
    child: Arc<Mutex<Child>>,
    stopped: Arc<Mutex<bool>>,
}

/// How the agent's process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Whether it exited successfully.
    pub success: bool,
    /// Whether it was stopped with [`Stopper::stop`].
    pub stopped: bool,
    /// Its exit code, if it exited rather than being killed.
    pub code: Option<i32>,
    /// The end of its error output, trimmed.
    pub stderr: String,
}

impl Session {
    /// Runs `program` with `args` in `dir`, writing `input` to its standard
    /// input.
    pub(crate) fn start(program: &Path, args: &[&str], dir: &Path, input: &str) -> Result<Self> {
        let start_error = |source| Error::Start {
            program: program.to_owned(),
            source,
        };
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Its own process group, so stopping it also stops the commands it
        // runs, which share its output.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command.spawn().map_err(start_error)?;
        let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            // Piped above, so always there; stop the process rather than
            // leave it waiting.
            child.kill().ok();
            return Err(start_error(std::io::Error::other("no pipe to the process")));
        };
        // Written from a thread, so a prompt larger than the pipe's buffer
        // cannot block reading the output. Closing stdin ends the prompt.
        let input = input.to_owned();
        let stdin = std::thread::spawn(move || {
            // The process may exit without reading it all.
            stdin.write_all(input.as_bytes()).ok();
        });
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
            child: Arc::new(Mutex::new(child)),
            stdout: BufReader::new(stdout),
            stderr: Some(stderr),
            stdin: Some(stdin),
            stopped: Arc::new(Mutex::new(false)),
        })
    }

    /// The next line of output, without its line ending; `None` once the
    /// agent closed its output.
    pub fn next_line(&mut self) -> Result<Option<String>> {
        let mut line = Vec::new();
        let read = self
            .stdout
            .read_until(b'\n', &mut line)
            .map_err(Error::Read)?;
        if read == 0 {
            return Ok(None);
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        Ok(Some(String::from_utf8_lossy(&line).into_owned()))
    }

    pub fn stopper(&self) -> Stopper {
        Stopper {
            child: self.child.clone(),
            stopped: self.stopped.clone(),
        }
    }

    /// Waits for the agent to exit.
    pub fn finish(mut self) -> Result<Outcome> {
        if let Some(stdin) = self.stdin.take() {
            stdin.join().ok();
        }
        // Polled rather than waited on, so the process stays free for
        // `Stopper::stop` to kill.
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
    /// Ends the agent's process. Its output then ends too.
    pub fn stop(&self) {
        *lock(&self.stopped) = true;
        let mut child = lock(&self.child);
        // Already exited: nothing to stop.
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        #[cfg(unix)]
        {
            // The whole group: the agent and the commands it started.
            Command::new("kill")
                .arg("-KILL")
                .arg(format!("-{}", child.id()))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .ok();
        }
        child.kill().ok();
    }
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
