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

#[test]
fn switches_to_a_branch_unless_changes_would_be_lost() {
    let dir = repository();
    let root = dir.path();
    std::fs::write(root.join("a.txt"), "main\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "--quiet", "-m", "main"]);
    git(root, &["switch", "--quiet", "-c", "feature"]);
    std::fs::write(root.join("a.txt"), "feature\n").expect("write");
    git(root, &["commit", "--quiet", "-am", "feature"]);
    git(root, &["switch", "--quiet", "main"]);

    let repo = Git::new(root);
    repo.switch("feature").expect("switch");
    assert_eq!(git(root, &["branch", "--show-current"]), "feature");

    // An edit that the other branch's version would overwrite.
    std::fs::write(root.join("a.txt"), "edited\n").expect("write");
    let error = repo.switch("main").expect_err("changes would be lost");
    assert!(matches!(error, Error::Failed { .. }), "{error}");
    assert_eq!(git(root, &["branch", "--show-current"]), "feature");
}

/// A repository where `feature` adds b.txt and `main` then adds c.txt,
/// on `checked_out`.
fn diverged(checked_out: &str) -> tempfile::TempDir {
    let dir = repository();
    let root = dir.path();
    std::fs::write(root.join("a.txt"), "a\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "--quiet", "-m", "a"]);
    git(root, &["switch", "--quiet", "-c", "feature"]);
    std::fs::write(root.join("b.txt"), "b\n").expect("write");
    git(root, &["add", "b.txt"]);
    git(root, &["commit", "--quiet", "-m", "b"]);
    git(root, &["switch", "--quiet", "main"]);
    std::fs::write(root.join("c.txt"), "c\n").expect("write");
    git(root, &["add", "c.txt"]);
    git(root, &["commit", "--quiet", "-m", "c"]);
    git(root, &["switch", "--quiet", checked_out]);
    dir
}

#[test]
fn merges_a_branch_each_way() {
    use gitez_command::MergeMethod;

    let dir = diverged("feature");
    let root = dir.path();
    let outcome = Git::new(root)
        .merge_branch(
            "feature",
            "main",
            MergeMethod::Merge,
            "Merge feature\n\nWhy.",
            Some("feature"),
        )
        .expect("merge");
    assert_eq!(outcome, gitez_command::MergeOutcome::Merged);
    assert_eq!(git(root, &["branch", "--show-current"]), "main");
    assert_eq!(
        git(root, &["log", "-1", "--format=%B"]),
        "Merge feature\n\nWhy."
    );
    assert_eq!(git(root, &["rev-list", "--count", "--merges", "HEAD"]), "1");

    let dir = diverged("feature");
    let root = dir.path();
    let outcome = Git::new(root)
        .merge_branch("feature", "main", MergeMethod::Squash, "Add b", None)
        .expect("squash");
    assert_eq!(outcome, gitez_command::MergeOutcome::Merged);
    assert_eq!(git(root, &["log", "--format=%s"]), "Add b\nc\na");
    assert_eq!(status(root), "");

    let dir = diverged("main");
    let root = dir.path();
    let outcome = Git::new(root)
        .merge_branch("feature", "main", MergeMethod::Rebase, "", None)
        .expect("rebase");
    assert_eq!(outcome, gitez_command::MergeOutcome::Merged);
    assert_eq!(git(root, &["log", "--format=%s"]), "b\nc\na");
    assert_eq!(
        git(root, &["rev-parse", "main"]),
        git(root, &["rev-parse", "feature"])
    );
    assert_eq!(git(root, &["branch", "--show-current"]), "main");

    // Main has a commit of its own: no fast-forward, and nothing changes.
    let dir = diverged("feature");
    let root = dir.path();
    let main = git(root, &["rev-parse", "main"]);
    let error = Git::new(root)
        .merge_branch(
            "feature",
            "main",
            MergeMethod::FastForward,
            "",
            Some("feature"),
        )
        .expect_err("diverged");
    assert!(matches!(error, Error::Failed { .. }), "{error}");
    assert_eq!(git(root, &["rev-parse", "main"]), main);
    assert_eq!(git(root, &["branch", "--show-current"]), "feature");
}

#[test]
fn rebases_a_branch_and_returns_to_the_one_checked_out() {
    let dir = diverged("main");
    let root = dir.path();
    let main = git(root, &["rev-parse", "main"]);
    Git::new(root)
        .rebase_branch("feature", "main", Some("main"))
        .expect("rebase");
    assert_eq!(git(root, &["log", "--format=%s", "feature"]), "b\nc\na");
    assert_eq!(git(root, &["rev-parse", "main"]), main);
    assert_eq!(git(root, &["branch", "--show-current"]), "main");

    // A conflict gives the rebase up.
    let dir = diverged("main");
    let root = dir.path();
    std::fs::write(root.join("b.txt"), "main\n").expect("write");
    git(root, &["add", "b.txt"]);
    git(root, &["commit", "--quiet", "-m", "main adds b"]);
    let feature = git(root, &["rev-parse", "feature"]);
    let error = Git::new(root)
        .rebase_branch("feature", "main", Some("main"))
        .expect_err("conflict");
    assert!(matches!(error, Error::Failed { .. }), "{error}");
    assert_eq!(git(root, &["rev-parse", "feature"]), feature);
    assert_eq!(git(root, &["branch", "--show-current"]), "main");
    assert_eq!(status(root), "");
}

#[test]
fn a_conflicting_merge_is_undone_or_left_to_resolve() {
    use gitez_command::{MergeMethod, MergeOutcome, Side};

    let dir = diverged("feature");
    let root = dir.path();
    std::fs::write(root.join("a.txt"), "feature\n").expect("write");
    git(root, &["commit", "--quiet", "-am", "feature edits a"]);
    git(root, &["switch", "--quiet", "main"]);
    std::fs::write(root.join("a.txt"), "main\n").expect("write");
    git(root, &["commit", "--quiet", "-am", "main edits a"]);
    git(root, &["switch", "--quiet", "feature"]);
    let main = git(root, &["rev-parse", "main"]);
    let feature = git(root, &["rev-parse", "feature"]);
    let repo = Git::new(root);

    // A squash or a rebase cannot stop half way: they are undone.
    for how in [MergeMethod::Squash, MergeMethod::Rebase] {
        let error = repo
            .merge_branch("feature", "main", how, "Merge", Some("feature"))
            .expect_err("conflict");
        assert!(matches!(error, Error::Failed { .. }), "{how:?}: {error}");
        assert_eq!(status(root), "", "{how:?}");
        assert_eq!(
            git(root, &["branch", "--show-current"]),
            "feature",
            "{how:?}"
        );
        assert_eq!(git(root, &["rev-parse", "main"]), main, "{how:?}");
        assert_eq!(git(root, &["rev-parse", "feature"]), feature, "{how:?}");
    }

    // A merge commit stops on main with the conflict to resolve, in the
    // diff3 style.
    let outcome = repo
        .merge_branch(
            "feature",
            "main",
            MergeMethod::Merge,
            "Merge feature",
            Some("feature"),
        )
        .expect("stopped");
    assert_eq!(outcome, MergeOutcome::Conflicts);
    assert_eq!(git(root, &["branch", "--show-current"]), "main");
    assert_eq!(repo.conflicted().expect("conflicted"), ["a.txt"]);
    let text = std::fs::read_to_string(root.join("a.txt")).expect("read");
    assert!(text.contains("|||||||"), "{text}");
    // Given up: as before.
    repo.abort_merge().expect("abort");
    assert_eq!(status(root), "");
    assert_eq!(git(root, &["rev-parse", "main"]), main);

    // Resolved with theirs and committed with the prepared message.
    repo.merge_branch("feature", "main", MergeMethod::Merge, "Merge feature", None)
        .expect("stopped");
    repo.take_side(&["a.txt".to_owned()], Side::Theirs)
        .expect("theirs");
    assert!(repo.conflicted().expect("conflicted").is_empty());
    repo.commit_merge().expect("commit");
    assert_eq!(git(root, &["log", "-1", "--format=%s"]), "Merge feature");
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).expect("read"),
        "feature\n"
    );
    assert_eq!(status(root), "");
}

#[test]
fn sets_and_removes_a_config_value() {
    let dir = repository();
    let root = dir.path();
    let repo = Git::new(root);
    // Removing what is not set is fine.
    repo.set_config("gitez.testCommand", None).expect("unset");
    repo.set_config("gitez.testCommand", Some("cargo test --workspace"))
        .expect("set");
    assert_eq!(
        git(root, &["config", "--local", "gitez.testCommand"]),
        "cargo test --workspace"
    );
    repo.set_config("gitez.testCommand", None).expect("unset");
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "--local", "gitez.testCommand"])
        .output()
        .expect("git runs");
    assert!(!output.status.success());
}

#[test]
fn cherry_picks_onto_a_branch_and_gives_up_on_a_conflict() {
    use gitez_command::PickOptions;

    // b.txt is feature's commit; pick it onto main from feature, with a
    // change in the working tree stashed along the way.
    let dir = diverged("feature");
    let root = dir.path();
    let b = git(root, &["rev-parse", "feature"]);
    std::fs::write(root.join("a.txt"), "edited\n").expect("write");
    let options = PickOptions {
        note_source: true,
        stash: true,
        ..PickOptions::default()
    };
    Git::new(root)
        .cherry_pick(std::slice::from_ref(&b), "main", options, Some("feature"))
        .expect("cherry-pick");
    assert_eq!(git(root, &["branch", "--show-current"]), "main");
    assert_eq!(git(root, &["log", "--format=%s", "main"]), "b\nc\na");
    assert!(
        git(root, &["log", "-1", "--format=%b"])
            .contains(&format!("cherry picked from commit {b}"))
    );
    assert_eq!(status(root), "M a.txt");

    // Main changes b.txt its own way: the pick conflicts and is undone.
    let dir = diverged("feature");
    let root = dir.path();
    let b = git(root, &["rev-parse", "feature"]);
    git(root, &["switch", "--quiet", "main"]);
    std::fs::write(root.join("b.txt"), "main\n").expect("write");
    git(root, &["add", "b.txt"]);
    git(root, &["commit", "--quiet", "-m", "main adds b"]);
    git(root, &["switch", "--quiet", "feature"]);
    let main = git(root, &["rev-parse", "main"]);
    let error = Git::new(root)
        .cherry_pick(&[b], "main", PickOptions::default(), Some("feature"))
        .expect_err("conflict");
    assert!(matches!(error, Error::Failed { .. }), "{error}");
    assert_eq!(git(root, &["rev-parse", "main"]), main);
    assert_eq!(git(root, &["branch", "--show-current"]), "feature");
    assert_eq!(status(root), "");
}
