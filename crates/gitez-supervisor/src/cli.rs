//! `gitez supervise <start|reply|retry> --git-dir <common dir> --thread <id>`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use gitez_agent_thread::{AgentThreadId, Step, Store, Timestamp};

use crate::liveness::SupervisorLock;
use crate::pipeline::log;
use crate::{Command, Environment, Error, Secrets, run};

const USAGE: &str = "usage: gitez supervise <start|reply|retry> --git-dir <git common dir> \
                     --thread <id>   (secrets as JSON on standard input)";

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Args {
    command: Command,
    git_dir: PathBuf,
    thread: AgentThreadId,
}

/// `start|reply|retry --git-dir <path> --thread <id>`; secrets JSON on
/// stdin. Exit codes: 0 the thread was run to a final state (any ending),
/// 1 usage error or the thread could not be read/locked, 2 another
/// supervisor runs it.
pub fn main(args: impl Iterator<Item = std::ffi::OsString>) -> std::process::ExitCode {
    let args = match parse(args) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("gitez supervise: {message}\n{USAGE}");
            return ExitCode::from(1);
        }
    };
    // Before anything else, so the GUI's write never blocks.
    let secrets = Secrets::read_from(std::io::stdin().lock());
    // A session of its own: no SIGHUP from a terminal, and quitting the
    // GUI does not reach it. Fails only when it already leads a process
    // group (started from a shell), which is fine.
    rustix::process::setsid().ok();

    let Some(data_dir) = gitez_settings::data_dir() else {
        eprintln!("gitez supervise: GitEZ has no data folder (no home folder)");
        return ExitCode::from(1);
    };
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        if let Err(error) = signal_hook::flag::register(signal, stop.clone()) {
            eprintln!("gitez supervise: could not handle signal {signal}: {error}");
        }
    }
    // Ignored: registering a handler replaces "terminate".
    signal_hook::flag::register(
        signal_hook::consts::SIGHUP,
        Arc::new(AtomicBool::new(false)),
    )
    .ok();

    let secrets = match secrets {
        Ok(secrets) => secrets,
        Err(error) => {
            log(&args.thread, &error.to_string());
            fail_if_free(
                &args.git_dir,
                &args.thread,
                &format!("GitEZ could not pass the secrets to its supervisor: {error}"),
            );
            return ExitCode::from(1);
        }
    };
    let environment = Environment {
        settings_path: gitez_settings::Settings::path(&data_dir),
        sandbox: test_sandbox().unwrap_or_else(|| gitez_sandbox::platform(&data_dir)),
        data_dir,
        agents: gitez_agent::Registry::builtin(),
        stop,
        poll: Duration::from_secs(1),
    };
    log(
        &args.thread,
        &format!(
            "{} (pid {}, secrets: {})",
            args.command.as_str(),
            std::process::id(),
            secrets.names().join(", ")
        ),
    );
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(
            &environment,
            &args.git_dir,
            &args.thread,
            args.command,
            secrets,
        )
    }));
    match ran {
        Ok(Ok(_)) => ExitCode::SUCCESS,
        Ok(Err(Error::AlreadyRunning)) => {
            log(&args.thread, "another supervisor is already running it");
            ExitCode::from(2)
        }
        Ok(Err(error)) => {
            log(&args.thread, &error_chain(&error));
            ExitCode::from(1)
        }
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_owned());
            fail_if_free(
                &args.git_dir,
                &args.thread,
                &format!("GitEZ's supervisor crashed: {message}"),
            );
            ExitCode::from(1)
        }
    }
}

fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Args, String> {
    let command = args
        .next()
        .ok_or_else(|| "no subcommand".to_owned())?
        .into_string()
        .ok()
        .and_then(|name| Command::from_name(&name))
        .ok_or_else(|| "the subcommand must be start, reply or retry".to_owned())?;
    let mut git_dir = None;
    let mut thread = None;
    while let Some(arg) = args.next() {
        let arg = arg.to_string_lossy().into_owned();
        let value = args.next().ok_or_else(|| format!("{arg} needs a value"))?;
        match arg.as_str() {
            "--git-dir" => git_dir = Some(PathBuf::from(value)),
            "--thread" => {
                let value = value.to_string_lossy().into_owned();
                thread = Some(AgentThreadId::new(value).map_err(|error| error.to_string())?);
            }
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    Ok(Args {
        command,
        git_dir: git_dir.ok_or_else(|| "--git-dir is missing".to_owned())?,
        thread: thread.ok_or_else(|| "--thread is missing".to_owned())?,
    })
}

/// The pass-through sandbox, only in debug builds and only when
/// `GITEZ_TEST_SANDBOX=passthrough`. The pipeline then warns on the thread.
fn test_sandbox() -> Option<Arc<dyn gitez_sandbox::Sandbox>> {
    if cfg!(debug_assertions)
        && std::env::var_os("GITEZ_TEST_SANDBOX").is_some_and(|value| value == "passthrough")
    {
        Some(Arc::new(gitez_sandbox::Passthrough))
    } else {
        None
    }
}

/// Marks the thread failed (step Supervisor) if it is active and no
/// supervisor holds it.
fn fail_if_free(git_dir: &Path, id: &AgentThreadId, message: &str) {
    let store = Store::new(git_dir);
    let Ok(_lock) = SupervisorLock::take(&store.thread_dir(id)) else {
        return;
    };
    let failed = store.update(id, |thread| {
        if thread.state.is_active() {
            thread.fail(Step::Supervisor, message, Timestamp::now());
        }
        Ok(())
    });
    if let Err(error) = failed {
        log(id, &format!("could not mark the thread failed: {error}"));
    }
}

/// `error` and its sources, for the log.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args, String> {
        parse(list.iter().map(OsString::from))
    }

    #[test]
    fn parses_the_three_subcommands() {
        let parsed = args(&["reply", "--thread", "fix-it", "--git-dir", "/r/.git"]).expect("ok");
        assert_eq!(parsed.command, Command::Reply);
        assert_eq!(parsed.git_dir, PathBuf::from("/r/.git"));
        assert_eq!(parsed.thread.as_str(), "fix-it");
        assert!(args(&["start", "--git-dir", "/r/.git", "--thread", "a"]).is_ok());
        assert!(args(&["retry", "--git-dir", "/r/.git", "--thread", "a"]).is_ok());
    }

    #[test]
    fn refuses_bad_command_lines() {
        assert!(args(&[]).is_err());
        assert!(args(&["stop", "--git-dir", "/r", "--thread", "a"]).is_err());
        assert!(args(&["start", "--git-dir", "/r"]).is_err());
        assert!(args(&["start", "--thread", "a"]).is_err());
        assert!(args(&["start", "--git-dir", "/r", "--thread", "Not An Id"]).is_err());
        assert!(args(&["start", "--git-dir", "/r", "--thread"]).is_err());
        assert!(args(&["start", "--git-dir", "/r", "--thread", "a", "--x", "y"]).is_err());
    }
}
