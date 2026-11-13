//! Preparing a new worktree so the agent can build and run the project at
//! once: the ignored files `.worktreeinclude` lists are copied from the
//! main worktree, and cache folders are cloned copy-on-write.
//!
//! Cloning `target/` was measured on 2026-11-13 (APFS, a project with
//! serde, serde_json, regex and clap; 29 crates): `cargo build` in a fresh
//! linked worktree took 3.3 s and compiled 29 crates without the clone,
//! 0.55 s and only the project itself with it. Cargo keeps third-party
//! crates fresh in a cloned `target/`, so the clone stays.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Cache folders cloned copy-on-write from the main worktree when present
/// there and absent in the new worktree.
pub const CACHE_DIRS: [&str; 2] = ["target", "node_modules"];

/// What [`prepare`] is doing, for the thread's activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareStep {
    CopyingIncludes { files: usize },
    Cloning { dir: String },
}

/// What [`prepare`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrepareReport {
    /// Paths relative to the worktree, as `.worktreeinclude` matched them
    /// (a folder once).
    pub copied: Vec<PathBuf>,
    pub cloned: Vec<String>,
    /// Cache folders not cloned because the filesystem cannot.
    pub not_cloned: Vec<String>,
}

/// Copies the files `.worktreeinclude` lists (and git ignores) from the
/// main worktree, keeping their relative paths and permissions, then clones
/// [`CACHE_DIRS`] where the filesystem supports it (macOS: one `clonefile`
/// of the whole tree; Linux: the tree walked, each file reflinked, symlinks
/// recreated; on the first failure the partial copy is removed and the
/// folder skipped silently). Files that already exist in the worktree are
/// left as they are. The setup command is not run here (the supervisor
/// runs it in the sandbox).
pub fn prepare(
    main: &gitez_git::Repository,
    worktree: &Path,
    mut report: impl FnMut(PrepareStep),
) -> Result<PrepareReport> {
    let Some(main_dir) = main.work_dir().map(Path::to_owned) else {
        return Err(Error::refused(
            "The repository has no working tree to prepare a worktree from.",
        ));
    };
    let mut done = PrepareReport::default();

    let includes = main.worktree_include_files()?;
    // A cache folder listed in `.worktreeinclude` is cloned below instead,
    // which is faster; it is copied only if the clone fails.
    let is_cache = |path: &Path| CACHE_DIRS.iter().any(|dir| path == Path::new(dir));
    let to_copy: Vec<&PathBuf> = includes
        .iter()
        .filter(|path| !is_cache(path) || !main_dir.join(path).is_dir())
        .collect();
    if !to_copy.is_empty() {
        report(PrepareStep::CopyingIncludes {
            files: to_copy.len(),
        });
    }
    for relative in to_copy {
        if copy_include(&main_dir, worktree, relative)? {
            done.copied.push(relative.clone());
        }
    }

    for dir in CACHE_DIRS {
        let source = main_dir.join(dir);
        let target = worktree.join(dir);
        let is_real_dir = fs::symlink_metadata(&source).is_ok_and(|meta| meta.is_dir());
        if !is_real_dir || fs::symlink_metadata(&target).is_ok() {
            continue;
        }
        report(PrepareStep::Cloning {
            dir: dir.to_owned(),
        });
        if clone_tree(&source, &target).is_ok() {
            done.cloned.push(dir.to_owned());
            continue;
        }
        // Leave nothing half cloned.
        let _ = fs::remove_dir_all(&target);
        done.not_cloned.push(dir.to_owned());
        let relative = PathBuf::from(dir);
        if includes.contains(&relative) && copy_include(&main_dir, worktree, &relative)? {
            done.copied.push(relative);
        }
    }
    Ok(done)
}

/// Copies `relative` from `main_dir` into `worktree` unless it exists
/// there. Returns whether it copied.
fn copy_include(main_dir: &Path, worktree: &Path, relative: &Path) -> Result<bool> {
    let source = main_dir.join(relative);
    let target = worktree.join(relative);
    if fs::symlink_metadata(&target).is_ok() {
        return Ok(false);
    }
    let Ok(meta) = fs::symlink_metadata(&source) else {
        // Gone since it was listed.
        return Ok(false);
    };
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(Error::io(parent))?;
    }
    copy_entry(&source, &target, &meta)?;
    Ok(true)
}

/// Copies a file, symlink or folder tree, keeping permissions.
fn copy_entry(source: &Path, target: &Path, meta: &fs::Metadata) -> Result<()> {
    let kind = meta.file_type();
    if kind.is_symlink() {
        let link = fs::read_link(source).map_err(Error::io(source))?;
        symlink(&link, target).map_err(Error::io(target))
    } else if kind.is_dir() {
        fs::create_dir(target).map_err(Error::io(target))?;
        for entry in fs::read_dir(source).map_err(Error::io(source))? {
            let entry = entry.map_err(Error::io(source))?;
            let meta = fs::symlink_metadata(entry.path()).map_err(Error::io(entry.path()))?;
            copy_entry(&entry.path(), &target.join(entry.file_name()), &meta)?;
        }
        fs::set_permissions(target, meta.permissions()).map_err(Error::io(target))
    } else if kind.is_file() {
        // `fs::copy` keeps the permissions (and clones on APFS).
        fs::copy(source, target).map_err(Error::io(target))?;
        Ok(())
    } else {
        // Sockets, pipes and devices are not copied.
        Ok(())
    }
}

#[cfg(unix)]
fn symlink(link: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(link, target)
}

#[cfg(not(unix))]
fn symlink(_link: &Path, _target: &Path) -> std::io::Result<()> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

/// Clones the folder `source` to `target` copy-on-write: one `clonefile`
/// of the whole tree.
#[cfg(target_os = "macos")]
fn clone_tree(source: &Path, target: &Path) -> std::io::Result<()> {
    reflink_copy::reflink(source, target)
}

/// Clones the folder `source` to `target` copy-on-write, file by file;
/// fails on the first file the filesystem cannot clone.
#[cfg(not(target_os = "macos"))]
fn clone_tree(source: &Path, target: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(source)?;
    fs::create_dir(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            symlink(&fs::read_link(&from)?, &to)?;
        } else if kind.is_dir() {
            clone_tree(&from, &to)?;
        } else if kind.is_file() {
            reflink_copy::reflink(&from, &to)?;
            fs::set_permissions(&to, fs::metadata(&from)?.permissions())?;
        }
    }
    fs::set_permissions(target, meta.permissions())
}
