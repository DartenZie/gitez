//! How the lines of one file moved between two commits, so that comments on
//! them can follow.

use std::ops::Range;

use gix::bstr::{BStr, ByteSlice};
use gix::diff::blob::platform::prepare_diff::Operation;
use gix::diff::blob::{InternedInput, diff_with_slider_heuristics};
use gix::object::tree::diff::Change;

use crate::{Oid, Repository, Result};

/// What became of a file between two commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Followed {
    /// Its content is the same.
    Unchanged,
    /// Its content changed, and it may have been renamed to `path`.
    Changed {
        path: String,
        lines: Vec<ChangedLines>,
    },
    /// It was deleted, or its lines cannot be followed: it became binary,
    /// a submodule, or a rename git did not detect.
    Gone,
}

/// Lines replaced between two versions of a file, as 0-based line indices:
/// `old` in the first version became `new` in the second. An empty `old`
/// is an insertion before line `old.start`; an empty `new` a deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedLines {
    pub old: Range<u32>,
    pub new: Range<u32>,
}

impl Repository {
    /// How the file at `path` in commit `from` changed by commit `to`,
    /// following a rename.
    pub fn follow_lines(&self, path: &str, from: Oid, to: Oid) -> Result<Followed> {
        let repo = self.local();
        let old_tree = repo.find_commit(from)?.tree()?;
        let new_tree = repo.find_commit(to)?.tree()?;
        let mut cache = repo.diff_resource_cache_for_tree_diff()?;
        let wanted: &BStr = path.into();

        let mut followed = Ok(Followed::Unchanged);
        let mut found = false;
        let walked = old_tree
            .changes()?
            .options(|options| {
                options.track_path();
            })
            .for_each_to_obtain_tree(&new_tree, |change| {
                let new_path = match &change {
                    Change::Modification { location, .. } if *location == wanted => *location,
                    Change::Rewrite {
                        source_location,
                        location,
                        copy: false,
                        ..
                    } if *source_location == wanted => *location,
                    Change::Deletion { location, .. } if *location == wanted => {
                        found = true;
                        followed = Ok(Followed::Gone);
                        return Ok(std::ops::ControlFlow::Break(()));
                    }
                    _ => return Ok(std::ops::ControlFlow::Continue(())),
                };
                found = true;
                followed = if change.entry_mode().is_blob_or_symlink() {
                    changed_lines(&change, &mut cache).map(|lines| match lines {
                        Some(lines) => Followed::Changed {
                            path: new_path.to_str_lossy().into_owned(),
                            lines,
                        },
                        None => Followed::Gone,
                    })
                } else {
                    Ok(Followed::Gone)
                };
                Ok(std::ops::ControlFlow::Break(()))
            });
        // Stopping at the file is reported as the walk being cancelled.
        if !found {
            walked?;
        }
        followed
    }
}

/// The lines `change` replaced; `None` when a side is binary.
fn changed_lines(
    change: &Change<'_, '_, '_>,
    cache: &mut gix::diff::blob::Platform,
) -> Result<Option<Vec<ChangedLines>>> {
    change.diff(cache)?;
    cache.options.skip_internal_diff_if_external_is_configured = false;
    let prepared = cache.prepare_diff().map_err(gix::Error::from)?;
    let lines = match prepared.operation {
        Operation::InternalDiff { algorithm } => {
            let input =
                InternedInput::new(prepared.old.intern_source(), prepared.new.intern_source());
            let diff = diff_with_slider_heuristics(algorithm, &input);
            Some(
                diff.hunks()
                    .map(|hunk| ChangedLines {
                        old: hunk.before,
                        new: hunk.after,
                    })
                    .collect(),
            )
        }
        Operation::SourceOrDestinationIsBinary | Operation::ExternalCommand { .. } => None,
    };
    cache.clear_resource_cache_keep_allocation();
    Ok(lines)
}
