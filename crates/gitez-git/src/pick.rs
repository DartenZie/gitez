//! What cherry-picking commits onto a branch would do, tried in memory
//! without touching the working tree, the index or the object database.

use gix::bstr::ByteSlice;
use gix::merge::tree::TreatAsUnresolved;

use crate::{Error, Oid, Repository, Result};

/// How commits would apply to a branch, one after the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickTrial {
    /// Every commit applies. `applied` are those that change something,
    /// in order; the others are already there and would be left out.
    Clean { applied: Vec<Oid>, skipped: usize },
    /// Commit `commit` conflicts in `files`, in path order; the commits
    /// before it apply.
    Conflict { commit: Oid, files: Vec<String> },
}

impl Repository {
    /// What cherry-picking `commits`, oldest first, onto commit `onto`
    /// would do, as `git cherry-pick` would see it. Commits `onto` already
    /// contains are skipped. Merge commits are not supported: the first
    /// parent is taken as their base.
    pub fn cherry_pick_trial(&self, onto: Oid, commits: &[Oid]) -> Result<PickTrial> {
        let mut skipped = 0;
        let mut missing = Vec::new();
        for &id in commits {
            if self.local().merge_base(id, onto)?.map(|base| base.detach()) == Some(id) {
                skipped += 1;
            } else {
                missing.push(id);
            }
        }
        let merge_error = |error: Box<dyn std::error::Error + Send + Sync>| Error::Merge(error);
        // Merged blobs and trees stay in memory.
        let repo = self.local().with_object_memory();
        let options = repo
            .tree_merge_options()
            .map_err(|error| merge_error(Box::new(error)))?;
        let mut ours = repo.find_commit(onto)?.tree_id()?.detach();
        let mut applied = Vec::new();
        for id in missing {
            let commit = repo.find_commit(id)?;
            let theirs = commit.tree_id()?.detach();
            let base = match commit.parent_ids().next() {
                Some(parent) => repo.find_commit(parent.detach())?.tree_id()?.detach(),
                None => repo.empty_tree().id,
            };
            let mut outcome = repo
                .merge_trees(base, ours, theirs, Default::default(), options.clone())
                .map_err(|error| merge_error(Box::new(error)))?;
            let mut files: Vec<String> = outcome
                .conflicts
                .iter()
                .filter(|conflict| conflict.is_unresolved(TreatAsUnresolved::git()))
                .map(|conflict| conflict.ours.location().to_str_lossy().into_owned())
                .collect();
            if !files.is_empty() {
                files.sort();
                files.dedup();
                return Ok(PickTrial::Conflict { commit: id, files });
            }
            let tree = outcome
                .tree
                .write()
                .map_err(|error| merge_error(Box::new(error)))?
                .detach();
            if tree == ours {
                skipped += 1;
            } else {
                applied.push(id);
                ours = tree;
            }
        }
        Ok(PickTrial::Clean { applied, skipped })
    }
}
