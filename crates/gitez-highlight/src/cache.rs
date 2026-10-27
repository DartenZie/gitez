//! Highlighted files, kept so each version of a file is highlighted once.

use std::{
    collections::{HashMap, VecDeque},
    hash::{DefaultHasher, Hash, Hasher},
    sync::{Arc, LazyLock, Mutex},
};

use crate::{Highlights, Language};

/// How many bytes of highlights the cache keeps before it forgets the
/// files it highlighted first.
const MAX_BYTES: usize = 32 * 1024 * 1024;

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Mutex::default);

/// A file's contents by their hash, and the name of the grammar they were
/// highlighted with. The hasher is only stable within a process, which is
/// as long as the cache lives.
type Key = (u64, &'static str);

#[derive(Default)]
struct Cache {
    entries: HashMap<Key, Arc<Highlights>>,
    /// Keys in the order they were added, oldest first.
    order: VecDeque<Key>,
    /// The bytes the entries hold.
    bytes: usize,
}

/// The highlights of `text` in `language`, from the cache or from
/// `highlight`, which runs without the cache locked.
pub(crate) fn get_or_insert(
    text: &str,
    language: Language,
    highlight: impl FnOnce() -> Highlights,
) -> Arc<Highlights> {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    let key = (hasher.finish(), language.name());

    if let Some(highlights) = lock().entries.get(&key) {
        return highlights.clone();
    }
    let highlights = Arc::new(highlight());

    let mut cache = lock();
    // Another thread may have highlighted the same file meanwhile; keep the
    // first so every caller shares one copy.
    if let Some(existing) = cache.entries.get(&key) {
        return existing.clone();
    }
    cache.bytes += highlights.footprint();
    cache.entries.insert(key, highlights.clone());
    cache.order.push_back(key);
    while cache.bytes > MAX_BYTES && cache.order.len() > 1 {
        let Some(oldest) = cache.order.pop_front() else {
            break;
        };
        if let Some(evicted) = cache.entries.remove(&oldest) {
            cache.bytes -= evicted.footprint();
        }
    }
    highlights
}

/// The cache, even if a thread panicked holding it: entries are only ever
/// added or removed whole, so it is never left half-changed.
fn lock() -> std::sync::MutexGuard<'static, Cache> {
    CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
