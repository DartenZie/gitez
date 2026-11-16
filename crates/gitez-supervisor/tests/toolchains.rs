//! Real toolchains inside the real sandbox (all `#[ignore]`).
//!
//! A fake agent runs a shell workload with exactly the policy and
//! environment a thread's agent gets: the `gitez-supervisor` binary runs
//! it, started with a Finder-like environment (no `CARGO_*`, a bare
//! `PATH`), in a linked worktree under a data folder whose path has a
//! space, prepared with the real `prepare` (so `target/` and
//! `node_modules/` are cloned from the main worktree). Run with srt
//! installed in a scratch data folder:
//!
//! ```sh
//! GITEZ_SANDBOX_TEST_DATA_DIR=/path/to/scratch/data \
//!   cargo test -p gitez-supervisor --test toolchains -- --ignored --nocapture
//! ```
//!
//! The Rust tests need cargo with serde, serde_json, regex, anyhow and
//! thiserror already in `~/.cargo/registry` (the setup builds the project
//! outside the sandbox, as the user would) and reach crates.io. The Node
//! tests need npm, the Python test `python3`, the Go test `go`; a test
//! whose toolchain is missing says so and passes.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::time::{Duration, Instant};

use common::{Fixture, git, is_done, worktree_of};
use gitez_agent::ClaudeCode;
use gitez_agent_thread::{AgentThread, Mode, NewAgentThread, Start, Timestamp};
use gitez_sandbox::NetworkProfile;
use gitez_settings::{Loaded, Settings};
use gitez_supervisor::{Command, Launch, Secrets};

/// The fake agent: prints an init event, runs `$GITEZ_WORKLOAD` with bash,
/// passing each output line on as `OUT <line>`, then a result event.
const RUNNER: &str = r#"#!/bin/sh
sid=""
prev=""
for arg in "$@"; do
  case "$prev" in --session-id|--resume) sid="$arg" ;; esac
  prev="$arg"
done
cat > /dev/null
printf '%s\n' "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$sid\",\"model\":\"fake\"}"
GITEZ_TEST_SESSION="$sid" /bin/bash "$GITEZ_WORKLOAD" < /dev/null 2>&1 | sed -u 's/^/OUT /'
printf '%s\n' "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\",\"session_id\":\"$sid\",\"num_turns\":1}"
"#;

/// Helpers every workload starts with: `step <name> <command…>` prints
/// `STEP <name> rc=<code>`, then the command's last lines, each as `  | …`.
const PRELUDE: &str = r#"
step() {
  name=$1; shift
  out=$("$@" 2>&1); rc=$?
  echo "STEP $name rc=$rc"
  printf '%s\n' "$out" | tail -n 40 | sed 's/^/  | /'
  return 0
}
"#;

const RUST_PROJECT: &str = r#"set -e
cat > Cargo.toml <<'T'
[package]
name = "demo"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
regex = "1"
anyhow = "1"
thiserror = "2"
T
mkdir -p src
cat > src/lib.rs <<'T'
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum DemoError {
    #[error("no match")]
    NoMatch,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

pub fn first_number(text: &str) -> Result<i32, DemoError> {
    let re = regex::Regex::new(r"\d+").expect("valid regex");
    re.find(text)
        .and_then(|m| m.as_str().parse().ok())
        .ok_or(DemoError::NoMatch)
}

pub fn to_json(point: &Point) -> anyhow::Result<String> {
    Ok(serde_json::to_string(point)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_numbers() {
        assert_eq!(first_number("abc 42 x").unwrap(), 42);
    }

    #[test]
    fn round_trips() {
        let p = Point { x: 1, y: 2 };
        let text = to_json(&p).unwrap();
        assert_eq!(serde_json::from_str::<Point>(&text).unwrap(), p);
    }
}
T
cat > src/main.rs <<'T'
fn main() {
    println!("first number: {}", demo::first_number("v12").unwrap());
}
T
printf 'target/\n' > .gitignore
cargo build --quiet
cargo test --quiet
cargo clippy --quiet --all-targets
"#;

const NODE_PROJECT: &str = r#"set -e
cat > package.json <<'T'
{
  "name": "demo",
  "version": "1.0.0",
  "private": true,
  "scripts": { "test": "node --test" },
  "dependencies": { "kleur": "^4.1.5", "ms": "^2.1.3" }
}
T
cat > index.js <<'T'
const ms = require("ms");
exports.twoHours = () => ms("2h");
T
cat > index.test.js <<'T'
const test = require("node:test");
const assert = require("node:assert");
test("two hours", () => assert.strictEqual(require("./index").twoHours(), 7200000));
T
printf 'node_modules/\n' > .gitignore
npm install --no-fund --no-audit --silent
"#;

/// The srt install the tests use: `$GITEZ_SANDBOX_TEST_DATA_DIR/sandbox`.
fn srt_data_dir() -> PathBuf {
    std::env::var_os("GITEZ_SANDBOX_TEST_DATA_DIR")
        .map(PathBuf::from)
        .expect("set GITEZ_SANDBOX_TEST_DATA_DIR to a scratch data folder with srt installed")
}

/// What the user's login shell has as `PATH`, as GitEZ asks it.
fn login_path() -> String {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_owned());
    let output = Process::new(shell)
        .args(["-l", "-i", "-c", "printf %s \"$PATH\""])
        .output()
        .expect("run the login shell");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Whether `program` runs from the login shell's `PATH`.
fn have(program: &str) -> bool {
    let found = Process::new("/bin/sh")
        .args(["-c", &format!("command -v {program}")])
        .env("PATH", login_path())
        .output()
        .is_ok_and(|output| output.status.success());
    if !found {
        println!("{program} is not installed; skipping");
    }
    found
}

/// `sh -c <script>` in `dir` with a clean environment (`HOME`, the login
/// shell's `PATH`), as the user would run it in a terminal; panics on
/// failure.
fn shell(dir: &Path, script: &str) -> String {
    let output = Process::new("/bin/sh")
        .args(["-c", script])
        .current_dir(dir)
        .env_clear()
        .env("HOME", std::env::var_os("HOME").expect("HOME"))
        .env("PATH", login_path())
        .env("LANG", "en_US.UTF-8")
        .output()
        .expect("run sh");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{script}:\n{text}");
    text
}

/// What one workload printed.
struct Output {
    lines: Vec<String>,
    thread: AgentThread,
}

impl Output {
    /// The exit code and output of step `name`.
    fn step(&self, name: &str) -> (i32, String) {
        let header = format!("STEP {name} rc=");
        let at = self
            .lines
            .iter()
            .position(|line| line.starts_with(&header))
            .unwrap_or_else(|| panic!("no step {name} in:\n{}", self.lines.join("\n")));
        let code = self.lines[at][header.len()..]
            .parse()
            .expect("an exit code");
        let text = self.lines[at + 1..]
            .iter()
            .take_while(|line| line.starts_with("  | "))
            .map(|line| &line[4..])
            .collect::<Vec<_>>()
            .join("\n");
        (code, text)
    }

    /// Step `name` succeeded; returns its output.
    fn ok(&self, name: &str) -> String {
        let (code, text) = self.step(name);
        assert_eq!(code, 0, "step {name} failed:\n{text}");
        text
    }

    /// Step `name` failed; returns its output.
    fn failed(&self, name: &str) -> String {
        let (code, text) = self.step(name);
        assert_ne!(code, 0, "step {name} should have failed:\n{text}");
        text
    }

    /// The value the workload printed as `<name>=<value>`.
    fn value(&self, name: &str) -> String {
        let prefix = format!("{name}=");
        self.lines
            .iter()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap_or_default()
            .to_owned()
    }
}

struct Toolchains {
    fixture: Fixture,
    /// Variables for the supervisor's environment, as `NAME='value'`.
    supervisor_env: String,
}

impl Toolchains {
    fn new() -> Self {
        let fixture = Fixture::new();
        // The real srt, where GitEZ installs it.
        std::os::unix::fs::symlink(
            srt_data_dir().join("sandbox"),
            fixture.data_dir.join("sandbox"),
        )
        .expect("link the srt install");
        let runner = fixture.base.join("bin").join("claude-runner");
        std::fs::write(&runner, RUNNER).expect("write the runner");
        std::fs::set_permissions(&runner, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        let toolchains = Self {
            fixture,
            supervisor_env: std::env::var("TOOLCHAIN_SUPERVISOR_ENV").unwrap_or_default(),
        };
        toolchains.change_settings(|settings| {
            settings
                .agents
                .get_mut(ClaudeCode::ID)
                .expect("the fixture's agent entry")
                .program = Some(runner);
        });
        toolchains
    }

    /// A fixture whose main worktree holds the project `setup` writes
    /// (run outside the sandbox, as the user would), committed.
    fn with_project(setup: &str) -> Self {
        let toolchains = Self::new();
        shell(&toolchains.fixture.root, setup);
        git(&toolchains.fixture.root, &["add", "-A"]);
        git(
            &toolchains.fixture.root,
            &["commit", "--quiet", "-m", "Project"],
        );
        toolchains
    }

    fn change_settings(&self, change: impl FnOnce(&mut Settings)) {
        let path = Settings::path(&self.fixture.data_dir);
        let Ok(Loaded::Current(mut settings)) = Settings::load(&path) else {
            panic!("settings load");
        };
        change(&mut settings);
        settings.save(&path).expect("save settings");
    }

    /// Runs `workload` as the agent of a new thread with `network`, through
    /// the supervisor binary and the real sandbox.
    fn run(&self, network: NetworkProfile, workload: &str) -> Output {
        self.run_titled(network, "workload", workload)
    }

    /// [`Self::run`] for a thread whose prompt starts with `title`.
    fn run_titled(&self, network: NetworkProfile, title: &str, workload: &str) -> Output {
        self.set_workload(workload);
        self.start(
            network,
            &format!("{title} {}", Timestamp::now().as_nanosecond()),
        )
    }

    /// Makes `workload` what the fake agent runs next.
    fn set_workload(&self, workload: &str) {
        let script = self
            .fixture
            .base
            .join(format!("workload-{}.sh", Timestamp::now().as_nanosecond()));
        std::fs::write(&script, format!("{PRELUDE}\n{workload}")).expect("write the workload");
        self.change_settings(|settings| {
            settings
                .agents
                .get_mut(ClaudeCode::ID)
                .expect("agent entry")
                .env
                .insert(
                    "GITEZ_WORKLOAD".to_owned(),
                    script.to_string_lossy().into_owned(),
                );
        });
    }

    /// Starts a thread for `prompt` with `network` through the supervisor
    /// binary and waits until it is done.
    fn start(&self, network: NetworkProfile, prompt: &str) -> Output {
        self.start_in_mode(Mode::Implement, network, prompt)
    }

    /// Runs `workload` as the agent of a new Discuss thread in the main
    /// worktree.
    fn discuss(&self, network: NetworkProfile, workload: &str) -> Output {
        self.set_workload(workload);
        self.start_in_mode(
            Mode::Discuss,
            network,
            &format!("discussion {}", Timestamp::now().as_nanosecond()),
        )
    }

    /// [`Self::start`] for a thread of `mode`.
    fn start_in_mode(&self, mode: Mode, network: NetworkProfile, prompt: &str) -> Output {
        let thread = self
            .fixture
            .store()
            .create(
                NewAgentThread {
                    prompt: prompt.to_owned(),
                    agent: ClaudeCode::ID.to_owned(),
                    model: Some("haiku".to_owned()),
                    mode,
                    start: Start {
                        branch: "main".to_owned(),
                        commit: self.fixture.head(),
                    },
                    work_dir: (mode == Mode::Discuss).then(|| self.fixture.root.clone()),
                    origin: None,
                    network,
                    secrets: Vec::new(),
                },
                &self.fixture.data_dir,
                "my repo",
                |_| false,
                Timestamp::now(),
            )
            .expect("create thread");
        let exe = self.supervisor_exe();
        let started = Instant::now();
        gitez_supervisor::launch(Launch {
            exe: &exe,
            prefix: &[],
            command: Command::Start,
            common_dir: &self.fixture.common_dir(),
            thread: &thread.id,
            secrets: &Secrets::default(),
        })
        .expect("launch the supervisor");
        let thread = self
            .fixture
            .wait_for(&thread.id, Duration::from_secs(900), is_done);
        let lines: Vec<String> = self
            .fixture
            .transcripts(&thread.id)
            .into_iter()
            .flat_map(|(_, lines)| lines)
            .filter_map(|line| line.strip_prefix("OUT ").map(str::to_owned))
            .collect();
        println!(
            "--- {} workload, {:.1} s:\n{}",
            network.label(),
            started.elapsed().as_secs_f64(),
            lines.join("\n")
        );
        Output { lines, thread }
    }

    /// The thread's caches folder.
    fn caches(&self, thread: &AgentThread) -> PathBuf {
        self.scratch(thread).caches
    }

    /// The thread's temporary and caches folders.
    fn scratch(&self, thread: &AgentThread) -> gitez_agent_thread::ScratchDirs {
        let key = worktree_of(thread)
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .expect("repository key");
        gitez_agent_thread::scratch_dirs(&self.fixture.data_dir, key, &thread.id)
    }

    /// The supervisor binary, started as GitEZ starts it from Finder: a
    /// launchd-like environment, the real sandbox.
    fn supervisor_exe(&self) -> PathBuf {
        let path = self.fixture.base.join("bin").join("supervise-real");
        if !path.exists() {
            let home = std::env::var("HOME").expect("HOME");
            let user = std::env::var("USER").unwrap_or_default();
            let tmp = std::env::temp_dir();
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\nexec /usr/bin/env -i HOME='{home}' USER='{user}' LOGNAME='{user}' \
                     SHELL=/bin/zsh LANG=en_US.UTF-8 TMPDIR='{}' PATH=/usr/bin:/bin:/usr/sbin:/sbin \
                     GITEZ_DATA_DIR='{}' {} '{}' \"$@\"\n",
                    tmp.display(),
                    self.fixture.data_dir.display(),
                    self.supervisor_env,
                    env!("CARGO_BIN_EXE_gitez-supervisor"),
                ),
            )
            .expect("write the launcher");
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .expect("chmod");
        }
        path
    }
}

/// How many crates a cargo command's output compiled, the project's own
/// (`demo`) left out.
fn compiled_dependencies(output: &str) -> usize {
    output
        .lines()
        .filter(|line| line.trim_start().starts_with("Compiling ") && !line.contains(" demo v"))
        .count()
}

/// The user's Cargo registry cache holds a `.crate` file for `name`.
fn in_user_registry(name: &str) -> bool {
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    std::fs::read_dir(home.join(".cargo/registry/cache"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|index| std::fs::read_dir(index.path()).ok())
        .flatten()
        .flatten()
        .any(|file| {
            file.file_name()
                .to_string_lossy()
                .starts_with(&format!("{name}-"))
        })
}

/// Agent API only: the user's Cargo cache, read-only, offline; the cloned
/// `target/` stays warm; a crate not on the machine fails at once with
/// Cargo's offline message.
#[test]
#[ignore = "runs the real sandbox, cargo and crates in ~/.cargo/registry"]
fn rust_builds_tests_and_lints_offline_with_a_warm_target() {
    if !have("cargo") {
        return;
    }
    let toolchains = Toolchains::with_project(RUST_PROJECT);
    let out = toolchains.run(
        NetworkProfile::AgentOnly,
        r#"
echo "CARGO_HOME=${CARGO_HOME-}"
echo "CARGO_NET_OFFLINE=${CARGO_NET_OFFLINE-}"
step build cargo build
step test cargo test
step clippy cargo clippy --all-targets
step fmt cargo fmt --check
step run cargo run
step add-cached cargo add itoa
step add-missing cargo add gitez-no-such-crate
git checkout --quiet -- Cargo.toml Cargo.lock
step commit sh -c 'echo x > x.txt && git add x.txt && git commit --quiet -m x'
"#,
    );
    assert_eq!(out.value("CARGO_HOME"), "", "the user's Cargo home");
    assert_eq!(out.value("CARGO_NET_OFFLINE"), "true");
    assert_eq!(
        compiled_dependencies(&out.ok("build")),
        0,
        "target/ is warm"
    );
    assert!(out.ok("test").contains("2 passed"));
    out.ok("clippy");
    out.ok("fmt");
    assert!(out.ok("run").contains("first number: 12"));
    out.ok("add-cached");
    let missing = out.failed("add-missing");
    assert!(missing.contains("could not be found"), "{missing}");
    out.ok("commit");
}

/// Packages and Open: a new crate is downloaded into the thread's own
/// Cargo home; the user's registry gets nothing.
#[test]
#[ignore = "runs the real sandbox and cargo, and downloads from crates.io"]
fn rust_downloads_a_new_crate_into_the_threads_own_cargo_home() {
    if !have("cargo") {
        return;
    }
    let toolchains = Toolchains::with_project(RUST_PROJECT);
    let user_had_humantime = in_user_registry("humantime");
    for profile in [NetworkProfile::Packages, NetworkProfile::Open] {
        let out = toolchains.run(
            profile,
            r#"
echo "CARGO_HOME=${CARGO_HOME-}"
step add cargo add humantime
step build cargo build
step test cargo test
"#,
        );
        let caches = toolchains.caches(&out.thread);
        assert_eq!(
            out.value("CARGO_HOME"),
            caches.join("cargo").to_string_lossy()
        );
        out.ok("add");
        assert!(out.ok("build").contains("humantime"));
        out.ok("test");
        let downloaded = std::fs::read_dir(caches.join("cargo/registry/cache"))
            .expect("the thread's registry")
            .flatten()
            .filter_map(|index| std::fs::read_dir(index.path()).ok())
            .flatten()
            .flatten()
            .any(|file| file.file_name().to_string_lossy().starts_with("humantime-"));
        assert!(downloaded, "{}: in the thread's registry", profile.label());
    }
    assert_eq!(
        in_user_registry("humantime"),
        user_had_humantime,
        "the user's registry is untouched"
    );
}

/// With the user's Cargo cache made writable in Settings, a thread uses it
/// directly: the target stays warm and the download lands there. Uses a
/// scratch Cargo home cloned from the user's, never the user's own.
#[test]
#[ignore = "runs the real sandbox and cargo, and downloads from crates.io"]
fn rust_shares_a_cargo_cache_made_writable_in_settings() {
    if !have("cargo") {
        return;
    }
    let scratch = tempfile::tempdir().expect("scratch Cargo home");
    let cargo_home = scratch.path().canonicalize().expect("canonical");
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    gitez_agent_thread::clone_tree(&home.join(".cargo/registry"), &cargo_home.join("registry"))
        .expect("clone the registry");
    let home_text = cargo_home.to_string_lossy().into_owned();
    // The scratch home reaches the setup through its script and the
    // supervisor (so the agent) through its launcher.
    let mut toolchains = Toolchains::new();
    toolchains.supervisor_env = format!("CARGO_HOME='{home_text}'");
    shell(
        &toolchains.fixture.root,
        &format!("export CARGO_HOME='{home_text}'\n{RUST_PROJECT}"),
    );
    git(&toolchains.fixture.root, &["add", "-A"]);
    git(
        &toolchains.fixture.root,
        &["commit", "--quiet", "-m", "Project"],
    );
    toolchains.change_settings(|settings| {
        settings.sandbox.writable_paths =
            vec![format!("{home_text}/registry"), format!("{home_text}/git")];
    });
    let out = toolchains.run(
        NetworkProfile::Packages,
        r#"
echo "CARGO_HOME=${CARGO_HOME-}"
step add cargo add humantime
step build cargo build
"#,
    );
    assert_eq!(out.value("CARGO_HOME"), home_text);
    out.ok("add");
    assert_eq!(compiled_dependencies(&out.ok("build")), 1, "only humantime");
    assert!(
        std::fs::read_dir(cargo_home.join("registry/cache"))
            .expect("registry cache")
            .flatten()
            .filter_map(|index| std::fs::read_dir(index.path()).ok())
            .flatten()
            .flatten()
            .any(|file| file.file_name().to_string_lossy().starts_with("humantime-"))
    );
}

/// npm: tests run on the cloned `node_modules/` without network; installs
/// need Packages and go to the thread's own cache; Node's own `fetch`
/// goes through the sandbox's proxy.
#[test]
#[ignore = "runs the real sandbox and npm, and downloads from the npm registry"]
fn node_tests_offline_and_installs_with_packages() {
    if !have("npm") {
        return;
    }
    let toolchains = Toolchains::with_project(NODE_PROJECT);
    let offline = toolchains.run(
        NetworkProfile::AgentOnly,
        r#"
step test npm test
step install-new npm install --no-fund left-pad
"#,
    );
    offline.ok("test");
    let refused = offline.failed("install-new");
    assert!(refused.contains("not on the allow list"), "{refused}");

    let packages = toolchains.run(
        NetworkProfile::Packages,
        r#"
echo "npm_config_cache=${npm_config_cache-}"
step install-new npm install --no-fund --no-audit left-pad
step ci npm ci --no-fund --no-audit
step test npm test
step fetch node -e "fetch('https://registry.npmjs.org/ms').then(r => { if (r.status !== 200) process.exit(1) }, () => process.exit(2))"
step yarn yarn --version
"#,
    );
    assert_eq!(
        packages.value("npm_config_cache"),
        toolchains
            .caches(&packages.thread)
            .join("npm")
            .to_string_lossy()
    );
    packages.ok("install-new");
    packages.ok("ci");
    packages.ok("test");
    packages.ok("fetch");
    if have("yarn") {
        packages.ok("yarn");
    }
}

/// Python: a virtual environment in the worktree, pip installs with
/// Packages (pip verifies certificates through macOS's trust service),
/// never into the user's site-packages.
#[test]
#[ignore = "runs the real sandbox and python3, and downloads from PyPI"]
fn python_installs_into_a_venv_with_packages() {
    if !have("python3") {
        return;
    }
    let toolchains = Toolchains::with_project(
        r#"cat > test_demo.py <<'T'
import unittest
import iniconfig
class T(unittest.TestCase):
    def test_add(self):
        self.assertEqual(1 + 1, 2)
T
printf '.venv/\n__pycache__/\n' > .gitignore
"#,
    );
    let out = toolchains.run(
        NetworkProfile::Packages,
        r#"
step venv python3 -m venv .venv
step install .venv/bin/pip install --disable-pip-version-check iniconfig
step test .venv/bin/python -m unittest
step user-site python3 -m pip install --user --break-system-packages iniconfig
"#,
    );
    out.ok("venv");
    out.ok("install");
    out.ok("test");
    let refused = out.failed("user-site");
    assert!(refused.contains("Operation not permitted"), "{refused}");
}

/// Go: builds and tests with the thread's own build cache, offline
/// without network, downloads with Packages.
#[test]
#[ignore = "runs the real sandbox and go, and downloads from the Go proxy"]
fn go_tests_offline_and_downloads_with_packages() {
    if !have("go") {
        return;
    }
    let toolchains = Toolchains::with_project(
        r#"cat > go.mod <<'T'
module example.com/demo

go 1.21
T
cat > demo.go <<'T'
package demo

func Double(x int) int { return 2 * x }
T
cat > demo_test.go <<'T'
package demo

import "testing"

func TestDouble(t *testing.T) {
	if Double(2) != 4 {
		t.Fatal("bad")
	}
}
T
"#,
    );
    let offline = toolchains.run(
        NetworkProfile::AgentOnly,
        r#"
step test go test ./...
step get go get golang.org/x/text@v0.20.0
"#,
    );
    offline.ok("test");
    assert!(offline.failed("get").contains("GOPROXY=off"));
    let packages = toolchains.run(
        NetworkProfile::Packages,
        r#"
step get go get golang.org/x/text@v0.20.0
step test go test ./...
"#,
    );
    packages.ok("get");
    packages.ok("test");
}

/// git beyond commit: everything on the thread's own branch works; stash
/// (a ref shared with every worktree), other refs, hooks, config, the main
/// worktree and pushing do not.
#[test]
#[ignore = "runs the real sandbox"]
fn git_works_on_its_own_branch_only() {
    let toolchains = Toolchains::new();
    let remote = toolchains.fixture.base.join("remote.git");
    git(
        &toolchains.fixture.base,
        &["init", "--quiet", "--bare", "remote.git"],
    );
    git(
        &toolchains.fixture.root,
        &["remote", "add", "origin", &remote.to_string_lossy()],
    );
    git(&toolchains.fixture.root, &["branch", "other"]);
    let other = toolchains.fixture.tip("other");
    let out = toolchains.run(
        NetworkProfile::AgentOnly,
        r#"
step status git status --short
echo change >> README.md
step diff git diff --stat
step log git log --oneline -3
step stash git stash
step commit git commit --quiet -am "Agent change"
step amend git commit --quiet --amend -m "Agent change, amended"
echo two >> README.md; git commit --quiet -am "Second"
start=$(git merge-base HEAD main)
step rebase git rebase --force-rebase "$start"
step worktree-list git worktree list
step move-other git update-ref refs/heads/other HEAD
step new-branch git branch sneaky
step tag git tag t1
common=$(git rev-parse --git-common-dir)
step hook sh -c "echo x > '$common/hooks/pre-commit'"
step config git config --local foo.bar 1
step push git push origin HEAD:refs/heads/pushed
step main-worktree sh -c "echo x > '$(dirname "$common")/escape.txt'"
"#,
    );
    for name in [
        "status",
        "diff",
        "log",
        "commit",
        "amend",
        "rebase",
        "worktree-list",
    ] {
        out.ok(name);
    }
    for name in [
        "stash",
        "move-other",
        "new-branch",
        "tag",
        "hook",
        "config",
        "push",
        "main-worktree",
    ] {
        out.failed(name);
    }
    assert_eq!(toolchains.fixture.tip("other"), other);
    assert!(!toolchains.fixture.root.join("escape.txt").exists());
    assert!(!remote.join("refs/heads/pushed").exists());
}

/// A Discuss thread runs in the user's working tree and reads it, with
/// what is uncommitted there; the sandbox refuses every write in the
/// repository (files, the index, commits, refs, hooks, config). Its own
/// `$TMPDIR` is writable, and in the agent's configuration folder only its
/// own conversation, not the rest of the folder it shares with the user's
/// sessions there.
#[test]
#[ignore = "runs the real sandbox"]
fn a_discussion_reads_the_working_tree_and_writes_nothing_in_it() {
    let toolchains = Toolchains::new();
    let root = &toolchains.fixture.root;
    // What the user has not committed.
    let readme = std::fs::read_to_string(root.join("README.md")).unwrap_or_default();
    let edited = format!("{readme}the user's uncommitted line\n");
    std::fs::write(root.join("README.md"), &edited).expect("edit");
    let status = git(root, &["status", "--porcelain"]);
    let refs = git(root, &["for-each-ref"]);
    let worktrees = git(root, &["worktree", "list", "--porcelain"]);
    let out = toolchains.discuss(
        NetworkProfile::AgentOnly,
        r#"
echo "cwd=$(pwd -P)"
step read grep -c "uncommitted line" README.md
step status git status --short
step diff git diff --stat
step log git log --oneline -3
step grep git grep -n hello
step new-file sh -c 'echo x > escape.txt'
step edit sh -c 'echo x >> README.md'
step delete rm README.md
step add git add -A
step commit git commit --quiet --allow-empty -m "sneaky"
step branch git branch sneaky
step tag git tag t1
step checkout git checkout --quiet -b elsewhere
step stash git stash
common=$(git rev-parse --git-common-dir)
step hook sh -c "echo x > '$common/hooks/pre-commit'"
step config git config --local foo.bar 1
step tmp sh -c 'echo note > "$TMPDIR/note.txt" && cat "$TMPDIR/note.txt"'
step system-tmp sh -c 'echo x > /tmp/gitez-discuss-test'
step home sh -c 'echo x > ~/gitez-discuss-test'
project=$(ls -d "$CLAUDE_CONFIG_DIR"/projects/*/ | head -n 1)
echo "project=$project"
step own-conversation sh -c "echo '{}' >> '${project}${GITEZ_TEST_SESSION}.jsonl'"
step own-folder mkdir -p "${project}${GITEZ_TEST_SESSION}/subagents"
step memory sh -c "mkdir -p '${project}memory' && echo x > '${project}memory/MEMORY.md'"
step other-conversation sh -c "echo '{}' > '${project}11111111-2222-3333-4444-555555555555.jsonl'"
"#,
    );
    let here = root.canonicalize().expect("canonical");
    assert_eq!(PathBuf::from(out.value("cwd")), here);
    assert_eq!(out.ok("read").trim(), "1");
    assert!(out.ok("status").contains("README.md"));
    for name in [
        "diff",
        "log",
        "grep",
        "tmp",
        "own-conversation",
        "own-folder",
    ] {
        out.ok(name);
    }
    for name in [
        "new-file",
        "edit",
        "delete",
        "add",
        "commit",
        "branch",
        "tag",
        "checkout",
        "stash",
        "hook",
        "config",
        "system-tmp",
        "home",
        "memory",
        "other-conversation",
    ] {
        out.failed(name);
    }
    assert!(
        !out.value("project").is_empty(),
        "the project folder is made"
    );
    // Nothing moved.
    assert_eq!(
        std::fs::read_to_string(root.join("README.md")).expect("read"),
        edited
    );
    assert!(!root.join("escape.txt").exists());
    assert_eq!(git(root, &["status", "--porcelain"]), status);
    assert_eq!(git(root, &["for-each-ref"]), refs);
    assert_eq!(git(root, &["worktree", "list", "--porcelain"]), worktrees);
    assert_eq!(out.thread.own(), None);
    assert_eq!(out.thread.ending, Some(gitez_agent_thread::Ending::Answer));
    assert_eq!(out.thread.pull_request, None);
}

/// Everyday tools: `find`, `sed -i`, temporary files in `$TMPDIR`. macOS
/// `mktemp` without a template uses the system temporary folder, which
/// stays read-only (the system prompt tells the agent); so do `/tmp`, the
/// home folder, `~/.cargo/bin` and shell startup files.
#[test]
#[ignore = "runs the real sandbox"]
fn everyday_tools_and_temporary_files() {
    let toolchains = Toolchains::new();
    let out = toolchains.run(
        NetworkProfile::AgentOnly,
        r#"
step find find . -name '*.md' -not -path './.git/*'
step sed-i sed -i '' 's/hello/hi/' README.md
step mktemp-template sh -c 'f=$(mktemp "$TMPDIR/x.XXXXXX") && rm "$f"'
step mktemp-dir sh -c 'd=$(mktemp -d "${TMPDIR%/}/d.XXXXXX") && rmdir "$d"'
step python-tempfile python3 -c 'import tempfile; tempfile.mkstemp()'
step mktemp-bare mktemp
step tmp sh -c 'echo x > /tmp/gitez-sandbox-test'
step home sh -c 'echo x > ~/gitez-sandbox-test'
step cargo-bin sh -c 'echo x > ~/.cargo/bin/gitez-sandbox-test'
step zshrc sh -c 'echo ": x" >> ~/.zshrc'
git checkout --quiet -- README.md
"#,
    );
    for name in [
        "find",
        "sed-i",
        "mktemp-template",
        "mktemp-dir",
        "python-tempfile",
    ] {
        out.ok(name);
    }
    for name in ["mktemp-bare", "tmp", "home", "cargo-bin", "zshrc"] {
        assert!(
            out.failed(name).contains("Operation not permitted"),
            "{name}"
        );
    }
}

/// srt keeps the Unix socket of its proxy in its own `TMPDIR`. Started
/// with the thread's temporary folder there, a long data folder and title
/// made the socket path longer than Node.js accepts and srt stopped with
/// `listen EINVAL` before the agent ran. srt now runs with a short private
/// folder; the agent still gets the thread's own `TMPDIR`.
#[test]
#[ignore = "runs the real sandbox"]
fn a_long_title_still_starts_the_sandbox() {
    let toolchains = Toolchains::new();
    let out = toolchains.run_titled(
        NetworkProfile::Packages,
        "Add the itoa crate and use it in a new function with a test",
        r#"
step tmpdir sh -c 'printf "%s\n" "$TMPDIR"'
step srt-files sh -c 'ls "$TMPDIR" | grep -c srt- || true'
"#,
    );
    let tmp = toolchains.scratch(&out.thread).tmp;
    let socket = tmp.join("srt-mux-99999-0.sock");
    if socket.as_os_str().len() <= 253 {
        println!(
            "note: {} bytes; run with a deeper TMPDIR to reproduce the old failure",
            socket.as_os_str().len()
        );
    }
    assert_eq!(
        out.ok("tmpdir").trim_end_matches('/'),
        tmp.to_string_lossy().trim_end_matches('/')
    );
    assert_eq!(out.ok("srt-files").trim(), "0", "srt's own files stay out");
}

fn network_from_env() -> NetworkProfile {
    match std::env::var("TOOLCHAIN_NETWORK").as_deref() {
        Ok("packages") => NetworkProfile::Packages,
        Ok("open") => NetworkProfile::Open,
        _ => NetworkProfile::AgentOnly,
    }
}

/// Exploration: `TOOLCHAIN_SETUP` (a script run in the main worktree, then
/// committed), `TOOLCHAIN_WORKLOAD` (a script run as the agent),
/// `TOOLCHAIN_NETWORK` (`agent_only`, `packages`, `open`),
/// `TOOLCHAIN_HIDDEN` and `TOOLCHAIN_WRITABLE` (paths, `:`-separated, added
/// to Settings), `TOOLCHAIN_SUPERVISOR_ENV` (`NAME='value'` for the
/// supervisor) and `TOOLCHAIN_KEEP` (keep the folders).
///
/// `TOOLCHAIN_REAL_PROMPT` instead runs the user's real `claude` (its own
/// configuration folder and login, model haiku) with that prompt: only by
/// hand, in a throwaway repository, never as part of a test run.
#[test]
#[ignore = "runs the real sandbox and toolchains"]
fn explore() {
    let toolchains = match std::env::var("TOOLCHAIN_SETUP") {
        Ok(setup) => Toolchains::with_project(&std::fs::read_to_string(setup).expect("read setup")),
        Err(_) => Toolchains::new(),
    };
    toolchains.change_settings(|settings| {
        if let Ok(hidden) = std::env::var("TOOLCHAIN_HIDDEN") {
            settings.sandbox.hidden_paths.push(hidden);
        }
        if let Ok(writable) = std::env::var("TOOLCHAIN_WRITABLE") {
            settings
                .sandbox
                .writable_paths
                .extend(writable.split(':').map(str::to_owned));
        }
    });
    let out = if let Ok(prompt) = std::env::var("TOOLCHAIN_REAL_PROMPT") {
        toolchains.change_settings(|settings| {
            settings.agents.remove(ClaudeCode::ID);
        });
        toolchains.start(network_from_env(), &prompt)
    } else {
        let workload = std::fs::read_to_string(
            std::env::var("TOOLCHAIN_WORKLOAD").expect("set TOOLCHAIN_WORKLOAD"),
        )
        .expect("read workload");
        toolchains.run(network_from_env(), &workload)
    };
    let thread = &out.thread;
    println!("--- thread: {:?} {:?}", thread.state, thread.ending);
    println!("warnings: {:?}", thread.warnings);
    if let Some(result) = thread.turns.last().and_then(|turn| turn.result.as_ref()) {
        println!("blocked hosts: {:?}", result.blocked_hosts);
    }
    println!("worktree: {}", worktree_of(thread).display());
    println!(
        "thread folder: {}",
        toolchains.fixture.thread_dir(&thread.id).display()
    );
    if let Some(message) = thread.final_message() {
        println!("final message: {message}");
    }
    if std::env::var_os("TOOLCHAIN_KEEP").is_some() {
        let keep = toolchains.fixture.base.clone();
        std::mem::forget(toolchains);
        println!("kept {}", keep.display());
    }
}
