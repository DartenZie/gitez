//! Worktrees of agent threads: creating, preparing and completing them, and
//! watching thread files, in temporary repositories whose paths have
//! spaces.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::{Fixture, branch_of, git, worktree_of};
use gitez_agent_thread::{
    AgentThread, CACHE_DIRS, CompleteReport, Error, IfUncommitted, Kept, PrepareStep, Timestamp,
    Uncommitted, check_worktree, complete, create_worktree, prepare, scratch_dirs, watch,
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

    let created = create_worktree(
        &fixture.root,
        worktree_of(&thread),
        branch_of(&thread),
        &start,
    )
    .expect("create");
    assert!(
        !created.needs_checkout,
        "the start commit is checked out at once"
    );
    let git_dir = created.git_dir.clone();
    assert!(
        worktree_of(&thread).starts_with(fixture.data_dir.join("worktrees")),
        "in the data folder"
    );
    assert_eq!(
        git(&fixture.root, &["rev-parse", "agent/add-a-blame-view"]),
        start
    );
    assert_eq!(
        git(worktree_of(&thread), &["branch", "--show-current"]),
        "agent/add-a-blame-view"
    );
    assert!(worktree_of(&thread).join("README.md").is_file());
    assert!(!worktree_of(&thread).join("later.txt").exists());
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
        worktree_git_dir(worktree_of(&thread)).expect("git dir"),
        git_dir
    );
    assert_eq!(
        canonical(Path::new(&git(
            worktree_of(&thread),
            &["rev-parse", "--absolute-git-dir"]
        ))),
        canonical(&git_dir)
    );

    assert_eq!(
        check_worktree(
            &fixture.common_dir(),
            worktree_of(&thread),
            Some(&created.record)
        )
        .expect("as made"),
        created.record
    );
    // Again: it is already there.
    assert_eq!(
        create_worktree(
            &fixture.root,
            worktree_of(&thread),
            branch_of(&thread),
            &start
        )
        .expect("again"),
        created
    );
    // Not for another branch.
    assert!(matches!(
        create_worktree(&fixture.root, worktree_of(&thread), "agent/other", &start),
        Err(Error::Refused(_))
    ));
}

#[test]
fn an_existing_branch_is_checked_out_as_it_is() {
    let fixture = Fixture::new();
    let first = fixture.head();
    let thread = fixture.create("Keep going");
    git(&fixture.root, &["branch", branch_of(&thread)]);
    fs::write(fixture.root.join("b.txt"), "b\n").expect("write");
    git(&fixture.root, &["add", "-A"]);
    git(&fixture.root, &["commit", "--quiet", "-m", "Second"]);
    let second = fixture.head();

    create_worktree(
        &fixture.root,
        worktree_of(&thread),
        branch_of(&thread),
        &second,
    )
    .expect("create");
    assert_eq!(git(worktree_of(&thread), &["rev-parse", "HEAD"]), first);
}

#[test]
fn a_folder_in_the_way_is_refused() {
    let fixture = Fixture::new();
    let thread = fixture.create("Something");
    fs::create_dir_all(worktree_of(&thread)).expect("create folder");
    fs::write(worktree_of(&thread).join("stray.txt"), "stray\n").expect("write");
    let result = create_worktree(
        &fixture.root,
        worktree_of(&thread),
        branch_of(&thread),
        &fixture.head(),
    );
    assert!(matches!(result, Err(Error::Refused(_))), "{result:?}");

    // An empty folder is used.
    fs::remove_file(worktree_of(&thread).join("stray.txt")).expect("remove");
    create_worktree(
        &fixture.root,
        worktree_of(&thread),
        branch_of(&thread),
        &fixture.head(),
    )
    .expect("create in an empty folder");
}

#[test]
fn completing_removes_the_worktree_and_a_branch_without_commits() {
    let fixture = Fixture::new();

    let idle = made(&fixture, "Nothing to do", Some(0));
    // Leftovers the supervisor did not see do not stop the removal.
    fs::write(worktree_of(&idle).join("scratch.txt"), "scratch\n").expect("write");

    let busy = made(&fixture, "Do something", Some(0));
    fs::write(worktree_of(&busy).join("work.txt"), "work\n").expect("write");
    git(worktree_of(&busy), &["add", "-A"]);
    git(worktree_of(&busy), &["commit", "--quiet", "-m", "Work"]);

    let gone = made(&fixture, "Folder gone", None);
    fs::remove_dir_all(worktree_of(&gone)).expect("remove folder");

    // The thread's temporary folder and caches go too, read-only cached
    // sources included; another thread's stay.
    let key = worktree_of(&idle)
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .expect("the repository key folder")
        .to_owned();
    let data_dir = worktree_of(&idle)
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("the data folder")
        .to_owned();
    let scratch = scratch_dirs(&data_dir, &key, &idle.id);
    assert_eq!(
        scratch.tmp,
        data_dir.join("tmp").join(format!("{key}-{}", idle.id))
    );
    assert_eq!(
        scratch.caches,
        data_dir.join("caches").join(format!("{key}-{}", idle.id))
    );
    let sources = scratch.caches.join("cargo/registry/src/crate-1.0");
    fs::create_dir_all(&sources).expect("create caches");
    fs::write(sources.join("lib.rs"), "").expect("write");
    #[cfg(unix)]
    set_mode(&sources, 0o555);
    fs::create_dir_all(&scratch.tmp).expect("create tmp");
    let other = scratch_dirs(&data_dir, &key, &busy.id);
    fs::create_dir_all(&other.caches).expect("create other caches");

    assert_eq!(
        complete(&fixture.root, &idle, IfUncommitted::Refuse, None).expect("complete"),
        CompleteReport {
            nothing_to_remove: false,
            worktree_removed: true,
            kept: None,
            branch_deleted: true,
            deleted_at: Some(idle.start.commit.clone()),
            branch_kept: None,
        }
    );
    assert!(!worktree_of(&idle).exists());
    assert!(!scratch.tmp.exists(), "the temporary folder is removed");
    assert!(!scratch.caches.exists(), "the caches are removed");
    assert!(other.caches.exists(), "another thread's caches stay");
    assert_eq!(
        complete(&fixture.root, &busy, IfUncommitted::Refuse, None).expect("complete"),
        CompleteReport {
            nothing_to_remove: false,
            worktree_removed: true,
            kept: None,
            branch_deleted: false,
            deleted_at: None,
            branch_kept: None,
        }
    );
    assert!(!worktree_of(&busy).exists());
    // A folder that is gone has nothing to lose, whatever is recorded.
    assert_eq!(
        complete(&fixture.root, &gone, IfUncommitted::Refuse, None).expect("complete"),
        CompleteReport {
            nothing_to_remove: false,
            worktree_removed: false,
            kept: None,
            branch_deleted: true,
            deleted_at: Some(gone.start.commit.clone()),
            branch_kept: None,
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
        complete(&fixture.root, &idle, IfUncommitted::Refuse, None).expect("complete again"),
        CompleteReport {
            nothing_to_remove: false,
            worktree_removed: false,
            kept: None,
            branch_deleted: false,
            deleted_at: None,
            branch_kept: None,
        }
    );
}

/// A thread for `prompt` with its worktree made, recorded as the
/// supervisor records it, and `count` uncommitted changes found at the end
/// of its last run (`None`: not known).
fn made(fixture: &Fixture, prompt: &str, count: Option<u32>) -> AgentThread {
    let mut thread = fixture.create(prompt);
    let created = create_worktree(
        &fixture.root,
        worktree_of(&thread),
        branch_of(&thread),
        &thread.start.commit,
    )
    .expect("create");
    thread.worktree_record = Some(created.record);
    thread.uncommitted = count.map(|count| Uncommitted {
        checked: Timestamp::now(),
        count,
        paths: (0..count.min(3)).map(|n| format!("?? file-{n}")).collect(),
        commit_error: None,
    });
    thread
}

#[test]
fn uncommitted_changes_are_never_removed_silently() {
    let fixture = Fixture::new();
    // The supervisor recorded uncommitted changes (a failed leftover commit).
    let thread = made(&fixture, "Left work", Some(2));
    fs::write(worktree_of(&thread).join("work.txt"), "unsaved\n").expect("write");
    let error = complete(&fixture.root, &thread, IfUncommitted::Refuse, None)
        .expect_err("refused by default");
    assert!(
        matches!(&error, Error::Uncommitted { count: Some(2), .. }),
        "{error}"
    );
    assert!(
        worktree_of(&thread).join("work.txt").exists(),
        "nothing removed"
    );

    // Completing it after a merge keeps the folder and its branch.
    let report = complete(&fixture.root, &thread, IfUncommitted::Keep, None).expect("kept");
    assert_eq!(
        report,
        CompleteReport {
            nothing_to_remove: false,
            worktree_removed: false,
            kept: Some(Kept::Uncommitted { count: Some(2) }),
            branch_deleted: false,
            deleted_at: None,
            branch_kept: None,
        }
    );
    assert!(worktree_of(&thread).join("work.txt").exists());
    assert_eq!(fixture.tip(branch_of(&thread)), thread.start.commit);

    // Removed later when the user says so.
    let report = complete(&fixture.root, &thread, IfUncommitted::Discard, None).expect("discarded");
    assert!(
        report.worktree_removed && report.branch_deleted,
        "{report:?}"
    );
    assert!(!worktree_of(&thread).exists());

    // With nothing recorded (count None), there is no evidence of changes,
    // so the worktree is removed as before.
    let unknown = made(&fixture, "No record", None);
    assert!(unknown.uncommitted.is_none());
    let report = complete(&fixture.root, &unknown, IfUncommitted::Refuse, None).expect("removed");
    assert!(report.worktree_removed, "{report:?}");
}

/// A git dir the agent wrote, whose configuration runs a command (it writes
/// `<marks>/pwned`) as soon as git reads a modified file's status.
fn plant_fake_git_dir(fixture: &Fixture, thread: &AgentThread, marks: &Path) {
    let fake = worktree_of(thread).join("fake.git");
    let fake_text = fake.to_str().expect("utf-8");
    git(
        &fixture.root,
        &[
            "clone",
            "--quiet",
            "--bare",
            fixture.common_dir().to_str().expect("utf-8"),
            fake_text,
        ],
    );
    let mark = format!("sh -c 'echo ran >> \"{}/pwned\"'; echo", marks.display());
    for (key, value) in [
        ("core.bare", "false".to_owned()),
        ("core.fsmonitor", mark.clone()),
        ("filter.evil.clean", format!("{mark}; cat")),
    ] {
        git(
            &fixture.root,
            &[
                "config",
                "--file",
                &format!("{fake_text}/config"),
                key,
                &value,
            ],
        );
    }
    fs::write(
        worktree_of(thread).join(".gitattributes"),
        "* filter=evil\n",
    )
    .expect("write");
    fs::write(worktree_of(thread).join("README.md"), "changed\n").expect("write");
}

#[test]
fn a_redirected_git_file_is_never_followed() {
    let fixture = Fixture::new();
    let marks = fixture.root.parent().expect("base").join("marks");
    fs::create_dir_all(&marks).expect("marks");
    let thread = made(&fixture, "Redirect", Some(0));
    plant_fake_git_dir(&fixture, &thread, &marks);
    let original = fs::read_to_string(worktree_of(&thread).join(".git")).expect("read");
    fs::write(
        worktree_of(&thread).join(".git"),
        format!(
            "gitdir: {}\n",
            worktree_of(&thread).join("fake.git").display()
        ),
    )
    .expect("redirect");

    for record in [thread.worktree_record.as_ref(), None] {
        let error = check_worktree(&fixture.common_dir(), worktree_of(&thread), record)
            .expect_err("redirected");
        assert!(matches!(error, Error::Tampered { .. }), "{error}");
    }
    // Neither a confirmed removal nor a reuse runs git there.
    let error =
        complete(&fixture.root, &thread, IfUncommitted::Discard, None).expect_err("untrusted");
    assert!(matches!(error, Error::Tampered { .. }), "{error}");
    let error = create_worktree(
        &fixture.root,
        worktree_of(&thread),
        branch_of(&thread),
        &thread.start.commit,
    )
    .expect_err("untrusted");
    assert!(matches!(error, Error::Tampered { .. }), "{error}");
    let report = complete(&fixture.root, &thread, IfUncommitted::Keep, None).expect("kept");
    assert!(
        matches!(report.kept, Some(Kept::Untrusted(_))),
        "{report:?}"
    );
    assert!(worktree_of(&thread).exists());
    assert!(!marks.join("pwned").exists(), "no command of the agent ran");

    // The same file, rewritten to say what it said, is trusted again; a
    // copy under another name (a hard link) is not.
    fs::write(worktree_of(&thread).join(".git"), &original).expect("restore");
    check_worktree(
        &fixture.common_dir(),
        worktree_of(&thread),
        thread.worktree_record.as_ref(),
    )
    .expect("as made");
    fs::hard_link(
        worktree_of(&thread).join(".git"),
        worktree_of(&thread).join("other"),
    )
    .expect("link");
    let error = check_worktree(
        &fixture.common_dir(),
        worktree_of(&thread),
        thread.worktree_record.as_ref(),
    )
    .expect_err("linked");
    assert!(error.to_string().contains("hard link"), "{error}");
    assert!(!marks.join("pwned").exists());

    // The trap was armed: git run there, as GitEZ ran it before, follows
    // the planted pointer and runs the agent's command.
    fs::remove_file(worktree_of(&thread).join("other")).expect("unlink");
    fs::write(
        worktree_of(&thread).join(".git"),
        format!(
            "gitdir: {}\n",
            worktree_of(&thread).join("fake.git").display()
        ),
    )
    .expect("redirect");
    let _ = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree_of(&thread))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output();
    assert!(
        marks.join("pwned").exists(),
        "the planted command runs under git"
    );
}

#[test]
fn changed_files_in_the_worktrees_git_dir_are_refused() {
    let fixture = Fixture::new();
    let thread = made(&fixture, "Admin files", Some(0));
    let record = thread.worktree_record.clone().expect("recorded");
    let git_dir = record.git_dir.clone();
    let check = || check_worktree(&fixture.common_dir(), worktree_of(&thread), Some(&record));
    let refused = |what: &str| {
        let error = check().expect_err(what);
        assert!(matches!(error, Error::Tampered { .. }), "{what}: {error}");
    };

    for (name, text) in [
        ("commondir", "../../../elsewhere\n"),
        ("gitdir", "/tmp/elsewhere/.git\n"),
    ] {
        let path = git_dir.join(name);
        let before = fs::read_to_string(&path).expect("read");
        fs::write(&path, text).expect("write");
        refused(name);
        fs::write(&path, &before).expect("restore");
        check().expect("restored");
    }
    // Files git would read there, which git never made.
    for name in ["config.worktree", "config"] {
        let path = git_dir.join(name);
        fs::write(&path, "[core]\n\tfsmonitor = touch /tmp/pwned\n").expect("write");
        refused(name);
        fs::remove_file(&path).expect("remove");
    }
    // The worktree folder replaced by a link to another worktree of it.
    let moved = worktree_of(&thread).with_file_name("moved away");
    fs::rename(worktree_of(&thread), &moved).expect("move");
    std::os::unix::fs::symlink(&moved, worktree_of(&thread)).expect("link");
    refused("a linked worktree folder");
    fs::remove_file(worktree_of(&thread)).expect("unlink");
    fs::rename(&moved, worktree_of(&thread)).expect("move back");
    check().expect("back");
}

#[test]
fn a_branch_with_commits_of_its_own_is_not_checked_out_outside_the_sandbox() {
    let fixture = Fixture::new();
    let marks = fixture.root.parent().expect("base").join("marks");
    fs::create_dir_all(&marks).expect("marks");
    // The user's own filter driver, as git-lfs installs one, which a
    // checkout runs on every file the attributes select.
    git(
        &fixture.root,
        &[
            "config",
            "filter.user.smudge",
            &format!("sh -c 'echo smudge >> \"{}/ran\"; cat'", marks.display()),
        ],
    );
    let thread = made(&fixture, "Agent work", Some(0));
    // The agent's commit selects it for every file.
    fs::write(
        worktree_of(&thread).join(".gitattributes"),
        "* filter=user\n",
    )
    .expect("write");
    fs::write(worktree_of(&thread).join("agent.txt"), "agent\n").expect("write");
    git(worktree_of(&thread), &["add", "-A"]);
    git(worktree_of(&thread), &["commit", "--quiet", "-m", "Agent"]);
    let _ = fs::remove_file(marks.join("ran"));
    // The folder is deleted by hand; a reply makes it again.
    fs::remove_dir_all(worktree_of(&thread)).expect("remove");
    git(&fixture.root, &["worktree", "prune"]);
    let created = create_worktree(
        &fixture.root,
        worktree_of(&thread),
        branch_of(&thread),
        &thread.start.commit,
    )
    .expect("create");
    assert!(created.needs_checkout);
    assert!(
        !worktree_of(&thread).join("agent.txt").exists(),
        "nothing checked out"
    );
    assert!(!marks.join("ran").exists(), "no filter ran");
    // The supervisor's checkout, in the sandbox, is a reset.
    git(worktree_of(&thread), &["reset", "--hard", "--quiet"]);
    assert!(worktree_of(&thread).join("agent.txt").exists());
}

#[test]
fn prepare_never_writes_through_a_link_in_the_worktree() {
    let fixture = Fixture::new();
    let root = &fixture.root;
    fs::write(root.join(".gitignore"), "config/\n.env\n").expect("write");
    fs::write(root.join(".worktreeinclude"), "config/local.env\n.env\n").expect("write");
    git(root, &["add", "-A"]);
    git(root, &["commit", "--quiet", "-m", "Ignore"]);
    fs::create_dir_all(root.join("config")).expect("dir");
    fs::write(root.join("config/local.env"), "SECRET=1\n").expect("write");
    fs::write(root.join(".env"), "A=1\n").expect("write");
    let victim = root.parent().expect("base").join("victim");
    fs::create_dir_all(&victim).expect("victim");

    let thread = made(&fixture, "Links", Some(0));
    // What a failed setup command left for the retry: links out of the
    // worktree where included files go.
    std::os::unix::fs::symlink(&victim, worktree_of(&thread).join("config")).expect("link");
    std::os::unix::fs::symlink(victim.join("dot-env"), worktree_of(&thread).join(".env"))
        .expect("link");
    let main = gitez_git::Repository::open(root).expect("open");
    let report = prepare(&main, worktree_of(&thread), |_| {}).expect("prepare");
    assert_eq!(report.skipped, [PathBuf::from("config/local.env")]);
    assert!(report.copied.is_empty(), "{report:?}");
    assert_eq!(
        fs::read_dir(&victim).expect("read").count(),
        0,
        "nothing was written outside the worktree"
    );
}

#[test]
fn completing_follows_no_link_and_runs_nothing_in_the_worktree() {
    let fixture = Fixture::new();
    let base = fixture.root.parent().expect("base").to_owned();
    let marks = base.join("marks");
    let victim = base.join("victim");
    fs::create_dir_all(&marks).expect("marks");
    fs::create_dir_all(&victim).expect("victim");
    fs::write(victim.join("keep.txt"), "keep\n").expect("write");
    let mark = |name: &str| format!("sh -c 'echo {name} >> \"{}/ran\"'; echo", marks.display());
    // The user's filter, diff driver and a hook path inside the worktree.
    git(
        &fixture.root,
        &[
            "config",
            "filter.user.clean",
            &format!("{}; cat", mark("clean")),
        ],
    );
    git(&fixture.root, &["config", "core.hooksPath", ".husky"]);
    let thread = made(&fixture, "Traps", Some(0));
    let worktree = worktree_of(&thread);
    fs::write(worktree.join(".gitattributes"), "* filter=user\n").expect("write");
    fs::create_dir_all(worktree.join(".husky")).expect("hooks");
    for hook in [
        "post-checkout",
        "reference-transaction",
        "post-index-change",
    ] {
        let path = worktree.join(".husky").join(hook);
        fs::write(&path, format!("#!/bin/sh\n{}\n", mark(hook))).expect("hook");
        set_mode(&path, 0o755);
    }
    // A nested repository and a planted submodule whose own configuration
    // runs a command whenever git looks at their state.
    for name in ["sub", "nested"] {
        let dir = worktree.join(name);
        fs::create_dir_all(&dir).expect("dir");
        git(&dir, &["init", "--quiet"]);
        git(&dir, &["config", "core.fsmonitor", &mark(name)]);
        fs::write(dir.join("f"), "f\n").expect("write");
        git(&dir, &["add", "f"]);
        git(&dir, &["commit", "--quiet", "-m", "f"]);
    }
    let sub_head = git(&worktree.join("sub"), &["rev-parse", "HEAD"]);
    git(
        worktree,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{sub_head},sub"),
        ],
    );
    std::os::unix::fs::symlink(&victim, worktree.join("out")).expect("link");
    fs::write(worktree.join("README.md"), "changed\n").expect("write");
    // Armed: a status check there, as GitEZ made before, runs the filter
    // and the submodule's command.
    git(worktree, &["status", "--porcelain"]);
    let ran = fs::read_to_string(marks.join("ran")).unwrap_or_default();
    assert!(
        ran.contains("clean") && ran.contains("sub"),
        "armed: {ran:?}"
    );
    fs::remove_file(marks.join("ran")).expect("reset the marks");

    let report = complete(&fixture.root, &thread, IfUncommitted::Discard, None).expect("complete");
    assert!(report.worktree_removed, "{report:?}");
    assert!(!worktree.exists());
    assert!(
        !marks.join("ran").exists(),
        "ran: {}",
        fs::read_to_string(marks.join("ran")).unwrap_or_default()
    );
    assert_eq!(
        fs::read_to_string(victim.join("keep.txt")).expect("kept"),
        "keep\n",
        "nothing outside the worktree was removed"
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
    let _ = create_worktree(
        &fixture.root,
        worktree_of(&thread),
        branch_of(&thread),
        &fixture.head(),
    )
    .expect("create");
    let main = gitez_git::Repository::open(root).expect("open");
    let mut steps = Vec::new();
    let report = prepare(&main, worktree_of(&thread), |step| steps.push(step)).expect("prepare");

    assert_eq!(
        report.copied,
        [PathBuf::from(".env"), PathBuf::from("secrets")]
    );
    let worktree = worktree_of(&thread);
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

#[test]
fn completing_a_discuss_thread_removes_only_its_scratch_folders() {
    let fixture = Fixture::new();
    let thread = fixture.discuss("How does the cache work?");
    let scratch = thread.scratch_dirs().expect("its scratch folders");
    for dir in [&scratch.tmp, &scratch.caches] {
        fs::create_dir_all(dir).expect("create");
        fs::write(dir.join("left.txt"), "left\n").expect("write");
    }
    fs::write(fixture.root.join("mine.txt"), "uncommitted\n").expect("write");
    let refs = git(&fixture.root, &["for-each-ref"]);

    let report = complete(&fixture.root, &thread, IfUncommitted::Refuse, None).expect("complete");
    assert!(report.nothing_to_remove);
    assert!(!report.worktree_removed);
    assert!(!report.branch_deleted);
    assert_eq!(report.kept, None);
    assert!(!scratch.tmp.exists());
    assert!(!scratch.caches.exists());
    assert!(
        fixture.root.join("mine.txt").exists(),
        "the user's tree is left alone"
    );
    assert_eq!(git(&fixture.root, &["for-each-ref"]), refs);

    let mut completed = thread.clone();
    completed.record_completion(&report, gitez_agent_thread::Timestamp::now());
    assert_eq!(completed, thread, "nothing to record");
}
