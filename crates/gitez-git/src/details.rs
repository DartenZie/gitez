//! Everything about one commit beyond its history row: the full message and
//! the files it changed.

use gix::bstr::ByteSlice;
use gix::object::tree::diff::Change;
use gix::objs::commit::message::BodyRef;

use crate::{Oid, Repository, Result, Signature};

/// Files after this many are listed without line counts, so a commit that
/// touches a whole vendored tree does not stall on diffing every file.
const MAX_FILES_WITH_LINE_COUNTS: usize = 2_000;

/// The full description of one commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDetails {
    pub id: Oid,
    /// The message after the summary line, without surrounding blank lines.
    /// Empty when the message is a single line.
    pub body: String,
    pub committer: Signature,
    /// The people credited in `Co-authored-by` trailers, in message order.
    pub co_authors: Vec<CoAuthor>,
    /// Whether the commit carries a signature. It is not verified.
    pub signed: bool,
    /// Files changed relative to the first parent (or added, for a root
    /// commit), in path order.
    pub files: Vec<FileChange>,
}

impl CommitDetails {
    /// Lines added and removed across all files that have line counts.
    pub fn line_totals(&self) -> (usize, usize) {
        self.files
            .iter()
            .filter_map(|file| file.lines)
            .fold((0, 0), |(added, removed), lines| {
                (added + lines.added, removed + lines.removed)
            })
    }
}

/// Someone credited on a commit with a `Co-authored-by` trailer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoAuthor {
    /// The name as written, or the email if the trailer gives only that.
    pub name: String,
    /// Empty if the trailer gives no email.
    pub email: String,
}

/// How a file changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    /// Moved from `from`, possibly with edits.
    Renamed {
        from: String,
    },
    /// Copied from `from`, possibly with edits.
    Copied {
        from: String,
    },
}

/// One changed file of a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Path from the repository root, after the change.
    pub path: String,
    pub kind: ChangeKind,
    /// `None` for binary files, and for files past the line-count limit.
    pub lines: Option<LineCounts>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LineCounts {
    pub added: usize,
    pub removed: usize,
}

impl Repository {
    /// The full message of the commit `HEAD` is on, as written, without
    /// trailing whitespace; `None` before the first commit.
    pub fn head_message(&self) -> Result<Option<String>> {
        let repo = self.local();
        if repo.head()?.is_unborn() {
            return Ok(None);
        }
        let commit = repo.head_commit()?;
        let message = commit.message_raw()?;
        Ok(Some(message.to_str_lossy().trim_end().to_owned()))
    }

    /// Reads the message and changed files of commit `id`. Merges are compared
    /// with their first parent, like the history's first-parent line.
    pub fn commit_details(&self, id: Oid) -> Result<CommitDetails> {
        let repo = self.local();
        let commit = repo.find_commit(id)?;
        let decoded = commit.decode()?;

        let body = decoded.message().body();
        let co_authors = body.as_ref().map(co_authors).unwrap_or_default();
        let message = body.map(|body| body.trim().to_str_lossy().into_owned());
        let committer = decoded.committer()?.trim();
        let committer = Signature {
            name: committer.name.to_str_lossy().into_owned(),
            email: committer.email.to_str_lossy().into_owned(),
            time: committer.time().unwrap_or_default().into(),
        };
        let signed = decoded.extra_headers().pgp_signature().is_some();
        drop(decoded);

        let (old_tree, new_tree) = trees(&repo, &commit)?;
        let mut line_cache = repo.diff_resource_cache_for_tree_diff()?;
        let mut files = Vec::new();
        let mut error = None;
        old_tree
            .changes()?
            .options(|options| {
                options.track_path();
            })
            .for_each_to_obtain_tree(&new_tree, |change| {
                if change.entry_mode().is_tree() {
                    return Ok(std::ops::ControlFlow::Continue(()));
                }
                let lines = if files.len() < MAX_FILES_WITH_LINE_COUNTS {
                    match line_counts(&change, &mut line_cache) {
                        Ok(lines) => lines,
                        Err(err) => {
                            error = Some(err);
                            return Ok(std::ops::ControlFlow::Break(()));
                        }
                    }
                } else {
                    None
                };
                files.push(FileChange {
                    path: change.location().to_str_lossy().into_owned(),
                    kind: kind(&change),
                    lines,
                });
                Ok(std::ops::ControlFlow::Continue(()))
            })?;
        if let Some(error) = error {
            return Err(error);
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(CommitDetails {
            id,
            body: message.unwrap_or_default(),
            committer,
            co_authors,
            signed,
            files,
        })
    }
}

/// The co-authors in the trailers of `body`, each person once.
fn co_authors(body: &BodyRef<'_>) -> Vec<CoAuthor> {
    let mut co_authors: Vec<CoAuthor> = Vec::new();
    for trailer in body.trailers() {
        if !trailer.token.eq_ignore_ascii_case(b"co-authored-by") {
            continue;
        }
        let Some(co_author) = parse_co_author(&trailer.value.to_str_lossy()) else {
            continue;
        };
        let known = co_authors.iter().any(|other| {
            if co_author.email.is_empty() {
                other.name == co_author.name
            } else {
                other.email.eq_ignore_ascii_case(&co_author.email)
            }
        });
        if !known {
            co_authors.push(co_author);
        }
    }
    co_authors
}

/// Reads the value of a trailer, normally `Name <email>`.
fn parse_co_author(value: &str) -> Option<CoAuthor> {
    let (name, email) = match value.rsplit_once('<') {
        Some((name, email)) => (name.trim(), email.trim().trim_end_matches('>').trim()),
        None => (value.trim(), ""),
    };
    let name = if name.is_empty() { email } else { name };
    (!name.is_empty()).then(|| CoAuthor {
        name: name.to_owned(),
        email: email.to_owned(),
    })
}

/// The trees commit `commit` compares: its first parent's and its own. A root
/// commit, or the boundary of a shallow clone, compares with the empty tree.
pub(crate) fn trees<'repo>(
    repo: &'repo gix::Repository,
    commit: &gix::Commit<'repo>,
) -> Result<(gix::Tree<'repo>, gix::Tree<'repo>)> {
    let first_parent = commit.parent_ids().next().map(|id| id.detach());
    // The boundary of a shallow clone has no parent to compare with.
    let is_shallow_boundary = repo
        .shallow_commits()?
        .is_some_and(|shallow| shallow.contains(&commit.id));
    let old_tree = match first_parent {
        Some(parent) if !is_shallow_boundary => repo.find_commit(parent)?.tree()?,
        _ => repo.empty_tree(),
    };
    Ok((old_tree, commit.tree()?))
}

pub(crate) fn kind(change: &Change<'_, '_, '_>) -> ChangeKind {
    match change {
        Change::Addition { .. } => ChangeKind::Added,
        Change::Deletion { .. } => ChangeKind::Deleted,
        Change::Modification { .. } => ChangeKind::Modified,
        Change::Rewrite {
            source_location,
            copy,
            ..
        } => {
            let from = source_location.to_str_lossy().into_owned();
            if *copy {
                ChangeKind::Copied { from }
            } else {
                ChangeKind::Renamed { from }
            }
        }
    }
}

/// Lines added and removed by `change`; `None` for binary content and for
/// entries that are not files, such as submodules.
fn line_counts(
    change: &Change<'_, '_, '_>,
    cache: &mut gix::diff::blob::Platform,
) -> Result<Option<LineCounts>> {
    if !change.entry_mode().is_blob_or_symlink() {
        return Ok(None);
    }
    let stats = change.diff(cache)?.line_counts()?;
    // The cache only grows; one file's contents are all it needs to hold.
    cache.clear_resource_cache_keep_allocation();
    Ok(stats.map(|stats| LineCounts {
        added: stats.insertions as usize,
        removed: stats.removals as usize,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(body: &[u8]) -> Vec<(String, String)> {
        co_authors(&BodyRef::from_bytes(body))
            .into_iter()
            .map(|co_author| (co_author.name, co_author.email))
            .collect()
    }

    fn pair(name: &str, email: &str) -> (String, String) {
        (name.to_owned(), email.to_owned())
    }

    #[test]
    fn co_authors_are_read_from_the_trailers_in_any_case() {
        assert_eq!(
            names(
                b"Explains why.\n\nCo-authored-by: Ada Lovelace <ada@example.com>\n\
                  co-AUTHORED-by: Grace Hopper <grace@example.com>\n"
            ),
            [
                pair("Ada Lovelace", "ada@example.com"),
                pair("Grace Hopper", "grace@example.com")
            ]
        );
    }

    #[test]
    fn a_message_of_only_trailers_has_co_authors() {
        assert_eq!(
            names(b"Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>\n"),
            [pair("Claude Opus 5.5", "noreply@anthropic.com")]
        );
    }

    #[test]
    fn a_person_credited_twice_is_listed_once() {
        assert_eq!(
            names(
                b"Co-authored-by: Ada <ada@example.com>\n\
                  Co-authored-by: Ada L. <ADA@example.com>\n"
            ),
            [pair("Ada", "ada@example.com")]
        );
    }

    #[test]
    fn other_trailers_and_prose_are_not_co_authors() {
        assert_eq!(names(b"Signed-off-by: Ada <ada@example.com>\n"), []);
        assert_eq!(
            names(b"Thanks to Co-authored-by: nobody at all, really.\n"),
            []
        );
    }

    #[test]
    fn a_co_author_can_lack_a_name_or_an_email() {
        assert_eq!(names(b"Co-authored-by: Ada\n"), [pair("Ada", "")]);
        assert_eq!(
            names(b"Co-authored-by: <ada@example.com>\n"),
            [pair("ada@example.com", "ada@example.com")]
        );
    }
}
