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
    /// Files not copied because a folder on their way in the worktree is a
    /// link or a file: copying through it would write outside the worktree.
    pub skipped: Vec<PathBuf>,
}

/// Copies the files `.worktreeinclude` lists (and git ignores) from the
/// main worktree, keeping their relative paths and permissions, then clones
/// [`CACHE_DIRS`] where the filesystem supports it (macOS: one `clonefile`
/// of the whole tree; Linux: the tree walked, each file reflinked, symlinks
/// recreated; on the first failure the partial copy is removed and the
/// folder skipped silently). Files that already exist in the worktree are
/// left as they are. The setup command is not run here (the supervisor
/// runs it in the sandbox).
///
/// The worktree may already hold what the agent or the setup command left
/// (a retry after a failed setup, a branch with the agent's commits), so
/// nothing is written through a link: every folder on the way to a copied
/// file must be a real folder in the worktree (missing ones are created),
/// and an existing file or link is never written over.
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
    if !fs::symlink_metadata(worktree).is_ok_and(|meta| meta.is_dir()) {
        return Err(Error::refused(format!(
            "{} is not a folder.",
            worktree.display()
        )));
    }
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
        match copy_include(&main_dir, worktree, relative)? {
            Copied::Yes => done.copied.push(relative.clone()),
            Copied::No => {}
            Copied::Unsafe => done.skipped.push(relative.clone()),
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
        if includes.contains(&relative) {
            match copy_include(&main_dir, worktree, &relative)? {
                Copied::Yes => done.copied.push(relative),
                Copied::No => {}
                Copied::Unsafe => done.skipped.push(relative),
            }
        }
    }
    Ok(done)
}

/// Whether [`copy_include`] copied.
enum Copied {
    Yes,
    /// It is already there, or gone from the main worktree.
    No,
    /// A folder on its way is a link or a file.
    Unsafe,
}

/// Copies `relative` from `main_dir` into `worktree` unless it exists
/// there, through real folders only.
fn copy_include(main_dir: &Path, worktree: &Path, relative: &Path) -> Result<Copied> {
    let Some(parent) = real_parent(worktree, relative)? else {
        return Ok(Copied::Unsafe);
    };
    let Some(name) = relative.file_name() else {
        return Ok(Copied::Unsafe);
    };
    let source = main_dir.join(relative);
    let target = parent.join(name);
    if fs::symlink_metadata(&target).is_ok() {
        return Ok(Copied::No);
    }
    let Ok(meta) = fs::symlink_metadata(&source) else {
        // Gone since it was listed.
        return Ok(Copied::No);
    };
    copy_entry(&source, &target, &meta)?;
    Ok(Copied::Yes)
}

/// The folder in `worktree` that `relative` goes in, every folder on the
/// way a real one (missing ones created); `None` when one is a link or a
/// file, or `relative` leaves the worktree (`..`, an absolute path).
fn real_parent(worktree: &Path, relative: &Path) -> Result<Option<PathBuf>> {
    let mut dir = worktree.to_owned();
    let Some(parent) = relative.parent() else {
        return Ok(None);
    };
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Ok(None);
    }
    for component in parent.components() {
        dir.push(component);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&dir).map_err(Error::io(&dir))?;
            }
            Err(error) => return Err(Error::io(&dir)(error)),
        }
    }
    Ok(Some(dir))
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

/// Clones the folder `source` to `target` (which must not exist)
/// copy-on-write: one `clonefile` of the whole tree. Fails where the
/// filesystem cannot clone (another volume, no APFS); the caller removes
/// what a failed clone left.
#[cfg(target_os = "macos")]
pub fn clone_tree(source: &Path, target: &Path) -> std::io::Result<()> {
    reflink_copy::reflink(source, target)
}

/// Clones the folder `source` to `target` (which must not exist)
/// copy-on-write, file by file; fails on the first file the filesystem
/// cannot clone. The caller removes what a failed clone left.
#[cfg(not(target_os = "macos"))]
pub fn clone_tree(source: &Path, target: &Path) -> std::io::Result<()> {
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
