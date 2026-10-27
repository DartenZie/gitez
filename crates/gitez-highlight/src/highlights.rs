//! Highlighted lines, stored compactly by line number.

use std::ops::Range;

use crate::{MAX_LINE_BYTES, TokenKind};

/// A run of one token kind within a line, as byte offsets into the line
/// without its ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    start: u16,
    end: u16,
    pub kind: TokenKind,
}

impl Span {
    /// A span over `range` of a line. Offsets past `u16::MAX`, which no
    /// highlighted line reaches, are clamped to it.
    pub fn new(range: Range<usize>, kind: TokenKind) -> Self {
        let clamp = |offset: usize| u16::try_from(offset).unwrap_or(u16::MAX);
        Self {
            start: clamp(range.start),
            end: clamp(range.end),
            kind,
        }
    }

    /// The bytes of the line the span covers.
    pub fn range(&self) -> Range<usize> {
        usize::from(self.start)..usize::from(self.end)
    }
}

/// Spans of highlighted lines, by 1-based line number.
///
/// Lines come in runs of consecutive numbers: one for a whole file, several
/// for separate pieces of one.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Highlights {
    /// Ordered by first line.
    runs: Vec<Run>,
}

#[derive(Debug, PartialEq, Eq)]
struct Run {
    /// The number of the run's first line.
    first: u32,
    /// The length of each line without its ending.
    lens: Vec<u32>,
    /// Where each line's spans start in `spans`, and one more entry for the
    /// end of the last line's.
    starts: Vec<u32>,
    spans: Vec<Span>,
}

impl Highlights {
    /// The spans of line `number`, which reads `text` (a trailing `'\r'` is
    /// ignored).
    ///
    /// `None` when the line was not highlighted, was too long to be, or is
    /// not as long as `text`: a file changed by a filter or since it was
    /// highlighted never has the wrong bytes coloured. `Some` of an empty
    /// slice is a highlighted line with nothing to colour.
    pub fn line(&self, number: u32, text: &str) -> Option<&[Span]> {
        let index = self.runs.partition_point(|run| run.first <= number);
        let run = &self.runs[index.checked_sub(1)?];
        let line = usize::try_from(number - run.first).ok()?;
        let len = usize::try_from(*run.lens.get(line)?).ok()?;
        let text = text.strip_suffix('\r').unwrap_or(text);
        if len != text.len() || len > MAX_LINE_BYTES {
            return None;
        }
        let start = usize::try_from(run.starts[line]).ok()?;
        let end = usize::try_from(run.starts[line + 1]).ok()?;
        run.spans.get(start..end)
    }

    /// Whether no line has a span.
    pub fn is_empty(&self) -> bool {
        self.runs.iter().all(|run| run.spans.is_empty())
    }

    /// About how many bytes the highlights hold: the spans, and each
    /// line's length and where its spans start, which a file with few
    /// spans still has for every line.
    pub(crate) fn footprint(&self) -> usize {
        size_of::<Self>()
            + self
                .runs
                .iter()
                .map(|run| {
                    size_of::<Run>()
                        + run.spans.len() * size_of::<Span>()
                        + (run.lens.len() + run.starts.len()) * size_of::<u32>()
                })
                .sum::<usize>()
    }
}

/// Collects lines in order, run by run.
#[derive(Debug, Default)]
pub(crate) struct Builder {
    runs: Vec<Run>,
}

impl Builder {
    /// Starts a run at line `first`; it continues the previous run when that
    /// ended right before it.
    pub(crate) fn start(&mut self, first: u32) {
        if let Some(run) = self.runs.last()
            && u32::try_from(run.lens.len())
                .is_ok_and(|len| run.first.checked_add(len) == Some(first))
        {
            return;
        }
        self.runs.push(Run {
            first,
            lens: Vec::new(),
            starts: vec![0],
            spans: Vec::new(),
        });
    }

    /// Adds the next line of the current run, `len` bytes long, with its
    /// ordered spans.
    pub(crate) fn line(&mut self, len: usize, spans: &[Span]) {
        let Some(run) = self.runs.last_mut() else {
            return;
        };
        run.lens.push(u32::try_from(len).unwrap_or(u32::MAX));
        run.spans.extend_from_slice(spans);
        run.starts
            .push(u32::try_from(run.spans.len()).unwrap_or(u32::MAX));
    }

    pub(crate) fn finish(mut self) -> Highlights {
        self.runs.retain(|run| !run.lens.is_empty());
        // Runs may be given in any order but are expected not to overlap;
        // where they do, the later-starting run answers for its lines.
        self.runs.sort_by_key(|run| run.first);
        self.runs.shrink_to_fit();
        for run in &mut self.runs {
            run.lens.shrink_to_fit();
            run.starts.shrink_to_fit();
            run.spans.shrink_to_fit();
        }
        Highlights { runs: self.runs }
    }
}
