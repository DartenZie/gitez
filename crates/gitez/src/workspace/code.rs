//! Lines of code as the diff views and the conflict editor show them: tabs
//! as spaces, and coloured by the spans of `gitez-highlight`.

use std::ops::Range;

use gitez_highlight::{Span, TokenKind};
use gpui_kit::*;

use crate::theme;

/// Spaces a tab is shown as.
pub(crate) const TAB: &str = "    ";

/// `text` as a line of code shows it, without trailing carriage returns and
/// with tabs as spaces, and `spans` moved to match. Spans past the end, off
/// a character boundary or overlapping the one before are left out.
pub(crate) fn display_runs(text: &str, spans: &[Span]) -> (String, Vec<(Range<usize>, TokenKind)>) {
    let text = text.trim_end_matches('\r');
    // Most lines with no spans are too long to have been highlighted, so
    // they skip the table of offsets, a word per byte.
    if spans.is_empty() {
        return (text.replace('\t', TAB), Vec::new());
    }
    let mut shown = String::with_capacity(text.len());
    // Where each byte of `text` lands in `shown`, and one past the end.
    let mut offsets = Vec::with_capacity(text.len() + 1);
    for c in text.chars() {
        for _ in 0..c.len_utf8() {
            offsets.push(shown.len());
        }
        if c == '\t' {
            shown.push_str(TAB);
        } else {
            shown.push(c);
        }
    }
    offsets.push(shown.len());

    let mut runs = Vec::with_capacity(spans.len());
    let mut end_of_last = 0;
    for span in spans {
        let range = span.range();
        let (start, end) = (range.start, range.end.min(text.len()));
        if start >= end
            || start < end_of_last
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(end)
        {
            continue;
        }
        end_of_last = end;
        runs.push((offsets[start]..offsets[end], span.kind));
    }
    (shown, runs)
}

/// A line of code coloured by `spans`, or plain without them.
pub(crate) fn styled_code(text: &str, spans: Option<&[Span]>) -> StyledText {
    let (shown, runs) = display_runs(text, spans.unwrap_or_default());
    // An empty line keeps its height.
    let shown = if shown.is_empty() {
        " ".to_owned()
    } else {
        shown
    };
    StyledText::new(shown).with_highlights(
        runs.into_iter()
            .map(|(range, kind)| (range, theme::syntax_style(kind))),
    )
}

#[cfg(test)]
mod tests {
    use gitez_highlight::{Span, TokenKind};

    use super::display_runs;

    #[test]
    fn tabs_move_the_spans_after_them() {
        let spans = [
            Span::new(0..2, TokenKind::Keyword),
            Span::new(3..7, TokenKind::String),
        ];
        let (shown, runs) = display_runs("fn\t\"é\"", &spans);
        assert_eq!(shown, "fn    \"é\"");
        assert_eq!(
            runs,
            [(0..2, TokenKind::Keyword), (6..10, TokenKind::String)]
        );
    }

    #[test]
    fn spans_past_the_end_or_inside_a_character_are_dropped() {
        let spans = [
            Span::new(0..1, TokenKind::Keyword),
            // Inside "é".
            Span::new(2..3, TokenKind::String),
            Span::new(3..10, TokenKind::Comment),
        ];
        let (shown, runs) = display_runs("aé b\r", &spans);
        assert_eq!(shown, "aé b");
        assert_eq!(
            runs,
            [(0..1, TokenKind::Keyword), (3..5, TokenKind::Comment)]
        );
    }
}
