use std::path::Path;
use std::process::Command;

use gitez_command::{Error, Git};

/// `git <args>` in `dir`, returning its trimmed output.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
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

/// A new repository that does not depend on the user's configuration.
fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temporary directory");
    git(dir.path(), &["init", "--quiet", "--initial-branch=main"]);
    for (key, value) in [
        ("user.name", "Test"),
        ("user.email", "test@example.com"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", ".git/hooks"),
    ] {
        git(dir.path(), &["config", key, value]);
    }
    dir
}

fn status(dir: &Path) -> String {
    git(dir, &["status", "--porcelain", "--untracked-files=all"])
}

#[test]
fn stages_and_unstages_paths_before_the_first_commit() {
    let dir = repository();
    std::fs::write(dir.path().join("a.txt"), "a\n").expect("write");
    std::fs::write(dir.path().join("[b].txt"), "b\n").expect("write");
    std::fs::write(dir.path().join("-c.txt"), "c\n").expect("write");
    let git_ = Git::new(dir.path());

    git_.stage(&["[b].txt".to_owned(), "-c.txt".to_owned()])
        .expect("stage");
    assert_eq!(status(dir.path()), "A  -c.txt\nA  [b].txt\n?? a.txt");

    git_.unstage(&["[b].txt".to_owned()]).expect("unstage");
    assert_eq!(status(dir.path()), "A  -c.txt\n?? [b].txt\n?? a.txt");

    git_.stage_all().expect("stage all");
    assert_eq!(status(dir.path()), "A  -c.txt\nA  [b].txt\nA  a.txt");
    git_.unstage_all().expect("unstage all");
    assert_eq!(status(dir.path()), "?? -c.txt\n?? [b].txt\n?? a.txt");
}

#[test]
fn stages_deletions_and_commits_the_staged_changes() {
    let dir = repository();
    std::fs::write(dir.path().join("keep.txt"), "keep\n").expect("write");
    std::fs::write(dir.path().join("gone.txt"), "gone\n").expect("write");
    let git_ = Git::new(dir.path());
    git_.stage_all().expect("stage all");
    git_.commit("First\n\nWith a body.\n").expect("commit");
    assert_eq!(
        git(dir.path(), &["log", "--format=%B"]),
        "First\n\nWith a body."
    );

    std::fs::remove_file(dir.path().join("gone.txt")).expect("remove");
    std::fs::write(dir.path().join("keep.txt"), "kept\n").expect("write");
    git_.stage(&["gone.txt".to_owned()])
        .expect("stage deletion");
    git_.commit("Remove gone.txt").expect("commit");

    assert_eq!(status(dir.path()), "M keep.txt");
    assert_eq!(
        git(dir.path(), &["show", "--name-status", "--format=%s"]),
        "Remove gone.txt\n\nD\tgone.txt"
    );
}

#[test]
fn a_rejected_commit_reports_the_hook_output() {
    let dir = repository();
    let hook = dir.path().join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\necho 'lint failed: a.txt'\nexit 1\n").expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
            .expect("make the hook executable");
    }
    std::fs::write(dir.path().join("a.txt"), "a\n").expect("write");
    let git_ = Git::new(dir.path());
    git_.stage_all().expect("stage all");

    match git_.commit("Add a") {
        Err(Error::Failed { output, .. }) => assert_eq!(output, "lint failed: a.txt"),
        other => panic!("expected the hook to reject the commit, got {other:?}"),
    }
    assert_eq!(status(dir.path()), "A  a.txt");
}

#[test]
fn nothing_staged_is_an_error() {
    let dir = repository();
    std::fs::write(dir.path().join("a.txt"), "a\n").expect("write");
    let error = Git::new(dir.path())
        .commit("Empty")
        .expect_err("nothing to commit");
    assert!(matches!(error, Error::Failed { .. }), "{error:?}");
}

#[test]
fn amends_the_message_and_adds_the_staged_changes() {
    let dir = repository();
    let git_ = Git::new(dir.path());
    std::fs::write(dir.path().join("a.txt"), "a\n").expect("write");
    git_.stage_all().expect("stage");
    git_.commit("First").expect("commit");
    std::fs::write(dir.path().join("b.txt"), "b\n").expect("write");
    git_.stage_all().expect("stage");
    git_.commit("Second").expect("commit");
    let parent = git(dir.path(), &["rev-parse", "HEAD~"]);

    std::fs::write(dir.path().join("c.txt"), "c\n").expect("write");
    git_.stage(&["c.txt".to_owned()]).expect("stage");
    git_.amend("Second, amended\n\nWith c.txt too.")
        .expect("amend");

    assert_eq!(git(dir.path(), &["rev-parse", "HEAD~"]), parent);
    assert_eq!(
        git(dir.path(), &["log", "-1", "--format=%B"]),
        "Second, amended\n\nWith c.txt too."
    );
    assert_eq!(
        git(dir.path(), &["show", "--name-only", "--format="]),
        "b.txt\nc.txt"
    );
    assert_eq!(status(dir.path()), "");
}

#[test]
fn applies_a_patch_to_the_index_and_takes_it_back() {
    let dir = repository();
    let git_ = Git::new(dir.path());
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").expect("write");
    git_.stage_all().expect("stage");
    git_.commit("Add a").expect("commit");
    std::fs::write(dir.path().join("a.txt"), "one\nTWO\n").expect("write");
    let patch =
        "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n";

    git_.apply_to_index(patch, false).expect("stage the hunk");
    assert_eq!(status(dir.path()), "M  a.txt");
    git_.apply_to_index(patch, true).expect("unstage the hunk");
    assert_eq!(status(dir.path()), "M a.txt");

    let error = git_
        .apply_to_index(patch, true)
        .expect_err("the hunk is not staged any more");
    assert!(matches!(error, Error::Failed { .. }), "{error:?}");
}
