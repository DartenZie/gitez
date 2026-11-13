//! Tests of the srt sandbox. The fast ones need only `bash` and `awk`: they
//! use a stand-in for Node.js that behaves like srt's command line (prints
//! srt's debug lines, then runs the command after `--`).
//!
//! The tests marked `#[ignore]` run the real srt and Seatbelt. Install srt
//! into a scratch data folder first, then run them with that folder:
//!
//! ```sh
//! GITEZ_SANDBOX_TEST_DATA_DIR=/path/to/scratch/data \
//!   cargo test -p gitez-sandbox -- --ignored --test-threads=1
//! ```
//!
//! `real_install` installs srt into that folder (it needs Node.js, npm and
//! the network); the others need it installed.

use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use super::*;

fn policy(write: Vec<WritePath>, network: Network) -> Policy {
    Policy {
        write,
        deny_write: Vec::new(),
        deny_read: Vec::new(),
        network,
        certificate_service: false,
    }
}

fn spec(program: &str, args: &[&str], cwd: &Path) -> CommandSpec {
    CommandSpec {
        program: program.into(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        env: Vec::new(),
        env_remove: Vec::new(),
        cwd: cwd.to_path_buf(),
        stdin: None,
    }
}

/// Starts `spec` as the supervisor does: in its own process group, stdout
/// and stderr piped, stdin written then closed.
fn spawn(spec: &CommandSpec) -> std::process::Child {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    for name in &spec.env_remove {
        command.env_remove(name);
    }
    command.envs(spec.env.iter().map(|(key, value)| (key, value)));
    let mut child = command.spawn().expect("the wrapped command starts");
    if let Some(input) = &spec.stdin {
        let mut stdin = child.stdin.take().expect("stdin is piped");
        stdin.write_all(input.as_bytes()).expect("stdin is written");
    }
    child
}

fn run(spec: &CommandSpec) -> Output {
    spawn(spec)
        .wait_with_output()
        .expect("the wrapped command ends")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn group(child: &std::process::Child) -> rustix::process::Pid {
    rustix::process::Pid::from_raw(child.id() as i32).expect("a child has a pid")
}

/// A data folder with a stand-in srt installed, and a folder holding a
/// stand-in `node` that acts as srt's command line.
struct Fake {
    data: tempfile::TempDir,
    bin: tempfile::TempDir,
}

impl Fake {
    fn new() -> Self {
        let data = tempfile::tempdir().expect("a temporary folder");
        let package = Srt::install_dir(data.path()).join(PACKAGE_DIR);
        std::fs::create_dir_all(package.join("dist")).expect("the package folder");
        std::fs::write(package.join(CLI), "// srt\n").expect("the cli");
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"@anthropic-ai/sandbox-runtime","version":"0.0.82"}"#,
        )
        .expect("the manifest");
        let bin = tempfile::tempdir().expect("a temporary folder");
        let node = bin.path().join("node");
        std::fs::write(
            &node,
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo v26.0.0; exit 0; fi
while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do shift; done
shift
echo '[SandboxDebug] Initializing sandbox...' >&2
echo '[SandboxDebug] {' >&2
echo '  "allowedHosts": []' >&2
echo '}' >&2
echo '[SandboxDebug] No matching rule, denying: example.com:443' >&2
echo '[SandboxDebug] Connection blocked to example.com:443' >&2
echo '[SandboxDebug] Connection blocked to [::1]:8080' >&2
echo 'Error: from srt itself' >&2
printf '[SandboxDebug] Original command: %s\n' "$*" >&2
exec "$@"
"#,
        )
        .expect("the stand-in node");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755))
            .expect("the stand-in node runs");
        Self { data, bin }
    }

    fn srt(&self) -> Srt {
        Srt::with_search(self.data.path(), Search::only(self.bin.path()))
    }
}

#[test]
fn macos_settings_tell_trees_from_files_and_keep_patterns() {
    let mut policy = policy(
        vec![
            WritePath::Tree("/w/wt".into()),
            WritePath::Tree("/Users/u/Library/Application Support/GitEZ/tmp/r-1".into()),
            WritePath::File("/w/repo/.git/refs/heads/agent/x".into()),
            WritePath::File("/w/repo/.git/refs/heads/agent/x.lock".into()),
            WritePath::Tree("/private/tmp/claude-501/fswatch-probe-**".into()),
            WritePath::Tree("/private/tmp/claude-*-cwd".into()),
            WritePath::Tree("/w/[WIP] project".into()),
        ],
        Network::Hosts(vec!["api.anthropic.com".into(), "*.crates.io".into()]),
    );
    policy.deny_write = vec!["/w/repo/.git/hooks".into(), "/w/repo/.git/config".into()];
    policy.deny_read = vec![
        "/Users/u/.ssh".into(),
        "/Users/u/.docker/config.json".into(),
        "/Users/u/.no-such-folder".into(),
    ];
    let hosts = ["api.anthropic.com".to_owned(), "*.crates.io".to_owned()];
    let settings = settings(&policy, &hosts, Os::Mac);
    assert_eq!(
        settings,
        json!({
            "network": {
                "allowedDomains": ["api.anthropic.com", "*.crates.io"],
                "deniedDomains": [],
            },
            "filesystem": {
                "denyRead": [
                    "/Users/u/.ssh",
                    "/Users/u/.docker/config.json",
                    "/Users/u/.no-such-folder",
                ],
                "allowWrite": [
                    "/w/wt",
                    "/Users/u/Library/Application Support/GitEZ/tmp/r-1",
                    { "path": "/w/repo/.git/refs/heads/agent/x", "literal": true },
                    { "path": "/w/repo/.git/refs/heads/agent/x.lock", "literal": true },
                    "/private/tmp/claude-501/fswatch-probe-**",
                    "/private/tmp/claude-*-cwd",
                    { "path": "/w/[WIP] project", "literal": true },
                ],
                "denyWrite": ["/w/repo/.git/hooks", "/w/repo/.git/config"],
            },
        })
    );
}

#[test]
fn the_certificate_service_is_opened_only_when_asked_and_only_on_macos() {
    let mut policy = policy(Vec::new(), Network::Hosts(vec!["pypi.org".into()]));
    let closed = settings(&policy, &["pypi.org".to_owned()], Os::Mac);
    assert!(closed.get("enableWeakerNetworkIsolation").is_none());
    policy.certificate_service = true;
    let open = settings(&policy, &["pypi.org".to_owned()], Os::Mac);
    assert_eq!(open["enableWeakerNetworkIsolation"], json!(true));
    let linux = settings(&policy, &["pypi.org".to_owned()], Os::Linux);
    assert!(linux.get("enableWeakerNetworkIsolation").is_none());
}

#[test]
fn no_hosts_is_no_network() {
    let settings = settings(
        &policy(Vec::new(), Network::Hosts(Vec::new())),
        &[],
        Os::Mac,
    );
    assert_eq!(settings["network"]["allowedDomains"], json!([]));
}

#[test]
fn linux_settings_write_through_folders_without_patterns() {
    let policy = policy(
        vec![
            WritePath::Tree("/w/wt".into()),
            WritePath::File("/w/repo/.git/refs/heads/agent/x".into()),
            WritePath::File("/w/repo/.git/refs/heads/agent/x.lock".into()),
            WritePath::File("/w/repo/.git/packed-refs.lock".into()),
            WritePath::Tree("/tmp/claude-*-cwd".into()),
        ],
        Network::Hosts(Vec::new()),
    );
    let settings = settings(&policy, &[], Os::Linux);
    assert_eq!(
        settings["filesystem"]["allowWrite"],
        json!(["/w/wt", "/w/repo/.git/refs/heads/agent", "/w/repo/.git"])
    );
}

#[test]
fn without_srt_anywhere_it_is_missing() {
    let data = tempfile::tempdir().expect("a temporary folder");
    let srt = Srt::with_search(data.path(), Search::only(""));
    assert_eq!(
        srt.status(),
        Status::Missing {
            fix: NO_NODE.to_owned()
        }
    );
    // With the usual install places, Node.js may be found; srt is not.
    let srt = Srt::with_search(data.path(), Search::with_usual_places(""));
    assert!(matches!(srt.status(), Status::Missing { .. }));
    let command = spec("/bin/echo", &[], data.path());
    let hosts = policy(Vec::new(), Network::Hosts(Vec::new()));
    assert!(matches!(
        srt.wrap(command, &hosts),
        Err(Error::Unavailable(_))
    ));
}

/// Installing is offered only when Node.js is new enough and srt is
/// nowhere; installed, without Node.js or with an old one, it is not.
#[cfg(target_os = "macos")]
#[test]
fn install_is_offered_only_when_it_is_the_fix() {
    use std::os::unix::fs::PermissionsExt;
    let node_of = |version: &str| {
        let bin = tempfile::tempdir().expect("a temporary folder");
        let node = bin.path().join("node");
        std::fs::write(&node, format!("#!/bin/sh\necho {version}\n")).expect("node");
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        bin
    };
    let data = tempfile::tempdir().expect("a temporary folder");

    let new_node = node_of("v22.12.0");
    let srt = Srt::with_search(data.path(), Search::only(new_node.path()));
    assert_eq!(
        srt.status(),
        Status::Missing {
            fix: NOT_INSTALLED.to_owned()
        }
    );
    assert!(srt.installable());

    let old_node = node_of("v20.1.0");
    let srt = Srt::with_search(data.path(), Search::only(old_node.path()));
    let Status::Missing { fix } = srt.status() else {
        panic!("an old Node.js cannot run srt");
    };
    assert!(fix.contains("22.12 or newer"), "{fix}");
    assert!(!srt.installable());

    let srt = Srt::with_search(data.path(), Search::only(""));
    assert!(!srt.installable(), "no Node.js");

    let fake = Fake::new();
    assert!(!fake.srt().installable(), "already installed");
    assert!(!crate::Passthrough.installable());
    assert!(matches!(
        crate::Passthrough.install(),
        Err(Error::Install(_))
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn an_installed_srt_is_ready() {
    let fake = Fake::new();
    let Status::Ready { program, version } = fake.srt().status() else {
        panic!("srt is installed");
    };
    assert!(program.ends_with("node_modules/@anthropic-ai/sandbox-runtime/dist/cli.js"));
    assert_eq!(version.as_deref(), Some("0.0.82"));
}

#[cfg(target_os = "macos")]
#[test]
fn wrap_keeps_env_cwd_and_stdin() {
    let fake = Fake::new();
    let cwd = tempfile::tempdir().expect("a temporary folder");
    let tmp = cwd.path().join("thread tmp");
    let mut command = spec(
        "/usr/bin/printf",
        &["%s|", "a b", "--settings", "-c"],
        cwd.path(),
    );
    command.env = vec![
        ("SECRET".into(), "s".into()),
        ("TMPDIR".into(), tmp.to_string_lossy().into_owned()),
    ];
    command.env_remove = vec!["CLAUDECODE".into()];
    command.stdin = Some("the prompt".into());
    let hosts = policy(Vec::new(), Network::Hosts(Vec::new()));
    let wrapped = fake.srt().wrap(command.clone(), &hosts).expect("it wraps");
    let out = &wrapped.command;
    assert_eq!(out.program, Path::new("/bin/bash"));
    assert_eq!(out.cwd, command.cwd);
    assert_eq!(out.stdin, command.stdin);
    assert_eq!(out.env[..2], command.env[..]);
    assert!(out.env.contains(&(
        "CLAUDE_CODE_TMPDIR".to_owned(),
        tmp.to_string_lossy().into_owned()
    )));
    // Node.js's own clients use srt's proxy.
    assert!(
        out.env
            .contains(&("NODE_USE_ENV_PROXY".to_owned(), "1".to_owned()))
    );
    assert!(out.env_remove.starts_with(&command.env_remove));
    assert!(tmp.is_dir(), "the thread's temporary folder is created");
    let dashes = out.args.iter().position(|arg| arg == "--").expect("a --");
    // srt itself keeps its sockets in the private, short run folder, not in
    // the command's temporary folder, which the sandbox can write.
    let srt_tmp = out
        .env
        .iter()
        .rev()
        .find(|(name, _)| name == "TMPDIR")
        .map(|(_, value)| PathBuf::from(value))
        .expect("srt's TMPDIR");
    assert_eq!(
        Path::new(&out.args[dashes - 1]).parent(),
        Some(srt_tmp.as_path())
    );
    assert!(srt_tmp.as_os_str().len() <= SHORT_BASE_MAX + "/gitez-sandbox-XXXXXX".len());
    assert_eq!(
        out.args[dashes + 1..],
        [
            "/usr/bin/env",
            "-u",
            "SRT_DEBUG",
            "/usr/bin/printf",
            "%s|",
            "a b",
            "--settings",
            "-c"
        ]
    );
    let settings = out.args[dashes - 1].clone();
    let written: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).expect("settings exist"))
            .expect("settings are JSON");
    assert_eq!(written["network"]["allowedDomains"], json!([]));

    // The arguments reach the command unchanged; the filter passes on the
    // command's output and srt's own errors, and keeps the refused hosts.
    let output = run(out);
    assert!(output.status.success());
    assert_eq!(text(&output.stdout), "a b|--settings|-c|");
    assert_eq!(text(&output.stderr), "Error: from srt itself\n");
    assert_eq!(wrapped.blocked_hosts(), ["example.com", "::1"]);
    drop(wrapped);
    assert!(
        !Path::new(&settings).exists(),
        "the guard removes the run's files"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn stdin_and_stderr_pass_through() {
    let fake = Fake::new();
    let cwd = tempfile::tempdir().expect("a temporary folder");
    let mut command = spec(
        "/bin/sh",
        &[
            "-c",
            "cat\necho '[SandboxDebug] from the agent?' >&2\n\necho agent-error >&2",
        ],
        cwd.path(),
    );
    command.stdin = Some("line1\nline2\n".into());
    let hosts = policy(Vec::new(), Network::Hosts(Vec::new()));
    let wrapped = fake.srt().wrap(command, &hosts).expect("it wraps");
    let output = run(&wrapped.command);
    assert_eq!(text(&output.stdout), "line1\nline2\n");
    // A line the agent writes in srt's debug format is taken for srt's.
    assert_eq!(
        text(&output.stderr),
        "Error: from srt itself\nagent-error\n"
    );
}

/// A script that counts the SIGINTs it gets, then exits.
const TRAPPING: &str = r#"n=0
trap 'n=$((n+1))' INT
echo ready
i=0
while [ $i -lt 50 ] && [ $n -eq 0 ]; do sleep 0.1; i=$((i+1)); done
sleep 0.3
echo "interrupted $n" >&2
exit 0"#;

#[cfg(target_os = "macos")]
#[test]
fn sigint_to_the_group_reaches_a_trapping_script() {
    let fake = Fake::new();
    let cwd = tempfile::tempdir().expect("a temporary folder");
    let hosts = policy(Vec::new(), Network::Hosts(Vec::new()));
    let wrapped = fake
        .srt()
        .wrap(spec("/bin/bash", &["-c", TRAPPING], cwd.path()), &hosts)
        .expect("it wraps");
    interrupt_after_ready(&wrapped.command);
}

/// Runs `spec`, sends SIGINT to its group once it printed `ready`, and
/// checks its trap ran (it reports so on stderr, through the filter, which
/// must survive the signal) and it exited in time. Signals of one kind that
/// arrive together count once, so a trap sees srt's forwarded SIGINT only
/// when it comes after the first was handled.
fn interrupt_after_ready(spec: &CommandSpec) -> Output {
    let mut child = spawn(spec);
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut seen = Vec::new();
    let mut byte = [0u8; 1];
    while !text(&seen).contains("ready") {
        let read = std::io::Read::read(&mut stdout, &mut byte).expect("stdout is readable");
        assert!(read > 0, "the command printed ready: {}", text(&seen));
        seen.push(byte[0]);
    }
    let started = Instant::now();
    rustix::process::kill_process_group(group(&child), rustix::process::Signal::INT)
        .expect("SIGINT is sent");
    let output = child.wait_with_output().expect("it ends");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "it stopped in time"
    );
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("interrupted ") && !stderr.contains("interrupted 0"),
        "the trap ran: {stderr:?}"
    );
    output
}

#[cfg(target_os = "macos")]
#[test]
fn killing_the_group_kills_every_process() {
    let fake = Fake::new();
    let cwd = tempfile::tempdir().expect("a temporary folder");
    let hosts = policy(Vec::new(), Network::Hosts(Vec::new()));
    let wrapped = fake
        .srt()
        .wrap(
            spec("/bin/sh", &["-c", "sleep 30 & sleep 30; wait"], cwd.path()),
            &hosts,
        )
        .expect("it wraps");
    hard_stop(&wrapped.command);
}

/// Starts `spec`, kills its process group and checks nothing is left.
fn hard_stop(spec: &CommandSpec) {
    let mut child = spawn(spec);
    std::thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    rustix::process::kill_process_group(group(&child), rustix::process::Signal::KILL)
        .expect("SIGKILL is sent");
    child.wait().expect("it ends");
    assert!(started.elapsed() < Duration::from_secs(2));
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        rustix::process::test_kill_process_group(group(&child)).is_err(),
        "no process is left in the group"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn open_runs_under_a_seatbelt_profile_without_srt() {
    let data = tempfile::tempdir().expect("a temporary folder");
    let srt = Srt::with_search(data.path(), Search::only(""));
    let cwd = tempfile::tempdir().expect("a temporary folder");
    let mut command = spec("/bin/echo", &["hi"], cwd.path());
    command.env = vec![("SECRET".into(), "s".into())];
    command.stdin = Some("input".into());
    let open = policy(vec![WritePath::Tree(cwd.path().into())], Network::Open);
    let wrapped = srt.wrap(command.clone(), &open).expect("it wraps");
    let out = &wrapped.command;
    assert_eq!(out.program, Path::new("/usr/bin/sandbox-exec"));
    assert_eq!(out.args[0], "-f");
    assert_eq!(out.args[2..], ["/bin/echo", "hi"]);
    assert_eq!((&out.cwd, &out.stdin), (&command.cwd, &command.stdin));
    assert_eq!(out.env[0], command.env[0]);
    assert!(
        out.env
            .contains(&("TMPDIR".to_owned(), SHARED_TMP.to_owned()))
    );
    assert!(out.env_remove.contains(&"CLAUDE_CODE_TMPDIR".to_owned()));
    let profile = std::fs::read_to_string(&out.args[1]).expect("the profile exists");
    assert!(profile.contains("(allow network*)"));
    assert!(wrapped.blocked_hosts().is_empty());
}

#[test]
fn ports_are_dropped_from_refused_hosts() {
    assert_eq!(without_port("example.com:443"), "example.com");
    assert_eq!(without_port("[::1]:8080"), "::1");
    assert_eq!(without_port("example.com"), "example.com");
    assert_eq!(without_port("fe80::1"), "fe80::1");
}

// ---------------------------------------------------------------------------
// The real sandbox (`#[ignore]`; see the module documentation).

/// The data folder with srt installed in it.
fn real_data_dir() -> PathBuf {
    std::env::var_os("GITEZ_SANDBOX_TEST_DATA_DIR")
        .map(PathBuf::from)
        .expect("set GITEZ_SANDBOX_TEST_DATA_DIR to a scratch data folder")
}

fn real_srt() -> Srt {
    let srt = Srt::new(&real_data_dir());
    assert!(
        matches!(srt.status(), Status::Ready { .. }),
        "srt is installed: {:?}",
        srt.status()
    );
    srt
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        text(&output.stderr)
    );
    text(&output.stdout).trim().to_owned()
}

/// A repository with one commit and a linked worktree on `agent/t`, and the
/// policy D.7 gives an agent working in it, with a private temporary folder
/// and a read-denied folder and file (and paths that do not exist).
struct Repo {
    root: tempfile::TempDir,
    main: PathBuf,
    worktree: PathBuf,
    common: PathBuf,
    secret_dir: PathBuf,
    secret_file: PathBuf,
    tmp: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("a temporary folder");
        // Paths under GitEZ's data folder have a space in them.
        let base = root
            .path()
            .canonicalize()
            .expect("it exists")
            .join("Application Support");
        let main = base.join("repo");
        std::fs::create_dir_all(&main).expect("the repository folder");
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["config", "user.email", "t@example.com"]);
        git(&main, &["config", "user.name", "T"]);
        git(&main, &["config", "commit.gpgsign", "false"]);
        std::fs::write(main.join("a.txt"), "a\n").expect("a file");
        git(&main, &["add", "a.txt"]);
        git(&main, &["commit", "-q", "-m", "first"]);
        let worktree = base.join("worktrees").join("t");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "agent/t",
                &worktree.to_string_lossy(),
            ],
        );
        let secret_dir = base.join("home/.ssh");
        std::fs::create_dir_all(&secret_dir).expect("a secret folder");
        std::fs::write(secret_dir.join("id"), "not a real key\n").expect("a secret");
        let secret_file = base.join("home/.netrc");
        std::fs::write(&secret_file, "machine x password y\n").expect("a secret file");
        let tmp = base.join("tmp/thread-1");
        Self {
            common: main.join(".git"),
            root,
            main,
            worktree,
            secret_dir,
            secret_file,
            tmp,
        }
    }

    fn policy(&self, network: Network) -> Policy {
        let common = &self.common;
        let branch = "agent/t";
        Policy {
            write: vec![
                WritePath::Tree(self.worktree.clone()),
                WritePath::Tree(common.join("objects")),
                WritePath::Tree(common.join("worktrees/t")),
                WritePath::File(common.join(format!("refs/heads/{branch}"))),
                WritePath::File(common.join(format!("refs/heads/{branch}.lock"))),
                WritePath::Tree(common.join(format!("logs/refs/heads/{branch}"))),
                WritePath::File(common.join("packed-refs")),
                WritePath::File(common.join("packed-refs.lock")),
                WritePath::Tree(self.tmp.clone()),
                WritePath::Tree("/private/tmp/gitez-sandbox-test-*".into()),
            ],
            deny_write: vec![common.join("hooks"), common.join("config")],
            deny_read: vec![
                self.secret_dir.clone(),
                self.secret_file.clone(),
                self.root.path().join("home/.aws"),
                self.root.path().join("home/.docker/config.json"),
            ],
            network,
            certificate_service: false,
        }
    }

    fn command(&self, script: &str) -> CommandSpec {
        let mut command = spec("/bin/sh", &["-c", script], &self.worktree);
        command.env = vec![
            ("TMPDIR".to_owned(), self.tmp.to_string_lossy().into_owned()),
            ("GIT_CONFIG_COUNT".to_owned(), "1".to_owned()),
            ("GIT_CONFIG_KEY_0".to_owned(), "commit.gpgsign".to_owned()),
            ("GIT_CONFIG_VALUE_0".to_owned(), "false".to_owned()),
        ];
        command
    }
}

/// Every file rule, with the given network: writes outside the policy are
/// refused, git commit in the worktree works and moves only its branch,
/// denied files and folders cannot be read, the temporary folder is the
/// thread's own.
fn check_file_rules(srt: &Srt, network: Network) {
    let repo = Repo::new();
    let home = std::env::var("HOME").expect("HOME is set");
    let probe = PathBuf::from(&home).join(format!("gitez-sandbox-probe-{}", std::process::id()));
    let main_before = git(&repo.main, &["rev-parse", "main"]);
    let script = format!(
        r#"
echo hi > '{probe}' && echo "home: written" || echo "home: refused"
echo hi > '{main}/x' && echo "main worktree: written" || echo "main worktree: refused"
echo hi > x && echo "worktree: written" || echo "worktree: refused"
git add x && git commit -q -m agent && echo "commit: ok" || echo "commit: failed"
echo x > '{common}/hooks/pre-commit' && echo "hooks: written" || echo "hooks: refused"
git config --local foo.bar 1 2>/dev/null && echo "config: written" || echo "config: refused"
git branch other 2>/dev/null && echo "branch: created" || echo "branch: refused"
cat '{secret_dir}/id' >/dev/null 2>&1 && echo "secret folder: read" || echo "secret folder: refused"
ls '{secret_dir}' >/dev/null 2>&1 && echo "secret listing: read" || echo "secret listing: refused"
cat '{secret_file}' >/dev/null 2>&1 && echo "secret file: read" || echo "secret file: refused"
echo "tmpdir: $TMPDIR"
touch "$TMPDIR/t" && echo "tmp: written" || echo "tmp: refused"
touch /private/tmp/gitez-sandbox-test-$$ && rm /private/tmp/gitez-sandbox-test-$$ && echo "pattern: written" || echo "pattern: refused"
"#,
        probe = probe.display(),
        main = repo.main.display(),
        common = repo.common.display(),
        secret_dir = repo.secret_dir.display(),
        secret_file = repo.secret_file.display(),
    );
    let wrapped = srt
        .wrap(repo.command(&script), &repo.policy(network))
        .expect("it wraps");
    let output = run(&wrapped.command);
    let stdout = text(&output.stdout);
    let _ = std::fs::remove_file(&probe);
    eprintln!("{stdout}{}", text(&output.stderr));
    for line in [
        "home: refused",
        "main worktree: refused",
        "worktree: written",
        "commit: ok",
        "hooks: refused",
        "config: refused",
        "branch: refused",
        "secret folder: refused",
        "secret listing: refused",
        "secret file: refused",
        &format!("tmpdir: {}", repo.tmp.display()),
        "tmp: written",
        "pattern: written",
    ] {
        assert!(stdout.contains(line), "expected {line:?} in:\n{stdout}");
    }
    assert_eq!(git(&repo.main, &["rev-parse", "main"]), main_before);
    assert_eq!(
        git(&repo.main, &["log", "-1", "--format=%s", "agent/t"]),
        "agent"
    );
}

#[test]
#[ignore = "installs srt with npm into GITEZ_SANDBOX_TEST_DATA_DIR"]
fn real_install() {
    let cli = Srt::install(&real_data_dir()).expect("srt installs");
    assert!(cli.ends_with("dist/cli.js"));
    let Status::Ready { version, .. } = Srt::new(&real_data_dir()).status() else {
        panic!("srt is ready after the install");
    };
    assert_eq!(version.as_deref(), Some("0.0.82"));
}

#[test]
#[ignore = "needs srt installed in GITEZ_SANDBOX_TEST_DATA_DIR"]
fn real_srt_file_rules() {
    check_file_rules(&real_srt(), Network::Hosts(Vec::new()));
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "runs sandbox-exec"]
fn real_open_file_rules() {
    let data = tempfile::tempdir().expect("a temporary folder");
    check_file_rules(&Srt::new(data.path()), Network::Open);
}

/// `curl https://example.com` under `network`: whether it got a response.
fn curl(srt: &Srt, network: Network) -> (bool, Vec<String>) {
    let cwd = tempfile::tempdir().expect("a temporary folder");
    let command = spec(
        "/bin/sh",
        &[
            "-c",
            "curl -sS -m 10 -o /dev/null -w '%{http_code}' https://example.com",
        ],
        cwd.path(),
    );
    let wrapped = srt
        .wrap(command, &policy(Vec::new(), network))
        .expect("it wraps");
    let output = run(&wrapped.command);
    eprintln!("curl: {} {}", text(&output.stdout), text(&output.stderr));
    (text(&output.stdout) == "200", wrapped.blocked_hosts())
}

#[test]
#[ignore = "needs srt installed in GITEZ_SANDBOX_TEST_DATA_DIR and the network"]
fn real_network_profiles() {
    let srt = real_srt();
    assert_eq!(
        curl(&srt, Network::Hosts(Vec::new())),
        (false, vec!["example.com".to_owned()])
    );
    let (allowed, blocked) = curl(&srt, Network::Hosts(vec!["example.com".to_owned()]));
    assert!(allowed && blocked.is_empty());
    let (allowed, blocked) = curl(&srt, Network::Open);
    assert!(allowed && blocked.is_empty());
}

#[test]
#[ignore = "needs srt installed in GITEZ_SANDBOX_TEST_DATA_DIR"]
fn real_srt_signals() {
    let srt = real_srt();
    let cwd = tempfile::tempdir().expect("a temporary folder");
    let hosts = policy(Vec::new(), Network::Hosts(Vec::new()));
    // srt forwards SIGINT to the command, which also gets it from the
    // group: a trapping script sees it once or twice.
    let wrapped = srt
        .wrap(spec("/bin/bash", &["-c", TRAPPING], cwd.path()), &hosts)
        .expect("it wraps");
    let output = interrupt_after_ready(&wrapped.command);
    assert_eq!(output.status.code(), Some(0));
    let wrapped = srt
        .wrap(
            spec("/bin/sh", &["-c", "sleep 30 & sleep 30; wait"], cwd.path()),
            &hosts,
        )
        .expect("it wraps");
    hard_stop(&wrapped.command);
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "runs sandbox-exec"]
fn real_open_signals() {
    let data = tempfile::tempdir().expect("a temporary folder");
    let srt = Srt::new(data.path());
    let cwd = tempfile::tempdir().expect("a temporary folder");
    let open = policy(Vec::new(), Network::Open);
    let wrapped = srt
        .wrap(spec("/bin/bash", &["-c", TRAPPING], cwd.path()), &open)
        .expect("it wraps");
    interrupt_after_ready(&wrapped.command);
    let wrapped = srt
        .wrap(
            spec("/bin/sh", &["-c", "sleep 30 & sleep 30; wait"], cwd.path()),
            &open,
        )
        .expect("it wraps");
    hard_stop(&wrapped.command);
}

#[test]
fn a_long_temporary_folder_gives_way_to_tmp() {
    let short = PathBuf::from("/var/folders/ab/cdefgh_ijklmn/T");
    assert_eq!(short_base(short.clone()), short);
    let long = PathBuf::from(format!("/Users/someone/{}", "deep/".repeat(20)));
    assert_eq!(short_base(long), Path::new("/tmp"));
}
