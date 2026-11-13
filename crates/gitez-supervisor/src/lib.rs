//! The supervisor: one process per agent thread that waits in the global
//! queue, prepares the thread's worktree, runs the agent inside the sandbox,
//! writes its transcript, and commits and opens a local pull request when
//! the agent is done. It outlives the GUI, which launches it, watches its
//! files and can stop it.
