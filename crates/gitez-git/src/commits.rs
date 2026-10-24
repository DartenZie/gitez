use std::collections::HashSet;

use gix::bstr::ByteSlice;
use gix::traverse::commit::{Topo, topo};

use crate::{Oid, Repository, Result};

/// A point in time as recorded by git: seconds since the Unix epoch plus the
/// author's UTC offset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    pub seconds: i64,
    /// Offset from UTC in seconds, east positive.
    pub offset: i32,
}

impl From<gix::date::Time> for Timestamp {
    fn from(time: gix::date::Time) -> Self {
        Self {
            seconds: time.seconds,
            offset: time.offset,
        }
    }
}

/// A name, email and time from a commit header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub time: Timestamp,
}

/// The parts of a commit shown in a history row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub id: Oid,
    /// Parents in order; the first parent is the branch the commit was made
    /// on. Empty for root commits and for the boundary of a shallow clone.
    pub parents: Vec<Oid>,
    /// First line of the message.
    pub summary: String,
    pub author: Signature,
    pub committed: Timestamp,
}

/// Order of commits in a history walk. Both orders show every child before
/// its parents, which the graph layout relies on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Order {
    /// Commit date order, like `git log --date-order`.
    #[default]
    Date,
    /// Keep each line of history together, like `git log --topo-order`.
    Topo,
}

/// Commits in history order. Created by [`Repository::walk`].
///
/// The walk does blocking I/O on every step; drive it from a background task,
/// for example with [`CommitWalk::next_batch`].
pub struct CommitWalk {
    repo: gix::Repository,
    topo: Topo<ShallowFind, fn(&gix::oid) -> bool>,
    /// Commits whose parents were cut off by a shallow clone.
    shallow: HashSet<Oid>,
}

impl Repository {
    /// Walks history from `tips`, newest first. Tips that are not commits
    /// (tags on trees or blobs) are skipped.
    pub fn walk(&self, tips: impl IntoIterator<Item = Oid>, order: Order) -> Result<CommitWalk> {
        self.walk_excluding(tips, [], order)
    }

    /// Walks the commits reachable from `tips` but not from `hidden`, like
    /// `git log tips ^hidden`.
    pub fn walk_excluding(
        &self,
        tips: impl IntoIterator<Item = Oid>,
        hidden: impl IntoIterator<Item = Oid>,
        order: Order,
    ) -> Result<CommitWalk> {
        let repo = self.local();
        let hidden: Vec<Oid> = hidden.into_iter().collect();
        let mut commit_tips = Vec::new();
        for tip in tips {
            // The topological walk yields a tip even when it is also hidden.
            if hidden.contains(&tip) {
                continue;
            }
            if repo.find_header(tip)?.kind() == gix::object::Kind::Commit {
                commit_tips.push(tip);
            }
        }
        let shallow: HashSet<Oid> = repo
            .shallow_commits()?
            .map(|commits| commits.iter().copied().collect())
            .unwrap_or_default();
        // Git does not use the commit-graph in shallow clones either: its
        // parent lists would point past the boundary.
        let commit_graph = if shallow.is_empty() {
            repo.commit_graph_if_enabled()?
        } else {
            None
        };
        let sorting = match order {
            Order::Date => topo::Sorting::DateOrder,
            Order::Topo => topo::Sorting::TopoOrder,
        };
        let find = ShallowFind {
            objects: repo.objects.clone(),
            shallow: shallow.clone(),
        };
        let topo = topo::Builder::new(find)
            .with_tips(commit_tips)
            .with_ends(hidden)
            .sorting(sorting)
            .with_commit_graph(commit_graph)
            .build()?;
        Ok(CommitWalk {
            repo,
            topo,
            shallow,
        })
    }

    /// How many commits `local` has that `upstream` does not, and the other
    /// way round, like `git rev-list --left-right --count local...upstream`.
    pub fn ahead_behind(&self, local: Oid, upstream: Oid) -> Result<(usize, usize)> {
        let count = |from: Oid, hidden: Oid| -> Result<usize> {
            // Count on the bare walk: decoding each commit is not needed.
            let walk = self.walk_excluding([from], [hidden], Order::Date)?;
            let mut count = 0;
            for info in walk.topo {
                info?;
                count += 1;
            }
            Ok(count)
        };
        Ok((count(local, upstream)?, count(upstream, local)?))
    }
}

impl CommitWalk {
    /// Returns up to `max` further commits; an empty batch means the walk is
    /// done.
    pub fn next_batch(&mut self, max: usize) -> Result<Vec<Commit>> {
        self.by_ref().take(max).collect()
    }

    fn read(&self, id: Oid) -> Result<Commit> {
        let object = self.repo.find_commit(id)?;
        let commit = object.decode()?;
        let author = commit.author()?.trim();
        let parents = if self.shallow.contains(&id) {
            Vec::new()
        } else {
            commit.parents().collect()
        };
        Ok(Commit {
            id,
            parents,
            summary: commit.message().summary().to_str_lossy().into_owned(),
            author: Signature {
                name: author.name.to_str_lossy().into_owned(),
                email: author.email.to_str_lossy().into_owned(),
                // A malformed date should not hide the commit.
                time: author.time().unwrap_or_default().into(),
            },
            committed: commit
                .committer()
                .and_then(|committer| committer.time())
                .unwrap_or_default()
                .into(),
        })
    }
}

/// Object lookup for the topological walk that shows the boundary commits of
/// a shallow clone without parents, as git does. The walk would otherwise
/// try to read the parents the clone left out and fail.
struct ShallowFind {
    objects: gix::OdbHandle,
    shallow: HashSet<Oid>,
}

impl gix::objs::Find for ShallowFind {
    fn try_find<'a>(
        &self,
        id: &gix::oid,
        buffer: &'a mut Vec<u8>,
    ) -> gix::Result<Option<gix::objs::Data<'a>>> {
        if self.shallow.is_empty() || !self.shallow.contains(id) {
            return self.objects.try_find(id, buffer);
        }
        let Some(data) = self.objects.try_find(id, buffer)? else {
            return Ok(None);
        };
        let (kind, object_hash) = (data.kind, data.object_hash);
        let without_parents = strip_parent_headers(data.data);
        *buffer = without_parents;
        Ok(Some(gix::objs::Data {
            kind,
            object_hash,
            data: buffer,
        }))
    }
}

/// Removes the `parent` lines from a raw commit's header.
fn strip_parent_headers(commit: &[u8]) -> Vec<u8> {
    let header_end = commit.find(b"\n\n").map_or(commit.len(), |end| end + 1);
    let (header, rest) = commit.split_at(header_end);
    let mut out = Vec::with_capacity(commit.len());
    for line in header.split_inclusive(|&byte| byte == b'\n') {
        if !line.starts_with(b"parent ") {
            out.extend_from_slice(line);
        }
    }
    out.extend_from_slice(rest);
    out
}

impl Iterator for CommitWalk {
    type Item = Result<Commit>;

    fn next(&mut self) -> Option<Self::Item> {
        let info = match self.topo.next()? {
            Ok(info) => info,
            Err(err) => return Some(Err(err.into())),
        };
        Some(self.read(info.id))
    }
}
