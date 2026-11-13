//! Worktrees of agent threads: creating, preparing and completing them, and
//! watching thread files, in temporary repositories whose paths have
//! spaces.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::{Fixture, git};
use gitez_agent_thread::{
    CACHE_DIRS, CompleteReport, Error, PrepareStep, complete, create_worktree, prepare, watch,
    worktree_git_dir,
};

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().expect("exists")
}

#[test]
fn creates_the_branch_and_its_worktree() {
    let fixture = Fixture::new();
    let start = fixture.head();
    let thread = fixture.create("Add a blame view");
    // The main worktree moves on; the thread starts where it was asked to.
    fs::write(fixture.root.join("later.txt"), "later\n").expect("write");
    git(&fixture.root, &["add", "-A"]);
    git(&fixture.root, &["commit", "--quiet", "-m", "Later"]);

    let git_dir =
        create_worktree(&fixture.root, &thread.worktree, &thread.branch, &start).expect("create");
    assert!(
        thread
            .worktree
            .starts_with(fixture.data_dir.join("worktrees")),
        "in the data folder"
    );
    assert_eq!(
        git(&fixture.root, &["rev-parse", "agent/add-a-blame-view"]),
        start
    );
    assert_eq!(
        git(&thread.worktree, &["branch", "--show-current"]),
        "agent/add-a-blame-view"
    );
    assert!(thread.worktree.join("README.md").is_file());
    assert!(!thread.worktree.join("later.txt").exists());
    assert_eq!(
        canonical(&git_dir),
        canonical(
            &fixture
                .common_dir()
                .join("worktrees")
                .join("add-a-blame-view")
        )
    );
    assert_eq!(
        worktree_git_dir(&thread.worktree).expect("git dir"),
        git_dir
    );
    assert_eq!(
        canonical(Path::new(&git(
            &thread.worktree,
            &["rev-parse", "--absolute-git-dir"]
        ))),
        canonical(&git_dir)
    );

    // Again: it is already there.
    assert_eq!(
        create_worktree(&fixture.root, &thread.worktree, &thread.branch, &start).expect("again"),
        git_dir
    );
    // Not for another branch.
    assert!(matches!(
        create_worktree(&fixture.root, &thread.worktree, "agent/other", &start),
        Err(Error::Refused(_))
    ));
}

#[test]
fn an_existing_branch_is_checked_out_as_it_is() {
    let fixture = Fixture::new();
    let first = fixture.head();
    let thread = fixture.create("Keep going");
    git(&fixture.root, &["branch", &thread.branch]);
    fs::write(fixture.root.join("b.txt"), "b\n").expect("write");
    git(&fixture.root, &["add", "-A"]);
    git(&fixture.root, &["commit", "--quiet", "-m", "Second"]);
    let second = fixture.head();

    create_worktree(&fixture.root, &thread.worktree, &thread.branch, &second).expect("create");
    assert_eq!(git(&thread.worktree, &["rev-parse", "HEAD"]), first);
}

#[test]
fn a_folder_in_the_way_is_refused() {
    let fixture = Fixture::new();
    let thread = fixture.create("Something");
    fs::create_dir_all(&thread.worktree).expect("create folder");
    fs::write(thread.worktree.join("stray.txt"), "stray\n").expect("write");
    let result = create_worktree(
        &fixture.root,
        &thread.worktree,
        &thread.branch,
        &fixture.head(),
    );
    assert!(matches!(result, Err(Error::Refused(_))), "{result:?}");

    // An empty folder is used.
    fs::remove_file(thread.worktree.join("stray.txt")).expect("remove");
    create_worktree(
        &fixture.root,
        &thread.worktree,
        &thread.branch,
        &fixture.head(),
    )
    .expect("create in an empty folder");
}

#[test]
fn completing_removes_the_worktree_and_a_branch_without_commits() {
    let fixture = Fixture::new();
    let start = fixture.head();

    let idle = fixture.create("Nothing to do");
    create_worktree(&fixture.root, &idle.worktree, &idle.branch, &start).expect("create");
    // Leftovers do not stop the removal.
    fs::write(idle.worktree.join("scratch.txt"), "scratch\n").expect("write");

    let busy = fixture.create("Do something");
    create_worktree(&fixture.root, &busy.worktree, &busy.branch, &start).expect("create");
    fs::write(busy.worktree.join("work.txt"), "work\n").expect("write");
    git(&busy.worktree, &["add", "-A"]);
    git(&busy.worktree, &["commit", "--quiet", "-m", "Work"]);

    let gone = fixture.create("Folder gone");
    create_worktree(&fixture.root, &gone.worktree, &gone.branch, &start).expect("create");
    fs::remove_dir_all(&gone.worktree).expect("remove folder");

    assert_eq!(
        complete(&fixture.root, &idle).expect("complete"),
        CompleteReport {
            worktree_removed: true,
            branch_deleted: true,
        }
    );
    assert!(!idle.worktree.exists());
    assert_eq!(
        complete(&fixture.root, &busy).expect("complete"),
        CompleteReport {
            worktree_removed: true,
            branch_deleted: false,
        }
    );
    assert!(!busy.worktree.exists());
    assert_eq!(
        complete(&fixture.root, &gone).expect("complete"),
        CompleteReport {
            worktree_removed: false,
            branch_deleted: true,
        }
    );

    let branches = git(&fixture.root, &["branch", "--format=%(refname:short)"]);
    assert_eq!(
        branches.lines().collect::<Vec<_>>(),
        ["agent/do-something", "main"]
    );
    assert_eq!(
        git(&fixture.root, &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count(),
        1,
        "only the main worktree is left"
    );
    // Completing twice changes nothing more.
    assert_eq!(
        complete(&fixture.root, &idle).expect("complete again"),
        CompleteReport {
            worktree_removed: false,
            branch_deleted: false,
        }
    );
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set permissions");
}

#[test]
fn prepare_copies_included_files_and_clones_caches() {
    let fixture = Fixture::new();
    let root = &fixture.root;
    fs::write(root.join(".gitignore"), ".env\nsecrets/\ntarget/\n*.log\n").expect("write");
    fs::write(root.join(".worktreeinclude"), ".env\nsecrets/\n").expect("write");
    git(root, &["add", "-A"]);
    git(root, &["commit", "--quiet", "-m", "Ignore"]);
    fs::write(root.join(".env"), "TOKEN=1\n").expect("write");
    fs::create_dir_all(root.join("secrets/nested dir")).expect("create folder");
    fs::write(root.join("secrets/nested dir/key"), "key\n").expect("write");
    fs::write(root.join("debug.log"), "not included\n").expect("write");
    fs::create_dir_all(root.join("target/debug/deps")).expect("create folder");
    fs::write(root.join("target/debug/deps/libserde.rlib"), "rlib\n").expect("write");
    #[cfg(unix)]
    {
        set_mode(&root.join(".env"), 0o600);
        set_mode(&root.join("secrets/nested dir/key"), 0o755);
        std::os::unix::fs::symlink("nested dir/key", root.join("secrets/link")).expect("symlink");
    }

    let thread = fixture.create("Prepare me");
    create_worktree(
        &fixture.root,
        &thread.worktree,
        &thread.branch,
        &fixture.head(),
    )
    .expect("create");
    let main = gitez_git::Repository::open(root).expect("open");
    let mut steps = Vec::new();
    let report = prepare(&main, &thread.worktree, |step| steps.push(step)).expect("prepare");

    assert_eq!(
        report.copied,
        [PathBuf::from(".env"), PathBuf::from("secrets")]
    );
    let worktree = &thread.worktree;
    assert_eq!(
        fs::read_to_string(worktree.join(".env")).expect("copied"),
        "TOKEN=1\n"
    );
    assert_eq!(
        fs::read_to_string(worktree.join("secrets/nested dir/key")).expect("copied"),
        "key\n"
    );
    assert!(!worktree.join("debug.log").exists());
    #[cfg(unix)]
    {
        assert_eq!(mode(&worktree.join(".env")), 0o600);
        assert_eq!(mode(&worktree.join("secrets/nested dir/key")), 0o755);
        assert_eq!(
            fs::read_link(worktree.join("secrets/link")).expect("a link"),
            PathBuf::from("nested dir/key")
        );
    }

    let cloned_file = worktree.join("target/debug/deps/libserde.rlib");
    if report.cloned.iter().any(|dir| dir == "target") {
        assert_eq!(fs::read_to_string(&cloned_file).expect("cloned"), "rlib\n");
        assert!(report.not_cloned.is_empty());
    } else {
        // A filesystem without copy-on-write clones: skipped, nothing left.
        assert_eq!(report.not_cloned, ["target"]);
        assert!(!worktree.join("target").exists());
    }
    // The temporary folder is on APFS on a Mac.
    #[cfg(target_os = "macos")]
    assert_eq!(report.cloned, ["target"]);
    assert!(CACHE_DIRS.contains(&"node_modules"));
    assert!(
        !report.cloned.iter().any(|dir| dir == "node_modules"),
        "absent in the main worktree"
    );
    assert_eq!(
        steps,
        [
            PrepareStep::CopyingIncludes { files: 2 },
            PrepareStep::Cloning {
                dir: "target".to_owned()
            }
        ]
    );
    assert!(
        git(worktree, &["status", "--porcelain"]).is_empty(),
        "everything copied is ignored"
    );

    // Again: what is there is left alone.
    fs::write(worktree.join(".env"), "TOKEN=changed\n").expect("write");
    let again = prepare(&main, worktree, |_| {}).expect("prepare again");
    assert!(again.copied.is_empty());
    assert!(again.cloned.is_empty());
    assert_eq!(
        fs::read_to_string(worktree.join(".env")).expect("kept"),
        "TOKEN=changed\n"
    );
}

#[test]
fn watch_reports_a_write() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let watched = dir.path().join("threads dir");
    fs::create_dir_all(&watched).expect("create folder");
    let (sender, receiver) = mpsc::channel();
    let _watcher = watch(
        &[watched.clone(), dir.path().join("missing")],
        move |paths| {
            let _ = sender.send(paths);
        },
    )
    .expect("watch");
    // FSEvents may start reporting a moment after the watch is set up.
    std::thread::sleep(Duration::from_millis(200));
    fs::write(watched.join("thread.json"), "{}\n").expect("write");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = false;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match receiver.recv_timeout(left) {
            Ok(paths)
                if paths.is_empty()
                    || paths
                        .iter()
                        .any(|path| path.file_name().is_some_and(|name| name == "thread.json")) =>
            {
                seen = true;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(seen, "the write was reported");
}
