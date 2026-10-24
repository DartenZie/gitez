//! Reads repositories built with the git CLI in temporary directories.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use gitez_git::graph::GraphLayout;
use gitez_git::{
    ChangeKind, Commit, FileChange, Head, LineCounts, Oid, Order, RefKind, Repository,
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
