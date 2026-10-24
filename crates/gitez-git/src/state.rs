//! What GitEZ's undo puts back: where `HEAD` is, the local branches, the
//! tags and the stash.

use std::collections::BTreeMap;

use crate::{Head, Oid, Repository, Result};

/// Where `HEAD` is, without the commit a branch is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadPlace {
    Branch(String),
    /// A branch with no commits yet.
    Unborn(String),
    Detached(Oid),
}

/// An entry of the stash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashEntry {
    pub id: Oid,
    /// The message as git recorded it.
    pub recorded: String,
    /// Whether it holds untracked files too (`--include-untracked`).
    pub untracked: bool,
}

/// `HEAD`, the local branches, the tags and the stash at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefState {
    pub head: Option<HeadPlace>,
    /// Branch name to the commit it is on.
    pub branches: BTreeMap<String, Oid>,
    /// Tag name to what the tag reference points to: the tag object of an
    /// annotated tag.
    pub tags: BTreeMap<String, Oid>,
    /// Newest first, as `git stash list` shows them.
    pub stashes: Vec<StashEntry>,
}

impl Repository {
    /// Reads `HEAD`, the local branches, the tags and the stash.
    pub fn ref_state(&self) -> Result<RefState> {
        let refs = self.refs()?;
        let repo = self.local();
        let head = refs.head.map(|head| match head {
            Head::Branch { name, .. } => HeadPlace::Branch(name),
            Head::Unborn { name } => HeadPlace::Unborn(name),
            Head::Detached { target } => HeadPlace::Detached(target),
        });
        let branches = refs
            .branches
            .into_iter()
            .map(|branch| (branch.name, branch.target))
            .collect();
        let tags = refs
            .tags
            .into_iter()
            .map(|tag| (tag.name, tag.id))
            .collect();
        let mut stashes = Vec::with_capacity(refs.stashes.len());
        for stash in refs.stashes {
            // The untracked files are the stash commit's third parent.
            let untracked = repo.find_commit(stash.id)?.parent_ids().count() > 2;
            stashes.push(StashEntry {
                id: stash.id,
                recorded: stash.recorded,
                untracked,
            });
        }
        Ok(RefState {
            head,
            branches,
            tags,
            stashes,
        })
    }
}
