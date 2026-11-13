//! Agent threads: a prompt given to a coding agent that works on its own
//! branch in its own linked worktree, and the conversation that follows.
//!
//! Each repository's agent threads are stored under its common git
//! directory, in `gitez/threads/`; a global index in GitEZ's data folder
//! lists the repositories that have them. This crate is the only reader and
//! writer of those files, and owns the threads' worktrees.
