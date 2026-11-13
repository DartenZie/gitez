//! The repositories opened lately, for File > Open Recent, and the one open
//! when GitEZ last quit, to open it again on the next start.
//!
//! Kept in `recent.json` in GitEZ's own data folder:
//! `~/Library/Application Support/GitEZ` on macOS,
//! `$XDG_STATE_HOME/gitez` (`~/.local/state/gitez`) on Linux, or
//! `$GITEZ_DATA_DIR` when set (see `gitez_settings::data_dir`).

use std::path::{Path, PathBuf};

use gpui_kit::{App, Global, SharedString, Task};
use serde::{Deserialize, Serialize};

/// How many repositories Open Recent lists.
const LIMIT: usize = 10;

/// The format of `recent.json`. A newer one is ignored and never
/// overwritten; one that cannot be read is replaced.
const VERSION: u32 = 1;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct File {
    version: u32,
    /// The repository open when GitEZ quit, unless it was closed first.
    last: Option<PathBuf>,
    /// Most recently opened first.
    recent: Vec<PathBuf>,
}

/// The recent repositories, as a global of the app.
pub struct Recent {
    /// Where the list is saved; `None` if there is no data folder, or the
    /// file there is of a newer GitEZ.
    file: Option<PathBuf>,
    data: File,
    /// The save in flight; the next one waits for it.
    saving: Option<Task<()>>,
    /// The listed repositories whose folder was gone when [`check`] last
    /// looked: moved or deleted.
    missing: Vec<PathBuf>,
    /// The check in flight, if any.
    checking: Option<Task<()>>,
}

impl Global for Recent {}

/// Reads the list. Called once at start, before the window opens: the file
/// is tiny, and it decides which repository to open.
pub fn init(cx: &mut App) {
    let file = gitez_settings::data_dir().map(|dir| dir.join("recent.json"));
    let (file, data) = match file.as_deref().map(read) {
        Some(Ok(Some(data))) => (file, data),
        // Written by a newer GitEZ: leave it alone.
        Some(Ok(None)) => (None, File::default()),
        Some(Err(error)) => {
            eprintln!("gitez: starting a new list of recent repositories: {error:#}");
            (file, File::default())
        }
        None => (None, File::default()),
    };
    cx.set_global(Recent {
        file,
        data,
        saving: None,
        missing: Vec::new(),
        checking: None,
    });
    check(cx);
}

/// An empty list that is never saved, for tests that open a window
/// without touching the user's data folder.
#[cfg(test)]
pub fn init_in_memory(cx: &mut App) {
    cx.set_global(Recent {
        file: None,
        data: File::default(),
        saving: None,
        missing: Vec::new(),
        checking: None,
    });
}

/// The repository to open at start, if the last one is still there.
pub fn last(cx: &App) -> Option<PathBuf> {
    cx.global::<Recent>()
        .data
        .last
        .clone()
        .filter(|path| path.is_dir())
}

/// The recent repositories, most recently opened first.
pub fn paths(cx: &App) -> &[PathBuf] {
    &cx.global::<Recent>().data.recent
}

/// Looks again, in the background, which recent repositories' folders are
/// gone; [`unavailable`] says so once it knows. Observers of [`Recent`]
/// are told when the answer changes.
pub fn check(cx: &mut App) {
    let paths = paths(cx).to_vec();
    let checked = cx
        .background_executor()
        .spawn(async move { paths.into_iter().filter(|path| !path.is_dir()).collect() });
    let task = cx.spawn(async move |cx| {
        let missing: Vec<PathBuf> = checked.await;
        cx.update(|cx| {
            // Borrowing it mutably tells its observers, so only on a change.
            if cx.global::<Recent>().missing != missing {
                cx.global_mut::<Recent>().missing = missing;
            }
        });
    });
    cx.global_mut::<Recent>().checking = Some(task);
}

/// Text shown for a repository that was moved or deleted.
const MOVED_OR_DELETED: &str = "Moved or deleted";

/// Why the repository whose main worktree is `work_dir` cannot be opened
/// or picked, if it cannot: its folder is gone, as last checked in the
/// background, by [`check`] for a recent repository and by the list of
/// repositories with threads (`threads`) for the others. Never touches the
/// disk, so views call it while drawing. Home's recent repositories and
/// its launcher's Repository picker both use it.
pub fn unavailable(
    work_dir: &Path,
    threads: &crate::agent_threads::AgentThreads,
    cx: &App,
) -> Option<SharedString> {
    let gone = cx
        .global::<Recent>()
        .missing
        .iter()
        .any(|path| path == work_dir)
        || threads
            .missing_repositories()
            .iter()
            .any(|repository| repository.work_dir == work_dir);
    gone.then(|| SharedString::from(MOVED_OR_DELETED))
}

/// Records that the repository at `path` was opened.
pub fn opened(path: &Path, cx: &mut App) {
    cx.add_recent_document(path);
    // Opened, so it is there.
    cx.global_mut::<Recent>()
        .missing
        .retain(|missing| missing != path);
    update(cx, |data| {
        data.recent.retain(|recent| recent != path);
        data.recent.insert(0, path.to_owned());
        data.recent.truncate(LIMIT);
        data.last = Some(path.to_owned());
    });
}

/// Records that the open repository was closed, so the next start opens
/// none.
pub fn closed(cx: &mut App) {
    update(cx, |data| data.last = None);
}

/// Takes `path` off the list. Its folder stays where it is.
pub fn remove(path: &Path, cx: &mut App) {
    update(cx, |data| data.recent.retain(|recent| recent != path));
}

/// Empties Open Recent.
pub fn clear(cx: &mut App) {
    update(cx, |data| data.recent.clear());
}

/// Changes the list, saves it in the background and updates the menus.
fn update(cx: &mut App, change: impl FnOnce(&mut File)) {
    let recent = cx.global_mut::<Recent>();
    let before = recent.data.clone();
    change(&mut recent.data);
    if recent.data == before {
        return;
    }
    if let Some(file) = recent.file.clone() {
        let data = recent.data.clone();
        let previous = recent.saving.take();
        let save = cx.background_executor().spawn(async move {
            if let Some(previous) = previous {
                previous.await;
            }
            if let Err(error) = write(&file, &data) {
                eprintln!("gitez: could not save the recent repositories: {error:#}");
            }
        });
        cx.global_mut::<Recent>().saving = Some(save);
    }
    crate::menus::set(cx);
}

/// Reads the list at `path`: empty if there is none, `None` if a newer
/// GitEZ wrote it.
fn read(path: &Path) -> anyhow::Result<Option<File>> {
    use anyhow::Context as _;
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(File {
                version: VERSION,
                ..File::default()
            }));
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let data: File =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok((data.version <= VERSION).then_some(data))
}

/// Writes `data` to a temporary file next to `path`, then renames it over
/// `path`, so a crash never leaves half a file.
fn write(path: &Path, data: &File) -> anyhow::Result<()> {
    let dir = path.parent().expect("the file is inside the data folder");
    std::fs::create_dir_all(dir)?;
    let data = File {
        version: VERSION,
        ..data.clone()
    };
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(&data)?)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// `path` for a menu: the home folder as `~`.
pub fn display(path: &Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_owned)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_reads_back() {
        let dir = tempfile::tempdir().expect("temporary folder");
        let path = dir.path().join("nested").join("recent.json");
        let empty = read(&path)
            .expect("missing file reads")
            .expect("as current");
        assert!(empty.recent.is_empty());
        let data = File {
            version: VERSION,
            last: Some("/a".into()),
            recent: vec!["/a".into(), "/b".into()],
        };
        write(&path, &data).expect("write");
        assert_eq!(read(&path).expect("read"), Some(data));
    }

    #[test]
    fn leaves_a_newer_version_alone() {
        let dir = tempfile::tempdir().expect("temporary folder");
        let path = dir.path().join("recent.json");
        std::fs::write(&path, r#"{"version":99,"last":null,"recent":[]}"#).expect("write");
        assert_eq!(read(&path).expect("read"), None);
    }
}
