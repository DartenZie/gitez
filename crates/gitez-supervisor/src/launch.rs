//! Starting a supervisor from the GUI.

use std::fs::OpenOptions;
use std::process::{Command as Process, Stdio};

use crate::{Command, Secrets};

const LOG_FILE: &str = "supervisor.log";

/// What [`launch`] starts.
pub struct Launch<'a> {
    /// The `gitez` binary (`std::env::current_exe()`); tests pass the
    /// `gitez-supervisor` test binary.
    pub exe: &'a std::path::Path,
    /// Args put before `start|reply|retry`: `["supervise"]` for `gitez`,
    /// `[]` for the test binary.
    pub prefix: &'a [&'a str],
    pub command: Command,
    pub common_dir: &'a std::path::Path,
    pub thread: &'a gitez_agent_thread::AgentThreadId,
    pub secrets: &'a Secrets,
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("could not start the supervisor")]
    Spawn(#[source] std::io::Error),
    #[error("could not pass the secrets to the supervisor")]
    Secrets(#[source] std::io::Error),
}

/// Spawns the supervisor detached: stdin piped (the secrets JSON is
/// written, then stdin closed), stdout and stderr appended to
/// `<thread dir>/supervisor.log`, the current environment inherited. A
/// background std thread waits for the child so it is reaped. Returns its
/// pid. Does not wait for the supervisor to take its lock.
///
/// The command line is `<exe> <prefix…> <start|reply|retry> --git-dir
/// <common_dir> --thread <id>`.
pub fn launch(launch: Launch<'_>) -> Result<u32, LaunchError> {
    let thread_dir = crate::thread_dir(launch.common_dir, launch.thread);
    std::fs::create_dir_all(&thread_dir).map_err(LaunchError::Spawn)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(thread_dir.join(LOG_FILE))
        .map_err(LaunchError::Spawn)?;
    let log_err = log.try_clone().map_err(LaunchError::Spawn)?;
    let mut child = Process::new(launch.exe)
        .args(launch.prefix)
        .arg(launch.command.as_str())
        .arg("--git-dir")
        .arg(launch.common_dir)
        .arg("--thread")
        .arg(launch.thread.as_str())
        .stdin(Stdio::piped())
        .stdout(log)
        .stderr(log_err)
        .spawn()
        .map_err(LaunchError::Spawn)?;
    let pid = child.id();
    let written = match child.stdin.take() {
        // Dropping the pipe closes it: the supervisor reads to EOF.
        Some(stdin) => launch.secrets.write_to(stdin),
        None => Err(std::io::Error::other("no pipe to the supervisor")),
    };
    // Reaped in the background, so it never lingers as a zombie of the GUI.
    std::thread::Builder::new()
        .name(format!("supervisor {pid}"))
        .spawn(move || {
            child.wait().ok();
        })
        .map_err(LaunchError::Spawn)?;
    // A supervisor whose secrets were cut short reads invalid JSON, marks
    // the thread failed and exits.
    written.map_err(LaunchError::Secrets)?;
    Ok(pid)
}
