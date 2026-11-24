//! The supervisor as its own process, started with `launch` as the GUI
//! does (DESIGN G, P6 acceptance tests 5, 8, 9, 10, 12). The binary runs
//! with this test's data folder and `GITEZ_TEST_SANDBOX=passthrough`.
#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command as Process, Stdio};
use std::time::{Duration, Instant};

use common::{Fixture, is_done};
use gitez_agent_thread::{AgentThread, Ending, State, Step, Timestamp};
use gitez_supervisor::{Command, Secrets, is_running, reap_if_dead, stop};

const LONG: Duration = Duration::from_secs(30);

fn running_agent(thread: &AgentThread) -> bool {
    thread.state == State::Running
        && thread
            .supervisor
            .as_ref()
            .is_some_and(|supervisor| supervisor.child_group.is_some())
}

/// Waits until the fake agent has printed its lines and set its trap.
fn wait_for_output(fixture: &Fixture, id: &gitez_agent_thread::AgentThreadId) {
    wait_until(LONG, "the agent's output", || {
        fixture
            .transcripts(id)
            .first()
            .is_some_and(|(_, lines)| lines.iter().any(|line| line.contains("tool_use")))
    });
}

fn group_exists(group: u32) -> bool {
    let pid = rustix::process::Pid::from_raw(group as i32).expect("a pid");
    rustix::process::test_kill_process_group(pid).is_ok()
}

fn kill_9(pid: u32) {
    let pid = rustix::process::Pid::from_raw(pid as i32).expect("a pid");
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).expect("kill -9");
}

fn wait_until(timeout: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn stop_interrupts_the_agent_and_ends_the_thread_as_stopped() {
    let fixture = Fixture::new();
    let id = fixture.create("SLEEP until stopped").id;
    let thread_dir = fixture.thread_dir(&id);
    fixture.launch(&id, Command::Start, &Secrets::default());
    let running = fixture.wait_for(&id, LONG, running_agent);
    assert!(is_running(&thread_dir));
    let group = running
        .supervisor
        .as_ref()
        .and_then(|supervisor| supervisor.child_group)
        .expect("child group");

    // The fake traps SIGINT once it has printed its lines.
    wait_for_output(&fixture, &id);
    let stopped_at = Instant::now();
    stop(
        &thread_dir,
        running.supervisor.as_ref().expect("supervisor"),
    )
    .expect("stop");
    let thread = fixture.wait_for(&id, Duration::from_secs(25), is_done);
    assert!(stopped_at.elapsed() < Duration::from_secs(25));
    assert_eq!(thread.state, State::Finished);
    assert_eq!(thread.ending, Some(Ending::Stopped), "{thread:#?}");
    assert!(thread.error.is_none());
    // Interrupted, not killed: the agent's trap ran.
    assert!(fixture.fake_log.join("trapped").is_file());
    let result = thread.turns[0].result.as_ref().expect("result");
    assert!(result.stopped);
    assert!(thread.turns[0].ended.is_some());
    wait_until(Duration::from_secs(5), "the supervisor to exit", || {
        !is_running(&thread_dir)
    });
    assert!(!group_exists(group));
    assert!(matches!(
        stop(
            &thread_dir,
            running.supervisor.as_ref().expect("supervisor")
        ),
        Err(gitez_supervisor::StopError::NotRunning)
    ));
}

/// Whether process `pid` is still there (a zombie counts until reaped).
fn process_exists(pid: u32) -> bool {
    let pid = rustix::process::Pid::from_raw(pid as i32).expect("a pid");
    rustix::process::test_kill_process(pid).is_ok()
}

/// The git GitEZ runs itself to make the worktree is in a process group
/// of its own, which no signal to the supervisor reaches: Stop cancels it,
/// with the user's hook it waits for.
#[test]
fn stop_ends_the_git_that_makes_the_worktree_with_its_hook() {
    let fixture = Fixture::new();
    // `git worktree add` runs the main repository's post-checkout hook,
    // which here never ends by itself.
    let pids = fixture.base.join("hook pids");
    common::write_script(
        &fixture.base.join("no hooks").join("post-checkout"),
        &format!(
            "#!/bin/sh\necho $$ > '{pids}.tmp'\nsleep 600 &\necho $! >> '{pids}.tmp'\n\
             mv '{pids}.tmp' '{pids}'\nwait\n",
            pids = pids.display()
        ),
    );
    let id = fixture.create("What is the answer?").id;
    let thread_dir = fixture.thread_dir(&id);
    fixture.launch(&id, Command::Start, &Secrets::default());
    wait_until(LONG, "the hook to start", || pids.is_file());
    let hook: Vec<u32> = std::fs::read_to_string(&pids)
        .expect("read the pids")
        .lines()
        .map(|line| line.trim().parse().expect("a pid"))
        .collect();
    assert_eq!(hook.len(), 2, "{hook:?}");
    let preparing = fixture.load(&id);
    assert_eq!(preparing.state, State::Preparing, "{preparing:#?}");

    let stopped_at = Instant::now();
    stop(
        &thread_dir,
        preparing.supervisor.as_ref().expect("supervisor"),
    )
    .expect("stop");
    let thread = fixture.wait_for(&id, Duration::from_secs(15), is_done);
    // The cancel's grace period is 2 s, the stop is noticed within 0.1 s.
    assert!(stopped_at.elapsed() < Duration::from_secs(10));
    assert_eq!(thread.ending, Some(Ending::Stopped), "{thread:#?}");
    assert!(thread.error.is_none(), "{thread:#?}");
    assert!(fixture.calls().is_empty(), "the agent never ran");
    wait_until(
        Duration::from_secs(5),
        "the hook and its child to go",
        || hook.iter().all(|pid| !process_exists(*pid)),
    );
    wait_until(Duration::from_secs(5), "the supervisor to exit", || {
        !is_running(&thread_dir)
    });
}

#[test]
fn the_global_limit_queues_threads_in_order_and_cleans_dead_tickets() {
    let fixture = Fixture::with_limit(1);
    let first = fixture.create("SLEEP until released").id;
    let second = fixture.create("What is the answer?").id;
    let third = fixture.create("What else is the answer?").id;
    let tickets = fixture.data_dir.join("queue").join("tickets");
    let count_tickets = || {
        std::fs::read_dir(&tickets)
            .map(|entries| entries.count())
            .unwrap_or(0)
    };

    fixture.launch(&first, Command::Start, &Secrets::default());
    fixture.wait_for(&first, LONG, running_agent);
    fixture.launch(&second, Command::Start, &Secrets::default());
    wait_until(LONG, "the second ticket", || count_tickets() == 1);
    fixture.launch(&third, Command::Start, &Secrets::default());
    wait_until(LONG, "the third ticket", || count_tickets() == 2);
    let third_supervisor = fixture.wait_for(&third, LONG, |thread| thread.supervisor.is_some());

    // While the first runs, the others wait.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(fixture.load(&second).state, State::Queued);
    assert_eq!(fixture.load(&third).state, State::Queued);

    // A waiting supervisor that dies leaves a ticket nobody holds: the
    // next poll of the one before it deletes it.
    kill_9(third_supervisor.supervisor.expect("supervisor").pid);
    wait_until(LONG, "the dead ticket to go", || count_tickets() == 1);
    assert!(
        reap_if_dead(&fixture.store(), &third, Timestamp::now()).expect("reap"),
        "the dead waiting thread is reaped"
    );
    let reaped = fixture.load(&third);
    assert_eq!(reaped.ending, Some(Ending::Failed));
    assert_eq!(reaped.error.expect("error").step, Step::Supervisor);

    std::fs::write(fixture.fake_log.join("release"), "").expect("release");
    let first_thread = fixture.wait_for(&first, LONG, is_done);
    let second_thread = fixture.wait_for(&second, LONG, is_done);
    assert_eq!(first_thread.ending, Some(Ending::Answer));
    assert_eq!(second_thread.ending, Some(Ending::Answer));
    let first_ended = first_thread.turns[0].ended.expect("ended");
    let second_started = second_thread.turns[0].started.expect("started");
    assert!(
        second_started >= first_ended,
        "{second_started} < {first_ended}"
    );
    wait_until(LONG, "the queue to empty", || count_tickets() == 0);
}

#[test]
fn a_killed_supervisor_is_reaped_and_its_agent_killed() {
    let fixture = Fixture::new();
    let id = fixture.create("SLEEP forever").id;
    let thread_dir = fixture.thread_dir(&id);
    fixture.launch(&id, Command::Start, &Secrets::default());
    let running = fixture.wait_for(&id, LONG, running_agent);
    let supervisor = running.supervisor.expect("supervisor");
    let group = supervisor.child_group.expect("child group");
    // Alive and well: nothing to reap.
    assert!(!reap_if_dead(&fixture.store(), &id, Timestamp::now()).expect("reap"));

    // A write after its supervisor died would end the fake with SIGPIPE.
    wait_for_output(&fixture, &id);
    kill_9(supervisor.pid);
    wait_until(Duration::from_secs(5), "the lock to be released", || {
        !is_running(&thread_dir)
    });
    assert!(group_exists(group), "the agent outlives its supervisor");
    assert!(reap_if_dead(&fixture.store(), &id, Timestamp::now()).expect("reap"));
    let thread = fixture.load(&id);
    assert_eq!(thread.state, State::Finished);
    assert_eq!(thread.ending, Some(Ending::Failed));
    let error = thread.error.expect("error");
    assert_eq!(error.step, Step::Supervisor);
    assert_eq!(
        error.message,
        "GitEZ's supervisor for this thread stopped unexpectedly. Retry to continue."
    );
    assert!(thread.supervisor.is_none());
    wait_until(Duration::from_secs(5), "the agent's group to go", || {
        !group_exists(group)
    });
    // Once is enough.
    assert!(!reap_if_dead(&fixture.store(), &id, Timestamp::now()).expect("reap"));
}

#[test]
fn a_thread_waiting_for_its_supervisor_is_not_reaped_at_once() {
    let fixture = Fixture::new();
    let id = fixture.create("What is the answer?").id;
    let now = Timestamp::now();
    assert!(!reap_if_dead(&fixture.store(), &id, now).expect("reap"));
    let later = now
        .checked_add(jiff::SignedDuration::from_secs(16))
        .expect("time");
    assert!(reap_if_dead(&fixture.store(), &id, later).expect("reap"));
}

/// Every file under `dir` whose content holds `needle`.
fn files_containing(dir: &Path, needle: &str) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            found.extend(files_containing(&path, needle));
        } else if kind.is_file()
            && std::fs::read(&path)
                .is_ok_and(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
        {
            found.push(path.display().to_string());
        }
    }
    found
}

#[test]
fn secrets_reach_the_agent_and_nothing_gitez_writes() {
    let fixture = Fixture::new();
    let value = "s3cr3t-value-0042";
    fixture.set_setup_command("echo \"setup sees $DEMO_SECRET\"; touch setup-ran");
    let id = fixture
        .create_with_secrets("SECRET please", &["DEMO_SECRET"])
        .id;
    let secrets = Secrets::new(BTreeMap::from([(
        "DEMO_SECRET".to_owned(),
        value.to_owned(),
    )]));
    fixture.launch(&id, Command::Start, &secrets);
    let thread = fixture.wait_for(&id, LONG, is_done);
    assert_eq!(thread.ending, Some(Ending::Answer), "{thread:#?}");
    // The agent saw it (and said so); GitEZ wrote the name instead.
    assert_eq!(
        thread.final_message(),
        Some("Saw [secret DEMO_SECRET] in my environment.")
    );
    assert_eq!(thread.secrets, ["DEMO_SECRET"]);
    let (_, lines) = fixture.transcripts(&id).remove(0);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("The secret is [secret DEMO_SECRET]"))
    );
    let thread_dir = fixture.thread_dir(&id);
    let setup_log = std::fs::read_to_string(thread_dir.join("setup.log")).expect("setup.log");
    assert!(
        setup_log.contains("setup sees [secret DEMO_SECRET]"),
        "{setup_log}"
    );
    assert!(thread_dir.join("supervisor.log").is_file());
    // Not in thread.json, the transcript, supervisor.log, setup.log, nor
    // anywhere else in the git dir or the data folder.
    assert_eq!(
        files_containing(&fixture.common_dir(), value),
        Vec::<String>::new()
    );
    assert_eq!(
        files_containing(&fixture.data_dir, value),
        Vec::<String>::new()
    );
    // Not in the agent's arguments.
    let calls = fixture.calls();
    assert_eq!(calls.len(), 1);
    assert!(!calls[0].contains(value));
}

/// A shipped `gitez` ignores `GITEZ_TEST_SANDBOX=passthrough`: with no
/// sandbox installed, the thread fails before the agent runs. Runs only
/// when `GITEZ_RELEASE_EXE` names a release build of `gitez` (`cargo build
/// --release -p gitez`, then `GITEZ_RELEASE_EXE=target/release/gitez`),
/// on a machine where `srt` is not on the `PATH`.
#[test]
fn a_release_build_never_runs_without_a_sandbox() {
    let Some(exe) = std::env::var_os("GITEZ_RELEASE_EXE") else {
        return;
    };
    let exe = Path::new(&exe).canonicalize().expect("the release gitez");
    let fixture = Fixture::new();
    let id = fixture.create("COMMIT").id;
    let script = fixture.gitez_exe(&exe);
    gitez_supervisor::launch(gitez_supervisor::Launch {
        exe: &script,
        prefix: &[],
        command: Command::Start,
        common_dir: &fixture.common_dir(),
        thread: &id,
        secrets: &Secrets::default(),
    })
    .expect("launch the release supervisor");
    let thread = fixture.wait_for(&id, LONG, is_done);
    assert_eq!(thread.ending, Some(Ending::Failed), "{thread:#?}");
    assert!(
        thread
            .warnings
            .iter()
            .all(|warning| !warning.contains("GITEZ_TEST_SANDBOX")),
        "{thread:#?}"
    );
    // The fake agent never ran.
    assert_eq!(fixture.calls(), Vec::<String>::new());
    let failure = format!("{thread:?}");
    assert!(failure.contains("sandbox"), "{failure}");
}

/// Runs the supervisor binary directly with `args` and `stdin`.
fn supervise(fixture: &Fixture, args: &[&str], stdin: &str) -> i32 {
    let mut child = Process::new(env!("CARGO_BIN_EXE_gitez-supervisor"))
        .args(args)
        .env("GITEZ_DATA_DIR", &fixture.data_dir)
        .env("GITEZ_TEST_SANDBOX", "passthrough")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait().expect("wait").code().expect("exit code")
}

#[test]
fn exit_codes() {
    let fixture = Fixture::new();
    let git_dir = fixture.common_dir();
    let git_dir = git_dir.to_str().expect("utf-8");
    let thread = fixture.create("What is the answer?").id;
    let id = thread.as_str();

    // 1: usage errors.
    assert_eq!(supervise(&fixture, &[], ""), 1);
    assert_eq!(supervise(&fixture, &["frobnicate"], ""), 1);
    assert_eq!(supervise(&fixture, &["start", "--git-dir", git_dir], ""), 1);
    // 1: no such thread.
    assert_eq!(
        supervise(
            &fixture,
            &["start", "--git-dir", git_dir, "--thread", "no-such-thread"],
            ""
        ),
        1
    );
    // 2: another supervisor holds the thread.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(fixture.thread_dir(&thread).join("supervisor.lock"))
        .expect("open lock");
    lock.try_lock().expect("lock");
    assert_eq!(
        supervise(
            &fixture,
            &["start", "--git-dir", git_dir, "--thread", id],
            ""
        ),
        2
    );
    drop(lock);
    assert_eq!(fixture.load(&thread).state, State::Queued);
    // 0: run to the end.
    assert_eq!(
        supervise(
            &fixture,
            &["start", "--git-dir", git_dir, "--thread", id],
            r#"{"version":1,"secrets":{}}"#
        ),
        0
    );
    assert_eq!(fixture.load(&thread).ending, Some(Ending::Answer));
    // 1: the subcommand does not match; the thread is left as it was.
    assert_eq!(
        supervise(
            &fixture,
            &["reply", "--git-dir", git_dir, "--thread", id],
            ""
        ),
        1
    );
    assert_eq!(fixture.load(&thread).ending, Some(Ending::Answer));
    // 1: secrets that are not valid JSON fail an active thread.
    let other = fixture.create("What else?").id;
    assert_eq!(
        supervise(
            &fixture,
            &["start", "--git-dir", git_dir, "--thread", other.as_str()],
            "{not json"
        ),
        1
    );
    let failed = fixture.load(&other);
    assert_eq!(failed.ending, Some(Ending::Failed));
    assert_eq!(failed.error.expect("error").step, Step::Supervisor);
}

/// Two repositories, one choosing the Work account in its git config and
/// one choosing none: each thread, resolved as the GUI resolves it and run
/// by its own supervisor process, gives its agent its own folder, and the
/// default account none at all (even when GitEZ inherited one).
#[test]
fn each_repository_runs_its_agents_with_its_own_account() {
    let fixture = Fixture::new();
    let work_folder = fixture.base.join("claude work");
    fixture.add_account("Work", &work_folder);
    let other = fixture.base.join("other repo");
    std::fs::create_dir_all(&other).expect("folder");
    common::git(&other, &["init", "--quiet", "--initial-branch=main"]);
    common::git(
        &other,
        &["commit", "--quiet", "--allow-empty", "-m", "First"],
    );
    common::git(&other, &["config", "gitez.agentAccount", "Work"]);

    let settings =
        match gitez_settings::Settings::load(&gitez_settings::Settings::path(&fixture.data_dir))
            .expect("settings")
        {
            gitez_settings::Loaded::Current(settings) => settings,
            gitez_settings::Loaded::TooNew => panic!("too new"),
        };
    let account_of = |dir: &Path| {
        let choice = gitez_git::Repository::open(dir)
            .expect("open")
            .config_string(gitez_settings::ACCOUNT_KEY);
        settings
            .account_for(gitez_agent::ClaudeCode::ID, choice.as_deref())
            .expect("an account")
    };
    let work = account_of(&other);
    assert_eq!(work.folder.as_deref(), Some(work_folder.as_path()));
    // The default account, with Claude Code's own folder: no variable.
    let default = gitez_agent_thread::Account {
        name: gitez_agent_thread::Account::DEFAULT_NAME.to_owned(),
        folder: None,
    };
    assert!(account_of(&fixture.root).is_default());

    let mine = fixture.create("What is the answer?").id;
    fixture.record_account(&mine, default);
    let other_common = other.join(".git");
    let theirs = gitez_agent_thread::Store::new(&other_common)
        .create(
            gitez_agent_thread::NewAgentThread {
                prompt: "And here?".to_owned(),
                agent: gitez_agent::ClaudeCode::ID.to_owned(),
                model: None,
                effort: None,
                account: Some(work),
                mode: gitez_agent_thread::Mode::Implement,
                auto: false,
                start: gitez_agent_thread::Start {
                    branch: "main".to_owned(),
                    commit: common::git(&other, &["rev-parse", "HEAD"]),
                },
                work_dir: None,
                origin: None,
                network: Default::default(),
                local_ports: false,
                secrets: Vec::new(),
            },
            &fixture.data_dir,
            "other repo",
            |_| false,
            Timestamp::now(),
        )
        .expect("create thread")
        .id;

    let exe = fixture.supervisor_exe();
    gitez_supervisor::launch(gitez_supervisor::Launch {
        exe: &exe,
        prefix: &[],
        command: Command::Start,
        common_dir: &other_common,
        thread: &theirs,
        secrets: &Secrets::default(),
    })
    .expect("launch");
    fixture.launch(&mine, Command::Start, &Secrets::default());
    let mine = fixture.wait_for(&mine, LONG, is_done);
    assert_eq!(mine.ending, Some(Ending::Answer), "{mine:#?}");
    let store = gitez_agent_thread::Store::new(&other_common);
    wait_until(LONG, "the other thread", || {
        store.load(&theirs).is_ok_and(|thread| is_done(&thread))
    });
    let mut seen = fixture.config_dirs();
    seen.sort();
    let mut expected = vec!["<unset>".to_owned(), work_folder.display().to_string()];
    expected.sort();
    assert_eq!(seen, expected);
}
