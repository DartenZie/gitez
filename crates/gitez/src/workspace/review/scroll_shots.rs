//! A local pull request whose description is long, as an agent thread's
//! final message is: the header stays put, the description scrolls away
//! with the diff and starts collapsed, and the diff is reachable at every
//! window size. All of them return at once unless `GITEZ_SCREENSHOTS` is
//! set; `screenshot_scroll_measure` also needs `GITEZ_MEASURE=1`.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use gitez_agent_thread::{AgentThread, Ending, State, Timestamp, TurnResult};
use gpui_kit::{
    AnyWindowHandle, Bounds, Entity, InputEvent as _, ListOffset, Pixels, Point, ScrollDelta,
    ScrollWheelEvent, Size, point, px, size,
};

use super::{Review, Row, Subject};
use crate::agent_threads::OpenAgentPullRequest;
use crate::screenshot::{Shots, git};
use crate::workspace::Workspace;
use crate::workspace::composer_shots::{common_dir_of, minutes_ago, repository, thread};
use crate::workspace::motion_tests::index_and_settings;

/// The final message of the thread in the owner's report: 45 lines of
/// Markdown with five headings.
pub(super) const LONG_DESCRIPTION: &str = "\
## What was wrong

GitEZ's **Pull** on a branch that is not checked out ran `git pull` in the
main worktree, which pulled into whatever branch was checked out there and
then waited for an editor to open for the merge message. Nothing showed the
editor, so the toolbar spinner turned forever.

## What changed

- A branch that is not checked out is now updated with `git fetch <remote>
  <branch>:<branch>`, which moves the branch without touching any worktree.
- When the fetch is not a fast-forward, GitEZ stops and says so instead of
  merging behind the user's back.
- Every git command GitEZ runs now sets `GIT_EDITOR=true` and
  `GIT_TERMINAL_PROMPT=0`, so no command can wait for input that never comes.
- The pull runs on the background executor and reports its progress in the
  status bar; the toolbar button shows it is busy and cannot be pressed twice.

## Tests

1. `pull_moves_a_branch_that_is_not_checked_out` creates a remote with two
   commits, pulls a branch that is not checked out, and checks that the
   branch moved and the checked-out branch did not.
2. `pull_refuses_a_branch_that_diverged` makes the branch diverge and checks
   that the pull fails with a message naming the branch.
3. `no_git_command_waits_for_an_editor` runs a merge that needs a message
   with `GIT_EDITOR` pointing at a script that would block, and checks that
   the command returns.

All 412 tests of the workspace pass: `cargo test --workspace`.

## How to check it

Open a repository with a remote, check out `main`, and press **Pull** on
another branch in the sidebar. The branch moves to its upstream, `main`
stays where it was, and the status bar says how many commits arrived.

## Left for later

- Pulling the checked-out branch still merges in the main worktree, as it
  did before; a rebase option belongs in the settings.
- The progress of a long fetch is shown only as a spinner; git's own
  progress lines could be parsed for a percentage.
- A remote that asks for a password now fails at once with a message,
  where it used to hang; credential helpers keep working.

Files: 16 changed. Commits: 3.";

/// Runs `git` in `dir` and returns what it printed.
fn output(dir: &Path, args: &[&str]) -> String {
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

/// Where file `index` of `files` lives: a few folders, as a real change has.
fn path_of(index: usize) -> String {
    const FOLDERS: [&str; 4] = [
        "crates/gitez-git/src",
        "crates/gitez-command/src",
        "crates/gitez/src/workspace",
        "tests",
    ];
    format!("{}/pull_{index:03}.rs", FOLDERS[index % FOLDERS.len()])
}

/// File `index` with `lines` lines, as on the base (`changed: false`) or
/// with every twelfth line changed and a few lines added.
fn source(index: usize, lines: usize, changed: bool) -> String {
    let mut text = String::new();
    for line in 0..lines {
        let text_line = match line % 4 {
            0 => format!("pub fn step_{index}_{line}(value: u32) -> u32 {{"),
            1 if changed && line % 12 == 5 => {
                format!(
                    "    // Changed: line {line} of file {index} no longer waits for an editor."
                )
            }
            1 => format!("    // Line {line} of file {index}: keep the pull fast."),
            2 => format!("    value.wrapping_mul({line}).wrapping_add({index})"),
            _ => "}".to_owned(),
        };
        text.push_str(&text_line);
        text.push('\n');
    }
    if changed {
        for line in 0..lines / 6 {
            text.push_str(&format!(
                "pub const ADDED_{index}_{line}: &str = \"set GIT_EDITOR=true for every command\";\n"
            ));
        }
    }
    text
}

/// A pull request of an agent thread's branch, three commits that change
/// `files` files of about `lines` lines each (a quarter of them new),
/// described by `description`. The thread is in `state`. Returns the
/// repository and the pull request's number.
pub(super) fn pull_request(
    base: &Path,
    files: usize,
    lines: usize,
    description: &str,
    state: State,
) -> (PathBuf, u32) {
    let work_dir = repository(base);
    let data_dir = base.join("data");
    index_and_settings(&work_dir, &data_dir);
    // The base: every file but the new ones.
    for index in (0..files).filter(|index| index % 4 != 3) {
        let path = work_dir.join(path_of(index));
        std::fs::create_dir_all(path.parent().expect("a folder")).expect("folders");
        std::fs::write(path, source(index, lines, false)).expect("write");
    }
    git(&work_dir, &["add", "."]);
    git(&work_dir, &["commit", "-qm", "Add the pull module"]);
    let made = thread(
        &work_dir,
        &data_dir,
        "Fix a hang in GitEZ's \"Pull\" for a branch that is not checked out in the main worktree",
        |thread| change_thread(thread, state),
    );
    gitez_agent_thread::create_worktree(
        &work_dir,
        &made.worktree,
        &made.branch,
        &made.start.commit,
    )
    .expect("worktree");
    let thirds = [0..files / 3, files / 3..files * 2 / 3, files * 2 / 3..files];
    for (number, range) in thirds.into_iter().enumerate() {
        for index in range {
            let path = made.worktree.join(path_of(index));
            std::fs::create_dir_all(path.parent().expect("a folder")).expect("folders");
            std::fs::write(path, source(index, lines, true)).expect("write");
        }
        git(&made.worktree, &["add", "."]);
        git(
            &made.worktree,
            &[
                "commit",
                "-qm",
                &format!("Fix the pull, part {}", number + 1),
            ],
        );
    }
    let common_dir = common_dir_of(&work_dir);
    let pull_request = gitez_review::Store::new(&common_dir)
        .create(
            gitez_review::NewPullRequest {
                title: made.title.clone(),
                description: description.into(),
                base: gitez_review::Tip {
                    branch: "main".into(),
                    commit: made.start.commit.clone(),
                },
                head: gitez_review::Tip {
                    branch: made.branch.clone(),
                    commit: output(&made.worktree, &["rev-parse", "HEAD"]),
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
    (work_dir, pull_request.number)
}

/// A thread that made a pull request and finished, or one at work again
/// (`State::Running`): dated ahead, so the liveness check without a
/// supervisor leaves it running.
fn change_thread(thread: &mut AgentThread, state: State) {
    thread.created = minutes_ago(50);
    thread.session_id = Some("session".into());
    thread.turns[0].started = Some(minutes_ago(49));
    thread.state = state;
    if state == State::Running {
        thread.turns[0].requested = Timestamp::now() + jiff::SignedDuration::from_hours(1);
        thread.turns[0].started = Some(Timestamp::now());
        thread.activity = Some(gitez_agent_thread::Activity {
            tool: Some("Bash".into()),
            detail: "cargo test -p gitez-command pull".into(),
            steps: 31,
            updated: Timestamp::now(),
        });
        return;
    }
    thread.ending = Some(Ending::PullRequest);
    thread.turns[0].ended = Some(minutes_ago(30));
    thread.turns[0].result = Some(TurnResult {
        success: true,
        message: LONG_DESCRIPTION.into(),
        cost_usd: Some(0.42),
        duration_ms: Some(1_140_000),
        steps: 58,
        exit_code: Some(0),
        stopped: false,
        blocked_hosts: Vec::new(),
    });
}

/// The main window of `size` with pull request `number` of `work_dir` open.
pub(super) fn open(
    base: &Path,
    work_dir: &Path,
    number: u32,
    size: gpui_kit::Size<gpui_kit::Pixels>,
) -> (Shots, AnyWindowHandle, Entity<Workspace>) {
    let mut shots = Shots::from_env(&base.join("data")).expect("enabled");
    let (window, workspace) = shots.open_workspace_sized(work_dir, size);
    shots.dispatch(
        window,
        OpenAgentPullRequest {
            common_dir: common_dir_of(work_dir),
            number,
        },
    );
    (shots, window, workspace)
}

/// The review the window shows.
pub(super) fn review_of(shots: &mut Shots, workspace: &Entity<Workspace>) -> Entity<Review> {
    shots.update(|cx| {
        workspace
            .read(cx)
            .review
            .as_ref()
            .map(|(review, _)| review.clone())
            .expect("a review is open")
    })
}

/// A short description, as a pull request made by hand has.
const SHORT_DESCRIPTION: &str = "Fixes the hang of Pull on a branch that is not checked out.";

/// Times a frame of the review while the diff scrolls three lines a frame,
/// down and back, on pull requests of 16 and 240 files with a short and a
/// long description (the numbers in the report).
#[test]
fn screenshot_scroll_measure() {
    if !crate::screenshot::enabled() || std::env::var_os("GITEZ_MEASURE").is_none() {
        return;
    }
    for (files, lines) in [(16, 120), (240, 160)] {
        for (name, description) in [("short", SHORT_DESCRIPTION), ("long", LONG_DESCRIPTION)] {
            let dir = tempfile::tempdir().expect("temp dir");
            let base = dir.path().canonicalize().expect("canonical");
            let (work_dir, number) =
                pull_request(&base, files, lines, description, State::Finished);
            let (mut shots, window, workspace) =
                open(&base, &work_dir, number, crate::screenshot::window_size());
            let review = review_of(&mut shots, &workspace);
            let rows = shots.update(|cx| review.read(cx).rows.len());
            let frame = Cell::new(0u32);
            let scroll = |window: &mut gpui_kit::Window, cx: &mut gpui_kit::App| {
                let at = frame.get();
                frame.set(at + 1);
                // Down for 200 frames (600 lines), then back up.
                let lines = if (at / 200).is_multiple_of(2) {
                    3.
                } else {
                    -3.
                };
                window.dispatch_event(
                    ScrollWheelEvent {
                        position: point(px(650.), px(600.)),
                        delta: ScrollDelta::Lines(point(0., -lines)),
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
            };
            // Warm up text shaping and layouts.
            shots.time_frames(window, 60, scroll);
            frame.set(0);
            let down = shots.time_frames(window, 200, scroll);
            let top = shots.update(|cx| review.read(cx).list.logical_scroll_top().item_ix);
            let up = shots.time_frames(window, 200, scroll);
            let idle = shots.time_frames(window, 120, |_, _| {});
            eprintln!(
                "{files} files ({rows} rows), {name} description: {:?} a frame scrolling \
                 (row {top} at the top after 600 lines down), {idle:?} a frame idle",
                (down + up) / 2
            );
            if description == LONG_DESCRIPTION {
                // The whole description in view, scrolled a line at a time:
                // laid out each frame, parsed only once.
                shots.update(|cx| {
                    review.update(cx, |review, cx| {
                        review.toggle_description(cx);
                        review.list.scroll_to(ListOffset {
                            item_ix: 0,
                            offset_in_item: px(0.),
                        });
                    })
                });
                frame.set(0);
                let nudge = |window: &mut gpui_kit::Window, cx: &mut gpui_kit::App| {
                    let at = frame.get();
                    frame.set(at + 1);
                    let lines = if (at / 20).is_multiple_of(2) { 1. } else { -1. };
                    window.dispatch_event(
                        ScrollWheelEvent {
                            position: point(px(650.), px(600.)),
                            delta: ScrollDelta::Lines(point(0., -lines)),
                            ..Default::default()
                        }
                        .to_platform_input(),
                        cx,
                    );
                };
                shots.time_frames(window, 40, nudge);
                let over = shots.time_frames(window, 200, nudge);
                eprintln!(
                    "{files} files, expanded description in view: {over:?} a frame scrolling"
                );
            }
        }
    }
}

/// Where the header is: its first and last control (the back button in
/// the breadcrumb, Merge at the end of the title row), which move with it.
fn header(shots: &mut Shots, window: AnyWindowHandle) -> (Bounds<Pixels>, Bounds<Pixels>) {
    (
        find(shots, window, "review-back").expect("the breadcrumb"),
        find(shots, window, "pull-request-merge").expect("the title row"),
    )
}

/// Where the element with id `id` is drawn now, if it is.
fn find(shots: &mut Shots, window: AnyWindowHandle, id: &'static str) -> Option<Bounds<Pixels>> {
    shots.update_window(window, |window, cx| {
        gpui_kit::test::TestWindowExt::render_frame(window, cx);
        gpui_kit::test::TestWindowExt::try_find(window, id).map(|element| element.bounds())
    })
}

/// Scrolls by `pixels` (positive: down) at `position`, as a trackpad does.
fn wheel(shots: &mut Shots, window: AnyWindowHandle, position: Point<Pixels>, pixels: f32) {
    shots.hover(window, position);
    shots.update_window(window, |window, cx| {
        window.dispatch_event(
            ScrollWheelEvent {
                position,
                delta: ScrollDelta::Pixels(point(px(0.), px(-pixels))),
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
    });
}

/// The diff list's rows, where it shows them and what is at its top.
struct Diff {
    rows: Vec<Row>,
    viewport: Bounds<Pixels>,
    top: ListOffset,
}

fn diff(shots: &mut Shots, window: AnyWindowHandle, review: &Entity<Review>) -> Diff {
    shots.update_window(window, |window, cx| {
        gpui_kit::test::TestWindowExt::render_frame(window, cx);
        let review = review.read(cx);
        Diff {
            rows: review.rows.clone(),
            viewport: review.list.viewport_bounds(),
            top: review.list.logical_scroll_top(),
        }
    })
}

/// Where row `index` of the diff is drawn, if it is.
fn row(shots: &mut Shots, review: &Entity<Review>, index: usize) -> Option<Bounds<Pixels>> {
    shots.update(|cx| review.read(cx).list.bounds_for_item(index))
}

/// Whether `inner` lies within `outer`.
fn inside(inner: Bounds<Pixels>, outer: Bounds<Pixels>) -> bool {
    inner.top() >= outer.top()
        && inner.bottom() <= outer.bottom()
        && inner.left() >= outer.left()
        && inner.right() <= outer.right()
}

/// The middle of the diff column, below its top by `down`.
fn in_diff(diff: &Diff, down: f32) -> Point<Pixels> {
    point(
        diff.viewport.left() + diff.viewport.size.width / 2.,
        diff.viewport.top() + px(down),
    )
}

/// The long-description pull request in a window of `size`, shot as
/// `scroll-<prefix>-…`: collapsed with the code in view, expanded,
/// scrolled past the description, and at the end. The header does not
/// move, and the description and the diff scroll as one.
fn shoot_long_description(
    prefix: &str,
    size: Size<Pixels>,
) -> (
    Shots,
    AnyWindowHandle,
    Entity<Workspace>,
    Entity<Review>,
    PathBuf,
) {
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let (work_dir, number) = pull_request(&base, 16, 120, LONG_DESCRIPTION, State::Finished);
    let (mut shots, window, workspace) = open(&base, &work_dir, number, size);
    let review = review_of(&mut shots, &workspace);
    // Kept for the caller's checks after the shots.
    std::mem::forget(dir);

    // Collapsed: the header, a few lines of the description, then code.
    shots.save(window, &format!("scroll-{prefix}-1-initial"));
    let header = header(&mut shots, window);
    let window_bounds = Bounds {
        origin: point(px(0.), px(0.)),
        size,
    };
    assert!(
        inside(header.0, window_bounds) && inside(header.1, window_bounds),
        "the header fits: {header:?}"
    );
    let shown = diff(&mut shots, window, &review);
    assert_eq!(shown.rows.first(), Some(&Row::Description));
    assert!(
        shown.viewport.top() > header.1.bottom(),
        "the diff is below the header: {:?} {header:?}",
        shown.viewport
    );
    let first_line = shown
        .rows
        .iter()
        .position(|row| matches!(row, Row::Line(..)))
        .expect("a diff line");
    let line = row(&mut shots, &review, first_line).expect("the first diff line is laid out");
    assert!(
        inside(line, shown.viewport) && line.bottom() <= size.height,
        "the first diff line shows in the window: {line:?} in {:?}",
        shown.viewport
    );
    let collapsed = row(&mut shots, &review, 0).expect("the description");
    assert!(
        find(&mut shots, window, "pull-request-description-toggle").is_some(),
        "a long description can be expanded"
    );

    // A wheel over the description scrolls the diff with it: no inner
    // scroll area catches it.
    wheel(&mut shots, window, in_diff(&shown, 40.), 30.);
    let scrolled = diff(&mut shots, window, &review);
    assert_eq!(
        (scrolled.top.item_ix, scrolled.top.offset_in_item),
        (0, px(30.)),
        "the description scrolls with the diff"
    );
    assert_eq!(self::header(&mut shots, window), header);
    wheel(&mut shots, window, in_diff(&shown, 40.), -30.);

    // Expanded: all of it, the code below it.
    shots.click_id(window, "pull-request-description-toggle");
    let expanded = row(&mut shots, &review, 0).expect("the description");
    assert!(
        expanded.size.height > collapsed.size.height * 3.,
        "expanded: {expanded:?}, collapsed: {collapsed:?}"
    );
    shots.save(window, &format!("scroll-{prefix}-2-expanded"));

    // Scrolled past it: the diff has all the room under the header.
    let past = f32::from(expanded.bottom() - shown.viewport.top()) + 1.;
    wheel(&mut shots, window, in_diff(&shown, 200.), past);
    let below = diff(&mut shots, window, &review);
    assert!(
        below.top.item_ix >= 1,
        "past the description: {:?}",
        below.top
    );
    assert_eq!(row(&mut shots, &review, 0), None, "the description is gone");
    assert_eq!(self::header(&mut shots, window), header);
    assert_eq!(below.viewport, shown.viewport, "the diff keeps its place");
    shots.save(window, &format!("scroll-{prefix}-3-past-description"));

    // The very end: the last row, still under the same header. Rows are
    // measured as they come into view, so the wheel goes on until the
    // list stops.
    let mut end = diff(&mut shots, window, &review);
    for _ in 0..500 {
        wheel(&mut shots, window, in_diff(&shown, 200.), 5_000.);
        let next = diff(&mut shots, window, &review);
        let stopped = (next.top.item_ix, next.top.offset_in_item)
            == (end.top.item_ix, end.top.offset_in_item);
        end = next;
        if stopped {
            break;
        }
    }
    let last = row(&mut shots, &review, end.rows.len() - 1).expect("the end shows");
    assert!(inside(last, end.viewport), "{last:?} in {:?}", end.viewport);
    assert_eq!(self::header(&mut shots, window), header);
    shots.save(window, &format!("scroll-{prefix}-4-end"));
    (shots, window, workspace, review, work_dir)
}

/// The owner's case at the window's opening size, then what still works
/// around it: the file list, Viewed, comments, expanding context, a new
/// commit on the branch, and Esc.
#[test]
fn screenshot_scroll_long_description() {
    if !crate::screenshot::enabled() {
        return;
    }
    let (mut shots, window, workspace, review, work_dir) =
        shoot_long_description("default", crate::screenshot::window_size());
    let path = |shots: &mut Shots, index: usize| {
        shots.update(|cx| match &review.read(cx).files {
            Some(Ok(files)) => files[index].path.clone(),
            _ => panic!("the diff is read"),
        })
    };
    let top_file = |shots: &mut Shots| {
        shots.update(|cx| {
            let review = review.read(cx);
            let top = review.list.logical_scroll_top();
            (review.rows[top.item_ix].file(), review.current_file)
        })
    };

    // A file of the list scrolls the diff to its header.
    let file_rows = shots.update(|cx| review.read(cx).file_rows.clone());
    shots.update(|cx| review.update(cx, |review, cx| review.scroll_to_file(5, cx)));
    let shown = diff(&mut shots, window, &review);
    assert_eq!(shown.top.item_ix, file_rows[5]);
    assert_eq!(top_file(&mut shots), (Some(5), 5));
    let header = row(&mut shots, &review, file_rows[5]).expect("its header");
    assert_eq!(header.top(), shown.viewport.top());

    // Viewed collapses the file and keeps the place.
    shots.update(|cx| review.update(cx, |review, cx| review.set_viewed(5, true, cx)));
    assert_eq!(top_file(&mut shots), (Some(5), 5));
    shots.update(|cx| review.update(cx, |review, cx| review.set_viewed(5, false, cx)));

    // Expanding all context keeps the file at the top.
    shots.click_id(window, "expand-all");
    let expanded_rows = diff(&mut shots, window, &review).rows.len();
    assert!(expanded_rows > shown.rows.len(), "context lines were added");
    assert_eq!(top_file(&mut shots).0, Some(5));

    // A new commit on the branch (an agent's run): the diff is read again
    // and stays where it was.
    let at = path(&mut shots, 5);
    let worktree = shots.update(|cx| {
        let review = review.read(cx);
        let Subject::PullRequest(subject) = &review.subject else {
            panic!("a pull request");
        };
        subject.pull_request.head.branch.clone()
    });
    let worktree_dir = output(&work_dir, &["worktree", "list", "--porcelain"])
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .find(|dir| output(dir, &["branch", "--show-current"]) == worktree)
        .expect("the thread's worktree");
    std::fs::write(worktree_dir.join(path_of(15)), source(15, 140, true)).expect("write");
    git(&worktree_dir, &["commit", "-qam", "Fix the pull, part 4"]);
    shots.update_window(window, |window, cx| {
        workspace.update(cx, |workspace, cx| workspace.reload(window, cx))
    });
    let top = top_file(&mut shots).0.expect("a file at the top");
    assert_eq!(path(&mut shots, top), at, "the same file stays at the top");

    // Back at the description, a reload keeps the place in it too.
    shots.update(|cx| {
        review.update(cx, |review, _| {
            review.list.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(40.),
            })
        })
    });
    std::fs::write(worktree_dir.join(path_of(14)), source(14, 140, true)).expect("write");
    git(&worktree_dir, &["commit", "-qam", "Fix the pull, part 5"]);
    shots.update_window(window, |window, cx| {
        workspace.update(cx, |workspace, cx| workspace.reload(window, cx))
    });
    let shown = diff(&mut shots, window, &review);
    assert_eq!((shown.top.item_ix, shown.top.offset_in_item), (0, px(40.)));

    // A comment in the agent prompt panel scrolls the diff to its thread.
    let comment = shots.update(|cx| {
        review.update(cx, |review, cx| {
            let Subject::PullRequest(subject) = &mut review.subject else {
                panic!("a pull request");
            };
            let pull_request = &mut subject.pull_request;
            let id = pull_request.add_thread(
                gitez_review::Anchor {
                    commit: pull_request.head.commit.clone(),
                    path: path_of(1),
                    side: gitez_review::Side::New,
                    lines: (6, 6),
                    excerpt: vec![
                        "    // Changed: line 5 of file 1 no longer waits for an editor.".into(),
                    ],
                },
                "Say why the editor waited.".into(),
                Timestamp::now(),
            );
            let anchor = review.scroll_anchor();
            review.rebuild_rows(anchor, cx);
            id
        })
    });
    shots.update(|cx| review.update(cx, |review, cx| review.scroll_to_thread(comment, cx)));
    let shown = diff(&mut shots, window, &review);
    let thread = shown
        .rows
        .iter()
        .position(|row| matches!(row, Row::Thread(_, id) if *id == comment))
        .expect("the thread's row");
    let bounds = row(&mut shots, &review, thread).expect("the thread shows");
    assert!(inside(bounds, shown.viewport), "{bounds:?}");
    shots.save(window, "scroll-default-5-comment");

    // Esc goes back to the history.
    shots.keys(window, "escape");
    assert!(shots.update(|cx| workspace.read(cx).review.is_none()));
}

/// The same in a window 700 points tall.
#[test]
fn screenshot_scroll_short_window() {
    if !crate::screenshot::enabled() {
        return;
    }
    shoot_long_description("700", size(px(1440.), px(700.)));
}

/// The same in the smallest window the app allows.
#[test]
fn screenshot_scroll_smallest_window() {
    if !crate::screenshot::enabled() {
        return;
    }
    shoot_long_description("min", size(px(960.), px(600.)));
}

/// A description of a line looks as before: all of it, nothing to expand.
#[test]
fn screenshot_scroll_short_description() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let (work_dir, number) = pull_request(&base, 16, 120, SHORT_DESCRIPTION, State::Finished);
    let (mut shots, window, workspace) =
        open(&base, &work_dir, number, crate::screenshot::window_size());
    let review = review_of(&mut shots, &workspace);
    shots.save(window, "scroll-short-description");
    assert_eq!(
        find(&mut shots, window, "pull-request-description-toggle"),
        None,
        "nothing to expand"
    );
    let shown = diff(&mut shots, window, &review);
    assert_eq!(shown.rows.first(), Some(&Row::Description));
}

/// The running thread's chip in the panel beside the diff, scrolled half
/// out of the panel: its blinking dot, painted by the live layer, is
/// clipped by the panel and never painted over the header. The window is
/// only 400 points tall, so the panel can scroll that far.
#[test]
fn screenshot_scroll_chip_clipped() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let (work_dir, number) = pull_request(&base, 16, 120, LONG_DESCRIPTION, State::Running);
    // Low enough for the panel to scroll the chip up to its top edge.
    let (mut shots, window, workspace) = open(&base, &work_dir, number, size(px(1440.), px(400.)));
    let (_, merge) = header(&mut shots, window);
    // The panel beside the diff starts where the diff does, 20 points to
    // its right, and reaches the window's right margin.
    let review = review_of(&mut shots, &workspace);
    let viewport = diff(&mut shots, window, &review).viewport;
    let panel = Bounds::from_corners(
        point(viewport.right() + px(20.), viewport.top()),
        point(px(1440. - 24.), viewport.bottom()),
    );
    let chip = |shots: &mut Shots| {
        shots.update_window(window, |window, cx| {
            gpui_kit::test::TestWindowExt::render_frame(window, cx);
            crate::motion::clipped_marks(window, cx)
                .into_iter()
                .find_map(|(mark, clip)| match mark {
                    crate::motion::Mark::Blink { center, radius, .. }
                        if center.x > panel.left() && center.y + radius >= panel.top() =>
                    {
                        Some((center, radius, clip))
                    }
                    _ => None,
                })
        })
    };
    let (center, _, clip) = chip(&mut shots).expect("the running chip's dot moves");
    assert_eq!(clip.top(), panel.top(), "clipped by the panel: {clip:?}");
    let to_top = f32::from(center.y - panel.top());
    wheel(
        &mut shots,
        window,
        point(panel.left() + px(100.), panel.top() + px(100.)),
        to_top,
    );
    let (center, radius, clip) = chip(&mut shots).expect("still painted, in part");
    assert!(
        (center.y - panel.top()).abs() <= radius,
        "the dot straddles the panel's top edge: {center:?}, panel {panel:?}"
    );
    assert!(
        clip.top() >= panel.top() && clip.top() > merge.bottom(),
        "clipped to the panel, below the header: {clip:?}, {panel:?}, {merge:?}"
    );
    let region = Bounds {
        origin: point(panel.left(), panel.top() - px(60.)),
        size: size(panel.size.width, px(160.)),
    };
    shots.save_region(window, region, "scroll-chip-clipped");
}
