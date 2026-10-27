use std::sync::Arc;

use gitez_highlight::{
    Highlights, Language, MAX_FILE_BYTES, MAX_LINE_BYTES, Outcome, Span, TokenKind, highlight_file,
    highlight_runs,
};

fn language(path: &str, first_line: &str) -> Option<&'static str> {
    Language::detect(path, first_line).map(Language::name)
}

fn highlights(path: &str, code: &str) -> Arc<Highlights> {
    match highlight_file(path, code.as_bytes()) {
        Outcome::Highlighted(highlights) => highlights,
        other => panic!("{path} was not highlighted: {other:?}"),
    }
}

/// The text and kind of each span of line `number` (1-based) of `code`.
fn tokens(highlights: &Highlights, code: &str, number: u32) -> Vec<(String, TokenKind)> {
    let line = code
        .lines()
        .nth(number as usize - 1)
        .expect("the line exists");
    highlights
        .line(number, line)
        .expect("the line is highlighted")
        .iter()
        .map(|span| (line[span.range()].to_owned(), span.kind))
        .collect()
}

/// Spans are ordered, do not overlap, and fall within the line on char
/// boundaries, for every line of `code`.
fn assert_well_formed(highlights: &Highlights, code: &str) {
    for (index, line) in code.lines().enumerate() {
        let number = index as u32 + 1;
        let Some(spans) = highlights.line(number, line) else {
            continue;
        };
        let mut end = 0;
        for span in spans {
            let range = span.range();
            assert!(range.start >= end, "line {number}: {spans:?}");
            assert!(range.start < range.end, "line {number}: {spans:?}");
            assert!(range.end <= line.len(), "line {number}: {spans:?}");
            assert!(line.is_char_boundary(range.start) && line.is_char_boundary(range.end));
            end = range.end;
        }
    }
}

#[test]
fn languages_are_detected_by_extension() {
    assert_eq!(language("src/main.rs", ""), Some("Rust"));
    assert_eq!(language("web/app.tsx", ""), Some("TypeScriptReact"));
    assert_eq!(language("web/app.TS", ""), Some("TypeScript"));
    assert_eq!(language("lib/module.mjs", ""), Some("JavaScript"));
    assert_eq!(language("config.jsonc", ""), Some("JSON"));
    assert_eq!(language("docs/README.md", ""), Some("Markdown"));
    assert_eq!(language("a/b/c.py", ""), Some("Python"));
    assert_eq!(language("view.html.erb", ""), Some("HTML (Rails)"));
    assert_eq!(language("C:\\code\\main.go", ""), Some("Go"));
}

#[test]
fn languages_are_detected_by_file_name() {
    assert_eq!(language("Makefile", ""), Some("Makefile"));
    assert_eq!(language("sub/GNUmakefile", ""), Some("Makefile"));
    assert_eq!(language("Dockerfile", ""), Some("Dockerfile"));
    assert_eq!(language("docker/Dockerfile.dev", ""), Some("Dockerfile"));
    assert_eq!(language("Containerfile", ""), Some("Dockerfile"));
    assert_eq!(language(".bashrc", ""), Some("Bourne Again Shell (bash)"));
    assert_eq!(
        language("home/.zshrc", ""),
        Some("Bourne Again Shell (bash)")
    );
    assert_eq!(language("CMakeLists.txt", ""), Some("CMake"));
    assert_eq!(language("Cargo.lock", ""), Some("TOML"));
    assert_eq!(language("Gemfile", ""), Some("Ruby"));
    assert_eq!(language(".gitignore", ""), Some("Git Ignore"));
}

#[test]
fn languages_are_detected_by_first_line() {
    assert_eq!(
        language("bin/tool", "#!/usr/bin/env python3"),
        Some("Python")
    );
    assert_eq!(
        language("scripts/run", "#!/bin/bash"),
        Some("Bourne Again Shell (bash)")
    );
    assert_eq!(language("data", "<?xml version=\"1.0\"?>"), Some("XML"));
    // The name wins over the first line.
    assert_eq!(language("x.rs", "#!/bin/bash"), Some("Rust"));
}

#[test]
fn unknown_files_have_no_language() {
    assert_eq!(language("x.unknownext", ""), None);
    assert_eq!(language("README", "Hello there"), None);
    // Plain text is not a language to highlight.
    assert_eq!(language("notes.txt", ""), None);
    assert!(matches!(
        highlight_file("x.unknownext", b"fn main() {}\n"),
        Outcome::Plain
    ));
}

#[test]
fn rust_tokens_have_their_kinds() {
    let code = "fn main() {\n    let s = \"hi\\n\"; // greet\n    let n = 42;\n}\n";
    let highlights = highlights("main.rs", code);
    let first = tokens(&highlights, code, 1);
    assert!(
        first.contains(&("fn".into(), TokenKind::Keyword)),
        "{first:?}"
    );
    assert!(
        first.contains(&("main".into(), TokenKind::Function)),
        "{first:?}"
    );

    let second = tokens(&highlights, code, 2);
    // The quotes colour as the string, the escape inside it on its own.
    assert!(
        second.contains(&("\"hi".into(), TokenKind::String)),
        "{second:?}"
    );
    assert!(
        second.contains(&("\\n".into(), TokenKind::Escape)),
        "{second:?}"
    );
    assert!(
        second.contains(&("\"".into(), TokenKind::String)),
        "{second:?}"
    );
    assert!(
        second.contains(&("// greet".into(), TokenKind::Comment)),
        "{second:?}"
    );

    let third = tokens(&highlights, code, 3);
    assert!(
        third.contains(&("42".into(), TokenKind::Number)),
        "{third:?}"
    );
    assert_well_formed(&highlights, code);
}

#[test]
fn declarations_are_keywords_and_primitive_types_are_types() {
    let rust = "struct S;\nfn f() { let x: i32 = 1; }\n";
    let spans = highlights("types.rs", rust);
    let first = tokens(&spans, rust, 1);
    assert!(
        first.contains(&("struct".into(), TokenKind::Keyword)),
        "{first:?}"
    );
    let second = tokens(&spans, rust, 2);
    for keyword in ["fn", "let"] {
        assert!(
            second.contains(&(keyword.into(), TokenKind::Keyword)),
            "{second:?}"
        );
    }
    assert!(
        second.contains(&("i32".into(), TokenKind::Type)),
        "{second:?}"
    );

    let c = "int main(void) { struct s x; return 0; }\n";
    let spans = highlights("types.c", c);
    let line = tokens(&spans, c, 1);
    assert_eq!(line.first(), Some(&("int".into(), TokenKind::Type)));
    assert!(
        line.contains(&("struct".into(), TokenKind::Keyword)),
        "{line:?}"
    );

    let js = "const a = 1;\n";
    let spans = highlights("decl.js", js);
    assert_eq!(
        tokens(&spans, js, 1).first(),
        Some(&("const".into(), TokenKind::Keyword))
    );
}

#[test]
fn a_block_comment_colours_every_line_it_spans() {
    let code = "let a = 1;\n/* start\n   middle fn let \"x\"\n   end */\nlet b = 2;\n";
    let highlights = highlights("lib.rs", code);
    assert_eq!(
        tokens(&highlights, code, 3),
        vec![("   middle fn let \"x\"".into(), TokenKind::Comment)]
    );
    assert_eq!(
        tokens(&highlights, code, 4),
        vec![("   end */".into(), TokenKind::Comment)]
    );
    let after = tokens(&highlights, code, 5);
    assert!(
        after.contains(&("let".into(), TokenKind::Keyword)),
        "{after:?}"
    );
}

#[test]
fn a_python_string_colours_every_line_it_spans() {
    let code = "x = \"\"\"first\ndef not_code():\n    return 1\n\"\"\"\ndef f():\n    pass\n";
    let highlights = highlights("doc.py", code);
    for number in 2..=3 {
        let line = tokens(&highlights, code, number);
        assert!(
            !line.is_empty() && line.iter().all(|(_, kind)| *kind == TokenKind::String),
            "line {number}: {line:?}"
        );
    }
    let after = tokens(&highlights, code, 5);
    assert!(
        after.contains(&("def".into(), TokenKind::Keyword)),
        "{after:?}"
    );
}

#[test]
fn spans_are_byte_ranges_into_utf8_lines() {
    let code = "// čeština 🦀\nlet s = \"žluťoučký 🦀\"; let n = 1;\n";
    let highlights = highlights("utf8.rs", code);
    assert_eq!(
        tokens(&highlights, code, 1),
        vec![("// čeština 🦀".into(), TokenKind::Comment)]
    );
    let second = tokens(&highlights, code, 2);
    assert!(
        second.contains(&("\"žluťoučký 🦀\"".into(), TokenKind::String)),
        "{second:?}"
    );
    assert!(
        second.contains(&("1".into(), TokenKind::Number)),
        "{second:?}"
    );
    assert_well_formed(&highlights, code);
}

#[test]
fn crlf_endings_are_not_part_of_lines() {
    let code = "fn a() {}\r\n// c\r\n";
    let highlights = highlights("crlf.rs", code);
    let comment = highlights.line(2, "// c").expect("highlighted");
    assert_eq!(comment.last().map(Span::range), Some(0..4));
    // A line given with its '\r' still matches.
    assert!(highlights.line(2, "// c\r").is_some());
}

#[test]
fn invalid_utf8_is_replaced_as_lossy_conversion_does() {
    let bytes = b"// bad \xff byte\nlet x = 1;\n";
    let Outcome::Highlighted(highlights) = highlight_file("bad.rs", bytes) else {
        panic!("not highlighted");
    };
    let text = String::from_utf8_lossy(bytes);
    let first = text.lines().next().expect("a first line");
    let spans = highlights.line(1, first).expect("highlighted");
    assert_eq!(spans.last().map(Span::range), Some(0..first.len()));
    assert_well_formed(&highlights, &text);
}

#[test]
fn a_line_of_another_length_gets_no_spans() {
    let code = "let x = 1;\n";
    let highlights = highlights("len.rs", code);
    assert!(highlights.line(1, "let x = 1;").is_some());
    assert!(highlights.line(1, "let x = 10;").is_none());
    assert!(highlights.line(2, "").is_none());
    assert!(highlights.line(0, "").is_none());
}

#[test]
fn binary_and_huge_files_are_skipped() {
    assert!(matches!(
        highlight_file("a.rs", b"fn main() {}\0\n"),
        Outcome::Skipped
    ));
    let huge = "// x\n".repeat(MAX_FILE_BYTES / 5 + 1);
    assert!(huge.len() > MAX_FILE_BYTES);
    assert!(matches!(
        highlight_file("huge.rs", huge.as_bytes()),
        Outcome::Skipped
    ));
}

#[test]
fn a_long_line_gets_no_spans_but_the_next_one_does() {
    let long = format!("let s = \"{}\";", "a".repeat(MAX_LINE_BYTES));
    let code = format!("{long}\nlet n = 1;\n");
    let highlights = highlights("min.rs", &code);
    assert!(highlights.line(1, &long).is_none());
    let next = tokens(&highlights, &code, 2);
    assert!(
        next.contains(&("let".into(), TokenKind::Keyword)),
        "{next:?}"
    );
}

#[test]
fn runs_are_highlighted_by_their_line_numbers() {
    let rust = Language::detect("x.rs", "").expect("Rust");
    let highlights = highlight_runs(
        rust,
        [
            (40, vec!["let z = 3;"]),
            (10, vec!["let a = 1;", "// two", "let c = \"3\";"]),
        ],
    );
    assert_eq!(
        highlights.line(11, "// two").map(|spans| spans.to_vec()),
        Some(vec![Span::new(0..6, TokenKind::Comment)])
    );
    assert!(highlights.line(12, "let c = \"3\";").is_some());
    assert!(highlights.line(40, "let z = 3;").is_some());
    assert!(highlights.line(9, "").is_none());
    assert!(highlights.line(13, "").is_none());
    assert!(highlights.line(20, "let a = 1;").is_none());
    assert!(!highlights.is_empty());
}

#[test]
fn common_languages_give_well_formed_spans() {
    let samples = [
        (
            "a.ts",
            "interface A { b: string }\nconst x: A = { b: `t${1}` };\n",
        ),
        (
            "a.py",
            "import os\n@dec\ndef f(x: int) -> str:\n    return f\"{x}\"\n",
        ),
        (
            "a.go",
            "package main\n\nfunc main() {\n\tfmt.Println(\"hi\")\n}\n",
        ),
        (
            "a.html",
            "<!doctype html>\n<p class=\"x\">Hi &amp; bye</p>\n<script>let a = 1;</script>\n",
        ),
        (
            "a.md",
            "# Title\n\nSome *text* and `code` and [link](http://x).\n",
        ),
        ("a.yaml", "key: value\nlist:\n  - 1\n  - \"two\" # c\n"),
        ("a.toml", "[package]\nname = \"x\"\nversion = 1\n"),
        ("a.c", "#include <stdio.h>\nint main(void) { return 0; }\n"),
        ("a.sh", "#!/bin/sh\nfor f in *; do echo \"$f\"; done\n"),
        ("a.json", "{\"a\": [1, true, null]}\n"),
    ];
    for (path, code) in samples {
        let highlights = highlights(path, code);
        assert!(!highlights.is_empty(), "{path} has no spans");
        assert_well_formed(&highlights, code);
    }
}

#[test]
fn the_same_contents_are_highlighted_once() {
    let code = b"fn cached() -> u32 { 7 }\n";
    let (Outcome::Highlighted(first), Outcome::Highlighted(second)) = (
        highlight_file("one.rs", code),
        highlight_file("other/two.rs", code),
    ) else {
        panic!("not highlighted");
    };
    assert!(Arc::ptr_eq(&first, &second));
    // The same bytes in another language are highlighted separately.
    let Outcome::Highlighted(other) = highlight_file("one.c", code) else {
        panic!("not highlighted");
    };
    assert!(!Arc::ptr_eq(&first, &other));
}
