//! Running syntect and naming what it matched.
//!
//! Syntect names what it matched with TextMate scopes
//! (`keyword.control.rust`, `string.quoted.double`). Those map onto a small
//! set of [`TokenKind`]s, the same way gpui-kit's own syntect highlighter
//! maps them onto its theme's highlight names, so code in a diff and in the
//! editor is coloured alike.

use std::collections::HashMap;

use syntect::{
    parsing::{ParseState, Scope, ScopeStack},
    util::LinesWithEndings,
};

use crate::{
    Language, MAX_LINE_BYTES, SYNTAXES, TokenKind,
    highlights::{Builder, Span},
};

/// Parses `text` from a fresh state and adds its lines to `builder`,
/// numbering them from `first`.
pub(crate) fn parse(language: Language, first: u32, text: &str, builder: &mut Builder) {
    let syntaxes = &*SYNTAXES;
    let mut parser = ParseState::new(language.syntax());
    let mut scopes = ScopeStack::new();
    let mut namings = HashMap::<Scope, Option<Naming>>::new();
    let mut spans = Vec::new();

    builder.start(first);
    for line in LinesWithEndings::from(text) {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let content = content.strip_suffix('\r').unwrap_or(content);
        spans.clear();

        // A minified line is not parsed at all: it would be slow and is
        // unreadable coloured or not. The next line starts from the scopes
        // this one started with, as it does after a line syntect cannot
        // parse.
        if content.len() <= MAX_LINE_BYTES
            && let Ok(operations) = parser.parse_line(line, syntaxes)
        {
            let mut cursor = 0;
            for (index, operation) in operations {
                push(&mut spans, cursor..index, content, &scopes, &mut namings);
                let _ = scopes.apply(&operation);
                cursor = index;
            }
            push(
                &mut spans,
                cursor..line.len(),
                content,
                &scopes,
                &mut namings,
            );
        }
        builder.line(content.len(), &spans);
    }
}

/// What a scope says about the token it covers.
#[derive(Debug, Clone, Copy)]
enum Naming {
    Kind(TokenKind),
    /// `storage.type` with nothing more specific than the language after
    /// it, which grammars give both declaration words (Rust's `let`, C's
    /// `struct`) and type names (`int`): the word decides.
    StorageType,
}

/// Words that declare something rather than name a type, for scopes that
/// do not tell the two apart.
const DECLARATION_WORDS: &[&str] = &[
    "class",
    "const",
    "def",
    "enum",
    "fn",
    "func",
    "function",
    "impl",
    "interface",
    "let",
    "mod",
    "module",
    "namespace",
    "object",
    "package",
    "static",
    "struct",
    "trait",
    "type",
    "typedef",
    "union",
    "val",
    "var",
];

/// Adds `range` of `line`, a line without its ending, clipped to the line,
/// as the kind of its innermost scope that has one.
fn push(
    spans: &mut Vec<Span>,
    range: std::ops::Range<usize>,
    line: &str,
    scopes: &ScopeStack,
    namings: &mut HashMap<Scope, Option<Naming>>,
) {
    let range = range.start.min(line.len())..range.end.min(line.len());
    if range.is_empty() {
        return;
    }
    // The innermost scope that names something wins, so the quotes of a
    // string (`punctuation.definition.string` inside `string`) colour as
    // the string rather than as punctuation.
    let Some(naming) = scopes
        .scopes
        .iter()
        .rev()
        .find_map(|scope| *namings.entry(*scope).or_insert_with(|| naming(*scope)))
    else {
        return;
    };
    let kind = match naming {
        Naming::Kind(kind) => kind,
        Naming::StorageType => {
            let word = line.get(range.clone()).unwrap_or_default().trim();
            if DECLARATION_WORDS.contains(&word) {
                TokenKind::Keyword
            } else {
                TokenKind::Type
            }
        }
    };
    match spans.last_mut() {
        Some(last) if last.range().end == range.start && last.kind == kind => {
            *last = Span::new(last.range().start..range.end, kind);
        }
        _ => spans.push(Span::new(range, kind)),
    }
}

/// What a TextMate scope names, or `None` to let an enclosing scope
/// decide.
fn naming(scope: Scope) -> Option<Naming> {
    let scope = scope.build_string();
    let is = |prefix: &str| {
        scope == prefix
            || scope
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('.'))
    };

    let kind = if is("punctuation.definition.string")
        || is("punctuation.definition.comment")
        || is("invalid")
        || is("meta")
        || is("source")
        || is("text")
    {
        // Delimiters belong to what they delimit; the rest are containers
        // with no colour of their own.
        return None;
    } else if is("comment") {
        TokenKind::Comment
    } else if is("constant.character.escape") {
        TokenKind::Escape
    } else if is("string") || is("markup.raw") {
        TokenKind::String
    } else if is("constant.numeric") {
        TokenKind::Number
    } else if is("constant") || is("support.constant") || is("entity.name.constant") {
        TokenKind::Constant
    } else if is("keyword.operator") {
        TokenKind::Operator
    } else if is("keyword.control.import") {
        TokenKind::Preprocessor
    } else if is("keyword") || is("storage.modifier") || is("variable.language") {
        TokenKind::Keyword
    } else if is("storage.type") {
        // `fn`, `class` and `let` are declarations, `int` and `bool` are
        // types. The segments after `storage.type` say which, such as
        // `storage.type.function.rust` or `storage.type.primitive.java`,
        // unless all that follows is the language.
        const DECLARATIONS: &[&str] = &[
            "class",
            "const",
            "enum",
            "function",
            "impl",
            "interface",
            "keyword",
            "let",
            "module",
            "namespace",
            "struct",
            "trait",
            "type",
            "union",
            "var",
            "variable",
        ];
        let details: Vec<&str> = scope.split('.').skip(2).collect();
        if details.iter().any(|detail| DECLARATIONS.contains(detail)) {
            TokenKind::Keyword
        } else if details.len() > 1 {
            TokenKind::Type
        } else {
            return Some(Naming::StorageType);
        }
    } else if is("entity.name.function.decorator") || is("entity.name.label") {
        TokenKind::Attribute
    } else if is("entity.name.function") || is("support.function") || is("variable.function") {
        TokenKind::Function
    } else if is("entity.name.type")
        || is("entity.name.class")
        || is("entity.name.struct")
        || is("entity.name.enum")
        || is("entity.name.trait")
        || is("entity.name.interface")
        || is("entity.other.inherited-class")
        || is("support.type")
        || is("support.class")
    {
        TokenKind::Type
    } else if is("entity.name.tag") {
        TokenKind::Tag
    } else if is("entity.other.attribute-name") {
        TokenKind::Attribute
    } else if is("entity.name.section") || is("markup.heading") {
        TokenKind::Heading
    } else if is("variable.other.member")
        || is("variable.other.property")
        || is("support.variable.property")
    {
        TokenKind::Property
    } else if is("variable") {
        TokenKind::Variable
    } else if is("markup.underline.link") {
        TokenKind::Link
    } else if is("punctuation") || is("markup.list") {
        TokenKind::Punctuation
    } else {
        return None;
    };
    Some(Naming::Kind(kind))
}
