//! Where GitEZ keeps its own data: recent repositories, settings, the
//! agent thread index, thread worktrees and the sandbox install.

use std::ffi::OsStr;
use std::path::PathBuf;

/// The environment variable that moves the data folder, for tests and trial
/// runs.
pub const DATA_DIR_ENV: &str = "GITEZ_DATA_DIR";

/// GitEZ's own data folder: `$GITEZ_DATA_DIR` if set and not empty, else
/// `~/Library/Application Support/GitEZ` on macOS, `$XDG_STATE_HOME/gitez`
/// (`~/.local/state/gitez`) elsewhere. `None` when there is no home folder
/// to put it in.
pub fn data_dir() -> Option<PathBuf> {
    data_dir_from(std::env::var_os(DATA_DIR_ENV).as_deref())
}

/// [`data_dir`] with `override_dir` as the value of `GITEZ_DATA_DIR`
/// (`None`: not set), so the rule can be tested without changing the
/// process environment.
pub fn data_dir_from(override_dir: Option<&OsStr>) -> Option<PathBuf> {
    match override_dir {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => platform_dir(),
    }
}

fn platform_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        dirs::data_dir().map(|dir| dir.join("GitEZ"))
    } else {
        dirs::state_dir()
            .or_else(dirs::data_dir)
            .map(|dir| dir.join("gitez"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_override_wins() {
        assert_eq!(
            data_dir_from(Some(OsStr::new("/tmp/gitez-trial"))),
            Some(PathBuf::from("/tmp/gitez-trial"))
        );
    }

    #[test]
    fn an_empty_override_is_ignored() {
        assert_eq!(data_dir_from(Some(OsStr::new(""))), platform_dir());
        assert_eq!(data_dir_from(None), platform_dir());
    }

    #[test]
    fn the_platform_folder_is_named_for_gitez() {
        // Only the name is checked; the folder itself is never touched.
        if let Some(dir) = platform_dir() {
            let name = dir.file_name().and_then(OsStr::to_str);
            let expected = if cfg!(target_os = "macos") {
                "GitEZ"
            } else {
                "gitez"
            };
            assert_eq!(name, Some(expected));
        }
    }
}
