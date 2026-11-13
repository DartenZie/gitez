//! The secret values a thread is given: passed from the GUI to the
//! supervisor on standard input, put in the agent's environment, and
//! replaced by `[secret NAME]` in everything GitEZ writes.

use std::collections::BTreeMap;
use std::io::Read as _;

use crate::{Error, Result};

/// The format version of the JSON on the supervisor's standard input.
const VERSION: u32 = 1;
/// The most the supervisor reads from its standard input.
const MAX_INPUT: u64 = 1024 * 1024;
/// Values shorter than this are not redacted: they would match ordinary
/// words and numbers.
const MIN_REDACTED: usize = 6;

/// Secret values by environment variable name. `Debug` prints names only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secrets(BTreeMap<String, String>);

#[derive(serde::Deserialize)]
struct Input {
    version: u32,
    #[serde(default)]
    secrets: BTreeMap<String, String>,
}

#[derive(serde::Serialize)]
struct Output<'a> {
    version: u32,
    secrets: &'a BTreeMap<String, String>,
}

impl Secrets {
    pub fn new(values: BTreeMap<String, String>) -> Self {
        Self(values)
    }

    /// The names, sorted.
    pub fn names(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }

    /// `(name, value)` pairs for a command's environment.
    pub fn env(&self) -> Vec<(String, String)> {
        self.0
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `line` with every value of 6 or more characters replaced by
    /// `[secret NAME]`, also in its JSON-escaped form.
    pub fn redact(&self, line: &str) -> String {
        let mut forms: Vec<(String, &str)> = Vec::new();
        for (name, value) in &self.0 {
            if value.chars().count() < MIN_REDACTED {
                continue;
            }
            forms.push((value.clone(), name));
            let escaped = json_escaped(value);
            if escaped != *value {
                forms.push((escaped, name));
            }
        }
        forms.retain(|(form, _)| line.contains(form.as_str()));
        if forms.is_empty() {
            return line.to_owned();
        }
        // Longest first, so a value that contains another is replaced whole.
        forms.sort_by_key(|form| std::cmp::Reverse(form.0.len()));
        // One pass from left to right, so a marker is never searched again
        // (a value may occur in another secret's name).
        let mut redacted = String::with_capacity(line.len());
        let mut rest = line;
        'scan: while let Some(next) = rest.chars().next() {
            for (form, name) in &forms {
                if let Some(after) = rest.strip_prefix(form.as_str()) {
                    redacted.push_str(&format!("[secret {name}]"));
                    rest = after;
                    continue 'scan;
                }
            }
            redacted.push(next);
            rest = &rest[next.len_utf8()..];
        }
        redacted
    }

    /// Reads `{"version":1,"secrets":{…}}` from `reader` until EOF (at most
    /// 1 MiB). Empty input: no secrets. Error messages never quote a value.
    pub fn read_from(reader: impl std::io::Read) -> Result<Self> {
        let mut bytes = Vec::new();
        reader
            .take(MAX_INPUT + 1)
            .read_to_end(&mut bytes)
            .map_err(Error::io("standard input"))?;
        if bytes.len() as u64 > MAX_INPUT {
            return Err(Error::Usage(
                "The secrets passed to the supervisor are larger than 1 MiB.".to_owned(),
            ));
        }
        let Ok(text) = String::from_utf8(bytes) else {
            return Err(Error::Usage(
                "The secrets passed to the supervisor are not UTF-8.".to_owned(),
            ));
        };
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        // serde's messages can quote what they could not read; only the
        // position is reported.
        let input: Input = serde_json::from_str(&text).map_err(|error| {
            Error::Usage(format!(
                "The secrets passed to the supervisor are not valid JSON (line {}, column {}).",
                error.line(),
                error.column()
            ))
        })?;
        if input.version > VERSION {
            return Err(Error::Usage(format!(
                "The secrets were passed by a newer GitEZ (format version {}).",
                input.version
            )));
        }
        if let Some(name) = input
            .secrets
            .keys()
            .find(|name| !gitez_settings::is_env_name(name))
        {
            return Err(Error::Usage(format!(
                "“{name}” is not a valid name for a secret."
            )));
        }
        Ok(Self(input.secrets))
    }

    pub fn write_to(&self, mut writer: impl std::io::Write) -> std::io::Result<()> {
        serde_json::to_writer(
            &mut writer,
            &Output {
                version: VERSION,
                secrets: &self.0,
            },
        )?;
        writer.flush()
    }
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Secrets").field(&self.names()).finish()
    }
}

/// `value` as it appears inside a JSON string.
fn json_escaped(value: &str) -> String {
    let quoted = serde_json::to_string(value).expect("a string serialises");
    quoted[1..quoted.len() - 1].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets(pairs: &[(&str, &str)]) -> Secrets {
        Secrets::new(
            pairs
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        )
    }

    #[test]
    fn redacts_values_and_their_json_form() {
        let secrets = secrets(&[
            ("TOKEN", "abc123def"),
            ("QUOTED", "pa\"ss\\word"),
            ("SHORT", "abc"),
        ]);
        assert_eq!(
            secrets.redact("token abc123def, short abc"),
            "token [secret TOKEN], short abc"
        );
        let json = serde_json::json!({ "text": "pa\"ss\\word" }).to_string();
        assert_eq!(secrets.redact(&json), r#"{"text":"[secret QUOTED]"}"#);
        assert_eq!(secrets.redact("pa\"ss\\word"), "[secret QUOTED]");
    }

    #[test]
    fn a_longer_value_wins_over_one_it_contains() {
        let secrets = secrets(&[("A", "secret"), ("B", "secret-and-more")]);
        assert_eq!(
            secrets.redact("x secret-and-more y secret"),
            "x [secret B] y [secret A]"
        );
    }

    #[test]
    fn round_trips_through_json() {
        let secrets = secrets(&[("GITHUB_TOKEN", "ghp_1234567"), ("API_KEY", "k\"ey\n")]);
        let mut out = Vec::new();
        secrets.write_to(&mut out).expect("write");
        assert_eq!(Secrets::read_from(out.as_slice()).expect("read"), secrets);
    }

    #[test]
    fn empty_input_is_no_secrets() {
        assert!(Secrets::read_from(&b""[..]).expect("read").is_empty());
        assert!(Secrets::read_from(&b" \n"[..]).expect("read").is_empty());
    }

    #[test]
    fn bad_input_is_refused_without_quoting_it() {
        let error = Secrets::read_from(&br#"{"version":1,"secrets":{"A":12345678}}"#[..])
            .expect_err("a number is not a value");
        assert!(!error.to_string().contains("12345678"), "{error}");
        let error =
            Secrets::read_from(&br#"{"version":2,"secrets":{}}"#[..]).expect_err("newer version");
        assert!(error.to_string().contains("newer"), "{error}");
        Secrets::read_from(&br#"{"version":1,"secrets":{"bad name":"x"}}"#[..])
            .expect_err("bad name");
        let big = vec![b' '; (MAX_INPUT + 10) as usize];
        Secrets::read_from(big.as_slice()).expect_err("too big");
    }

    #[test]
    fn debug_shows_names_only() {
        let secrets = secrets(&[("TOKEN", "hunter2-secret")]);
        let debug = format!("{secrets:?}");
        assert!(debug.contains("TOKEN"));
        assert!(!debug.contains("hunter2"));
    }
}
