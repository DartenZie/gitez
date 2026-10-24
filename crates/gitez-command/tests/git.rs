use std::path::Path;
use std::process::Command;

use gitez_command::{
    Error, Git, PullMethod, PullOutcome, PushRejection, RefUpdate, ResetMode, RevertOutcome,
    UnstashOutcome, not_merged,
};

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

/// A repository whose main has commits a to e, each adding its own file
/// with the commit's name in it.
fn five_commits() -> (tempfile::TempDir, Vec<String>) {
    let dir = repository();
    let root = dir.path();
    let ids = ["a", "b", "c", "d", "e"]
        .into_iter()
        .map(|name| {
            std::fs::write(root.join(format!("{name}.txt")), format!("{name}\n")).expect("write");
            git(root, &["add", "--all"]);
            git(root, &["commit", "--quiet", "-m", name]);
            git(root, &["rev-parse", "HEAD"])
        })
        .collect();
    (dir, ids)
}

#[test]
fn rewrites_reorders_folds_and_drops_commits() {
    use gitez_command::{RewriteAction, RewriteOutcome, RewriteStep};

    let (dir, ids) = five_commits();
    let root = dir.path();
    let tip = git(root, &["rev-parse", "HEAD"]);
    std::fs::write(root.join("a.txt"), "uncommitted\n").expect("write");
    let step = |index: usize, action| RewriteStep {
        commit: ids[index].clone(),
        action,
    };
    // After a: e, moved first and reworded, then b with c squashed into it
    // and d folded in as a fixup.
    let steps = [
        step(
            4,
            RewriteAction::Reword("E, first\n\nWith a body.\n".to_owned()),
        ),
        step(1, RewriteAction::Pick),
        step(2, RewriteAction::Squash),
        step(3, RewriteAction::Fixup),
    ];
    let outcome = Git::new(root)
        .rewrite("main", Some(&ids[0]), &steps, "refs/gitez/backup/main")
        .expect("rewrite");
    assert_eq!(outcome, RewriteOutcome::Done);
    assert_eq!(git(root, &["log", "--format=%s", "main"]), "b\nE, first\na");
    assert_eq!(
        git(root, &["log", "-1", "--format=%b", "main~1"]),
        "With a body."
    );
    // The squash keeps both messages; the fixup's is left out.
    assert_eq!(git(root, &["log", "-1", "--format=%B", "main"]), "b\n\nc");
    assert_eq!(
        git(root, &["ls-tree", "--name-only", "main"]),
        "a.txt\nb.txt\nc.txt\nd.txt\ne.txt"
    );
    assert_eq!(git(root, &["rev-parse", "refs/gitez/backup/main"]), tip);
    assert_eq!(status(root), "M a.txt");
    assert!(!root.join(".git/gitez/rewrite").exists());

    // Undo puts the old commits back, keeping the uncommitted change.
    Git::new(root)
        .restore("main", "refs/gitez/backup/main")
        .expect("restore");
    assert_eq!(git(root, &["rev-parse", "main"]), tip);
    assert_eq!(status(root), "M a.txt");

    // Dropping d and e, from the root.
    let steps = [
        step(0, RewriteAction::Pick),
        step(1, RewriteAction::Pick),
        step(2, RewriteAction::Pick),
        step(3, RewriteAction::Drop),
        step(4, RewriteAction::Drop),
    ];
    Git::new(root)
        .rewrite("main", None, &steps, "refs/gitez/backup/main")
        .expect("rewrite from the root");
    assert_eq!(git(root, &["log", "--format=%s", "main"]), "c\nb\na");
}

#[test]
fn a_rewrite_stops_at_an_edit_and_gives_up_on_a_conflict() {
    use gitez_command::{RewriteAction, RewriteOutcome, RewriteStep};

    let (dir, ids) = five_commits();
    let root = dir.path();
    let step = |index: usize, action| RewriteStep {
        commit: ids[index].clone(),
        action,
    };
    let steps = [
        step(1, RewriteAction::Edit),
        step(2, RewriteAction::Pick),
        step(3, RewriteAction::Reword("D again".to_owned())),
        step(4, RewriteAction::Pick),
    ];
    let git_ = Git::new(root);
    let outcome = git_
        .rewrite("main", Some(&ids[0]), &steps, "refs/gitez/backup/main")
        .expect("rewrite");
    assert_eq!(outcome, RewriteOutcome::Stopped);
    assert_eq!(git(root, &["log", "-1", "--format=%s", "HEAD"]), "b");
    std::fs::write(root.join("b.txt"), "b, edited\n").expect("write");
    git(
        root,
        &["commit", "--quiet", "--all", "--amend", "-m", "b, edited"],
    );
    // The reword after the stop still gets its message.
    assert_eq!(
        git_.continue_rebase().expect("continue"),
        RewriteOutcome::Done
    );
    assert_eq!(
        git(root, &["log", "--format=%s", "main"]),
        "e\nD again\nc\nb, edited\na"
    );

    // A commit that changes e.txt, moved before the commit adding it,
    // conflicts.
    let tip = git(root, &["rev-parse", "HEAD"]);
    std::fs::write(root.join("e.txt"), "e, changed\n").expect("write");
    git(root, &["commit", "--quiet", "--all", "-m", "f changes e"]);
    let f = git(root, &["rev-parse", "HEAD"]);
    let e = git(root, &["rev-parse", "HEAD~1"]);
    let before = git(root, &["rev-parse", "HEAD"]);
    let steps = [
        RewriteStep {
            commit: f,
            action: RewriteAction::Pick,
        },
        RewriteStep {
            commit: e,
            action: RewriteAction::Pick,
        },
    ];
    let error = git_
        .rewrite(
            "main",
            Some(&format!("{tip}~1")),
            &steps,
            "refs/gitez/backup/main",
        )
        .expect_err("conflict");
    assert!(matches!(error, Error::Failed { .. }), "{error}");
    assert_eq!(git(root, &["rev-parse", "HEAD"]), before);
    assert_eq!(git(root, &["branch", "--show-current"]), "main");
    assert_eq!(status(root), "");
}

#[test]
fn runs_a_command_as_typed_and_streams_its_output() {
    let (dir, _) = five_commits();
    let root = dir.path();
    let mut lines = Vec::new();
    let ok = Git::new(root)
        .run_as_typed(&["log".to_owned(), "--format=%s".to_owned()], |line| {
            lines.push(line)
        })
        .expect("run");
    assert!(ok);
    assert_eq!(lines, ["e", "d", "c", "b", "a"]);

    // Standard error comes too, and a failure is reported.
    let mut lines = Vec::new();
    let ok = Git::new(root)
        .run_as_typed(&["switch".to_owned(), "nowhere".to_owned()], |line| {
            lines.push(line)
        })
        .expect("run");
    assert!(!ok);
    assert!(
        lines.iter().any(|line| line.contains("nowhere")),
        "{lines:?}"
    );
}

/// A bare repository with one commit on `main`, and two clones of it.
fn remote_and_clones() -> (tempfile::TempDir, Git, Git) {
    let dir = tempfile::tempdir().expect("temporary directory");
    let root = dir.path();
    let seed = root.join("seed");
    std::fs::create_dir(&seed).expect("create seed");
    git(&seed, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(seed.join("a.txt"), "a\n").expect("write");
    git(&seed, &["add", "a.txt"]);
    git(
        &seed,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "a",
        ],
    );
    git(root, &["clone", "--quiet", "--bare", "seed", "origin.git"]);
    let clone = |name: &str| {
        git(root, &["clone", "--quiet", "origin.git", name]);
        let path = root.join(name);
        for (key, value) in [
            ("user.name", "Test"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", ".git/hooks"),
            ("push.default", "simple"),
        ] {
            git(&path, &["config", key, value]);
        }
        Git::new(path)
    };
    let (a, b) = (clone("a"), clone("b"));
    (dir, a, b)
}

fn commit_file(git_: &Git, file: &str, text: &str) {
    let root = git_.work_dir();
    std::fs::write(root.join(file), text).expect("write");
    git(root, &["add", file]);
    git(root, &["commit", "--quiet", "-m", text.trim()]);
}

#[test]
fn publishes_fetches_and_pulls_a_branch() {
    let (_dir, a, b) = remote_and_clones();
    git(a.work_dir(), &["switch", "--quiet", "-c", "feature"]);
    commit_file(&a, "f.txt", "f\n");
    a.push("feature", Some("origin"), false).expect("publish");
    assert_eq!(
        git(a.work_dir(), &["rev-parse", "--abbrev-ref", "feature@{u}"]),
        "origin/feature"
    );

    b.fetch().expect("fetch");
    assert_eq!(
        git(b.work_dir(), &["rev-parse", "origin/feature"]),
        git(a.work_dir(), &["rev-parse", "feature"])
    );

    git(a.work_dir(), &["switch", "--quiet", "main"]);
    commit_file(&a, "a.txt", "a2\n");
    a.push("main", None, false).expect("push");
    assert_eq!(
        b.pull(PullMethod::Configured).expect("pull"),
        PullOutcome::Pulled
    );
    assert_eq!(
        git(b.work_dir(), &["rev-parse", "main"]),
        git(a.work_dir(), &["rev-parse", "main"])
    );
}

#[test]
fn a_rejected_push_says_why_and_can_be_forced() {
    let (_dir, a, b) = remote_and_clones();
    commit_file(&a, "a.txt", "from a\n");
    a.push("main", None, false).expect("push");

    commit_file(&b, "b.txt", "from b\n");
    let error = b.push("main", None, false).expect_err("not fetched");
    assert_eq!(PushRejection::of(&error), Some(PushRejection::NotFetched));
    // The lease is the last fetch, which does not have a's commit.
    assert!(b.push("main", None, true).is_err());

    b.fetch().expect("fetch");
    let error = b.push("main", None, false).expect_err("diverged");
    assert_eq!(PushRejection::of(&error), Some(PushRejection::Diverged));

    // Merged, it goes through.
    assert_eq!(
        b.pull(PullMethod::Merge).expect("pull"),
        PullOutcome::Pulled
    );
    b.push("main", None, false).expect("push the merge");

    // Rewritten, it needs force.
    git(
        b.work_dir(),
        &["commit", "--quiet", "--amend", "-m", "rewritten"],
    );
    let error = b.push("main", None, false).expect_err("rewritten");
    assert_eq!(PushRejection::of(&error), Some(PushRejection::Diverged));
    b.push("main", None, true).expect("force with lease");
    assert_eq!(
        PushRejection::of(&Error::Spawn(std::io::ErrorKind::Other.into())),
        None
    );
}

#[test]
fn a_conflicting_pull_stops_to_resolve() {
    let (_dir, a, b) = remote_and_clones();
    commit_file(&a, "a.txt", "from a\n");
    a.push("main", None, false).expect("push");
    commit_file(&b, "a.txt", "from b\n");
    assert_eq!(
        b.pull(PullMethod::Merge).expect("pull"),
        PullOutcome::Conflicts
    );
    assert_eq!(b.conflicted().expect("conflicts"), ["a.txt"]);
}

#[test]
fn creates_renames_and_deletes_branches() {
    let dir = diverged("main");
    let root = dir.path();
    let git_ = Git::new(root);
    let feature = git(root, &["rev-parse", "feature"]);

    git_.create_branch("topic", &feature, false)
        .expect("create");
    assert_eq!(git(root, &["rev-parse", "topic"]), feature);
    assert_eq!(git(root, &["branch", "--show-current"]), "main");
    assert!(git_.create_branch("topic", "main", false).is_err());

    git_.create_branch("work", "main", true)
        .expect("create and check out");
    assert_eq!(git(root, &["branch", "--show-current"]), "work");

    git_.rename_branch("work", "chores")
        .expect("rename the current branch");
    assert_eq!(git(root, &["branch", "--show-current"]), "chores");

    // "topic" has b, which "chores" lacks.
    let error = git_.delete_branch("topic", false).expect_err("not merged");
    assert!(not_merged(&error), "{error}");
    git_.delete_branch("topic", true).expect("force");
    assert!(git(root, &["branch", "--list", "topic"]).is_empty());

    git_.create_branch("done", "chores", false).expect("create");
    git_.delete_branch("done", false).expect("merged");
    assert!(git_.delete_branch("chores", true).is_err(), "checked out");
}

#[test]
fn stashes_applies_pops_drops_and_restores() {
    let dir = repository();
    let root = dir.path();
    let git_ = Git::new(root);
    std::fs::write(root.join("a.txt"), "a\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "--quiet", "-m", "a"]);
    assert!(!git_.stash(None, true).expect("nothing to stash"));

    std::fs::write(root.join("a.txt"), "a\nb\n").expect("write");
    std::fs::write(root.join("new.txt"), "new\n").expect("write");
    assert!(git_.stash(Some("Half-done parser"), false).expect("stash"));
    // Untracked files stay without `untracked`.
    assert_eq!(status(root), "?? new.txt");
    assert!(git_.stash(None, true).expect("stash untracked"));
    assert_eq!(status(root), "");
    let list = git(root, &["stash", "list", "--format=%gs"]);
    assert!(list.ends_with("On main: Half-done parser"), "{list}");

    // stash@{1} is the parser change; apply keeps it, pop drops it.
    assert_eq!(
        git_.unstash(1, false).expect("apply"),
        UnstashOutcome::Applied
    );
    assert_eq!(status(root), "M a.txt");
    assert_eq!(git(root, &["stash", "list"]).lines().count(), 2);
    git(root, &["checkout", "--", "a.txt"]);
    assert_eq!(git_.unstash(1, true).expect("pop"), UnstashOutcome::Applied);
    assert_eq!(git(root, &["stash", "list"]).lines().count(), 1);

    // A change to the same line conflicts; the stash stays.
    git(root, &["commit", "--quiet", "-am", "b"]);
    git(root, &["stash", "pop", "--quiet"]);
    std::fs::write(root.join("a.txt"), "a\nours\n").expect("write");
    assert!(git_.stash(None, false).expect("stash ours"));
    std::fs::write(root.join("a.txt"), "a\ntheirs\n").expect("write");
    git(root, &["commit", "--quiet", "-am", "theirs"]);
    assert_eq!(
        git_.unstash(0, true).expect("pop"),
        UnstashOutcome::Conflicts
    );
    assert_eq!(git(root, &["stash", "list"]).lines().count(), 1);
    git(root, &["reset", "--quiet", "--hard"]);

    let id = git(root, &["rev-parse", "stash@{0}"]);
    git_.drop_stash(0).expect("drop");
    assert!(git(root, &["stash", "list"]).is_empty());
    git_.restore_stash(&id, "Brought back").expect("restore");
    assert_eq!(git(root, &["rev-parse", "stash@{0}"]), id);
    assert!(git_.drop_stash(5).is_err());
}

#[test]
fn updates_refs_together_or_not_at_all() {
    let dir = diverged("main");
    let root = dir.path();
    let git_ = Git::new(root);
    let (main, feature) = (
        git(root, &["rev-parse", "main"]),
        git(root, &["rev-parse", "feature"]),
    );
    let update = |name: &str, old: Option<&str>, new: Option<&str>| RefUpdate {
        name: format!("refs/heads/{name}"),
        old: old.map(str::to_owned),
        new: new.map(str::to_owned),
    };

    git_.update_refs(&[
        update("copy", None, Some(&feature)),
        update("feature", Some(&feature), Some(&main)),
    ])
    .expect("update");
    assert_eq!(git(root, &["rev-parse", "copy"]), feature);
    assert_eq!(git(root, &["rev-parse", "feature"]), main);

    // "feature" is no longer where this expects, so "copy" stays too.
    let error = git_
        .update_refs(&[
            update("copy", Some(&feature), None),
            update("feature", Some(&feature), Some(&feature)),
        ])
        .expect_err("stale");
    assert!(matches!(error, Error::Failed { .. }));
    assert_eq!(git(root, &["rev-parse", "copy"]), feature);

    git_.update_refs(&[update("copy", Some(&feature), None)])
        .expect("delete");
    assert!(git(root, &["branch", "--list", "copy"]).is_empty());
}

#[test]
fn resets_softly_or_keeping_local_changes() {
    let dir = repository();
    let root = dir.path();
    let git_ = Git::new(root);
    std::fs::write(root.join("a.txt"), "a\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "--quiet", "-m", "a"]);
    let first = git(root, &["rev-parse", "HEAD"]);
    std::fs::write(root.join("a.txt"), "a\nb\n").expect("write");
    git(root, &["commit", "--quiet", "-am", "b"]);
    let second = git(root, &["rev-parse", "HEAD"]);

    // Soft: the commit's change comes back staged.
    git_.reset(&first, ResetMode::Soft).expect("soft");
    assert_eq!(status(root), "M  a.txt");
    git_.reset(&second, ResetMode::Soft).expect("soft back");
    assert_eq!(status(root), "");

    // Keep: the files follow, local changes elsewhere stay.
    std::fs::write(root.join("other.txt"), "local\n").expect("write");
    git_.reset(&first, ResetMode::Keep).expect("keep");
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).expect("read"),
        "a\n"
    );
    assert_eq!(status(root), "?? other.txt");

    git_.switch_detached(&second).expect("detach");
    assert_eq!(git(root, &["branch", "--show-current"]), "");
}

#[test]
fn discards_files_and_brings_them_back() {
    let dir = repository();
    let root = dir.path();
    let git_ = Git::new(root);
    std::fs::write(root.join("a.txt"), "a\n").expect("write");
    std::fs::write(root.join("[b].txt"), "b\n").expect("write");
    git_.stage_all().expect("stage");
    git_.commit("Add a and b").expect("commit");
    std::fs::write(root.join("a.txt"), "a\nolder\n").expect("write");
    assert!(git_.stash(Some("Older"), false).expect("stash"));
    std::fs::write(root.join("a.txt"), "a\nstaged\n").expect("write");
    git_.stage(&["a.txt".to_owned()]).expect("stage");
    std::fs::write(root.join("a.txt"), "a\nstaged\nunstaged\n").expect("write");
    std::fs::write(root.join("[b].txt"), "B\n").expect("write");
    std::fs::write(root.join("new.txt"), "new\n").expect("write");
    let dirty = status(root);

    let saved = git_
        .discard(Some(&["a.txt".to_owned(), "new.txt".to_owned()]))
        .expect("discard")
        .expect("something was discarded");
    assert_eq!(status(root), "M [b].txt");
    // The stashes already there are left alone.
    assert_eq!(git(root, &["stash", "list"]), "stash@{0}: On main: Older");

    git_.restore_discarded(&saved).expect("restore");
    assert_eq!(status(root), dirty);
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).expect("read"),
        "a\nstaged\nunstaged\n"
    );

    git_.discard(None).expect("discard everything");
    assert_eq!(status(root), "");
    assert_eq!(git_.discard(None).expect("discard nothing"), None);
}

#[test]
fn discards_a_hunk_from_the_working_tree_and_brings_it_back() {
    let dir = repository();
    let git_ = Git::new(dir.path());
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").expect("write");
    git_.stage_all().expect("stage");
    git_.commit("Add a").expect("commit");
    std::fs::write(dir.path().join("a.txt"), "one\nTWO\n").expect("write");
    let patch =
        "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n";

    git_.apply_to_worktree(patch, true)
        .expect("discard the hunk");
    assert_eq!(status(dir.path()), "");
    git_.apply_to_worktree(patch, false).expect("bring it back");
    assert_eq!(status(dir.path()), "M a.txt");
}

#[test]
fn checks_out_a_remote_branch_and_deletes_it_on_the_remote() {
    let (_dir, a, b) = remote_and_clones();
    git(a.work_dir(), &["switch", "--quiet", "-c", "feature/x"]);
    commit_file(&a, "x.txt", "x\n");
    a.push("feature/x", Some("origin"), false).expect("publish");
    b.fetch().expect("fetch");

    b.track("origin", "feature/x").expect("track");
    let root = b.work_dir();
    assert_eq!(git(root, &["branch", "--show-current"]), "feature/x");
    assert_eq!(
        git(root, &["rev-parse", "--abbrev-ref", "feature/x@{upstream}"]),
        "origin/feature/x"
    );
    assert!(b.track("origin", "feature/x").is_err(), "it exists already");

    b.delete_remote_branch("origin", "feature/x")
        .expect("delete");
    assert_eq!(
        git(root, &["branch", "--remotes", "--list", "origin/feature/x"]),
        ""
    );
    a.fetch().expect("fetch");
    assert_eq!(
        git(
            a.work_dir(),
            &["ls-remote", "--heads", "origin", "feature/x"]
        ),
        ""
    );
}

#[test]
fn creates_pushes_and_deletes_tags() {
    let (_dir, a, _b) = remote_and_clones();
    let root = a.work_dir();
    a.create_tag("v1", "HEAD", None).expect("lightweight");
    a.create_tag("v2", "main", Some("Release two\n"))
        .expect("annotated");
    assert_eq!(git(root, &["cat-file", "-t", "v1"]), "commit");
    assert_eq!(git(root, &["cat-file", "-t", "v2"]), "tag");
    assert_eq!(
        git(root, &["tag", "-l", "--format=%(contents)", "v2"]),
        "Release two"
    );
    assert!(
        a.create_tag("v1", "HEAD", None).is_err(),
        "it exists already"
    );

    a.push_tag("origin", "v2").expect("push");
    assert!(git(root, &["ls-remote", "--tags", "origin", "v2"]).ends_with("refs/tags/v2"));
    a.delete_remote_tag("origin", "v2")
        .expect("delete on the remote");
    assert_eq!(git(root, &["ls-remote", "--tags", "origin", "v2"]), "");

    a.delete_tag("v1").expect("delete");
    assert_eq!(git(root, &["tag", "-l"]), "v2");
}

#[test]
fn reverts_a_commit_and_gives_up_on_a_conflict() {
    let dir = repository();
    let root = dir.path();
    let git_ = Git::new(root);
    std::fs::write(root.join("a.txt"), "one\n").expect("write");
    git_.stage_all().expect("stage");
    git_.commit("One").expect("commit");
    std::fs::write(root.join("a.txt"), "two\n").expect("write");
    git_.stage_all().expect("stage");
    git_.commit("Two").expect("commit");
    let two = git(root, &["rev-parse", "HEAD"]);
    std::fs::write(root.join("a.txt"), "three\n").expect("write");
    git_.stage_all().expect("stage");
    git_.commit("Three").expect("commit");
    let three = git(root, &["rev-parse", "HEAD"]);

    // "Two" changed a line "Three" changed again.
    assert_eq!(
        git_.revert(&two, None).expect("revert"),
        RevertOutcome::Conflicts
    );
    assert_eq!(git(root, &["rev-parse", "HEAD"]), three);
    assert_eq!(status(root), "");

    assert_eq!(
        git_.revert(&three, None).expect("revert"),
        RevertOutcome::Reverted
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).expect("read"),
        "two\n"
    );
    assert_eq!(git(root, &["log", "-1", "--format=%s"]), "Revert \"Three\"");
}

#[test]
fn resets_three_ways_and_discards_only_tracked_files() {
    let dir = repository();
    let root = dir.path();
    let git_ = Git::new(root);
    std::fs::write(root.join("a.txt"), "one\n").expect("write");
    git_.stage_all().expect("stage");
    git_.commit("One").expect("commit");
    let one = git(root, &["rev-parse", "HEAD"]);
    std::fs::write(root.join("a.txt"), "two\n").expect("write");
    git_.stage_all().expect("stage");
    git_.commit("Two").expect("commit");
    let two = git(root, &["rev-parse", "HEAD"]);

    git_.reset(&one, ResetMode::Soft).expect("soft");
    assert_eq!(status(root), "M  a.txt");
    git_.reset(&two, ResetMode::Soft).expect("back");
    git_.reset(&one, ResetMode::Mixed).expect("mixed");
    assert_eq!(status(root), "M a.txt");
    git_.reset(&two, ResetMode::Mixed).expect("back");
    assert_eq!(status(root), "");

    std::fs::write(root.join("a.txt"), "local\n").expect("write");
    std::fs::write(root.join("new.txt"), "new\n").expect("write");
    let saved = git_
        .discard_tracked()
        .expect("discard")
        .expect("there were changes");
    assert_eq!(status(root), "?? new.txt");
    git_.reset(&one, ResetMode::Hard).expect("hard");
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).expect("read"),
        "one\n"
    );
    git_.reset(&two, ResetMode::Hard).expect("back");
    git_.restore_discarded(&saved).expect("restore");
    assert_eq!(status(root), "M a.txt\n?? new.txt");
}

#[test]
fn adds_renames_repoints_and_removes_remotes() {
    let (dir, a, _b) = remote_and_clones();
    let root = a.work_dir();
    let url = dir.path().join("origin.git").display().to_string();
    a.add_remote("mirror", &url).expect("add");
    assert!(a.add_remote("mirror", &url).is_err(), "it exists already");
    a.fetch_remote("mirror").expect("fetch");
    assert!(
        git(
            root,
            &["for-each-ref", "--format=%(refname)", "refs/remotes/mirror"]
        )
        .contains("refs/remotes/mirror/main")
    );

    a.set_upstream("main", Some("mirror/main"))
        .expect("set upstream");
    assert_eq!(
        git(root, &["rev-parse", "--abbrev-ref", "main@{upstream}"]),
        "mirror/main"
    );
    a.rename_remote("mirror", "backup").expect("rename");
    assert_eq!(
        git(root, &["rev-parse", "--abbrev-ref", "main@{upstream}"]),
        "backup/main"
    );
    a.set_remote_url("backup", "https://example.com/x.git")
        .expect("set url");
    assert_eq!(
        git(root, &["remote", "get-url", "backup"]),
        "https://example.com/x.git"
    );
    a.set_upstream("main", None).expect("unset upstream");
    assert!(!git(root, &["config", "--list"]).contains("branch.main.merge"));

    a.remove_remote("backup").expect("remove");
    assert_eq!(git(root, &["remote"]), "origin");
    assert_eq!(
        git(
            root,
            &["for-each-ref", "--format=%(refname)", "refs/remotes/backup"]
        ),
        ""
    );
}
