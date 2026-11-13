//! Screenshots of the toolbar's Home button and Agents indicator, its
//! popover and the sidebar's `agent/` folder, and runs that drive them:
//! clicks on the popover's rows and buttons, checked against the window's
//! state. All of them return at once unless `GITEZ_SCREENSHOTS` is set;
//! the run with a real supervisor also needs `GITEZ_E2E=1` and a built
//! `target/debug/gitez` (see `composer_shots.rs`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use gitez_agent_thread::{
    Activity, AgentThread, AgentThreadId, Ending, IndexedRepository, State, Step, Timestamp,
    TurnKind, TurnResult,
};
use gitez_sandbox::NetworkProfile;
use gpui_kit::{AnyWindowHandle, Bounds, Entity, Pixels, Point, point, px, size};

use crate::agent_threads::{AgentThreadKey, AgentThreads};
use crate::screenshot::{Shots, git};
use crate::workspace::Workspace;
use crate::workspace::composer_shots::{
    common_dir_of, fake_agent, gitez_binary, minutes_ago, repository, save_settings,
    supervisor_script, thread, wait_until,
};

/// The toolbar, across the whole window.
fn toolbar() -> Bounds<Pixels> {
    Bounds {
        origin: point(px(0.), px(0.)),
        size: size(px(1440.), px(48.)),
    }
}

/// The right part of the window, where the popover opens.
fn popover_region() -> Bounds<Pixels> {
    Bounds {
        origin: point(px(1000.), px(0.)),
        size: size(px(440.), px(720.)),
    }
}

/// The sidebar.
fn sidebar_region() -> Bounds<Pixels> {
    Bounds {
        origin: point(px(0.), px(48.)),
        size: size(px(240.), px(520.)),
    }
}

/// Window coordinates of what the tests click, read off `u3-popover`: the
/// indicator at the toolbar's right end, Home at its left end, and, in the
/// popover (360 wide, its right edge at 1426), rows and their buttons.
mod at {
    use gpui_kit::{Pixels, Point, point, px};

    pub fn indicator() -> Point<Pixels> {
        point(px(1400.), px(24.))
    }

    pub fn home() -> Point<Pixels> {
        point(px(26.), px(24.))
    }

    /// Somewhere that closes the popover and does nothing else: the
    /// status bar.
    pub fn outside() -> Point<Pixels> {
        point(px(700.), px(890.))
    }

    /// The footer's "All threads", under [`super::mix`]'s rows.
    pub fn all_threads() -> Point<Pixels> {
        let y = std::env::var("U3_ALL_THREADS_Y")
            .ok()
            .and_then(|y| y.parse().ok())
            .unwrap_or(700.);
        point(px(1366.), px(y))
    }

    /// The first row of the first group: its trailing button (Stop on a
    /// running row).
    pub fn first_row_trailing() -> Point<Pixels> {
        point(px(1389.), px(146.))
    }

    /// The first button under the text of a failed row that is first:
    /// Retry.
    pub fn first_row_retry() -> Point<Pixels> {
        point(px(1130.), px(192.))
    }

    /// Under one failed row, the first row of the next group: its text
    /// and its trailing button (Complete on an answer).
    pub fn second_group_row() -> Point<Pixels> {
        point(px(1150.), px(271.))
    }

    pub fn second_group_trailing() -> Point<Pixels> {
        point(px(1369.), px(264.))
    }

    /// The row under that one: its trailing button (Review on a pull
    /// request).
    pub fn second_group_next_trailing() -> Point<Pixels> {
        point(px(1371.), px(318.))
    }
}

fn index(data_dir: &Path, work_dir: &Path, name: &str) {
    gitez_agent_thread::Index::new(data_dir)
        .add(IndexedRepository {
            common_dir: common_dir_of(work_dir),
            work_dir: work_dir.to_owned(),
            name: name.into(),
        })
        .expect("index");
}

fn result(message: &str, success: bool, blocked: &[&str]) -> TurnResult {
    TurnResult {
        success,
        message: message.into(),
        cost_usd: Some(0.42),
        duration_ms: Some(60_000),
        steps: 12,
        exit_code: Some(if success { 0 } else { 1 }),
        stopped: false,
        blocked_hosts: blocked.iter().map(|host| (*host).to_owned()).collect(),
    }
}

/// Runs for `minutes` and ended `ago` minutes ago.
fn ran(thread: &mut AgentThread, minutes: i64, ago: i64) {
    thread.created = minutes_ago(ago + minutes + 1);
    thread.turns[0].started = Some(minutes_ago(ago + minutes));
    thread.turns[0].ended = Some(minutes_ago(ago));
}

/// Running, started `minutes` ago, doing `tool` on `detail`.
fn running(minutes: i64, tool: &str, detail: &str) -> impl FnOnce(&mut AgentThread) {
    let (tool, detail) = (tool.to_owned(), detail.to_owned());
    move |thread| {
        thread.state = State::Running;
        thread.created = minutes_ago(minutes);
        thread.network = NetworkProfile::Packages;
        thread.turns[0].started = Some(minutes_ago(minutes));
        thread.activity = Some(Activity {
            tool: Some(tool),
            detail,
            steps: 31,
            updated: Timestamp::now(),
        });
    }
}

/// Adds a commit on `branch` (made from `main`) and returns its tip.
fn commit_on(work_dir: &Path, branch: &str, file: &str, message: &str) -> String {
    git(work_dir, &["checkout", "-qb", branch, "main"]);
    std::fs::write(work_dir.join(file), "fn windows_paths() {}\n".repeat(30)).expect("write");
    git(work_dir, &["add", "."]);
    git(work_dir, &["commit", "-qm", message]);
    git(work_dir, &["checkout", "-q", "main"]);
    gitez_git::Repository::open(work_dir)
        .expect("open")
        .resolve_commit(&format!("refs/heads/{branch}"))
        .expect("resolve")
        .expect("a tip")
        .to_string()
}

/// The repository and threads of a realistic popover.
struct Mix {
    work_dir: PathBuf,
    data_dir: PathBuf,
    common_dir: PathBuf,
    answer: AgentThreadId,
    pull_request: u32,
}

/// A repository with threads in every group: running, queued, failed
/// (blocked by the network), finished with a pull request, finished with
/// an answer, two completed; and another repository with one running.
fn mix(base: &Path) -> Mix {
    let work_dir = repository(base);
    let data_dir = base.join("data");
    let common_dir = common_dir_of(&work_dir);
    save_settings(&data_dir, &common_dir, fake_agent(base, true));

    thread(
        &work_dir,
        &data_dir,
        "Add a blame view to the commit detail",
        running(14, "Bash", "cargo test -p gitez-git blame"),
    );
    thread(
        &work_dir,
        &data_dir,
        "Document the review storage format",
        |thread| thread.created = minutes_ago(2),
    );
    thread(
        &work_dir,
        &data_dir,
        "Fix the flaky watcher test on Linux",
        |thread| {
            ran(thread, 6, 40);
            thread.fail(
                Step::Agent,
                "The agent could not download crates.",
                minutes_ago(40),
            );
            thread.turns[0].result = Some(result("", false, &["static.crates.io"]));
            thread.session_id = Some("session-failed".into());
        },
    );
    let windows = thread(
        &work_dir,
        &data_dir,
        "Windows path handling in gitez-command",
        |thread| {
            ran(thread, 26, 60);
            thread.state = State::Finished;
            thread.ending = Some(Ending::PullRequest);
            thread.session_id = Some("session-pr".into());
            thread.turns[0].result = Some(result("Accepts verbatim paths.", true, &[]));
        },
    );
    let head = commit_on(
        &work_dir,
        &windows.branch,
        "paths.rs",
        "Accept verbatim and UNC paths",
    );
    let base_commit = windows.start.commit.clone();
    let pull_request = gitez_review::Store::new(&common_dir)
        .create(
            gitez_review::NewPullRequest {
                title: windows.title.clone(),
                description: "Accepts verbatim paths.".into(),
                base: gitez_review::Tip {
                    branch: "main".into(),
                    commit: base_commit,
                },
                head: gitez_review::Tip {
                    branch: windows.branch.clone(),
                    commit: head,
                },
            },
            Timestamp::now(),
        )
        .expect("pull request")
        .number;
    gitez_agent_thread::Store::new(&common_dir)
        .update(&windows.id, |thread| {
            thread.pull_request = Some(pull_request);
            Ok(())
        })
        .expect("link the pull request");
    let answer = thread(
        &work_dir,
        &data_dir,
        "Explain how the graph layout caches lanes",
        |thread| {
            ran(thread, 3, 20);
            thread.state = State::Finished;
            thread.ending = Some(Ending::Answer);
            thread.session_id = Some("session-answer".into());
            thread.turns[0].result = Some(result("Lanes are cached per ref snapshot.", true, &[]));
        },
    );
    for (title, days) in [
        ("Diff whitespace toggle", 2),
        ("Rename the sidebar sections", 3),
    ] {
        thread(&work_dir, &data_dir, title, |thread| {
            ran(thread, 9, days * 24 * 60);
            thread.state = State::Completed;
            thread.ending = Some(Ending::Answer);
            thread.completed = Some(minutes_ago(days * 24 * 60));
        });
    }
    index(&data_dir, &work_dir, "repo");

    // Another repository, with a thread of its own running.
    let other = repository(&base.join("other"));
    thread(
        &other,
        &data_dir,
        "Speed up the status read",
        running(5, "Edit", "src/status.rs"),
    );
    index(&data_dir, &other, "notes-site");

    Mix {
        work_dir,
        data_dir,
        common_dir,
        answer: answer.id,
        pull_request,
    }
}

fn shown_thread(shots: &mut Shots, workspace: &Entity<Workspace>) -> Option<AgentThreadKey> {
    shots.update(|cx| {
        workspace
            .read(cx)
            .agent_thread
            .as_ref()
            .map(|(thread, _, _)| thread.read(cx).key().clone())
    })
}

fn shown_pull_request(shots: &mut Shots, workspace: &Entity<Workspace>) -> Option<u32> {
    shots.update(|cx| {
        workspace
            .read(cx)
            .review
            .as_ref()
            .and_then(|(review, _)| review.read(cx).pull_request_number())
    })
}

fn open_popover(shots: &mut Shots, window: AnyWindowHandle) {
    shots.click(window, at::indicator());
}

/// The toolbar with no threads, with two running, and with one running
/// and two finished waiting for the user.
#[test]
fn screenshot_u3_toolbar() {
    if !crate::screenshot::enabled() {
        return;
    }
    for (name, finished) in [("none", 0), ("two-running", 0), ("waiting", 2)] {
        let dir = tempfile::tempdir().expect("temp dir");
        let work_dir = repository(dir.path());
        let data_dir = dir.path().join("data");
        save_settings(
            &data_dir,
            &common_dir_of(&work_dir),
            fake_agent(dir.path(), true),
        );
        let runs = match name {
            "none" => 0,
            "two-running" => 2,
            _ => 1,
        };
        for (index, title) in ["Add a blame view", "Speed up graph layout"]
            .iter()
            .take(runs)
            .enumerate()
        {
            thread(
                &work_dir,
                &data_dir,
                title,
                running(14 + index as i64 * 20, "Bash", "cargo test"),
            );
        }
        for (index, title) in ["Windows path handling", "Explain the cache"]
            .iter()
            .take(finished)
            .enumerate()
        {
            thread(&work_dir, &data_dir, title, |thread| {
                ran(thread, 5, 30);
                thread.state = State::Finished;
                thread.ending = Some(if index == 0 {
                    Ending::Failed
                } else {
                    Ending::Answer
                });
            });
        }
        index(&data_dir, &work_dir, "repo");
        let mut shots = Shots::from_env(&data_dir).expect("enabled");
        let (window, _) = shots.open_workspace(&work_dir);
        shots.save(window, &format!("u3-window-{name}"));
        shots.save_region(window, toolbar(), &format!("u3-toolbar-{name}"));
    }
}

/// The popover with a realistic mix of threads, and with none.
#[test]
fn screenshot_u3_popover() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let mix = mix(dir.path());
    let mut shots = Shots::from_env(&mix.data_dir).expect("enabled");
    let (window, _) = shots.open_workspace(&mix.work_dir);
    open_popover(&mut shots, window);
    shots.save(window, "u3-popover");
    shots.save_region(window, popover_region(), "u3-popover-detail");

    let dir = tempfile::tempdir().expect("temp dir");
    let work_dir = repository(dir.path());
    let mut shots = Shots::from_env(&dir.path().join("data")).expect("enabled");
    let (window, _) = shots.open_workspace(&work_dir);
    open_popover(&mut shots, window);
    shots.save_region(window, popover_region(), "u3-popover-empty");
}

/// The sidebar with two agent branches: one collapsed `agent/` row, then
/// expanded.
#[test]
fn screenshot_u3_sidebar() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let work_dir = repository(dir.path());
    for branch in ["agent/blame-view", "agent/windows-paths"] {
        commit_on(&work_dir, branch, &format!("{}.rs", &branch[6..]), "Work");
    }
    let mut shots = Shots::from_env(&dir.path().join("data")).expect("enabled");
    let (window, _) = shots.open_workspace(&work_dir);
    shots.save(window, "u3-sidebar-window");
    shots.save_region(window, sidebar_region(), "u3-sidebar-collapsed");
    shots.click(window, folder_row());
    shots.save_region(window, sidebar_region(), "u3-sidebar-expanded");
}

/// The `agent/` row in Branches of `repository`'s sidebar: no pull
/// requests, the sections' headings, and `main`, `feature/parser`,
/// `fix/typo` above it.
fn folder_row() -> Point<Pixels> {
    let y = std::env::var("U3_FOLDER_Y")
        .ok()
        .and_then(|y| y.parse().ok())
        .unwrap_or(321.);
    point(px(80.), px(y))
}

/// Rows open their thread in the detail panel; Review opens the pull
/// request; Home and All threads close the repository, and without one a
/// row shows its thread in Home.
#[test]
fn screenshot_u3_drive() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let mix = mix(dir.path());
    let mut shots = Shots::from_env(&mix.data_dir).expect("enabled");
    let (window, workspace) = shots.open_workspace(&mix.work_dir);

    // The finished answer: newest, first in Ready for review.
    open_popover(&mut shots, window);
    shots.click(window, at::second_group_row());
    assert_eq!(
        shown_thread(&mut shots, &workspace),
        Some(AgentThreadKey {
            common_dir: mix.common_dir.clone(),
            id: mix.answer.clone(),
        }),
        "a row opens its thread in the detail panel"
    );
    shots.save(window, "u3-drive-1-thread");

    // The pull request's Review button: second in Ready for review.
    open_popover(&mut shots, window);
    shots.save(window, "u3-drive-2-popover");
    shots.click(window, at::second_group_next_trailing());
    assert_eq!(
        shown_pull_request(&mut shots, &workspace),
        Some(mix.pull_request),
        "Review opens the thread's pull request"
    );
    shots.save(window, "u3-drive-3-pull-request");

    // All threads closes the repository: Home.
    open_popover(&mut shots, window);
    shots.save(window, "u3-drive-4-popover-over-review");
    shots.click(window, at::all_threads());
    let open = shots.update(|cx| workspace.read(cx).snapshot.is_some());
    assert!(!open, "All threads closes the repository");
    shots.save(window, "u3-drive-5-all-threads");

    // Without a repository the popover lists every repository's threads;
    // a row shows the thread in Home.
    open_popover(&mut shots, window);
    shots.save(window, "u3-drive-6-popover-all");
    shots.click(window, at::second_group_row());
    let open = shots.update(|cx| workspace.read(cx).snapshot.is_some());
    assert!(!open, "Home shows the thread itself");
    let in_home = shots.update(|cx| {
        let workspace = workspace.read(cx);
        let (home, _) = workspace.home.as_ref().expect("Home is made");
        home.read(cx).shown_thread(cx)
    });
    assert_eq!(
        in_home,
        Some(AgentThreadKey {
            common_dir: mix.common_dir.clone(),
            id: mix.answer.clone(),
        }),
        "the row's thread"
    );
    shots.save(window, "u3-drive-7-opened");

    // Open the repository again, then go Home.
    let work_dir = mix.work_dir.clone();
    shots.update_window(window, |window, cx| {
        workspace.update(cx, |workspace, cx| workspace.open(work_dir, window, cx))
    });
    let open = shots.update(|cx| workspace.read(cx).snapshot.is_some());
    assert!(open, "the repository opens again");
    // Home closes the repository too.
    shots.click(window, at::home());
    let open = shots.update(|cx| workspace.read(cx).snapshot.is_some());
    assert!(!open, "Home closes the repository");
    shots.save(window, "u3-drive-8-home");
}

/// With a real supervisor and the fake agent: the popover's Stop ends a
/// running thread, Retry runs a failed one again, Complete removes a
/// finished answer's worktree.
#[test]
fn e2e_u3_popover_actions() {
    if !crate::screenshot::enabled() || std::env::var_os("GITEZ_E2E").is_none() {
        return;
    }
    let binary = gitez_binary().expect("build target/debug/gitez first: cargo build -p gitez");
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let work_dir = repository(&base);
    let common_dir = common_dir_of(&work_dir);
    let data_dir = base.join("data");
    save_settings(&data_dir, &common_dir, fake_agent(&base, true));
    let script = supervisor_script(&base, &binary, &data_dir, true);
    let mut shots = Shots::with_options(&data_dir, |options| {
        options.exe = Ok(script);
        options.watch = true;
    })
    .expect("enabled");
    let (window, _) = shots.open_workspace(&work_dir);
    let start = |shots: &mut Shots, prompt: &str| {
        shots.keys(window, "cmd-shift-a");
        shots.type_text(window, prompt);
        shots.keys(window, "cmd-enter");
    };

    // Stop, on the only (running) thread.
    start(&mut shots, "Wait a while. SLEEP");
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the thread to run",
        |threads| threads.iter().any(|thread| thread.state == State::Running),
    );
    let sleeper = threads[0].id.clone();
    open_popover(&mut shots, window);
    shots.save(window, "u3-e2e-1-running");
    shots.click(window, at::first_row_trailing());
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the thread to stop",
        |threads| {
            threads
                .iter()
                .any(|thread| thread.id == sleeper && thread.state == State::Finished)
        },
    );
    let stopped = threads
        .iter()
        .find(|thread| thread.id == sleeper)
        .expect("it");
    assert_eq!(stopped.ending, Some(Ending::Stopped), "Stop stops it");
    shots.save(window, "u3-e2e-2-stopped");
    shots.click(window, at::outside());

    // Retry, on a thread whose agent fails: the newest in Needs attention.
    start(&mut shots, "Try something. FAIL");
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the thread to fail",
        |threads| {
            threads
                .iter()
                .any(|thread| thread.id != sleeper && thread.ending == Some(Ending::Failed))
        },
    );
    let failing = threads
        .iter()
        .find(|thread| thread.id != sleeper)
        .expect("the failed thread")
        .clone();
    assert!(
        failing.session_id.is_some(),
        "the agent started: {failing:#?}"
    );
    open_popover(&mut shots, window);
    shots.save(window, "u3-e2e-3-failed");
    shots.click(window, at::first_row_retry());
    // The retry continues the conversation; the fake agent answers.
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the retry to finish",
        |threads| {
            threads.iter().any(|thread| {
                thread.id == failing.id
                    && thread.state == State::Finished
                    && thread.ending == Some(Ending::Answer)
            })
        },
    );
    let retried = threads
        .iter()
        .find(|thread| thread.id == failing.id)
        .expect("it");
    assert_eq!(retried.turns.len(), 2, "Retry added a turn");
    assert!(
        retried.worktree.is_dir(),
        "a finished thread keeps its worktree"
    );
    shots.save(window, "u3-e2e-4-retried");
    shots.click(window, at::outside());

    // Complete, on the answer: first in Ready for review, under the
    // stopped thread in Needs attention.
    open_popover(&mut shots, window);
    shots.save(window, "u3-e2e-5-answer");
    shots.click(window, at::second_group_trailing());
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the thread to complete",
        |threads| {
            threads
                .iter()
                .any(|thread| thread.id == failing.id && thread.state == State::Completed)
        },
    );
    let completed = threads
        .iter()
        .find(|thread| thread.id == failing.id)
        .expect("it")
        .clone();
    assert!(
        !completed.worktree.exists(),
        "Complete removes the worktree: {}",
        completed.worktree.display()
    );
    assert!(completed.completed.is_some());
    // Readable, read-only: it loads, and a reply is refused.
    let loaded = gitez_agent_thread::Store::new(&common_dir)
        .load(&completed.id)
        .expect("a completed thread stays readable");
    assert_eq!(loaded.final_message(), Some("The answer is 42."));
    assert!(
        loaded
            .clone()
            .request_turn(TurnKind::Reply, "More".into(), Timestamp::now())
            .is_err(),
        "a completed thread takes no reply"
    );
    let shown = shots.update(|cx| {
        AgentThreads::global(cx)
            .read(cx)
            .thread(&AgentThreadKey {
                common_dir: common_dir.clone(),
                id: completed.id.clone(),
            })
            .map(|thread| thread.state)
    });
    assert_eq!(
        shown,
        Some(State::Completed),
        "the model shows it completed"
    );
    shots.save(window, "u3-e2e-6-completed");
    shots.dispatch(
        window,
        crate::agent_threads::OpenAgentThread {
            common_dir: common_dir.clone(),
            id: completed.id.clone(),
        },
    );
    shots.save(window, "u3-e2e-7-completed-thread");
}
