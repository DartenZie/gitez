//! What the Settings fields turn text into, and back: host lists, hidden
//! paths, environment variables, launch arguments, and the setup command
//! GitEZ suggests. Pure functions, so they are tested without a window.

use std::collections::BTreeMap;
use std::path::Path;

/// The hosts in `text`, one per line (commas and spaces also separate
/// them), in order and without repeats. A URL is cut down to its host.
pub fn parse_hosts(text: &str) -> Result<Vec<String>, String> {
    let mut hosts: Vec<String> = Vec::new();
    for raw in text.split(|c: char| c == ',' || c.is_whitespace()) {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let host = host_of(raw).ok_or_else(|| {
            format!("“{raw}” is not a host name, such as crates.io or *.example.com.")
        })?;
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    Ok(hosts)
}

/// One host per line, as the allowlist editor shows them.
pub fn format_hosts(hosts: &[String]) -> String {
    hosts.join("\n")
}

/// `raw` as a host name, lowercase: letters, digits and `-` in labels
/// separated by dots, optionally after `*.`. A scheme and a path are
/// dropped; a port is refused (the sandbox allows hosts, not ports).
fn host_of(raw: &str) -> Option<String> {
    let rest = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .unwrap_or(raw);
    let host = rest.split('/').next()?.trim_end_matches('.').to_lowercase();
    let (wildcard, name) = match host.strip_prefix("*.") {
        Some(name) => (true, name),
        None => (false, host.as_str()),
    };
    let labels_ok = !name.is_empty()
        && name.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        });
    (labels_ok && (!wildcard || name.contains('.'))).then_some(host)
}

/// A path to hide from the agent as Settings keeps it: `~/…` inside the
/// home folder `home`, else absolute; no trailing `/`. The home folder
/// itself and `/` are refused: the agent could not run.
pub fn hidden_path(input: &str, home: Option<&Path>) -> Result<String, String> {
    settings_path(
        input,
        home,
        "Hiding the whole home folder or disk would stop the agent from running.",
    )
}

/// A folder the agent may write in every thread, as Settings keeps it (as
/// [`hidden_path`]). The home folder itself and `/` are refused: the
/// sandbox would no longer keep the agent's writes in.
pub fn writable_path(input: &str, home: Option<&Path>) -> Result<String, String> {
    settings_path(
        input,
        home,
        "Letting the agent write the whole home folder or disk would undo the sandbox.",
    )
}

/// `input` as Settings keeps a path; `whole` says why the home folder or
/// `/` is refused.
fn settings_path(input: &str, home: Option<&Path>, whole: &str) -> Result<String, String> {
    let text = input.trim();
    if text.is_empty() {
        return Err("Type a path, or choose one.".to_owned());
    }
    let mut text = text.to_owned();
    if let Some(home) = home.and_then(Path::to_str).filter(|home| !home.is_empty()) {
        if text == home {
            text = "~".to_owned();
        } else if let Some(rest) = text.strip_prefix(home).filter(|rest| rest.starts_with('/')) {
            text = format!("~{rest}");
        }
    }
    while text.len() > 1 && text.ends_with('/') {
        text.pop();
    }
    if text == "~" || text == "/" {
        return Err(whole.to_owned());
    }
    if !(text.starts_with("~/") || text.starts_with('/')) {
        return Err("Give a full path, or one that starts with ~/.".to_owned());
    }
    Ok(text)
}

/// `NAME=value` lines as environment variables. Blank lines and lines
/// starting with `#` are skipped; the value is everything after the first
/// `=`, as typed.
pub fn parse_env(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut env = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let number = index + 1;
        let Some((name, value)) = line.split_once('=') else {
            return Err(format!("Line {number}: write NAME=value."));
        };
        let name = name.trim();
        if !gitez_settings::is_env_name(name) {
            return Err(format!(
                "Line {number}: “{name}” is not an environment variable name (capital letters, \
                 digits and _)."
            ));
        }
        if env.insert(name.to_owned(), value.to_owned()).is_some() {
            return Err(format!("Line {number}: {name} is set twice."));
        }
    }
    Ok(env)
}

/// One `NAME=value` per line.
pub fn format_env(env: &BTreeMap<String, String>) -> String {
    env.iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Arguments as a shell splits them: spaces separate; single quotes keep
/// everything; double quotes keep everything but `\"` and `\\`; a
/// backslash outside quotes keeps the next character. Nothing is expanded.
pub fn split_args(text: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_arg = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_arg = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => current.push(c),
                        None => return Err("A ' quote is not closed.".to_owned()),
                    }
                }
            }
            '"' => {
                in_arg = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\')) => current.push(c),
                            Some(c) => {
                                current.push('\\');
                                current.push(c);
                            }
                            None => return Err("A \" quote is not closed.".to_owned()),
                        },
                        Some(c) => current.push(c),
                        None => return Err("A \" quote is not closed.".to_owned()),
                    }
                }
            }
            '\\' => {
                in_arg = true;
                match chars.next() {
                    Some(c) => current.push(c),
                    None => current.push('\\'),
                }
            }
            c if c.is_whitespace() => {
                if in_arg {
                    args.push(std::mem::take(&mut current));
                    in_arg = false;
                }
            }
            c => {
                in_arg = true;
                current.push(c);
            }
        }
    }
    if in_arg {
        args.push(current);
    }
    Ok(args)
}

/// `args` as one line that [`split_args`] reads back the same.
pub fn join_args(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            let plain = !arg.is_empty()
                && arg.chars().all(|c| {
                    c.is_ascii_alphanumeric()
                        || matches!(c, '-' | '_' | '.' | '/' | '=' | ':' | ',' | '@' | '+')
                });
            if plain {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The setup command GitEZ suggests for a repository whose top folder has
/// the files `exists` says it has: fetching Rust crates, installing npm or
/// pnpm packages from their lock files. Shown only; it never runs unless
/// the user types it.
pub fn guess_setup_command(exists: impl Fn(&str) -> bool) -> Option<String> {
    let mut steps: Vec<&str> = Vec::new();
    if exists("Cargo.toml") {
        steps.push("cargo fetch");
    }
    if exists("pnpm-lock.yaml") {
        steps.push("pnpm install --frozen-lockfile");
    } else if exists("package-lock.json") {
        steps.push("npm ci");
    }
    (!steps.is_empty()).then(|| steps.join(" && "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writable_paths_are_kept_like_hidden_ones_and_never_the_home_folder() {
        let home = Path::new("/Users/me");
        assert_eq!(
            writable_path("/Users/me/.gradle/caches/", Some(home)).expect("valid"),
            "~/.gradle/caches"
        );
        assert_eq!(
            writable_path("/opt/cache", Some(home)).expect("valid"),
            "/opt/cache"
        );
        assert!(writable_path("/Users/me", Some(home)).is_err());
        assert!(writable_path("/", Some(home)).is_err());
        assert!(writable_path("relative/dir", Some(home)).is_err());
    }

    #[test]
    fn hosts_are_read_one_per_line_without_repeats() {
        assert_eq!(
            parse_hosts("crates.io\n\n  index.crates.io  \nCrates.IO\n*.example.com, ghcr.io")
                .expect("valid hosts"),
            ["crates.io", "index.crates.io", "*.example.com", "ghcr.io"]
        );
        assert_eq!(
            parse_hosts("https://registry.npmjs.org/some/package\nhttp://pypi.org.")
                .expect("URLs become hosts"),
            ["registry.npmjs.org", "pypi.org"]
        );
        assert_eq!(parse_hosts("  \n").expect("empty"), Vec::<String>::new());
    }

    #[test]
    fn bad_hosts_are_named() {
        for bad in [
            "crates.io:443",
            "exa_mple.com",
            "-bad.com",
            "*.com",
            "a..b",
            "*",
        ] {
            let error = parse_hosts(&format!("crates.io\n{bad}")).expect_err(bad);
            assert!(error.contains(bad), "{error}");
        }
    }

    #[test]
    fn hosts_round_trip() {
        let hosts: Vec<String> = gitez_settings::DEFAULT_PACKAGE_HOSTS
            .iter()
            .map(|host| (*host).to_owned())
            .collect();
        assert_eq!(parse_hosts(&format_hosts(&hosts)).expect("valid"), hosts);
    }

    #[test]
    fn hidden_paths_are_kept_under_the_home_folder() {
        let home = Some(Path::new("/Users/me"));
        assert_eq!(
            hidden_path("/Users/me/.config/tool/", home).as_deref(),
            Ok("~/.config/tool")
        );
        assert_eq!(hidden_path("  ~/.netrc ", home).as_deref(), Ok("~/.netrc"));
        assert_eq!(
            hidden_path("/opt/secrets", home).as_deref(),
            Ok("/opt/secrets")
        );
        // Not the home folder's own prefix.
        assert_eq!(
            hidden_path("/Users/meg/.ssh", home).as_deref(),
            Ok("/Users/meg/.ssh")
        );
        for bad in ["", "relative/path", "/Users/me", "~/", "/"] {
            assert!(hidden_path(bad, home).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn environment_lines() {
        let env = parse_env("A=1\n\n# a comment\nB = has = signs \nEMPTY=\n").expect("valid");
        assert_eq!(
            env,
            BTreeMap::from([
                ("A".to_owned(), "1".to_owned()),
                ("B".to_owned(), " has = signs ".to_owned()),
                ("EMPTY".to_owned(), String::new()),
            ])
        );
        assert_eq!(parse_env(&format_env(&env)).expect("round trip"), env);
        assert_eq!(
            parse_env("A=1\nlower=2").expect_err("lowercase"),
            "Line 2: “lower” is not an environment variable name (capital letters, digits and _)."
        );
        assert_eq!(
            parse_env("A").expect_err("no ="),
            "Line 1: write NAME=value."
        );
        assert_eq!(
            parse_env("A=1\nA=2").expect_err("twice"),
            "Line 2: A is set twice."
        );
    }

    #[test]
    fn arguments_split_like_a_shell() {
        assert_eq!(
            split_args(
                r#"--max-turns 40  --append-system-prompt "Be brief, \"please\"" 'it''s' a\ b"#
            )
            .expect("valid"),
            [
                "--max-turns",
                "40",
                "--append-system-prompt",
                r#"Be brief, "please""#,
                "its",
                "a b"
            ]
        );
        assert_eq!(split_args("''").expect("an empty argument"), [""]);
        assert_eq!(split_args("   ").expect("none"), Vec::<String>::new());
        assert!(split_args("'open").is_err());
        assert!(split_args("\"open").is_err());
    }

    #[test]
    fn arguments_round_trip() {
        let args: Vec<String> = ["--model", "haiku", "two words", "it's", "", "a\"b", "--x=1"]
            .map(String::from)
            .to_vec();
        let line = join_args(&args);
        assert_eq!(
            line,
            r#"--model haiku 'two words' 'it'\''s' '' 'a"b' --x=1"#
        );
        assert_eq!(split_args(&line).expect("valid"), args);
    }

    #[test]
    fn setup_command_guess() {
        let with = |files: &'static [&'static str]| move |name: &str| files.contains(&name);
        assert_eq!(
            guess_setup_command(with(&["Cargo.toml", "Cargo.lock"])).as_deref(),
            Some("cargo fetch")
        );
        assert_eq!(
            guess_setup_command(with(&["package.json", "package-lock.json"])).as_deref(),
            Some("npm ci")
        );
        assert_eq!(
            guess_setup_command(with(&["package-lock.json", "pnpm-lock.yaml"])).as_deref(),
            Some("pnpm install --frozen-lockfile")
        );
        assert_eq!(
            guess_setup_command(with(&["Cargo.toml", "package-lock.json"])).as_deref(),
            Some("cargo fetch && npm ci")
        );
        assert_eq!(
            guess_setup_command(with(&["package.json", "README.md"])),
            None
        );
    }
}
