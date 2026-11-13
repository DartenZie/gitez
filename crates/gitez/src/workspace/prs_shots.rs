//! Screenshots of the sidebar's Local pull requests section: only the pull
//! requests in review are listed and counted; the merged and closed ones
//! wait behind one muted row, and stay reachable. They return at once
//! unless `GITEZ_SCREENSHOTS` is set.

use std::path::{Path, PathBuf};

use gitez_review::{NewPullRequest, State, Store, Timestamp, Tip};
use gpui_kit::{AnyWindowHandle, Bounds, Entity, Pixels, Point, point, px, size};

use crate::agent_threads::OpenAgentPullRequest;
use crate::screenshot::{Shots, git};
use crate::workspace::Workspace;
use crate::workspace::composer_shots::{common_dir_of, repository};

fn sidebar() -> Bounds<Pixels> {
    Bounds {
        origin: point(px(0.), px(48.)),
        size: size(px(240.), px(560.)),
    }
}

/// The muted row of the finished pull requests, under `open` rows of open
/// ones (each 41 high), read off `prs-window`; `PRS_FINISHED_Y` moves it.
fn finished_row(open: usize) -> Point<Pixels> {
    let y = std::env::var("PRS_FINISHED_Y")
        .ok()
        .and_then(|y| y.parse().ok())
        .unwrap_or(182. + 41. * open as f32);
    point(px(80.), px(y))
}

/// A pull request of `branch` into `main` with `title`, in `state`.
fn pull_request(work_dir: &Path, title: &str, branch: &str, state: State) -> u32 {
    let tip = |branch: &str| {
        let commit = gitez_git::Repository::open(work_dir)
            .expect("open")
            .resolve_commit(&format!("refs/heads/{branch}"))
            .expect("resolve")
            .expect("a tip")
            .to_string();
        Tip {
            branch: branch.into(),
            commit,
        }
    };
    let store = Store::new(&common_dir_of(work_dir));
    let mut created = store
        .create(
            NewPullRequest {
                title: title.into(),
                description: String::new(),
                base: tip("main"),
                head: tip(branch),
            },
            Timestamp::now(),
        )
        .expect("create");
    created.state = state;
    store.save(&created).expect("save");
    created.number
}

/// The sample repository with two more branches.
fn setup(base: &Path) -> PathBuf {
    let work_dir = repository(base);
    for branch in ["docs/storage", "perf/graph"] {
        git(&work_dir, &["branch", branch, "feature/parser"]);
    }
    work_dir
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

fn open_pull_request(shots: &mut Shots, window: AnyWindowHandle, work_dir: &Path, number: u32) {
    shots.dispatch(
        window,
        OpenAgentPullRequest {
            common_dir: common_dir_of(work_dir),
            number,
        },
    );
}

/// Two merged, one closed and one open: collapsed, then expanded. A
/// merged one still opens; one closed on its own screen stays open there
/// and leaves the list, and comes back when reopened.
#[test]
fn screenshot_prs_open_and_finished() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let work_dir = setup(dir.path());
    let merged = pull_request(
        &work_dir,
        "Fix a typo in the README",
        "fix/typo",
        State::Merged,
    );
    pull_request(
        &work_dir,
        "Describe the review storage",
        "docs/storage",
        State::Merged,
    );
    pull_request(
        &work_dir,
        "Virtualise the graph",
        "perf/graph",
        State::Closed,
    );
    let open = pull_request(&work_dir, "Add a parser", "feature/parser", State::Open);

    let mut shots = Shots::from_env(&dir.path().join("data")).expect("enabled");
    let (window, workspace) = shots.open_workspace(&work_dir);
    shots.save(window, "prs-window");
    shots.save_region(window, sidebar(), "prs-collapsed");
    shots.click(window, finished_row(1));
    shots.save_region(window, sidebar(), "prs-expanded");
    shots.click(window, finished_row(1));

    // Reachable elsewhere: the Agents popover's and Home's Review, the
    // thread card's "Review PR #n", all through this action.
    open_pull_request(&mut shots, window, &work_dir, merged);
    assert_eq!(shown_pull_request(&mut shots, &workspace), Some(merged));
    shots.save(window, "prs-merged-opens");

    // Closed while its screen is open: the screen stays, the list loses it.
    open_pull_request(&mut shots, window, &work_dir, open);
    assert_eq!(shown_pull_request(&mut shots, &workspace), Some(open));
    shots.click_id(window, "pull-request-close");
    assert_eq!(shown_pull_request(&mut shots, &workspace), Some(open));
    shots.save(window, "prs-closed-while-open");
    // The breadcrumb's History goes back; the list no longer has it.
    shots.click_id(window, "review-back");
    assert_eq!(shown_pull_request(&mut shots, &workspace), None);
    shots.save_region(window, sidebar(), "prs-closed-back");
    // Reopened from its screen: back in the list.
    open_pull_request(&mut shots, window, &work_dir, open);
    shots.click_id(window, "pull-request-close");
    assert_eq!(shown_pull_request(&mut shots, &workspace), Some(open));
    shots.click_id(window, "review-back");
    shots.save_region(window, sidebar(), "prs-reopened");
}

/// Only merged pull requests: no open one is listed, only the muted row.
#[test]
fn screenshot_prs_only_merged() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let work_dir = setup(dir.path());
    pull_request(
        &work_dir,
        "When I merge main into a branch",
        "fix/typo",
        State::Merged,
    );
    pull_request(
        &work_dir,
        "Fix a hang in GitEZ's Pull",
        "perf/graph",
        State::Merged,
    );
    let mut shots = Shots::from_env(&dir.path().join("data")).expect("enabled");
    let (window, _) = shots.open_workspace(&work_dir);
    shots.save_region(window, sidebar(), "prs-only-merged");
    shots.click(window, finished_row(0));
    shots.save_region(window, sidebar(), "prs-only-merged-expanded");
}
