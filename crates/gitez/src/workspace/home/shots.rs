//! Screenshots of Home, and runs that drive it: with no threads, with a
//! realistic set across three repositories, each kind of thread opened,
//! the launcher's pickers and problems; and, with a real supervisor and
//! the fake agent (`GITEZ_E2E=1`), starting threads from Home, replying,
//! opening their pull request, completing them, and threads that outlive
//! the app. All return at once unless `GITEZ_SCREENSHOTS` is set.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gitez_agent_thread::{
    Activity, AgentThread, AgentThreadId, Ending, IndexedRepository, State, Step, Timestamp,
    TurnResult,
};
use gitez_sandbox::NetworkProfile;
use gitez_settings::{AgentEntry, SecretEntry, SecretScope, Settings, ThreadSettings};
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity};

use super::Home;
use crate::agent_threads::{AgentThreadKey, AgentThreads};
use crate::screenshot::{Shots, git, window_size};
use crate::workspace::Workspace;
use crate::workspace::composer_shots::{
    fake_agent, gitez_binary, minutes_ago, picker, supervisor_script, thread, wait_until,
};

/// A repository called `name` in `base`: a few commits on `main`, two
/// branches, the sample's author, no hooks and nothing uncommitted.
fn named_repository(base: &Path, name: &str) -> PathBuf {
    let work_dir = base.join(name);
    crate::screenshot::sample_repository(&work_dir);
    std::fs::remove_file(work_dir.join("notes.txt")).expect("clean");
    git(&work_dir, &["config", "user.name", "Ada Lovelace"]);
    git(&work_dir, &["config", "user.email", "ada@example.com"]);
    git(&work_dir, &["config", "commit.gpgsign", "false"]);
    let hooks = base.join("no hooks");
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

fn common_dir_of(work_dir: &Path) -> PathBuf {
    crate::workspace::composer_shots::common_dir_of(work_dir)
}

/// Settings with Claude Code run as `agent`, a running limit of `limit`
/// and three secrets, one of them for `common_dir` only.
fn save_settings(data_dir: &Path, common_dir: &Path, agent: AgentEntry, limit: u32) {
    let settings = Settings {
        agents: [(gitez_agent::ClaudeCode::ID.to_owned(), agent)].into(),
        secrets: vec![
            SecretEntry {
                name: "GITHUB_TOKEN".into(),
                scope: SecretScope::All,
                default: true,
            },
            SecretEntry {
                name: "SENTRY_DSN".into(),
                scope: SecretScope::Repository(common_dir.to_owned()),
                default: true,
            },
            SecretEntry {
                name: "CRATES_IO_TOKEN".into(),
                scope: SecretScope::Repository(common_dir.to_owned()),
                default: false,
            },
        ],
        threads: ThreadSettings {
            max_running: limit,
            ..ThreadSettings::default()
        },
        ..Settings::default()
    };
    settings
        .save(&Settings::path(data_dir))
        .expect("settings saved");
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

/// Ran for `minutes` and ended `ago` minutes ago.
fn ran(thread: &mut AgentThread, minutes: i64, ago: i64) {
    thread.created = minutes_ago(ago + minutes + 1);
    thread.turns[0].started = Some(minutes_ago(ago + minutes));
    thread.turns[0].ended = Some(minutes_ago(ago));
}

/// Finished `ago` minutes ago after `minutes`, ending `ending`, saying
/// `message`.
fn finished(
    minutes: i64,
    ago: i64,
    ending: Ending,
    message: &'static str,
) -> impl FnOnce(&mut AgentThread) {
    move |thread| {
        ran(thread, minutes, ago);
        thread.state = State::Finished;
        thread.ending = Some(ending);
        thread.session_id = Some(format!("session-{}", thread.id.as_str()));
        thread.turns[0].result = Some(result(message, true, &[]));
        thread.network = NetworkProfile::Packages;
        thread.secrets = vec!["GITHUB_TOKEN".into()];
    }
}

/// Running for `minutes`, doing `tool` on `detail`.
fn running(minutes: i64, tool: &str, detail: &str) -> impl FnOnce(&mut AgentThread) {
    let (tool, detail) = (tool.to_owned(), detail.to_owned());
    move |thread| {
        thread.state = State::Running;
        thread.created = minutes_ago(minutes);
        thread.network = NetworkProfile::Packages;
        thread.session_id = Some("session-running".into());
        thread.turns[0].started = Some(minutes_ago(minutes));
        thread.activity = Some(Activity {
            tool: Some(tool),
            detail,
            steps: 31,
            updated: Timestamp::now(),
        });
    }
}

/// A commit on `branch`, made from `main`; returns its tip.
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

/// Gives `thread` a local pull request of one commit on its branch.
fn with_pull_request(work_dir: &Path, thread: &AgentThread) -> u32 {
    let common_dir = common_dir_of(work_dir);
    let head = commit_on(
        work_dir,
        crate::agent_threads::thread_branch(thread),
        &format!("{}.rs", thread.id.as_str()),
        &thread.title,
    );
    let number = gitez_review::Store::new(&common_dir)
        .create(
            gitez_review::NewPullRequest {
                title: thread.title.clone(),
                description: thread.final_message().unwrap_or_default().to_owned(),
                base: gitez_review::Tip {
                    branch: "main".into(),
                    commit: thread.start.commit.clone(),
                },
                head: gitez_review::Tip {
                    branch: crate::agent_threads::thread_branch(thread).to_owned(),
                    commit: head,
                },
            },
            Timestamp::now(),
        )
        .expect("pull request")
        .number;
    gitez_agent_thread::Store::new(&common_dir)
        .update(&thread.id, |thread| {
            thread.pull_request = Some(number);
            Ok(())
        })
        .expect("link the pull request");
    number
}

/// Three repositories with a realistic set of threads, and the data
/// folder listing them.
struct World {
    data_dir: PathBuf,
    gitez: PathBuf,
    notes: PathBuf,
    dotfiles: PathBuf,
    /// gitez's threads: failed, pull request, answer, running.
    failed: AgentThreadId,
    pull_request: AgentThreadId,
    pull_request_number: u32,
    answer: AgentThreadId,
    running: AgentThreadId,
}

fn world(base: &Path) -> World {
    let data_dir = base.join("data");
    let gitez = named_repository(base, "gitez");
    let notes = named_repository(base, "notes-site");
    let dotfiles = named_repository(base, "dotfiles");
    save_settings(&data_dir, &common_dir_of(&gitez), fake_agent(base, true), 4);

    let failed = thread(
        &gitez,
        &data_dir,
        "Fix the flaky watcher test on Linux",
        |thread| {
            ran(thread, 6, 40);
            thread.fail(
                Step::Agent,
                "The agent could not download crates: static.crates.io was blocked.",
                minutes_ago(40),
            );
            thread.turns[0].result = Some(result("", false, &["static.crates.io"]));
            thread.session_id = Some("session-failed".into());
        },
    );
    let windows = thread(
        &gitez,
        &data_dir,
        "Windows path handling in gitez-command",
        finished(
            26,
            62,
            Ending::PullRequest,
            "Done, in one commit. `path.rs` is now the one place that turns a path into an \
             argument for git, and it accepts verbatim and UNC paths.",
        ),
    );
    let pull_request_number = with_pull_request(&gitez, &windows);
    let answer = thread(
        &gitez,
        &data_dir,
        "Explain how lane assignment is cached",
        finished(
            3,
            26 * 60,
            Ending::Answer,
            "Lanes are assigned by `LaneAllocator`. The cache has no key: every refresh of the \
             graph clears it and lays out the whole history again.",
        ),
    );
    thread(&gitez, &data_dir, "Rename the sidebar sections", |thread| {
        ran(thread, 4, 3 * 60);
        thread.state = State::Finished;
        thread.ending = Some(Ending::Stopped);
        thread.session_id = Some("session-stopped".into());
    });
    let running_thread = thread(
        &gitez,
        &data_dir,
        "Add a blame view to the commit detail",
        running(14, "Bash", "cargo test -p gitez-git blame"),
    );
    thread(
        &gitez,
        &data_dir,
        "Speed up graph layout for 50k commits",
        running(41, "Edit", "crates/gitez/src/workspace/graph.rs"),
    );
    thread(
        &gitez,
        &data_dir,
        "Keyboard navigation in the conflict editor",
        |thread| {
            thread.state = State::Preparing;
            thread.created = minutes_ago(0);
            thread.activity = Some(Activity {
                tool: Some("Setup".into()),
                detail: "creating worktree, cargo fetch".into(),
                steps: 0,
                updated: Timestamp::now(),
            });
        },
    );
    thread(
        &gitez,
        &data_dir,
        "Document the review storage format",
        |thread| thread.created = minutes_ago(1),
    );
    index(&data_dir, &gitez, "gitez");

    thread(
        &notes,
        &data_dir,
        "Migrate the build to Vite 7",
        running(8, "Bash", "pnpm install"),
    );
    thread(
        &notes,
        &data_dir,
        "Update the RSS feed template",
        |thread| {
            thread.state = State::Waiting;
            thread.created = minutes_ago(30);
            thread.waiting_until = Some(Timestamp::now() + jiff::SignedDuration::from_mins(50));
            thread.session_id = Some("session-waiting".into());
        },
    );
    thread(
        &notes,
        &data_dir,
        "Add a dark-mode toggle to the header",
        |thread| {
            ran(thread, 12, 2 * 24 * 60);
            thread.state = State::Completed;
            thread.ending = Some(Ending::PullRequest);
            thread.pull_request = Some(4);
            thread.completed = Some(minutes_ago(2 * 24 * 60));
        },
    );
    index(&data_dir, &notes, "notes-site");

    let zsh = thread(
        &dotfiles,
        &data_dir,
        "Deduplicate zsh aliases",
        finished(
            7,
            3 * 24 * 60,
            Ending::PullRequest,
            "Removed eleven aliases defined twice.",
        ),
    );
    with_pull_request(&dotfiles, &zsh);
    thread(
        &dotfiles,
        &data_dir,
        "Why does the nightly backup script exit 1?",
        finished(
            2,
            8 * 24 * 60,
            Ending::Answer,
            "`rsync` exits 24 when files vanish during the copy; the script treats it as a failure.",
        ),
    );
    thread(&dotfiles, &data_dir, "Explain the git aliases", |thread| {
        ran(thread, 1, 5 * 24 * 60);
        thread.state = State::Completed;
        thread.ending = Some(Ending::Answer);
        thread.completed = Some(minutes_ago(5 * 24 * 60));
    });
    index(&data_dir, &dotfiles, "dotfiles");

    World {
        data_dir,
        gitez,
        notes,
        dotfiles,
        failed: failed.id,
        pull_request: windows.id,
        pull_request_number,
        answer: answer.id,
        running: running_thread.id,
    }
}

/// The window with no repository: Home. `recent` are listed as recent
/// repositories, the first most recent.
fn open_home(shots: &mut Shots, recent: &[&Path]) -> (AnyWindowHandle, Entity<Workspace>) {
    let recent: Vec<PathBuf> = recent.iter().map(|path| path.to_path_buf()).collect();
    shots.update(|cx| {
        for path in recent.iter().rev() {
            crate::recent::opened(path, cx);
        }
        crate::recent::closed(cx);
    });
    shots.open(window_size(), |window, cx| {
        cx.new(|cx| Workspace::new(None, window, cx))
    })
}

fn home_of(shots: &mut Shots, workspace: &Entity<Workspace>) -> Entity<Home> {
    shots.update(|cx| {
        workspace
            .read(cx)
            .home
            .as_ref()
            .map(|(home, _)| home.clone())
            .expect("Home is made")
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

/// The ids in Home's lists: finished, active, waiting.
fn listed(shots: &mut Shots) -> [Vec<AgentThreadId>; 3] {
    shots.update(|cx| {
        let listed = super::list(AgentThreads::global(cx).read(cx), None);
        [listed.finished, listed.active, listed.waiting].map(|items| {
            items
                .into_iter()
                .map(|item| item.thread.id.clone())
                .collect()
        })
    })
}

fn key(work_dir: &Path, id: &AgentThreadId) -> AgentThreadKey {
    AgentThreadKey {
        common_dir: common_dir_of(work_dir),
        id: id.clone(),
    }
}

/// Window coordinates of what the tests click, read off `u6-home`.
mod at {
    use gpui_kit::{Pixels, Point, point, px};

    fn env(name: &str, x: f32, y: f32) -> Point<Pixels> {
        let (x, y) = std::env::var(name)
            .ok()
            .and_then(|value| {
                let (x, y) = value.split_once(':')?;
                Some((x.parse().ok()?, y.parse().ok()?))
            })
            .unwrap_or((x, y));
        point(px(x), px(y))
    }

    /// The button at the right of finished row `index`'s second line: PR
    /// or Complete.
    pub fn row_button(index: usize) -> Point<Pixels> {
        let title = finished_row(index);
        point(px(240.), title.y + px(24.))
    }

    /// Entry `index` of the launcher's repository picker, which is at
    /// `picker`.
    pub fn repository_item(picker: Point<Pixels>, index: usize) -> Point<Pixels> {
        point(picker.x + px(79.), picker.y + px(60. + 41.5 * index as f32))
    }

    /// Haiku in the launcher's model picker, which is at `picker`.
    pub fn haiku(picker: Point<Pixels>) -> Point<Pixels> {
        point(picker.x - px(48.), picker.y + px(154.))
    }

    /// The reply box of a finished thread shown in Home.
    pub fn reply() -> Point<Pixels> {
        env("U6_REPLY", 700., 809.)
    }

    /// Home in a repository's toolbar.
    pub fn home() -> Point<Pixels> {
        point(px(26.), px(24.))
    }

    pub fn new_thread() -> Point<Pixels> {
        env("U6_NEW_THREAD", 150., 73.)
    }

    pub fn filter() -> Point<Pixels> {
        env("U6_FILTER", 180., 109.)
    }

    /// Entry `index` of the filter's menu: 0 is All repositories, then
    /// the repositories as indexed.
    pub fn filter_item(index: usize) -> Point<Pixels> {
        let y = match index {
            0 => 141.,
            n => 174. + 27.5 * (n - 1) as f32,
        };
        point(px(120.), px(y))
    }

    /// Recent repository `index` in the launcher's page, under the error
    /// banner.
    pub fn recent(index: usize) -> Point<Pixels> {
        let y: f32 = std::env::var("U6_RECENT_Y")
            .ok()
            .and_then(|y| y.parse().ok())
            .unwrap_or(560.);
        point(px(700.), px(y + 55. * index as f32))
    }

    /// The finished row `index` (0 first): its title.
    /// In [`super::world`]'s list: rows with Retry are taller.
    pub fn finished_row(index: usize) -> Point<Pixels> {
        let y = [181., 270., 331., 418., 480., 540.][index];
        point(px(120.), px(y))
    }
}

/// Recent repositories with a favicon show it on Home and in the launcher's
/// Repository picker; the others keep the square with their initial.
#[test]
fn screenshot_u6_repository_icons() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let site = named_repository(&base, "notes-site");
    std::fs::create_dir_all(site.join("public")).expect("public folder");
    std::fs::write(
        site.join("public/favicon.svg"),
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="7" fill="#4fc6b0"/><circle cx="16" cy="16" r="8" fill="#121214"/></svg>"##,
    )
    .expect("favicon");
    git(&site, &["add", "public/favicon.svg"]);
    git(&site, &["commit", "-qm", "Add a favicon"]);
    let plain = named_repository(&base, "dotfiles");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&data_dir).expect("data folder");
    save_settings(&data_dir, &common_dir_of(&site), fake_agent(&base, true), 3);
    let mut shots = Shots::from_env(&data_dir).expect("enabled");
    let (window, _) = open_home(&mut shots, &[&site, &plain]);
    let found = shots.update(|cx| crate::project_icon::of(&site, cx).is_some());
    assert!(found, "the favicon is found");
    shots.save(window, "u6-icons-home");
    let repository = picker(&mut shots, window, "repository");
    shots.click(window, repository);
    shots.save(window, "u6-icons-picker");
}

/// Finding 9: a repository listed with threads that was moved or deleted
/// shows in the launcher's Repository picker as unavailable, and Home
/// says so with "Remove from list".
#[test]
fn screenshot_u6_moved_repository() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let here = named_repository(&base, "notes-site");
    let gone = base.join("old-api");
    let data_dir = base.join("data");
    std::fs::create_dir_all(&data_dir).expect("data folder");
    save_settings(&data_dir, &common_dir_of(&here), fake_agent(&base, true), 3);
    index(&data_dir, &here, "notes-site");
    gitez_agent_thread::Index::new(&data_dir)
        .add(IndexedRepository {
            common_dir: gone.join(".git"),
            work_dir: gone.clone(),
            name: "old-api".into(),
        })
        .expect("index");
    let mut shots = Shots::from_env(&data_dir).expect("enabled");
    let (window, _) = open_home(&mut shots, &[&gone, &here]);
    shots.save(window, "u6-moved-home");
    let repository = picker(&mut shots, window, "repository");
    shots.click(window, repository);
    shots.save(window, "u6-moved-picker");
}

/// Home with no threads and no recent repositories; then with three
/// repositories' threads.
#[test]
fn screenshot_u6_home() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data folder");
    save_settings(
        &data_dir,
        Path::new("/nowhere/.git"),
        fake_agent(dir.path(), true),
        3,
    );
    let mut shots = Shots::from_env(&data_dir).expect("enabled");
    let (window, _) = open_home(&mut shots, &[]);
    shots.save(window, "u6-empty");

    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let world = world(&base);
    let mut shots = Shots::from_env(&world.data_dir).expect("enabled");
    let (window, workspace) = open_home(&mut shots, &[&world.gitez, &world.notes, &world.dotfiles]);
    shots.save(window, "u6-home");
    let home = home_of(&mut shots, &workspace);
    // The pickers of the launcher, each opened once.
    for name in ["repository", "agent", "model", "from", "sandbox", "secrets"] {
        let place = picker(&mut shots, window, name);
        shots.click(window, place);
        shots.save(window, &format!("u6-picker-{name}"));
        shots.keys(window, "escape");
    }
    // Active and Waiting, unfolded at the bottom of the sidebar.
    shots.click_id(window, "home-active-toggle");
    shots.save(window, "u6-home-active");
    shots.click_id(window, "home-active-toggle");

    // The repository filter: notes-site has no finished thread.
    shots.click(window, at::filter());
    shots.save(window, "u6-filter-menu");
    shots.click(window, at::filter_item(2));
    let filter = shots.update(|cx| home.read(cx).filter.clone());
    assert_eq!(
        filter,
        Some(common_dir_of(&world.notes)),
        "the filter narrows"
    );
    shots.save(window, "u6-filtered");
    shots.click(window, at::filter());
    shots.click(window, at::filter_item(3));
    shots.save(window, "u6-filtered-dotfiles");
    shots.click(window, at::filter());
    shots.click(window, at::filter_item(0));
    let filter = shots.update(|cx| home.read(cx).filter.clone());
    assert_eq!(filter, None);

    // Each kind of finished thread, opened from its row.
    for (row, id, name) in [
        (0, &world.failed, "u6-thread-failed"),
        (1, &world.pull_request, "u6-thread-pull-request"),
        (3, &world.answer, "u6-thread-answer"),
    ] {
        shots.click(window, at::finished_row(row));
        let shown = shots.update(|cx| home.read(cx).shown_thread(cx));
        assert_eq!(shown, Some(key(&world.gitez, id)), "{name}");
        shots.save(window, name);
    }
    // New thread goes back to the launcher.
    shots.click(window, at::new_thread());
    let shown = shots.update(|cx| home.read(cx).shown_thread(cx));
    assert_eq!(shown, None, "New thread shows the launcher");
    // A running thread, as the Agents popover opens it.
    shots.dispatch(
        window,
        crate::agent_threads::OpenAgentThread {
            common_dir: common_dir_of(&world.gitez),
            id: world.running.clone(),
        },
    );
    let shown = shots.update(|cx| home.read(cx).shown_thread(cx));
    assert_eq!(shown, Some(key(&world.gitez, &world.running)));
    let open = shots.update(|cx| workspace.read(cx).snapshot.is_some());
    assert!(!open, "Home shows the thread itself");
    shots.save(window, "u6-thread-running");

    // A row's PR button opens the repository on its pull request.
    shots.click(window, at::row_button(1));
    assert_eq!(
        shown_pull_request(&mut shots, &workspace),
        Some(world.pull_request_number),
        "PR #n opens the pull request"
    );
    shots.save(window, "u6-pull-request-opened");
}

/// The launcher when the sandbox or the agent is not ready, and Home
/// after a repository failed to open.
#[test]
fn screenshot_u6_problems() {
    if !crate::screenshot::enabled() {
        return;
    }
    for (name, missing_agent, missing_sandbox) in [
        ("u6-no-sandbox", false, true),
        ("u6-agent-missing", true, false),
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        let base = dir.path().canonicalize().expect("canonical");
        let work_dir = named_repository(&base, "gitez");
        let data_dir = base.join("data");
        let mut agent = fake_agent(&base, true);
        if missing_agent {
            agent.program = Some(base.join("nowhere").join("claude"));
        }
        save_settings(&data_dir, &common_dir_of(&work_dir), agent, 3);
        let empty = base.join("no sandbox");
        let mut shots = Shots::with_options(&data_dir, |options| {
            if missing_sandbox {
                options.sandbox = gitez_sandbox::platform(&empty);
            }
        })
        .expect("enabled");
        let (window, _) = open_home(&mut shots, &[&work_dir]);
        shots.type_text(window, "Fix the typo in the README");
        shots.save(window, name);
    }

    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let work_dir = named_repository(&base, "gitez");
    let data_dir = base.join("data");
    save_settings(
        &data_dir,
        &common_dir_of(&work_dir),
        fake_agent(&base, true),
        3,
    );
    let mut shots = Shots::from_env(&data_dir).expect("enabled");
    let gone = base.join("moved-away");
    shots.update(|cx| crate::recent::opened(&work_dir, cx));
    let (window, workspace) = shots.open(window_size(), |window, cx| {
        cx.new(|cx| Workspace::new(Some(gone), window, cx))
    });
    let home = home_of(&mut shots, &workspace);
    let error = shots.update(|cx| home.read(cx).error.clone());
    assert!(error.is_some(), "the failed open shows in Home");
    shots.save(window, "u6-open-error");
    // Opening the recent repository from Home hides the message.
    shots.click(window, at::recent(0));
    let open = shots.update(|cx| workspace.read(cx).snapshot.is_some());
    assert!(open, "a recent repository opens with one click");
    let error = shots.update(|cx| home.read(cx).error.clone());
    assert!(error.is_none());
}

/// A real supervisor and the fake agent, all from Home: a thread started
/// in the repository picked in the launcher shows under Active, then
/// Finished; opened, it takes a reply; its PR button opens the pull
/// request, and Home afterwards is as it was; an answer is completed from
/// its row.
#[test]
fn e2e_u6_home_fake_agent() {
    if !crate::screenshot::enabled() || std::env::var_os("GITEZ_E2E").is_none() {
        return;
    }
    let binary = gitez_binary().expect("build target/debug/gitez first: cargo build -p gitez");
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let gitez = named_repository(&base, "gitez");
    let notes = named_repository(&base, "notes-site");
    let common_dir = common_dir_of(&notes);
    let data_dir = base.join("data");
    save_settings(&data_dir, &common_dir, fake_agent(&base, true), 3);
    let log = base.join("fake log");
    let script = supervisor_script(&base, &binary, &data_dir, true);
    let mut shots = Shots::with_options(&data_dir, |options| {
        options.exe = Ok(script);
        options.watch = true;
    })
    .expect("enabled");
    let (window, workspace) = open_home(&mut shots, &[&gitez, &notes]);
    let home = home_of(&mut shots, &workspace);

    // notes-site, picked in the launcher; the prompt; ⌘↵.
    let repository = picker(&mut shots, window, "repository");
    shots.click(window, repository);
    shots.click(window, at::repository_item(repository, 1));
    let picked = shots.update(|cx| {
        home.read(cx)
            .composer
            .read(cx)
            .picked_repository()
            .map(Path::to_owned)
    });
    assert_eq!(picked, Some(notes.clone()), "notes-site is picked");
    shots.type_text(window, "Write agent.txt and commit it. WORK");
    shots.save(window, "u6-e2e-1-typed");
    let started = Instant::now();
    shots.keys(window, "cmd-enter");
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the thread to run",
        |threads| threads.iter().any(|thread| thread.state == State::Running),
    );
    let work = threads[0].id.clone();
    eprintln!("running after {:?}", started.elapsed());
    let [finished, active, ..] = listed(&mut shots);
    assert_eq!(active, std::slice::from_ref(&work), "listed under Active");
    assert!(finished.is_empty());
    shots.save(window, "u6-e2e-2-running");

    std::fs::write(log.join("release"), "").expect("release the agent");
    wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the thread to finish",
        |threads| threads.iter().any(|thread| thread.state == State::Finished),
    );
    eprintln!("finished after {:?}", started.elapsed());
    let [finished, active, ..] = listed(&mut shots);
    assert_eq!(finished, std::slice::from_ref(&work), "moved to Finished");
    assert!(active.is_empty());
    shots.save(window, "u6-e2e-3-finished");

    // Opened from its row, it takes a reply.
    shots.click(window, at::finished_row(0));
    let shown = shots.update(|cx| home.read(cx).shown_thread(cx));
    assert_eq!(shown, Some(key(&notes, &work)), "the row opens the thread");
    shots.save(window, "u6-e2e-4-opened");
    shots.click(window, at::reply());
    shots.type_text(window, "What did you change?");
    shots.keys(window, "cmd-enter");
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the reply to run",
        |threads| threads.iter().any(|thread| thread.turns.len() == 2),
    );
    eprintln!("reply: {:?}", threads[0].state);
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the reply to finish",
        |threads| {
            threads
                .iter()
                .any(|thread| thread.turns.len() == 2 && thread.state == State::Finished)
        },
    );
    let replied = threads[0].clone();
    assert_eq!(replied.ending, Some(Ending::PullRequest));
    let number = replied.pull_request.expect("a pull request");
    let [finished, ..] = listed(&mut shots);
    assert_eq!(finished, std::slice::from_ref(&work), "back under Finished");
    shots.save(window, "u6-e2e-5-replied");

    // Its PR button opens the repository on the pull request.
    shots.click(window, at::row_button(0));
    assert_eq!(
        shown_pull_request(&mut shots, &workspace),
        Some(number),
        "PR #n opens the pull request"
    );
    shots.save(window, "u6-e2e-6-pull-request");

    // Home again: the same Home, the thread still shown.
    shots.click(window, at::home());
    let open = shots.update(|cx| workspace.read(cx).snapshot.is_some());
    assert!(!open, "Home closes the repository");
    let again = home_of(&mut shots, &workspace);
    assert_eq!(again, home, "the same Home");
    let shown = shots.update(|cx| home.read(cx).shown_thread(cx));
    assert_eq!(shown, Some(key(&notes, &work)), "still showing the thread");
    let [finished, ..] = listed(&mut shots);
    assert_eq!(finished, std::slice::from_ref(&work));
    shots.save(window, "u6-e2e-7-home-again");

    // An answer, completed from its row.
    shots.keys(window, "cmd-n");
    shots.type_text(window, "What is the answer?");
    shots.keys(window, "cmd-enter");
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the answer",
        |threads| {
            threads
                .iter()
                .any(|thread| thread.id != work && thread.state == State::Finished)
        },
    );
    let answer = threads
        .iter()
        .find(|thread| thread.id != work)
        .expect("the answer")
        .clone();
    assert_eq!(answer.ending, Some(Ending::Answer));
    let [finished, ..] = listed(&mut shots);
    assert_eq!(finished, [answer.id.clone(), work.clone()], "newest first");
    shots.save(window, "u6-e2e-8-answer");
    shots.click(window, at::row_button(0));
    wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the answer to complete",
        |threads| {
            threads
                .iter()
                .any(|thread| thread.id == answer.id && thread.state == State::Completed)
        },
    );
    let [finished, active, waiting] = listed(&mut shots);
    assert_eq!(finished, std::slice::from_ref(&work), "gone from Finished");
    assert!(
        !active.contains(&answer.id) && !waiting.contains(&answer.id),
        "a completed thread is not listed"
    );
    shots.save(window, "u6-e2e-9-completed");
}

/// A thread outlives the app: started from Home, the app goes, a new one
/// over the same data folder shows the thread running, then finished.
#[test]
fn e2e_u6_thread_outlives_the_app() {
    if !crate::screenshot::enabled() || std::env::var_os("GITEZ_E2E").is_none() {
        return;
    }
    let binary = gitez_binary().expect("build target/debug/gitez first: cargo build -p gitez");
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().canonicalize().expect("canonical");
    let work_dir = named_repository(&base, "gitez");
    let common_dir = common_dir_of(&work_dir);
    let data_dir = base.join("data");
    save_settings(&data_dir, &common_dir, fake_agent(&base, true), 3);
    let log = base.join("fake log");
    let script = supervisor_script(&base, &binary, &data_dir, true);
    let app = |script: &Path| {
        let script = script.to_owned();
        Shots::with_options(&data_dir, |options| {
            options.exe = Ok(script);
            options.watch = true;
        })
        .expect("enabled")
    };

    let mut shots = app(&script);
    let (window, _) = open_home(&mut shots, &[&work_dir]);
    shots.type_text(window, "Write agent.txt and commit it. WORK");
    shots.keys(window, "cmd-enter");
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the thread to run",
        |threads| threads.iter().any(|thread| thread.state == State::Running),
    );
    let id = threads[0].id.clone();
    // The app quits.
    drop(shots);

    let mut shots = app(&script);
    let (window, _) = open_home(&mut shots, &[&work_dir]);
    let [_, active, ..] = listed(&mut shots);
    assert_eq!(
        active,
        std::slice::from_ref(&id),
        "still running after the app went"
    );
    shots.save(window, "u6-e2e-outlives-1-running");
    std::fs::write(log.join("release"), "").expect("release the agent");
    wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(30),
        "the thread to finish",
        |threads| {
            threads
                .iter()
                .any(|thread| thread.id == id && thread.state == State::Finished)
        },
    );
    let [finished, ..] = listed(&mut shots);
    assert_eq!(finished, [id], "finished, in the new app");
    shots.save(window, "u6-e2e-outlives-2-finished");
}

/// One real run from Home: Claude Code with haiku, picked in the launcher,
/// in the real sandbox, in a throwaway repository under
/// `GITEZ_E2E_SCRATCH`; srt is copied from `GITEZ_E2E_SANDBOX`.
#[test]
fn e2e_u6_home_real_haiku() {
    if !crate::screenshot::enabled() || std::env::var_os("GITEZ_E2E_REAL").is_none() {
        return;
    }
    let binary = gitez_binary().expect("build target/debug/gitez first: cargo build -p gitez");
    let scratch = PathBuf::from(std::env::var_os("GITEZ_E2E_SCRATCH").expect("a scratch folder"));
    let sandbox = PathBuf::from(std::env::var_os("GITEZ_E2E_SANDBOX").expect("an srt install"));
    let base = scratch.join(format!("u6-real-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("scratch folder");
    let work_dir = named_repository(&base, "hello");
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
        3,
    );
    let script = supervisor_script(&base, &binary, &data_dir, false);
    let mut shots = Shots::with_options(&data_dir, |options| {
        options.sandbox = gitez_sandbox::platform(&data_dir);
        options.exe = Ok(script);
        options.watch = true;
    })
    .expect("enabled");
    let (window, workspace) = open_home(&mut shots, &[&work_dir]);
    let home = home_of(&mut shots, &workspace);
    shots.type_text(
        window,
        "Add a file hello.txt containing hi and commit it. Do nothing else.",
    );
    // haiku, as the owner allows, picked with the model picker.
    let model = picker(&mut shots, window, "model");
    shots.click(window, model);
    shots.click(window, at::haiku(model));
    let model = shots.update(|cx| home.read(cx).composer.read(cx).model_alias(cx));
    assert_eq!(model, "haiku", "haiku is picked");
    shots.save(window, "u6-real-1-ready");
    let started = Instant::now();
    shots.keys(window, "cmd-enter");
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(60),
        "the thread to start",
        |threads| !threads.is_empty(),
    );
    let id = threads[0].id.clone();
    shots.save(window, "u6-real-2-started");
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
        "done after {:?}: {:?} {:?}, pull request {:?}, model {:?}, error {:?}",
        started.elapsed(),
        done.state,
        done.ending,
        done.pull_request,
        done.model,
        done.error,
    );
    shots.click(window, at::finished_row(0));
    let shown = shots.update(|cx| home.read(cx).shown_thread(cx));
    assert_eq!(shown, Some(key(&work_dir, &id)));
    shots.save(window, "u6-real-3-done");
    assert_eq!(done.model.as_deref(), Some("haiku"));
    // A reply continues the same thread in its worktree.
    let replied = Instant::now();
    shots.click(window, at::reply());
    shots.type_text(
        window,
        "Now also add a file bye.txt containing bye and commit it. Do nothing else.",
    );
    shots.save(window, "u6-real-4-reply-typed");
    shots.keys(window, "cmd-enter");
    wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(60),
        "the reply to start",
        |threads| {
            threads
                .iter()
                .any(|thread| thread.id == id && thread.turns.len() == 2)
        },
    );
    shots.save(window, "u6-real-5-continuing");
    let threads = wait_until(
        &mut shots,
        &common_dir,
        Duration::from_secs(300),
        "the reply to finish",
        |threads| {
            threads.iter().any(|thread| {
                thread.id == id && thread.turns.len() == 2 && !thread.state.is_active()
            })
        },
    );
    let after = threads
        .iter()
        .find(|thread| thread.id == id)
        .expect("the thread");
    eprintln!(
        "reply done after {:?}: {:?} {:?}, pull request {:?}, resumed {}, session kept {}",
        replied.elapsed(),
        after.state,
        after.ending,
        after.pull_request,
        after.turns[1].resume,
        after.session_id == done.session_id,
    );
    shots.save(window, "u6-real-6-replied");
    assert!(after.turns[1].resume, "the reply resumes the conversation");
    assert_eq!(after.session_id, done.session_id);
    assert!(
        crate::agent_threads::thread_worktree(after)
            .join("bye.txt")
            .is_file(),
        "{after:#?}"
    );
}
