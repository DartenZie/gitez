//! Local pull requests: a branch reviewed like a pull request without
//! leaving the machine. Each one has threads of comments on lines of its
//! diff, which become a prompt for a coding agent, and transcripts of the
//! agent's runs.
//!
//! Everything is stored under the repository's common git directory, in
//! `gitez/reviews/`; see [`Store`] and CLAUDE.md ("Review storage") for the
//! layout. This crate is the only reader and writer of those files.

mod error;
mod follow;
mod model;
mod prompt;
mod store;

pub use error::{Error, Result};
pub use follow::{FileUpdate, HeadMoved, LineChange};
pub use model::{
    Anchor, Comment, PromptOptions, PullRequest, Resolution, ResolvedBy, Side, State, Template,
    Thread, Tip,
};
pub use prompt::PromptContext;
pub use store::{NewPullRequest, Run, RunHeader, Store};

/// Times are stored as RFC 3339 UTC timestamps.
pub use jiff::Timestamp;

/// The newest version of the `review.json` format this crate reads and
/// writes.
pub const VERSION: u32 = 1;
