//! Reads repositories built with the git CLI in temporary directories.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use gitez_git::graph::GraphLayout;
use gitez_git::{
    Affected, ChangeKind, Commit, DiffContent, DiffLine, DiffOptions, FileChange, FileStatus, Head,
    Hunk, LineCounts, LineKind, LineSource, NewSide, Oid, Order, RefKind, Repository, Staging,
    WorktreeChange,
};
use tempfile::TempDir;

/// A scratch repository driven through the git CLI.
struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    /// Seconds added to the commit date, so every commit gets a distinct time.
    clock: std::cell::Cell<i64>,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().expect("create temp dir");
        let root = dir.path().join("repo");
        std::fs::create_dir(&root).expect("create repo dir");
        let fixture = Self {
            _dir: dir,
            root,
            clock: std::cell::Cell::new(1_700_000_000),
        };
        fixture.git(&["init", "--quiet", "--initial-branch=main"]);
        fixture
    }

    fn git(&self, args: &[&str]) -> String {
        git_in(&self.root, args, self.clock.get())
    }

    /// Commits a change to `file` and returns the new commit id.
    fn commit(&self, file: &str, message: &str) -> Oid {
        self.clock.set(self.clock.get() + 60);
        let path = self.root.join(file);
        let mut contents = std::fs::read_to_string(&path).unwrap_or_default();
        contents.push_str(message);
        contents.push('\n');
        std::fs::write(&path, contents).expect("write file");
        self.git(&["add", file]);
        self.git(&["commit", "--quiet", "-m", message]);
        self.rev("HEAD")
    }

    fn merge(&self, branch: &str, message: &str) -> Oid {
        self.clock.set(self.clock.get() + 60);
        self.git(&["merge", "--quiet", "--no-ff", "-m", message, branch]);
        self.rev("HEAD")
    }

    fn rev(&self, spec: &str) -> Oid {
        let hex = self.git(&["rev-parse", spec]);
        Oid::from_hex(hex.trim().as_bytes()).expect("rev-parse prints a hash")
    }

    fn open(&self) -> Repository {
        Repository::open(&self.root).expect("open repository")
    }
}

fn git_in(dir: &Path, args: &[&str], time: i64) -> String {
    let date = format!("@{time} +0200");
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Ada Lovelace")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada Lovelace")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git prints UTF-8")
}

fn walk_all(repo: &Repository, order: Order) -> Vec<Commit> {
    let refs = repo.refs().expect("read refs");
    repo.walk(refs.tips(true), order)
        .expect("start walk")
        .collect::<Result<_, _>>()
        .expect("walk history")
}

/// Asserts that every commit comes before its parents.
fn assert_children_first(commits: &[Commit]) {
    let position: HashMap<Oid, usize> = commits
        .iter()
        .enumerate()
        .map(|(index, commit)| (commit.id, index))
        .collect();
    for (index, commit) in commits.iter().enumerate() {
        for parent in &commit.parents {
            assert!(
                position[parent] > index,
                "{} listed before its child",
                parent
            );
        }
    }
}

/// Lays the commits out and returns how many lanes stayed open.
fn open_lanes_after_layout(commits: &[Commit]) -> usize {
    let mut layout = GraphLayout::new();
    for commit in commits {
        layout.push(commit.id, &commit.parents);
    }
    layout.open_lanes()
}

/// main:    A - B ------- M - E
///               \       /
/// feature:       C --- D
fn branchy() -> (Fixture, HashMap<&'static str, Oid>) {
    let fx = Fixture::new();
    let a = fx.commit("main.txt", "A");
    let b = fx.commit("main.txt", "B");
    fx.git(&["switch", "--quiet", "-c", "feature"]);
    let c = fx.commit("feature.txt", "C");
    let d = fx.commit("feature.txt", "D");
    fx.git(&["switch", "--quiet", "main"]);
    let m = fx.merge("feature", "Merge feature");
    let e = fx.commit("main.txt", "E");
    let ids = HashMap::from([("A", a), ("B", b), ("C", c), ("D", d), ("M", m), ("E", e)]);
    (fx, ids)
}

#[test]
fn opens_from_a_subdirectory() {
    let fx = Fixture::new();
    fx.commit("a.txt", "first");
    let sub = fx.root.join("nested/dir");
    std::fs::create_dir_all(&sub).expect("create subdir");
    let repo = Repository::open(&sub).expect("discover repository");
    assert_eq!(
        repo.work_dir()
            .map(|dir| dir.canonicalize().expect("canonical")),
        Some(fx.root.canonicalize().expect("canonical"))
    );
    assert_eq!(repo.name(), "repo");
}

#[test]
fn open_fails_outside_a_repository() {
    let dir = TempDir::new().expect("create temp dir");
    let err = Repository::open(dir.path()).expect_err("not a repository");
    assert!(err.to_string().contains("could not open a git repository"));
}

#[test]
fn walks_history_with_merges() {
    let (fx, ids) = branchy();
    let repo = fx.open();

    for order in [Order::Date, Order::Topo] {
        let commits = walk_all(&repo, order);
        assert_eq!(commits.len(), 6);
        assert_children_first(&commits);
        assert_eq!(open_lanes_after_layout(&commits), 0);
    }

    let commits = walk_all(&repo, Order::Topo);
    let summaries: Vec<&str> = commits.iter().map(|c| c.summary.as_str()).collect();
    assert_eq!(summaries, ["E", "Merge feature", "D", "C", "B", "A"]);

    let merge = &commits[1];
    assert_eq!(merge.id, ids["M"]);
    assert_eq!(merge.parents, [ids["B"], ids["D"]]);
    assert_eq!(merge.author.name, "Ada Lovelace");
    assert_eq!(merge.author.email, "ada@example.com");
    assert_eq!(merge.author.time.offset, 2 * 3600);
    assert_eq!(merge.committed, merge.author.time);
}

#[test]
fn walks_in_batches() {
    let (fx, _) = branchy();
    let repo = fx.open();
    let refs = repo.refs().expect("read refs");
    let mut walk = repo.walk(refs.tips(true), Order::Date).expect("start walk");
    let sizes: Vec<usize> = std::iter::from_fn(|| {
        let batch = walk.next_batch(4).expect("read batch");
        (!batch.is_empty()).then_some(batch.len())
    })
    .collect();
    assert_eq!(sizes, [4, 2]);
}

#[test]
fn walk_excluding_hides_reachable_commits() {
    let (fx, ids) = branchy();
    let repo = fx.open();
    let only_feature: HashSet<Oid> = repo
        .walk_excluding([ids["D"]], [ids["B"]], Order::Date)
        .expect("start walk")
        .map(|commit| commit.map(|c| c.id))
        .collect::<Result<_, _>>()
        .expect("walk");
    assert_eq!(only_feature, HashSet::from([ids["C"], ids["D"]]));
}

#[test]
fn reads_branches_tags_and_head() {
    let (fx, ids) = branchy();
    fx.git(&["tag", "v0.1.0", &ids["A"].to_string()]);
    fx.git(&[
        "tag",
        "-a",
        "-m",
        "Release",
        "v0.2.0",
        &ids["M"].to_string(),
    ]);
    let repo = fx.open();
    let refs = repo.refs().expect("read refs");

    assert_eq!(
        refs.head,
        Some(Head::Branch {
            name: "main".into(),
            target: ids["E"]
        })
    );
    let branches: Vec<(&str, Oid, bool)> = refs
        .branches
        .iter()
        .map(|b| (b.name.as_str(), b.target, b.is_head))
        .collect();
    assert_eq!(
        branches,
        [("feature", ids["D"], false), ("main", ids["E"], true)]
    );

    let tags: Vec<(&str, Oid, bool)> = refs
        .tags
        .iter()
        .map(|t| (t.name.as_str(), t.target, t.annotated))
        .collect();
    assert_eq!(
        tags,
        [("v0.1.0", ids["A"], false), ("v0.2.0", ids["M"], true)]
    );

    // The annotated tag peels to its commit, so the walk can start there.
    let labels = refs.labels_by_commit();
    let at_merge: Vec<(RefKind, &str)> = labels[&ids["M"]]
        .iter()
        .map(|l| (l.kind, l.name.as_str()))
        .collect();
    assert_eq!(at_merge, [(RefKind::Tag, "v0.2.0")]);
    assert!(labels[&ids["E"]][0].is_head);
}

#[test]
fn reads_remotes_upstreams_and_ahead_behind() {
    let (upstream, _) = branchy();
    let parent = upstream.root.parent().expect("temp dir").to_owned();
    git_in(
        &parent,
        &["clone", "--quiet", "repo", "clone"],
        1_700_000_000,
    );
    let fx = Fixture {
        _dir: TempDir::new().expect("unused temp dir"),
        root: parent.join("clone"),
        clock: std::cell::Cell::new(1_800_000_000),
    };
    fx.commit("main.txt", "local 1");
    fx.commit("main.txt", "local 2");
    fx.git(&["switch", "--quiet", "--detach", "HEAD~1"]);

    let repo = fx.open();
    let refs = repo.refs().expect("read refs");
    assert_eq!(
        refs.head,
        Some(Head::Detached {
            target: fx.rev("HEAD")
        })
    );

    let remotes: Vec<String> = refs
        .remote_branches
        .iter()
        .map(|b| b.short_name())
        .collect();
    assert_eq!(remotes, ["origin/feature", "origin/main"]);
    assert_eq!(repo.remote_names(), ["origin"]);
    assert_eq!(upstream.open().remote_names(), Vec::<String>::new());
    // A clone has not fetched; fetching writes `FETCH_HEAD`.
    assert_eq!(repo.last_fetch(), None);
    fx.git(&["fetch", "--quiet"]);
    assert!(fx.open().last_fetch().is_some());

    let main = refs
        .branches
        .iter()
        .find(|b| b.name == "main")
        .expect("main");
    assert_eq!(main.upstream.as_deref(), Some("origin/main"));
    assert!(!main.is_head);
    let origin_main = fx.rev("origin/main");
    assert_eq!(
        repo.ahead_behind(main.target, origin_main).expect("count"),
        (2, 0)
    );

    let labels = refs.labels_by_commit();
    let head = &labels[&fx.rev("HEAD")][0];
    assert_eq!(
        (head.kind, head.name.as_str(), head.is_head),
        (RefKind::Head, "HEAD", true)
    );
    assert_eq!(walk_all(&repo, Order::Date).len(), 8);
    assert_eq!(
        repo.walk(refs.tips(false), Order::Date)
            .expect("walk")
            .count(),
        8
    );
}

#[test]
fn reads_stashes_newest_first() {
    let fx = Fixture::new();
    fx.commit("a.txt", "first");
    std::fs::write(fx.root.join("a.txt"), "changed\n").expect("write");
    fx.git(&["stash", "push", "--quiet", "-m", "Older change"]);
    std::fs::write(fx.root.join("a.txt"), "changed again\n").expect("write");
    fx.git(&["stash", "push", "--quiet"]);

    let stashes = fx.open().refs().expect("read refs").stashes;
    let summary: Vec<(usize, Option<&str>, &str)> = stashes
        .iter()
        .map(|s| (s.index, s.branch.as_deref(), s.message.as_str()))
        .collect();
    let first = fx.rev("HEAD").to_hex_with_len(7).to_string();
    assert_eq!(
        summary,
        [
            (0, Some("main"), format!("{first} first").as_str()),
            (1, Some("main"), "Older change"),
        ]
    );
    assert_eq!(stashes[0].id, fx.rev("stash@{0}"));
    assert_eq!(stashes[0].recorded, format!("WIP on main: {first} first"));
    assert_eq!(stashes[1].recorded, "On main: Older change");
}

#[test]
fn reads_an_empty_repository() {
    let fx = Fixture::new();
    let repo = fx.open();
    let refs = repo.refs().expect("read refs");
    assert_eq!(
        refs.head,
        Some(Head::Unborn {
            name: "main".into()
        })
    );
    assert!(refs.branches.is_empty() && refs.stashes.is_empty());
    assert!(walk_all(&repo, Order::Date).is_empty());
}

#[test]
fn shallow_clone_ends_at_its_boundary() {
    let (upstream, _) = branchy();
    let parent = upstream.root.parent().expect("temp dir").to_owned();
    let url = format!("file://{}", upstream.root.display());
    git_in(
        &parent,
        &["clone", "--quiet", "--depth", "2", &url, "shallow"],
        1_700_000_000,
    );
    let repo = Repository::open(parent.join("shallow")).expect("open shallow clone");
    let commits = walk_all(&repo, Order::Date);
    assert!(commits.len() < 6, "walked past the shallow boundary");
    assert_children_first(&commits);
    assert_eq!(open_lanes_after_layout(&commits), 0);
}

#[test]
fn types_can_move_to_background_threads() {
    fn assert_send<T: Send>() {}
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Repository>();
    assert_send::<gitez_git::CommitWalk>();
    assert_send_sync::<gitez_git::CommitDetails>();
    assert_send_sync::<gitez_git::WorkingTreeStatus>();
    assert_send_sync::<gitez_git::Watcher>();
    assert_send_sync::<gitez_git::Refs>();
    assert_send_sync::<GraphLayout>();
}

#[test]
fn details_list_changed_files_against_the_first_parent() {
    let fixture = Fixture::new();
    let write = |path: &str, contents: &[u8]| {
        std::fs::write(fixture.root.join(path), contents).expect("write file");
    };
    write("notes.txt", b"one\ntwo\nthree\n");
    write("image.bin", b"\x00\x01\x02binary");
    write(
        "old-name.txt",
        b"a file that keeps its content\nacross a rename\n",
    );
    fixture.git(&["add", "."]);
    fixture.git(&["commit", "--quiet", "-m", "Add files"]);
    let root = fixture.rev("HEAD");

    write("notes.txt", b"one\n2\nthree\nfour\n");
    fixture.git(&["rm", "--quiet", "image.bin"]);
    fixture.git(&["mv", "old-name.txt", "new-name.txt"]);
    fixture.git(&["add", "."]);
    fixture.git(&[
        "commit",
        "--quiet",
        "-m",
        "Edit, delete and rename\n\nThe body explains why.\nOver two lines.\n",
    ]);
    let head = fixture.rev("HEAD");

    let repo = fixture.open();
    let details = repo.commit_details(head).expect("read details");
    assert_eq!(details.body, "The body explains why.\nOver two lines.");
    assert_eq!(details.committer.name, "Ada Lovelace");
    assert!(!details.signed);
    assert_eq!(
        details.files,
        vec![
            FileChange {
                path: "image.bin".to_owned(),
                kind: ChangeKind::Deleted,
                lines: None,
            },
            FileChange {
                path: "new-name.txt".to_owned(),
                kind: ChangeKind::Renamed {
                    from: "old-name.txt".to_owned()
                },
                lines: Some(LineCounts::default()),
            },
            FileChange {
                path: "notes.txt".to_owned(),
                kind: ChangeKind::Modified,
                lines: Some(LineCounts {
                    added: 2,
                    removed: 1
                }),
            },
        ]
    );
    assert_eq!(details.line_totals(), (2, 1));

    let details = repo.commit_details(root).expect("read root details");
    assert_eq!(details.body, "");
    let added: Vec<_> = details
        .files
        .iter()
        .map(|file| (file.path.as_str(), &file.kind, file.lines))
        .collect();
    assert_eq!(
        added,
        [
            ("image.bin", &ChangeKind::Added, None),
            (
                "notes.txt",
                &ChangeKind::Added,
                Some(LineCounts {
                    added: 3,
                    removed: 0
                })
            ),
            (
                "old-name.txt",
                &ChangeKind::Added,
                Some(LineCounts {
                    added: 2,
                    removed: 0
                })
            ),
        ]
    );
}

#[test]
fn details_of_a_shallow_boundary_compare_with_nothing() {
    let (upstream, _) = branchy();
    let parent = upstream.root.parent().expect("temp dir").to_owned();
    let url = format!("file://{}", upstream.root.display());
    git_in(
        &parent,
        &["clone", "--quiet", "--depth", "1", &url, "shallow"],
        1_700_000_000,
    );
    let repo = Repository::open(parent.join("shallow")).expect("open shallow clone");
    let [boundary] = &walk_all(&repo, Order::Date)[..] else {
        panic!("a depth-1 clone has one commit");
    };
    let details = repo.commit_details(boundary.id).expect("read details");
    assert!(
        details
            .files
            .iter()
            .all(|file| file.kind == ChangeKind::Added)
    );
    assert!(!details.files.is_empty());
}

fn file_status(
    path: &str,
    staged: Option<ChangeKind>,
    unstaged: Option<WorktreeChange>,
) -> FileStatus {
    FileStatus {
        path: path.to_owned(),
        staged,
        unstaged,
    }
}

#[test]
fn status_of_a_clean_working_tree_is_empty() {
    let fixture = Fixture::new();
    fixture.commit("a.txt", "one");
    let status = fixture.open().status().expect("read status");
    assert!(status.is_clean());
}

#[test]
fn status_separates_staged_unstaged_and_untracked() {
    let fixture = Fixture::new();
    fixture.commit("edited.txt", "one");
    fixture.commit("both.txt", "one");
    fixture.commit("removed.txt", "one");
    fixture.commit("old-name.txt", "a line long enough for rename detection\n");
    std::fs::write(fixture.root.join(".gitignore"), "*.log\n").expect("write");
    fixture.git(&["add", ".gitignore"]);
    fixture.git(&["commit", "--quiet", "-m", "Ignore logs"]);

    std::fs::write(fixture.root.join("edited.txt"), "two\n").expect("write");
    std::fs::write(fixture.root.join("both.txt"), "two\n").expect("write");
    fixture.git(&["add", "both.txt"]);
    std::fs::write(fixture.root.join("both.txt"), "three\n").expect("write");
    std::fs::remove_file(fixture.root.join("removed.txt")).expect("remove");
    fixture.git(&["mv", "old-name.txt", "new-name.txt"]);
    std::fs::write(fixture.root.join("staged-new.txt"), "new\n").expect("write");
    fixture.git(&["add", "staged-new.txt"]);
    std::fs::create_dir(fixture.root.join("dir")).expect("mkdir");
    std::fs::write(fixture.root.join("dir/untracked.txt"), "?\n").expect("write");
    std::fs::write(fixture.root.join("debug.log"), "ignored\n").expect("write");

    let status = fixture.open().status().expect("read status");
    assert_eq!(
        status.files,
        vec![
            file_status(
                "both.txt",
                Some(ChangeKind::Modified),
                Some(WorktreeChange::Modified)
            ),
            file_status("dir/untracked.txt", None, Some(WorktreeChange::Untracked)),
            file_status("edited.txt", None, Some(WorktreeChange::Modified)),
            file_status(
                "new-name.txt",
                Some(ChangeKind::Renamed {
                    from: "old-name.txt".to_owned()
                }),
                None
            ),
            file_status("removed.txt", None, Some(WorktreeChange::Deleted)),
            file_status("staged-new.txt", Some(ChangeKind::Added), None),
        ]
    );
    assert_eq!(status.staged(), 3);
    assert_eq!(status.conflicted(), 0);
}

#[test]
fn status_reports_merge_conflicts() {
    let fixture = Fixture::new();
    fixture.commit("shared.txt", "base");
    fixture.git(&["checkout", "--quiet", "-b", "other"]);
    fixture.commit("shared.txt", "theirs");
    fixture.git(&["checkout", "--quiet", "main"]);
    fixture.commit("shared.txt", "ours");
    let merge = Command::new("git")
        .args(["merge", "--quiet", "other"])
        .current_dir(&fixture.root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run git");
    assert!(!merge.status.success(), "the merge should conflict");

    let status = fixture.open().status().expect("read status");
    assert_eq!(
        status.files,
        vec![file_status(
            "shared.txt",
            None,
            Some(WorktreeChange::Conflicted)
        )]
    );
    assert_eq!(status.conflicted(), 1);
}

#[test]
fn status_of_an_empty_repository_lists_untracked_files() {
    let fixture = Fixture::new();
    std::fs::write(fixture.root.join("first.txt"), "hello\n").expect("write");
    let status = fixture.open().status().expect("read status");
    assert_eq!(
        status.files,
        vec![file_status(
            "first.txt",
            None,
            Some(WorktreeChange::Untracked)
        )]
    );
}

#[test]
fn classify_tells_refs_from_working_tree_changes() {
    let fixture = Fixture::new();
    fixture.commit("tracked.txt", "one");
    std::fs::write(fixture.root.join(".gitignore"), "target/\n*.log\n").expect("write");
    std::fs::create_dir(fixture.root.join("target")).expect("mkdir");
    let repo = fixture.open();
    let git = fixture.root.join(".git");
    let classify = |paths: &[PathBuf]| repo.classify(paths).expect("classify");
    let working_tree = Affected {
        refs: false,
        working_tree: true,
    };
    let everything = Affected {
        refs: true,
        working_tree: true,
    };

    assert_eq!(classify(&[fixture.root.join("tracked.txt")]), working_tree);
    assert_eq!(classify(&[fixture.root.join("new/file.txt")]), working_tree);
    assert_eq!(classify(&[fixture.root.join(".gitignore")]), working_tree);
    assert_eq!(classify(&[git.join("index")]), working_tree);
    assert_eq!(classify(&[git.join("refs/heads/main")]), everything);
    assert_eq!(classify(&[git.join("HEAD")]), everything);
    // Losing events reports the git directory itself.
    assert_eq!(classify(std::slice::from_ref(&git)), everything);

    for nothing in [
        fixture.root.join("target/debug/build/output"),
        fixture.root.join("target"),
        fixture.root.join("debug.log"),
        git.join("objects/ab/cdef"),
        git.join("index.lock"),
        git.join("refs/heads/main.lock"),
        git.join("logs/HEAD"),
        git.join("FETCH_HEAD"),
        git.join("gitez/reviews/1/review.json"),
        fixture._dir.path().join("elsewhere.txt"),
    ] {
        assert!(
            classify(std::slice::from_ref(&nothing)).is_empty(),
            "{nothing:?}"
        );
    }
    assert_eq!(
        classify(&[
            fixture.root.join("target/x"),
            fixture.root.join("tracked.txt")
        ]),
        working_tree
    );
}

#[test]
fn watch_reports_changes_made_by_git() {
    let fixture = Fixture::new();
    fixture.commit("a.txt", "one");
    let repo = fixture.open();
    let (sender, receiver) = std::sync::mpsc::channel();
    let _watcher = repo
        .watch(move |paths| {
            sender.send(paths).ok();
        })
        .expect("watch");
    // Some backends start delivering events shortly after the watch begins.
    std::thread::sleep(std::time::Duration::from_millis(200));
    while receiver.try_recv().is_ok() {}

    fixture.commit("a.txt", "two");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut affected = Affected::default();
    while !affected.refs {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let paths = receiver
            .recv_timeout(remaining)
            .expect("a commit is reported within 5 seconds");
        let part = repo.classify(&paths).expect("classify");
        affected.refs |= part.refs;
        affected.working_tree |= part.working_tree;
    }
}

#[test]
fn diff_numbers_lines_and_keeps_three_lines_of_context() {
    let fixture = Fixture::new();
    let lines: Vec<String> = (1..=20).map(|n| format!("line {n}")).collect();
    std::fs::write(fixture.root.join("text.txt"), lines.join("\n") + "\n").expect("write");
    std::fs::write(fixture.root.join("image.bin"), [0u8, 1, 2, 3]).expect("write");
    std::fs::write(
        fixture.root.join("moved.txt"),
        "same content in both places\n",
    )
    .expect("write");
    fixture.git(&["add", "."]);
    fixture.git(&["commit", "--quiet", "-m", "Add files"]);

    let mut changed = lines.clone();
    changed[9] = "line 10, edited".to_owned();
    changed.remove(16);
    std::fs::write(fixture.root.join("text.txt"), changed.join("\n") + "\n").expect("write");
    std::fs::write(fixture.root.join("image.bin"), [0u8, 1, 2, 4]).expect("write");
    fixture.git(&["mv", "moved.txt", "renamed.txt"]);
    std::fs::write(fixture.root.join("new.txt"), "hello\r\nworld\n").expect("write");
    fixture.git(&["add", "."]);
    fixture.git(&["commit", "--quiet", "-m", "Change files"]);

    let diffs = fixture
        .open()
        .commit_diff(fixture.rev("HEAD"), DiffOptions::default())
        .expect("diff");
    let paths: Vec<&str> = diffs.iter().map(|diff| diff.path.as_str()).collect();
    assert_eq!(paths, ["image.bin", "new.txt", "renamed.txt", "text.txt"]);
    assert_eq!(diffs[0].content, DiffContent::Binary);
    assert_eq!(diffs[2].content, DiffContent::Unchanged);
    assert_eq!(
        diffs[2].kind,
        ChangeKind::Renamed {
            from: "moved.txt".to_owned()
        }
    );

    let line = |kind, old: Option<u32>, new: Option<u32>, text: &str| DiffLine {
        kind,
        old,
        new,
        text: text.to_owned(),
        crlf: false,
        no_newline: false,
    };
    assert_eq!(
        diffs[1].content,
        DiffContent::Text(vec![Hunk {
            old_start: 0,
            old_len: 0,
            new_start: 1,
            new_len: 2,
            lines: vec![
                // The carriage return of a CRLF line ending is not shown.
                DiffLine {
                    crlf: true,
                    ..line(LineKind::Added, None, Some(1), "hello")
                },
                line(LineKind::Added, None, Some(2), "world"),
            ],
            staging: None,
            patchable: false,
            new_side: Some(NewSide {
                source: LineSource::Blob(fixture.rev("HEAD:new.txt")),
                lines: 2,
            }),
        }])
    );

    let context = |n: u32| line(LineKind::Context, Some(n), Some(n), &format!("line {n}"));
    let DiffContent::Text(hunks) = &diffs[3].content else {
        panic!("text.txt has a text diff: {:?}", diffs[3].content);
    };
    // The two changes are 7 lines apart, closer than twice the context, so
    // they share one hunk.
    assert_eq!(hunks.len(), 1);
    let hunk = &hunks[0];
    assert_eq!(
        (hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len),
        (7, 14, 7, 13)
    );
    let mut expected: Vec<DiffLine> = (7..=9).map(context).collect();
    expected.push(line(LineKind::Removed, Some(10), None, "line 10"));
    expected.push(line(LineKind::Added, None, Some(10), "line 10, edited"));
    expected.extend((11..=16).map(context));
    expected.push(line(LineKind::Removed, Some(17), None, "line 17"));
    expected.extend((18..=20).map(|n| {
        line(
            LineKind::Context,
            Some(n),
            Some(n - 1),
            &format!("line {n}"),
        )
    }));
    assert_eq!(hunk.lines, expected);
    let counts = diffs[3].line_counts().expect("text has line counts");
    assert_eq!((counts.added, counts.removed), (1, 2));

    // The whole new file, for showing the lines around the hunk.
    let new_side = hunk.new_side.as_ref().expect("text.txt has a new side");
    assert_eq!(
        new_side.source,
        LineSource::Blob(fixture.rev("HEAD:text.txt"))
    );
    assert_eq!(new_side.lines as usize, changed.len());
    assert_eq!(
        fixture.open().lines(&new_side.source).expect("lines"),
        changed
    );
}

#[test]
fn uncommitted_hunks_know_the_file_their_lines_are_from() {
    let fixture = Fixture::new();
    std::fs::write(fixture.root.join("a.txt"), "1\n2\n3\n4\n5\n6\n7\n8\n9\n").expect("write");
    std::fs::write(fixture.root.join("gone.txt"), "bye\n").expect("write");
    fixture.git(&["add", "."]);
    fixture.git(&["commit", "--quiet", "-m", "first"]);

    std::fs::write(fixture.root.join("a.txt"), "one\n2\n3\n4\n5\n6\n7\n8\n9\n").expect("write");
    fixture.git(&["add", "a.txt"]);
    std::fs::write(
        fixture.root.join("a.txt"),
        "one\n2\n3\n4\n5\n6\n7\n8\nnine\r\nten\n",
    )
    .expect("write");
    std::fs::remove_file(fixture.root.join("gone.txt")).expect("remove");

    let repository = fixture.open();
    let diffs = repository
        .working_tree_diff(DiffOptions::default())
        .expect("diff");
    let sides: Vec<Vec<Option<NewSide>>> = diffs
        .iter()
        .map(|diff| match &diff.content {
            DiffContent::Text(hunks) => hunks.iter().map(|hunk| hunk.new_side.clone()).collect(),
            other => panic!("a text diff: {other:?}"),
        })
        .collect();
    let staged = NewSide {
        source: LineSource::Blob(fixture.rev(":a.txt")),
        lines: 9,
    };
    let on_disk = NewSide {
        source: LineSource::WorkingTree("a.txt".to_owned()),
        lines: 10,
    };
    assert_eq!(
        sides,
        [
            vec![Some(staged.clone()), Some(on_disk.clone())],
            vec![None]
        ]
    );
    assert_eq!(repository.lines(&staged.source).expect("lines")[0], "one");
    assert_eq!(
        repository.lines(&on_disk.source).expect("lines")[8..],
        ["nine", "ten"]
    );
}

#[test]
fn diff_shows_a_missing_newline_at_end_of_file() {
    let fixture = Fixture::new();
    std::fs::write(fixture.root.join("a.txt"), "one\ntwo").expect("write");
    fixture.git(&["add", "."]);
    fixture.git(&["commit", "--quiet", "-m", "No newline"]);
    std::fs::write(fixture.root.join("a.txt"), "one\ntwo\n").expect("write");
    fixture.git(&["commit", "--quiet", "-am", "Add newline"]);

    let diffs = fixture
        .open()
        .commit_diff(fixture.rev("HEAD"), DiffOptions::default())
        .expect("diff");
    let DiffContent::Text(hunks) = &diffs[0].content else {
        panic!("a text diff: {:?}", diffs[0].content);
    };
    let lines: Vec<(LineKind, &str, bool)> = hunks[0]
        .lines
        .iter()
        .map(|line| (line.kind, line.text.as_str(), line.no_newline))
        .collect();
    assert_eq!(
        lines,
        [
            (LineKind::Context, "one", false),
            (LineKind::Removed, "two", true),
            (LineKind::Added, "two", false),
        ]
    );
}

#[test]
fn working_tree_diff_compares_head_with_the_files_on_disk() {
    let fixture = Fixture::new();
    fixture.commit("edited.txt", "one");
    fixture.commit("removed.txt", "gone");
    fixture.commit("old-name.txt", "a line long enough for rename detection");

    // Staged and unstaged edits to one file show as one diff against HEAD.
    std::fs::write(fixture.root.join("edited.txt"), "one\ntwo\n").expect("write");
    fixture.git(&["add", "edited.txt"]);
    std::fs::write(fixture.root.join("edited.txt"), "one\ntwo\nthree\n").expect("write");
    std::fs::remove_file(fixture.root.join("removed.txt")).expect("remove");
    fixture.git(&["mv", "old-name.txt", "new-name.txt"]);
    std::fs::write(
        fixture.root.join("new-name.txt"),
        "a line long enough for rename detection\nand one more\n",
    )
    .expect("write");
    std::fs::write(fixture.root.join("untracked.txt"), "fresh\n").expect("write");

    let diffs = fixture
        .open()
        .working_tree_diff(DiffOptions::default())
        .expect("diff");
    type Summary<'a> = (&'a str, &'a ChangeKind, Option<(usize, usize)>);
    let summary: Vec<Summary> = diffs
        .iter()
        .map(|diff| {
            (
                diff.path.as_str(),
                &diff.kind,
                diff.line_counts()
                    .map(|counts| (counts.added, counts.removed)),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("edited.txt", &ChangeKind::Modified, Some((2, 0))),
            (
                "new-name.txt",
                &ChangeKind::Renamed {
                    from: "old-name.txt".to_owned()
                },
                Some((1, 0))
            ),
            ("removed.txt", &ChangeKind::Deleted, Some((0, 1))),
            ("untracked.txt", &ChangeKind::Added, Some((1, 0))),
        ]
    );
}

#[test]
fn head_message_is_the_full_message_of_head() {
    let fixture = Fixture::new();
    let repo = fixture.open();
    assert_eq!(repo.head_message().expect("no commits"), None);

    fixture.commit("a.txt", "Summary line");
    fixture.git(&[
        "commit",
        "--quiet",
        "--amend",
        "-m",
        "Summary line\n\nBody, with # a hash.\n\n",
    ]);
    let repo = fixture.open();
    assert_eq!(
        repo.head_message().expect("message").as_deref(),
        Some("Summary line\n\nBody, with # a hash.")
    );
}

#[test]
fn ahead_behind_counts_only_the_commits_one_side_lacks() {
    let fixture = Fixture::new();
    let base = fixture.commit("a.txt", "base");
    let ahead = fixture.commit("a.txt", "ahead");
    fixture.git(&["checkout", "--quiet", "-b", "other", &base.to_string()]);
    let other = fixture.commit("b.txt", "other");
    let repo = fixture.open();

    assert_eq!(repo.ahead_behind(base, base).expect("same"), (0, 0));
    assert_eq!(repo.ahead_behind(ahead, base).expect("ahead"), (1, 0));
    assert_eq!(repo.ahead_behind(base, ahead).expect("behind"), (0, 1));
    assert_eq!(repo.ahead_behind(ahead, other).expect("diverged"), (1, 1));
}

#[test]
fn branch_diff_shows_the_changes_since_the_branch_left_its_base() {
    let fixture = Fixture::new();
    fixture.commit("shared.txt", "shared");
    fixture.git(&["checkout", "--quiet", "-b", "feature"]);
    fixture.commit("feature.txt", "feature one");
    let head = fixture.commit("feature.txt", "feature two");
    fixture.git(&["checkout", "--quiet", "main"]);
    // Changes on the base after the branch left are not the branch's.
    let base = fixture.commit("shared.txt", "main moved on");
    let fork = fixture.rev("feature~2");
    let repo = fixture.open();

    let diff = repo
        .branch_diff(base, head, DiffOptions::default())
        .expect("branch diff");
    assert_eq!(diff.merge_base, Some(fork));
    assert_eq!(diff.ahead, 2);
    let paths: Vec<&str> = diff.files.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(paths, ["feature.txt"]);
    let blob = fixture.rev("feature:feature.txt");
    assert_eq!(diff.files[0].new_id, Some(blob));
    assert_eq!(
        diff.files[0].line_counts(),
        Some(LineCounts {
            added: 2,
            removed: 0
        })
    );

    // Merged: nothing left to review.
    fixture.merge("feature", "Merge feature");
    let merged = fixture.rev("HEAD");
    let diff = repo
        .branch_diff(merged, head, DiffOptions::default())
        .expect("merged");
    assert_eq!((diff.merge_base, diff.ahead), (Some(head), 0));
    assert!(diff.files.is_empty());
}

#[test]
fn user_name_is_read_from_the_repository_config() {
    let fixture = Fixture::new();
    fixture.git(&["config", "user.name", "Ada Lovelace"]);
    fixture.git(&["config", "gitez.testCommand", "  cargo test  "]);
    fixture.git(&["config", "gitez.empty", ""]);
    let repo = fixture.open();
    assert_eq!(repo.user_name().as_deref(), Some("Ada Lovelace"));
    assert_eq!(
        repo.config_string("gitez.testCommand").as_deref(),
        Some("cargo test")
    );
    assert_eq!(repo.config_string("gitez.empty"), None);
    assert_eq!(repo.config_string("gitez.missing"), None);
}

/// `git apply --cached` of `patch`, reversed if `reverse`.
fn apply_to_index(fixture: &Fixture, patch: &str, reverse: bool) {
    let file = fixture.root.join("../hunk.patch");
    std::fs::write(&file, patch).expect("write patch");
    let file = file.to_str().expect("UTF-8 path");
    let mut args = vec!["apply", "--cached", file];
    if reverse {
        args.insert(2, "--reverse");
    }
    fixture.git(&args);
}

/// The hunks of the uncommitted changes to `path`: staging, and the text of
/// their changed lines.
fn uncommitted_hunks(fixture: &Fixture, path: &str) -> Vec<(Option<Staging>, bool, Vec<String>)> {
    let diffs = fixture
        .open()
        .working_tree_diff(DiffOptions::default())
        .expect("diff");
    let diff = diffs
        .iter()
        .find(|diff| diff.path == path)
        .expect("the file has changes");
    let DiffContent::Text(hunks) = &diff.content else {
        panic!("a text diff: {:?}", diff.content);
    };
    hunks
        .iter()
        .map(|hunk| {
            let changed = hunk
                .lines
                .iter()
                .filter(|line| line.kind != LineKind::Context)
                .map(|line| line.text.clone())
                .collect();
            (hunk.staging, hunk.patchable, changed)
        })
        .collect()
}

#[test]
fn hunks_stage_and_unstage_one_at_a_time() {
    let fixture = Fixture::new();
    let path = "odd \"name\".txt";
    let original: Vec<String> = (1..=30).map(|n| format!("line {n}\r")).collect();
    // CRLF line endings and no newline at the end of the file.
    std::fs::write(fixture.root.join(path), original.join("\n")).expect("write");
    fixture.git(&["add", "."]);
    fixture.git(&["commit", "--quiet", "-m", "Add"]);

    let mut edited = original.clone();
    edited[1] = "line 2, edited\r".to_owned();
    edited[29] = "line 30, edited\r".to_owned();
    std::fs::write(fixture.root.join(path), edited.join("\n")).expect("write");
    let removed_and_added = |n: u32, edited: &str| vec![format!("line {n}"), edited.to_owned()];
    // Without a newline after it, the last line's carriage return is text.
    let last = vec!["line 30\r".to_owned(), "line 30, edited\r".to_owned()];
    assert_eq!(
        uncommitted_hunks(&fixture, path),
        [
            (
                Some(Staging::Unstaged),
                true,
                removed_and_added(2, "line 2, edited")
            ),
            (Some(Staging::Unstaged), true, last.clone()),
        ]
    );

    // Stage the second hunk alone.
    let diffs = fixture
        .open()
        .working_tree_diff(DiffOptions::default())
        .expect("diff");
    let DiffContent::Text(hunks) = &diffs[0].content else {
        panic!("a text diff");
    };
    apply_to_index(&fixture, &hunks[1].patch(path), false);
    assert_eq!(
        uncommitted_hunks(&fixture, path),
        [
            (Some(Staging::Staged), true, last.clone()),
            (
                Some(Staging::Unstaged),
                true,
                removed_and_added(2, "line 2, edited")
            ),
        ]
    );

    // And unstage it again.
    let diffs = fixture
        .open()
        .working_tree_diff(DiffOptions::default())
        .expect("diff");
    let DiffContent::Text(hunks) = &diffs[0].content else {
        panic!("a text diff");
    };
    apply_to_index(&fixture, &hunks[0].patch(path), true);
    assert_eq!(fixture.git(&["diff", "--cached"]), "");
    assert_eq!(uncommitted_hunks(&fixture, path).len(), 2);
}

#[test]
fn new_and_deleted_files_are_not_patchable_by_hunk() {
    let fixture = Fixture::new();
    fixture.commit("gone.txt", "gone");
    std::fs::remove_file(fixture.root.join("gone.txt")).expect("remove");
    std::fs::write(fixture.root.join("new.txt"), "new\n").expect("write");
    std::fs::write(fixture.root.join("staged.txt"), "staged\n").expect("write");
    fixture.git(&["add", "staged.txt"]);

    for (path, staging) in [
        ("gone.txt", Staging::Unstaged),
        ("new.txt", Staging::Unstaged),
        ("staged.txt", Staging::Staged),
    ] {
        let hunks = uncommitted_hunks(&fixture, path);
        assert_eq!(hunks.len(), 1, "{path}");
        assert_eq!((hunks[0].0, hunks[0].1), (Some(staging), false), "{path}");
    }
}

#[test]
fn whitespace_can_be_ignored() {
    let fixture = Fixture::new();
    std::fs::write(fixture.root.join("code.rs"), "fn a() {\n    one();\n}\n").expect("write");
    std::fs::write(fixture.root.join("spaces.txt"), "a b\n").expect("write");
    fixture.git(&["add", "."]);
    fixture.git(&["commit", "--quiet", "-m", "first"]);

    std::fs::write(
        fixture.root.join("code.rs"),
        "fn a() {\n\tone();\n    two();\n}\n",
    )
    .expect("write");
    std::fs::write(fixture.root.join("spaces.txt"), "a  b\r\n").expect("write");
    let ignore = DiffOptions {
        ignore_whitespace: true,
    };
    let check = |diffs: Vec<gitez_git::FileDiff>| {
        let [code, spaces] = diffs.as_slice() else {
            panic!("two files: {diffs:?}");
        };
        let DiffContent::Text(hunks) = &code.content else {
            panic!("a text diff: {:?}", code.content);
        };
        let lines: Vec<(LineKind, &str)> = hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .map(|line| (line.kind, line.text.as_str()))
            .collect();
        // The re-indented line is context, shown as it was.
        assert_eq!(
            lines,
            [
                (LineKind::Context, "fn a() {"),
                (LineKind::Context, "    one();"),
                (LineKind::Added, "    two();"),
                (LineKind::Context, "}"),
            ]
        );
        assert!(hunks.iter().all(|hunk| !hunk.patchable));
        assert_eq!(spaces.content, DiffContent::OnlyWhitespace);
    };

    let repository = fixture.open();
    check(repository.working_tree_diff(ignore).expect("diff"));
    let all = repository
        .working_tree_diff(DiffOptions::default())
        .expect("diff");
    let counts = all[0]
        .line_counts()
        .map(|counts| (counts.added, counts.removed));
    assert_eq!(counts, Some((2, 1)));

    fixture.git(&["commit", "--quiet", "--all", "-m", "second"]);
    check(
        fixture
            .open()
            .commit_diff(fixture.rev("HEAD"), ignore)
            .expect("diff"),
    );
}

#[test]
fn worktrees_share_the_common_git_directory() {
    let fixture = Fixture::new();
    fixture.commit("a.txt", "first");
    let linked = fixture._dir.path().join("linked");
    fixture.git(&[
        "worktree",
        "add",
        "--quiet",
        "-b",
        "side",
        linked.to_str().expect("a UTF-8 path"),
    ]);
    let canonical = |path: std::path::PathBuf| path.canonicalize().expect("canonical");

    let main = fixture.open();
    let worktree = Repository::open(&linked).expect("open the worktree");
    assert_eq!(
        canonical(main.common_dir()),
        canonical(fixture.root.join(".git"))
    );
    assert_eq!(
        canonical(worktree.common_dir()),
        canonical(main.common_dir())
    );
    // Its own git directory is not the common one.
    assert_ne!(
        canonical(worktree.git_dir().to_owned()),
        canonical(main.common_dir())
    );
}

#[test]
fn lines_are_followed_from_one_commit_to_another() {
    use gitez_git::{ChangedLines, Followed};

    let fixture = Fixture::new();
    let numbered = |prefix: &str, count: u32| -> Vec<String> {
        (1..=count).map(|n| format!("{prefix} line {n}")).collect()
    };
    let write = |path: &str, lines: &[String]| {
        std::fs::write(fixture.root.join(path), lines.join("\n") + "\n").expect("write");
    };
    let anchor = numbered("anchor", 30);
    let store = numbered("store", 20);
    write("anchor.txt", &anchor);
    write("store.txt", &store);
    write("gone.txt", &numbered("gone", 3));
    write("same.txt", &numbered("same", 3));
    std::fs::write(fixture.root.join("image.bin"), [0u8, 1, 2, 3]).expect("write");
    fixture.git(&["add", "."]);
    fixture.git(&["commit", "--quiet", "-m", "first"]);
    let from = fixture.rev("HEAD");

    // Two lines inserted before line 3; lines 5–7 rewritten as four.
    let mut edited = anchor.clone();
    edited.splice(4..7, numbered("rewritten", 4));
    edited.splice(2..2, numbered("inserted", 2));
    write("anchor.txt", &edited);
    // Renamed, with its first line changed.
    let mut moved = store.clone();
    moved[0] = "store line one".to_owned();
    std::fs::remove_file(fixture.root.join("store.txt")).expect("remove");
    write("storage.txt", &moved);
    std::fs::remove_file(fixture.root.join("gone.txt")).expect("remove");
    std::fs::write(fixture.root.join("image.bin"), [0u8, 9, 9]).expect("write");
    fixture.git(&["add", "--all"]);
    fixture.git(&["commit", "--quiet", "-m", "second"]);
    let to = fixture.rev("HEAD");

    let repository = fixture.open();
    let follow = |path: &str| repository.follow_lines(path, from, to).expect("follow");
    assert_eq!(
        follow("anchor.txt"),
        Followed::Changed {
            path: "anchor.txt".to_owned(),
            lines: vec![
                ChangedLines {
                    old: 2..2,
                    new: 2..4
                },
                ChangedLines {
                    old: 4..7,
                    new: 6..10
                },
            ],
        }
    );
    assert_eq!(
        follow("store.txt"),
        Followed::Changed {
            path: "storage.txt".to_owned(),
            lines: vec![ChangedLines {
                old: 0..1,
                new: 0..1
            }],
        }
    );
    assert_eq!(follow("gone.txt"), Followed::Gone);
    assert_eq!(follow("image.bin"), Followed::Gone);
    assert_eq!(follow("same.txt"), Followed::Unchanged);
}

#[test]
fn merge_preview_finds_the_files_that_would_conflict() {
    let fixture = Fixture::new();
    fixture.commit("shared.txt", "shared");
    fixture.git(&["checkout", "--quiet", "-b", "feature"]);
    fixture.commit("feature.txt", "feature");
    let fast = fixture.rev("HEAD");
    let base = fixture.rev("main");
    let repo = fixture.open();

    // The base has not moved: a fast-forward.
    let preview = repo.merge_preview(base, fast).expect("fast-forward");
    assert_eq!((preview.ahead, preview.behind, preview.files), (1, 0, 1));
    assert!(preview.conflicts.is_empty());

    // Both change the same line of shared.txt; only feature changes
    // feature.txt.
    std::fs::write(fixture.root.join("shared.txt"), "feature's line\n").expect("write");
    fixture.git(&["commit", "--quiet", "-am", "feature edits shared"]);
    let head = fixture.rev("HEAD");
    fixture.git(&["checkout", "--quiet", "main"]);
    std::fs::write(fixture.root.join("shared.txt"), "main's line\n").expect("write");
    fixture.git(&["commit", "--quiet", "-am", "main edits shared"]);
    let base = fixture.rev("HEAD");
    let preview = repo.merge_preview(base, head).expect("diverged");
    assert_eq!((preview.ahead, preview.behind, preview.files), (2, 1, 2));
    assert_eq!(preview.conflicts, ["shared.txt"]);
    // Nothing was written: the merge leaves no trace.
    assert_eq!(fixture.git(&["status", "--porcelain"]), "");
}

#[test]
fn a_merge_stopped_by_conflicts_is_read() {
    let fixture = Fixture::new();
    fixture.commit("shared.txt", "shared");
    fixture.git(&["checkout", "--quiet", "-b", "feature"]);
    std::fs::write(fixture.root.join("shared.txt"), "feature\n").expect("write");
    fixture.git(&["commit", "--quiet", "-am", "feature edits shared"]);
    let theirs = fixture.rev("HEAD");
    fixture.git(&["checkout", "--quiet", "main"]);
    std::fs::write(fixture.root.join("shared.txt"), "main\n").expect("write");
    fixture.git(&["commit", "--quiet", "-am", "main edits shared"]);
    let ours = fixture.rev("HEAD");
    let repo = fixture.open();
    assert_eq!(repo.merge_in_progress().expect("read"), None);

    let merge = Command::new("git")
        .arg("-C")
        .arg(&fixture.root)
        .args([
            "-c",
            "merge.conflictStyle=diff3",
            "merge",
            "--quiet",
            "feature",
        ])
        .output()
        .expect("git runs");
    assert!(!merge.status.success(), "the merge conflicts");

    let merge = repo
        .merge_in_progress()
        .expect("read")
        .expect("in progress");
    assert_eq!((merge.ours, merge.theirs), (ours, theirs));
    assert_eq!(merge.branch.as_deref(), Some("main"));
    assert_eq!(merge.theirs_branch.as_deref(), Some("feature"));
    assert!(
        merge.message.starts_with("Merge branch 'feature'"),
        "{}",
        merge.message
    );

    let file = repo
        .conflict_file("shared.txt")
        .expect("read")
        .expect("conflicts");
    let conflict = file.conflicts().next().expect("one conflict");
    assert_eq!(conflict.ours, ["main"]);
    assert_eq!(conflict.theirs, ["feature"]);
    assert_eq!(conflict.base.as_deref(), Some(&["shared".to_owned()][..]));
    assert_eq!(repo.conflict_file("missing.txt").expect("read"), None);
}

#[test]
fn cherry_pick_trial_applies_skips_and_finds_conflicts() {
    use gitez_git::PickTrial;

    let fixture = Fixture::new();
    fixture.commit("shared.txt", "shared");
    fixture.git(&["checkout", "--quiet", "-b", "feature"]);
    fixture.commit("new.txt", "new");
    let adds_new = fixture.rev("HEAD");
    std::fs::write(fixture.root.join("shared.txt"), "feature's line\n").expect("write");
    fixture.git(&["commit", "--quiet", "-am", "feature edits shared"]);
    let edits_shared = fixture.rev("HEAD");
    fixture.git(&["checkout", "--quiet", "main"]);
    let main = fixture.rev("main");
    let repo = fixture.open();

    assert_eq!(
        repo.cherry_pick_trial(main, &[adds_new, edits_shared])
            .expect("trial"),
        PickTrial::Clean {
            applied: vec![adds_new, edits_shared],
            skipped: 0
        }
    );

    // Main already has new.txt and changes shared.txt its own way.
    fixture.commit("new.txt", "new");
    std::fs::write(fixture.root.join("shared.txt"), "main's line\n").expect("write");
    fixture.git(&["commit", "--quiet", "-am", "main edits shared"]);
    let main = fixture.rev("main");
    assert_eq!(
        repo.cherry_pick_trial(main, &[adds_new]).expect("trial"),
        PickTrial::Clean {
            applied: vec![],
            skipped: 1
        }
    );
    assert_eq!(
        repo.cherry_pick_trial(main, &[adds_new, edits_shared])
            .expect("trial"),
        PickTrial::Conflict {
            commit: edits_shared,
            files: vec!["shared.txt".to_owned()]
        }
    );
    // Feature contains both, though they no longer apply to its tip.
    let feature = fixture.rev("feature");
    assert_eq!(
        repo.cherry_pick_trial(feature, &[adds_new, edits_shared])
            .expect("trial"),
        PickTrial::Clean {
            applied: vec![],
            skipped: 2
        }
    );
    // Nothing was written.
    assert_eq!(fixture.git(&["status", "--porcelain"]), "");
}

#[test]
fn finds_the_commits_to_rewrite_and_a_stopped_rebase() {
    use gitez_git::{RebaseInProgress, Rewritable, RewriteBase};

    let fixture = Fixture::new();
    let a = fixture.commit("a.txt", "a");
    let b = fixture.commit("b.txt", "b");
    fixture.git(&["commit", "--quiet", "--allow-empty", "-m", "c\n\nThe body."]);
    let c = fixture.rev("HEAD");
    let repo = fixture.open();

    let Rewritable::Commits { commits, base } = repo.rewritable(c, b, Some(b)).expect("read")
    else {
        panic!("b is on main");
    };
    let ids: Vec<Oid> = commits.iter().map(|commit| commit.id).collect();
    assert_eq!(ids, [c, b]);
    assert_eq!(commits[0].message, "c\n\nThe body.");
    assert_eq!(commits[0].summary, "c");
    // The upstream is on b.
    assert_eq!(
        commits
            .iter()
            .map(|commit| commit.pushed)
            .collect::<Vec<_>>(),
        [false, true]
    );
    assert_eq!(
        base,
        Some(RewriteBase {
            id: a,
            summary: "a".to_owned()
        })
    );
    assert_eq!(repo.resolve_commit("main~2").expect("resolve"), Some(a));
    assert_eq!(repo.resolve_commit("no-such-ref").expect("resolve"), None);
    assert_eq!(repo.first_after(c, a).expect("walk"), Some(b));
    assert_eq!(repo.first_after(c, c).expect("walk"), None);
    let Rewritable::Commits { base, .. } = repo.rewritable(c, a, None).expect("read") else {
        panic!("a is on main");
    };
    assert_eq!(base, None);

    // A commit on another branch, and a merge in the way.
    fixture.git(&["checkout", "--quiet", "-b", "side", "HEAD~1"]);
    let side = fixture.commit("side.txt", "side");
    fixture.git(&["checkout", "--quiet", "main"]);
    let merge = fixture.merge("side", "Merge side");
    let repo = fixture.open();
    assert_eq!(
        repo.rewritable(c, side, None).expect("read"),
        Rewritable::Elsewhere
    );
    assert_eq!(
        repo.rewritable(merge, b, None).expect("read"),
        Rewritable::Merge {
            id: merge,
            summary: "Merge side".to_owned()
        }
    );

    // Stopped at an edit of b.
    assert_eq!(repo.rebase_in_progress().expect("read"), None);
    let output = Command::new("git")
        .args(["rebase", "--quiet", "--interactive", a.to_string().as_str()])
        .current_dir(&fixture.root)
        .env("GIT_SEQUENCE_EDITOR", "sed -i.bak '1s/^pick/edit/'")
        .output()
        .expect("git runs");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        repo.rebase_in_progress().expect("read"),
        Some(RebaseInProgress {
            branch: Some("main".to_owned()),
            stopped_at: Some(b),
            summary: Some("b".to_owned()),
        })
    );
}

#[test]
fn branch_names_follow_git_rules() {
    use gitez_git::branch_name_problem;
    for good in ["main", "feature/graph-cache", "fix-1.2", "ünïcode"] {
        assert_eq!(branch_name_problem(good), None, "{good}");
    }
    for bad in [
        "",
        "two words",
        "-x",
        "HEAD",
        "a..b",
        "a~1",
        "a^",
        "a:b",
        "a?",
        "a*",
        "a[",
        "a\\b",
        "a@{1}",
        "/a",
        "a/",
        "a.",
        ".a",
        "a.lock",
        "a//b",
    ] {
        assert!(branch_name_problem(bad).is_some(), "{bad:?}");
    }
}

#[test]
fn ref_state_reads_head_branches_tags_and_stashes() {
    use gitez_git::HeadPlace;
    let fx = Fixture::new();
    let first = fx.commit("a.txt", "first");
    fx.git(&["branch", "feature"]);
    fx.git(&["tag", "light"]);
    fx.git(&["tag", "-a", "-m", "Release", "v1"]);
    let annotated = fx.git(&["rev-parse", "refs/tags/v1"]);
    std::fs::write(fx.root.join("a.txt"), "changed\n").expect("write");
    fx.git(&["stash", "push", "--quiet", "-m", "tracked only"]);
    std::fs::write(fx.root.join("new.txt"), "new\n").expect("write");
    fx.git(&["stash", "push", "--quiet", "--include-untracked"]);

    let state = fx.open().ref_state().expect("read state");
    assert_eq!(state.head, Some(HeadPlace::Branch("main".into())));
    assert_eq!(
        state.branches.into_iter().collect::<Vec<_>>(),
        [("feature".to_owned(), first), ("main".to_owned(), first)]
    );
    let tags: Vec<(String, String)> = state
        .tags
        .iter()
        .map(|(name, id)| (name.clone(), id.to_string()))
        .collect();
    assert_eq!(
        tags,
        [
            ("light".to_owned(), first.to_string()),
            ("v1".to_owned(), annotated.trim().to_owned())
        ]
    );
    let stashes: Vec<(bool, &str)> = state
        .stashes
        .iter()
        .map(|stash| (stash.untracked, stash.recorded.as_str()))
        .collect();
    let short = first.to_hex_with_len(7).to_string();
    assert_eq!(
        stashes,
        [
            (true, format!("WIP on main: {short} first").as_str()),
            (false, "On main: tracked only")
        ]
    );

    fx.git(&["switch", "--quiet", "--detach", "HEAD"]);
    assert_eq!(
        fx.open().ref_state().expect("read state").head,
        Some(HeadPlace::Detached(first))
    );
}
