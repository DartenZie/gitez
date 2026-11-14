//! Runs fake agents: shell scripts standing in for `claude`, and a second
//! agent of the tests' own.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gitez_agent::{
    Agent, AgentConfig, AgentEvent, AgentNeeds, AgentProcess, Availability, ClaudeCode,
    CommandSpec, Conversation, Error, Finished, LoginStatus, ModelChoice, Registry, RunSpec,
};
use tempfile::TempDir;

/// A script named `claude` running `body`, in a fresh directory.
fn fake_agent(body: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("create temp dir");
    let script = dir.path().join("claude");
    std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    (dir, script)
}

/// Claude Code configured to run `script`, in a configuration folder of the
/// test's own, so nothing is looked up on this machine.
fn config(dir: &TempDir, script: &Path) -> AgentConfig {
    AgentConfig {
        program: Some(script.to_owned()),
        config_dir: Some(dir.path().join("config")),
        ..AgentConfig::default()
    }
}

fn command(dir: &TempDir, script: &Path, prompt: &str) -> CommandSpec {
    ClaudeCode
        .command(
            &config(dir, script),
            &RunSpec {
                prompt,
                images: &[],
                cwd: dir.path(),
                model: None,
                conversation: Conversation::New { session_id: "s-1" },
                system_prompt: None,
            },
        )
        .expect("command")
}

fn lines(process: &mut AgentProcess) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(line) = process.next_line().expect("read") {
        lines.push(line);
    }
    lines
}

#[test]
fn output_comes_line_by_line_and_the_prompt_goes_to_stdin() {
    let (dir, script) = fake_agent(
        r#"echo "args: $*"
pwd
echo "config: $CLAUDE_CONFIG_DIR"
echo "nested: ${CLAUDECODE:-no}"
while IFS= read -r line; do echo "prompt: $line"; done
echo "oops" >&2
exit 3"#,
    );
    let mut process =
        AgentProcess::spawn(&command(&dir, &script, "first line\nsecond line\n")).expect("spawn");
    assert!(process.id() > 0);
    let lines = lines(&mut process);
    assert_eq!(
        lines[0],
        "args: --print --output-format stream-json --verbose --permission-mode \
         bypassPermissions --settings {\"sandbox\":{\"enabled\":false}} --session-id s-1"
    );
    let cwd = std::fs::canonicalize(dir.path()).expect("canonical");
    assert_eq!(std::fs::canonicalize(&lines[1]).expect("canonical"), cwd);
    assert_eq!(
        lines[2],
        format!("config: {}", dir.path().join("config").display())
    );
    assert_eq!(lines[3], "nested: no");
    assert_eq!(lines[4..], ["prompt: first line", "prompt: second line"]);
    let outcome = process.finish().expect("finish");
    assert!(!outcome.success);
    assert!(!outcome.stopped);
    assert_eq!(outcome.code, Some(3));
    assert_eq!(outcome.stderr, "oops");
}

#[test]
fn without_input_stdin_is_empty() {
    let (dir, script) = fake_agent("cat; echo end");
    let command = CommandSpec {
        program: script,
        cwd: dir.path().to_owned(),
        stdin: None,
        ..CommandSpec::default()
    };
    let mut process = AgentProcess::spawn(&command).expect("spawn");
    assert_eq!(lines(&mut process), ["end"]);
    assert!(process.finish().expect("finish").success);
}

#[test]
fn the_environment_is_changed_as_the_command_says() {
    let (dir, script) = fake_agent(r#"echo "${GITEZ_TEST_GONE:-unset} $GITEZ_TEST_SET""#);
    let command = CommandSpec {
        program: script,
        env: vec![
            ("GITEZ_TEST_SET".to_owned(), "first".to_owned()),
            ("GITEZ_TEST_SET".to_owned(), "second".to_owned()),
        ],
        // Not set in the test's environment either; removing it is harmless.
        env_remove: vec!["GITEZ_TEST_GONE".to_owned()],
        cwd: dir.path().to_owned(),
        ..CommandSpec::default()
    };
    let mut process = AgentProcess::spawn(&command).expect("spawn");
    assert_eq!(lines(&mut process), ["unset second"]);
}

#[test]
fn the_end_of_the_error_output_is_kept() {
    // 20 KiB of error output, then a last line.
    let (dir, script) = fake_agent(
        "i=0; while [ $i -lt 2000 ]; do echo 'xxxxxxxxx' >&2; i=$((i+1)); done; echo last >&2",
    );
    let command = CommandSpec {
        program: script,
        cwd: dir.path().to_owned(),
        ..CommandSpec::default()
    };
    let process = AgentProcess::spawn(&command).expect("spawn");
    let outcome = process.finish().expect("finish");
    assert!(outcome.success);
    assert!(outcome.stderr.len() <= 8 * 1024);
    assert!(outcome.stderr.ends_with("xxxxxxxxx\nlast"));
}

#[test]
fn an_interrupt_lets_the_agent_end_its_turn() {
    let (dir, script) = fake_agent(
        // `wait` returns as soon as a trapped signal arrives; a foreground
        // `sleep` would delay the trap if the signal came before it started.
        r#"trap 'kill $pid; echo "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true}"; exit 0' INT
sleep 30 &
pid=$!
echo started
wait $pid
echo never"#,
    );
    let mut process = AgentProcess::spawn(&command(&dir, &script, "")).expect("spawn");
    assert_eq!(
        process.next_line().expect("read").as_deref(),
        Some("started")
    );
    let started = Instant::now();
    process.stopper().interrupt();
    let rest = lines(&mut process);
    assert_eq!(rest.len(), 1, "{rest:?}");
    assert!(matches!(
        ClaudeCode.parse(&rest[0]),
        Some(AgentEvent::Finished(Finished { success: false, .. }))
    ));
    let outcome = process.finish().expect("finish");
    assert!(outcome.stopped);
    assert!(outcome.success);
    assert_eq!(outcome.code, Some(0));
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_stopped_agent_ends_its_output() {
    // The sleep is a child that shares the output, as a tool command would.
    let (dir, script) = fake_agent("echo started\nsleep 30\necho never");
    let mut process = AgentProcess::spawn(&command(&dir, &script, "")).expect("spawn");
    assert_eq!(
        process.next_line().expect("read").as_deref(),
        Some("started")
    );
    let started = Instant::now();
    process.stopper().stop();
    assert_eq!(process.next_line().expect("read"), None);
    let outcome = process.finish().expect("finish");
    assert!(outcome.stopped);
    assert!(!outcome.success);
    assert_eq!(outcome.code, None);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn stopping_an_exited_process_does_nothing() {
    let (dir, script) = fake_agent("echo done");
    let mut process = AgentProcess::spawn(&command(&dir, &script, "")).expect("spawn");
    let stopper = process.stopper();
    assert_eq!(lines(&mut process), ["done"]);
    let outcome = process.finish().expect("finish");
    stopper.stop();
    stopper.interrupt();
    assert!(outcome.success);
    assert!(!outcome.stopped);
}

#[test]
fn a_missing_program_fails_to_start() {
    let dir = TempDir::new().expect("create temp dir");
    let command = CommandSpec {
        program: dir.path().join("nowhere"),
        cwd: dir.path().to_owned(),
        ..CommandSpec::default()
    };
    let error = AgentProcess::spawn(&command)
        .err()
        .expect("no such program");
    assert!(matches!(error, Error::Start { .. }), "{error}");

    let config = AgentConfig {
        program: Some(dir.path().join("nowhere")),
        ..AgentConfig::default()
    };
    let error = ClaudeCode.locate(&config).expect_err("no such program");
    assert!(matches!(error, Error::NotAProgram { .. }), "{error}");
}

#[test]
fn the_login_status_comes_from_the_agent() {
    let (dir, script) = fake_agent(
        r#"[ "$*" = "auth status --json" ] || exit 2
[ -n "$CLAUDE_CONFIG_DIR" ] || exit 2
echo '{"loggedIn":true,"authMethod":"claude.ai"}'"#,
    );
    assert_eq!(
        ClaudeCode.login_status(&config(&dir, &script)),
        LoginStatus::SignedIn
    );

    let (dir, script) = fake_agent("echo 'Not logged in' >&2; exit 1");
    assert_eq!(
        ClaudeCode.login_status(&config(&dir, &script)),
        LoginStatus::SignedOut
    );

    let (dir, script) = fake_agent(r#"echo '{"loggedIn":false}'; exit 1"#);
    assert_eq!(
        ClaudeCode.login_status(&config(&dir, &script)),
        LoginStatus::SignedOut
    );

    let (dir, script) = fake_agent("echo 'something else'");
    assert!(matches!(
        ClaudeCode.login_status(&config(&dir, &script)),
        LoginStatus::Unknown(_)
    ));

    let dir = TempDir::new().expect("create temp dir");
    let missing = dir.path().join("nowhere");
    assert!(matches!(
        ClaudeCode.login_status(&config(&dir, &missing)),
        LoginStatus::Unknown(_)
    ));
}

#[test]
fn a_hanging_login_status_gives_up() {
    let (dir, script) = fake_agent("sleep 30");
    let started = Instant::now();
    assert!(matches!(
        ClaudeCode.login_status(&config(&dir, &script)),
        LoginStatus::Unknown(_)
    ));
    assert!(started.elapsed() < Duration::from_secs(15));
}

/// A second agent: prints the prompt back as its only event.
struct Echo;

impl Agent for Echo {
    fn id(&self) -> &'static str {
        "echo"
    }

    fn name(&self) -> &'static str {
        "Echo"
    }

    fn models(&self) -> &'static [ModelChoice] {
        &[("echo", "Echo")]
    }

    fn locate(&self, config: &AgentConfig) -> gitez_agent::Result<PathBuf> {
        config
            .program
            .clone()
            .or_else(|| Some(PathBuf::from("/bin/cat")))
            .filter(|program| program.is_file())
            .ok_or_else(|| Error::NotInstalled {
                name: "Echo",
                program: "cat",
                searched: "/bin".to_owned(),
            })
    }

    fn command(&self, config: &AgentConfig, run: &RunSpec<'_>) -> gitez_agent::Result<CommandSpec> {
        Ok(CommandSpec {
            program: self.locate(config)?,
            cwd: run.cwd.to_owned(),
            stdin: Some(format!("echo:{}\n", run.prompt)),
            ..CommandSpec::default()
        })
    }

    fn needs(&self, _config: &AgentConfig) -> AgentNeeds {
        AgentNeeds::default()
    }

    fn parse(&self, line: &str) -> Option<AgentEvent> {
        line.strip_prefix("echo:")
            .map(|text| AgentEvent::Text(text.to_owned()))
    }

    fn login_status(&self, _config: &AgentConfig) -> LoginStatus {
        LoginStatus::SignedIn
    }
}

#[test]
fn another_agent_runs_through_the_registry() {
    let registry = Registry::with(vec![Arc::new(Echo), Arc::new(ClaudeCode)]);
    assert_eq!(registry.all().len(), 2);
    assert!(registry.get("nobody").is_none());
    let echo = registry.get("echo").expect("registered");
    let dir = TempDir::new().expect("create temp dir");
    let run = RunSpec {
        prompt: "hello",
        images: &[],
        cwd: dir.path(),
        model: None,
        conversation: Conversation::New { session_id: "s-1" },
        system_prompt: None,
    };
    let config = AgentConfig::default();
    assert_eq!(echo.needs_for_run(&config, &run), AgentNeeds::default());
    let mut process =
        AgentProcess::spawn(&echo.command(&config, &run).expect("command")).expect("spawn");
    let events: Vec<AgentEvent> = lines(&mut process)
        .iter()
        .filter_map(|line| echo.parse(line))
        .collect();
    assert_eq!(events, [AgentEvent::Text("hello".to_owned())]);
    assert!(process.finish().expect("finish").success);

    let (_dir, script) = fake_agent("exit 0");
    let missing = dir.path().join("nowhere");
    let statuses = registry.statuses(|id| AgentConfig {
        program: Some(if id == "echo" {
            missing.clone()
        } else {
            script.clone()
        }),
        ..AgentConfig::default()
    });
    assert_eq!(statuses[0].id, "echo");
    assert!(matches!(
        statuses[0].availability,
        Availability::Unavailable { .. }
    ));
    assert_eq!(statuses[1].name, "Claude Code");
    assert_eq!(
        statuses[1].availability,
        Availability::Installed { program: script }
    );
}

#[test]
fn the_builtin_registry_has_claude_code() {
    let registry = Registry::builtin();
    let agent = registry.get(ClaudeCode::ID).expect("built in");
    assert_eq!(agent.name(), "Claude Code");
    assert_eq!(registry.all().len(), 1);
}
