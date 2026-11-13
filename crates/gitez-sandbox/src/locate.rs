//! Finding the programs a sandbox needs. An app started from the Finder gets
//! a short `PATH`, so the usual install places are searched too.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where to look for programs.
#[derive(Debug, Clone)]
pub(crate) struct Search {
    /// The `PATH` to search; `None`: this process's.
    path: Option<OsString>,
    /// Also look where installers put programs (Homebrew, nvm, Volta, …).
    usual_places: bool,
    home: Option<PathBuf>,
}

impl Search {
    /// This process's `PATH` and the usual install places.
    pub(crate) fn system() -> Self {
        Self {
            path: None,
            usual_places: true,
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }

    /// Only `path`, for tests.
    #[cfg(test)]
    pub(crate) fn only(path: impl Into<OsString>) -> Self {
        Self {
            path: Some(path.into()),
            usual_places: false,
            home: None,
        }
    }

    /// Only `path` and the usual install places, for tests.
    #[cfg(test)]
    pub(crate) fn with_usual_places(path: impl Into<OsString>) -> Self {
        Self {
            path: Some(path.into()),
            ..Self::system()
        }
    }

    pub(crate) fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// `name` on the `PATH` only.
    pub(crate) fn on_path(&self, name: &str) -> Option<PathBuf> {
        let path = match &self.path {
            Some(path) => Some(path.clone()),
            None => std::env::var_os("PATH"),
        }?;
        std::env::split_paths(&path)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(name))
            .find(|candidate| is_program(candidate))
    }

    /// `name` on the `PATH`, else in the usual install places.
    pub(crate) fn find(&self, name: &str) -> Option<PathBuf> {
        self.on_path(name).or_else(|| {
            self.usual_dirs()
                .into_iter()
                .map(|dir| dir.join(name))
                .find(|candidate| is_program(candidate))
        })
    }

    fn usual_dirs(&self) -> Vec<PathBuf> {
        if !self.usual_places {
            return Vec::new();
        }
        let mut dirs: Vec<PathBuf> = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"]
            .map(PathBuf::from)
            .to_vec();
        if let Some(home) = &self.home {
            dirs.extend(
                [
                    ".volta/bin",
                    ".local/bin",
                    ".asdf/shims",
                    ".local/share/mise/shims",
                    ".nodenv/shims",
                ]
                .map(|dir| home.join(dir)),
            );
            dirs.extend(newest_nvm_node(home));
        }
        dirs
    }
}

/// The `bin` folder of the newest Node.js that nvm installed.
fn newest_nvm_node(home: &Path) -> Option<PathBuf> {
    let versions = std::fs::read_dir(home.join(".nvm/versions/node")).ok()?;
    versions
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            Some((parse_version(&name)?, entry.path().join("bin")))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, bin)| bin)
}

/// Whether `path` is a file anyone may run.
pub(crate) fn is_program(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// A version such as `v22.12.0` or `0.0.82`, as numbers.
pub(crate) fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let text = text.trim().trim_start_matches('v');
    let mut parts = text
        .split(['.', '-', '+'])
        .map(|part| part.parse::<u32>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// What `node --version` says, such as `v26.10.0`.
pub(crate) fn node_version(node: &Path) -> Option<String> {
    let output = Command::new(node)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse() {
        assert_eq!(parse_version("v22.12.0\n"), Some((22, 12, 0)));
        assert_eq!(parse_version("0.0.82"), Some((0, 0, 82)));
        assert_eq!(parse_version("v23.0.0-nightly"), Some((23, 0, 0)));
        assert_eq!(parse_version("node"), None);
    }

    #[test]
    fn an_empty_path_finds_nothing() {
        assert_eq!(Search::only("").find("sh"), None);
        assert!(Search::only("/bin").find("sh").is_some());
    }
}
