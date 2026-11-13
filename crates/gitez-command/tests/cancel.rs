//! A git that never finishes: pulling where the branch is checked out,
//! cancelling, and reading what git wrote while it still runs.
//!
//! No test contacts a remote. `origin` is an `ssh://` URL, and
//! `GIT_SSH_COMMAND` is a script that ignores the host and runs the remote
//! command on this computer.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gitez_command::{
    CANCEL_GRACE, Error, Git, MergeMethod, Operation, STALLED_AFTER, WorktreeProblem,
    blocked_by_changes, not_fast_forward,
};

/// Stands in for ssh. Like the ssh the hang was seen with, it does not
/// leave on SIGTERM; it leaves once the remote command has ended.
const SSH: &str = "#!/bin/sh
trap '' TERM          # like the real ssh here: does not leave on SIGTERM
for last; do :; done
sh -c \"$last\"
";

/// The same, but it stays after the remote command has ended, like an ssh
/// that keeps its connection open. Only SIGKILL ends it.
const SSH_THAT_STAYS: &str = "#!/bin/sh
trap '' TERM
echo $$ > \"$0.pid\"
for last; do :; done
sh -c \"$last\"
sleep 600
";

/// Longer than any test should take; a test that is still waiting then has
/// hung.
const HUNG: Duration = Duration::from_secs(60);

/// How much later than expected something may happen on a busy computer.
const SLACK: Duration = Duration::from_millis(1500);

/// `git <args>` in `dir`, with no user or system configuration, returning
/// its trimmed output.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf-8 output")
        .trim()
        .to_owned()
}

fn write_script(path: &Path, script: &str) {
    std::fs::write(path, script).expect("write the script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("make the script executable");
}

/// `path` quoted for `sh`.
fn quoted(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// The repository of the hang.
///
/// A bare remote with `master`; `clone`, with `master` checked out and a
/// branch `feature` that tracks `origin/master` checked out in the linked
/// worktree `wt`; and one more commit on the remote's `master`, which
/// changes `a.txt`. The clone's `origin` is an `ssh://` URL.
struct Scratch {
    _dir: tempfile::TempDir,
    root: PathBuf,
    clone: PathBuf,
    wt: PathBuf,
    /// A second clone, over the file system, to push to the remote from.
    other: PathBuf,
    ssh: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        Self::with_ssh(SSH)
    }

    fn with_ssh(script: &str) -> Self {
        let dir = tempfile::tempdir().expect("temporary directory");
        // As git names it, so paths compare equal.
        let root = dir.path().canonicalize().expect("canonical path");
        let (remote, clone, wt, other) = (
            root.join("remote.git"),
            root.join("clone"),
            root.join("wt"),
            root.join("other"),
        );
        git(
            &root,
            &["init", "--quiet", "--bare", "-b", "master", "remote.git"],
        );
        git(&root, &["init", "--quiet", "-b", "master", "other"]);
        git(&other, &["remote", "add", "origin", "../remote.git"]);
        std::fs::write(other.join("a.txt"), "a\n").expect("write");
        git(&other, &["add", "a.txt"]);
        git(&other, &["commit", "--quiet", "-m", "a"]);
        git(&other, &["push", "--quiet", "-u", "origin", "master"]);

        git(&root, &["clone", "--quiet", "remote.git", "clone"]);
        git(
            &clone,
            &["branch", "--quiet", "--track", "feature", "origin/master"],
        );
        git(&clone, &["worktree", "add", "--quiet", "../wt", "feature"]);

        std::fs::write(other.join("a.txt"), "a\nb\n").expect("write");
        git(&other, &["commit", "--quiet", "-am", "b"]);
        git(&other, &["push", "--quiet", "origin", "master"]);

        let ssh = root.join("ssh.sh");
        write_script(&ssh, script);
        let url = format!("ssh://stand-in.invalid{}", remote.display());
        git(&clone, &["remote", "set-url", "origin", &url]);
        Self {
            _dir: dir,
            root,
            clone,
            wt,
            other,
            ssh,
        }
    }

    /// What is tested: git in `dir`, reaching `origin` through the
    /// stand-in for ssh, whatever the user's configuration and environment
    /// say.
    fn git_in(&self, dir: &Path) -> Git {
        Git::new(dir)
            .env("GIT_SSH_COMMAND", quoted(&self.ssh))
            .env("GIT_SSH_VARIANT", "simple")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
    }

    /// The commit the remote's `master` is on.
    fn remote_master(&self) -> String {
        git(&self.other, &["rev-parse", "master"])
    }

    /// Every `*.lock` file in the repository and its worktrees.
    fn locks(&self) -> Vec<PathBuf> {
        fn visit(dir: &Path, found: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read the folder").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    visit(&path, found);
                } else if path
                    .extension()
                    .is_some_and(|extension| extension == "lock")
                {
                    found.push(path);
                }
            }
        }
        let mut found = Vec::new();
        visit(&self.root, &mut found);
        found
    }
}

/// Something running on another thread.
struct Running<T> {
    result: mpsc::Receiver<T>,
    operation: Operation,
}

impl<T: Send + 'static> Running<T> {
    fn start(operation: &Operation, work: impl FnOnce() -> T + Send + 'static) -> Self {
        let (sender, result) = mpsc::channel();
        std::thread::spawn(move || drop(sender.send(work())));
        Self {
            result,
            operation: operation.clone(),
        }
    }

    fn is_running(&self) -> bool {
        matches!(self.result.try_recv(), Err(mpsc::TryRecvError::Empty))
    }

    /// Its result, which must come within `limit`. A hung git is ended
    /// rather than left behind by a failing test.
    fn finish_within(self, limit: Duration) -> T {
        match self.result.recv_timeout(limit) {
            Ok(result) => result,
            Err(_) => {
                self.operation.cancel();
                panic!("still running after {limit:?}");
            }
        }
    }
}

/// Waits until `ready` says so; panics when it takes too long.
fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(start.elapsed() < HUNG, "waited too long for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn pulls_a_branch_in_the_worktree_it_is_checked_out_in() {
    let scratch = Scratch::new();
    let master_before = git(&scratch.clone, &["rev-parse", "master"]);
    let operation = Operation::new();
    let git_ = scratch.git_in(&scratch.clone).with_operation(&operation);

    // Fetching into `feature` from here is what git refuses, and what hung.
    let pulled = Running::start(&operation, move || git_.pull_branch("feature"))
        .finish_within(HUNG)
        .expect("pull in the worktree");

    assert_eq!(pulled.as_deref(), Some(scratch.wt.as_path()));
    assert_eq!(
        git(&scratch.clone, &["rev-parse", "feature"]),
        scratch.remote_master()
    );
    // Its files followed it, and nothing else moved.
    assert_eq!(
        std::fs::read_to_string(scratch.wt.join("a.txt")).expect("read"),
        "a\nb\n"
    );
    assert_eq!(git(&scratch.wt, &["status", "--porcelain"]), "");
    assert_eq!(git(&scratch.clone, &["rev-parse", "master"]), master_before);
    assert_eq!(git(&scratch.clone, &["branch", "--show-current"]), "master");
    assert_eq!(git(&scratch.clone, &["status", "--porcelain"]), "");
    assert_eq!(scratch.locks(), Vec::<PathBuf>::new());
}

#[test]
fn pulls_a_branch_checked_out_in_the_main_worktree_from_a_linked_one() {
    let scratch = Scratch::new();
    let git_ = scratch.git_in(&scratch.wt);

    let pulled = git_
        .pull_branch("master")
        .expect("pull in the main worktree");

    assert_eq!(pulled.as_deref(), Some(scratch.clone.as_path()));
    assert_eq!(
        git(&scratch.clone, &["rev-parse", "master"]),
        scratch.remote_master()
    );
    assert_eq!(
        std::fs::read_to_string(scratch.clone.join("a.txt")).expect("read"),
        "a\nb\n"
    );
}

#[test]
fn a_branch_checked_out_nowhere_is_still_fetched_into() {
    let scratch = Scratch::new();
    git(
        &scratch.clone,
        &["branch", "--quiet", "--track", "spare", "origin/master"],
    );

    let pulled = scratch
        .git_in(&scratch.clone)
        .pull_branch("spare")
        .expect("fetch into the branch");

    assert_eq!(pulled, None);
    assert_eq!(
        git(&scratch.clone, &["rev-parse", "spare"]),
        scratch.remote_master()
    );
}

#[test]
fn refuses_to_pull_in_a_worktree_when_the_branch_cannot_fast_forward() {
    let scratch = Scratch::new();
    std::fs::write(scratch.wt.join("own.txt"), "own\n").expect("write");
    git(&scratch.wt, &["add", "own.txt"]);
    git(&scratch.wt, &["commit", "--quiet", "-m", "own"]);
    let before = git(&scratch.clone, &["rev-parse", "feature"]);

    let error = scratch
        .git_in(&scratch.clone)
        .pull_branch("feature")
        .expect_err("diverged");

    assert!(not_fast_forward(&error), "{error}");
    assert!(!blocked_by_changes(&error), "{error}");
    // Nothing moved and no merge was started in the worktree.
    assert_eq!(git(&scratch.clone, &["rev-parse", "feature"]), before);
    assert_eq!(git(&scratch.wt, &["status", "--porcelain"]), "");
    let merge_head = git(&scratch.wt, &["rev-parse", "--git-path", "MERGE_HEAD"]);
    assert!(!scratch.wt.join(merge_head).exists());
    assert_eq!(scratch.locks(), Vec::<PathBuf>::new());
}

#[test]
fn refuses_to_pull_in_a_worktree_over_uncommitted_changes() {
    let scratch = Scratch::new();
    // The commit to pull changes this file too.
    std::fs::write(scratch.wt.join("a.txt"), "mine\n").expect("write");
    let before = git(&scratch.clone, &["rev-parse", "feature"]);

    let error = scratch
        .git_in(&scratch.clone)
        .pull_branch("feature")
        .expect_err("changes in the way");

    assert!(blocked_by_changes(&error), "{error}");
    assert!(!not_fast_forward(&error), "{error}");
    assert_eq!(git(&scratch.clone, &["rev-parse", "feature"]), before);
    assert_eq!(
        std::fs::read_to_string(scratch.wt.join("a.txt")).expect("read"),
        "mine\n"
    );
    assert_eq!(scratch.locks(), Vec::<PathBuf>::new());
}

#[test]
fn says_so_and_changes_nothing_when_the_worktree_is_locked_or_missing() {
    let scratch = Scratch::new();
    let git_ = scratch.git_in(&scratch.clone);
    let feature = git(&scratch.clone, &["rev-parse", "feature"]);
    let fetched = git(&scratch.clone, &["rev-parse", "origin/master"]);
    let unchanged = || {
        assert_eq!(git(&scratch.clone, &["rev-parse", "feature"]), feature);
        // Nothing was fetched either.
        assert_eq!(
            git(&scratch.clone, &["rev-parse", "origin/master"]),
            fetched
        );
    };

    git(
        &scratch.clone,
        &["worktree", "lock", "--reason", "on a stick", "../wt"],
    );
    match git_.pull_branch("feature") {
        Err(Error::WorktreeUnavailable {
            branch,
            path,
            problem,
        }) => {
            assert_eq!(branch, "feature");
            assert_eq!(path, scratch.wt);
            assert_eq!(
                problem,
                WorktreeProblem::Locked(Some("on a stick".to_owned()))
            );
        }
        other => panic!("expected the locked worktree to be refused, got {other:?}"),
    }
    unchanged();
    assert_eq!(
        std::fs::read_to_string(scratch.wt.join("a.txt")).expect("read"),
        "a\n"
    );

    git(&scratch.clone, &["worktree", "unlock", "../wt"]);
    std::fs::remove_dir_all(&scratch.wt).expect("delete the worktree's folder");
    let error = git_.pull_branch("feature").expect_err("missing");
    assert!(
        matches!(
            &error,
            Error::WorktreeUnavailable {
                problem: WorktreeProblem::Missing,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "feature is checked out in the worktree at {}, which no longer exists",
            scratch.wt.display()
        )
    );
    unchanged();
}

/// The fetch from the report: git refuses it, says why, and then never
/// exits, because ssh is still there.
fn refused_fetch() -> Vec<String> {
    ["fetch", "origin", "refs/heads/master:refs/heads/feature"]
        .map(str::to_owned)
        .to_vec()
}

#[test]
fn what_git_wrote_is_there_while_it_still_runs_and_cancelling_ends_it() {
    let scratch = Scratch::new();
    let told = Arc::new(Mutex::new(Vec::new()));
    let operation = Operation::new().on_stalled({
        let told = told.clone();
        move |said| told.lock().expect("lock").push(said.to_owned())
    });
    let git_ = scratch.git_in(&scratch.clone).with_operation(&operation);
    let lines = Arc::new(Mutex::new(Vec::new()));
    let fetch = Running::start(&operation, {
        let lines = lines.clone();
        move || {
            git_.run_as_typed(&refused_fetch(), |line| {
                lines.lock().expect("lock").push(line)
            })
        }
    });

    // The error is there as soon as git wrote it.
    let started = Instant::now();
    wait_until("git to refuse the fetch", || {
        operation.said().contains("refusing to fetch into branch")
    });
    assert!(
        operation.said().starts_with("fatal: "),
        "{}",
        operation.said()
    );
    assert!(fetch.is_running());
    assert_eq!(operation.stalled(), None);

    // A few seconds later git has still not exited, and says so itself.
    wait_until("the error to be reported", || operation.stalled().is_some());
    let reported = started.elapsed();
    assert!(reported >= STALLED_AFTER, "reported after {reported:?}");
    assert!(
        reported < STALLED_AFTER + SLACK,
        "reported after {reported:?}"
    );
    assert!(fetch.is_running());
    assert_eq!(*told.lock().expect("lock"), [operation.said()]);
    assert!(
        lines.lock().expect("lock")[0].starts_with("fatal: refusing to fetch into branch"),
        "{lines:?}"
    );

    let cancelled = Instant::now();
    operation.cancel();
    let result = fetch.finish_within(HUNG);
    let took = cancelled.elapsed();
    assert!(took < CANCEL_GRACE, "ended after {took:?}");
    match result {
        Err(Error::Cancelled { command, output }) => {
            assert!(command.starts_with("git fetch origin "), "{command}");
            assert!(output.contains("refusing to fetch into branch"), "{output}");
        }
        other => panic!("expected the fetch to be cancelled, got {other:?}"),
    }
    assert!(operation.is_cancelled());
    assert_eq!(operation.stalled(), None);
    assert_eq!(scratch.locks(), Vec::<PathBuf>::new());
    // Nothing was fetched into the branch.
    assert_ne!(
        git(&scratch.clone, &["rev-parse", "feature"]),
        scratch.remote_master()
    );
}

#[test]
fn another_error_git_cannot_leave_after_is_handled_the_same_way() {
    let scratch = Scratch::new();
    // A branch whose upstream was deleted on the remote.
    git(&scratch.clone, &["branch", "--quiet", "stale"]);
    git(&scratch.clone, &["config", "branch.stale.remote", "origin"]);
    git(
        &scratch.clone,
        &["config", "branch.stale.merge", "refs/heads/gone"],
    );
    let operation = Operation::new();
    let git_ = scratch.git_in(&scratch.clone).with_operation(&operation);
    let pull = Running::start(&operation, move || git_.pull_branch("stale"));

    wait_until("the error to be reported", || operation.stalled().is_some());
    assert_eq!(
        operation.stalled().as_deref(),
        Some("fatal: couldn't find remote ref refs/heads/gone")
    );
    assert!(pull.is_running());

    let cancelled = Instant::now();
    operation.cancel();
    let result = pull.finish_within(HUNG);
    let took = cancelled.elapsed();
    assert!(took < CANCEL_GRACE, "ended after {took:?}");
    match result {
        Err(error @ Error::Cancelled { .. }) => {
            assert!(error.is_cancelled());
            assert!(!not_fast_forward(&error));
        }
        other => panic!("expected the pull to be cancelled, got {other:?}"),
    }
    assert_eq!(scratch.locks(), Vec::<PathBuf>::new());
}

#[test]
fn a_process_that_stays_after_sigterm_is_killed_after_the_grace_period() {
    let scratch = Scratch::with_ssh(SSH_THAT_STAYS);
    let operation = Operation::new();
    let git_ = scratch.git_in(&scratch.clone).with_operation(&operation);
    let fetch = Running::start(&operation, move || {
        git_.run_as_typed(&refused_fetch(), |_| {})
    });
    wait_until("git to refuse the fetch", || {
        operation.said().contains("refusing to fetch into branch")
    });
    let ssh_pid = std::fs::read_to_string(scratch.root.join("ssh.sh.pid")).expect("read the pid");
    let ssh_runs = || {
        Command::new("kill")
            .args(["-0", ssh_pid.trim()])
            .output()
            .expect("kill runs")
            .status
            .success()
    };
    assert!(ssh_runs());

    let cancelled = Instant::now();
    operation.cancel();
    let result = fetch.finish_within(HUNG);
    let took = cancelled.elapsed();

    assert!(matches!(result, Err(Error::Cancelled { .. })), "{result:?}");
    // Git left when asked to; the stand-in held its output open until it
    // was killed.
    assert!(took >= CANCEL_GRACE, "ended after {took:?}");
    assert!(took < CANCEL_GRACE + SLACK, "ended after {took:?}");
    wait_until("the stand-in for ssh to be gone", || !ssh_runs());
    assert_eq!(scratch.locks(), Vec::<PathBuf>::new());
}

#[test]
fn cancelling_a_commit_that_holds_the_index_lock_leaves_no_lock() {
    let scratch = Scratch::new();
    let started = scratch.root.join("hook-started");
    // Ignores SIGTERM, so it is killed; git itself leaves when asked to.
    write_script(
        &scratch.clone.join(".git/hooks/pre-commit"),
        &format!(
            "#!/bin/sh\ntrap '' TERM\ntouch {}\nsleep 600\n",
            quoted(&started)
        ),
    );
    git(&scratch.clone, &["config", "user.name", "Test"]);
    git(
        &scratch.clone,
        &["config", "user.email", "test@example.com"],
    );
    git(&scratch.clone, &["config", "commit.gpgsign", "false"]);
    std::fs::write(scratch.clone.join("a.txt"), "edited\n").expect("write");
    let head = git(&scratch.clone, &["rev-parse", "HEAD"]);
    let operation = Operation::new();
    let git_ = scratch
        .git_in(&scratch.clone)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .with_operation(&operation);
    let commit = Running::start(&operation, move || {
        git_.commit_only("Edit a", &["a.txt".to_owned()])
    });
    wait_until("the hook to start", || started.exists());
    // Git holds its locks while the hook runs.
    let held = scratch.locks();
    assert!(
        held.contains(&scratch.clone.join(".git/index.lock")),
        "{held:?}"
    );

    let cancelled = Instant::now();
    operation.cancel();
    let result = commit.finish_within(HUNG);
    let took = cancelled.elapsed();

    assert!(matches!(result, Err(Error::Cancelled { .. })), "{result:?}");
    assert!(took >= CANCEL_GRACE, "ended after {took:?}");
    assert!(took < CANCEL_GRACE + SLACK, "ended after {took:?}");
    assert_eq!(scratch.locks(), Vec::<PathBuf>::new());
    // Nothing was committed, and the edit is still there.
    assert_eq!(git(&scratch.clone, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&scratch.clone, &["status", "--porcelain"]), "M a.txt");
}

#[test]
fn a_process_git_leaves_behind_does_not_keep_the_command_waiting() {
    let scratch = Scratch::new();
    let pid = scratch.root.join("left-behind.pid");
    // Starts something that outlives git and holds its output open.
    write_script(
        &scratch.clone.join(".git/hooks/post-commit"),
        &format!(
            "#!/bin/sh\nsleep 30 &\necho $! > {}\necho committed\n",
            quoted(&pid)
        ),
    );
    git(&scratch.clone, &["config", "user.name", "Test"]);
    git(
        &scratch.clone,
        &["config", "user.email", "test@example.com"],
    );
    git(&scratch.clone, &["config", "commit.gpgsign", "false"]);
    std::fs::write(scratch.clone.join("a.txt"), "edited\n").expect("write");
    let git_ = scratch.git_in(&scratch.clone);
    git_.stage_all().expect("stage");

    let started = Instant::now();
    let committed = git_.commit("Edit a");
    let took = started.elapsed();

    // It is left alone: git started it to stay.
    let pid = std::fs::read_to_string(&pid).expect("read the pid");
    let kill = |signal: &str| {
        Command::new("kill")
            .args([signal, pid.trim()])
            .output()
            .expect("kill runs")
            .status
            .success()
    };
    let still_there = kill("-0");
    kill("-KILL");
    committed.expect("commit");
    assert!(took < SLACK, "returned after {took:?}");
    assert!(still_there);
    assert_eq!(git(&scratch.clone, &["log", "-1", "--format=%s"]), "Edit a");
}

#[test]
fn a_cancelled_operation_runs_nothing_more_but_undoes_what_it_left() {
    let scratch = Scratch::new();
    let git_ = scratch.git_in(&scratch.clone);

    // Cancelled before it starts: nothing runs, so nothing is fetched.
    let operation = Operation::new();
    operation.cancel();
    let fetched = git(&scratch.clone, &["rev-parse", "origin/master"]);
    let error = git_
        .clone()
        .with_operation(&operation)
        .fetch()
        .expect_err("cancelled");
    assert!(error.is_cancelled(), "{error:?}");
    assert_eq!(
        git(&scratch.clone, &["rev-parse", "origin/master"]),
        fetched
    );

    // Cancelled while switching to the base of a merge: the merge does not
    // go on, and the branch checked out before is checked out again.
    git(&scratch.clone, &["branch", "--quiet", "base"]);
    git(&scratch.clone, &["branch", "--quiet", "topic"]);
    let started = scratch.root.join("hook-started");
    // Waits the first time only: the switch back runs it again.
    write_script(
        &scratch.clone.join(".git/hooks/post-checkout"),
        &format!(
            "#!/bin/sh\nif [ ! -e {started} ]; then touch {started}; sleep 600; fi\n",
            started = quoted(&started)
        ),
    );
    let operation = Operation::new();
    let merge = Running::start(&operation, {
        let git_ = git_.with_operation(&operation);
        move || git_.merge_branch("topic", "base", MergeMethod::Merge, "Merge", Some("master"))
    });
    wait_until("the hook to start", || started.exists());
    assert_eq!(git(&scratch.clone, &["branch", "--show-current"]), "base");
    operation.cancel();
    let result = merge.finish_within(HUNG);

    assert!(matches!(result, Err(Error::Cancelled { .. })), "{result:?}");
    assert_eq!(git(&scratch.clone, &["branch", "--show-current"]), "master");
    assert_eq!(
        git(&scratch.clone, &["log", "--format=%s", "-1", "base"]),
        "a"
    );
    assert_eq!(scratch.locks(), Vec::<PathBuf>::new());
}
