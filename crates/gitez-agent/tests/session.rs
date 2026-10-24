//! Runs fake agents: shell scripts standing in for `claude`.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gitez_agent::{Agent, ClaudeCode, Error};
use tempfile::TempDir;

/// A script running `body`, in a fresh directory.
fn fake_agent(body: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("create temp dir");
    let script = dir.path().join("claude");
    std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    (dir, script)
}

#[test]
fn output_comes_line_by_line_and_the_prompt_goes_to_stdin() {
    let (dir, script) = fake_agent(
        r#"echo "args: $*"
pwd
while IFS= read -r line; do echo "prompt: $line"; done
echo "oops" >&2
exit 3"#,
    );
    let agent = ClaudeCode::with_program(&script);
    let mut session = agent
        .start("first line\nsecond line\n", dir.path())
        .expect("start");
    let mut lines = Vec::new();
    while let Some(line) = session.next_line().expect("read") {
        lines.push(line);
    }
    assert_eq!(
        lines[0],
        "args: --print --output-format stream-json --verbose --permission-mode bypassPermissions"
    );
    let cwd = std::fs::canonicalize(dir.path()).expect("canonical");
    assert_eq!(std::fs::canonicalize(&lines[1]).expect("canonical"), cwd);
    assert_eq!(lines[2..], ["prompt: first line", "prompt: second line"]);
    let outcome = session.finish().expect("finish");
    assert!(!outcome.success);
    assert!(!outcome.stopped);
    assert_eq!(outcome.code, Some(3));
    assert_eq!(outcome.stderr, "oops");
}

#[test]
fn a_stopped_agent_ends_its_output() {
    // The sleep is a child that shares the output, as a tool command would.
    let (dir, script) = fake_agent("echo started\nsleep 30\necho never");
    let mut session = ClaudeCode::with_program(&script)
        .start("", dir.path())
        .expect("start");
    assert_eq!(
        session.next_line().expect("read").as_deref(),
        Some("started")
    );
    let started = Instant::now();
    session.stopper().stop();
    assert_eq!(session.next_line().expect("read"), None);
    let outcome = session.finish().expect("finish");
    assert!(outcome.stopped);
    assert!(!outcome.success);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_missing_program_fails_to_start() {
    let dir = TempDir::new().expect("create temp dir");
    let agent = ClaudeCode::with_program(dir.path().join("nowhere"));
    let error = agent.start("", dir.path()).err().expect("no such program");
    assert!(matches!(error, Error::Start { .. }), "{error}");
}
