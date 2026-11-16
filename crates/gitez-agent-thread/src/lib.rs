//! Agent threads: a prompt given to a coding agent, and the conversation
//! that follows. An Implement thread works on its own branch in its own
//! linked worktree; a Discuss thread reads the user's working tree and
//! changes nothing ([`Mode`]).
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
//!   images/2026-11-13T09-41-31Z-1.png an image sent with a turn
//! <data dir>/threads.json             the repositories with threads ([`Index`])
//! <data dir>/worktrees/<repository key>/<id>/   the threads' worktrees
//! ```
//!
//! Everything here blocks on the file system or on `git`; call it off the
//! UI thread.

mod error;
mod images;
mod index;
mod lock;
mod model;
mod names;
mod prepare;
mod store;
mod watch;
mod worktree;

pub use error::{Error, Result};
pub use images::{
    Changed, ImageError, ImageType, MAX_BYTES, MAX_EDGE, MAX_IMAGES, MAX_INPUT_BYTES,
    PreparedImage, Thumbnail, TurnImage, prepare_image, size_text, thumbnail,
};
pub use index::{Index, IndexReport, IndexedRepository};
pub use model::{
    Activity, AgentThread, AgentThreadId, BRIEF_PROMPT, CONTINUE_PROMPT, DeletedBranch, Ending,
    Failure, Mode, NewAgentThread, RetryPlan, Start, State, Step, SupervisorInfo, Turn, TurnKind,
    TurnResult, Uncommitted, WorktreeRecord,
};
pub use names::{
    clean_title, message_without_title, repository_key, slug, split_title, title_from_message,
    title_from_prompt, title_from_request,
};
pub use prepare::{CACHE_DIRS, PrepareReport, PrepareStep, clone_tree, prepare};
pub use store::{Run, RunHeader, Store};
pub use watch::{Watcher, watch};
pub use worktree::{
    BranchKept, CompleteReport, CreatedWorktree, IfUncommitted, Kept, Merged, ScratchDirs,
    check_worktree, complete, create_worktree, create_worktree_with, rebase_in_worktree,
    scratch_dirs, worktree_git_dir, worktree_path,
};

/// Times are RFC 3339 UTC, as in gitez-review.
pub use jiff::Timestamp;

/// The newest `thread.json` format this crate reads and writes.
pub const VERSION: u32 = 1;
