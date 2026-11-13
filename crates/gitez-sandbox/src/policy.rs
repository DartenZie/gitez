//! What a sandbox allows: where the wrapped command may write, what it may
//! not read, and which hosts it may reach.
//!
//! A path that contains `*` is a pattern, matched the way `srt` matches
//! them: `*` stands for any part of one name, `**` for any number of
//! folders (`/private/tmp/claude-*-cwd`, `/private/tmp/claude-501/probe-**`).
//! Patterns work on macOS only; on Linux they are left out.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WritePath {
    /// A folder and everything in it.
    Tree(PathBuf),
    /// Exactly this file (it may not exist yet).
    File(PathBuf),
}

impl WritePath {
    pub fn path(&self) -> &Path {
        match self {
            Self::Tree(path) | Self::File(path) => path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Network {
    /// Only these hosts (`*.example.com` allowed). Empty: no network.
    Hosts(Vec<String>),
    Open,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Everything else is read-only.
    pub write: Vec<WritePath>,
    /// Never writable, even inside a `write` tree.
    pub deny_write: Vec<PathBuf>,
    /// Not readable (hidden).
    pub deny_read: Vec<PathBuf>,
    pub network: Network,
    /// Whether programs may ask macOS's certificate trust service
    /// (`com.apple.trustd.agent`) to verify TLS certificates, as Python's
    /// pip (through `truststore`), Go programs and others do; without it
    /// they cannot reach any HTTPS host. trustd fetches revocation and
    /// issuer data itself, outside the sandbox, which is a way past the
    /// host list: allow it only where the host list already lets data
    /// out. srt calls it `enableWeakerNetworkIsolation`. The Open profile
    /// always allows it; Linux has no such service.
    pub certificate_service: bool,
}

/// Whether `path` is a pattern rather than a name.
pub(crate) fn is_pattern(path: &Path) -> bool {
    path.to_string_lossy().contains('*')
}

/// The anchored regular expression a pattern stands for, as `srt` builds
/// it: `**/` is any number of folders, `**` anything, `*` any part of one
/// name. Every other character stands for itself.
pub(crate) fn pattern_regex(pattern: &str) -> String {
    let mut regex = String::from("^");
    let mut rest = pattern;
    while let Some(c) = rest.chars().next() {
        if let Some(after) = rest.strip_prefix("**/") {
            regex.push_str("(.*/)?");
            rest = after;
        } else if let Some(after) = rest.strip_prefix("**") {
            regex.push_str(".*");
            rest = after.trim_start_matches('*');
        } else if let Some(after) = rest.strip_prefix('*') {
            regex.push_str("[^/]*");
            rest = after;
        } else {
            regex.push_str(&escape_regex(&c.to_string()));
            rest = &rest[c.len_utf8()..];
        }
    }
    regex.push('$');
    regex
}

/// `text` as a regular expression that matches it and nothing else.
pub(crate) fn escape_regex(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if ".*+?^${}()|[]\\".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// The folder a pattern starts from: everything before the first name that
/// holds a `*`.
pub(crate) fn pattern_base(pattern: &Path) -> PathBuf {
    pattern
        .components()
        .take_while(|component| !component.as_os_str().to_string_lossy().contains('*'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_compile_as_srt_compiles_them() {
        assert_eq!(
            pattern_regex("/private/tmp/claude-*-cwd"),
            "^/private/tmp/claude-[^/]*-cwd$"
        );
        assert_eq!(
            pattern_regex("/private/tmp/claude-501/fswatch-probe-**"),
            "^/private/tmp/claude-501/fswatch-probe-.*$"
        );
        assert_eq!(pattern_regex("/a/**/b.json"), "^/a/(.*/)?b\\.json$");
        assert_eq!(pattern_regex("/x(1)+[2]"), "^/x\\(1\\)\\+\\[2\\]$");
    }

    #[test]
    fn a_pattern_starts_from_its_last_plain_folder() {
        assert!(is_pattern(Path::new("/tmp/claude-*-cwd")));
        assert!(!is_pattern(Path::new("/tmp/[WIP] project")));
        assert_eq!(
            pattern_base(Path::new("/private/tmp/claude-*-cwd")),
            Path::new("/private/tmp")
        );
        assert_eq!(pattern_base(Path::new("/a/b/**/c")), Path::new("/a/b"));
    }

    #[test]
    fn write_path_names_its_path() {
        assert_eq!(WritePath::Tree("/a".into()).path(), Path::new("/a"));
        assert_eq!(WritePath::File("/b".into()).path(), Path::new("/b"));
    }
}
