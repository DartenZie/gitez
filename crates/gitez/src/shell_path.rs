//! Gives GitEZ the user's `PATH` when it is started outside a terminal.
//!
//! An app opened from Finder or a desktop launcher inherits a minimal `PATH`
//! (`/usr/bin:/bin:/usr/sbin:/sbin` on macOS). Programs that git runs on the
//! user's behalf, such as the `git-crypt` filter, `git-lfs`, credential
//! helpers and signing tools, then fail with "command not found", and a
//! git-crypt repository cannot even report its status. GitEZ therefore asks
//! the login shell for its `PATH` and adds the usual install directories.

use std::ffi::OsString;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long the login shell may take to print its `PATH`.
const SHELL_TIMEOUT: Duration = Duration::from_secs(3);

/// Surrounds the `PATH` in the shell's output.
const MARKER: &str = "__GITEZ_PATH__";

/// Replaces `PATH` with the login shell's, plus well-known install
/// directories. Does nothing when started from a terminal, whose `PATH` is
/// already the user's.
///
/// Must run before any other thread starts, because it changes the process
/// environment.
pub fn init() {
    if std::io::stdin().is_terminal() {
        return;
    }
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = login_shell_path()
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    dirs.extend(std::env::split_paths(&current));
    dirs.extend(well_known_dirs().into_iter().filter(|dir| dir.is_dir()));
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|dir| !dir.as_os_str().is_empty() && seen.insert(dir.clone()));
    let Ok(path) = std::env::join_paths(dirs) else {
        return;
    };
    if path != current {
        // SAFETY: called at the top of `main`, before GPUI or any other
        // thread exists, so nothing can read the environment concurrently.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("PATH", path);
        }
    }
}

/// The `PATH` that `$SHELL -l` ends up with, if it answers in time.
fn login_shell_path() -> Option<OsString> {
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let mut child = Command::new(shell)
        .args([
            "-l",
            "-c",
            &format!("printf '{MARKER}%s{MARKER}' \"$PATH\""),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if started.elapsed() < SHELL_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = Vec::new();
    child.stdout.take()?.read_to_end(&mut out).ok()?;
    // Profiles may print their own output; take what is between the markers.
    let out = String::from_utf8(out).ok()?;
    let (_, rest) = out.split_once(MARKER)?;
    let (path, _) = rest.split_once(MARKER)?;
    (!path.is_empty()).then(|| path.into())
}

/// Directories package managers install into, in case the shell did not
/// answer or does not set them up for non-interactive logins.
fn well_known_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = [
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
        "/usr/local/sbin",
        "/opt/local/bin",
        "/nix/var/nix/profiles/default/bin",
        "/home/linuxbrew/.linuxbrew/bin",
    ]
    .map(PathBuf::from)
    .into();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.extend([".local/bin", ".cargo/bin", ".nix-profile/bin"].map(|dir| home.join(dir)));
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::login_shell_path;

    #[test]
    fn the_login_shell_reports_a_path() {
        let path = login_shell_path().expect("the login shell answers");
        assert!(std::env::split_paths(&path).any(|dir| dir.is_dir()));
    }
}
