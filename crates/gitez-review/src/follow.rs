//! Keeping threads on the lines they point at when the head branch moves:
//! a thread whose lines a new commit changed resolves itself, the others
//! follow their lines to the new commit.

use std::collections::HashMap;
use std::ops::Range;

use jiff::Timestamp;

use crate::model::{PullRequest, Resolution, ResolvedBy, Side, Thread};

/// What became of a file between the old and the new head commit, as read
/// from the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileUpdate {
    /// Its content is the same.
    Unchanged,
    /// Its content changed, and it may have been renamed to `path`.
    Changed {
        path: String,
        lines: Vec<LineChange>,
    },
    /// It was deleted, or its lines cannot be followed.
    Gone,
}

/// Lines replaced between the two versions, as 0-based line indices: `old`
/// became `new`. An empty `old` is an insertion before line `old.start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineChange {
    pub old: Range<u32>,
    pub new: Range<u32>,
}

/// The threads a head move affected, by id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadMoved {
    /// Open threads whose lines changed, now resolved.
    pub resolved: Vec<u32>,
    /// Threads whose lines did not change, now pointing at the new commit.
    pub followed: Vec<u32>,
}

impl PullRequest {
    /// Moves the head to commit `to`. Threads on the head's side of the
    /// diff at the old head commit either follow their lines or, when the
    /// lines changed, resolve themselves. `update` tells what became of a
    /// file between the two commits; it is asked once per file with
    /// threads. Threads on the base's side, or anchored at an older
    /// commit, are left as they are.
    pub fn move_head<E>(
        &mut self,
        to: &str,
        at: Timestamp,
        mut update: impl FnMut(&str) -> Result<FileUpdate, E>,
    ) -> Result<HeadMoved, E> {
        let from = self.head.commit.clone();
        let mut moved = HeadMoved::default();
        if from == to {
            return Ok(moved);
        }
        let follows =
            |thread: &Thread| thread.anchor.side == Side::New && thread.anchor.commit == from;
        // Everything is read before anything changes, so a failed read
        // leaves the pull request as it was.
        let mut updates: HashMap<String, FileUpdate> = HashMap::new();
        for thread in self.threads.iter().filter(|thread| follows(thread)) {
            if !updates.contains_key(&thread.anchor.path) {
                let file = update(&thread.anchor.path)?;
                updates.insert(thread.anchor.path.clone(), file);
            }
        }
        for thread in &mut self.threads {
            if !follows(thread) {
                continue;
            }
            let anchor = &mut thread.anchor;
            let followed = match &updates[&anchor.path] {
                FileUpdate::Unchanged => Some((anchor.path.clone(), anchor.lines)),
                FileUpdate::Changed { path, lines } => {
                    follow(anchor.lines, lines).map(|followed| (path.clone(), followed))
                }
                FileUpdate::Gone => None,
            };
            match followed {
                Some((path, lines)) => {
                    anchor.path = path;
                    anchor.lines = lines;
                    anchor.commit = to.to_owned();
                    moved.followed.push(thread.id);
                }
                None if thread.resolved.is_none() => {
                    thread.resolved = Some(Resolution {
                        by: ResolvedBy::Auto,
                        commit: to.to_owned(),
                        at,
                    });
                    moved.resolved.push(thread.id);
                }
                // Already resolved: it stays on the commit it was made on.
                None => {}
            }
        }
        self.head.commit = to.to_owned();
        self.updated = at;
        Ok(moved)
    }
}

/// Lines `first..=last` (1-based) after `changes`, or `None` when one of
/// the changes touches them.
fn follow((first, last): (u32, u32), changes: &[LineChange]) -> Option<(u32, u32)> {
    // The lines as a 0-based range, like the changes.
    let (start, end) = (first.saturating_sub(1), last);
    let mut shift = 0_i64;
    for change in changes {
        let touched = if change.old.is_empty() {
            // Inserted between two of the lines.
            start < change.old.start && change.old.start < end
        } else {
            change.old.start < end && start < change.old.end
        };
        if touched {
            return None;
        }
        if change.old.end <= start {
            shift += i64::from(change.new.len() as u32) - i64::from(change.old.len() as u32);
        }
    }
    let moved = |line: u32| u32::try_from(i64::from(line) + shift).ok();
    Some((moved(first)?, moved(last)?))
}

#[cfg(test)]
mod tests {
    use super::{LineChange, follow};

    fn change(old: std::ops::Range<u32>, new: std::ops::Range<u32>) -> LineChange {
        LineChange { old, new }
    }

    #[test]
    fn lines_shift_by_changes_above_them() {
        // Lines 10–12 are indices 9..12.
        let lines = (10, 12);
        // Two lines inserted before line 3, one line removed at line 5.
        let above = [change(2..2, 2..4), change(4..5, 6..6)];
        assert_eq!(follow(lines, &above), Some((11, 13)));
        // Inserted right before the first line, or right after the last.
        assert_eq!(follow(lines, &[change(9..9, 9..12)]), Some((13, 15)));
        assert_eq!(follow(lines, &[change(12..12, 12..14)]), Some((10, 12)));
        // Changes below do not move them.
        assert_eq!(follow(lines, &[change(20..25, 20..21)]), Some((10, 12)));
    }

    #[test]
    fn changes_to_the_lines_touch_them() {
        let lines = (10, 12);
        // The first line, the last line, or a line inserted between them.
        assert_eq!(follow(lines, &[change(9..10, 9..10)]), None);
        assert_eq!(follow(lines, &[change(11..13, 11..11)]), None);
        assert_eq!(follow(lines, &[change(10..10, 10..11)]), None);
        // A change covering them all.
        assert_eq!(follow(lines, &[change(0..40, 0..2)]), None);
        // A single line.
        assert_eq!(follow((1, 1), &[change(0..1, 0..1)]), None);
        assert_eq!(follow((1, 1), &[change(0..0, 0..1)]), Some((2, 2)));
    }
}
