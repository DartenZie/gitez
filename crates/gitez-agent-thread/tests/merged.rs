//! Completing a thread whose local pull request was merged: its branch is
//! deleted with the worktree whatever the merge method, unless something
//! on it would be lost.

mod common;

use std::fs;

use common::{Fixture, at, git};
use gitez_agent_thread::{
    AgentThread, BranchKept, DeletedBranch, IfUncommitted, Kept, Merged, Timestamp, Uncommitted,
    complete, create_worktree,
};

/// A thread with its worktree made and two commits of its own on its
/// branch, the last run having left `uncommitted` changes (`Some(0)`: a
/// clean worktree). Returns the thread and its branch's tip.
fn worked(fixture: &Fixture, prompt: &str, uncommitted: u32) -> (AgentThread, String) {
    let mut thread = fixture.create(prompt);
    let created = create_worktree(
        &fixture.root,
        &thread.worktree,
        &thread.branch,
        &thread.start.commit,
    )
    .expect("create");
    thread.worktree_record = Some(created.record);
    thread.uncommitted = Some(Uncommitted {
        checked: Timestamp::now(),
        count: uncommitted,
        paths: Vec::new(),
        commit_error: None,
    });
    for n in 1..=2 {
        let name = format!("{}-{n}.txt", thread.id);
        fs::write(thread.worktree.join(&name), format!("{n}\n")).expect("write");
        git(&thread.worktree, &["add", "-A"]);
        git(
            &thread.worktree,
            &["commit", "--quiet", "-m", &format!("Add {name}")],
        );
    }
    let head = fixture.tip(&thread.branch);
    (thread, head)
}

/// What the review store records once pull request #1 of `thread` is
/// merged into main at `head`.
fn merged(head: &str) -> Merged {
    Merged {
        number: 1,
        head: head.to_owned(),
        base: "main".to_owned(),
    }
}

/// Moves main on by a commit (adding `name`), so a merge is not a
/// fast-forward.
fn main_moves_on(fixture: &Fixture, name: &str) {
    fs::write(fixture.root.join(name), "main\n").expect("write");
    git(&fixture.root, &["add", "-A"]);
    git(
        &fixture.root,
        &["commit", "--quiet", "-m", "Meanwhile on main"],
    );
}

fn exists(fixture: &Fixture, branch: &str) -> bool {
    git(
        &fixture.root,
        &["branch", "--list", "--format=%(refname:short)", branch],
    ) == branch
}

/// Completes `thread` after `merge`, keeping a worktree with uncommitted
/// changes as an automatic completion does, and records the report.
fn complete_merged(fixture: &Fixture, thread: &mut AgentThread, merge: &Merged) {
    let report =
        complete(&fixture.root, thread, IfUncommitted::Keep, Some(merge)).expect("complete");
    thread.record_completion(&report, at("2026-11-13T12:00:00Z"));
}

#[test]
fn a_merge_commit_deletes_the_branch_and_records_where_it_was() {
    let fixture = Fixture::new();
    let (mut thread, head) = worked(&fixture, "Merge commit", 0);
    main_moves_on(&fixture, "main.txt");
    git(
        &fixture.root,
        &["merge", "--quiet", "--no-ff", "-m", "Merge", &thread.branch],
    );
    let report = complete(
        &fixture.root,
        &thread,
        IfUncommitted::Keep,
        Some(&merged(&head)),
    )
    .expect("complete");
    assert!(
        report.worktree_removed && report.branch_deleted,
        "{report:?}"
    );
    assert_eq!(report.deleted_at.as_deref(), Some(head.as_str()));
    assert_eq!(report.branch_kept, None);
    assert!(!thread.worktree.exists());
    assert!(!exists(&fixture, &thread.branch));

    thread.record_completion(&report, at("2026-11-13T12:00:00Z"));
    assert_eq!(
        thread.deleted_branch,
        Some(DeletedBranch {
            commit: head.clone(),
            at: at("2026-11-13T12:00:00Z"),
        })
    );
    assert!(thread.warnings.is_empty(), "{:?}", thread.warnings);
    // It can be made again where it was.
    git(&fixture.root, &["branch", &thread.branch, &head]);
}

#[test]
fn a_squash_deletes_the_branch_git_does_not_see_merged() {
    let fixture = Fixture::new();
    let (mut thread, head) = worked(&fixture, "Squash", 0);
    main_moves_on(&fixture, "main.txt");
    git(
        &fixture.root,
        &["merge", "--quiet", "--squash", &thread.branch],
    );
    git(&fixture.root, &["commit", "--quiet", "-m", "Squash (#1)"]);
    // git itself would refuse: the branch's commits are not in main.
    assert_ne!(
        git(
            &fixture.root,
            &["branch", "--merged", "main", "--list", &thread.branch]
        ),
        thread.branch
    );
    complete_merged(&fixture, &mut thread, &merged(&head));
    assert!(!exists(&fixture, &thread.branch));
    assert_eq!(
        thread
            .deleted_branch
            .as_ref()
            .map(|deleted| &deleted.commit),
        Some(&head)
    );
}

#[test]
fn a_rebase_and_merge_deletes_the_rewritten_branch() {
    let fixture = Fixture::new();
    let (mut thread, before) = worked(&fixture, "Rebase", 0);
    main_moves_on(&fixture, "main.txt");
    // As "Rebase and merge" does: rebased in the thread's worktree, then
    // main fast-forwarded; the pull request follows the rewritten head.
    git(&thread.worktree, &["rebase", "--quiet", "main"]);
    let rebased = fixture.tip(&thread.branch);
    assert_ne!(rebased, before);
    git(
        &fixture.root,
        &["merge", "--quiet", "--ff-only", &thread.branch],
    );
    complete_merged(&fixture, &mut thread, &merged(&rebased));
    assert!(!exists(&fixture, &thread.branch));
    assert_eq!(
        thread
            .deleted_branch
            .as_ref()
            .map(|deleted| &deleted.commit),
        Some(&rebased)
    );

    // A head recorded before the rebase still lets it go: its tip is in
    // main.
    let (mut other, stale) = worked(&fixture, "Rebase again", 0);
    main_moves_on(&fixture, "again.txt");
    git(&other.worktree, &["rebase", "--quiet", "main"]);
    git(
        &fixture.root,
        &["merge", "--quiet", "--ff-only", &other.branch],
    );
    assert_ne!(fixture.tip(&other.branch), stale);
    complete_merged(&fixture, &mut other, &merged(&stale));
    assert!(!exists(&fixture, &other.branch));
}

#[test]
fn commits_after_the_merge_keep_the_branch_and_say_why() {
    let fixture = Fixture::new();
    let (mut thread, head) = worked(&fixture, "Moved on", 0);
    git(
        &fixture.root,
        &["merge", "--quiet", "--squash", &thread.branch],
    );
    git(&fixture.root, &["commit", "--quiet", "-m", "Squash (#1)"]);
    fs::write(thread.worktree.join("later.txt"), "later\n").expect("write");
    git(&thread.worktree, &["add", "-A"]);
    git(
        &thread.worktree,
        &["commit", "--quiet", "-m", "After the merge"],
    );
    let tip = fixture.tip(&thread.branch);

    let report = complete(
        &fixture.root,
        &thread,
        IfUncommitted::Keep,
        Some(&merged(&head)),
    )
    .expect("complete");
    assert!(report.worktree_removed, "{report:?}");
    assert!(!report.branch_deleted);
    assert_eq!(report.branch_kept, Some(BranchKept::Unmerged));
    assert_eq!(fixture.tip(&thread.branch), tip, "the branch is untouched");

    thread.record_completion(&report, Timestamp::now());
    assert_eq!(thread.deleted_branch, None);
    let warning = format!(
        "GitEZ kept the branch {} because it has commits that its pull request did not merge.",
        thread.branch
    );
    assert_eq!(thread.warnings, std::slice::from_ref(&warning));
    assert_eq!(thread.branch_kept_warning(), Some(warning.as_str()));
    // Said once.
    thread.record_completion(&report, Timestamp::now());
    assert_eq!(thread.warnings.len(), 1);
}

#[test]
fn a_branch_checked_out_elsewhere_is_kept() {
    let fixture = Fixture::new();
    let (mut thread, head) = worked(&fixture, "Elsewhere", 0);
    git(
        &fixture.root,
        &["merge", "--quiet", "--no-ff", "-m", "Merge", &thread.branch],
    );
    let other = fixture.root.parent().expect("base").join("other tree");
    git(
        &fixture.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "--force",
            other.to_str().expect("utf-8"),
            &thread.branch,
        ],
    );
    let report = complete(
        &fixture.root,
        &thread,
        IfUncommitted::Keep,
        Some(&merged(&head)),
    )
    .expect("complete");
    assert!(
        report.worktree_removed && !report.branch_deleted,
        "{report:?}"
    );
    match &report.branch_kept {
        Some(BranchKept::CheckedOut(place)) => assert_eq!(
            place.canonicalize().expect("exists"),
            other.canonicalize().expect("exists")
        ),
        kept => panic!("expected checked out elsewhere, got {kept:?}"),
    }
    assert!(exists(&fixture, &thread.branch));
    thread.record_completion(&report, Timestamp::now());
    assert!(
        thread.warnings[0].contains("because it is checked out in"),
        "{:?}",
        thread.warnings
    );

    // Checked out in the main worktree, likewise.
    let (main_thread, main_head) = worked(&fixture, "In main", 0);
    git(
        &fixture.root,
        &[
            "merge",
            "--quiet",
            "--no-ff",
            "-m",
            "Merge",
            &main_thread.branch,
        ],
    );
    git(
        &fixture.root,
        &[
            "worktree",
            "remove",
            "--force",
            other.to_str().expect("utf-8"),
        ],
    );
    fs::remove_dir_all(&main_thread.worktree).expect("deleted by hand");
    git(&fixture.root, &["worktree", "prune"]);
    git(&fixture.root, &["switch", "--quiet", &main_thread.branch]);
    let report = complete(
        &fixture.root,
        &main_thread,
        IfUncommitted::Keep,
        Some(&merged(&main_head)),
    )
    .expect("complete");
    assert_eq!(
        report.branch_kept,
        Some(BranchKept::CheckedOut(fixture.root.clone()))
    );
    assert!(exists(&fixture, &main_thread.branch));
}

#[test]
fn a_kept_worktree_keeps_the_branch_until_it_is_removed() {
    let fixture = Fixture::new();
    let (mut thread, head) = worked(&fixture, "Left work", 2);
    fs::write(thread.worktree.join("draft.md"), "draft\n").expect("write");
    git(
        &fixture.root,
        &["merge", "--quiet", "--no-ff", "-m", "Merge", &thread.branch],
    );
    let merge = merged(&head);
    let report =
        complete(&fixture.root, &thread, IfUncommitted::Keep, Some(&merge)).expect("complete");
    assert_eq!(report.kept, Some(Kept::Uncommitted { count: Some(2) }));
    assert!(!report.branch_deleted);
    assert_eq!(report.branch_kept, None, "the kept worktree says it all");
    thread.record_completion(&report, Timestamp::now());
    assert!(thread.worktree.join("draft.md").exists());
    assert!(exists(&fixture, &thread.branch));
    assert_eq!(thread.deleted_branch, None);

    // "Remove worktree" later: the same rule deletes the branch then.
    let report =
        complete(&fixture.root, &thread, IfUncommitted::Discard, Some(&merge)).expect("remove");
    assert!(
        report.worktree_removed && report.branch_deleted,
        "{report:?}"
    );
    thread.record_completion(&report, Timestamp::now());
    assert!(!thread.worktree.exists());
    assert!(!exists(&fixture, &thread.branch));
    assert_eq!(
        thread
            .deleted_branch
            .as_ref()
            .map(|deleted| &deleted.commit),
        Some(&head)
    );
}

#[test]
fn closing_without_a_merge_keeps_the_branch_silently() {
    let fixture = Fixture::new();
    let (mut thread, head) = worked(&fixture, "Closed", 0);
    let report = complete(&fixture.root, &thread, IfUncommitted::Keep, None).expect("complete");
    assert!(
        report.worktree_removed && !report.branch_deleted,
        "{report:?}"
    );
    assert_eq!(report.branch_kept, None);
    thread.record_completion(&report, Timestamp::now());
    assert!(thread.warnings.is_empty());
    assert_eq!(thread.deleted_branch, None);
    assert_eq!(fixture.tip(&thread.branch), head);
}

#[test]
fn a_branch_no_thread_made_is_never_touched() {
    let fixture = Fixture::new();
    // The user's own branch, merged by squash like a thread's.
    git(&fixture.root, &["switch", "--quiet", "-c", "feature"]);
    fs::write(fixture.root.join("feature.txt"), "feature\n").expect("write");
    git(&fixture.root, &["add", "-A"]);
    git(&fixture.root, &["commit", "--quiet", "-m", "Feature"]);
    let feature = fixture.tip("feature");
    git(&fixture.root, &["switch", "--quiet", "main"]);
    git(&fixture.root, &["merge", "--quiet", "--squash", "feature"]);
    git(
        &fixture.root,
        &["commit", "--quiet", "-m", "Squash feature"],
    );

    let (mut thread, head) = worked(&fixture, "Next to it", 0);
    git(
        &fixture.root,
        &["merge", "--quiet", "--no-ff", "-m", "Merge", &thread.branch],
    );
    complete_merged(&fixture, &mut thread, &merged(&head));
    assert!(!exists(&fixture, &thread.branch));
    assert_eq!(
        fixture.tip("feature"),
        feature,
        "only the thread's branch goes"
    );
}

#[test]
fn a_thread_written_before_deleted_branch_existed_loads() {
    let fixture = Fixture::new();
    let thread = fixture.create("Old");
    let path = fixture
        .common_dir()
        .join("gitez/threads")
        .join(thread.id.as_str())
        .join("thread.json");
    let text = fs::read_to_string(&path).expect("read");
    assert!(!text.contains("deleted_branch"), "left out while None");
    let loaded = fixture.store().load(&thread.id).expect("loads");
    assert_eq!(loaded.deleted_branch, None);

    // Recorded, it is written and read back.
    fixture
        .store()
        .update(&thread.id, |thread| {
            thread.deleted_branch = Some(DeletedBranch {
                commit: "8f4a2b6c0d9e1f2a3b4c5d6e7f8091a2b3c4d5e6".into(),
                at: at("2026-11-13T12:00:00Z"),
            });
            Ok(())
        })
        .expect("update");
    let text = fs::read_to_string(&path).expect("read");
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(
        value["deleted_branch"],
        serde_json::json!({
            "commit": "8f4a2b6c0d9e1f2a3b4c5d6e7f8091a2b3c4d5e6",
            "at": "2026-11-13T12:00:00Z"
        })
    );
    assert_eq!(value["version"], 1, "an additive field keeps the version");
    assert!(
        fixture
            .store()
            .load(&thread.id)
            .expect("loads")
            .deleted_branch
            .is_some()
    );
}
