//! The live marks of a window with agent threads at work (see
//! `crate::motion`): frames are asked for only while a mark moves in view,
//! the content is not laid out again for them, and an idle window asks for
//! none. `measure_live_lanes` times a frame on a large history:
//! `GITEZ_MEASURE=1 cargo test -p gitez measure_live_lanes -- --nocapture`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gitez_agent_thread::{
    Activity, AgentThread, Ending, IndexedRepository, NewAgentThread, Start, State, Store,
    Timestamp,
};
use gitez_sandbox::NetworkProfile;
use gitez_settings::{AgentEntry, Settings};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Entity, ScrollDelta, ScrollWheelEvent, TestAppContext,
    VisualTestContext, WindowBounds, WindowOptions, WindowVisibility, point, px, size,
};

use super::Workspace;
use crate::agent_threads::{AgentThreads, Options, init_with};
use crate::motion::{self, FRAME, Stats};

/// Runs `git` in `dir` with a fixed identity and no user configuration.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

/// A repository in `dir/repo` with `commits` commits on `main`, and every
/// 500 commits a branch of three that is merged back ten commits later, so
/// the graph has a few lanes. Written with `git fast-import`: 50,000
/// commits take about a second.
pub(crate) fn history(dir: &Path, commits: usize) -> PathBuf {
    let work_dir = dir.join("repo");
    std::fs::create_dir_all(&work_dir).expect("the repository folder");
    git(&work_dir, &["init", "-q", "-b", "main"]);
    let mut stream = String::new();
    let mut mark = 0usize;
    let mut time = 1_700_000_000u64;
    let mut commit = |stream: &mut String,
                      branch: &str,
                      from: Option<usize>,
                      merge: Option<usize>,
                      message: &str| {
        mark += 1;
        time += 60;
        stream.push_str(&format!(
            "commit refs/heads/{branch}\nmark :{mark}\ncommitter Ada Lovelace <ada@example.com> {time} +0000\ndata {}\n{message}\n",
            message.len()
        ));
        if let Some(from) = from {
            stream.push_str(&format!("from :{from}\n"));
        }
        if let Some(merge) = merge {
            stream.push_str(&format!("merge :{merge}\n"));
        }
        stream.push('\n');
        mark
    };
    let mut head = None;
    let mut open: Vec<(usize, usize)> = Vec::new();
    for index in 0..commits {
        if let Some(at) = open.iter().position(|(due, _)| *due == index) {
            let (_, side) = open.remove(at);
            head = Some(commit(
                &mut stream,
                "main",
                head,
                Some(side),
                "Merge a feature",
            ));
            continue;
        }
        if index > 0 && index % 500 == 0 {
            let branch = format!("feature/f{index}");
            let mut tip = head;
            for step in 0..3 {
                tip = Some(commit(
                    &mut stream,
                    &branch,
                    tip,
                    None,
                    &format!("Feature {index} step {step}"),
                ));
            }
            open.push((index + 10, tip.expect("three commits")));
        }
        head = Some(commit(
            &mut stream,
            "main",
            head,
            None,
            &format!("Change number {index} of the history"),
        ));
    }
    let mut import = Command::new("git")
        .args(["fast-import", "--quiet"])
        .current_dir(&work_dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::piped())
        .spawn()
        .expect("fast-import runs");
    import
        .stdin
        .take()
        .expect("stdin")
        .write_all(stream.as_bytes())
        .expect("the stream is written");
    assert!(import.wait().expect("fast-import ends").success());
    git(&work_dir, &["reset", "-q", "--hard", "main"]);
    git(&work_dir, &["config", "user.name", "Ada Lovelace"]);
    git(&work_dir, &["config", "user.email", "ada@example.com"]);
    work_dir
}

fn common_dir_of(work_dir: &Path) -> PathBuf {
    gitez_git::Repository::open(work_dir)
        .expect("open")
        .common_dir()
}

/// A thread of the repository at `work_dir`, in `state`, started from
/// `main`. Its launch is dated an hour ahead, so the model's liveness check
/// (no supervisor runs in tests) leaves it as it is.
pub(crate) fn live_thread(
    work_dir: &Path,
    data_dir: &Path,
    prompt: &str,
    state: State,
) -> AgentThread {
    let common_dir = common_dir_of(work_dir);
    let store = Store::new(&common_dir);
    let head = gitez_git::Repository::open(work_dir)
        .expect("open")
        .resolve_commit("refs/heads/main")
        .expect("resolve")
        .expect("main has a commit")
        .to_string();
    let thread = store
        .create(
            NewAgentThread {
                prompt: prompt.into(),
                agent: gitez_agent::ClaudeCode::ID.into(),
                model: None,
                effort: None,
                start: Start {
                    branch: "main".into(),
                    commit: head,
                },
                network: NetworkProfile::Packages,
                secrets: Vec::new(),
                mode: gitez_agent_thread::Mode::Implement,
                auto: false,
                work_dir: None,
                origin: None,
            },
            data_dir,
            "repo",
            |_| false,
            Timestamp::now(),
        )
        .expect("create");
    store
        .update(&thread.id, |thread| {
            thread.state = state;
            let ahead = Timestamp::now() + jiff::SignedDuration::from_hours(1);
            thread.turns[0].requested = ahead;
            if state == State::Running {
                thread.turns[0].started = Some(Timestamp::now());
                thread.activity = Some(Activity {
                    tool: Some("Bash".into()),
                    detail: "cargo test -p gitez-git blame".into(),
                    steps: 31,
                    updated: Timestamp::now(),
                });
            }
            Ok(thread.clone())
        })
        .expect("update")
}

/// Lists the repository at `work_dir` and saves settings that never run
/// the real `claude`.
pub(crate) fn index_and_settings(work_dir: &Path, data_dir: &Path) {
    gitez_agent_thread::Index::new(data_dir)
        .add(IndexedRepository {
            common_dir: common_dir_of(work_dir),
            work_dir: work_dir.to_owned(),
            name: "repo".into(),
        })
        .expect("index");
    Settings {
        agents: [(
            gitez_agent::ClaudeCode::ID.to_owned(),
            AgentEntry {
                program: Some("/nonexistent/claude".into()),
                ..AgentEntry::default()
            },
        )]
        .into(),
        ..Settings::default()
    }
    .save(&Settings::path(data_dir))
    .expect("settings saved");
}

/// The main window on the repository at `work_dir`, as the app opens it.
fn open(
    work_dir: &Path,
    data_dir: &Path,
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, VisualTestContext) {
    let data_dir = data_dir.to_owned();
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::theme::init(cx);
        crate::recent::init_in_memory(cx);
        init_with(
            Options {
                data_dir: Some(data_dir),
                sandbox: Arc::new(gitez_sandbox::Passthrough),
                agents: gitez_agent::Registry::builtin(),
                secrets: Arc::new(gitez_secret::Memory::default()),
                exe: Err("no supervisor in tests".into()),
                watch: false,
            },
            cx,
        );
        cx.bind_keys(super::key_bindings());
        cx.set_global(super::NoWatch);
    });
    cx.run_until_parked();
    let path = work_dir.to_owned();
    let (window, workspace): (AnyWindowHandle, Entity<Workspace>) = cx
        .update(|cx| {
            motion::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds {
                        origin: point(px(0.), px(0.)),
                        size: size(px(1440.), px(900.)),
                    })),
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| Workspace::new(Some(path), window, cx)),
            )
        })
        .expect("the window opens");
    let mut cx = VisualTestContext::from_window(window, cx);
    // The repository, its history and the threads load in the background.
    for _ in 0..20 {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.executor().advance_clock(Duration::from_millis(50));
    }
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    (workspace, cx)
}

/// One turn of the app's frame loop: time passes by a frame, what is due
/// runs, and the window is drawn (only what changed is rendered again).
/// Returns how many next-frame callbacks (`request_animation_frame`) were
/// waiting.
fn frame(cx: &mut VisualTestContext) -> usize {
    cx.executor().advance_clock(FRAME);
    cx.run_until_parked();
    cx.update(|window, cx| {
        let callbacks = window.simulate_next_frame(cx);
        window.draw(cx).clear(cx);
        callbacks
    })
}

fn stats(cx: &mut VisualTestContext) -> Stats {
    cx.update(|window, cx| motion::stats(window, cx).expect("the window has a live layer"))
}

/// Runs `frames` frames and returns what they changed in the counts, and
/// the next-frame callbacks requested.
fn run(cx: &mut VisualTestContext, frames: usize) -> (Stats, usize) {
    let before = stats(cx);
    let callbacks = (0..frames).map(|_| frame(cx)).sum();
    let after = stats(cx);
    (
        Stats {
            frames: after.frames - before.frames,
            content_paints: after.content_paints - before.content_paints,
            layer_paints: after.layer_paints - before.layer_paints,
        },
        callbacks,
    )
}

fn scroll(cx: &mut VisualTestContext, lines: f32) {
    cx.simulate_event(ScrollWheelEvent {
        position: point(px(600.), px(500.)),
        delta: ScrollDelta::Lines(point(0., -lines)),
        ..Default::default()
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
}

/// Two lanes move (a running thread's pulse and chip, a preparing one's
/// ring and chip, the toolbar's dot) at thirty frames a second, by painting
/// the layer alone: the history is not rendered or laid out again. Frames
/// stop when the lanes are scrolled away, the window is hidden, motion is
/// reduced or the threads finish, and an idle window asks for none.
#[gpui_kit::test]
fn live_marks_ask_for_frames_only_while_something_moves(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let work_dir = history(&base, 2_000);
    let data_dir = base.join("data");
    index_and_settings(&work_dir, &data_dir);
    let running = live_thread(&work_dir, &data_dir, "Add a blame view", State::Running);
    live_thread(
        &work_dir,
        &data_dir,
        "Speed up the status",
        State::Preparing,
    );
    let (_workspace, mut cx) = open(&work_dir, &data_dir, cx);

    let marks = cx.update(|window, cx| motion::marks(window, cx));
    let count = |kind: fn(&motion::Mark) -> bool| marks.iter().filter(|mark| kind(mark)).count();
    assert_eq!(
        count(|mark| matches!(mark, motion::Mark::Pulse { .. })),
        1,
        "{marks:?}"
    );
    assert_eq!(
        count(|mark| matches!(mark, motion::Mark::Spin { .. })),
        1,
        "{marks:?}"
    );
    // The two chips' dots and the toolbar's "2 active" dot.
    assert_eq!(
        count(|mark| matches!(mark, motion::Mark::Blink { .. })),
        3,
        "{marks:?}"
    );

    // Two seconds: a frame each 1/30 s, the content replayed from the cache.
    let (moving, callbacks) = run(&mut cx, 60);
    assert!(moving.frames >= 55, "about 30 frames a second: {moving:?}");
    assert!(moving.layer_paints >= 55, "{moving:?}");
    assert_eq!(
        moving.content_paints, 0,
        "the content is not rendered again: {moving:?}"
    );
    assert_eq!(callbacks, 0, "no view asks for animation frames itself");

    // Scrolled away, the lanes are not painted and do not register: only
    // the toolbar's dot is left to move.
    scroll(&mut cx, 400.);
    let marks = cx.update(|window, cx| motion::marks(window, cx));
    assert!(
        matches!(marks.as_slice(), [motion::Mark::Blink { center, .. }] if center.y < px(48.)),
        "only the toolbar's dot: {marks:?}"
    );
    scroll(&mut cx, -400.);
    let marks = cx.update(|window, cx| motion::marks(window, cx));
    assert_eq!(marks.len(), 5, "the lanes are back: {marks:?}");
    let (back, _) = run(&mut cx, 30);
    assert!(back.frames >= 25, "lanes back in view: {back:?}");

    // A hidden window asks for no frames, and starts again when shown.
    cx.simulate_visibility_change(WindowVisibility::Hidden);
    run(&mut cx, 2);
    let (hidden, _) = run(&mut cx, 30);
    assert_eq!(hidden.frames, 0, "hidden: {hidden:?}");
    cx.simulate_visibility_change(WindowVisibility::Visible);
    let (shown, _) = run(&mut cx, 30);
    assert!(shown.frames >= 25, "shown again: {shown:?}");

    // Reduced motion: the marks stand still and no frame is asked for.
    cx.update(|_, cx| cx.set_reduce_motion(true));
    run(&mut cx, 2);
    let (reduced, _) = run(&mut cx, 30);
    assert_eq!(reduced.frames, 0, "reduced motion: {reduced:?}");
    assert!(cx.update(|window, cx| motion::marks(window, cx)).is_empty());
    cx.update(|_, cx| cx.set_reduce_motion(false));
    let (again, _) = run(&mut cx, 30);
    assert!(again.frames >= 25, "motion again: {again:?}");

    // The threads finish: the lanes go and the window falls idle.
    let model = cx.update(|_, cx| AgentThreads::global(cx));
    let common_dir = common_dir_of(&work_dir);
    model.update(&mut cx, |model, cx| {
        let finished: Vec<Arc<AgentThread>> = model
            .repository(&common_dir)
            .expect("the repository")
            .threads
            .iter()
            .map(|thread| {
                let mut thread = (**thread).clone();
                thread.state = State::Finished;
                thread.ending = Some(Ending::Answer);
                Arc::new(thread)
            })
            .collect();
        assert!(finished.iter().any(|thread| thread.id == running.id));
        model.replace_threads(&common_dir, finished);
        cx.notify();
    });
    run(&mut cx, 3);
    assert!(cx.update(|window, cx| motion::marks(window, cx)).is_empty());
    let (idle, callbacks) = run(&mut cx, 300);
    assert_eq!(
        idle,
        Stats {
            frames: 0,
            content_paints: 0,
            layer_paints: idle.layer_paints,
        },
        "ten idle seconds: no frame asked for, nothing rendered again"
    );
    assert_eq!(callbacks, 0);
}

/// A window whose repository has no thread at work asks for no frame from
/// the start.
#[gpui_kit::test]
fn a_window_without_threads_at_work_stays_idle(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let work_dir = history(&base, 200);
    let data_dir = base.join("data");
    index_and_settings(&work_dir, &data_dir);
    live_thread(&work_dir, &data_dir, "Wait for a slot", State::Queued);
    let (_workspace, mut cx) = open(&work_dir, &data_dir, cx);
    assert!(cx.update(|window, cx| motion::marks(window, cx)).is_empty());
    let (idle, callbacks) = run(&mut cx, 300);
    assert_eq!(
        (idle.frames, idle.content_paints, callbacks),
        (0, 0, 0),
        "{idle:?}"
    );
}

/// Times one frame of a window on a history of 50,000 commits with two
/// live lanes: as an animation inside the history would draw it (the
/// history rendered again), as the live layer draws it, and the whole
/// window refreshed.
#[gpui_kit::test]
fn measure_live_lanes(cx: &mut TestAppContext) {
    if std::env::var_os("GITEZ_MEASURE").is_none() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let started = Instant::now();
    let work_dir = history(&base, 50_000);
    eprintln!("50,000 commits written in {:?}", started.elapsed());
    let data_dir = base.join("data");
    index_and_settings(&work_dir, &data_dir);
    live_thread(&work_dir, &data_dir, "Add a blame view", State::Running);
    live_thread(
        &work_dir,
        &data_dir,
        "Speed up the status",
        State::Preparing,
    );
    let (workspace, mut cx) = open(&work_dir, &data_dir, cx);
    let commits = workspace.read_with(&cx, |workspace, cx| {
        workspace.history.read(cx).commits().len()
    });
    eprintln!("history: {commits} commits");
    let history = workspace.read_with(&cx, |workspace, _| workspace.history.clone());

    let frames = 120u32;
    let time = |cx: &mut VisualTestContext, before: &dyn Fn(&mut VisualTestContext)| {
        let mut total = Duration::ZERO;
        for _ in 0..frames {
            before(cx);
            cx.executor().advance_clock(FRAME);
            cx.run_until_parked();
            total += cx.update(|window, cx| {
                let started = Instant::now();
                window.draw(cx).clear(cx);
                started.elapsed()
            });
        }
        total / frames
    };
    // Warm up caches (text shaping, layouts).
    time(&mut cx, &|_| {});
    let layer = time(&mut cx, &|_| {});
    let history_frame = time(&mut cx, &|cx| history.update(cx, |_, cx| cx.notify()));
    let refresh = time(&mut cx, &|cx| cx.update(|window, _| window.refresh()));
    eprintln!("a frame, live layer only:              {layer:?}");
    eprintln!("a frame, the history rendered again:   {history_frame:?}");
    eprintln!("a frame, the whole window refreshed:   {refresh:?}");
    let stats = stats(&mut cx);
    eprintln!("{stats:?}");
}
