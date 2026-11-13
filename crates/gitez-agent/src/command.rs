//! A process to run, described without starting it.

/// A process to run: what an agent, a setup command or a git command needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: std::path::PathBuf,
    pub args: Vec<String>,
    /// Set on top of the inherited environment, in order.
    pub env: Vec<(String, String)>,
    /// Removed from the inherited environment before `env` is applied.
    pub env_remove: Vec<String>,
    pub cwd: std::path::PathBuf,
    /// Written to standard input, which is then closed. `None`: stdin is
    /// /dev/null.
    pub stdin: Option<String>,
}
