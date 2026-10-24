use std::collections::HashMap;

use gix::bstr::{BStr, ByteSlice};
use gix::refs::Category;

use crate::{Error, Oid, Repository, Result, Timestamp};

/// Where `HEAD` points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// `HEAD` is on a branch.
    Branch { name: String, target: Oid },
    /// `HEAD` names a branch that has no commits yet, as in a new repository.
    Unborn { name: String },
    /// `HEAD` points straight at a commit.
    Detached { target: Oid },
}

impl Head {
    /// The commit `HEAD` resolves to, if any.
    pub fn target(&self) -> Option<Oid> {
        match self {
            Head::Branch { target, .. } | Head::Detached { target } => Some(*target),
            Head::Unborn { .. } => None,
        }
    }
}

/// A local branch (`refs/heads/*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    /// Short name, such as `feature/graph`.
    pub name: String,
    pub target: Oid,
    /// Short name of the remote-tracking branch it follows, such as
    /// `origin/main`, if one is configured.
    pub upstream: Option<String>,
    /// Whether `HEAD` is on this branch.
    pub is_head: bool,
}

/// A remote-tracking branch (`refs/remotes/<remote>/*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBranch {
    pub remote: String,
    /// Branch name without the remote, such as `main`.
    pub name: String,
    pub target: Oid,
}

impl RemoteBranch {
    /// The name as git shows it, such as `origin/main`.
    pub fn short_name(&self) -> String {
        format!("{}/{}", self.remote, self.name)
    }
}

/// A tag (`refs/tags/*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub name: String,
    /// The object the tag finally points to, after peeling annotated tags.
    /// Usually a commit.
    pub target: Oid,
    pub annotated: bool,
}

/// An entry of the stash, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
    /// Position in the stash: `0` is `stash@{0}`.
    pub index: usize,
    pub id: Oid,
    /// The branch the stash was made on, when the message records it.
    pub branch: Option<String>,
    /// The stash message without git's `On <branch>:` prefix.
    pub message: String,
    pub time: Timestamp,
}

/// All references shown in the sidebar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Refs {
    pub head: Option<Head>,
    /// Sorted by name.
    pub branches: Vec<Branch>,
    /// Sorted by remote, then name.
    pub remote_branches: Vec<RemoteBranch>,
    /// Sorted by name.
    pub tags: Vec<Tag>,
    pub stashes: Vec<Stash>,
}

/// What kind of reference a [`RefLabel`] shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefKind {
    /// `HEAD` when it is detached.
    Head,
    Branch,
    RemoteBranch,
    Tag,
}

/// A reference decorating a commit row in the history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefLabel {
    pub kind: RefKind,
    pub name: String,
    /// Set on the branch `HEAD` is on, and on a detached `HEAD`.
    pub is_head: bool,
}

impl Refs {
    /// Commits to start a history walk from: `HEAD`, every branch and tag,
    /// and remote-tracking branches when `include_remotes` is set. Stashes are
    /// left out, as in `git log --all` they clutter the graph.
    pub fn tips(&self, include_remotes: bool) -> Vec<Oid> {
        let mut tips = Vec::new();
        tips.extend(self.head.as_ref().and_then(Head::target));
        tips.extend(self.branches.iter().map(|b| b.target));
        if include_remotes {
            tips.extend(self.remote_branches.iter().map(|b| b.target));
        }
        tips.extend(self.tags.iter().map(|t| t.target));
        tips
    }

    /// Labels to draw next to each commit, keyed by commit. Labels for one
    /// commit come in sidebar order: `HEAD`, branches, remotes, tags.
    pub fn labels_by_commit(&self) -> HashMap<Oid, Vec<RefLabel>> {
        let mut labels: HashMap<Oid, Vec<RefLabel>> = HashMap::new();
        let mut add = |target: Oid, kind, name: String, is_head| {
            labels.entry(target).or_default().push(RefLabel {
                kind,
                name,
                is_head,
            });
        };
        if let Some(Head::Detached { target }) = &self.head {
            add(*target, RefKind::Head, "HEAD".to_owned(), true);
        }
        for branch in &self.branches {
            add(
                branch.target,
                RefKind::Branch,
                branch.name.clone(),
                branch.is_head,
            );
        }
        for branch in &self.remote_branches {
            add(
                branch.target,
                RefKind::RemoteBranch,
                branch.short_name(),
                false,
            );
        }
        for tag in &self.tags {
            add(tag.target, RefKind::Tag, tag.name.clone(), false);
        }
        labels
    }
}

impl Repository {
    /// Reads `HEAD`, branches, remote-tracking branches, tags and stashes.
    pub fn refs(&self) -> Result<Refs> {
        let repo = self.local();
        let head = read_head(&repo)?;
        let head_branch = match &head {
            Some(Head::Branch { name, .. }) => Some(name.as_str()),
            _ => None,
        };
        let remote_names = repo.remote_names();

        let mut refs = Refs::default();
        let platform = repo.references()?;
        for reference in platform.all()? {
            let mut reference = reference?;
            let name = reference.name().to_owned();
            let Some((category, short)) = name.category_and_short_name() else {
                continue;
            };
            // Symbolic refs such as `refs/remotes/origin/HEAD` only alias
            // another ref that is listed on its own.
            let Some(direct) = reference.try_id().map(|id| id.detach()) else {
                continue;
            };
            match category {
                Category::LocalBranch => {
                    let short = lossy(short);
                    let upstream = repo
                        .branch_remote_tracking_ref_name(
                            name.as_ref(),
                            gix::remote::Direction::Fetch,
                        )
                        .and_then(|upstream| upstream.ok())
                        .map(|upstream| lossy(upstream.shorten()));
                    refs.branches.push(Branch {
                        is_head: head_branch == Some(short.as_str()),
                        name: short,
                        target: direct,
                        upstream,
                    });
                }
                Category::RemoteBranch => {
                    let (remote, branch) = split_remote(short, &remote_names);
                    refs.remote_branches.push(RemoteBranch {
                        remote,
                        name: branch,
                        target: direct,
                    });
                }
                Category::Tag => {
                    let target = reference.peel_to_id()?.detach();
                    refs.tags.push(Tag {
                        name: lossy(short),
                        annotated: target != direct,
                        target,
                    });
                }
                _ => {}
            }
        }
        refs.branches.sort_by(|a, b| a.name.cmp(&b.name));
        refs.remote_branches
            .sort_by(|a, b| (&a.remote, &a.name).cmp(&(&b.remote, &b.name)));
        refs.tags.sort_by(|a, b| a.name.cmp(&b.name));
        refs.stashes = read_stashes(&repo)?;
        refs.head = head;
        Ok(refs)
    }
}

fn read_head(repo: &gix::Repository) -> Result<Option<Head>> {
    let head = repo.head()?;
    Ok(Some(match head.kind {
        gix::head::Kind::Symbolic(reference) => {
            let target = match reference.target.try_id() {
                Some(id) => id.to_owned(),
                None => return Ok(None),
            };
            Head::Branch {
                name: lossy(reference.name.shorten()),
                target,
            }
        }
        gix::head::Kind::Unborn(name) => Head::Unborn {
            name: lossy(name.shorten()),
        },
        gix::head::Kind::Detached { target, .. } => Head::Detached { target },
    }))
}

fn read_stashes(repo: &gix::Repository) -> Result<Vec<Stash>> {
    const STASH: &str = "refs/stash";
    let Some(stash) = repo.try_find_reference(STASH)? else {
        return Ok(Vec::new());
    };
    let reflog_error = |source| Error::Reflog {
        name: STASH.to_owned(),
        source,
    };
    let mut platform = stash.log_iter();
    let Some(lines) = platform.rev().map_err(reflog_error)? else {
        return Ok(Vec::new());
    };
    let mut stashes = Vec::new();
    for (index, line) in lines.enumerate() {
        let line = line.map_err(|err| reflog_error(std::io::Error::other(err)))?;
        let (branch, message) = parse_stash_message(line.message.as_ref());
        stashes.push(Stash {
            index,
            id: line.new_oid,
            branch,
            message,
            time: line.signature.time.into(),
        });
    }
    Ok(stashes)
}

/// Splits git's stash message, `On <branch>: <message>` or
/// `WIP on <branch>: <commit summary>`, into branch and message.
fn parse_stash_message(raw: &BStr) -> (Option<String>, String) {
    let raw = raw.to_str_lossy();
    let rest = raw
        .strip_prefix("WIP on ")
        .or_else(|| raw.strip_prefix("On "));
    match rest.and_then(|rest| rest.split_once(": ")) {
        Some((branch, message)) => (Some(branch.to_owned()), message.to_owned()),
        None => (None, raw.into_owned()),
    }
}

/// Splits `origin/feature/x` into remote and branch. Remote names may contain
/// `/`, so the longest configured remote that matches wins; without one, the
/// first path component is taken as the remote.
fn split_remote(short: &BStr, remotes: &gix::remote::Names) -> (String, String) {
    let short = lossy(short);
    let configured = remotes
        .iter()
        .map(|remote| remote.to_str_lossy())
        .filter(|remote| {
            short.len() > remote.len()
                && short.starts_with(remote.as_ref())
                && short.as_bytes()[remote.len()] == b'/'
        })
        .max_by_key(|remote| remote.len())
        .map(|remote| remote.len());
    let split_at = configured.or_else(|| short.find('/'));
    match split_at {
        Some(at) => (short[..at].to_owned(), short[at + 1..].to_owned()),
        None => (String::new(), short),
    }
}

fn lossy(bytes: &BStr) -> String {
    bytes.to_str_lossy().into_owned()
}

/// Why `name` cannot name a branch, as `git check-ref-format --branch`
/// would refuse it; `None` when it can.
pub fn branch_name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("Name the branch.");
    }
    if name.chars().any(char::is_whitespace) {
        return Some("A branch name cannot contain spaces.");
    }
    if name.starts_with('-') {
        return Some("A branch name cannot start with “-”.");
    }
    if name == "HEAD" {
        return Some("“HEAD” cannot name a branch.");
    }
    if gix::refs::FullName::try_from(format!("refs/heads/{name}")).is_err() {
        return Some(
            "A branch name cannot contain “..”, “~”, “^”, “:”, “?”, “*”, “[”, “\\” or “@{”, \
             or start or end with “/” or “.”, or end with “.lock”.",
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stash_messages() {
        assert_eq!(
            parse_stash_message("On main: Diff toolbar spacing".into()),
            (Some("main".into()), "Diff toolbar spacing".into())
        );
        assert_eq!(
            parse_stash_message("WIP on feature/x: abc1234 Fix it".into()),
            (Some("feature/x".into()), "abc1234 Fix it".into())
        );
        assert_eq!(
            parse_stash_message("custom".into()),
            (None, "custom".into())
        );
    }

    #[test]
    fn remote_split_prefers_longest_configured_remote() {
        let remotes: gix::remote::Names = ["origin".into(), "team/a".into()].into();
        assert_eq!(
            split_remote("team/a/main".into(), &remotes),
            ("team/a".into(), "main".into())
        );
        assert_eq!(
            split_remote("origin/feature/x".into(), &remotes),
            ("origin".into(), "feature/x".into())
        );
        assert_eq!(
            split_remote("unknown/main".into(), &remotes),
            ("unknown".into(), "main".into())
        );
    }
}
