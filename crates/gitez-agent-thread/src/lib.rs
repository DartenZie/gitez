//! Agent threads: a prompt given to a coding agent that works on its own
//! branch in its own linked worktree, and the conversation that follows.
//!
//! Each repository's agent threads are stored under its common git
//! directory, in `gitez/threads/`; a global index in GitEZ's data folder
//! lists the repositories that have them. This crate is the only reader and
//! writer of those files, and owns the threads' worktrees.
//!
//! ```text
//! <git common dir>/gitez/threads/<id>/
//!   thread.json                       the agent thread (see [`AgentThread`])
//!   thread.json.lock                  only while it is being saved
//!   runs/2026-11-13T09-41-31Z.jsonl   one transcript per turn
//! <data dir>/threads.json             the repositories with threads ([`Index`])
//! <data dir>/worktrees/<repository key>/<id>/   the threads' worktrees
//! ```
//!
//! Everything here blocks on the file system or on `git`; call it off the
//! UI thread.

mod error;
mod index;
mod lock;
mod model;
mod names;
mod prepare;
mod store;
mod watch;
mod worktree;

pub use error::{Error, Result};
pub use index::{Index, IndexedRepository};
pub use model::{
    Activity, AgentThread, AgentThreadId, CONTINUE_PROMPT, Ending, Failure, NewAgentThread,
    RetryPlan, Start, State, Step, SupervisorInfo, Turn, TurnKind, TurnResult,
};
pub use names::{repository_key, slug, title_from_prompt};
pub use prepare::{CACHE_DIRS, PrepareReport, PrepareStep, prepare};
pub use store::{Run, RunHeader, Store};
pub use watch::{Watcher, watch};
pub use worktree::{CompleteReport, complete, create_worktree, worktree_git_dir, worktree_path};

/// Times are RFC 3339 UTC, as in gitez-review.
pub use jiff::Timestamp;

/// The newest `thread.json` format this crate reads and writes.
pub const VERSION: u32 = 1;
