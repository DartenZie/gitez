//! Screenshots of what `main`'s cancellable git operations look like next
//! to agent threads: the toolbar's Cancel beside Home and the Agents
//! indicator, the sidebar row of a running pull or fetch beside the
//! `agent/` folder, the notice of a git that stalled, and the menu of a
//! thread's branch, which does not offer Pull. They return at once unless
//! `GITEZ_SCREENSHOTS` is set.

use std::path::{Path, PathBuf};

use gitez_agent_thread::{Activity, Ending, IndexedRepository, State, Timestamp};
use gpui_kit::{AnyWindowHandle, Bounds, Entity, Pixels, point, px, size};

use crate::screenshot::{Shots, git};
use crate::workspace::Workspace;
use crate::workspace::composer_shots::{
    common_dir_of, fake_agent, minutes_ago, repository, save_settings, thread,
};
use crate::workspace::remote::{RemoteOp, RemoteTarget};

/// A point in the sidebar at height `y`, or what environment variable
/// `name` says, read off `merge-open`.
fn at(name: &str, y: f32) -> gpui_kit::Point<Pixels> {
    let y = std::env::var(name)
        .ok()
        .and_then(|y| y.parse().ok())
        .unwrap_or(y);
    point(px(80.), px(y))
}

fn toolbar() -> Bounds<Pixels> {
    Bounds {
        origin: point(px(0.), px(0.)),
        size: size(px(1440.), px(48.)),
    }
}

fn sidebar() -> Bounds<Pixels> {
    Bounds {
        origin: point(px(0.), px(48.)),
        size: size(px(240.), px(620.)),
    }
}

/// A repository with a remote that `main` tracks, a running thread and a
/// finished one whose branch is published to the remote, so it has an
/// upstream to pull from. Returns the work dir, the data folder and the
/// published thread branch.
fn setup(base: &Path) -> (PathBuf, PathBuf, String) {
    let work_dir = repository(base);
    let data_dir = base.join("data");
    let common_dir = common_dir_of(&work_dir);
    save_settings(&data_dir, &common_dir, fake_agent(base, true));
    let remote = base.join("remote.git");
    git(base, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
    git(
        &work_dir,
        &["remote", "add", "origin", remote.to_str().expect("utf-8")],
    );
    git(&work_dir, &["push", "-q", "-u", "origin", "main"]);

    let running = thread(
        &work_dir,
        &data_dir,
        "Add a blame view to the commit detail",
        |thread| {
            thread.state = State::Running;
            thread.created = minutes_ago(14);
            thread.turns[0].started = Some(minutes_ago(14));
            thread.activity = Some(Activity {
                tool: Some("Bash".into()),
                detail: "cargo test -p gitez-git blame".into(),
                steps: 31,
                updated: Timestamp::now(),
            });
        },
    );
    git(&work_dir, &["branch", &running.branch, "main"]);
    let finished = thread(
        &work_dir,
        &data_dir,
        "Windows path handling in gitez-command",
        |thread| {
            thread.created = minutes_ago(60);
            thread.turns[0].started = Some(minutes_ago(59));
            thread.turns[0].ended = Some(minutes_ago(30));
            thread.state = State::Finished;
            thread.ending = Some(Ending::Answer);
        },
    );
    git(&work_dir, &["checkout", "-qb", &finished.branch, "main"]);
    std::fs::write(work_dir.join("paths.rs"), "fn windows_paths() {}\n").expect("write");
    git(&work_dir, &["add", "."]);
    git(
        &work_dir,
        &["commit", "-qm", "Accept verbatim and UNC paths"],
    );
    git(&work_dir, &["checkout", "-q", "main"]);
    git(&work_dir, &["push", "-q", "-u", "origin", &finished.branch]);
    gitez_agent_thread::Index::new(&data_dir)
        .add(IndexedRepository {
            common_dir,
            work_dir: work_dir.clone(),
            name: "repo".into(),
        })
        .expect("index");
    (work_dir, data_dir, finished.branch)
}

fn show_running(
    shots: &mut Shots,
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    op: RemoteOp,
    target: RemoteTarget,
    said: Option<&str>,
) {
    shots.update_window(window, |window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.show_running_for_test(op, target, said, window, cx)
        })
    });
}

/// A pull of `main` running, then a fetch from every remote with what
/// ssh said, then a thread branch's menu.
#[test]
fn screenshot_merge_cancel_beside_threads() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let (work_dir, data_dir, _published) = setup(dir.path());
    let mut shots = Shots::from_env(&data_dir).expect("enabled");
    let (window, workspace) = shots.open_workspace(&work_dir);
    shots.save(window, "merge-open");
    shots.click(window, at("MERGE_FOLDER_Y", 320.));
    assert!(
        shots.has_id(window, "home") && shots.has_id(window, "agents-indicator"),
        "the feature's toolbar is there"
    );

    show_running(
        &mut shots,
        window,
        &workspace,
        RemoteOp::Pull,
        RemoteTarget::Branch("main".into()),
        None,
    );
    assert!(
        shots.has_id(window, "cancel-remote"),
        "the toolbar's Cancel"
    );
    shots.save(window, "merge-pull-running");
    shots.save_region(window, toolbar(), "merge-pull-running-toolbar");
    shots.save_region(window, sidebar(), "merge-pull-running-sidebar");

    let workspace_fetch = workspace.clone();
    shots.update(|cx| workspace_fetch.update(cx, |workspace, _| workspace.remote_op = None));
    show_running(
        &mut shots,
        window,
        &workspace,
        RemoteOp::Fetch,
        RemoteTarget::Remotes,
        Some("ssh: connect to host example.invalid port 22: Operation timed out"),
    );
    // Notices slide in on an animation that runs in real time.
    std::thread::sleep(std::time::Duration::from_millis(600));
    shots.settle();
    shots.save(window, "merge-fetch-stalled");
    shots.save_region(window, sidebar(), "merge-fetch-stalled-sidebar");

    // The published thread branch: Pull is not offered, and says why.
    let workspace_idle = workspace.clone();
    shots.update(|cx| {
        workspace_idle.update(cx, |workspace, cx| {
            workspace.remote_op = None;
            workspace
                .sidebar
                .update(cx, |sidebar, cx| sidebar.set_running(None, cx));
        })
    });
    let row = at("MERGE_PUBLISHED_Y", 376.);
    shots.right_click(window, row);
    shots.save(window, "merge-thread-branch-menu");
    shots.save_region(
        window,
        Bounds {
            origin: point(px(0.), row.y - px(60.)),
            size: size(px(520.), px(420.)),
        },
        "merge-thread-branch-menu-detail",
    );
}
