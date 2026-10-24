//! Files left with conflict markers by a merge: the lines both sides agree
//! on, and for each conflict the lines of each side, read from the markers
//! git writes into the working tree.

/// A file with conflict markers, split into what merged cleanly and what
/// did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictFile {
    pub segments: Vec<Segment>,
    /// Lines end in CRLF.
    pub crlf: bool,
    /// The last line ends with a line break.
    pub final_newline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Lines that merged cleanly.
    Clean(Vec<String>),
    Conflict(Conflict),
}

/// One conflict: what each side has where they disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The branch checked out ("ours").
    pub ours: Vec<String>,
    /// The common ancestor's lines, when git wrote them (the `diff3` and
    /// `zdiff3` conflict styles).
    pub base: Option<Vec<String>>,
    /// The branch merged in ("theirs").
    pub theirs: Vec<String>,
}

/// Marker lines are seven characters, then a space and a label or nothing.
const MARKER_SIZE: usize = 7;

#[derive(Clone, Copy, PartialEq)]
enum Marker {
    Ours,
    Base,
    Separator,
    Theirs,
}

fn marker(line: &str) -> Option<Marker> {
    let (head, rest) = line.split_at_checked(MARKER_SIZE)?;
    if !(rest.is_empty() || rest.starts_with(' ')) {
        return None;
    }
    match head {
        "<<<<<<<" => Some(Marker::Ours),
        "|||||||" => Some(Marker::Base),
        "=======" if rest.is_empty() => Some(Marker::Separator),
        ">>>>>>>" => Some(Marker::Theirs),
        _ => None,
    }
}

impl ConflictFile {
    /// Reads `text`; `None` when it has no conflict markers, or markers git
    /// would not write (unclosed or nested ones).
    pub fn parse(text: &str) -> Option<Self> {
        let crlf = text.contains("\r\n");
        let final_newline = text.ends_with('\n');
        let mut segments = Vec::new();
        let mut clean = Vec::new();
        // The conflict being read and which part of it.
        let mut open: Option<(Conflict, Marker)> = None;
        let body = text.strip_suffix('\n').unwrap_or(text);
        for line in body.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            match (marker(line), &mut open) {
                (Some(Marker::Ours), None) => {
                    if !clean.is_empty() {
                        segments.push(Segment::Clean(std::mem::take(&mut clean)));
                    }
                    open = Some((
                        Conflict {
                            ours: Vec::new(),
                            base: None,
                            theirs: Vec::new(),
                        },
                        Marker::Ours,
                    ));
                }
                (Some(Marker::Base), Some((conflict, part @ Marker::Ours))) => {
                    conflict.base = Some(Vec::new());
                    *part = Marker::Base;
                }
                (Some(Marker::Separator), Some((_, part @ (Marker::Ours | Marker::Base)))) => {
                    *part = Marker::Separator;
                }
                (Some(Marker::Theirs), Some((_, Marker::Separator))) => {
                    let (conflict, _) = open.take()?;
                    segments.push(Segment::Conflict(conflict));
                }
                (Some(_), _) => return None,
                (None, None) => clean.push(line.to_owned()),
                (None, Some((conflict, part))) => {
                    let lines = match part {
                        Marker::Ours => &mut conflict.ours,
                        Marker::Base => conflict.base.get_or_insert_with(Vec::new),
                        _ => &mut conflict.theirs,
                    };
                    lines.push(line.to_owned());
                }
            }
        }
        if open.is_some() {
            return None;
        }
        if !clean.is_empty() {
            segments.push(Segment::Clean(clean));
        }
        segments
            .iter()
            .any(|segment| matches!(segment, Segment::Conflict(_)))
            .then_some(Self {
                segments,
                crlf,
                final_newline,
            })
    }

    /// The conflicts, in file order.
    pub fn conflicts(&self) -> impl Iterator<Item = &Conflict> {
        self.segments.iter().filter_map(|segment| match segment {
            Segment::Conflict(conflict) => Some(conflict),
            Segment::Clean(_) => None,
        })
    }

    /// The file with conflict `n` (counting from 0) replaced by
    /// `resolved(n)`, with the line endings it had.
    pub fn resolve(&self, mut resolved: impl FnMut(usize) -> Vec<String>) -> String {
        let mut lines: Vec<String> = Vec::new();
        let mut n = 0;
        for segment in &self.segments {
            match segment {
                Segment::Clean(clean) => lines.extend(clean.iter().cloned()),
                Segment::Conflict(_) => {
                    lines.extend(resolved(n));
                    n += 1;
                }
            }
        }
        let newline = if self.crlf { "\r\n" } else { "\n" };
        let mut text = lines.join(newline);
        if self.final_newline && !lines.is_empty() {
            text.push_str(newline);
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "\
import a;
<<<<<<< HEAD
const DIR = '.gitez';
||||||| base
const DIR = 'reviews';
=======
const DIR = 'gitez-reviews';
>>>>>>> feature
fn x() {
<<<<<<< HEAD
  ours();
=======
>>>>>>> feature
}
";

    #[test]
    fn markers_split_the_file_into_clean_lines_and_conflicts() {
        let file = ConflictFile::parse(TEXT).expect("conflicts");
        let conflicts: Vec<_> = file.conflicts().collect();
        assert_eq!(conflicts.len(), 2);
        assert_eq!(conflicts[0].ours, ["const DIR = '.gitez';"]);
        assert_eq!(
            conflicts[0].base.as_deref(),
            Some(&["const DIR = 'reviews';".to_owned()][..])
        );
        assert_eq!(conflicts[0].theirs, ["const DIR = 'gitez-reviews';"]);
        // The second has no base section, and theirs removed the line.
        assert_eq!(conflicts[1].base, None);
        assert!(conflicts[1].theirs.is_empty());
        assert_eq!(file.segments.len(), 5);

        let resolved = file.resolve(|n| match n {
            0 => conflicts[0].theirs.clone(),
            _ => conflicts[1].ours.clone(),
        });
        assert_eq!(
            resolved,
            "import a;\nconst DIR = 'gitez-reviews';\nfn x() {\n  ours();\n}\n"
        );
    }

    #[test]
    fn line_endings_are_kept() {
        let text = "a\r\n<<<<<<< HEAD\r\nb\r\n=======\r\nc\r\n>>>>>>> x\r\nd";
        let file = ConflictFile::parse(text).expect("conflicts");
        assert!(file.crlf && !file.final_newline);
        assert_eq!(file.resolve(|_| vec!["c".into()]), "a\r\nc\r\nd");
    }

    #[test]
    fn files_without_proper_markers_are_not_conflicts() {
        assert_eq!(ConflictFile::parse("a\nb\n"), None);
        assert_eq!(ConflictFile::parse("<<<<<<< HEAD\na\n=======\nb\n"), None);
        assert_eq!(
            ConflictFile::parse("<<<<<<< HEAD\n<<<<<<< HEAD\n=======\n>>>>>>> x\n"),
            None
        );
        // Lines that merely start like markers are content.
        assert_eq!(ConflictFile::parse("=======x\n<<<<<<<<\n"), None);
    }
}
