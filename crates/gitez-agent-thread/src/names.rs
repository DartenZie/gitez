//! Names derived from a prompt or a repository: a thread's title and slug,
//! and the key of a repository's folder of worktrees.

use std::path::Path;

/// The longest title, in characters, "…" included.
const MAX_TITLE: usize = 72;
/// The longest slug, in characters.
const MAX_SLUG: usize = 40;

/// The agent thread's title: the prompt's first sentence or line, at most 72
/// characters, cut at a word boundary with "…". A sentence ends at `.`,
/// `!`, `?` or `:` followed by a space; the `.` and `:` are left out.
pub fn title_from_prompt(prompt: &str) -> String {
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
    let sentence = line[..end]
        .trim_end_matches(['.', ':'])
        .trim_end()
        .to_owned();
    if sentence.is_empty() {
        return "Untitled thread".to_owned();
    }
    if sentence.chars().count() <= MAX_TITLE {
        return sentence;
    }
    // Room for the "…".
    let cut: String = sentence.chars().take(MAX_TITLE - 1).collect();
    let next_is_space = sentence
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
