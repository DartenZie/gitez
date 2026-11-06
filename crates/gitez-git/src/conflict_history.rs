//! Why a file conflicts: the commits each side of a merge made since the
//! two sides split, and which of them changed the lines of each conflict.
//!
//! Each side is followed along its first parents, newest first, from its tip
//! back to the merge base. The lines of a conflict are tracked through every
//! commit that changed the file, like `git log -L` does: a commit whose
//! change overlaps them touched the conflict, and each line is credited to
//! the newest commit that changed it, like `git blame`.

use std::ops::Range;

use gix::bstr::ByteSlice;

use crate::{Conflict, ConflictFile, LineKind, Oid, Repository, Result, Segment, Signature};

/// Commits followed on one side at most; a side longer than this is cut
/// off, and [`SideHistory::truncated`] says so.
const MAX_COMMITS: usize = 1000;
/// Unchanged lines shown around what a commit changed.
const CHANGE_CONTEXT: u32 = 1;
/// Lines of one commit's change kept for a conflict.
const MAX_CHANGE_LINES: usize = 60;

/// The history behind the conflicts of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictHistory {
    /// Where the two sides split; `None` when they share no history.
    pub base: Option<HistoryCommit>,
    /// The side checked out.
    pub ours: SideHistory,
    /// The side merged in.
    pub theirs: SideHistory,
}

/// A commit in a conflict's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryCommit {
    pub id: Oid,
    /// First line of the message.
    pub summary: String,
    /// The message after its first line, trimmed; `None` when there is none.
    pub body: Option<String>,
    pub author: Signature,
}

/// One side of a merge since the merge base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideHistory {
    /// The side's commits along its first parents, newest first, up to but
    /// not including the merge base.
    pub commits: Vec<HistoryCommit>,
    /// The side has more commits than were followed.
    pub truncated: bool,
    /// For each conflict of the file, in file order, what this side's
    /// commits did to it.
    pub conflicts: Vec<ConflictSide>,
}

/// What one side's commits did to the lines of one conflict.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConflictSide {
    /// The commits whose changes touch the conflict, as indices into
    /// [`SideHistory::commits`], newest first, each with what it changed
    /// there, numbered as in that commit and its parent.
    pub touched_by: Vec<(usize, Vec<ChangeLine>)>,
    /// For each of the side's lines in the conflict, the newest commit that
    /// changed it, as an index into [`SideHistory::commits`]; `None` for a
    /// line that is as it was at the merge base, or older than the commits
    /// followed.
    pub lines: Vec<Option<usize>>,
}

/// A line of a commit's change to a conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeLine {
    pub kind: LineKind,
    /// Line number in the parent, from 1; `None` for added lines.
    pub old: Option<u32>,
    /// Line number in the commit, from 1; `None` for removed lines.
    pub new: Option<u32>,
    pub text: String,
}

/// A change between two versions of a file: lines `old` (0-based) became
/// lines `new`.
#[derive(Debug, Clone)]
struct Hunk {
    old: Range<u32>,
    new: Range<u32>,
}

impl Repository {
    /// The history behind the conflicts in `file`, the working tree's
    /// version of the file at `path` while merging commit `theirs` into
    /// commit `ours`. Blocks; run it on a background thread.
    pub fn conflict_history(
        &self,
        path: &str,
        file: &ConflictFile,
        ours: Oid,
        theirs: Oid,
    ) -> Result<ConflictHistory> {
        let repo = self.local();
        let merge_base = repo.merge_base(ours, theirs)?.map(|id| id.detach());
        let base = merge_base.map(|id| history_commit(&repo, id)).transpose()?;
        Ok(ConflictHistory {
            base,
            ours: side_history(&repo, path, file, ours, merge_base, |c| &c.ours)?,
            theirs: side_history(&repo, path, file, theirs, merge_base, |c| &c.theirs)?,
        })
    }
}

fn history_commit(repo: &gix::Repository, id: Oid) -> Result<HistoryCommit> {
    let commit = repo.find_commit(id)?;
    let decoded = commit.decode()?;
    let message = decoded.message();
    let author = decoded.author()?.trim();
    Ok(HistoryCommit {
        id,
        summary: message.summary().to_str_lossy().into_owned(),
        body: message
            .body()
            .map(|body| body.trim().to_str_lossy().into_owned())
            .filter(|body| !body.is_empty()),
        author: Signature {
            name: author.name.to_str_lossy().into_owned(),
            email: author.email.to_str_lossy().into_owned(),
            time: author.time().unwrap_or_default().into(),
        },
    })
}

/// The content of the file at `path` in commit `id`; `None` when the commit
/// has no such file or it is not a file.
fn blob_at(repo: &gix::Repository, id: Oid, path: &str) -> Result<Option<(Oid, String)>> {
    let tree = repo.find_commit(id)?.tree()?;
    let Some(entry) = tree.lookup_entry_by_path(path)? else {
        return Ok(None);
    };
    if !entry.mode().is_blob() {
        return Ok(None);
    }
    let blob = repo.find_blob(entry.object_id())?;
    Ok(Some((
        entry.object_id(),
        String::from_utf8_lossy(&blob.data).into_owned(),
    )))
}

fn split_lines(text: &str) -> Vec<&str> {
    let body = text.strip_suffix('\n').unwrap_or(text);
    if body.is_empty() && text.is_empty() {
        return Vec::new();
    }
    body.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

/// The lines that changed from `old` to `new`.
fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    use gix::diff::blob::{Algorithm, InternedInput, diff_with_slider_heuristics};
    let input = InternedInput::new(old, new);
    diff_with_slider_heuristics(Algorithm::Histogram, &input)
        .hunks()
        .map(|hunk| Hunk {
            old: hunk.before,
            new: hunk.after,
        })
        .collect()
}

/// Where `range` of the new version was in the old one, given the `hunks`
/// between them. A range inside a change widens to the whole change.
fn map_back(range: &Range<u32>, hunks: &[Hunk]) -> Range<u32> {
    let point = |line: u32, end: bool| -> u32 {
        let mut shift: i64 = 0;
        for hunk in hunks {
            if line < hunk.new.start || (end && line == hunk.new.start) {
                break;
            }
            if line < hunk.new.end {
                return if end { hunk.old.end } else { hunk.old.start };
            }
            shift = i64::from(hunk.old.end) - i64::from(hunk.new.end);
        }
        u32::try_from(i64::from(line) + shift).unwrap_or(0)
    };
    let start = point(range.start, false);
    let end = point(range.end, true).max(start);
    start..end
}

/// The change `hunk` overlaps `range`, both in the new version. An empty
/// range or change touches what is next to it.
fn touches(hunk: &Hunk, range: &Range<u32>) -> bool {
    if hunk.new.is_empty() || range.is_empty() {
        hunk.new.start <= range.end && range.start <= hunk.new.end
    } else {
        hunk.new.start < range.end && range.start < hunk.new.end
    }
}

/// The side's version of the file as the conflict markers give it, with
/// where each conflict's lines are, 0-based.
fn side_version(
    file: &ConflictFile,
    side: fn(&Conflict) -> &Vec<String>,
) -> (String, Vec<Range<u32>>) {
    let mut text = String::new();
    let mut ranges = Vec::new();
    let mut next: u32 = 0;
    for segment in &file.segments {
        let lines: &[String] = match segment {
            Segment::Clean(lines) => lines,
            Segment::Conflict(conflict) => {
                let lines = side(conflict);
                let count = u32::try_from(lines.len()).unwrap_or(u32::MAX);
                ranges.push(next..next.saturating_add(count));
                lines
            }
        };
        next = next.saturating_add(u32::try_from(lines.len()).unwrap_or(u32::MAX));
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
    }
    (text, ranges)
}

/// Lines `hunks` changed that touch `range` (all in the new version), with
/// a little context, from the `old` and `new` versions.
fn change_lines(hunks: &[Hunk], range: &Range<u32>, old: &[&str], new: &[&str]) -> Vec<ChangeLine> {
    let mut lines = Vec::new();
    let line = |kind, old: Option<u32>, new: Option<u32>, text: &str| ChangeLine {
        kind,
        old: old.map(|n| n + 1),
        new: new.map(|n| n + 1),
        text: text.to_owned(),
    };
    let touching: Vec<&Hunk> = hunks.iter().filter(|hunk| touches(hunk, range)).collect();
    let mut shown_to: Option<u32> = None;
    for hunk in touching {
        // Context before, in the new version; the old one lines up.
        let from = hunk.new.start.saturating_sub(CHANGE_CONTEXT);
        let from = shown_to.map_or(from, |shown| from.max(shown));
        for n in from..hunk.new.start {
            let o = hunk.old.start - (hunk.new.start - n);
            if let Some(text) = new.get(n as usize) {
                lines.push(line(LineKind::Context, Some(o), Some(n), text));
            }
        }
        for o in hunk.old.clone() {
            if let Some(text) = old.get(o as usize) {
                lines.push(line(LineKind::Removed, Some(o), None, text));
            }
        }
        for n in hunk.new.clone() {
            if let Some(text) = new.get(n as usize) {
                lines.push(line(LineKind::Added, None, Some(n), text));
            }
        }
        let to = (hunk.new.end + CHANGE_CONTEXT).min(u32::try_from(new.len()).unwrap_or(0));
        for n in hunk.new.end..to {
            let o = hunk.old.end + (n - hunk.new.end);
            if let Some(text) = new.get(n as usize) {
                lines.push(line(LineKind::Context, Some(o), Some(n), text));
            }
        }
        shown_to = Some(to);
        if lines.len() >= MAX_CHANGE_LINES {
            lines.truncate(MAX_CHANGE_LINES);
            break;
        }
    }
    lines
}

fn side_history(
    repo: &gix::Repository,
    path: &str,
    file: &ConflictFile,
    tip: Oid,
    merge_base: Option<Oid>,
    side: fn(&Conflict) -> &Vec<String>,
) -> Result<SideHistory> {
    // The commits along the first parents, down to the merge base.
    let mut commits = Vec::new();
    let mut truncated = false;
    let mut next = Some(tip);
    while let Some(id) = next {
        if Some(id) == merge_base {
            break;
        }
        if commits.len() == MAX_COMMITS {
            truncated = true;
            break;
        }
        let commit = repo.find_commit(id)?;
        next = commit.parent_ids().next().map(|parent| parent.detach());
        commits.push((history_commit(repo, id)?, next));
    }

    // The conflicts' lines in the side's version as the markers give it,
    // then in the tip's own version of the file: the clean lines of the
    // working tree's file can hold the other side's changes.
    let (marked, mut ranges) = side_version(file, side);
    let mut lines: Vec<Vec<Option<u32>>> = ranges
        .iter()
        .map(|range| range.clone().map(Some).collect())
        .collect();
    let mut current = blob_at(repo, tip, path)?;
    if let Some((_, tip_text)) = &current {
        let to_tip = hunks(tip_text, &marked);
        for (range, lines) in ranges.iter_mut().zip(&mut lines) {
            *range = map_back(range, &to_tip);
            for line in lines.iter_mut() {
                *line = line.and_then(|n| {
                    let mapped = map_back(&(n..n + 1), &to_tip);
                    (mapped.len() == 1).then_some(mapped.start)
                });
            }
        }
    }

    let mut conflicts: Vec<ConflictSide> = lines
        .iter()
        .map(|tracked| ConflictSide {
            touched_by: Vec::new(),
            lines: vec![None; tracked.len()],
        })
        .collect();
    for (index, (_, parent)) in commits.iter().enumerate() {
        let Some((blob, text)) = &current else {
            break;
        };
        let previous = match parent {
            Some(parent) => blob_at(repo, *parent, path)?,
            None => None,
        };
        if previous.as_ref().is_some_and(|(id, _)| id == blob) {
            continue;
        }
        let old_text = previous.as_ref().map_or("", |(_, text)| text.as_str());
        let changes = hunks(old_text, text);
        let (old_lines, new_lines) = (split_lines(old_text), split_lines(text));
        for ((range, conflict), tracked) in ranges.iter_mut().zip(&mut conflicts).zip(&mut lines) {
            if changes.iter().any(|hunk| touches(hunk, range)) {
                let shown = change_lines(&changes, range, &old_lines, &new_lines);
                conflict.touched_by.push((index, shown));
            }
            for (line, credit) in tracked.iter_mut().zip(&mut conflict.lines) {
                let Some(n) = *line else { continue };
                if changes.iter().any(|hunk| hunk.new.contains(&n)) {
                    *credit = Some(index);
                    *line = None;
                } else {
                    *line = Some(map_back(&(n..n + 1), &changes).start);
                }
            }
            *range = map_back(range, &changes);
        }
        current = previous;
    }
    Ok(SideHistory {
        commits: commits.into_iter().map(|(commit, _)| commit).collect(),
        truncated,
        conflicts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(old: Range<u32>, new: Range<u32>) -> Hunk {
        Hunk { old, new }
    }

    #[test]
    fn ranges_map_back_through_changes() {
        // Two lines inserted before line 5 of the old version.
        let inserted = [hunk(5..5, 5..7)];
        assert_eq!(map_back(&(8..9), &inserted), 6..7);
        assert_eq!(map_back(&(2..3), &inserted), 2..3);
        // A range inside the inserted lines came from nowhere.
        assert_eq!(map_back(&(5..6), &inserted), 5..5);
        // A replaced line maps to what it replaced.
        let replaced = [hunk(3..4, 3..5)];
        assert_eq!(map_back(&(4..5), &replaced), 3..4);
    }

    #[test]
    fn changes_next_to_an_empty_range_touch_it() {
        assert!(touches(&hunk(2..2, 2..3), &(3..3)));
        assert!(!touches(&hunk(2..2, 2..3), &(4..6)));
        assert!(touches(&hunk(4..5, 4..4), &(4..6)));
    }
}
