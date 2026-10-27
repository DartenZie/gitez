//! Choosing a grammar for a file.

use syntect::parsing::SyntaxReference;

use crate::SYNTAXES;

/// A grammar from bat's syntax set.
#[derive(Clone, Copy)]
pub struct Language(&'static SyntaxReference);

impl Language {
    /// The grammar for the file at `path` whose first line is `first_line`:
    /// by its whole name (`Makefile`, `.bashrc`), its extensions, longest
    /// first (`d.ts` before `ts`), a few names bat recognises by pattern,
    /// and lastly the first line (a shebang, a modeline, `<?xml`).
    ///
    /// `None` for a file no grammar fits; plain text counts as none.
    pub fn detect(path: &str, first_line: &str) -> Option<Self> {
        let syntaxes = &*SYNTAXES;
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let undotted = name.strip_prefix('.').unwrap_or(name);
        let suffixes = undotted
            .match_indices('.')
            .map(|(dot, _)| &undotted[dot + 1..]);

        [name, undotted]
            .into_iter()
            .chain(suffixes)
            .filter(|token| !token.is_empty())
            .find_map(|token| syntaxes.find_syntax_by_extension(token))
            .or_else(|| alias(name).and_then(|token| syntaxes.find_syntax_by_token(token)))
            .or_else(|| syntaxes.find_syntax_by_first_line(first_line))
            .filter(|syntax| syntax.name != "Plain Text")
            .map(Self)
    }

    /// The grammar's name, such as `"Rust"` or `"Makefile"`.
    pub fn name(self) -> &'static str {
        &self.0.name
    }

    /// The token gpui-kit's code editor finds this same grammar by: the
    /// name, or for a name that is first taken as another grammar's
    /// extension (`"ASP"`, `"TeX"`), one of the grammar's extensions.
    pub fn editor_token(self) -> &'static str {
        // The editor looks a token up as syntect does, extensions first,
        // and takes the names of plain text as no grammar.
        let resolves = |token: &&str| {
            !matches!(
                token.to_lowercase().as_str(),
                "text" | "plain" | "plaintext"
            ) && SYNTAXES
                .find_syntax_by_token(token)
                .is_some_and(|syntax| std::ptr::eq(syntax, self.0))
        };
        std::iter::once(self.name())
            .chain(self.0.file_extensions.iter().map(String::as_str))
            .find(resolves)
            .unwrap_or(self.name())
    }

    pub(crate) fn syntax(self) -> &'static SyntaxReference {
        self.0
    }
}

impl std::fmt::Debug for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Language").field(&self.0.name).finish()
    }
}

impl PartialEq for Language {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.0, other.0)
    }
}

impl Eq for Language {}

/// The grammar token for file names bat matches with globs or its own
/// mapping, which its syntax set alone does not carry.
fn alias(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    let extension = lower.rsplit_once('.').map(|(_, extension)| extension);
    let token = match (lower.as_str(), extension) {
        ("containerfile", _) => "dockerfile",
        (name, _) if name.starts_with("dockerfile.") || name.starts_with("containerfile.") => {
            "dockerfile"
        }
        ("flake.lock" | "composer.lock", _) => "json",
        (_, Some("jsonc" | "json5" | "webmanifest")) => "json",
        (_, Some("mjs" | "cjs" | "jsx")) => "js",
        (name, _) if name.ends_with("bashrc") || name.ends_with("zshrc") => "bash",
        _ => return None,
    };
    Some(token)
}

#[cfg(test)]
mod tests {
    use super::Language;
    use crate::SYNTAXES;

    #[test]
    fn every_grammar_is_found_again_by_its_editor_token() {
        for syntax in SYNTAXES.syntaxes() {
            if syntax.name == "Plain Text" {
                continue;
            }
            let token = Language(syntax).editor_token();
            let found = SYNTAXES
                .find_syntax_by_token(token)
                .map(|found| &found.name);
            assert_eq!(found, Some(&syntax.name), "token {token:?}");
        }
        let asp = SYNTAXES.find_syntax_by_name("ASP").expect("ASP");
        assert_ne!(Language(asp).editor_token(), "ASP");
    }
}
