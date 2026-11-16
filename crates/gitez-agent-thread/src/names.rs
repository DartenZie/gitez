//! Names derived from a prompt or a repository: a thread's title and slug,
//! and the key of a repository's folder of worktrees.

use std::path::Path;

/// The longest title, in characters, "…" included.
const MAX_TITLE: usize = 72;
/// The longest slug, in characters.
const MAX_SLUG: usize = 40;

/// The agent thread's title: the prompt's first sentence or line, at most 72
/// characters, cut at a word boundary with "…". A sentence ends at `.`,
/// `!`, `?` or `:` followed by a space; the `.` and `:` are left out. A
/// prompt that starts with a level-1 Markdown heading (a brief does) is
/// titled by it ([`title_from_message`]).
pub fn title_from_prompt(prompt: &str) -> String {
    if let Some(title) = title_from_message(prompt) {
        return shorten(&title);
    }
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut end = line.len();
    let mut chars = line.char_indices().peekable();
    while let Some((index, char)) = chars.next() {
        if matches!(char, '.' | '!' | '?' | ':')
            && chars.peek().is_some_and(|(_, next)| next.is_whitespace())
        {
            end = index + char.len_utf8();
            break;
        }
    }
    let sentence = line[..end].trim_end_matches(['.', ':']).trim_end();
    if sentence.is_empty() {
        return "Untitled thread".to_owned();
    }
    shorten(sentence)
}

/// The title of a new thread asked for with `prompt` and `images` images:
/// [`title_from_prompt`], or "Pasted image" ("Pasted images") when the
/// prompt has no words but images.
pub fn title_from_request(prompt: &str, images: usize) -> String {
    match images {
        0 => title_from_prompt(prompt),
        _ if !prompt.trim().is_empty() => title_from_prompt(prompt),
        1 => "Pasted image".to_owned(),
        _ => "Pasted images".to_owned(),
    }
}

/// A title the agent gave its work: its final message starts with a
/// level-1 ATX heading (`# Title`; up to three spaces before it, blank
/// lines before it allowed). Cleaned with [`clean_title`]; `None` when the
/// message does not start so or nothing is left.
pub fn title_from_message(message: &str) -> Option<String> {
    split_title(message).0
}

/// The title of [`title_from_message`] and the rest of the message: what
/// follows the heading line and the blank lines after it. Without a title
/// the rest is the whole message.
pub fn split_title(message: &str) -> (Option<String>, &str) {
    let mut offset = 0;
    for line in message.split_inclusive('\n') {
        offset += line.len();
        let text = line.trim_end_matches(['\n', '\r']);
        if text.trim().is_empty() {
            continue;
        }
        let Some(title) = heading(text).and_then(clean_title) else {
            return (None, message);
        };
        let rest = &message[offset..];
        let mut body = rest.len();
        let mut skipped = 0;
        for line in rest.split_inclusive('\n') {
            if !line.trim().is_empty() {
                body = skipped;
                break;
            }
            skipped += line.len();
        }
        return (Some(title), &rest[body..]);
    }
    (None, message)
}

/// `message` as shown under a thread titled `title`: without its first
/// line when that line is a heading that gave the thread its title, so the
/// title is not shown twice.
pub fn message_without_title<'a>(message: &'a str, title: &str) -> &'a str {
    match split_title(message) {
        (Some(heading), body) if heading == title => body,
        _ => message,
    }
}

/// The text of `line` if it is a level-1 ATX heading (CommonMark: up to
/// three spaces of indentation, `#`, then a space, a tab or the end), its
/// closing `#`s left out.
fn heading(line: &str) -> Option<&str> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = line[indent..].strip_prefix('#')?;
    if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    let text = rest.trim();
    // An optional closing sequence: `#`s after a space, or only `#`s.
    let without = text.trim_end_matches('#');
    if without.is_empty() {
        return Some("");
    }
    if without.len() < text.len() && without.ends_with([' ', '\t']) {
        return Some(without.trim_end());
    }
    Some(text)
}

/// A title from untrusted text (the agent's heading, a commit subject): one
/// line, whitespace collapsed, control and bidirectional-override
/// characters removed, surrounding quotes or backticks and a trailing
/// period dropped, and at most 72 characters as [`title_from_prompt`]
/// cuts them. `None` when nothing is left.
pub fn clean_title(raw: &str) -> Option<String> {
    let plain: String = raw
        .chars()
        .map(|char| if char.is_whitespace() { ' ' } else { char })
        .filter(|&char| !char.is_control() && !is_bidi_control(char))
        .collect();
    let mut title = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    // Again until nothing changes: `"Title."` or ``` ``Title`` ```.
    loop {
        let before = title.len();
        title = unquote(&title)
            .trim_end_matches(['.', '。'])
            .trim()
            .to_owned();
        if title.len() == before {
            break;
        }
    }
    (!title.is_empty()).then(|| shorten(&title))
}

/// `text` without one pair of quotes or backticks around all of it.
fn unquote(text: &str) -> &str {
    const PAIRS: [(char, char); 9] = [
        ('"', '"'),
        ('\'', '\''),
        ('`', '`'),
        ('“', '”'),
        ('‘', '’'),
        ('„', '“'),
        ('«', '»'),
        ('「', '」'),
        ('『', '』'),
    ];
    for (open, close) in PAIRS {
        if let Some(inner) = text
            .strip_prefix(open)
            .and_then(|rest| rest.strip_suffix(close))
        {
            return inner.trim();
        }
    }
    text
}

/// Characters that reorder the text around them when shown.
fn is_bidi_control(char: char) -> bool {
    matches!(
        char,
        '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// `text` (trimmed, not empty) at most 72 characters, cut at a word
/// boundary with "…".
fn shorten(text: &str) -> String {
    if text.chars().count() <= MAX_TITLE {
        return text.to_owned();
    }
    // Room for the "…".
    let cut: String = text.chars().take(MAX_TITLE - 1).collect();
    let next_is_space = text
        .chars()
        .nth(MAX_TITLE - 1)
        .is_some_and(char::is_whitespace);
    let kept = match cut.rfind(char::is_whitespace) {
        _ if next_is_space => cut.as_str(),
        Some(space) if space > 0 => &cut[..space],
        // One long word: cut inside it.
        _ => cut.as_str(),
    };
    let kept = kept.trim_end_matches(|char: char| char.is_whitespace() || ",;:.-".contains(char));
    format!("{kept}…")
}

/// A slug of `title`: lower-case ASCII letters and digits, words joined by
/// `-`, at most 40 characters at a word boundary; "thread" if nothing is
/// left. Non-ASCII letters are transliterated when simple (é→e) or dropped.
pub fn slug(title: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    for char in title.chars().flat_map(char::to_lowercase) {
        if char.is_ascii_lowercase() || char.is_ascii_digit() {
            word.push(char);
        } else if let Some(ascii) = transliterate(char) {
            word.push_str(ascii);
        } else if !char.is_alphanumeric() && !word.is_empty() {
            // Anything but a letter separates words; letters with no simple
            // ASCII form are dropped without splitting the word.
            words.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    let mut slug = String::new();
    for word in words {
        let extra = if slug.is_empty() { 0 } else { 1 };
        if slug.len() + extra + word.len() > MAX_SLUG {
            if slug.is_empty() {
                slug = word[..MAX_SLUG].to_owned();
            }
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word);
    }
    if slug.is_empty() {
        "thread".to_owned()
    } else {
        slug
    }
}

/// The ASCII letters for a lower-case accented Latin letter.
fn transliterate(char: char) -> Option<&'static str> {
    Some(match char {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'æ' => "ae",
        'ç' | 'ć' | 'č' | 'ĉ' | 'ċ' => "c",
        'ď' | 'đ' | 'ð' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
        'ĥ' | 'ħ' => "h",
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => "i",
        'ĵ' => "j",
        'ķ' => "k",
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
        'ñ' | 'ń' | 'ņ' | 'ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => "o",
        'œ' => "oe",
        'ŕ' | 'ŗ' | 'ř' => "r",
        'ś' | 'ŝ' | 'ş' | 'š' | 'ș' => "s",
        'ß' => "ss",
        'ţ' | 'ť' | 'ŧ' | 'ț' => "t",
        'þ' => "th",
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
        'ŵ' => "w",
        'ý' | 'ÿ' | 'ŷ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        _ => return None,
    })
}

/// `<repository name>-<8 hex digits>`: the name made filesystem-safe, and
/// FNV-1a 64 of the canonical common dir's bytes, first 8 hex digits.
/// Stable across processes and Rust versions. A common dir that cannot be
/// made canonical (it does not exist) is hashed as given.
pub fn repository_key(repository_name: &str, common_dir: &Path) -> String {
    let canonical = common_dir
        .canonicalize()
        .unwrap_or_else(|_| common_dir.to_owned());
    let hash = fnv1a64(canonical.as_os_str().as_encoded_bytes());
    let mut name = String::new();
    for char in repository_name.chars() {
        if char.is_ascii_alphanumeric() || matches!(char, '-' | '_' | '.') {
            name.push(char);
        } else if let Some(ascii) = char
            .to_lowercase()
            .next()
            .and_then(transliterate)
            .filter(|_| char.is_alphabetic())
        {
            name.push_str(ascii);
        } else if !name.ends_with('-') {
            name.push('-');
        }
    }
    // No hidden folder, no leading or trailing dash.
    let name = name.trim_start_matches(['.', '-']).trim_end_matches('-');
    let name = if name.is_empty() { "repository" } else { name };
    format!("{name}-{:08x}", hash >> 32)
}

/// The 64-bit FNV-1a hash of `bytes`.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(OFFSET, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_take_the_first_sentence_or_line() {
        assert_eq!(
            title_from_prompt(
                "Add a blame view to the commit detail: clicking a file shows who changed each line."
            ),
            "Add a blame view to the commit detail"
        );
        assert_eq!(
            title_from_prompt("\n  Fix the flaky test.  Then run clippy.\nMore"),
            "Fix the flaky test"
        );
        assert_eq!(
            title_from_prompt("Why does v1.2 crash? It did not before."),
            "Why does v1.2 crash?"
        );
        assert_eq!(title_from_prompt("Rename foo\nand bar"), "Rename foo");
        assert_eq!(title_from_prompt("   \n "), "Untitled thread");
    }

    #[test]
    fn a_prompt_that_starts_with_a_heading_is_titled_by_it() {
        let brief = "# Cache parsed config files\n\n## Goal\n\nParse each file once.";
        assert_eq!(title_from_prompt(brief), "Cache parsed config files");
        assert_eq!(title_from_request(brief, 0), "Cache parsed config files");
        // Not a heading: an issue number, a tag.
        assert_eq!(title_from_prompt("#123 fails on CI"), "#123 fails on CI");
        assert_eq!(
            title_from_prompt("## Goal\n\nParse each file once."),
            "## Goal"
        );
    }

    #[test]
    fn long_titles_are_cut_at_a_word() {
        let prompt = "Refactor the commit graph renderer so that lanes are allocated lazily and reused across rows";
        let title = title_from_prompt(prompt);
        assert_eq!(
            title,
            "Refactor the commit graph renderer so that lanes are allocated lazily…"
        );
        assert!(title.chars().count() <= 72);

        let word = "x".repeat(100);
        let title = title_from_prompt(&word);
        assert_eq!(title.chars().count(), 72);
        assert!(title.ends_with('…'));

        // Exactly 72 characters stay whole.
        let exact = "a".repeat(72);
        assert_eq!(title_from_prompt(&exact), exact);
    }

    #[test]
    fn a_title_is_a_level_one_heading_on_the_first_line() {
        let message = "# Make the greeting friendlier\n\nThe greeting now says hello.\n";
        assert_eq!(
            split_title(message),
            (
                Some("Make the greeting friendlier".to_owned()),
                "The greeting now says hello.\n"
            )
        );
        // Blank lines (and up to three spaces) before it.
        assert_eq!(
            split_title("\n  \r\n   # Fix the parser #\r\n\r\n\r\nBody\r\nmore"),
            (Some("Fix the parser".to_owned()), "Body\r\nmore")
        );
        // A heading alone.
        assert_eq!(
            split_title("# Rename the module"),
            (Some("Rename the module".to_owned()), "")
        );
        assert_eq!(
            split_title("# Rename the module\n\n  \n"),
            (Some("Rename the module".to_owned()), "")
        );
        // A closing sequence must follow a space; `#` inside stays.
        assert_eq!(
            title_from_message("# Use C# 12 #").as_deref(),
            Some("Use C# 12")
        );
        assert_eq!(title_from_message("# Fix #42").as_deref(), Some("Fix #42"));
        assert_eq!(title_from_message("#\tTabbed").as_deref(), Some("Tabbed"));
    }

    #[test]
    fn anything_else_is_not_a_title() {
        for message in [
            "",
            "   \n\n",
            "Changed the greeting.\n\n# Make the greeting friendlier",
            "## Summary\n\nChanged the greeting.",
            "### Summary",
            "#Hashtag",
            "    # Indented code",
            "#",
            "#   #",
            "# \"\"",
            "# ``",
            "# .",
            "# \u{7}\u{1b}",
        ] {
            assert_eq!(split_title(message), (None, message), "{message:?}");
        }
    }

    #[test]
    fn titles_are_cleaned() {
        let clean = |message: &str| title_from_message(message).expect(message);
        assert_eq!(clean("# \"Add a dark theme.\""), "Add a dark theme");
        assert_eq!(clean("# `Add a dark theme`"), "Add a dark theme");
        assert_eq!(clean("# “Add a dark theme”."), "Add a dark theme");
        assert_eq!(clean("# Add   a\tdark  theme..."), "Add a dark theme");
        // Quotes inside the title stay.
        assert_eq!(clean("# Rename `foo` to 'bar'"), "Rename `foo` to 'bar'");
        assert_eq!(clean("# Don't panic"), "Don't panic");
        // Control and direction-changing characters go; nothing but one
        // plain line is left.
        let title = clean("# Fix\u{1b}[31m red\u{7}\u{0} text\u{202e}evil\u{2066}\u{85}x\u{2028}y");
        assert_eq!(title, "Fix[31m red textevil x y");
        assert!(!title.chars().any(|c| c.is_control() || is_bidi_control(c)));
        assert!(!title.contains(['\n', '\r']));
    }

    #[test]
    fn long_headings_are_cut_like_prompts() {
        let long = "Refactor the commit graph renderer so that lanes are allocated lazily and reused across rows";
        let title = title_from_message(&format!("# {long}\n\nBody")).expect("title");
        assert_eq!(title, title_from_prompt(long));
        assert_eq!(
            title,
            "Refactor the commit graph renderer so that lanes are allocated lazily…"
        );
        let word = format!("# {}", "x".repeat(500));
        assert_eq!(
            title_from_message(&word).expect("title").chars().count(),
            72
        );
    }

    #[test]
    fn titles_in_other_languages_are_kept() {
        assert_eq!(
            title_from_message("# Přidej přátelštější pozdrav\n\nPozdrav je teď vlídnější.")
                .as_deref(),
            Some("Přidej přátelštější pozdrav")
        );
        assert_eq!(
            title_from_message("# 「挨拶をもっと親しみやすくする」。\n\n本文").as_deref(),
            Some("挨拶をもっと親しみやすくする")
        );
        // No spaces to cut at: cut inside, by characters, not bytes.
        let japanese = title_from_message(&format!("# {}", "挨拶".repeat(60))).expect("title");
        assert_eq!(japanese.chars().count(), 72);
        assert!(japanese.ends_with('…'));
    }

    #[test]
    fn the_title_line_is_left_out_only_when_it_is_the_title() {
        let message = "# Make the greeting friendlier\n\nDone.";
        assert_eq!(
            message_without_title(message, "Make the greeting friendlier"),
            "Done."
        );
        assert_eq!(
            message_without_title(message, "Can you look into it"),
            message
        );
        assert_eq!(message_without_title("Done.", "Done"), "Done.");
    }

    #[test]
    fn slugs_are_ascii_words_joined_by_dashes() {
        assert_eq!(
            slug("Add a blame view to the commit detail"),
            "add-a-blame-view-to-the-commit-detail"
        );
        assert_eq!(
            slug("Přidej čeština: žluťoučký kůň"),
            "pridej-cestina-zlutoucky-kun"
        );
        assert_eq!(slug("Fix `foo::bar()` — again!"), "fix-foo-bar-again");
        assert_eq!(slug("Straße & Œuvre"), "strasse-oeuvre");
        assert_eq!(slug("日本語"), "thread");
        assert_eq!(slug("v2 日本 ok"), "v2-ok");
        assert_eq!(slug("…"), "thread");
        assert_eq!(
            slug("Refactor the commit graph renderer so that lanes are allocated lazily"),
            "refactor-the-commit-graph-renderer-so"
        );
        assert_eq!(slug(&"y".repeat(60)), "y".repeat(40));
        for title in ["Add a blame view", "Přidej", "日本語", &"y".repeat(60)] {
            assert!(crate::AgentThreadId::new(slug(title)).is_ok(), "{title}");
        }
    }

    #[test]
    fn repository_keys_are_stable() {
        // The path does not exist, so it is hashed as given; the expected
        // value was computed independently (Python).
        let key = repository_key("my repo", Path::new("/nowhere/my repo/.git"));
        assert_eq!(key, format!("my-repo-{EXPECTED_NOWHERE}"));
        assert_eq!(
            repository_key("Čeština.app", Path::new("/nowhere/my repo/.git")),
            format!("cestina.app-{EXPECTED_NOWHERE}")
        );
        assert_eq!(
            repository_key("..", Path::new("/nowhere/my repo/.git")),
            format!("repository-{EXPECTED_NOWHERE}")
        );
        assert_ne!(
            repository_key("my repo", Path::new("/nowhere/other/.git")),
            key
        );
    }

    #[test]
    fn fnv1a64_matches_the_reference_values() {
        // From the FNV reference test suite.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    /// First 8 hex digits of FNV-1a 64 of `/nowhere/my repo/.git`
    /// (`88ba6ef0e079550c`).
    const EXPECTED_NOWHERE: &str = "88ba6ef0";
}
