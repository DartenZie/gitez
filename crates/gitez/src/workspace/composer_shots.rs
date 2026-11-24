//! Screenshots of the "Ask an agent" composer and the live lanes, and
//! end-to-end runs of the composer with a real supervisor. All of them
//! return at once unless `GITEZ_SCREENSHOTS` is set; the end-to-end runs
//! also need `GITEZ_E2E=1` (a fake agent) or `GITEZ_E2E_REAL=1` (the real
//! Claude Code with haiku, in a repository under `GITEZ_E2E_SCRATCH`), and
//! a built `target/debug/gitez` for the supervisor.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gitez_agent_thread::{
    Activity, AgentThread, Ending, IndexedRepository, Mode, NewAgentThread, Start, State, Store,
    Timestamp,
};
use gitez_sandbox::NetworkProfile;
use gitez_settings::{AgentEntry, SecretEntry, SecretScope, Settings};
use gpui_kit::{Bounds, Entity, point, px, size};

use crate::agent_threads::{AgentThreadKey, AgentThreads};
use crate::screenshot::{Shots, git};
use crate::workspace::Workspace;

/// Window coordinates: the history's first row, below the toolbar (48) and
/// the list's headings (46 + 28).
const FIRST_ROW: f32 = 48. + 74.;
const ROW: f32 = 36.;

/// The centre of history row `index` (0 is the top row), in the
/// description column.
fn row(index: usize) -> gpui_kit::Point<gpui_kit::Pixels> {
    point(px(600.), px(FIRST_ROW + ROW * index as f32 + ROW / 2.))
}

/// The fake `claude`: answers `auth status` as signed in unless
/// `FAKE_SIGNED_IN=false`, and acts on a keyword in the prompt: WORK
/// commits and waits for `release`, FAIL exits without a result, SLEEP
/// waits until interrupted; GitEZ's request for a brief gets one; anything
/// else is answered.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = auth ]; then printf '{"loggedIn": %s}\n' "${FAKE_SIGNED_IN:-true}"; exit 0; fi
log="$FAKE_LOG"
sid=""
prev=""
for arg in "$@"; do
  case "$prev" in --session-id|--resume) sid="$arg" ;; esac
  prev="$arg"
done
printf '%s\n' "$*" >> "$log/calls"
pwd -P > "$log/cwd"
prompt=$(cat)
say() { printf '%s\n' "$1"; }
init() { say '{"type":"system","subtype":"init","session_id":"'"$sid"'","model":"fake-model"}'; }
tool() { say '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"'"$1"'"}}]}}'; }
result() { say '{"type":"result","subtype":"success","is_error":false,"result":"'"$1"'","session_id":"'"$sid"'","num_turns":1,"duration_ms":5,"total_cost_usd":0.01}'; }
case "$prompt" in
  *WORK*)
    init; tool "git commit"
    echo first > agent.txt && git add agent.txt && git commit --quiet -m "Agent: first line" >&2
    tool "sleep until released"
    while [ ! -e "$log/release" ]; do sleep 0.1; done
    result "Wrote agent.txt and committed it." ;;
  *FAIL*)
    init; say "the fake agent fails" >&2; exit 1 ;;
  *SLEEP*)
    init; tool "sleep"
    trap 'say "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"\",\"session_id\":\"$sid\"}"; exit 0' INT
    while [ ! -e "$log/stop-never" ]; do sleep 0.1; done ;;
  *"Write a brief for an engineer"*)
    init; result '# Cover renames in the parser\n\nAdd a test with a file renamed twice.' ;;
  *)
    init; result "The answer is 42." ;;
esac
"#;

pub(crate) fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// Settings with Claude Code run as `program`, and two secrets.
pub(crate) fn save_settings(data_dir: &Path, common_dir: &Path, agent: AgentEntry) {
    let settings = Settings {
        agents: BTreeMap::from([(gitez_agent::ClaudeCode::ID.to_owned(), agent)]),
        secrets: vec![
            SecretEntry {
                name: "GITHUB_TOKEN".into(),
                scope: SecretScope::All,
                default: true,
            },
            SecretEntry {
                name: "SENTRY_DSN".into(),
                scope: SecretScope::Repository(common_dir.to_owned()),
                default: false,
            },
        ],
        ..Settings::default()
    };
    settings
        .save(&Settings::path(data_dir))
        .expect("settings saved");
}

/// The fake agent in `base/bin`, logging to `base/fake log`.
pub(crate) fn fake_agent(base: &Path, signed_in: bool) -> AgentEntry {
    let bin = base.join("bin");
    let log = base.join("fake log");
    std::fs::create_dir_all(&bin).expect("bin folder");
    std::fs::create_dir_all(&log).expect("log folder");
    let program = bin.join("claude");
    write_script(&program, FAKE_CLAUDE);
    AgentEntry {
        program: Some(program),
        config_dir: Some(base.join("claude config")),
        env: BTreeMap::from([
            ("FAKE_LOG".to_owned(), log.to_string_lossy().into_owned()),
            ("FAKE_SIGNED_IN".to_owned(), signed_in.to_string()),
        ]),
        ..AgentEntry::default()
    }
}

/// A repository in `dir` with a few commits and a second branch, the
/// sample's author, and nothing uncommitted.
pub(crate) fn repository(dir: &Path) -> PathBuf {
    let work_dir = dir.join("repo");
    crate::screenshot::sample_repository(&work_dir);
    std::fs::remove_file(work_dir.join("notes.txt")).expect("clean");
    git(&work_dir, &["config", "user.name", "Ada Lovelace"]);
    git(&work_dir, &["config", "user.email", "ada@example.com"]);
    git(&work_dir, &["config", "commit.gpgsign", "false"]);
    let hooks = dir.join("no hooks");
    std::fs::create_dir_all(&hooks).expect("hooks folder");
    git(
        &work_dir,
        &[
            "config",
            "core.hooksPath",
            hooks.to_str().expect("utf-8 path"),
        ],
    );
    work_dir
}

/// What `git <args>` prints in `dir`.
fn git_output(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

pub(crate) fn common_dir_of(work_dir: &Path) -> PathBuf {
    gitez_git::Repository::open(work_dir)
        .expect("open")
        .common_dir()
}

/// The composer, closed and open, and each of its pickers.
#[test]
fn screenshot_u2_composer() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let work_dir = repository(dir.path());
    // The uncommitted-changes row above the composer's.
    std::fs::write(work_dir.join("notes.txt"), "draft\n").expect("write");
    let data_dir = dir.path().join("data");
    save_settings(
        &data_dir,
        &common_dir_of(&work_dir),
        fake_agent(dir.path(), true),
    );
    let mut shots = Shots::from_env(&data_dir).expect("enabled");
    let (window, _) = shots.open_workspace(&work_dir);
    shots.save(window, "u2-composer-closed");
    shots.keys(window, "cmd-shift-a");
    shots.save(window, "u2-composer-open");
    shots.type_text(
        window,
        "Add a blame view to the commit detail. Reuse the diff list and keep it fast on big files.",
    );
    shots.save(window, "u2-composer-filled");
    for name in PICKERS {
        let place = picker(&mut shots, window, name);
        shots.click(window, place);
        shots.save(window, &format!("u2-picker-{name}"));
        shots.keys(window, "escape");
    }
    // A discussion: no From picker, and what the agent may do.
    shots.click_id(window, "composer-mode-discuss");
    shots.save(window, "u2-composer-discuss");
    // Auto: the same, and the agent decides; the middle segment is on.
    shots.click_id(window, "composer-mode-auto");
    shots.save(window, "u2-composer-auto");
    shots.click_id(window, "composer-mode-implement");
}

/// The composer with two pasted screenshots, at the window's opening size
/// and in a narrow one, and with a text-only prompt's notice.
#[test]
fn screenshot_composer_images() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let work_dir = repository(dir.path());
    let data_dir = dir.path().join("data");
    save_settings(
        &data_dir,
        &common_dir_of(&work_dir),
        fake_agent(dir.path(), true),
    );
    let mut shots = Shots::from_env(&data_dir).expect("enabled");
    for (name, size) in [
        ("composer-images", crate::screenshot::window_size()),
        ("composer-images-narrow", size(px(900.), px(700.))),
    ] {
        let (window, _) = shots.open_workspace_sized(&work_dir, size);
        shots.keys(window, "cmd-shift-a");
        shots.type_text(
            window,
            "The sidebar overlaps the graph at this width, see the screenshots.",
        );
        shots.paste_image(
            window,
            crate::screenshot::sample_image(1600, 1000, [95, 168, 245]),
        );
        shots.paste_image(
            window,
            crate::screenshot::sample_image(600, 1400, [232, 176, 75]),
        );
        shots.save(window, name);
        if name == "composer-images" {
            // The first thumbnail's tooltip.
            shots.hover(window, point(px(291.), px(300.)));
            shots.save(window, "composer-images-hover");
        }
    }
}

/// The pickers of the open composer, by the name their screenshots have.
const PICKERS: [&str; 3] = ["agent", "model", "from"];

/// Where picker `name` of the open composer is drawn.
pub(crate) fn picker(
    shots: &mut Shots,
    window: gpui_kit::AnyWindowHandle,
    name: &str,
) -> gpui_kit::Point<gpui_kit::Pixels> {
    let id = match name {
        "repository" => "composer-repository",
        "agent" => "composer-agent",
        "model" => "composer-model",
        "from" => "composer-from",
        _ => panic!("no picker {name}"),
    };
    shots
        .bounds_of(window, id)
        .unwrap_or_else(|| panic!("the {name} picker is shown"))
        .center()
}

/// Why Send is disabled: no sandbox, the agent missing, or signed out.
#[test]
fn screenshot_u2_composer_problems() {
    if !crate::screenshot::enabled() {
        return;
    }
    for (name, signed_in, missing_agent, missing_sandbox) in [
        ("u2-no-sandbox", true, false, true),
        ("u2-agent-missing", true, true, false),
        ("u2-signed-out", false, false, false),
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        let work_dir = repository(dir.path());
        let data_dir = dir.path().join("data");
        let mut agent = fake_agent(dir.path(), signed_in);
        if missing_agent {
            agent.program = Some(dir.path().join("nowhere").join("claude"));
        }
        save_settings(&data_dir, &common_dir_of(&work_dir), agent);
        let empty = dir.path().join("no sandbox");
        let mut shots = Shots::with_options(&data_dir, |options| {
            if missing_sandbox {
                options.sandbox = gitez_sandbox::platform(&empty);
            }
        })
        .expect("enabled");
        let (window, _) = shots.open_workspace(&work_dir);
        shots.keys(window, "cmd-shift-a");
        shots.type_text(window, "Fix the typo in the README");
        shots.save(window, name);
    }
}

/// Creates a thread in `work_dir` for `prompt` and changes it with
/// `change`.
pub(crate) fn thread(
    work_dir: &Path,
    data_dir: &Path,
    prompt: &str,
    change: impl FnOnce(&mut AgentThread),
) -> AgentThread {
    thread_in_mode(Mode::Implement, work_dir, data_dir, prompt, change)
}

/// [`thread`] with a thread of `mode`.
pub(crate) fn thread_in_mode(
    mode: Mode,
    work_dir: &Path,
    data_dir: &Path,
    prompt: &str,
    change: impl FnOnce(&mut AgentThread),
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
                network: NetworkProfile::AgentOnly,
                local_ports: false,
                secrets: Vec::new(),
                mode,
                auto: false,
                work_dir: (mode == Mode::Discuss).then(|| work_dir.to_owned()),
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
            change(thread);
            Ok(thread.clone())
        })
        .expect("update")
}

pub(crate) fn minutes_ago(minutes: i64) -> Timestamp {
    Timestamp::now() - jiff::SignedDuration::from_mins(minutes)
}

/// A running discussion's live lane beside an Implement thread's: at the
/// commit it started from, in a neutral colour, named "Discussion" where
/// the other has its branch.
#[test]
fn screenshot_lane_discussion() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let work_dir = repository(dir.path());
    let data_dir = dir.path().join("data");
    save_settings(
        &data_dir,
        &common_dir_of(&work_dir),
        fake_agent(dir.path(), true),
    );
    thread(&work_dir, &data_dir, "Speed up the status read", |thread| {
        thread.state = State::Running;
        thread.created = minutes_ago(6);
        thread.turns[0].started = Some(minutes_ago(6));
    });
    let discussion = thread_in_mode(
        Mode::Discuss,
        &work_dir,
        &data_dir,
        "How should blame follow a renamed file?",
        |thread| {
            thread.state = State::Running;
            thread.created = minutes_ago(2);
            thread.turns[0].started = Some(minutes_ago(2));
            thread.activity = Some(Activity {
                tool: Some("Read".into()),
                detail: "crates/gitez-git/src/blame.rs".into(),
                steps: 7,
                updated: Timestamp::now(),
            });
        },
    );
    assert_eq!(discussion.own(), None);
    gitez_agent_thread::Index::new(&data_dir)
        .add(IndexedRepository {
            common_dir: common_dir_of(&work_dir),
            work_dir: work_dir.clone(),
            name: "repo".into(),
        })
        .expect("index");
    let mut shots = Shots::from_env(&data_dir).expect("enabled");
    let (window, _) = shots.open_workspace(&work_dir);
    shots.save(window, "lane-discussion");
    shots.dispatch(
        window,
        crate::agent_threads::OpenAgentThread {
            common_dir: common_dir_of(&work_dir),
            id: discussion.id.clone(),
        },
    );
    shots.save(window, "lane-discussion-open");
}

/// Two live lanes, Preparing and Running, with a queued and a finished
/// thread, which have none.
#[test]
fn screenshot_u2_lanes() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let work_dir = repository(dir.path());
    let data_dir = dir.path().join("data");
    save_settings(
        &data_dir,
        &common_dir_of(&work_dir),
        fake_agent(dir.path(), true),
    );
    let running = thread(
        &work_dir,
        &data_dir,
        "Add a blame view to the commit detail",
        |thread| {
            thread.state = State::Running;
            thread.created = minutes_ago(14);
            thread.network = NetworkProfile::Packages;
            thread.turns[0].started = Some(minutes_ago(14));
            thread.activity = Some(Activity {
                tool: Some("Bash".into()),
                detail: "cargo test -p gitez-git blame".into(),
                steps: 31,
                updated: Timestamp::now(),
            });
        },
    );
    // The running thread's branch has two commits of its own.
    git(
        &work_dir,
        &[
            "checkout",
            "-qb",
            crate::agent_threads::thread_branch(&running),
        ],
    );
    for (file, message) in [
        ("blame.rs", "Read who last changed each line"),
        ("detail.rs", "Show blame in the commit detail"),
    ] {
        std::fs::write(work_dir.join(file), "fn blame() {}\n").expect("write");
        git(&work_dir, &["add", "."]);
        git(&work_dir, &["commit", "-qm", message]);
    }
    git(&work_dir, &["checkout", "-q", "main"]);
    thread(&work_dir, &data_dir, "Speed up the status read", |thread| {
        thread.state = State::Preparing;
        thread.created = minutes_ago(1);
    });
    thread(&work_dir, &data_dir, "Write the release notes", |thread| {
        thread.created = minutes_ago(0);
    });
    thread(&work_dir, &data_dir, "Explain the graph layout", |thread| {
        thread.state = State::Finished;
        thread.ending = Some(Ending::Answer);
        thread.created = minutes_ago(40);
    });
    gitez_agent_thread::Index::new(&data_dir)
        .add(IndexedRepository {
            common_dir: common_dir_of(&work_dir),
            work_dir: work_dir.clone(),
            name: "repo".into(),
        })
        .expect("index");
    let mut shots = Shots::from_env(&data_dir).expect("enabled");
    let (window, _) = shots.open_workspace(&work_dir);
    shots.save(window, "u2-lanes");
    // The running lane, selected: its thread opens in the detail panel.
    shots.click(window, row(1));
    shots.save(window, "u2-lanes-selected");
    shots.save_region(
        window,
        Bounds {
            origin: point(px(240.), px(48.)),
            size: size(px(840.), px(300.)),
        },
        "u2-lanes-detail",
    );
}

/// `target/debug/gitez`, built before the test, to run supervisors.
pub(crate) fn gitez_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // target/debug/deps/gitez-… → target/debug/gitez
    let binary = exe.parent()?.parent()?.join("gitez");
    binary.is_file().then_some(binary)
}

/// A script that runs the supervisor of `binary` with `data_dir` and the
/// sandbox `GITEZ_TEST_SANDBOX` names, if any.
pub(crate) fn supervisor_script(
    dir: &Path,
    binary: &Path,
    data_dir: &Path,
    passthrough: bool,
) -> PathBuf {
    let script = dir.join("supervise");
    let sandbox = if passthrough {
        "export GITEZ_TEST_SANDBOX=passthrough\n"
    } else {
        ""
    };
    write_script(
        &script,
        &format!(
            "#!/bin/sh\nexport GITEZ_DATA_DIR='{}'\n{sandbox}exec '{}' \"$@\"\n",
            data_dir.display(),
            binary.display(),
        ),
    );
    script
}

/// Settles the app and waits, in real time, until `done` holds for the
/// model's threads of `common_dir`, or `timeout` passes.
pub(crate) fn wait_until(
    shots: &mut Shots,
    common_dir: &Path,
    timeout: Duration,
    what: &str,
    done: impl Fn(&[Arc<AgentThread>]) -> bool,
) -> Vec<Arc<AgentThread>> {
    let deadline = Instant::now() + timeout;
    loop {
        shots.settle();
        let threads = shots.update(|cx| {
            AgentThreads::global(cx)
                .read(cx)
                .repository(common_dir)
                .map(|repository| repository.threads.clone())
                .unwrap_or_default()
        });
        if done(&threads) {
            return threads;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {threads:#?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn lanes_shown(shots: &mut Shots, workspace: &Entity<Workspace>) -> usize {
    shots.update(|cx| workspace.read(cx).history.read(cx).lanes().len())
}

/// The composer starts a thread with a real supervisor and the fake
/// agent: its lane shows while it runs and goes when it finishes with a
/// local pull request; another thread is stopped from its view.
#[test]
fn e2e_u2_composer_fake_agent() {
    if !crate::screenshot::enabled() || std::env::var_os("GITEZ_E2E").is_none() {
        return;
    }
    let binary = gitez_binary().expect("build target/debug/gitez first: cargo build -p gitez");
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let work_dir = repository(&base);
    // An uncommitted change, as the pickers' places assume; threads start
    // from the branch, without it.
    std::fs::write(work_dir.join("notes.txt"), "draft\n").expect("write");
    let common_dir = common_dir_of(&work_dir);
    let data_dir = base.join("data");
    let agent = fake_agent(&base, true);
    let log = base.join("fake log");
    save_settings(&data_dir, &common_dir, agent);
    let script = supervisor_script(&base, &binary, &data_dir, true);
    let mut shots = Shots::with_options(&data_dir, |options| {
        options.exe = Ok(script);
        options.watch = true;
    })
    .expect("enabled");
    let (window, workspace) = shots.open_workspace(&work_dir);

    let started = Instant::now();
    shots.keys(window, "cmd-shift-a");
    shots.type_text(window, "Write agent.txt and commit it. WORK");
    shots.save(window, "u2-e2e-1-typed");
    shots.keys(window, "cmd-enter");
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the thread to run",
        |threads| threads.iter().any(|thread| thread.state == State::Running),
    );
    let first = threads[0].clone();
    eprintln!("running after {:?}", started.elapsed());
    assert_eq!(lanes_shown(&mut shots, &workspace), 1);
    shots.save(window, "u2-e2e-2-running");
    // Its lane, under the composer's row, opens its thread.
    shots.click(window, row(2));
    let key = AgentThreadKey {
        common_dir: common_dir.clone(),
        id: first.id.clone(),
    };
    let shown = shots.update(|cx| {
        workspace
            .read(cx)
            .agent_thread
            .as_ref()
            .map(|(thread, _, _)| thread.read(cx).key().clone())
    });
    assert_eq!(shown.as_ref(), Some(&key));
    shots.save(window, "u2-e2e-3-lane-selected");

    std::fs::write(log.join("release"), "").expect("release the agent");
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the thread to finish",
        |threads| {
            threads
                .iter()
                .any(|thread| thread.id == first.id && thread.state == State::Finished)
        },
    );
    let finished = threads
        .iter()
        .find(|thread| thread.id == first.id)
        .expect("the thread");
    eprintln!(
        "finished after {:?}: {:?}, pull request {:?}",
        started.elapsed(),
        finished.ending,
        finished.pull_request
    );
    assert_eq!(finished.ending, Some(Ending::PullRequest));
    assert!(finished.pull_request.is_some());
    assert_eq!(lanes_shown(&mut shots, &workspace), 0);
    shots.save(window, "u2-e2e-4-finished");

    // A second thread, stopped while it sleeps.
    shots.keys(window, "cmd-shift-a");
    shots.type_text(window, "Wait a while. SLEEP");
    shots.keys(window, "cmd-enter");
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the second thread to run",
        |threads| {
            threads
                .iter()
                .any(|thread| thread.id != first.id && thread.state == State::Running)
        },
    );
    let second = threads
        .iter()
        .find(|thread| thread.id != first.id)
        .expect("the second thread")
        .clone();
    assert_eq!(lanes_shown(&mut shots, &workspace), 1);
    shots.save(window, "u2-e2e-5-second-running");
    let key = AgentThreadKey {
        common_dir: common_dir.clone(),
        id: second.id.clone(),
    };
    let stop = shots
        .update(|cx| AgentThreads::global(cx).update(cx, |threads, cx| threads.stop(&key, cx)));
    shots.settle();
    drop(stop);
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the second thread to stop",
        |threads| {
            threads
                .iter()
                .any(|thread| thread.id == second.id && !thread.state.is_active())
        },
    );
    let stopped = threads
        .iter()
        .find(|thread| thread.id == second.id)
        .expect("the second thread");
    eprintln!("stopped: {:?} {:?}", stopped.state, stopped.ending);
    assert_eq!(stopped.ending, Some(Ending::Stopped));
    assert_eq!(lanes_shown(&mut shots, &workspace), 0);
    shots.save(window, "u2-e2e-6-stopped");
}

/// A discussion from the composer to its handoff, with a fake agent and
/// the real supervisor: the thread runs in the working tree and makes no
/// branch, worktree or pull request; "Implement…" asks for a brief, and
/// when it arrives the composer opens with it; sent, it starts an
/// Implement thread that names the discussion.
#[test]
fn e2e_discuss_fake_agent() {
    if !crate::screenshot::enabled() || std::env::var_os("GITEZ_E2E").is_none() {
        return;
    }
    let binary = gitez_binary().expect("build target/debug/gitez first: cargo build -p gitez");
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let work_dir = repository(&base);
    // What the user has not committed: a discussion reads it as it is.
    std::fs::write(work_dir.join("notes.txt"), "draft\n").expect("write");
    let common_dir = common_dir_of(&work_dir);
    let data_dir = base.join("data");
    let log = base.join("fake log");
    save_settings(&data_dir, &common_dir, fake_agent(&base, true));
    let script = supervisor_script(&base, &binary, &data_dir, true);
    let mut shots = Shots::with_options(&data_dir, |options| {
        options.exe = Ok(script);
        options.watch = true;
    })
    .expect("enabled");
    let (window, workspace) = shots.open_workspace(&work_dir);
    let branches = git_output(&work_dir, &["branch", "--list"]);

    shots.keys(window, "cmd-shift-a");
    shots.click_id(window, "composer-mode-discuss");
    shots.type_text(window, "How does the parser read a rename?");
    shots.save(window, "discuss-e2e-1-typed");
    shots.keys(window, "cmd-enter");
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the discussion to answer",
        |threads| threads.iter().any(|thread| thread.state == State::Finished),
    );
    let discussion = threads[0].clone();
    assert_eq!(discussion.mode, Mode::Discuss);
    assert_eq!(discussion.ending, Some(Ending::Answer));
    assert_eq!(discussion.own(), None);
    assert_eq!(discussion.pull_request, None);
    assert_eq!(discussion.work_dir.as_deref(), Some(work_dir.as_path()));
    // The agent ran in the working tree itself.
    let calls = std::fs::read_to_string(log.join("cwd")).expect("the fake agent logs its folder");
    assert_eq!(Path::new(calls.trim()), work_dir);
    // No branch, no worktree, and the user's file is as it was.
    assert_eq!(git_output(&work_dir, &["branch", "--list"]), branches);
    let worktrees = git_output(&work_dir, &["worktree", "list", "--porcelain"]);
    assert_eq!(worktrees.matches("worktree ").count(), 1, "{worktrees}");
    assert_eq!(
        std::fs::read_to_string(work_dir.join("notes.txt")).expect("read"),
        "draft\n"
    );
    // The mode is remembered for the next thread.
    let last = shots.update(|cx| {
        AgentThreads::global(cx)
            .read(cx)
            .settings()
            .threads
            .last_mode
    });
    assert_eq!(last, Mode::Discuss);
    shots.save(window, "discuss-e2e-2-answered");

    // Implement…: a brief is asked for, and opens in the composer.
    shots.click_id(window, "thread-implement");
    wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the brief",
        |threads| threads.iter().any(|thread| thread.brief().is_some()),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !shots.has_id(window, "composer-origin-remove") {
        assert!(
            Instant::now() < deadline,
            "the composer opens with the brief"
        );
        std::thread::sleep(Duration::from_millis(100));
        shots.settle();
    }
    let composer = shots
        .update(|cx| workspace.read(cx).history.read(cx).composer().cloned())
        .expect("the composer");
    let (prompt, mode, origin) = shots.update(|cx| {
        let composer = composer.read(cx);
        (
            composer.prompt_text(cx),
            composer.thread_mode(cx),
            composer.origin().cloned(),
        )
    });
    assert_eq!(
        prompt,
        "# Cover renames in the parser\n\nAdd a test with a file renamed twice."
    );
    assert_eq!(mode, Mode::Implement);
    assert_eq!(origin.map(|origin| origin.id), Some(discussion.id.clone()));
    shots.save(window, "discuss-e2e-3-brief");

    // Sent: an Implement thread of its own, named by the brief's heading.
    shots.keys(window, "cmd-enter");
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the Implement thread to finish",
        |threads| {
            threads
                .iter()
                .any(|thread| thread.id != discussion.id && thread.state == State::Finished)
        },
    );
    let implement = threads
        .iter()
        .find(|thread| thread.id != discussion.id)
        .expect("the Implement thread");
    assert_eq!(implement.mode, Mode::Implement);
    assert_eq!(implement.origin.as_ref(), Some(&discussion.id));
    assert_eq!(implement.title, "Cover renames in the parser");
    assert_eq!(
        implement.branch.as_deref(),
        Some("agent/cover-renames-in-the-parser")
    );
    // A conversation of its own, not the discussion's.
    assert_ne!(implement.session_id, discussion.session_id);
    assert!(!implement.turns[0].resume);
    shots.save(window, "discuss-e2e-4-implementing");
    // The discussion lists it.
    shots.dispatch(
        window,
        crate::agent_threads::OpenAgentThread {
            common_dir: common_dir.clone(),
            id: discussion.id.clone(),
        },
    );
    let implementations = shots.update(|cx| {
        AgentThreads::global(cx)
            .read(cx)
            .implementations(&common_dir, &discussion.id)
            .len()
    });
    assert_eq!(implementations, 1);
    shots.save(window, "discuss-e2e-5-discussion");
}

/// One real run: Claude Code with haiku, picked in the composer's model
/// picker, in the real sandbox, in a throwaway repository under
/// `GITEZ_E2E_SCRATCH`; srt is copied from `GITEZ_E2E_SANDBOX`.
#[test]
fn e2e_u2_composer_real_haiku() {
    if !crate::screenshot::enabled() || std::env::var_os("GITEZ_E2E_REAL").is_none() {
        return;
    }
    let binary = gitez_binary().expect("build target/debug/gitez first: cargo build -p gitez");
    let scratch = PathBuf::from(std::env::var_os("GITEZ_E2E_SCRATCH").expect("a scratch folder"));
    let sandbox = PathBuf::from(std::env::var_os("GITEZ_E2E_SANDBOX").expect("an srt install"));
    let base = scratch.join(format!("u2-real-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("scratch folder");
    let work_dir = repository(&base);
    std::fs::write(work_dir.join("notes.txt"), "draft\n").expect("write");
    let common_dir = common_dir_of(&work_dir);
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
    let config_dir = std::env::var_os("GITEZ_E2E_CLAUDE_CONFIG").map(PathBuf::from);
    save_settings(
        &data_dir,
        &common_dir,
        AgentEntry {
            program: Some(claude),
            config_dir,
            ..AgentEntry::default()
        },
    );
    let script = supervisor_script(&base, &binary, &data_dir, false);
    let mut shots = Shots::with_options(&data_dir, |options| {
        options.sandbox = gitez_sandbox::platform(&data_dir);
        options.exe = Ok(script);
        options.watch = true;
    })
    .expect("enabled");
    let (window, workspace) = shots.open_workspace(&work_dir);
    shots.keys(window, "cmd-shift-a");
    shots.type_text(
        window,
        "Add a file hello.txt containing hi and commit it. Do nothing else.",
    );
    // haiku, as the owner allows, picked with the model picker.
    let model_picker = picker(&mut shots, window, "model");
    shots.click(window, model_picker);
    shots.save(window, "u2-real-1-model-picker");
    let haiku = std::env::var("U2_HAIKU_Y")
        .ok()
        .and_then(|y| y.parse().ok())
        .unwrap_or(478.);
    shots.click(window, point(model_picker.x + px(20.), px(haiku)));
    let model = shots.update(|cx| {
        workspace
            .read(cx)
            .history
            .read(cx)
            .composer()
            .map(|composer| composer.read(cx).model_alias(cx))
    });
    assert_eq!(model.as_deref(), Some("haiku"), "haiku is picked");
    shots.save(window, "u2-real-2-ready");
    let started = Instant::now();
    shots.keys(window, "cmd-enter");
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(60),
        "the thread to run",
        |threads| {
            threads
                .iter()
                .any(|thread| thread.state == State::Running || !thread.state.is_active())
        },
    );
    eprintln!("running after {:?}", started.elapsed());
    shots.save(window, "u2-real-3-running");
    let id = threads[0].id.clone();
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(300),
        "the thread to finish",
        |threads| {
            threads
                .iter()
                .any(|thread| thread.id == id && !thread.state.is_active())
        },
    );
    let done = threads
        .iter()
        .find(|thread| thread.id == id)
        .expect("the thread");
    eprintln!(
        "done after {:?}: {:?} {:?}, pull request {:?}, model {:?}",
        started.elapsed(),
        done.state,
        done.ending,
        done.pull_request,
        done.model
    );
    shots.dispatch(
        window,
        crate::agent_threads::OpenAgentThread {
            common_dir: common_dir.clone(),
            id: id.clone(),
        },
    );
    shots.save(window, "u2-real-4-done");
    assert_eq!(done.model.as_deref(), Some("haiku"));
    assert_eq!(done.ending, Some(Ending::PullRequest), "{done:#?}");
    // The local pull request it made opens in the window.
    let number = done.pull_request.expect("a pull request");
    crate::workspace::threads_real_shots::open_pull_request(
        &mut shots,
        window,
        &workspace,
        &common_dir,
        number,
    );
    shots.save(window, "u2-real-5-pull-request");
}
