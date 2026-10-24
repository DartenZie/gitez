//! Undo and redo of what GitEZ did to a repository.
//!
//! Git has no undo of its own, so GitEZ keeps one: around each change it
//! makes, it reads where `HEAD`, the local branches, the tags and the
//! stash were before and after, and keeps the difference. Undo puts those references
//! back, redo moves them forward again. Either runs only while the
//! references are still where the change left them; one that moved since
//! (a later commit, a command in a terminal) is never overwritten.
//!
//! The log lives in memory, per repository, for as long as GitEZ runs.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use gitez_command::{Git, RefUpdate, ResetMode, UnstashOutcome};
use gitez_git::{HeadPlace, Oid, RefState, Repository, StashEntry};
use gitez_review::{PullRequest, State, Store, Tip};
use gpui_kit::*;

/// Undo steps kept per repository.
const LIMIT: usize = 50;

/// Which way to move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Undo,
    Redo,
}

/// How one branch or tag moved.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RefChange {
    name: String,
    /// `None` when it did not exist.
    before: Option<Oid>,
    after: Option<Oid>,
}

/// How the stash changed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StashChange {
    /// This entry was added on top.
    Pushed(StashEntry),
    /// This entry was removed.
    Dropped(StashEntry),
}

/// The fields of a local pull request that merging it changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewTips {
    pub state: State,
    pub base: Tip,
    pub head: Tip,
}

impl ReviewTips {
    pub fn of(pull_request: &PullRequest) -> Self {
        Self {
            state: pull_request.state,
            base: pull_request.base.clone(),
            head: pull_request.head.clone(),
        }
    }
}

/// A local pull request that a change merged.
#[derive(Debug, Clone)]
pub struct ReviewChange {
    pub store: Store,
    pub number: u32,
    pub before: ReviewTips,
    pub after: ReviewTips,
}

/// Uncommitted changes thrown away, which undo brings back.
#[derive(Debug, Clone)]
pub enum Discard {
    /// The changes to whole files, `None` for every file, kept in the
    /// stash commit `saved`.
    Files {
        paths: Option<Vec<String>>,
        saved: String,
    },
    /// One hunk of a file's unstaged changes, as a patch.
    Hunk { patch: String },
}

/// What one change did.
#[derive(Debug, Clone)]
pub struct Change {
    /// Where `HEAD` was before and after, when it moved.
    head: Option<(Option<HeadPlace>, Option<HeadPlace>)>,
    branches: Vec<RefChange>,
    tags: Vec<RefChange>,
    stash: Option<StashChange>,
    /// How the branch checked out moves back and forth.
    mode: ResetMode,
    review: Option<ReviewChange>,
    discard: Option<Discard>,
}

impl Change {
    /// What changed from `before` to `after`; `None` when nothing did.
    /// `mode` says how the branch checked out moves: [`ResetMode::Soft`]
    /// for a commit, whose changes come back staged when it is undone.
    pub fn between(before: &RefState, after: &RefState, mode: ResetMode) -> Option<Self> {
        let head = (before.head != after.head).then(|| (before.head.clone(), after.head.clone()));
        let branches = ref_changes(&before.branches, &after.branches);
        let tags = ref_changes(&before.tags, &after.tags);
        let stash = stash_change(&before.stashes, &after.stashes);
        if head.is_none() && branches.is_empty() && tags.is_empty() && stash.is_none() {
            return None;
        }
        Some(Self {
            head,
            branches,
            tags,
            stash,
            mode,
            review: None,
            discard: None,
        })
    }

    /// Also puts back the local pull request `review`.
    pub fn with_review(mut self, review: ReviewChange) -> Self {
        self.review = Some(review);
        self
    }

    /// Why moving `direction` cannot run with the repository as it is
    /// `now`, if it cannot: something it would put back has changed since.
    pub fn why_not(&self, now: &RefState, direction: Direction) -> Option<String> {
        if let Some(head) = &self.head {
            let expected = match direction {
                Direction::Undo => &head.1,
                Direction::Redo => &head.0,
            };
            if &now.head != expected {
                return Some("Something else has been checked out since.".to_owned());
            }
        }
        for branch in &self.branches {
            let expected = match direction {
                Direction::Undo => branch.after,
                Direction::Redo => branch.before,
            };
            if now.branches.get(&branch.name).copied() != expected {
                return Some(match expected {
                    Some(_) if now.branches.contains_key(&branch.name) => {
                        format!("{} has moved since.", branch.name)
                    }
                    Some(_) => format!("{} has been deleted since.", branch.name),
                    None => format!("A branch named {} exists again.", branch.name),
                });
            }
        }
        for tag in &self.tags {
            let expected = match direction {
                Direction::Undo => tag.after,
                Direction::Redo => tag.before,
            };
            if now.tags.get(&tag.name).copied() != expected {
                return Some(match expected {
                    Some(_) if now.tags.contains_key(&tag.name) => {
                        format!("Tag {} has moved since.", tag.name)
                    }
                    Some(_) => format!("Tag {} has been deleted since.", tag.name),
                    None => format!("A tag named {} exists again.", tag.name),
                });
            }
        }
        let stash_moved = match (&self.stash, direction) {
            (Some(StashChange::Pushed(entry)), Direction::Undo) => {
                now.stashes.first().map(|top| top.id) != Some(entry.id)
            }
            (Some(StashChange::Dropped(entry)), Direction::Redo) => {
                !now.stashes.iter().any(|stash| stash.id == entry.id)
            }
            _ => false,
        };
        stash_moved.then(|| "The stash has changed since.".to_owned())
    }

    /// Moves the repository in `direction` with `git`, which runs in its
    /// working tree, after checking with [`Change::why_not`]. Blocks; run it
    /// off the UI thread.
    pub fn apply(&mut self, git: &Git, direction: Direction) -> anyhow::Result<()> {
        let repository = Repository::open(git.work_dir())?;
        let now = repository.ref_state()?;
        if let Some(why_not) = self.why_not(&now, direction) {
            bail!(why_not);
        }
        let ends = |before: Option<Oid>, after: Option<Oid>| match direction {
            Direction::Undo => (after, before),
            Direction::Redo => (before, after),
        };

        // Branches to switch to must exist first.
        let creates: Vec<RefUpdate> = self
            .branches
            .iter()
            .filter_map(|branch| {
                let (from, to) = ends(branch.before, branch.after);
                (from.is_none()).then(|| ref_update("refs/heads/", &branch.name, from, to))
            })
            .collect();
        if !creates.is_empty() {
            git.update_refs(&creates)?;
        }

        let target_head = match &self.head {
            Some((before, after)) => match direction {
                Direction::Undo => before.clone(),
                Direction::Redo => after.clone(),
            },
            None => now.head.clone(),
        };
        if self.head.is_some() {
            match &target_head {
                Some(HeadPlace::Branch(name)) => git.switch(name)?,
                Some(HeadPlace::Detached(id)) => git.switch_detached(&id.to_string())?,
                Some(HeadPlace::Unborn(name)) => {
                    bail!("{name} has no commits, so it cannot be checked out again.")
                }
                None => bail!("There was no HEAD to go back to."),
            }
        }
        let checked_out = match &target_head {
            Some(HeadPlace::Branch(name)) => Some(name.as_str()),
            _ => None,
        };

        // The branch checked out moves with the files; the others are
        // only references.
        let mut others = Vec::new();
        for branch in &self.branches {
            let (from, to) = ends(branch.before, branch.after);
            match (from, to) {
                (None, _) => {}
                (Some(_), Some(to)) if Some(branch.name.as_str()) == checked_out => {
                    git.reset(&to.to_string(), self.mode)?;
                }
                _ => others.push(ref_update("refs/heads/", &branch.name, from, to)),
            }
        }
        others.extend(self.tags.iter().map(|tag| {
            let (from, to) = ends(tag.before, tag.after);
            ref_update("refs/tags/", &tag.name, from, to)
        }));
        if !others.is_empty() {
            git.update_refs(&others)?;
        }

        match (&mut self.stash, direction) {
            (Some(StashChange::Pushed(_)), Direction::Undo) => {
                if git.unstash(0, true)? == UnstashOutcome::Conflicts {
                    bail!("The stashed changes conflict with your files; the stash is kept.");
                }
            }
            (Some(StashChange::Pushed(entry)), Direction::Redo) => {
                let message = entry
                    .recorded
                    .split_once(": ")
                    .filter(|(prefix, _)| prefix.starts_with("On "))
                    .map(|(_, message)| message.to_owned());
                if !git.stash(message.as_deref(), entry.untracked)? {
                    bail!("There are no changes to stash again.");
                }
                // Stashing again makes a new stash commit.
                if let Some(top) = repository.ref_state()?.stashes.into_iter().next() {
                    *entry = top;
                }
            }
            (Some(StashChange::Dropped(entry)), Direction::Undo) => {
                git.restore_stash(&entry.id.to_string(), &entry.recorded)?;
            }
            (Some(StashChange::Dropped(entry)), Direction::Redo) => {
                let index = repository
                    .ref_state()?
                    .stashes
                    .iter()
                    .position(|stash| stash.id == entry.id)
                    .context("The stash has changed since.")?;
                git.drop_stash(index)?;
            }
            (None, _) => {}
        }

        if let Some(review) = &self.review {
            let tips = match direction {
                Direction::Undo => &review.before,
                Direction::Redo => &review.after,
            };
            let mut pull_request = review.store.load(review.number)?;
            pull_request.state = tips.state;
            pull_request.base = tips.base.clone();
            pull_request.head = tips.head.clone();
            pull_request.updated = gitez_review::Timestamp::now();
            review.store.save(&pull_request)?;
        }

        match (&mut self.discard, direction) {
            (Some(Discard::Files { saved, .. }), Direction::Undo) => {
                git.restore_discarded(saved)?;
            }
            (Some(Discard::Files { paths, saved }), Direction::Redo) => {
                // Discarding again keeps them in a new stash commit.
                *saved = git
                    .discard(paths.as_deref())?
                    .context("There are no changes to discard again.")?;
            }
            (Some(Discard::Hunk { patch }), direction) => {
                git.apply_to_worktree(patch, direction == Direction::Redo)?;
            }
            (None, _) => {}
        }
        Ok(())
    }
}

/// How the references in `before` and `after`, by name, differ.
fn ref_changes(before: &BTreeMap<String, Oid>, after: &BTreeMap<String, Oid>) -> Vec<RefChange> {
    let mut names: Vec<&String> = before.keys().chain(after.keys()).collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter_map(|name| {
            let (old, new) = (before.get(name), after.get(name));
            (old != new).then(|| RefChange {
                name: name.clone(),
                before: old.copied(),
                after: new.copied(),
            })
        })
        .collect()
}

/// Moving reference `prefix` + `name` from `old` to `new`.
fn ref_update(prefix: &str, name: &str, old: Option<Oid>, new: Option<Oid>) -> RefUpdate {
    RefUpdate {
        name: format!("{prefix}{name}"),
        old: old.map(|id| id.to_string()),
        new: new.map(|id| id.to_string()),
    }
}

/// One entry pushed or dropped between `before` and `after`; `None` when
/// the stash is the same, or changed some other way.
fn stash_change(before: &[StashEntry], after: &[StashEntry]) -> Option<StashChange> {
    if after.len() == before.len() + 1 && after[1..] == *before {
        return Some(StashChange::Pushed(after[0].clone()));
    }
    if before.len() == after.len() + 1 {
        let index = before
            .iter()
            .zip(after)
            .position(|(old, new)| old != new)
            .unwrap_or(after.len());
        let mut rest = before.to_vec();
        let dropped = rest.remove(index);
        if rest == after {
            return Some(StashChange::Dropped(dropped));
        }
    }
    None
}

/// Reads a repository before a change, to record what the change did.
pub struct Recorder {
    repository: Option<Repository>,
    before: Option<RefState>,
}

/// What a change did to a repository, ready for [`record`].
pub struct Recorded {
    key: PathBuf,
    change: Change,
}

impl Recorded {
    /// The changes `discard` threw away in the repository in `dir`.
    pub fn discard(dir: &Path, discard: Discard) -> Option<Self> {
        let key = Repository::open(dir).ok()?.common_dir();
        Some(Self {
            key,
            change: Change {
                head: None,
                branches: Vec::new(),
                tags: Vec::new(),
                stash: None,
                mode: ResetMode::Keep,
                review: None,
                discard: Some(discard),
            },
        })
    }

    /// Also puts back the local pull request `review`.
    pub fn with_review(self, review: ReviewChange) -> Self {
        Self {
            key: self.key,
            change: self.change.with_review(review),
        }
    }
}

impl Recorder {
    /// Reads the repository in `dir`. Blocks; call it on the thread that
    /// makes the change, just before it.
    pub fn start(dir: &Path) -> Self {
        let repository = Repository::open(dir).ok();
        let before = repository
            .as_ref()
            .and_then(|repository| repository.ref_state().ok());
        Self { repository, before }
    }

    /// What changed since [`Recorder::start`]; `None` when nothing did or
    /// the repository could not be read.
    pub fn finish(self, mode: ResetMode) -> Option<Recorded> {
        let repository = self.repository?;
        let after = repository.ref_state().ok()?;
        let change = Change::between(&self.before?, &after, mode)?;
        Some(Recorded {
            key: repository.common_dir(),
            change,
        })
    }
}

/// One step that can be undone or redone.
#[derive(Clone)]
pub struct Entry {
    pub id: u64,
    /// What it did, such as "Commit “Fix the parser”".
    pub label: String,
    pub change: Change,
}

#[derive(Default)]
struct Log {
    undo: Vec<Entry>,
    redo: Vec<Entry>,
}

/// The undo and redo steps of every repository GitEZ changed, by git
/// common directory.
#[derive(Default)]
pub struct UndoLog {
    logs: HashMap<PathBuf, Log>,
    next_id: u64,
}

impl Global for UndoLog {}

/// Adds `recorded` as the newest step, `label` saying what it did, and
/// forgets the steps that could be redone. Returns its id.
pub fn record(cx: &mut App, recorded: Option<Recorded>, label: impl Into<String>) -> Option<u64> {
    let recorded = recorded?;
    let undo = cx.default_global::<UndoLog>();
    undo.next_id += 1;
    let id = undo.next_id;
    let log = undo.logs.entry(recorded.key).or_default();
    log.redo.clear();
    log.undo.push(Entry {
        id,
        label: label.into(),
        change: recorded.change,
    });
    if log.undo.len() > LIMIT {
        log.undo.remove(0);
    }
    Some(id)
}

/// The step that would be undone, or redone, next in the repository with
/// git common directory `key`.
pub fn next(cx: &App, key: &Path, direction: Direction) -> Option<Entry> {
    let log = cx.try_global::<UndoLog>()?.logs.get(key)?;
    match direction {
        Direction::Undo => log.undo.last(),
        Direction::Redo => log.redo.last(),
    }
    .cloned()
}

/// Takes the next step `direction`, if its id is `id`, to run it.
pub fn take(cx: &mut App, key: &Path, direction: Direction, id: u64) -> Option<Entry> {
    let log = cx.default_global::<UndoLog>().logs.get_mut(key)?;
    let stack = match direction {
        Direction::Undo => &mut log.undo,
        Direction::Redo => &mut log.redo,
    };
    if stack.last()?.id != id {
        return None;
    }
    stack.pop()
}

/// Puts `entry` back after running it `direction`: on the other stack when
/// it `ran`, on its own when it failed.
pub fn put_back(cx: &mut App, key: &Path, direction: Direction, entry: Entry, ran: bool) {
    let log = cx
        .default_global::<UndoLog>()
        .logs
        .entry(key.to_owned())
        .or_default();
    let undoable = (direction == Direction::Undo) != ran;
    if undoable {
        log.undo.push(entry);
    } else {
        log.redo.push(entry);
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use gitez_command::{Git, ResetMode};
    use gitez_git::Repository;
    use gitez_review::{NewPullRequest, State, Store, Timestamp, Tip};

    use super::{Change, Direction, Discard, Recorded, Recorder, ReviewChange, ReviewTips};

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temporary directory");
        let root = dir.path();
        git(root, &["init", "--quiet", "--initial-branch=main"]);
        for (key, value) in [
            ("user.name", "Test"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
        ] {
            git(root, &["config", key, value]);
        }
        std::fs::write(root.join("a.txt"), "a\n").expect("write");
        git(root, &["add", "a.txt"]);
        git(root, &["commit", "--quiet", "-m", "a"]);
        dir
    }

    /// Runs `change` as a recorded step, then undoes and redoes it.
    fn record(root: &std::path::Path, mode: ResetMode, change: impl FnOnce()) -> Change {
        let recorder = Recorder::start(root);
        change();
        recorder.finish(mode).expect("something changed").change
    }

    fn state(root: &std::path::Path) -> String {
        format!(
            "{} | {} | {}",
            git(root, &["branch", "--show-current"]),
            git(
                root,
                &[
                    "for-each-ref",
                    "--format=%(refname:short)=%(objectname:short)",
                    "refs/heads"
                ]
            )
            .replace('\n', " "),
            git(root, &["status", "--porcelain"]).replace('\n', " ")
        )
    }

    #[test]
    fn undoes_and_redoes_a_commit_softly() {
        let dir = repository();
        let root = dir.path();
        let tool = Git::new(root);
        std::fs::write(root.join("a.txt"), "a\nb\n").expect("write");
        git(root, &["add", "a.txt"]);
        let before = state(root);
        let mut change = record(root, ResetMode::Soft, || {
            tool.commit("b").expect("commit");
        });
        let after = state(root);

        change.apply(&tool, Direction::Undo).expect("undo");
        // The commit's change is staged again.
        assert_eq!(state(root), before);
        change.apply(&tool, Direction::Redo).expect("redo");
        assert_eq!(state(root), after);
    }

    #[test]
    fn undoes_a_branch_created_and_checked_out_then_a_rename() {
        let dir = repository();
        let root = dir.path();
        let tool = Git::new(root);
        let start = state(root);
        let mut created = record(root, ResetMode::Keep, || {
            tool.create_branch("topic", "main", true).expect("create");
        });
        let mut renamed = record(root, ResetMode::Keep, || {
            tool.rename_branch("topic", "feature").expect("rename");
        });
        let end = state(root);

        renamed.apply(&tool, Direction::Undo).expect("undo rename");
        assert!(state(root).starts_with("topic |"), "{}", state(root));
        created.apply(&tool, Direction::Undo).expect("undo create");
        assert_eq!(state(root), start);

        created.apply(&tool, Direction::Redo).expect("redo create");
        renamed.apply(&tool, Direction::Redo).expect("redo rename");
        assert_eq!(state(root), end);
    }

    #[test]
    fn refuses_once_a_branch_moved() {
        let dir = repository();
        let root = dir.path();
        let tool = Git::new(root);
        std::fs::write(root.join("a.txt"), "a\nb\n").expect("write");
        let mut change = record(root, ResetMode::Soft, || {
            git(root, &["commit", "--quiet", "-am", "b"]);
        });
        std::fs::write(root.join("a.txt"), "a\nb\nc\n").expect("write");
        git(root, &["commit", "--quiet", "-am", "c"]);
        let now = Repository::open(root)
            .expect("open")
            .ref_state()
            .expect("state");
        assert_eq!(
            change.why_not(&now, Direction::Undo).as_deref(),
            Some("main has moved since.")
        );
        let tip = git(root, &["rev-parse", "HEAD"]);
        assert!(change.apply(&tool, Direction::Undo).is_err());
        assert_eq!(git(root, &["rev-parse", "HEAD"]), tip);
    }

    #[test]
    fn undoes_a_merge_into_another_branch_and_comes_back() {
        let dir = repository();
        let root = dir.path();
        let tool = Git::new(root);
        git(root, &["switch", "--quiet", "-c", "feature"]);
        std::fs::write(root.join("f.txt"), "f\n").expect("write");
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "--quiet", "-m", "f"]);
        let before = state(root);
        // A merge by drag: main is checked out and moves.
        let mut change = record(root, ResetMode::Keep, || {
            tool.merge_branch(
                "feature",
                "main",
                gitez_command::MergeMethod::Merge,
                "Merge feature",
                Some("feature"),
            )
            .expect("merge");
        });
        let after = state(root);
        change.apply(&tool, Direction::Undo).expect("undo");
        assert_eq!(state(root), before);
        change.apply(&tool, Direction::Redo).expect("redo");
        assert_eq!(state(root), after);
    }

    #[test]
    fn undoing_a_merge_reopens_its_pull_request() {
        let dir = repository();
        let root = dir.path();
        let tool = Git::new(root);
        git(root, &["switch", "--quiet", "-c", "feature"]);
        std::fs::write(root.join("f.txt"), "f\n").expect("write");
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "--quiet", "-m", "f"]);
        git(root, &["switch", "--quiet", "main"]);
        let tip = |branch: &str| Tip {
            branch: branch.to_owned(),
            commit: git(root, &["rev-parse", branch]),
        };
        let store = Store::new(&Repository::open(root).expect("open").common_dir());
        let opened = store
            .create(
                NewPullRequest {
                    title: "Feature".to_owned(),
                    description: String::new(),
                    base: tip("main"),
                    head: tip("feature"),
                },
                Timestamp::now(),
            )
            .expect("create");
        let recorder = Recorder::start(root);
        tool.merge_branch(
            "feature",
            "main",
            gitez_command::MergeMethod::Merge,
            "Merge feature",
            Some("main"),
        )
        .expect("merge");
        let mut merged = opened.clone();
        merged.state = State::Merged;
        merged.base = tip("main");
        store.save(&merged).expect("save");
        let review = ReviewChange {
            store: store.clone(),
            number: opened.number,
            before: ReviewTips::of(&opened),
            after: ReviewTips::of(&merged),
        };
        let mut change = recorder
            .finish(ResetMode::Keep)
            .expect("main moved")
            .with_review(review)
            .change;

        change.apply(&tool, Direction::Undo).expect("undo");
        let reopened = store.load(opened.number).expect("load");
        assert_eq!(ReviewTips::of(&reopened), ReviewTips::of(&opened));
        change.apply(&tool, Direction::Redo).expect("redo");
        let again = store.load(opened.number).expect("load");
        assert_eq!(ReviewTips::of(&again), ReviewTips::of(&merged));
    }

    #[test]
    fn undoes_creating_and_deleting_tags() {
        let dir = repository();
        let root = dir.path();
        let tool = Git::new(root);
        let tags = || git(root, &["tag", "--list"]).replace('\n', " ");
        let mut created = record(root, ResetMode::Keep, || {
            tool.create_tag("v1", "HEAD", Some("Release one"))
                .expect("create");
        });
        let annotated = git(root, &["rev-parse", "refs/tags/v1"]);
        let mut deleted = record(root, ResetMode::Keep, || {
            tool.delete_tag("v1").expect("delete");
        });

        deleted.apply(&tool, Direction::Undo).expect("undo delete");
        // The same tag object, message and all, is back.
        assert_eq!(git(root, &["rev-parse", "refs/tags/v1"]), annotated);
        created.apply(&tool, Direction::Undo).expect("undo create");
        assert_eq!(tags(), "");
        created.apply(&tool, Direction::Redo).expect("redo create");
        assert_eq!(tags(), "v1");

        git(root, &["tag", "--force", "v1", "HEAD"]);
        let now = Repository::open(root)
            .expect("open")
            .ref_state()
            .expect("state");
        assert_eq!(
            deleted.why_not(&now, Direction::Redo).as_deref(),
            Some("Tag v1 has moved since.")
        );
    }

    #[test]
    fn undoes_and_redoes_a_discard() {
        let dir = repository();
        let root = dir.path();
        let tool = Git::new(root);
        std::fs::write(root.join("a.txt"), "a\nlocal\n").expect("write");
        std::fs::write(root.join("new.txt"), "new\n").expect("write");
        let dirty = state(root);
        let paths = vec!["a.txt".to_owned(), "new.txt".to_owned()];
        let saved = tool
            .discard(Some(&paths))
            .expect("discard")
            .expect("something to discard");
        let clean = state(root);
        let mut change = Recorded::discard(
            root,
            Discard::Files {
                paths: Some(paths),
                saved,
            },
        )
        .expect("a repository")
        .change;

        change.apply(&tool, Direction::Undo).expect("undo");
        assert_eq!(state(root), dirty);
        change.apply(&tool, Direction::Redo).expect("redo");
        assert_eq!(state(root), clean);
        change.apply(&tool, Direction::Undo).expect("undo again");
        assert_eq!(state(root), dirty);
    }

    #[test]
    fn undoes_a_stash_and_a_drop() {
        let dir = repository();
        let root = dir.path();
        let tool = Git::new(root);
        std::fs::write(root.join("a.txt"), "a\nlocal\n").expect("write");
        std::fs::write(root.join("new.txt"), "new\n").expect("write");
        let dirty = state(root);
        let mut stashed = record(root, ResetMode::Keep, || {
            tool.stash(Some("Half done"), true).expect("stash");
        });
        let clean = state(root);
        stashed.apply(&tool, Direction::Undo).expect("undo stash");
        assert_eq!(state(root), dirty);
        assert!(git(root, &["stash", "list"]).is_empty());
        stashed.apply(&tool, Direction::Redo).expect("redo stash");
        assert_eq!(state(root), clean);
        assert!(git(root, &["stash", "list"]).ends_with("On main: Half done"));

        let mut dropped = record(root, ResetMode::Keep, || {
            tool.drop_stash(0).expect("drop");
        });
        dropped.apply(&tool, Direction::Undo).expect("undo drop");
        assert!(git(root, &["stash", "list"]).ends_with("On main: Half done"));
        dropped.apply(&tool, Direction::Redo).expect("redo drop");
        assert!(git(root, &["stash", "list"]).is_empty());
    }
}
