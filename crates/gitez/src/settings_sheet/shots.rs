//! Screenshots of the settings sheet, and runs that drive it with clicks
//! and typing and check what was saved. All of them return at once unless
//! `GITEZ_SCREENSHOTS` is set. Two more need the real thing and run only
//! when asked: `GITEZ_U4_KEYCHAIN=1` uses the OS keychain under the
//! service `GitEZ-u4-test`, `GITEZ_U4_INSTALL=1` installs srt with npm
//! into the data folder. `GITEZ_U4_SCRATCH` names the folder the test
//! repositories and data folders are made in.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gitez_sandbox::{NetworkProfile, Status};
use gitez_secret::SecretStore;
use gitez_settings::{AgentEntry, SecretEntry, SecretScope, Settings};
use gpui_kit::component::WindowExt as _;
use gpui_kit::{AnyWindowHandle, Bounds, Entity, Pixels, Point, point, px, size};

use super::{OpenSheets, SettingsSheet};
use crate::screenshot::{Shots, git};
use crate::workspace::Workspace;

/// A stand-in `claude`: answers `auth status` as signed in unless
/// `FAKE_SIGNED_IN=false`, or its configuration folder holds a file named
/// `signed-out`.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = auth ] && [ -n "$CLAUDE_CONFIG_DIR" ] && [ -e "$CLAUDE_CONFIG_DIR/signed-out" ]; then
  printf '{"loggedIn": false}\n'; exit 0
fi
if [ "$1" = auth ]; then printf '{"loggedIn": %s}\n' "${FAKE_SIGNED_IN:-true}"; exit 0; fi
exit 1
"#;

/// A temporary folder for one test, in `GITEZ_U4_SCRATCH` if set.
fn scratch() -> tempfile::TempDir {
    match std::env::var_os("GITEZ_U4_SCRATCH").filter(|dir| !dir.is_empty()) {
        Some(dir) => {
            std::fs::create_dir_all(&dir).expect("the scratch folder can be made");
            tempfile::tempdir_in(dir).expect("temp dir")
        }
        None => tempfile::tempdir().expect("temp dir"),
    }
}

/// A Rust repository in `base/gitez` with the sample history.
fn repository(base: &Path) -> PathBuf {
    let work_dir = base.join("gitez");
    crate::screenshot::sample_repository(&work_dir);
    std::fs::remove_file(work_dir.join("notes.txt")).expect("clean");
    std::fs::write(work_dir.join("Cargo.toml"), "[workspace]\n").expect("Cargo.toml");
    git(&work_dir, &["add", "."]);
    git(&work_dir, &["commit", "-qm", "Add the workspace manifest"]);
    git(&work_dir, &["config", "user.name", "Ada Lovelace"]);
    git(&work_dir, &["config", "user.email", "ada@example.com"]);
    work_dir
}

fn common_dir_of(work_dir: &Path) -> PathBuf {
    gitez_git::Repository::open(work_dir)
        .expect("open")
        .common_dir()
}

/// Claude Code run as the stand-in in `base/bin`.
fn fake_agent(base: &Path, signed_in: bool) -> AgentEntry {
    let bin = base.join("bin");
    std::fs::create_dir_all(&bin).expect("bin folder");
    let program = bin.join("claude");
    std::fs::write(&program, FAKE_CLAUDE).expect("write the stand-in");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    AgentEntry {
        program: Some(program),
        config_dir: Some(base.join("claude-config")),
        env: BTreeMap::from([("FAKE_SIGNED_IN".to_owned(), signed_in.to_string())]),
        ..AgentEntry::default()
    }
}

/// The board's three secrets: one for all repositories, two for this one,
/// the last one not given by default.
fn three_secrets(common_dir: &Path) -> Vec<SecretEntry> {
    vec![
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
    ]
}

/// A data folder in which srt counts as installed: its package files, run
/// by the Node.js on the PATH.
fn installed_srt(data_dir: &Path) {
    let package = gitez_sandbox::Srt::install_dir(data_dir)
        .join("node_modules/@anthropic-ai/sandbox-runtime");
    std::fs::create_dir_all(package.join("dist")).expect("package folder");
    std::fs::write(package.join("dist/cli.js"), "// srt\n").expect("cli");
    std::fs::write(
        package.join("package.json"),
        r#"{"name":"@anthropic-ai/sandbox-runtime","version":"0.0.82"}"#,
    )
    .expect("manifest");
}

/// A sandbox that says what a test wants, and installs when released.
struct FakeSandbox {
    status: Mutex<Status>,
    installable: AtomicBool,
    /// What `install` returns once `release` is set.
    outcome: Result<(), String>,
    release: Arc<AtomicBool>,
}

impl FakeSandbox {
    fn missing(fix: &str, installable: bool, outcome: Result<(), String>) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(Status::Missing { fix: fix.into() }),
            installable: AtomicBool::new(installable),
            outcome,
            release: Arc::new(AtomicBool::new(false)),
        })
    }
}

impl gitez_sandbox::Sandbox for FakeSandbox {
    fn name(&self) -> &'static str {
        "srt (Anthropic sandbox runtime)"
    }

    fn status(&self) -> Status {
        self.status.lock().expect("status").clone()
    }

    fn installable(&self) -> bool {
        self.installable.load(Ordering::SeqCst)
    }

    fn install(&self) -> gitez_sandbox::Result<()> {
        let started = Instant::now();
        while !self.release.load(Ordering::SeqCst) && started.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(10));
        }
        match &self.outcome {
            Ok(()) => {
                *self.status.lock().expect("status") = Status::Ready {
                    program: "/data/sandbox/node_modules/@anthropic-ai/sandbox-runtime/dist/cli.js"
                        .into(),
                    version: Some("0.0.82".into()),
                };
                self.installable.store(false, Ordering::SeqCst);
                Ok(())
            }
            Err(error) => Err(gitez_sandbox::Error::Install(error.clone())),
        }
    }

    fn wrap(
        &self,
        _command: gitez_agent::CommandSpec,
        _policy: &gitez_sandbox::Policy,
    ) -> gitez_sandbox::Result<gitez_sandbox::Wrapped> {
        Err(gitez_sandbox::Error::Unavailable("a stand-in".into()))
    }
}

/// What a test starts from.
struct Setup {
    work_dir: PathBuf,
    common_dir: PathBuf,
    data_dir: PathBuf,
}

/// A repository, and a data folder with `settings` (given the common
/// directory) saved in it.
fn prepare(base: &Path, settings: impl FnOnce(&Path, &Path) -> Settings) -> Setup {
    let work_dir = repository(base);
    let common_dir = common_dir_of(&work_dir);
    let data_dir = base.join("data");
    settings(&common_dir, base)
        .save(&Settings::path(&data_dir))
        .expect("settings saved");
    Setup {
        work_dir,
        common_dir,
        data_dir,
    }
}

/// The sheet open in the window.
fn sheet(shots: &mut Shots) -> Entity<SettingsSheet> {
    shots.update(|cx| {
        cx.global::<OpenSheets>()
            .0
            .iter()
            .find_map(|(_, sheet)| sheet.upgrade())
            .expect("the settings sheet is open")
    })
}

fn settings_on_disk(data_dir: &Path) -> Settings {
    match Settings::load(&Settings::path(data_dir)).expect("settings.json reads") {
        gitez_settings::Loaded::Current(settings) => settings,
        gitez_settings::Loaded::TooNew => panic!("settings.json is too new"),
    }
}

/// Whether the settings sheet is shown in `window`.
fn sheet_open(shots: &mut Shots, window: AnyWindowHandle) -> bool {
    shots.update_window(window, |window, cx| {
        let id = window.window_handle().window_id();
        window.has_active_dialog(cx)
            && cx.try_global::<OpenSheets>().is_some_and(|open| {
                open.0
                    .iter()
                    .any(|(window, sheet)| *window == id && sheet.upgrade().is_some())
            })
    })
}

/// The window with the repository and the sheet opened with `⌘,`, as the
/// app binds it.
fn open_sheet(shots: &mut Shots, work_dir: &Path) -> (AnyWindowHandle, Entity<Workspace>) {
    shots.update(super::init);
    let (window, workspace) = shots.open_workspace(work_dir);
    shots.keys(window, "cmd-,");
    assert!(sheet_open(shots, window), "⌘, opens the settings sheet");
    (window, workspace)
}

/// Every file under `dir` that contains `needle`.
fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if let Ok(bytes) = std::fs::read(&path)
                && bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes())
            {
                found.push(path);
            }
        }
    }
    found
}

/// Waits, in real time, until `done` holds, letting the app run between
/// looks; for work on threads of its own, such as an install.
fn wait_for(shots: &mut Shots, limit: Duration, mut done: impl FnMut(&mut Shots) -> bool) {
    let started = Instant::now();
    while !done(shots) {
        assert!(started.elapsed() < limit, "gave up waiting after {limit:?}");
        std::thread::sleep(Duration::from_millis(200));
        shots.settle();
    }
}

/// The sheet, as placed in a 1440 × 900 window.
fn sheet_region() -> Bounds<Pixels> {
    Bounds {
        origin: point(px(170.), px(32.)),
        size: size(px(1100.), px(836.)),
    }
}

fn at(x: f32, y: f32) -> Point<Pixels> {
    point(px(x), px(y))
}

/// The sheet with realistic content: three secrets (one missing from the
/// keychain), Claude Code signed in, srt ready, Packages for new threads,
/// and this repository's setup command.
#[test]
fn screenshot_u4_sheet() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = scratch();
    let setup = prepare(dir.path(), |common_dir, base| {
        let mut settings = Settings {
            secrets: three_secrets(common_dir),
            ..Settings::default()
        };
        let mut agent = fake_agent(base, true);
        agent.env.insert("DISABLE_AUTOUPDATER".into(), "1".into());
        agent.args = vec!["--max-turns".into(), "40".into()];
        settings
            .agents
            .insert(gitez_agent::ClaudeCode::ID.into(), agent);
        settings.sandbox.default_network = NetworkProfile::Packages;
        settings.threads.max_running = 4;
        settings.sandbox.default_local_ports = true;
        settings
    });
    git(
        &setup.work_dir,
        &["config", "gitez.setupCommand", "cargo fetch"],
    );
    installed_srt(&setup.data_dir);
    let store = Arc::new(gitez_secret::Memory::default());
    for entry in three_secrets(&setup.common_dir).iter().take(2) {
        store.set(&entry.account(), "value").expect("set");
    }
    let data_dir = setup.data_dir.clone();
    let mut shots = Shots::with_options(&setup.data_dir, |options| {
        options.sandbox = gitez_sandbox::platform(&data_dir);
        options.secrets = store.clone();
    })
    .expect("enabled");
    let (window, _) = open_sheet(&mut shots, &setup.work_dir);
    shots.save(window, "u4-sheet");
    shots.save_region(window, sheet_region(), "u4-sheet-detail");
    shots.scroll(window, at(720., 500.), 30.);
    shots.save(window, "u4-sheet-scrolled");
}

/// Claude accounts: the default one and Work signed in, Client signed
/// out, and this repository running its agents with Work, as its own git
/// config says.
#[test]
fn screenshot_accounts() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = scratch();
    let setup = prepare(dir.path(), |_, base| {
        let mut settings = Settings::default();
        settings
            .agents
            .insert(gitez_agent::ClaudeCode::ID.into(), fake_agent(base, true));
        for (name, folder) in [("Work", "claude-work"), ("Client", "claude-client")] {
            std::fs::create_dir_all(base.join(folder)).expect("account folder");
            settings.accounts.push(gitez_settings::AccountEntry {
                name: name.into(),
                folder: base.join(folder),
            });
        }
        std::fs::write(base.join("claude-client/signed-out"), "").expect("signed out");
        settings
    });
    git(&setup.work_dir, &["config", "gitez.agentAccount", "Work"]);
    installed_srt(&setup.data_dir);
    let data_dir = setup.data_dir.clone();
    let mut shots = Shots::with_options(&setup.data_dir, |options| {
        options.sandbox = gitez_sandbox::platform(&data_dir);
    })
    .expect("enabled");
    let (window, _) = open_sheet(&mut shots, &setup.work_dir);
    wait_for(&mut shots, Duration::from_secs(20), |shots| {
        let sheet = sheet(shots);
        shots.update(|cx| sheet.read(cx).accounts_checked())
    });
    shots.save(window, "accounts-sheet");
    shots.scroll(window, at(450., 600.), 400.);
    shots.save(window, "accounts-list");
    shots.scroll(window, at(1000., 600.), 400.);
    shots.save(window, "accounts-repository");

    // A repository naming an account Settings do not have.
    git(
        &setup.work_dir,
        &["config", "gitez.agentAccount", "Personal"],
    );
    let dir = scratch();
    let other = prepare(dir.path(), |_, base| {
        let mut settings = Settings::default();
        settings
            .agents
            .insert(gitez_agent::ClaudeCode::ID.into(), fake_agent(base, true));
        settings
    });
    git(
        &other.work_dir,
        &["config", "gitez.agentAccount", "Personal"],
    );
    installed_srt(&other.data_dir);
    let data_dir = other.data_dir.clone();
    let mut shots = Shots::with_options(&other.data_dir, |options| {
        options.sandbox = gitez_sandbox::platform(&data_dir);
    })
    .expect("enabled");
    let (window, _) = open_sheet(&mut shots, &other.work_dir);
    shots.scroll(window, at(1000., 600.), 400.);
    shots.save(window, "accounts-repository-unknown");
}

/// Empty and broken states: no secrets, srt not installed, Claude Code
/// not found; and Claude Code signed out; and Node.js missing or too old.
#[test]
fn screenshot_u4_states() {
    if !crate::screenshot::enabled() {
        return;
    }
    // No secrets, srt not installed (Node.js is), the agent not found.
    let dir = scratch();
    let setup = prepare(dir.path(), |_, base| {
        let mut agent = fake_agent(base, true);
        agent.program = Some(base.join("nowhere/claude"));
        let mut settings = Settings::default();
        settings
            .agents
            .insert(gitez_agent::ClaudeCode::ID.into(), agent);
        settings
    });
    let data_dir = setup.data_dir.clone();
    let mut shots = Shots::with_options(&setup.data_dir, |options| {
        options.sandbox = gitez_sandbox::platform(&data_dir);
    })
    .expect("enabled");
    let (window, _) = open_sheet(&mut shots, &setup.work_dir);
    shots.save(window, "u4-empty");

    // Signed out.
    let dir = scratch();
    let setup = prepare(dir.path(), |_, base| {
        let mut settings = Settings::default();
        settings
            .agents
            .insert(gitez_agent::ClaudeCode::ID.into(), fake_agent(base, false));
        settings
    });
    installed_srt(&setup.data_dir);
    let data_dir = setup.data_dir.clone();
    let mut shots = Shots::with_options(&setup.data_dir, |options| {
        options.sandbox = gitez_sandbox::platform(&data_dir);
    })
    .expect("enabled");
    let (window, _) = open_sheet(&mut shots, &setup.work_dir);
    shots.save(window, "u4-signed-out");

    for (name, fix) in [
        (
            "u4-node-missing",
            "The sandbox needs Node.js. Install it (for example from nodejs.org), then install \
             the sandbox in Settings.",
        ),
        (
            "u4-node-old",
            "The sandbox needs Node.js 22.12 or newer; /usr/local/bin/node is v20.11.1. Update \
             Node.js, then try again.",
        ),
    ] {
        let dir = scratch();
        let setup = prepare(dir.path(), |_, base| {
            let mut settings = Settings::default();
            settings
                .agents
                .insert(gitez_agent::ClaudeCode::ID.into(), fake_agent(base, true));
            settings
        });
        let sandbox = FakeSandbox::missing(fix, false, Ok(()));
        let mut shots = Shots::with_options(&setup.data_dir, |options| {
            options.sandbox = sandbox.clone();
        })
        .expect("enabled");
        let (window, _) = open_sheet(&mut shots, &setup.work_dir);
        shots.save(window, name);
    }
}

/// The values the driven runs type; none may reach a file.
const FIRST_VALUE: &str = "ghp_u4FirstValue7731";
const REPLACED_VALUE: &str = "ghp_u4ReplacedValue9902";
const OTHER_VALUE: &str = "npm_u4OtherValue4410";
/// The height of a row in the secrets table, and the middle of the first.
const ROW: f32 = 51.;
const FIRST_ROW: f32 = 302.;

/// A data folder with Claude Code signed in, srt ready, and no secrets.
fn plain_setup(base: &Path) -> Setup {
    let setup = prepare(base, |_, base| {
        let mut settings = Settings::default();
        settings
            .agents
            .insert(gitez_agent::ClaudeCode::ID.into(), fake_agent(base, true));
        settings
    });
    installed_srt(&setup.data_dir);
    setup
}

/// Drives the sheet as a person would, with `store` as the keychain:
/// adds three secrets by typing (after a name that is refused), replaces
/// one value, turns one off for new threads, deletes one; raises the
/// limit, picks the Open network, edits the Packages hosts, hides a path
/// and shows another; then opens the composer. Checks the store,
/// settings.json and the composer after each step, and that no value is
/// written to a file. `keychain` checks an account in the OS keychain
/// without reading its value, when the store is the keychain.
fn drive(
    shots: &mut Shots,
    setup: &Setup,
    store: &dyn SecretStore,
    keychain: Option<&dyn Fn(&str) -> bool>,
    prefix: &str,
) {
    let (window, workspace) = open_sheet(shots, &setup.work_dir);
    let account = |name: &str| {
        SecretEntry {
            name: name.into(),
            scope: SecretScope::Repository(setup.common_dir.clone()),
            default: true,
        }
        .account()
    };

    // A name that cannot be an environment variable is refused as typed.
    shots.click(window, at(323., 387.));
    shots.type_text(window, "github-token");
    shots.click(window, at(649., 422.));
    if prefix == "u4" {
        shots.save_region(window, sheet_region(), "u4-add-secret-invalid");
    }
    assert!(settings_on_disk(&setup.data_dir).secrets.is_empty());

    // Three secrets, given to new threads by default.
    shots.click(window, at(381., 422.));
    for (index, (name, value)) in [
        ("GITHUB_TOKEN", FIRST_VALUE),
        ("NPM_TOKEN", OTHER_VALUE),
        ("SENTRY_DSN", OTHER_VALUE),
    ]
    .into_iter()
    .enumerate()
    {
        // The form moves down a row with each secret listed.
        let y = if index == 0 {
            387.
        } else {
            372. + ROW * (index - 1) as f32
        };
        shots.click(window, at(323., y));
        shots.keys(window, "cmd-a backspace");
        shots.type_text(window, name);
        shots.click(window, at(571., y));
        shots.type_text(window, value);
        shots.keys(window, "enter");
        shots.save(window, &format!("{prefix}-drive-add-{index}"));
        assert_eq!(
            store
                .get(&account(name))
                .expect("the store reads")
                .as_deref(),
            Some(value),
            "{name} is in the keychain"
        );
    }
    let on_disk = settings_on_disk(&setup.data_dir);
    assert_eq!(on_disk.secrets.len(), 3);
    assert!(
        on_disk.secrets.iter().all(|entry| entry.default
            && entry.scope == SecretScope::Repository(setup.common_dir.clone()))
    );
    if let Some(in_keychain) = keychain {
        for name in ["GITHUB_TOKEN", "NPM_TOKEN", "SENTRY_DSN"] {
            assert!(in_keychain(&account(name)), "{name} is in the OS keychain");
        }
    }
    let open = sheet(shots);
    let presence = shots.update(|cx| {
        open.read(cx)
            .secrets
            .presence(&account("NPM_TOKEN"))
            .cloned()
    });
    assert_eq!(presence, Some(super::secrets::Presence::Stored));

    // Replace GITHUB_TOKEN's value: a write-only field under its row.
    shots.click(window, at(651., FIRST_ROW));
    shots.type_text(window, REPLACED_VALUE);
    if prefix == "u4" {
        shots.save_region(window, sheet_region(), "u4-secret-replace");
    }
    shots.keys(window, "enter");
    assert_eq!(
        store
            .get(&account("GITHUB_TOKEN"))
            .expect("reads")
            .as_deref(),
        Some(REPLACED_VALUE),
        "the value is replaced"
    );

    // NPM_TOKEN is no longer given to new threads.
    shots.click(window, at(533., FIRST_ROW + ROW));
    let on_disk = settings_on_disk(&setup.data_dir);
    let npm = on_disk
        .secrets
        .iter()
        .find(|entry| entry.name == "NPM_TOKEN")
        .expect("NPM_TOKEN is listed");
    assert!(!npm.default, "NPM_TOKEN is off for new threads");

    // Delete SENTRY_DSN, after saying yes.
    shots.click(window, at(679., FIRST_ROW + 2. * ROW));
    if prefix == "u4" {
        shots.save_region(window, sheet_region(), "u4-secret-delete");
    }
    shots.click(window, at(667., FIRST_ROW + 2. * ROW + 37.));
    assert_eq!(store.get(&account("SENTRY_DSN")).expect("reads"), None);
    if let Some(in_keychain) = keychain {
        assert!(
            !in_keychain(&account("SENTRY_DSN")),
            "deleted from the OS keychain"
        );
    }
    let names: Vec<String> = settings_on_disk(&setup.data_dir)
        .secrets
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(names, ["GITHUB_TOKEN", "NPM_TOKEN"]);

    // No value reached a file: not settings.json, not git config.
    for value in [FIRST_VALUE, REPLACED_VALUE, OTHER_VALUE] {
        for dir in [&setup.data_dir, &setup.common_dir] {
            assert_eq!(files_containing(dir, value), Vec::<PathBuf>::new());
        }
    }

    // The Open network for new threads.
    shots.click(window, at(787., 584.));
    assert_eq!(
        settings_on_disk(&setup.data_dir).sandbox.default_network,
        NetworkProfile::Open
    );

    // The Packages hosts, edited as text.
    shots.click(window, at(1047., 532.));
    shots.keys(window, "cmd-a backspace");
    for (index, host) in [
        "crates.io",
        "https://index.crates.io/",
        "*.u4-mirror.example.com",
    ]
    .into_iter()
    .enumerate()
    {
        if index > 0 {
            shots.keys(window, "enter");
        }
        shots.type_text(window, host);
    }
    shots.save(window, &format!("{prefix}-drive-hosts"));
    shots.click(window, at(SAVE_HOSTS.0, SAVE_HOSTS.1));
    assert_eq!(
        settings_on_disk(&setup.data_dir).sandbox.package_hosts,
        ["crates.io", "index.crates.io", "*.u4-mirror.example.com"]
    );

    // Further down: the limit, and the hidden paths.
    shots.scroll(window, at(720., 500.), 30.);
    shots.save(window, &format!("{prefix}-drive-scrolled"));
    shots.click(window, at(PLUS.0, PLUS.1));
    assert_eq!(settings_on_disk(&setup.data_dir).threads.max_running, 4);
    shots.click(window, at(HIDE.0, HIDE.1));
    shots.type_text(window, "~/.config/u4-tool");
    shots.keys(window, "enter");
    shots.click(window, at(UNHIDE_AWS.0, UNHIDE_AWS.1));
    // A path hidden by default is shown only after asking.
    assert!(
        settings_on_disk(&setup.data_dir)
            .sandbox
            .hidden_paths
            .contains(&"~/.aws".to_owned()),
        "not before the answer"
    );
    if prefix == "u4" {
        shots.save(window, "fix-u4-unhide-asks");
    }
    shots.click_id(window, "unhide-confirm");
    let hidden = settings_on_disk(&setup.data_dir).sandbox.hidden_paths;
    assert!(
        hidden.contains(&"~/.config/u4-tool".to_owned()),
        "{hidden:?}"
    );
    assert!(!hidden.contains(&"~/.aws".to_owned()), "{hidden:?}");

    // This repository's setup command, saved in its git config.
    shots.click(window, at(SETUP.0, SETUP.1));
    shots.type_text(window, "cargo fetch --locked");
    let open = sheet(shots);
    wait_for(shots, Duration::from_secs(30), |shots| {
        !shots.update(|cx| open.read(cx).repository_busy())
    });
    let setup_command = std::process::Command::new("git")
        .args(["config", "--get", "gitez.setupCommand"])
        .current_dir(&setup.work_dir)
        .output()
        .expect("git runs");
    assert_eq!(
        String::from_utf8_lossy(&setup_command.stdout).trim(),
        "cargo fetch --locked"
    );
    if prefix == "u4" {
        shots.save(window, "u4-drive-done");
    }

    // Done closes the sheet; the composer starts from the new defaults.
    shots.click(window, at(1229., 843.));
    assert!(!sheet_open(shots, window), "Done closes the sheet");
    shots.keys(window, "cmd-,");
    assert!(sheet_open(shots, window), "⌘, opens it again");
    shots.keys(window, "escape");
    assert!(!sheet_open(shots, window), "Escape closes it");
    shots.keys(window, "cmd-shift-a");
    let (network, secrets, _model) = shots
        .update(|cx| workspace.read(cx).composer_choices(cx))
        .expect("the composer is open");
    assert_eq!(network, NetworkProfile::Open);
    assert_eq!(secrets, ["GITHUB_TOKEN"]);
    if prefix == "u4" {
        shots.save(window, "u4-composer-after");
    }
}

/// Where the driven run clicks, measured on its screenshots: "Save hosts",
/// and after scrolling to the end the limit's +, "Hide a path" and the ×
/// of ~/.aws.
const SAVE_HOSTS: (f32, f32) = (1199., 774.);
const PLUS: (f32, f32) = (994., 587.);
const HIDE: (f32, f32) = (1100., 491.);
const UNHIDE_AWS: (f32, f32) = (976., 394.);
const SETUP: (f32, f32) = (1044., 692.);

/// The sheet driven with clicks and typing, with an in-memory keychain.
#[test]
fn screenshot_u4_driven() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = scratch();
    let setup = plain_setup(dir.path());
    let store = Arc::new(gitez_secret::Memory::default());
    let data_dir = setup.data_dir.clone();
    let mut shots = Shots::with_options(&setup.data_dir, |options| {
        options.sandbox = gitez_sandbox::platform(&data_dir);
        options.secrets = store.clone();
    })
    .expect("enabled");
    drive(&mut shots, &setup, store.as_ref(), None, "u4");
}

/// The Install button of the sandbox card, with srt not installed.
const INSTALL: (f32, f32) = (1211., 250.);

/// Whether the sheet's sandbox install is running.
fn installing(shots: &mut Shots) -> bool {
    let open = sheet(shots);
    shots.update(|cx| open.read(cx).install_state().0 == super::sandbox::Install::Running)
}

/// Installing the sandbox from the sheet: running, failed with npm's
/// error, and done, with a stand-in sandbox that waits to be released.
#[test]
fn screenshot_u4_install() {
    if !crate::screenshot::enabled() {
        return;
    }
    let fix = "The sandbox is not installed.";
    let failure = "npm exited with exit status: 1: npm error code E404\nnpm error 404 Not Found - \
                   GET https://registry.npmjs.org/@anthropic-ai%2fsandbox-runtime - Not found";
    for outcome in [Err(failure.to_owned()), Ok(())] {
        let dir = scratch();
        let setup = plain_setup(dir.path());
        let sandbox = FakeSandbox::missing(fix, true, outcome.clone());
        let mut shots = Shots::with_options(&setup.data_dir, |options| {
            options.sandbox = sandbox.clone();
        })
        .expect("enabled");
        let (window, _) = open_sheet(&mut shots, &setup.work_dir);
        let open = sheet(&mut shots);
        let environment = shots.update(|cx| open.read(cx).model.read(cx).environment());
        shots.click(window, at(INSTALL.0, INSTALL.1));
        assert!(installing(&mut shots), "the install runs");
        if outcome.is_err() {
            shots.save_region(window, sheet_region(), "u4-install-running");
        }
        sandbox.release.store(true, Ordering::SeqCst);
        wait_for(&mut shots, Duration::from_secs(30), |shots| {
            !installing(shots)
        });
        let (install, ready) = shots.update(|cx| open.read(cx).install_state());
        let changed = shots.update(|cx| open.read(cx).model.read(cx).environment());
        assert_ne!(changed, environment, "the other views are told");
        match outcome {
            Err(_) => {
                assert!(
                    matches!(install, super::sandbox::Install::Failed(_)),
                    "{install:?}"
                );
                assert_eq!(ready, Some(false));
                shots.save_region(window, sheet_region(), "u4-install-failed");
            }
            Ok(()) => {
                assert_eq!(install, super::sandbox::Install::Idle);
                assert_eq!(ready, Some(true));
                shots.save_region(window, sheet_region(), "u4-install-done");
            }
        }
    }
}

/// Removes the driven run's items from the OS keychain however the test
/// ends.
struct KeychainCleanup {
    store: Arc<gitez_secret::Keychain>,
    accounts: Vec<String>,
}

impl Drop for KeychainCleanup {
    fn drop(&mut self) {
        for account in &self.accounts {
            self.store.delete(account).ok();
        }
    }
}

/// The driven run against the real OS keychain, under the service
/// `GitEZ-u4-test`; only with `GITEZ_U4_KEYCHAIN=1`.
#[test]
fn screenshot_u4_keychain() {
    const SERVICE: &str = "GitEZ-u4-test";
    if !crate::screenshot::enabled()
        || std::env::var_os("GITEZ_U4_KEYCHAIN").is_none_or(|on| on != "1")
    {
        return;
    }
    let dir = scratch();
    let setup = plain_setup(dir.path());
    let store = Arc::new(gitez_secret::Keychain::new(SERVICE));
    let _cleanup = KeychainCleanup {
        store: store.clone(),
        accounts: ["GITHUB_TOKEN", "NPM_TOKEN", "SENTRY_DSN"]
            .into_iter()
            .map(|name| {
                SecretEntry {
                    name: name.into(),
                    scope: SecretScope::Repository(setup.common_dir.clone()),
                    default: true,
                }
                .account()
            })
            .collect(),
    };
    // Asks the keychain whether the item exists; its value is not printed.
    let in_keychain = |account: &str| {
        std::process::Command::new("security")
            .args(["find-generic-password", "-s", SERVICE, "-a", account])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("security runs")
            .success()
    };
    let data_dir = setup.data_dir.clone();
    let mut shots = Shots::with_options(&setup.data_dir, |options| {
        options.sandbox = gitez_sandbox::platform(&data_dir);
        options.secrets = store.clone();
    })
    .expect("enabled");
    drive(
        &mut shots,
        &setup,
        store.as_ref(),
        Some(&in_keychain),
        "u4-keychain",
    );
}

/// Installs srt for real, with npm, into a scratch data folder; only with
/// `GITEZ_U4_INSTALL=1`.
#[test]
fn screenshot_u4_real_install() {
    if !crate::screenshot::enabled()
        || std::env::var_os("GITEZ_U4_INSTALL").is_none_or(|on| on != "1")
    {
        return;
    }
    let dir = scratch();
    let setup = prepare(dir.path(), |_, base| {
        let mut settings = Settings::default();
        settings
            .agents
            .insert(gitez_agent::ClaudeCode::ID.into(), fake_agent(base, true));
        settings
    });
    let data_dir = setup.data_dir.clone();
    let mut shots = Shots::with_options(&setup.data_dir, |options| {
        options.sandbox = gitez_sandbox::platform(&data_dir);
    })
    .expect("enabled");
    let (window, _) = open_sheet(&mut shots, &setup.work_dir);
    let open = sheet(&mut shots);
    let (_, ready) = shots.update(|cx| open.read(cx).install_state());
    assert_eq!(ready, Some(false), "srt is not in the new data folder");
    shots.click(window, at(INSTALL.0, INSTALL.1));
    assert!(installing(&mut shots), "the install runs");
    shots.save_region(window, sheet_region(), "u4-install-real-running");
    wait_for(&mut shots, Duration::from_secs(600), |shots| {
        !installing(shots)
    });
    shots.save_region(window, sheet_region(), "u4-install-real-done");
    let (install, ready) = shots.update(|cx| open.read(cx).install_state());
    assert_eq!(install, super::sandbox::Install::Idle, "{install:?}");
    assert_eq!(ready, Some(true));
    assert!(
        gitez_sandbox::Srt::install_dir(&setup.data_dir)
            .join("node_modules/@anthropic-ai/sandbox-runtime/package.json")
            .is_file()
    );
}

/// The sheet in a short window (1440 × 600) with twelve secrets: it
/// scrolls to its last section.
#[test]
fn screenshot_scroll_settings_sheet() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = scratch();
    let setup = prepare(dir.path(), |common_dir, base| {
        let mut secrets = three_secrets(common_dir);
        secrets.extend((0..9).map(|index| SecretEntry {
            name: format!("EXTRA_TOKEN_{index}"),
            scope: SecretScope::All,
            default: index % 2 == 0,
        }));
        let mut settings = Settings {
            secrets,
            ..Settings::default()
        };
        settings
            .agents
            .insert(gitez_agent::ClaudeCode::ID.into(), fake_agent(base, true));
        settings
    });
    installed_srt(&setup.data_dir);
    let data_dir = setup.data_dir.clone();
    let mut shots = Shots::with_options(&setup.data_dir, |options| {
        options.sandbox = gitez_sandbox::platform(&data_dir);
    })
    .expect("enabled");
    shots.update(super::init);
    let (window, _) = shots.open_workspace_sized(&setup.work_dir, size(px(1440.), px(600.)));
    shots.keys(window, "cmd-,");
    assert!(
        sheet_open(&mut shots, window),
        "⌘, opens the settings sheet"
    );
    shots.save(window, "scroll-settings-top");
    for _ in 0..10 {
        shots.scroll(window, at(720., 300.), 100.);
    }
    shots.save(window, "scroll-settings-end");
}
