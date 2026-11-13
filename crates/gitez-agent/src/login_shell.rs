//! What the user's login shell sets up, asked once per process.
//!
//! An app opened from Finder or a desktop launcher does not inherit what
//! the user's shell profiles export, such as `CLAUDE_CONFIG_DIR` or the
//! full `PATH`. The binary's `shell_path` module fixes GitEZ's own `PATH`
//! the same way; this crate asks on its own so it does not depend on the
//! binary having done so.

use std::ffi::OsString;
use std::io::Read as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long the login shell may take to answer.
const SHELL_TIMEOUT: Duration = Duration::from_secs(3);

/// Separates the values in the shell's output; profiles may print their
/// own output around them.
const MARKER: &str = "__GITEZ_LOGIN_SHELL__";

/// The variables GitEZ asks the login shell for, in the order printed.
const VARIABLES: [&str; 2] = ["PATH", "CLAUDE_CONFIG_DIR"];

/// What `$SHELL -l` exports, as far as GitEZ needs it.
#[derive(Debug, Clone, Default)]
struct LoginShell {
    path: Option<OsString>,
    claude_config_dir: Option<OsString>,
}

/// The value of `name` (`PATH` or `CLAUDE_CONFIG_DIR`) in the user's login
/// shell; `None` when it is unset or empty there, when the shell does not
/// answer within a few seconds, or for any other variable. The shell is
/// started at most once per process; later calls return the same answer.
pub fn login_shell_var(name: &str) -> Option<OsString> {
    static ANSWER: OnceLock<LoginShell> = OnceLock::new();
    let answer = ANSWER.get_or_init(ask);
    match name {
        "PATH" => answer.path.clone(),
        "CLAUDE_CONFIG_DIR" => answer.claude_config_dir.clone(),
        _ => None,
    }
}

fn ask() -> LoginShell {
    let Some(out) = run_login_shell() else {
        return LoginShell::default();
    };
    let mut values = parse(&out).into_iter();
    LoginShell {
        path: values.next().flatten(),
        claude_config_dir: values.next().flatten(),
    }
}

/// The shell's output, if it exits successfully within the timeout.
fn run_login_shell() -> Option<String> {
    let shell = std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".into());
    let format = VARIABLES.map(|_| "%s").join(MARKER);
    let values = VARIABLES.map(|name| format!("\"${name}\"")).join(" ");
    let script = format!("printf '{MARKER}{format}{MARKER}' {values}");
    let mut child = Command::new(shell)
        .args(["-l", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Read from a thread, so a chatty profile cannot fill the pipe and
    // stall the shell before it exits.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).ok();
        out
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if started.elapsed() < SHELL_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                child.kill().ok();
                child.wait().ok();
                return None;
            }
        }
    }
    String::from_utf8(reader.join().ok()?).ok()
}

/// The values between the first and the last marker, one per variable;
/// empty values are `None`.
fn parse(out: &str) -> Vec<Option<OsString>> {
    let Some((_, rest)) = out.split_once(MARKER) else {
        return Vec::new();
    };
    let Some((values, _)) = rest.rsplit_once(MARKER) else {
        return Vec::new();
    };
    let values: Vec<_> = values
        .split(MARKER)
        .map(|value| (!value.is_empty()).then(|| value.into()))
        .collect();
    if values.len() == VARIABLES.len() {
        values
    } else {
        Vec::new()
    }
}

/// `dirs` joined as a `PATH`, empty and repeated entries dropped.
pub(crate) fn join_path(dirs: impl IntoIterator<Item = PathBuf>) -> Option<OsString> {
    let mut seen = std::collections::HashSet::new();
    let dirs: Vec<PathBuf> = dirs
        .into_iter()
        .filter(|dir| !dir.as_os_str().is_empty() && seen.insert(dir.clone()))
        .collect();
    std::env::join_paths(dirs).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_between_the_markers_are_read() {
        let out =
            format!("Welcome!\n{MARKER}/usr/bin:/bin{MARKER}/Users/me/.config/claude{MARKER}bye");
        assert_eq!(
            parse(&out),
            [
                Some("/usr/bin:/bin".into()),
                Some("/Users/me/.config/claude".into())
            ]
        );
    }

    #[test]
    fn an_unset_variable_is_none() {
        let out = format!("{MARKER}/bin{MARKER}{MARKER}");
        assert_eq!(parse(&out), [Some("/bin".into()), None]);
        assert!(parse("no markers").is_empty());
    }

    #[test]
    fn the_login_shell_reports_a_path() {
        let path = login_shell_var("PATH").expect("the login shell answers");
        assert!(std::env::split_paths(&path).any(|dir| dir.is_dir()));
        assert_eq!(login_shell_var("HOME"), None);
    }

    #[test]
    fn a_path_has_no_repeated_entries() {
        let path = join_path(["/a", "", "/b", "/a"].map(PathBuf::from)).expect("joins");
        assert_eq!(path, "/a:/b");
    }
}
