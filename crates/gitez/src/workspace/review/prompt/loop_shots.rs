//! Screenshots of the agent panel of local pull requests that agent
//! threads created, and end-to-end runs of the review loop (E.11) with a
//! real supervisor: review comments sent to the thread, which continues in
//! its worktree, then the pull request merged or closed, which completes
//! the thread. All of them return at once unless `GITEZ_SCREENSHOTS` is
//! set; the runs also need `GITEZ_E2E=1` (a fake agent) or
//! `GITEZ_E2E_REAL=1` (the real Claude Code with haiku), and a built
//! `target/debug/gitez` for the supervisor (see `composer_shots.rs`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gitez_agent_thread::{AgentThread, Ending, State, Step, Timestamp, TurnKind, TurnResult};
use gitez_review::{Anchor, ResolvedBy, Side};
use gitez_sandbox::NetworkProfile;
use gitez_settings::AgentEntry;
use gpui_kit::{AnyWindowHandle, Bounds, Entity, Pixels, Point, point, px, size};

use crate::agent_threads::{AgentThreadKey, AgentThreads, OpenAgentPullRequest, StartRequest};
use crate::screenshot::{Shots, git};
use crate::workspace::Workspace;
use crate::workspace::composer_shots::{
    common_dir_of, gitez_binary, minutes_ago, repository, save_settings, supervisor_script, thread,
    wait_until, write_script,
};
use crate::workspace::review::Subject;

/// The fake `claude` of the review loop. The first prompt writes
/// `agent.txt` with three lines and commits it. A prompt with "Say TWO"
/// (the review comment) waits for `release`, then changes line two and
/// commits that. Every call's arguments go to `calls`.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = auth ]; then printf '{"loggedIn": true}\n'; exit 0; fi
log="$FAKE_LOG"
sid=""
prev=""
for arg in "$@"; do
  case "$prev" in --session-id|--resume) sid="$arg" ;; esac
  prev="$arg"
done
printf '%s\n' "$*" >> "$log/calls"
prompt=$(cat)
say() { printf '%s\n' "$1"; }
init() { say '{"type":"system","subtype":"init","session_id":"'"$sid"'","model":"fake-model"}'; }
tool() { say '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"'"$1"'"}}]}}'; }
result() { say '{"type":"result","subtype":"success","is_error":false,"result":"'"$1"'","session_id":"'"$sid"'","num_turns":1,"duration_ms":5,"total_cost_usd":0.01}'; }
case "$prompt" in
  *"Say TWO"*)
    init; tool "sed -i s/two/TWO/ agent.txt"
    while [ ! -e "$log/release" ]; do sleep 0.1; done
    rm -f "$log/release"
    printf 'one\nTWO\nthree\n' > agent.txt && git commit --quiet -am "Agent: say TWO" >&2
    result "Line two now says TWO, as the review asked." ;;
  *)
    init; tool "git commit"
    printf 'one\ntwo\nthree\n' > agent.txt && git add agent.txt && git commit --quiet -m "Agent: add agent.txt" >&2
    result "Wrote agent.txt with three lines and committed it." ;;
esac
"#;

/// The fake agent in `base/bin`, logging to `base/fake log`.
fn review_agent(base: &Path) -> AgentEntry {
    let bin = base.join("bin");
    let log = base.join("fake log");
    std::fs::create_dir_all(&bin).expect("bin folder");
    std::fs::create_dir_all(&log).expect("log folder");
    let program = bin.join("claude");
    write_script(&program, FAKE_CLAUDE);
    AgentEntry {
        program: Some(program),
        config_dir: Some(base.join("claude config")),
        env: [("FAKE_LOG".to_owned(), log.to_string_lossy().into_owned())].into(),
        ..AgentEntry::default()
    }
}

/// Window coordinates of what the runs click, read off the `u5-` shots:
/// in the review's header (its buttons at the right end), in the agent
/// panel (380 wide, at the right edge) and in the merge dialog. Each can
/// be moved with `U5_<NAME>=x:y`.
fn at(name: &str, x: f32, y: f32) -> Point<Pixels> {
    let place = std::env::var(format!("U5_{}", name.to_uppercase()))
        .ok()
        .and_then(|place| {
            let (x, y) = place.split_once(':')?;
            Some((x.parse().ok()?, y.parse().ok()?))
        });
    let (x, y) = place.unwrap_or((x, y));
    point(px(x), px(y))
}

fn send_button() -> Point<Pixels> {
    at("send", 1290., 549.)
}

fn merge_button() -> Point<Pixels> {
    at("merge", 1233., 121.)
}

fn close_button() -> Point<Pixels> {
    at("close", 1029., 121.)
}

/// "Merge into main" in the merge dialog.
fn merge_option() -> Point<Pixels> {
    at("merge_option", 714., 176.)
}

/// The agent panel, beside the diff.
fn panel_region() -> Bounds<Pixels> {
    Bounds {
        origin: point(px(1020.), px(250.)),
        size: size(px(420.), px(650.)),
    }
}

/// Scrolls the agent panel to its end, where Send and the agent's work
/// are.
fn scroll_panel(shots: &mut Shots, window: AnyWindowHandle) {
    shots.scroll(window, point(px(1220.), px(600.)), 60.);
}

/// The detail panel of the main window, where a thread opens.
fn detail_region() -> Bounds<Pixels> {
    Bounds {
        origin: point(px(1080.), px(48.)),
        size: size(px(360.), px(824.)),
    }
}

/// A comment on line `line` of `path` at the pull request's head, saved in
/// the review store as the diff's + button would.
fn comment(common_dir: &Path, number: u32, path: &str, line: u32, excerpt: &str, body: &str) {
    let store = gitez_review::Store::new(common_dir);
    let mut pull_request = store.load(number).expect("the pull request");
    let anchor = Anchor {
        commit: pull_request.head.commit.clone(),
        path: path.into(),
        side: Side::New,
        lines: (line, line),
        excerpt: vec![excerpt.into()],
    };
    pull_request.add_thread(anchor, body.into(), Timestamp::now());
    store.save(&pull_request).expect("saved");
}

/// A real-supervisor app for `work_dir` with `agent`: threads run with the
/// pass-through sandbox unless `sandbox` is given.
fn app(
    base: &Path,
    work_dir: &Path,
    agent: AgentEntry,
    sandbox: Option<Arc<dyn gitez_sandbox::Sandbox>>,
) -> (Shots, PathBuf) {
    let binary = gitez_binary().expect("build target/debug/gitez first: cargo build -p gitez");
    let common_dir = common_dir_of(work_dir);
    let data_dir = base.join("data");
    save_settings(&data_dir, &common_dir, agent);
    let script = supervisor_script(base, &binary, &data_dir, sandbox.is_none());
    let shots = Shots::with_options(&data_dir, |options| {
        if let Some(sandbox) = sandbox {
            options.sandbox = sandbox;
        }
        options.exe = Ok(script);
        options.watch = true;
    })
    .expect("enabled");
    (shots, common_dir)
}

/// Starts a thread on `prompt` from `main` with `model`.
fn start(
    shots: &mut Shots,
    work_dir: &Path,
    common_dir: &Path,
    prompt: &str,
    model: Option<&str>,
) -> AgentThreadKey {
    let head = gitez_git::Repository::open(work_dir)
        .expect("open")
        .resolve_commit("refs/heads/main")
        .expect("resolve")
        .expect("main")
        .to_string();
    let request = StartRequest {
        work_dir: work_dir.to_owned(),
        common_dir: common_dir.to_owned(),
        repository_name: "repo".into(),
        prompt: prompt.into(),
        agent: gitez_agent::ClaudeCode::ID.into(),
        model: model.map(str::to_owned),
        start: gitez_agent_thread::Start {
            branch: "main".into(),
            commit: head,
        },
        network: NetworkProfile::AgentOnly,
        secrets: Vec::new(),
    };
    let task = shots
        .update(|cx| AgentThreads::global(cx).update(cx, |model, cx| model.start(request, cx)));
    let deadline = Instant::now() + Duration::from_secs(30);
    // The task runs on the background executor, in real time.
    let mut task = Some(task);
    loop {
        shots.settle();
        if let Some(key) = shots
            .update(|cx| {
                AgentThreads::global(cx)
                    .read(cx)
                    .repository(common_dir)
                    .and_then(|repository| repository.threads.first().cloned())
            })
            .map(|thread| AgentThreadKey {
                common_dir: common_dir.to_owned(),
                id: thread.id.clone(),
            })
        {
            drop(task.take());
            return key;
        }
        assert!(Instant::now() < deadline, "the thread was not created");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The thread `key` in the model, once `done` holds for it.
fn wait_for(
    shots: &mut Shots,
    key: &AgentThreadKey,
    timeout: Duration,
    what: &str,
    done: impl Fn(&AgentThread) -> bool,
) -> Arc<AgentThread> {
    let threads = wait_until(shots, &key.common_dir, timeout, what, |threads| {
        threads
            .iter()
            .any(|thread| thread.id == key.id && done(thread))
    });
    threads
        .into_iter()
        .find(|thread| thread.id == key.id)
        .expect("the thread")
}

/// The pull request the window's review shows: how many commits it has,
/// its state, and its comment threads.
fn reviewed(
    shots: &mut Shots,
    workspace: &Entity<Workspace>,
) -> Option<(Option<usize>, gitez_review::PullRequest)> {
    shots.update(|cx| {
        let (review, _) = workspace.read(cx).review.as_ref()?;
        match &review.read(cx).subject {
            Subject::PullRequest(subject) => Some((subject.ahead, subject.pull_request.clone())),
            _ => None,
        }
    })
}

fn branch_of(work_dir: &Path) -> String {
    gitez_command_output(work_dir, &["branch", "--show-current"])
}

fn gitez_command_output(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout)
        .expect("utf-8")
        .trim()
        .to_owned()
}

/// How the end of the loop looks: completed, worktree gone, branch kept,
/// git's list of worktrees only the main one.
fn assert_completed(work_dir: &Path, thread: &AgentThread) {
    assert_eq!(thread.state, State::Completed, "{thread:#?}");
    assert!(thread.completed.is_some());
    assert!(!thread.worktree.exists(), "the worktree folder is gone");
    let worktrees = gitez_command_output(work_dir, &["worktree", "list", "--porcelain"]);
    assert_eq!(
        worktrees
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count(),
        1,
        "{worktrees}"
    );
    // The branch has commits of its own: kept.
    gitez_command_output(work_dir, &["rev-parse", "--verify", &thread.branch]);
}

/// Case 1: a thread commits → its pull request; a comment on a line is
/// sent from the panel while `main` is checked out → the thread runs a
/// Review turn in its worktree with `--resume`, commits again, the pull
/// request shows two commits and the comment resolved itself; merged from
/// the review → the thread is completed.
#[test]
fn e2e_u5_review_loop_and_merge() {
    if !crate::screenshot::enabled() || std::env::var_os("GITEZ_E2E").is_none() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let work_dir = repository(&base);
    let log = base.join("fake log");
    let (mut shots, common_dir) = app(&base, &work_dir, review_agent(&base), None);
    let (window, workspace) = shots.open_workspace(&work_dir);

    let key = start(
        &mut shots,
        &work_dir,
        &common_dir,
        "Write agent.txt with three lines and commit it.",
        None,
    );
    let first = wait_for(
        &mut shots,
        &key,
        Duration::from_secs(30),
        "a pull request",
        |thread| thread.state == State::Finished && thread.pull_request.is_some(),
    );
    assert_eq!(first.ending, Some(Ending::PullRequest));
    let number = first.pull_request.expect("a pull request");
    let session = first.session_id.clone().expect("a session");
    comment(
        &common_dir,
        number,
        "agent.txt",
        2,
        "two",
        "Say TWO here, in capitals.",
    );
    assert_eq!(branch_of(&work_dir), "main");

    shots.dispatch(
        window,
        OpenAgentPullRequest {
            common_dir: common_dir.clone(),
            number,
        },
    );
    let (ahead, pull_request) = reviewed(&mut shots, &workspace).expect("the review shows it");
    assert_eq!(ahead, Some(1));
    assert_eq!(pull_request.open_threads(), 1);
    shots.save(window, "u5-1-thread-pr-before");
    scroll_panel(&mut shots, window);
    shots.save_region(window, panel_region(), "u5-1-thread-pr-before-panel");

    shots.click(window, send_button());
    let running = wait_for(
        &mut shots,
        &key,
        Duration::from_secs(30),
        "the review turn to run",
        |thread| thread.state == State::Running && thread.turns.len() == 2,
    );
    let turn = running.turns.last().expect("a turn");
    assert_eq!(turn.kind, TurnKind::Review);
    assert!(turn.resume);
    assert!(turn.prompt.contains("Say TWO"), "{}", turn.prompt);
    assert_eq!(
        branch_of(&work_dir),
        "main",
        "the main worktree is not touched"
    );
    shots.save(window, "u5-2-thread-pr-running");
    shots.save_region(window, panel_region(), "u5-2-thread-pr-running-panel");

    std::fs::write(log.join("release"), "").expect("release the agent");
    let finished = wait_for(
        &mut shots,
        &key,
        Duration::from_secs(30),
        "the review turn to end",
        |thread| thread.state == State::Finished && thread.turns.len() == 2,
    );
    assert_eq!(finished.ending, Some(Ending::PullRequest));
    assert_eq!(finished.pull_request, Some(number));
    let calls = std::fs::read_to_string(log.join("calls")).expect("calls");
    let review_call = calls.lines().last().expect("a call");
    assert!(
        review_call.contains(&format!("--resume {session}")),
        "the review turn resumes the conversation: {review_call}"
    );
    // The review follows the branch without being opened again.
    let deadline = Instant::now() + Duration::from_secs(10);
    let pull_request = loop {
        let (ahead, pull_request) = reviewed(&mut shots, &workspace).expect("still shown");
        if ahead == Some(2) {
            break pull_request;
        }
        assert!(
            Instant::now() < deadline,
            "the pull request shows {ahead:?} commits"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    let resolved = &pull_request.threads[0].resolved;
    assert_eq!(
        resolved.as_ref().map(|resolution| resolution.by),
        Some(ResolvedBy::Auto),
        "the commented line changed, so its comment resolved itself"
    );
    shots.save(window, "u5-3-thread-pr-finished");
    shots.save_region(window, panel_region(), "u5-3-thread-pr-finished-panel");

    shots.click(window, merge_button());
    // The dialog slides in.
    shots.settle();
    shots.save(window, "u5-4-merge-dialog");
    shots.click(window, merge_option());
    shots.save(window, "u5-4-merge-clicked");
    let completed = wait_for(
        &mut shots,
        &key,
        Duration::from_secs(30),
        "the thread to complete",
        |thread| thread.state == State::Completed,
    );
    assert_completed(&work_dir, &completed);
    let merged = gitez_review::Store::new(&common_dir)
        .load(number)
        .expect("load");
    assert_eq!(merged.state, gitez_review::State::Merged);
    assert_eq!(branch_of(&work_dir), "main");
    let log_of_main = gitez_command_output(&work_dir, &["log", "--format=%s", "-4", "main"]);
    assert!(log_of_main.contains("Agent: say TWO"), "{log_of_main}");
    // "Merge into main" made a merge commit: main's tip has two parents.
    let parents = gitez_command_output(&work_dir, &["log", "-1", "--format=%P", "main"]);
    assert_eq!(
        parents.split(' ').count(),
        2,
        "a merge commit: {log_of_main}"
    );
    shots.save(window, "u5-5-merged-review");
    shots.save_region(window, panel_region(), "u5-5-merged-review-panel");
    shots.dispatch(
        window,
        crate::agent_threads::OpenAgentThread {
            common_dir: common_dir.clone(),
            id: key.id.clone(),
        },
    );
    shots.save(window, "u5-6-completed-thread");
    shots.save_region(window, detail_region(), "u5-6-completed-thread-panel");
}

/// Case 2: closed without merging → completed; reopened → the thread stays
/// completed and the pull request is reviewed like any other.
#[test]
fn e2e_u5_close_completes() {
    if !crate::screenshot::enabled() || std::env::var_os("GITEZ_E2E").is_none() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let work_dir = repository(&base);
    let (mut shots, common_dir) = app(&base, &work_dir, review_agent(&base), None);
    let (window, workspace) = shots.open_workspace(&work_dir);
    let key = start(
        &mut shots,
        &work_dir,
        &common_dir,
        "Write agent.txt and commit it.",
        None,
    );
    let finished = wait_for(
        &mut shots,
        &key,
        Duration::from_secs(30),
        "a pull request",
        |thread| thread.state == State::Finished && thread.pull_request.is_some(),
    );
    let number = finished.pull_request.expect("a pull request");
    comment(&common_dir, number, "agent.txt", 1, "one", "Say TWO here.");
    shots.dispatch(
        window,
        OpenAgentPullRequest {
            common_dir: common_dir.clone(),
            number,
        },
    );
    shots.click(window, close_button());
    let completed = wait_for(
        &mut shots,
        &key,
        Duration::from_secs(30),
        "the thread to complete",
        |thread| thread.state == State::Completed,
    );
    assert_completed(&work_dir, &completed);
    let (_, pull_request) = reviewed(&mut shots, &workspace).expect("shown");
    assert_eq!(pull_request.state, gitez_review::State::Closed);
    shots.save(window, "u5-7-closed");
    // Reopen: Close's place now says Reopen.
    shots.click(window, close_button());
    let (_, pull_request) = reviewed(&mut shots, &workspace).expect("shown");
    assert_eq!(pull_request.state, gitez_review::State::Open);
    shots.settle();
    let thread = wait_for(
        &mut shots,
        &key,
        Duration::from_secs(5),
        "the thread",
        |_| true,
    );
    assert_eq!(
        *thread, *completed,
        "reopening leaves the completed thread alone"
    );
    shots.save(window, "u5-8-reopened");
    shots.save_region(window, panel_region(), "u5-8-reopened-panel");
}

/// Case 3: the pull request is merged while no GitEZ runs (here: on the
/// review store, with git); when the model starts, the thread completes.
#[test]
fn e2e_u5_merged_while_closed() {
    if !crate::screenshot::enabled() || std::env::var_os("GITEZ_E2E").is_none() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let work_dir = repository(&base);
    let common_dir = common_dir_of(&work_dir);
    let data_dir = base.join("data");
    let made = thread(&work_dir, &data_dir, "Write agent.txt", |thread| {
        thread.state = State::Finished;
        thread.ending = Some(Ending::PullRequest);
        thread.session_id = Some("session".into());
    });
    gitez_agent_thread::create_worktree(
        &work_dir,
        &made.worktree,
        &made.branch,
        &made.start.commit,
    )
    .expect("worktree");
    std::fs::write(made.worktree.join("agent.txt"), "one\n").expect("write");
    git(&made.worktree, &["add", "."]);
    git(&made.worktree, &["commit", "-qm", "Agent: add agent.txt"]);
    let head = gitez_command_output(&made.worktree, &["rev-parse", "HEAD"]);
    let store = gitez_review::Store::new(&common_dir);
    let mut pull_request = store
        .create(
            gitez_review::NewPullRequest {
                title: made.title.clone(),
                description: String::new(),
                base: gitez_review::Tip {
                    branch: "main".into(),
                    commit: made.start.commit.clone(),
                },
                head: gitez_review::Tip {
                    branch: made.branch.clone(),
                    commit: head,
                },
            },
            Timestamp::now(),
        )
        .expect("pull request");
    gitez_agent_thread::Store::new(&common_dir)
        .update(&made.id, |thread| {
            thread.pull_request = Some(pull_request.number);
            Ok(())
        })
        .expect("linked");
    gitez_agent_thread::Index::new(&data_dir)
        .add(gitez_agent_thread::IndexedRepository {
            common_dir: common_dir.clone(),
            work_dir: work_dir.clone(),
            name: "repo".into(),
        })
        .expect("index");
    // Merged with git and saved as merged, as GitEZ would, but with no
    // GitEZ running.
    git(
        &work_dir,
        &["merge", "-q", "--no-ff", "-m", "Merge", &made.branch],
    );
    pull_request.state = gitez_review::State::Merged;
    store.save(&pull_request).expect("saved");

    let (mut shots, _) = app(&base, &work_dir, review_agent(&base), None);
    let key = AgentThreadKey {
        common_dir: common_dir.clone(),
        id: made.id.clone(),
    };
    let completed = wait_for(
        &mut shots,
        &key,
        Duration::from_secs(20),
        "the thread to complete",
        |thread| thread.state == State::Completed,
    );
    assert_completed(&work_dir, &completed);
    let (window, _) = shots.open_workspace(&work_dir);
    shots.dispatch(
        window,
        crate::agent_threads::OpenAgentThread {
            common_dir: common_dir.clone(),
            id: made.id.clone(),
        },
    );
    shots.save_region(window, detail_region(), "u5-9-completed-at-start-panel");
}

fn result(message: &str, success: bool) -> TurnResult {
    TurnResult {
        success,
        message: message.into(),
        cost_usd: Some(0.04),
        duration_ms: Some(42_000),
        steps: 6,
        exit_code: Some(if success { 0 } else { 1 }),
        stopped: false,
        blocked_hosts: Vec::new(),
    }
}

/// A thread finished with pull request `number` of its branch, which has
/// one commit, and a comment on it; `change` sets the thread's state.
fn thread_with_pull_request(
    base: &Path,
    work_dir: &Path,
    change: impl FnOnce(&mut AgentThread),
) -> (AgentThreadKey, u32) {
    let common_dir = common_dir_of(work_dir);
    let data_dir = base.join("data");
    let made = thread(
        work_dir,
        &data_dir,
        "Add a blame view to the commit detail",
        |thread| {
            thread.created = minutes_ago(50);
            thread.state = State::Finished;
            thread.ending = Some(Ending::PullRequest);
            thread.session_id = Some("session".into());
            thread.turns[0].started = Some(minutes_ago(49));
            thread.turns[0].ended = Some(minutes_ago(30));
            thread.turns[0].result = Some(result("Added the blame view.", true));
        },
    );
    gitez_agent_thread::create_worktree(work_dir, &made.worktree, &made.branch, &made.start.commit)
        .expect("worktree");
    std::fs::write(
        made.worktree.join("blame.rs"),
        "fn blame() {}\nfn lines() {}\n",
    )
    .expect("write");
    git(&made.worktree, &["add", "."]);
    git(
        &made.worktree,
        &["commit", "-qm", "Read who last changed each line"],
    );
    let head = gitez_command_output(&made.worktree, &["rev-parse", "HEAD"]);
    let pull_request = gitez_review::Store::new(&common_dir)
        .create(
            gitez_review::NewPullRequest {
                title: made.title.clone(),
                description: "Added the blame view.".into(),
                base: gitez_review::Tip {
                    branch: "main".into(),
                    commit: made.start.commit.clone(),
                },
                head: gitez_review::Tip {
                    branch: made.branch.clone(),
                    commit: head,
                },
            },
            Timestamp::now(),
        )
        .expect("pull request");
    comment(
        &common_dir,
        pull_request.number,
        "blame.rs",
        2,
        "fn lines() {}",
        "Name this blame_lines.",
    );
    gitez_agent_thread::Store::new(&common_dir)
        .update(&made.id, |thread| {
            thread.pull_request = Some(pull_request.number);
            change(thread);
            Ok(())
        })
        .expect("linked");
    gitez_agent_thread::Index::new(&data_dir)
        .add(gitez_agent_thread::IndexedRepository {
            common_dir: common_dir.clone(),
            work_dir: work_dir.to_owned(),
            name: "repo".into(),
        })
        .expect("index");
    (
        AgentThreadKey {
            common_dir,
            id: made.id,
        },
        pull_request.number,
    )
}

/// Opens pull request `number` of `work_dir` and shoots its panel as
/// `name`.
fn shoot_panel(
    base: &Path,
    work_dir: &Path,
    number: u32,
    name: &str,
) -> (Shots, AnyWindowHandle, Entity<Workspace>) {
    let data_dir = base.join("data");
    let mut shots = Shots::from_env(&data_dir).expect("enabled");
    let (window, workspace) = shots.open_workspace(work_dir);
    shots.dispatch(
        window,
        OpenAgentPullRequest {
            common_dir: common_dir_of(work_dir),
            number,
        },
    );
    shots.save(window, name);
    scroll_panel(&mut shots, window);
    shots.save_region(window, panel_region(), &format!("{name}-panel"));
    (shots, window, workspace)
}

/// Changes a thread made for a shot.
type ChangeThread = fn(&mut AgentThread);

/// The panel of a thread's pull request after the thread failed, was
/// stopped or waits; and one whose worktree folder is gone.
#[test]
fn screenshot_u5_thread_states() {
    if !crate::screenshot::enabled() {
        return;
    }
    let cases: [(&str, ChangeThread); 4] = [
        ("u5-failed", |thread| {
            thread.turns.push(gitez_agent_thread::Turn {
                kind: TurnKind::Review,
                prompt: "Name this blame_lines.".into(),
                requested: minutes_ago(9),
                resume: true,
                started: Some(minutes_ago(9)),
                ended: None,
                transcript: None,
                result: Some(result("", false)),
            });
            thread.fail(
                Step::Agent,
                "Claude Code exited with code 1: API Error: 529 Overloaded.",
                minutes_ago(8),
            );
        }),
        ("u5-stopped", |thread| {
            thread.ending = Some(Ending::Stopped);
        }),
        ("u5-waiting", |thread| {
            thread.state = State::Waiting;
            thread.ending = None;
            thread.waiting_until = Some(Timestamp::now() + jiff::SignedDuration::from_mins(40));
        }),
        ("u5-worktree-gone", |_| {}),
    ];
    for (name, change) in cases {
        let dir = tempfile::tempdir().expect("temp dir");
        let base = dir.path().canonicalize().expect("canonical");
        let work_dir = repository(&base);
        let (key, number) = thread_with_pull_request(&base, &work_dir, change);
        if name == "u5-worktree-gone" {
            let worktree = gitez_agent_thread::Store::new(&key.common_dir)
                .load(&key.id)
                .expect("load")
                .worktree;
            std::fs::remove_dir_all(worktree).expect("deleted by hand");
        }
        shoot_panel(&base, &work_dir, number, name);
    }
}

/// A pull request that no thread created looks as before: its head branch
/// not checked out, so it cannot be sent; and checked out, so it can.
#[test]
fn screenshot_u5_plain_pull_request() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let work_dir = repository(&base);
    git(&work_dir, &["checkout", "-qb", "feature/blame"]);
    std::fs::write(work_dir.join("blame.rs"), "fn blame() {}\nfn lines() {}\n").expect("write");
    git(&work_dir, &["add", "."]);
    git(
        &work_dir,
        &["commit", "-qm", "Read who last changed each line"],
    );
    let head = gitez_command_output(&work_dir, &["rev-parse", "HEAD"]);
    git(&work_dir, &["checkout", "-q", "main"]);
    let common_dir = common_dir_of(&work_dir);
    let base_commit = gitez_command_output(&work_dir, &["rev-parse", "main"]);
    let pull_request = gitez_review::Store::new(&common_dir)
        .create(
            gitez_review::NewPullRequest {
                title: "Add a blame view".into(),
                description: String::new(),
                base: gitez_review::Tip {
                    branch: "main".into(),
                    commit: base_commit,
                },
                head: gitez_review::Tip {
                    branch: "feature/blame".into(),
                    commit: head,
                },
            },
            Timestamp::now(),
        )
        .expect("pull request");
    comment(
        &common_dir,
        pull_request.number,
        "blame.rs",
        2,
        "fn lines() {}",
        "Name this blame_lines.",
    );
    let (mut shots, window, workspace) = shoot_panel(
        &base,
        &work_dir,
        pull_request.number,
        "u5-plain-not-checked-out",
    );
    git(&work_dir, &["checkout", "-q", "feature/blame"]);
    shots.update_window(window, |window, cx| {
        workspace.update(cx, |workspace, cx| workspace.reload(window, cx))
    });
    shots.save_region(window, panel_region(), "u5-plain-checked-out-panel");
}

/// One real run of the loop: Claude Code with haiku in the real sandbox,
/// in a throwaway repository under `GITEZ_E2E_SCRATCH` (srt copied from
/// `GITEZ_E2E_SANDBOX`, the agent at `GITEZ_E2E_CLAUDE`).
#[test]
fn e2e_u5_real_haiku() {
    if !crate::screenshot::enabled() || std::env::var_os("GITEZ_E2E_REAL").is_none() {
        return;
    }
    let scratch = PathBuf::from(std::env::var_os("GITEZ_E2E_SCRATCH").expect("a scratch folder"));
    let sandbox = PathBuf::from(std::env::var_os("GITEZ_E2E_SANDBOX").expect("an srt install"));
    let base = scratch.join(format!("u5-real-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("scratch folder");
    let work_dir = repository(&base);
    let data_dir = base.join("data");
    std::fs::create_dir_all(&data_dir).expect("data folder");
    let copied = std::process::Command::new("cp")
        .arg("-R")
        .arg(&sandbox)
        .arg(data_dir.join("sandbox"))
        .status()
        .expect("cp runs");
    assert!(copied.success(), "copy srt");
    let claude =
        PathBuf::from(std::env::var_os("GITEZ_E2E_CLAUDE").expect("the path of the real claude"));
    let agent = AgentEntry {
        program: Some(claude),
        config_dir: std::env::var_os("GITEZ_E2E_CLAUDE_CONFIG").map(PathBuf::from),
        ..AgentEntry::default()
    };
    let (mut shots, common_dir) = app(
        &base,
        &work_dir,
        agent,
        Some(gitez_sandbox::platform(&data_dir)),
    );
    let (window, workspace) = shots.open_workspace(&work_dir);
    let started = Instant::now();
    let key = start(
        &mut shots,
        &work_dir,
        &common_dir,
        "Add a file hello.txt containing hi and commit it",
        Some("haiku"),
    );
    let first = wait_for(
        &mut shots,
        &key,
        Duration::from_secs(300),
        "the first turn",
        |thread| !thread.state.is_active(),
    );
    eprintln!(
        "first turn after {:?}: {:?} {:?} #{:?}",
        started.elapsed(),
        first.state,
        first.ending,
        first.pull_request
    );
    let number = first.pull_request.expect("a pull request");
    let excerpt = std::fs::read_to_string(first.worktree.join("hello.txt")).expect("hello.txt");
    comment(
        &common_dir,
        number,
        "hello.txt",
        1,
        excerpt.lines().next().unwrap_or_default(),
        "Say hello instead of hi",
    );
    shots.dispatch(
        window,
        OpenAgentPullRequest {
            common_dir: common_dir.clone(),
            number,
        },
    );
    shots.save(window, "u5-real-1-before");
    scroll_panel(&mut shots, window);
    shots.click_id(window, "prompt-send");
    std::thread::sleep(Duration::from_secs(2));
    shots.settle();
    let sent = shots.update(|cx| {
        AgentThreads::global(cx)
            .read(cx)
            .thread(&key)
            .is_some_and(|thread| thread.turns.len() == 2)
    });
    if !sent {
        // The click missed (the panel's height follows the agent's
        // answer); send as the button does.
        eprintln!("the click missed Send; sending from the panel directly");
        shots.update(|cx| {
            let review = workspace
                .read(cx)
                .review
                .as_ref()
                .map(|(review, _)| review.clone())
                .expect("a review");
            review.update(cx, |review, cx| review.send_prompt(cx));
        });
    }
    let review_started = Instant::now();
    let second = wait_for(
        &mut shots,
        &key,
        Duration::from_secs(300),
        "the review turn",
        |thread| thread.turns.len() == 2 && !thread.state.is_active(),
    );
    eprintln!(
        "review turn after {:?}: {:?} {:?}, result {:?}",
        review_started.elapsed(),
        second.state,
        second.ending,
        second.turns.last().and_then(|turn| turn.result.clone())
    );
    let text = std::fs::read_to_string(second.worktree.join("hello.txt")).expect("hello.txt");
    eprintln!("hello.txt now: {text:?}");
    let commits = gitez_command_output(
        &work_dir,
        &["log", "--format=%s", &format!("main..{}", second.branch)],
    );
    eprintln!("commits of the branch:\n{commits}");
    shots.settle();
    let (ahead, _) = reviewed(&mut shots, &workspace).expect("shown");
    eprintln!("the review shows {ahead:?} commits");
    shots.save(window, "u5-real-2-after-review");
    shots.click(window, merge_button());
    shots.settle();
    shots.click(window, merge_option());
    let completed = wait_for(
        &mut shots,
        &key,
        Duration::from_secs(30),
        "completed",
        |thread| thread.state == State::Completed,
    );
    assert_completed(&work_dir, &completed);
    eprintln!("completed after {:?} in all", started.elapsed());
    shots.save(window, "u5-real-3-merged");
    assert_eq!(ahead, Some(2));
    assert!(text.contains("hello"), "{text:?}");
}

/// The fake `claude` of the uncommitted-work runs (C1). A prompt with "and
/// commit it" commits `agent.txt` past the hook (`--no-verify`) and then
/// leaves `draft.md`; any other prompt writes `draft.md` and tries to
/// commit it, which the repository's pre-commit hook refuses.
const LEAVING_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = auth ]; then printf '{"loggedIn": true}\n'; exit 0; fi
sid=""
prev=""
for arg in "$@"; do
  case "$prev" in --session-id|--resume) sid="$arg" ;; esac
  prev="$arg"
done
prompt=$(cat)
say() { printf '%s\n' "$1"; }
say '{"type":"system","subtype":"init","session_id":"'"$sid"'","model":"fake-model"}'
tool() { say '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"'"$1"'"}}]}}'; }
result() { say '{"type":"result","subtype":"success","is_error":false,"result":"'"$1"'","session_id":"'"$sid"'","num_turns":1,"duration_ms":5,"total_cost_usd":0.01}'; }
case "$prompt" in
  *"and commit it"*)
    tool "git commit --no-verify"
    printf 'one\n' > agent.txt && git add agent.txt && git commit --quiet --no-verify -m "Agent: add agent.txt" >&2
    printf 'a draft\n' > draft.md
    result "Committed agent.txt; draft.md is still a draft." ;;
  *)
    tool "git commit"
    printf 'a draft\n' > draft.md && git add draft.md >&2
    git commit --quiet -m "Agent: add draft.md" >&2 || true
    result "Wrote draft.md; the pre-commit hook refused the commit." ;;
esac
"#;

/// A repository whose pre-commit hook refuses every commit, and the
/// leaving agent: the app, its window and the repository.
fn leaving_app(base: &Path) -> (Shots, AnyWindowHandle, PathBuf, PathBuf) {
    let work_dir = repository(base);
    // `repository` points core.hooksPath here.
    write_script(
        &base.join("no hooks").join("pre-commit"),
        "#!/bin/sh\necho 'pre-commit: run the linter first' >&2\nexit 1\n",
    );
    let mut agent = review_agent(base);
    write_script(
        agent.program.as_ref().expect("the fake agent"),
        LEAVING_CLAUDE,
    );
    agent.env.clear();
    let (mut shots, common_dir) = app(base, &work_dir, agent, None);
    let (window, _) = shots.open_workspace(&work_dir);
    (shots, window, work_dir, common_dir)
}

/// Waits until the thread `key` ended its turn with uncommitted changes
/// recorded.
fn wait_for_leftovers(shots: &mut Shots, key: &AgentThreadKey) -> Arc<AgentThread> {
    let thread = wait_for(
        shots,
        key,
        Duration::from_secs(30),
        "the turn to end with uncommitted changes",
        |thread| !thread.state.is_active() && thread.uncommitted.is_some(),
    );
    assert_eq!(thread.has_uncommitted(), Some(true), "{thread:#?}");
    let uncommitted = thread.uncommitted.as_ref().expect("recorded");
    assert!(
        uncommitted
            .paths
            .iter()
            .any(|path| path.contains("draft.md")),
        "{uncommitted:?}"
    );
    assert!(
        uncommitted
            .commit_error
            .as_deref()
            .is_some_and(|error| error.contains("pre-commit")),
        "the hook's words are kept: {uncommitted:?}"
    );
    thread
}

fn open_thread(shots: &mut Shots, window: AnyWindowHandle, key: &AgentThreadKey) {
    shots.dispatch(
        window,
        crate::agent_threads::OpenAgentThread {
            common_dir: key.common_dir.clone(),
            id: key.id.clone(),
        },
    );
}

/// Settles until `done` holds, in real time, for up to `timeout`.
fn settle_until(shots: &mut Shots, timeout: Duration, what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        shots.settle();
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// C1, case 1: the hook refuses the agent's commit, so the thread ends
/// with uncommitted changes and says so; Complete asks; "Keep the
/// worktree" completes it and keeps the folder with the change; removing
/// the kept worktree asks again and deletes it.
#[test]
fn e2e_c1_uncommitted_keep_then_remove() {
    if !crate::screenshot::enabled() || std::env::var_os("GITEZ_E2E").is_none() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let (mut shots, window, work_dir, common_dir) = leaving_app(&base);
    let key = start(&mut shots, &work_dir, &common_dir, "Write draft.md", None);
    let thread = wait_for_leftovers(&mut shots, &key);
    assert_eq!(thread.pull_request, None, "nothing was committed");
    open_thread(&mut shots, window, &key);
    assert!(shots.has_id(window, "thread-ask-commit"));
    shots.save(window, "fix-c1-1-uncommitted-thread");
    shots.save_region(window, detail_region(), "fix-c1-1-uncommitted-thread-panel");

    shots.click_id(window, "thread-complete");
    assert!(
        shots.has_id(window, "complete-keep"),
        "Complete asks before discarding"
    );
    assert!(shots.has_id(window, "complete-discard"));
    shots.save(window, "fix-c1-2-complete-asks");
    shots.click_id(window, "complete-keep");
    let completed = wait_for(
        &mut shots,
        &key,
        Duration::from_secs(30),
        "the thread to complete",
        |thread| thread.state == State::Completed,
    );
    assert!(completed.worktree.join("draft.md").exists(), "kept");
    shots.save(window, "fix-c1-3-completed-kept");
    shots.save_region(window, detail_region(), "fix-c1-3-completed-kept-panel");

    shots.click_id(window, "thread-remove-worktree");
    assert!(shots.has_id(window, "remove-worktree-confirm"), "it asks");
    shots.save(window, "fix-c1-4-remove-asks");
    shots.click_id(window, "remove-worktree-confirm");
    let worktree = completed.worktree.clone();
    settle_until(
        &mut shots,
        Duration::from_secs(30),
        "the worktree to be removed",
        || !worktree.exists(),
    );
    let worktrees = gitez_command_output(&work_dir, &["worktree", "list", "--porcelain"]);
    assert_eq!(
        worktrees
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count(),
        1,
        "{worktrees}"
    );
    shots.settle();
    assert!(!shots.has_id(window, "thread-remove-worktree"));
    shots.save_region(window, detail_region(), "fix-c1-5-removed-panel");
}

/// C1, case 2: "Discard and complete" removes the worktree and its
/// uncommitted change.
#[test]
fn e2e_c1_uncommitted_discard() {
    if !crate::screenshot::enabled() || std::env::var_os("GITEZ_E2E").is_none() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let (mut shots, window, work_dir, common_dir) = leaving_app(&base);
    let key = start(&mut shots, &work_dir, &common_dir, "Write draft.md", None);
    let thread = wait_for_leftovers(&mut shots, &key);
    open_thread(&mut shots, window, &key);
    shots.click_id(window, "thread-complete");
    assert!(shots.has_id(window, "complete-discard"), "it asks");
    shots.click_id(window, "complete-discard");
    let completed = wait_for(
        &mut shots,
        &key,
        Duration::from_secs(30),
        "the thread to complete",
        |thread| thread.state == State::Completed,
    );
    assert!(!thread.worktree.exists(), "discarded with the worktree");
    let worktrees = gitez_command_output(&work_dir, &["worktree", "list", "--porcelain"]);
    assert!(
        !worktrees.contains(&completed.worktree.display().to_string()),
        "{worktrees}"
    );
    shots.save_region(window, detail_region(), "fix-c1-6-discarded-panel");
}

/// C1, case 3: the agent commits past the hook and leaves a file, so its
/// pull request comes with uncommitted changes; merging it completes the
/// thread, keeps the worktree with the change, and the thread says so.
#[test]
fn e2e_c1_merge_keeps_leftovers() {
    if !crate::screenshot::enabled() || std::env::var_os("GITEZ_E2E").is_none() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let (mut shots, window, work_dir, common_dir) = leaving_app(&base);
    let key = start(
        &mut shots,
        &work_dir,
        &common_dir,
        "Write agent.txt and commit it",
        None,
    );
    let thread = wait_for_leftovers(&mut shots, &key);
    let number = thread.pull_request.expect("a pull request");
    shots.dispatch(
        window,
        OpenAgentPullRequest {
            common_dir: common_dir.clone(),
            number,
        },
    );
    shots.save(window, "fix-c1-7-pull-request-with-leftovers");
    scroll_panel(&mut shots, window);
    shots.save_region(
        window,
        panel_region(),
        "fix-c1-7-pull-request-with-leftovers-panel",
    );
    shots.click_id(window, "pull-request-merge");
    shots.save(window, "fix-c1-7-merge-dialog");
    shots.click(window, merge_option());
    let completed = wait_for(
        &mut shots,
        &key,
        Duration::from_secs(30),
        "the thread to complete",
        |thread| thread.state == State::Completed,
    );
    assert!(
        completed.worktree.join("draft.md").exists(),
        "the merge keeps the worktree with its uncommitted change"
    );
    assert!(
        completed
            .warnings
            .iter()
            .any(|warning| warning.contains(&format!(
                "Pull request #{number} was merged; GitEZ completed the thread but kept its worktree"
            ))),
        "{:?}",
        completed.warnings
    );
    let merged = gitez_review::Store::new(&common_dir)
        .load(number)
        .expect("load");
    assert_eq!(merged.state, gitez_review::State::Merged);
    open_thread(&mut shots, window, &key);
    assert!(shots.has_id(window, "thread-remove-worktree"));
    shots.save(window, "fix-c1-8-merged-kept");
    shots.save_region(window, detail_region(), "fix-c1-8-merged-kept-panel");
}
