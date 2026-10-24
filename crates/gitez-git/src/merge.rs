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

/// A merge git stopped in the middle of, to have its conflicts resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeInProgress {
    /// The commit checked out when the merge started ("ours").
    pub ours: Oid,
    /// The branch checked out, unless `HEAD` is detached.
    pub branch: Option<String>,
    /// The commit being merged in ("theirs").
    pub theirs: Oid,
    /// The local branch on `theirs`, if one is.
    pub theirs_branch: Option<String>,
    /// The merge commit's message as git prepared it.
    pub message: String,
}

impl Repository {
    /// The merge in progress in this working tree, if git stopped one to
    /// have conflicts resolved.
    pub fn merge_in_progress(&self) -> Result<Option<MergeInProgress>> {
        let read = |name: &str| {
            let path = self.git_dir().join(name);
            match std::fs::read_to_string(&path) {
                Ok(text) => Ok(Some(text)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(source) => Err(Error::ReadFile { path, source }),
            }
        };
        let Some(merge_head) = read("MERGE_HEAD")? else {
            return Ok(None);
        };
        // An octopus merge lists several; GitEZ resolves the first.
        let Some(theirs) = merge_head
            .lines()
            .next()
            .and_then(|hex| Oid::from_hex(hex.trim().as_bytes()).ok())
        else {
            return Ok(None);
        };
        let refs = self.refs()?;
        let (ours, branch) = match refs.head {
            Some(crate::Head::Branch { name, target }) => (target, Some(name)),
            Some(crate::Head::Detached { target }) => (target, None),
            _ => return Ok(None),
        };
        let theirs_branch = refs
            .branches
            .iter()
            .find(|candidate| candidate.target == theirs)
            .map(|candidate| candidate.name.clone());
        Ok(Some(MergeInProgress {
            ours,
            branch,
            theirs,
            theirs_branch,
            message: read("MERGE_MSG")?.unwrap_or_default(),
        }))
    }

    /// The conflicts in the working tree's file at `path`, read from its
    /// conflict markers; `None` when it has none git wrote, such as a
    /// binary file or one deleted on one side.
    pub fn conflict_file(&self, path: &str) -> Result<Option<crate::ConflictFile>> {
        let Some(dir) = self.work_dir() else {
            return Ok(None);
        };
        let path = dir.join(path);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(std::str::from_utf8(&bytes)
                .ok()
                .and_then(crate::ConflictFile::parse)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::ReadFile { path, source }),
        }
    }
}
