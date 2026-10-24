//! What rewriting a branch's recent history works on: the commits from one
//! of them up to the branch's tip, and a rebase git stopped in the middle of.

use gix::bstr::ByteSlice;

use crate::{Error, Oid, Repository, Result};

/// A commit that can be rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteCommit {
    pub id: Oid,
    /// First line of the message.
    pub summary: String,
    /// The whole message as written, without trailing blank lines.
    pub message: String,
    /// Whether the branch's upstream already has it.
    pub pushed: bool,
}

/// The commit a rewrite builds on, which stays as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteBase {
    pub id: Oid,
    pub summary: String,
}

/// What can be rewritten from a commit up to a branch's tip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rewritable {
    /// The commits, newest first, and the commit before the oldest of them;
    /// `None` when the oldest is a root commit.
    Commits {
        commits: Vec<RewriteCommit>,
        base: Option<RewriteBase>,
    },
    /// The commit is not in the branch's history.
    Elsewhere,
    /// Merge commit `id` is between the commit and the tip; GitEZ rewrites
    /// only a straight line of commits.
    Merge { id: Oid, summary: String },
}

/// A rebase git stopped in the middle of: at an `edit`, on a conflict or
/// on a command that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseInProgress {
    /// The branch being rebased, unless `HEAD` was detached.
    pub branch: Option<String>,
    /// The commit it stopped at, if git says.
    pub stopped_at: Option<Oid>,
    /// The first line of that commit's message.
    pub summary: Option<String>,
}

impl Repository {
    /// The commits from `from` up to `tip`, following first parents, and
    /// for each whether `upstream` has it.
    pub fn rewritable(&self, tip: Oid, from: Oid, upstream: Option<Oid>) -> Result<Rewritable> {
        let repo = self.local();
        if !self.contains(tip, from)? {
            return Ok(Rewritable::Elsewhere);
        }
        let mut commits = Vec::new();
        let mut next = Some(tip);
        while let Some(id) = next {
            let commit = repo.find_commit(id)?;
            let message = commit.message_raw()?.to_str_lossy().trim_end().to_owned();
            let summary = message.lines().next().unwrap_or_default().to_owned();
            let parents: Vec<Oid> = commit.parent_ids().map(|parent| parent.detach()).collect();
            if parents.len() > 1 {
                return Ok(Rewritable::Merge { id, summary });
            }
            commits.push(RewriteCommit {
                id,
                summary,
                message,
                pushed: false,
            });
            if id == from {
                let base = match parents.first() {
                    Some(&parent) => Some(RewriteBase {
                        id: parent,
                        summary: repo
                            .find_commit(parent)?
                            .message()?
                            .summary()
                            .to_str_lossy()
                            .into_owned(),
                    }),
                    None => None,
                };
                if let Some(upstream) = upstream {
                    // The upstream has an unbroken run of the oldest ones.
                    for commit in commits.iter_mut().rev() {
                        if !self.contains(upstream, commit.id)? {
                            break;
                        }
                        commit.pushed = true;
                    }
                }
                return Ok(Rewritable::Commits { commits, base });
            }
            next = parents.first().copied();
        }
        // Reached a root commit without meeting `from`: it is on a side
        // line that the first parents skip.
        Ok(Rewritable::Elsewhere)
    }

    /// The commit `rev` names, as `git rev-parse rev^{commit}` would find
    /// it; `None` when it names none.
    pub fn resolve_commit(&self, rev: &str) -> Result<Option<Oid>> {
        let repo = self.local();
        let Ok(id) = repo.rev_parse_single(rev) else {
            return Ok(None);
        };
        Ok(id.object()?.peel_to_commit().ok().map(|commit| commit.id))
    }

    /// The first commit after `base` on the line of first parents that
    /// leads to `tip`: where rewriting the commits since `base` starts.
    /// `None` when `base` is not on that line, or is `tip` itself.
    pub fn first_after(&self, tip: Oid, base: Oid) -> Result<Option<Oid>> {
        if tip == base || !self.contains(tip, base)? {
            return Ok(None);
        }
        let repo = self.local();
        let mut current = tip;
        loop {
            let parent = repo
                .find_commit(current)?
                .parent_ids()
                .next()
                .map(|parent| parent.detach());
            match parent {
                Some(parent) if parent == base => return Ok(Some(current)),
                Some(parent) => current = parent,
                // `base` is only on a side line.
                None => return Ok(None),
            }
        }
    }

    /// Whether commit `id` is in the history of commit `tip`.
    fn contains(&self, tip: Oid, id: Oid) -> Result<bool> {
        Ok(self.local().merge_base(id, tip)?.map(|base| base.detach()) == Some(id))
    }

    /// The rebase in progress in this working tree, if git stopped one.
    pub fn rebase_in_progress(&self) -> Result<Option<RebaseInProgress>> {
        let read = |name: &str| {
            let path = self.git_dir().join(name);
            match std::fs::read_to_string(&path) {
                Ok(text) => Ok(Some(text.trim().to_owned())),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(source) => Err(Error::ReadFile { path, source }),
            }
        };
        // `rebase-merge` for interactive and merge-based rebases,
        // `rebase-apply` for the old patch-based ones.
        let (dir, head_name) = match read("rebase-merge/head-name")? {
            Some(head_name) => ("rebase-merge", head_name),
            None => match read("rebase-apply/head-name")? {
                Some(head_name) => ("rebase-apply", head_name),
                None => return Ok(None),
            },
        };
        let branch = head_name.strip_prefix("refs/heads/").map(str::to_owned);
        let stopped_at = match read(&format!("{dir}/stopped-sha"))? {
            Some(hex) => Some(hex),
            // The last command done names the commit it was working on.
            None => read(&format!("{dir}/done"))?.and_then(|done| {
                done.lines()
                    .last()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .map(str::to_owned)
            }),
        };
        let repo = self.local();
        let stopped_at = stopped_at
            .and_then(|hex| repo.rev_parse_single(hex.as_str()).ok())
            .map(|id| id.detach());
        let summary = match stopped_at {
            Some(id) => Some(
                repo.find_commit(id)?
                    .message()?
                    .summary()
                    .to_str_lossy()
                    .into_owned(),
            ),
            None => None,
        };
        Ok(Some(RebaseInProgress {
            branch,
            stopped_at,
            summary,
        }))
    }
}
