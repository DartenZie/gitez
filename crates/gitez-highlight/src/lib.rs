//! Syntax highlighting for GitEZ's code views.
//!
//! Files are highlighted whole with syntect and bat's collection of about two
//! hundred grammars (through `two-face`), so a string or comment that spans
//! lines is coloured on every one of them. The result is line-oriented: the
//! spans of each line are byte ranges into that line without its ending, so a
//! diff can colour a removed line from the old file and an added one from the
//! new file by their line numbers.
//!
//! The crate knows nothing of GPUI or git. Spans carry a [`TokenKind`]; the
//! application turns kinds into colours with its own theme.

mod cache;
mod highlights;
mod language;
mod parse;

use std::sync::{Arc, LazyLock};

use syntect::parsing::SyntaxSet;

pub use highlights::{Highlights, Span};
pub use language::Language;

/// Files larger than this are not highlighted.
pub const MAX_FILE_BYTES: usize = 1024 * 1024;

/// Longer lines, usually minified code, get no spans. Parsing carries on
/// after them with the state it had before.
///
/// Small enough that a span's offsets fit in a `u16`.
pub const MAX_LINE_BYTES: usize = 2000;

/// How much of a file is searched for a NUL byte to call it binary, as git
/// does.
const BINARY_PROBE_BYTES: usize = 8000;

/// Deserialising the syntax set takes tens of milliseconds, so it happens
/// once, on the first use, which callers keep off the UI thread.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

/// What a token is, for choosing its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    Comment,
    Keyword,
    /// Imports, includes and other preprocessor directives.
    Preprocessor,
    Operator,
    Punctuation,
    String,
    /// An escape sequence inside a string.
    Escape,
    Number,
    /// Named constants, booleans and other language constants.
    Constant,
    Type,
    Function,
    Variable,
    /// Fields, properties and the keys of maps.
    Property,
    /// Attributes, annotations, decorators and labels.
    Attribute,
    /// Markup tags.
    Tag,
    /// Markup headings and section names.
    Heading,
    Link,
}

/// The outcome of highlighting a file.
#[derive(Debug, Clone)]
pub enum Outcome {
    Highlighted(Arc<Highlights>),
    /// No grammar for this file.
    Plain,
    /// Binary, or larger than [`MAX_FILE_BYTES`].
    Skipped,
}

impl Outcome {
    /// The highlights, if the file was highlighted.
    pub fn highlights(&self) -> Option<&Arc<Highlights>> {
        match self {
            Outcome::Highlighted(highlights) => Some(highlights),
            Outcome::Plain | Outcome::Skipped => None,
        }
    }
}

/// Highlights the whole file at `path` (which only picks its language) with
/// contents `bytes`.
///
/// Invalid UTF-8 is replaced as `String::from_utf8_lossy` does, line by line
/// the same as for the whole file, so spans match lines converted that way.
/// Results are cached by content and language, so the same version of a file
/// is highlighted once however many times it is shown.
pub fn highlight_file(path: &str, bytes: &[u8]) -> Outcome {
    if is_binary(bytes) || bytes.len() > MAX_FILE_BYTES {
        return Outcome::Skipped;
    }
    let text = String::from_utf8_lossy(bytes);
    let first_line = text.lines().next().unwrap_or_default();
    let Some(language) = Language::detect(path, first_line) else {
        return Outcome::Plain;
    };
    let highlights = cache::get_or_insert(&text, language, || {
        let mut builder = highlights::Builder::default();
        parse::parse(language, 1, &text, &mut builder);
        builder.finish()
    });
    Outcome::Highlighted(highlights)
}

/// Highlights separate runs of lines, each given with the 1-based number of
/// its first line and its lines without their endings.
///
/// The fallback for a file that cannot be read or highlighted whole, such as
/// the lines of one side of a diff's hunks. Each run is parsed from a fresh
/// state, so a run that starts inside a comment is coloured as code.
pub fn highlight_runs<'a>(
    language: Language,
    runs: impl IntoIterator<Item = (u32, Vec<&'a str>)>,
) -> Highlights {
    let mut builder = highlights::Builder::default();
    for (first, lines) in runs {
        let mut text = String::with_capacity(lines.iter().map(|line| line.len() + 1).sum());
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        parse::parse(language, first, &text, &mut builder);
    }
    builder.finish()
}

/// Whether `bytes` looks binary: a NUL among its first bytes, as git checks.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_PROBE_BYTES)].contains(&0)
}
