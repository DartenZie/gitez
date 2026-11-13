//! Noticing changes to agent thread files, written by supervisors in other
//! processes.

use std::path::PathBuf;

use notify::Watcher as _;

use crate::Result;

/// Watches until dropped.
pub struct Watcher {
    _inner: notify::RecommendedWatcher,
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher").finish_non_exhaustive()
    }
}

/// Watches each of `dirs` recursively (missing ones are skipped), calling
/// `on_change` from a background thread with changed paths; an empty Vec
/// means events were lost (re-read everything).
pub fn watch(
    dirs: &[PathBuf],
    on_change: impl Fn(Vec<PathBuf>) + Send + 'static,
) -> Result<Watcher> {
    let mut watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(event) if event.need_rescan() => on_change(Vec::new()),
            Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => {}
            Ok(event) if event.paths.is_empty() => {}
            Ok(event) => on_change(event.paths),
            Err(_) => on_change(Vec::new()),
        })?;
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        match watcher.watch(dir, notify::RecursiveMode::Recursive) {
            Ok(()) => {}
            // Removed between the check and the watch.
            Err(error) if matches!(error.kind, notify::ErrorKind::PathNotFound) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(Watcher { _inner: watcher })
}
