//! The ignored files a new linked worktree should get a copy of, as listed
//! in the working tree's `.worktreeinclude` (the file Claude Code reads for
//! the same purpose).

use std::path::{Path, PathBuf};

use gix::bstr::{BStr, BString, ByteSlice as _};

use crate::{Error, Repository, Result};

/// The file that lists, in gitignore syntax, which ignored files to copy.
const INCLUDE_FILE: &str = ".worktreeinclude";

impl Repository {
    /// Untracked, git-ignored files and folders of the working tree that
    /// match a pattern in its `.worktreeinclude` (gitignore syntax), as
    /// paths relative to the working tree, sorted. No file (or a bare
    /// repository): empty.
    ///
    /// A folder that matches is returned once, not file by file. An
    /// ignored folder that does not match is looked into only when a
    /// pattern names a path inside it (such as `config/local.toml` for an
    /// ignored `config/`), so a large ignored folder such as `target/` is
    /// not walked for patterns like `.env`.
    pub fn worktree_include_files(&self) -> Result<Vec<PathBuf>> {
        let repo = self.local();
        let Some(work_dir) = repo.workdir().map(Path::to_owned) else {
            return Ok(Vec::new());
        };
        let include_path = work_dir.join(INCLUDE_FILE);
        let bytes = match std::fs::read(&include_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(Error::ReadFile {
                    path: include_path,
                    source,
                });
            }
        };
        let patterns = Patterns::parse(&bytes, &include_path)?;
        if patterns.is_empty() {
            return Ok(Vec::new());
        }

        let options = repo
            .dirwalk_options()?
            .emit_ignored(Some(gix::dir::walk::EmissionMode::Matching))
            .emit_untracked(gix::dir::walk::EmissionMode::CollapseDirectory)
            .emit_tracked(false)
            .emit_pruned(false);
        let index = repo.index_or_empty()?;
        let mut found = Vec::new();
        let walk = repo.dirwalk_iter(index, None::<BString>, Default::default(), options)?;
        for item in walk {
            let entry = item?.entry;
            if !matches!(entry.status, gix::dir::entry::Status::Ignored(_)) {
                continue;
            }
            let is_dir = matches!(
                entry.disk_kind,
                Some(gix::dir::entry::Kind::Directory | gix::dir::entry::Kind::Repository)
            );
            let relative = entry.rela_path;
            if patterns.includes(relative.as_ref(), is_dir) {
                found.push(relative);
            } else if is_dir && patterns.may_match_inside(relative.as_ref()) {
                walk_ignored_dir(&work_dir, relative, &patterns, &mut found)?;
            }
        }

        let mut paths: Vec<PathBuf> = found
            .into_iter()
            .filter_map(|path| gix::path::from_bstring(path).ok())
            .collect();
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

/// The patterns of a `.worktreeinclude` file.
struct Patterns {
    search: gix::ignore::Search,
}

impl Patterns {
    fn parse(bytes: &[u8], source: &Path) -> Result<Self> {
        let mut search = gix::ignore::Search::default();
        search
            .add_patterns_buffer(bytes, source, None, gix::ignore::search::Ignore::default())
            .map_err(|source_error| Error::ReadFile {
                path: source.to_owned(),
                source: source_error,
            })?;
        Ok(Self { search })
    }

    fn patterns(&self) -> impl Iterator<Item = &gix::glob::Pattern> {
        self.search
            .patterns
            .iter()
            .flat_map(|list| list.patterns.iter().map(|mapping| &mapping.pattern))
    }

    fn is_empty(&self) -> bool {
        self.patterns().next().is_none()
    }

    /// Whether `path` matches as gitignore would: the last matching pattern
    /// decides, and a matching parent folder includes everything in it.
    fn includes(&self, path: &BStr, is_dir: bool) -> bool {
        let mut start = 0;
        while let Some(slash) = path[start..].find_byte(b'/') {
            let parent = &path[..start + slash];
            if self.matches(parent, true) {
                return true;
            }
            start += slash + 1;
        }
        self.matches(path, is_dir)
    }

    fn matches(&self, path: &BStr, is_dir: bool) -> bool {
        self.search
            .pattern_matching_relative_path(path, Some(is_dir), gix::glob::pattern::Case::Sensitive)
            .is_some_and(|found| !found.pattern.is_negative())
    }

    /// Whether a pattern could match something inside the folder `dir`:
    /// one with a slash whose literal start leads into it.
    fn may_match_inside(&self, dir: &BStr) -> bool {
        let mut dir = BString::from(dir);
        dir.push(b'/');
        self.patterns().any(|pattern| {
            if pattern.is_negative() || pattern.mode.contains(gix::glob::pattern::Mode::NO_SUB_DIR)
            {
                return false;
            }
            let text = pattern.text.as_bstr();
            let literal = &text[..pattern.first_wildcard_pos.unwrap_or(text.len())];
            let literal_dirs = &literal[..literal.rfind_byte(b'/').map_or(0, |slash| slash + 1)];
            literal.starts_with(dir.as_slice()) || dir.starts_with(literal_dirs)
        })
    }
}

/// Adds the entries inside the ignored folder `relative` (relative to
/// `work_dir`) that `patterns` include; a matching folder is added whole.
fn walk_ignored_dir(
    work_dir: &Path,
    relative: BString,
    patterns: &Patterns,
    found: &mut Vec<BString>,
) -> Result<()> {
    let mut pending = vec![relative];
    while let Some(dir) = pending.pop() {
        let Ok(dir_path) = gix::path::from_bstr(dir.as_bstr()) else {
            continue;
        };
        let full = work_dir.join(dir_path);
        let entries = std::fs::read_dir(&full).map_err(|source| Error::ReadFile {
            path: full.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| Error::ReadFile {
                path: full.clone(),
                source,
            })?;
            let Ok(name) = gix::path::os_str_into_bstr(&entry.file_name()).map(BString::from)
            else {
                continue;
            };
            let mut path = dir.clone();
            path.push(b'/');
            path.extend_from_slice(&name);
            // A symlink is copied as a link, never followed.
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if patterns.includes(path.as_ref(), is_dir) {
                found.push(path);
            } else if is_dir && patterns.may_match_inside(path.as_ref()) {
                pending.push(path);
            }
        }
    }
    Ok(())
}
