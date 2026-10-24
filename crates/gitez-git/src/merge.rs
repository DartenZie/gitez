//! What merging one branch into another would do, worked out in memory
//! without touching the working tree, the index or the object database.

use gix::bstr::ByteSlice;
use gix::merge::tree::TreatAsUnresolved;

use crate::{Error, Oid, Repository, Result};

/// How a branch compares to the branch it would be merged into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePreview {
    /// Commits on the branch that the target lacks.
    pub ahead: usize,
    /// Commits on the target that the branch lacks: when there are none,
    /// the target can be fast-forwarded.
    pub behind: usize,
    /// The files a merge would leave with conflicts, in path order.
    pub conflicts: Vec<String>,
}

impl Repository {
    /// What merging commit `head` into commit `base` would do, as
    /// `git merge` would see it.
    pub fn merge_preview(&self, base: Oid, head: Oid) -> Result<MergePreview> {
        let (ahead, behind) = self.ahead_behind(head, base)?;
        let conflicts = if ahead == 0 || behind == 0 {
            // Nothing to merge, or a fast-forward: nothing can conflict.
            Vec::new()
        } else {
            // Merged blobs and trees stay in memory.
            let repo = self.local().with_object_memory();
            let options = repo
                .tree_merge_options()
                .map_err(|error| Error::Merge(Box::new(error)))?;
            let outcome = repo
                .merge_commits(base, head, Default::default(), options.into())
                .map_err(|error| Error::Merge(Box::new(error)))?;
            let mut conflicts: Vec<String> = outcome
                .tree_merge
                .conflicts
                .iter()
                .filter(|conflict| conflict.is_unresolved(TreatAsUnresolved::git()))
                .map(|conflict| conflict.ours.location().to_str_lossy().into_owned())
                .collect();
            conflicts.sort();
            conflicts.dedup();
            conflicts
        };
        Ok(MergePreview {
            ahead,
            behind,
            conflicts,
        })
    }
}
