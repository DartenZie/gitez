//! Loading an open repository's state for the UI.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use gitez_git::{Refs, Repository};
use gitez_review::{PullRequest, State, Store};
use gpui_kit::SharedString;

/// What the window shows about a repository, read in one go on a background
/// thread. Replaced wholesale when the repository changes.
#[derive(Debug)]
pub struct Snapshot {
    /// Clone it for each background read.
    pub repository: Repository,
    pub name: SharedString,
    /// The working tree, or the git directory of a bare repository.
    pub path: PathBuf,
    /// Tags are sorted newest version first; everything else as read.
    pub refs: Refs,
    /// How each local branch with an upstream compares to it, by branch name.
    pub tracking: HashMap<String, Tracking>,
    /// Where the local pull requests are stored.
    pub reviews: Store,
    /// The local pull requests: open ones first, then newest first.
    pub pull_requests: Vec<PullRequest>,
    /// Why some local pull requests could not be read, if any could not.
    pub pull_requests_error: Option<String>,
    /// The name commits are made under, shown on review comments.
    pub user_name: Option<String>,
    /// The merge git stopped to have conflicts resolved, if one did.
    pub merge: Option<gitez_git::MergeInProgress>,
}

/// How a local branch compares to its upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracking {
    /// Short name of the upstream, such as `origin/main`.
    pub upstream: String,
    pub ahead: usize,
    pub behind: usize,
}

impl Snapshot {
    /// Opens the repository containing `path` and reads its refs. Blocks; run
    /// it on a background thread.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let repository = Repository::open(path)?;
        let mut refs = repository.refs().context("could not read references")?;
        refs.tags
            .sort_by(|a, b| compare_natural(&b.name, &a.name).then_with(|| b.name.cmp(&a.name)));

        let remote_targets: HashMap<String, _> = refs
            .remote_branches
            .iter()
            .map(|branch| (branch.short_name(), branch.target))
            .collect();
        let mut tracking = HashMap::new();
        for branch in &refs.branches {
            // An upstream that was deleted on the remote has no target.
            let Some(upstream) = &branch.upstream else {
                continue;
            };
            let Some(&upstream_target) = remote_targets.get(upstream) else {
                continue;
            };
            let (ahead, behind) = repository
                .ahead_behind(branch.target, upstream_target)
                .with_context(|| format!("could not compare {} with {upstream}", branch.name))?;
            tracking.insert(
                branch.name.clone(),
                Tracking {
                    upstream: upstream.clone(),
                    ahead,
                    behind,
                },
            );
        }

        let reviews = Store::new(&repository.common_dir());
        let (pull_requests, pull_requests_error) = load_pull_requests(&reviews);
        let user_name = repository.user_name();
        let merge = repository
            .merge_in_progress()
            .context("could not read the merge in progress")?;

        Ok(Self {
            name: repository.name().into(),
            path: repository
                .work_dir()
                .unwrap_or_else(|| repository.git_dir())
                .to_owned(),
            repository,
            refs,
            tracking,
            reviews,
            pull_requests,
            pull_requests_error,
            user_name,
            merge,
        })
    }

    /// The tip of the local branch `name`, if it exists.
    pub fn branch_tip(&self, name: &str) -> Option<gitez_git::Oid> {
        self.refs
            .branches
            .iter()
            .find(|branch| branch.name == name)
            .map(|branch| branch.target)
    }
}

/// Every pull request in `store` that can be read, sorted for the sidebar,
/// and why the others cannot. One unreadable pull request does not hide the
/// rest.
fn load_pull_requests(store: &Store) -> (Vec<PullRequest>, Option<String>) {
    let numbers = match store.numbers() {
        Ok(numbers) => numbers,
        Err(error) => {
            let error = anyhow::Error::from(error).context("could not list local pull requests");
            return (Vec::new(), Some(format!("{error:#}")));
        }
    };
    let mut pull_requests = Vec::with_capacity(numbers.len());
    let mut errors = Vec::new();
    for number in numbers {
        match store.load(number) {
            Ok(pull_request) => pull_requests.push(pull_request),
            // Being created by another GitEZ: its file is not written yet.
            Err(gitez_review::Error::NotFound(_)) => {}
            Err(error) => errors.push(format!("#{number}: {:#}", anyhow::Error::from(error))),
        }
    }
    pull_requests.sort_by_key(|pull_request| {
        (
            pull_request.state != State::Open,
            std::cmp::Reverse(pull_request.number),
        )
    });
    let error = (!errors.is_empty()).then(|| {
        format!(
            "Could not read some local pull requests: {}",
            errors.join("; ")
        )
    });
    (pull_requests, error)
}

/// Compares strings with runs of digits ordered by value, so that `v0.10.0`
/// sorts after `v0.9.1`.
fn compare_natural(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let a_len = a.iter().take_while(|c| c.is_ascii_digit()).count();
                let b_len = b.iter().take_while(|c| c.is_ascii_digit()).count();
                let (a_digits, b_digits) = (&a[..a_len], &b[..b_len]);
                let trim =
                    |digits: &[u8]| -> usize { digits.iter().take_while(|&&c| c == b'0').count() };
                let (a_value, b_value) = (&a_digits[trim(a_digits)..], &b_digits[trim(b_digits)..]);
                let ordering = a_value
                    .len()
                    .cmp(&b_value.len())
                    .then_with(|| a_value.cmp(b_value));
                if ordering != Ordering::Equal {
                    return ordering;
                }
                a = &a[a_len..];
                b = &b[b_len..];
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(y);
                }
                a = &a[1..];
                b = &b[1..];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digit_runs_compare_by_value() {
        assert_eq!(compare_natural("v0.10.0", "v0.9.1"), Ordering::Greater);
        assert_eq!(compare_natural("v1.2", "v1.2"), Ordering::Equal);
        assert_eq!(compare_natural("v1.02", "v1.2"), Ordering::Equal);
        assert_eq!(compare_natural("v1.2", "v1.2.1"), Ordering::Less);
        assert_eq!(compare_natural("release", "v1"), Ordering::Less);
        assert_eq!(compare_natural("a9", "a10"), Ordering::Less);
    }

    #[test]
    fn tags_sort_newest_first() {
        let mut names = vec!["v0.3.2", "v0.10.0", "v0.4.0", "v0.9.1"];
        names.sort_by(|a, b| compare_natural(b, a));
        assert_eq!(names, ["v0.10.0", "v0.9.1", "v0.4.0", "v0.3.2"]);
    }

    /// Runs git in `dir` with a fixed identity and no user or system config.
    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn load_reads_tracking_and_sorts_tags() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let root = dir.path();
        git(root, &["init", "--quiet", "--initial-branch=main"]);
        for message in ["one", "two", "three"] {
            git(root, &["commit", "--quiet", "--allow-empty", "-m", message]);
        }
        git(root, &["tag", "v0.9.0", "HEAD~1"]);
        git(root, &["tag", "v0.10.0"]);
        // `main` is two commits ahead of `origin/main`.
        git(
            root,
            &["remote", "add", "origin", "https://example.com/repo.git"],
        );
        git(root, &["update-ref", "refs/remotes/origin/main", "HEAD~2"]);
        git(root, &["branch", "--set-upstream-to=origin/main", "main"]);
        // `feature` follows a branch that no longer exists on the remote.
        git(root, &["branch", "feature"]);
        git(root, &["config", "branch.feature.remote", "origin"]);
        git(root, &["config", "branch.feature.merge", "refs/heads/gone"]);

        let snapshot = Snapshot::load(root).expect("load the repository");

        assert_eq!(
            snapshot.tracking.get("main"),
            Some(&Tracking {
                upstream: "origin/main".to_owned(),
                ahead: 2,
                behind: 0,
            })
        );
        assert!(!snapshot.tracking.contains_key("feature"));
        let tags: Vec<_> = snapshot
            .refs
            .tags
            .iter()
            .map(|tag| tag.name.as_str())
            .collect();
        assert_eq!(tags, ["v0.10.0", "v0.9.0"]);
    }

    #[test]
    fn load_fails_outside_a_repository() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let error = Snapshot::load(dir.path()).expect_err("not a repository");
        assert!(format!("{error:#}").contains("could not open a git repository"));
    }
}
