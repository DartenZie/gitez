//! `settings.json`: what the user set in GitEZ's Settings, and how it is
//! saved safely.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};

use gitez_agent_thread::Mode;
use gitez_sandbox::NetworkProfile;
use serde::{Deserialize, Serialize};

use crate::{Error, Result, VERSION};

const SETTINGS_FILE: &str = "settings.json";

/// At most this many agents run at once, across all repositories.
pub const DEFAULT_MAX_RUNNING: u32 = 3;

/// The hosts the "Packages" network profile adds to the agent's own hosts
/// (`api.anthropic.com` and `platform.claude.com` for Claude Code, which
/// come from the agent, not from here): package registries and the code
/// hosts they download from.
pub const DEFAULT_PACKAGE_HOSTS: &[&str] = &[
    // Rust
    "crates.io",
    "index.crates.io",
    "static.crates.io",
    "static.rust-lang.org",
    // JavaScript
    "registry.npmjs.org",
    "registry.yarnpkg.com",
    // Python
    "pypi.org",
    "files.pythonhosted.org",
    // GitHub: sources, releases and container images
    "github.com",
    "api.github.com",
    "codeload.github.com",
    "objects.githubusercontent.com",
    "raw.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "ghcr.io",
    "pkg-containers.githubusercontent.com",
    // Go
    "proxy.golang.org",
    "sum.golang.org",
    "storage.googleapis.com",
];

/// Paths the agent cannot read: credentials it has no need for. `~` is the
/// home folder.
pub const DEFAULT_HIDDEN_PATHS: &[&str] = &[
    "~/.ssh",
    "~/.aws",
    "~/.gnupg",
    "~/.config/gh",
    "~/.netrc",
    "~/.kube",
    "~/.docker/config.json",
    "~/.git-credentials",
    "~/.config/git/credentials",
    "~/.npmrc",
    "~/.pypirc",
    "~/.cargo/credentials",
    "~/.cargo/credentials.toml",
];

/// Everything in `settings.json` but its format version.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub threads: ThreadSettings,
    pub sandbox: SandboxSettings,
    /// By agent id, such as `claude-code`. A missing entry is the default.
    pub agents: BTreeMap<String, AgentEntry>,
    pub secrets: Vec<SecretEntry>,
}

/// How agent threads run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadSettings {
    /// At most this many agents run at once, across all repositories.
    /// Default [`DEFAULT_MAX_RUNNING`].
    pub max_running: u32,
    /// The mode of the thread started last, which the composer starts
    /// with. Default: Implement.
    pub last_mode: Mode,
    /// Whether that thread was started as Auto (a Discuss thread whose
    /// agent may hand off to an Implement thread): the composer then starts
    /// with Auto. Kept apart from `last_mode`, which stays Discuss, so a
    /// GitEZ without Auto still reads the file. Written only when set.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub last_auto: bool,
}

/// What the sandbox of an agent thread allows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxSettings {
    /// The network profile a new thread starts with.
    pub default_network: NetworkProfile,
    /// Whether a new thread starts with local ports open (its agent may
    /// listen on and connect to ports on this computer's localhost; see
    /// `gitez_sandbox::Policy::local_ports`). Default: off. Written only
    /// when set.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub default_local_ports: bool,
    /// The hosts "Packages" adds. Default: [`DEFAULT_PACKAGE_HOSTS`].
    pub package_hosts: Vec<String>,
    /// Paths hidden from the agent, `~` for the home folder. Default:
    /// [`DEFAULT_HIDDEN_PATHS`].
    pub hidden_paths: Vec<String>,
    /// Extra folders (or files) the agent may write in every thread, `~`
    /// for the home folder, for tools whose caches GitEZ does not give
    /// each thread itself (for example `~/.gradle/caches`). What is
    /// written there is shared with programs that run outside the
    /// sandbox. GitEZ's own denies (the repository's hooks and config,
    /// GitEZ's settings) still win. Default: none.
    pub writable_paths: Vec<String>,
}

/// How one agent is run: an agent entry in Settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentEntry {
    /// The binary; `None`: look on the PATH and in the usual places.
    pub program: Option<PathBuf>,
    /// Claude Code: `CLAUDE_CONFIG_DIR`. `None`: the agent's default.
    pub config_dir: Option<PathBuf>,
    /// Set for the agent, on top of what GitEZ sets.
    pub env: BTreeMap<String, String>,
    /// Added after GitEZ's own arguments.
    pub args: Vec<String>,
    /// Default model alias; `None`: the agent's default.
    pub model: Option<String>,
    /// Default effort level, such as `high`; `None`: the agent's default
    /// for the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// A secret's name and scope. Its value is in the OS keychain, under
/// [`SecretEntry::account`], never here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretEntry {
    /// An environment variable name, `[A-Z_][A-Z0-9_]*`.
    pub name: String,
    pub scope: SecretScope,
    /// Given to new threads by default.
    pub default: bool,
}

/// Which repositories a secret is for.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretScope {
    All,
    /// This repository only, by its common git directory.
    Repository(PathBuf),
}

/// What [`Settings::load`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loaded {
    Current(Settings),
    /// Written by a newer GitEZ: use `Settings::default()` and never save.
    TooNew,
}

/// `settings.json` as written: the settings with the format version.
#[derive(Serialize)]
struct OnDisk<'a> {
    version: u32,
    #[serde(flatten)]
    settings: &'a Settings,
}

/// Only the version, read before the rest so a newer format is refused
/// rather than misread.
#[derive(Deserialize)]
struct VersionOnly {
    version: u32,
}

impl Default for ThreadSettings {
    fn default() -> Self {
        Self {
            max_running: DEFAULT_MAX_RUNNING,
            last_mode: Mode::default(),
            last_auto: false,
        }
    }
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            default_network: NetworkProfile::default(),
            default_local_ports: false,
            package_hosts: strings(DEFAULT_PACKAGE_HOSTS),
            hidden_paths: strings(DEFAULT_HIDDEN_PATHS),
            writable_paths: Vec::new(),
        }
    }
}

impl Settings {
    /// `<data_dir>/settings.json`.
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(SETTINGS_FILE)
    }

    /// Reads the settings at `path`. A missing file is
    /// `Current(Settings::default())`; missing fields take their defaults.
    pub fn load(path: &Path) -> Result<Loaded> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(Loaded::Current(Settings::default()));
            }
            Err(error) => return Err(Error::io(path)(error)),
        };
        let invalid = |source| Error::Invalid {
            path: path.to_owned(),
            source,
        };
        let VersionOnly { version } = serde_json::from_str(&text).map_err(invalid)?;
        if version > VERSION {
            return Ok(Loaded::TooNew);
        }
        serde_json::from_str(&text)
            .map(Loaded::Current)
            .map_err(invalid)
    }

    /// Writes the settings to `path`. The new content goes to
    /// `settings.json.lock`, created exclusively, which is then renamed over
    /// `settings.json`, so the file is never half written and two writers
    /// cannot interleave. Refuses (`TooNew`) to overwrite a newer version.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            fs::create_dir_all(dir).map_err(Error::io(dir))?;
        }
        let lock_path = lock_path(path);
        let lock = Lock::take(&lock_path)?;

        // Never overwrite what a newer GitEZ wrote.
        match fs::read_to_string(path) {
            Ok(text) => {
                if let Ok(VersionOnly { version }) = serde_json::from_str(&text)
                    && version > VERSION
                {
                    return Err(Error::TooNew {
                        path: path.to_owned(),
                        version,
                    });
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(path)(error)),
        }

        let mut text = serde_json::to_string_pretty(&OnDisk {
            version: VERSION,
            settings: self,
        })
        .map_err(|source| Error::Invalid {
            path: path.to_owned(),
            source,
        })?;
        text.push('\n');
        let mut file = &lock.file;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(Error::io(&lock_path))?;
        lock.commit(path)
    }

    /// The agent entry for `id`, or the default entry.
    pub fn agent(&self, id: &str) -> AgentEntry {
        self.agents.get(id).cloned().unwrap_or_default()
    }

    /// The secrets a thread of the repository at `common_dir` can be given,
    /// one per name: a repository-scoped entry hides an all-repositories
    /// entry of the same name. Sorted by name.
    pub fn secrets_for(&self, common_dir: &Path) -> Vec<&SecretEntry> {
        let mut chosen: BTreeMap<&str, &SecretEntry> = BTreeMap::new();
        let of_repository = self.secrets.iter().filter(
            |entry| matches!(&entry.scope, SecretScope::Repository(dir) if dir == common_dir),
        );
        let of_all = self
            .secrets
            .iter()
            .filter(|entry| entry.scope == SecretScope::All);
        for entry in of_repository.chain(of_all) {
            chosen.entry(entry.name.as_str()).or_insert(entry);
        }
        chosen.into_values().collect()
    }

    /// `hidden_paths` with `~` expanded. Paths that are not absolute after
    /// expansion (or start with `~` when there is no home folder) are left
    /// out: a sandbox rule needs an absolute path.
    pub fn hidden_paths(&self) -> Vec<PathBuf> {
        expand_all(&self.sandbox.hidden_paths, dirs::home_dir().as_deref())
    }

    /// `writable_paths` with `~` expanded, absolute paths only (as
    /// [`Settings::hidden_paths`]).
    pub fn writable_paths(&self) -> Vec<PathBuf> {
        expand_all(&self.sandbox.writable_paths, dirs::home_dir().as_deref())
    }
}

impl SecretEntry {
    /// The keychain account: `all/<NAME>`, or `repo:<common dir>/<NAME>`.
    pub fn account(&self) -> String {
        match &self.scope {
            SecretScope::All => format!("all/{}", self.name),
            SecretScope::Repository(dir) => format!("repo:{}/{}", dir.display(), self.name),
        }
    }
}

impl AgentEntry {
    /// The entry as the agent takes it. `model` and `effort` are not part
    /// of it: they are chosen per run.
    pub fn config(&self) -> gitez_agent::AgentConfig {
        gitez_agent::AgentConfig {
            program: self.program.clone(),
            config_dir: self.config_dir.clone(),
            env: self
                .env
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            args: self.args.clone(),
        }
    }
}

/// Whether `name` can be an environment variable name: `[A-Z_][A-Z0-9_]*`.
pub fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('A'..='Z' | '_'))
        && chars.all(|c| matches!(c, 'A'..='Z' | '0'..='9' | '_'))
}

/// The shortest secret value GitEZ can keep out of what it writes
/// (transcripts, logs, pull request descriptions): it replaces every
/// occurrence of a value, and of each line of a multi-line value, of at
/// least this many characters. A shorter value would match ordinary words
/// and numbers, and replacing those would garble the agent's output.
pub const MIN_SECRET_LEN: usize = 4;

/// Why `value` cannot be a secret's value, said for the user, or `None`
/// when it can. Settings must refuse such a value: GitEZ could not keep it
/// out of transcripts. Lines of a multi-line value (a key file) that are
/// shorter than [`MIN_SECRET_LEN`] are not replaced; the rest of it is.
pub fn secret_value_problem(value: &str) -> Option<String> {
    let value = value.trim_end_matches(['\r', '\n']);
    if value.trim().is_empty() {
        return Some("The value is empty.".to_owned());
    }
    (value.chars().count() < MIN_SECRET_LEN).then(|| {
        format!(
            "The value is shorter than {MIN_SECRET_LEN} characters, too short for GitEZ to keep \
             it out of what agents print."
        )
    })
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|&value| value.to_owned()).collect()
}

/// `<path>.lock`, beside `path`.
fn lock_path(path: &Path) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(".lock");
    PathBuf::from(name)
}

fn expand_all(paths: &[String], home: Option<&Path>) -> Vec<PathBuf> {
    paths
        .iter()
        .filter_map(|path| expand(path, home))
        .filter(|path| path.is_absolute())
        .collect()
}

/// `path` with a leading `~` as `home`.
fn expand(path: &str, home: Option<&Path>) -> Option<PathBuf> {
    if path == "~" {
        home.map(Path::to_owned)
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.map(|home| home.join(rest))
    } else {
        Some(PathBuf::from(path))
    }
}

/// An exclusively created lock file, removed on drop unless renamed over
/// its target.
struct Lock {
    path: PathBuf,
    file: File,
    committed: bool,
}

impl Lock {
    fn take(path: &Path) -> Result<Self> {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(file) => Ok(Self {
                path: path.to_owned(),
                file,
                committed: false,
            }),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => Err(Error::Locked {
                path: path.to_owned(),
            }),
            Err(error) => Err(Error::io(path)(error)),
        }
    }

    /// Renames the lock file, with what was written to it, to `target`.
    fn commit(mut self, target: &Path) -> Result<()> {
        fs::rename(&self.path, target).map_err(Error::io(target))?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if !self.committed {
            // Best effort: a lock left behind is reported on the next save.
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(name: &str, scope: SecretScope, default: bool) -> SecretEntry {
        SecretEntry {
            name: name.to_owned(),
            scope,
            default,
        }
    }

    fn customised() -> Settings {
        let mut settings = Settings::default();
        settings.threads.max_running = 5;
        settings.threads.last_mode = Mode::Discuss;
        settings.sandbox.default_network = NetworkProfile::Packages;
        settings.sandbox.default_local_ports = true;
        settings.sandbox.package_hosts = vec!["crates.io".to_owned()];
        settings.sandbox.hidden_paths = vec!["~/.ssh".to_owned()];
        settings.sandbox.writable_paths = vec!["~/.gradle/caches".to_owned()];
        settings.agents.insert(
            "claude-code".to_owned(),
            AgentEntry {
                program: Some("/opt/claude/bin/claude".into()),
                config_dir: Some("/Users/me/.claude-work".into()),
                env: BTreeMap::from([("A".to_owned(), "1".to_owned())]),
                args: vec!["--verbose".to_owned()],
                model: Some("haiku".to_owned()),
                effort: Some("high".to_owned()),
            },
        );
        settings.secrets = vec![
            secret("GITHUB_TOKEN", SecretScope::All, true),
            secret(
                "SENTRY_DSN",
                SecretScope::Repository("/Users/me/repos/gitez/.git".into()),
                false,
            ),
        ];
        settings
    }

    #[test]
    fn defaults() {
        let settings = Settings::default();
        assert_eq!(settings.threads.max_running, 3);
        assert_eq!(settings.threads.last_mode, Mode::Implement);
        assert_eq!(settings.sandbox.default_network, NetworkProfile::AgentOnly);
        assert_eq!(
            settings.sandbox.package_hosts,
            strings(DEFAULT_PACKAGE_HOSTS)
        );
        assert_eq!(settings.sandbox.hidden_paths, strings(DEFAULT_HIDDEN_PATHS));
        assert!(settings.sandbox.writable_paths.is_empty());
        assert!(!settings.sandbox.default_local_ports);
        assert!(settings.writable_paths().is_empty());
        assert!(settings.agents.is_empty());
        assert!(settings.secrets.is_empty());
        assert_eq!(settings.agent("claude-code"), AgentEntry::default());
    }

    #[test]
    fn default_hosts_and_hidden_paths() {
        for host in ["crates.io", "registry.npmjs.org", "pypi.org", "github.com"] {
            assert!(DEFAULT_PACKAGE_HOSTS.contains(&host), "{host}");
        }
        // The agent's own API comes from the agent, not from this list.
        assert!(!DEFAULT_PACKAGE_HOSTS.contains(&"api.anthropic.com"));
        for path in [
            "~/.ssh",
            "~/.aws",
            "~/.gnupg",
            "~/.config/gh",
            "~/.netrc",
            "~/.kube",
            "~/.docker/config.json",
        ] {
            assert!(DEFAULT_HIDDEN_PATHS.contains(&path), "{path}");
        }
        // Claude Code reads its own configuration folder; it must stay visible.
        assert!(
            !DEFAULT_HIDDEN_PATHS
                .iter()
                .any(|path| path.contains("claude"))
        );
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        let dir = tempfile::tempdir().expect("temporary folder");
        let path = Settings::path(dir.path());
        assert_eq!(path, dir.path().join("settings.json"));
        assert_eq!(
            Settings::load(&path).expect("load"),
            Loaded::Current(Settings::default())
        );
    }

    #[test]
    fn missing_fields_take_their_defaults() {
        let dir = tempfile::tempdir().expect("temporary folder");
        let path = Settings::path(dir.path());
        fs::write(
            &path,
            r#"{"version":1,"threads":{},"sandbox":{"default_network":"open"}}"#,
        )
        .expect("write");
        let Loaded::Current(settings) = Settings::load(&path).expect("load") else {
            panic!("version 1 is current");
        };
        assert_eq!(settings.threads.max_running, DEFAULT_MAX_RUNNING);
        assert_eq!(settings.sandbox.default_network, NetworkProfile::Open);
        assert_eq!(
            settings.sandbox.package_hosts,
            strings(DEFAULT_PACKAGE_HOSTS)
        );
        assert!(
            settings.sandbox.writable_paths.is_empty(),
            "a file written before writable_paths existed has none"
        );
        assert!(
            !settings.sandbox.default_local_ports,
            "a file written before local ports existed keeps them closed"
        );
    }

    #[test]
    fn local_ports_are_written_only_when_open() {
        let dir = tempfile::tempdir().expect("temporary folder");
        let path = Settings::path(dir.path());
        let mut settings = Settings::default();
        settings.save(&path).expect("save");
        let text = fs::read_to_string(&path).expect("read");
        assert!(!text.contains("default_local_ports"), "{text}");

        settings.sandbox.default_local_ports = true;
        settings.save(&path).expect("save");
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(json["sandbox"]["default_local_ports"], true);
        assert_eq!(
            Settings::load(&path).expect("load"),
            Loaded::Current(settings)
        );
    }

    #[test]
    fn auto_is_remembered_beside_the_mode() {
        let dir = tempfile::tempdir().expect("temporary folder");
        let path = Settings::path(dir.path());
        // A file written before Auto existed.
        fs::write(
            &path,
            r#"{"version":1,"threads":{"max_running":2,"last_mode":"discuss"}}"#,
        )
        .expect("write");
        let Loaded::Current(mut settings) = Settings::load(&path).expect("load") else {
            panic!("version 1 is current");
        };
        assert_eq!(settings.threads.last_mode, Mode::Discuss);
        assert!(!settings.threads.last_auto);

        // Not written while unset, so such files stay as they were.
        settings.save(&path).expect("save");
        let text = fs::read_to_string(&path).expect("read");
        assert!(!text.contains("last_auto"), "{text}");

        // Auto: the mode stays Discuss, which a GitEZ without Auto reads.
        settings.threads.last_auto = true;
        settings.save(&path).expect("save");
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(json["threads"]["last_mode"], "discuss");
        assert_eq!(json["threads"]["last_auto"], true);
        assert_eq!(
            Settings::load(&path).expect("load"),
            Loaded::Current(settings)
        );
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().expect("temporary folder");
        let path = Settings::path(&dir.path().join("nested"));
        let settings = customised();
        settings.save(&path).expect("save");
        assert_eq!(
            Settings::load(&path).expect("load"),
            Loaded::Current(settings.clone())
        );
        assert!(!lock_path(&path).exists(), "the lock is gone after saving");

        // The format of C.5.
        let text = fs::read_to_string(&path).expect("read");
        assert!(text.starts_with("{\n  \"version\": 1,"), "{text}");
        let json: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(json["threads"]["max_running"], 5);
        assert_eq!(json["threads"]["last_mode"], "discuss");
        assert_eq!(json["sandbox"]["default_network"], "packages");
        assert_eq!(json["sandbox"]["writable_paths"][0], "~/.gradle/caches");
        assert_eq!(json["secrets"][0]["scope"], "all");
        assert_eq!(
            json["secrets"][1]["scope"]["repository"],
            "/Users/me/repos/gitez/.git"
        );
        assert_eq!(json["agents"]["claude-code"]["model"], "haiku");
        assert_eq!(json["agents"]["claude-code"]["effort"], "high");

        // Saving again replaces the file.
        let mut changed = settings;
        changed.threads.max_running = 1;
        changed.save(&path).expect("save again");
        assert_eq!(
            Settings::load(&path).expect("load"),
            Loaded::Current(changed)
        );
    }

    #[test]
    fn a_newer_version_is_refused_and_not_overwritten() {
        let dir = tempfile::tempdir().expect("temporary folder");
        let path = Settings::path(dir.path());
        let newer = r#"{"version":2,"threads":{"max_running":"many"}}"#;
        fs::write(&path, newer).expect("write");
        assert_eq!(Settings::load(&path).expect("load"), Loaded::TooNew);
        match Settings::default().save(&path) {
            Err(Error::TooNew {
                path: refused,
                version,
            }) => {
                assert_eq!(refused, path);
                assert_eq!(version, 2);
            }
            other => panic!("expected TooNew, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&path).expect("read"), newer);
        assert!(!lock_path(&path).exists(), "the lock is removed");
    }

    #[test]
    fn a_lock_file_means_locked() {
        let dir = tempfile::tempdir().expect("temporary folder");
        let path = Settings::path(dir.path());
        Settings::default().save(&path).expect("save");
        let before = fs::read_to_string(&path).expect("read");
        let lock = lock_path(&path);
        assert_eq!(lock, dir.path().join("settings.json.lock"));
        fs::write(&lock, "").expect("lock");
        match customised().save(&path) {
            Err(Error::Locked { path: locked }) => assert_eq!(locked, lock),
            other => panic!("expected Locked, got {other:?}"),
        }
        assert!(lock.exists(), "another writer's lock is left alone");
        assert_eq!(fs::read_to_string(&path).expect("read"), before);
    }

    #[test]
    fn invalid_settings() {
        let dir = tempfile::tempdir().expect("temporary folder");
        let path = Settings::path(dir.path());
        fs::write(&path, "{ not json").expect("write");
        assert!(matches!(Settings::load(&path), Err(Error::Invalid { .. })));
        fs::write(&path, r#"{"threads":{}}"#).expect("write");
        assert!(
            matches!(Settings::load(&path), Err(Error::Invalid { .. })),
            "a file without a version is not GitEZ settings"
        );
    }

    #[test]
    fn short_secret_values_are_refused() {
        assert!(secret_value_problem("").is_some());
        assert!(secret_value_problem(" \n").is_some());
        assert!(secret_value_problem("abc").is_some());
        assert!(secret_value_problem("abc\n").is_some());
        assert_eq!(secret_value_problem("abcd"), None);
        assert_eq!(secret_value_problem("-----BEGIN KEY-----\nab\n"), None);
    }

    #[test]
    fn a_repository_secret_hides_an_all_repositories_one() {
        let gitez = PathBuf::from("/repos/gitez/.git");
        let other = PathBuf::from("/repos/other/.git");
        let settings = Settings {
            secrets: vec![
                secret("TOKEN", SecretScope::All, true),
                secret("ZED", SecretScope::All, false),
                secret("TOKEN", SecretScope::Repository(gitez.clone()), false),
                secret("ONLY_OTHER", SecretScope::Repository(other.clone()), true),
                secret("API_KEY", SecretScope::All, false),
            ],
            ..Settings::default()
        };

        let for_gitez = settings.secrets_for(&gitez);
        let names: Vec<&str> = for_gitez.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["API_KEY", "TOKEN", "ZED"]);
        assert_eq!(for_gitez[1].scope, SecretScope::Repository(gitez.clone()));
        assert!(!for_gitez[1].default);

        let for_other = settings.secrets_for(&other);
        let names: Vec<&str> = for_other.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["API_KEY", "ONLY_OTHER", "TOKEN", "ZED"]);
        assert_eq!(for_other[2].scope, SecretScope::All);
    }

    #[test]
    fn accounts() {
        assert_eq!(
            secret("GITHUB_TOKEN", SecretScope::All, true).account(),
            "all/GITHUB_TOKEN"
        );
        assert_eq!(
            secret(
                "SENTRY_DSN",
                SecretScope::Repository("/Users/me/repos/gitez/.git".into()),
                false
            )
            .account(),
            "repo:/Users/me/repos/gitez/.git/SENTRY_DSN"
        );
    }

    #[test]
    fn env_names() {
        for name in ["A", "_", "GITHUB_TOKEN", "_X1", "A1_B2"] {
            assert!(is_env_name(name), "{name}");
        }
        for name in ["", "1A", "lower", "A-B", "A B", "A=B", "É", "Aé"] {
            assert!(!is_env_name(name), "{name}");
        }
    }

    #[test]
    fn hidden_paths_expand_the_home_folder() {
        let home = Path::new("/Users/me");
        let paths = strings(&["~", "~/.ssh", "/etc/secret", "relative", "~other/x", ""]);
        assert_eq!(
            expand_all(&paths, Some(home)),
            [
                PathBuf::from("/Users/me"),
                PathBuf::from("/Users/me/.ssh"),
                PathBuf::from("/etc/secret"),
            ]
        );
        assert_eq!(expand_all(&paths, None), [PathBuf::from("/etc/secret")]);
        // The real home folder is only read, never written.
        let expanded = Settings::default().hidden_paths();
        assert!(expanded.iter().all(|path| path.is_absolute()));
        let mut settings = Settings::default();
        settings.sandbox.writable_paths = strings(&["~/.gradle/caches", "relative", "/opt/cache"]);
        let writable = settings.writable_paths();
        assert_eq!(writable.len(), 2, "{writable:?}");
        assert!(writable.iter().all(|path| path.is_absolute()));
        assert_eq!(writable[1], PathBuf::from("/opt/cache"));
    }

    #[test]
    fn agent_entry_config() {
        let settings = customised();
        let config = settings.agent("claude-code").config();
        assert_eq!(config.program, Some("/opt/claude/bin/claude".into()));
        assert_eq!(config.config_dir, Some("/Users/me/.claude-work".into()));
        assert_eq!(config.args, ["--verbose"]);

        let mut entry = AgentEntry::default();
        entry.env.insert("ZED".to_owned(), "z".to_owned());
        entry.env.insert("ALPHA".to_owned(), "a".to_owned());
        assert_eq!(
            entry.config().env,
            [
                ("ALPHA".to_owned(), "a".to_owned()),
                ("ZED".to_owned(), "z".to_owned())
            ]
        );
        assert_eq!(
            AgentEntry::default().config(),
            gitez_agent::AgentConfig::default()
        );
    }
}
