//! Read-only access to git repositories for GitEZ.
//!
//! Everything in this crate reads the repository in-process through gitoxide
//! (`gix`). Mutating operations do not belong here: they run the user's own
//! `git` binary so that hooks, signing and credential helpers behave exactly as
//! they do in the terminal.
//!
//! [`Repository`] is cheap to clone and can be sent to background threads. Its
//! methods do blocking I/O, so call them off the UI thread.

mod commits;
mod conflict;
mod details;
mod diff;
mod error;
mod follow;
pub mod graph;
mod merge;
mod pick;
mod refs;
mod repository;
mod rewrite;
mod status;
mod watch;

pub use commits::{Commit, CommitWalk, Order, Signature, Timestamp};
pub use conflict::{Conflict, ConflictFile, Segment};
pub use details::{ChangeKind, CommitDetails, FileChange, LineCounts};
pub use diff::{
    BranchDiff, DiffContent, DiffLine, DiffOptions, FileDiff, Hunk, LineKind, LineSource, NewSide,
    Staging,
};
pub use error::{Error, Result};
pub use follow::{ChangedLines, Followed};
pub use merge::{MergeInProgress, MergePreview};
pub use pick::PickTrial;
pub use refs::{
    Branch, Head, RefKind, RefLabel, Refs, RemoteBranch, Stash, Tag, branch_name_problem,
};
pub use repository::Repository;
pub use rewrite::{RebaseInProgress, Rewritable, RewriteBase, RewriteCommit};
pub use status::{FileStatus, WorkingTreeStatus, WorktreeChange};
pub use watch::{Affected, Watcher};

/// A git object id (a commit hash, for the purposes of this crate).
pub use gix::ObjectId as Oid;
