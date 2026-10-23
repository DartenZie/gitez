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
mod error;
pub mod graph;
mod refs;
mod repository;

pub use commits::{Commit, CommitWalk, Order, Signature, Timestamp};
pub use error::{Error, Result};
pub use refs::{Branch, Head, RefKind, RefLabel, Refs, RemoteBranch, Stash, Tag};
pub use repository::Repository;

/// A git object id (a commit hash, for the purposes of this crate).
pub use gix::ObjectId as Oid;
