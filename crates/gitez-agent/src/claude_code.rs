//! Claude Code, run headless: `claude --print` with the prompt on standard
//! input and its stream-JSON events on standard output. A prompt with
//! images goes in as one stream-JSON user message instead
//! (`--input-format stream-json`), whose content holds the images as base64
//! blocks and then the text; standard input is closed after it, so the run
//! ends with that turn as a plain prompt's does.
//!
//! What a sandbox must let it write was measured with `claude` 2.1.323
//! under `srt` on macOS: per-project and per-session folders inside its
//! configuration folder, its temporary and cache folders, and a lock
//! folder. Never the whole configuration folder: `.claude.json`, settings,
//! hooks, commands, plugins and shell snapshots there run code in the
//! user's own later sessions.

use std::borrow::Cow;
use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};

use crate::login_shell::{join_path, login_shell_var};
use crate::process::find_program;
use crate::{
    Agent, AgentConfig, AgentEvent, AgentNeeds, CommandSpec, ConfigDirChoice, Conversation, Error,
    Finished, LoginStatus, ModelChoice, OptionalHost, Result, RunSpec, UsageLimit,
};

const PROGRAM: &str = "claude";

/// Arguments of every run. It cannot ask for permission while running
/// headless, so it runs with all permissions (decided with the owner,
/// see CLAUDE.md).
const ARGS: [&str; 6] = [
    "--print",
    "--output-format",
    "stream-json",
    "--verbose",
    "--permission-mode",
    "bypassPermissions",
];

/// Claude Code's own Bash sandbox cannot start inside GitEZ's sandbox
/// (nested Seatbelt fails), so every run turns it off. Flag settings win
/// over the user's settings.
const SANDBOX_OFF: [&str; 2] = ["--settings", r#"{"sandbox":{"enabled":false}}"#];

/// Only for a run with images: the prompt is one stream-JSON message.
const STREAM_INPUT: [&str; 2] = ["--input-format", "stream-json"];

/// What an image's base64 data is replaced with in a transcript.
const IMAGE_OMITTED: &str = "[image omitted by GitEZ]";
/// Shorter runs of base64 after `"data":"` are kept in a transcript.
const OMIT_FROM: usize = 256;

/// Variables a Claude Code session sets for the commands it runs. GitEZ
/// may itself have been started from one; the agent must not think it is
/// nested inside it.
const ENV_REMOVE: [&str; 3] = [
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SSE_PORT",
];

const CONFIG_DIR_VAR: &str = "CLAUDE_CONFIG_DIR";
/// Where Claude Code keeps its temporary files; `/tmp` when unset.
const TMPDIR_VAR: &str = "CLAUDE_CODE_TMPDIR";

/// Hosts every run needs: the API, and the login refresh.
const HOSTS: [&str; 2] = ["api.anthropic.com", "platform.claude.com"];

/// Hosts Claude Code contacts in every run that a thread does not need,
/// as seen in real sandboxed runs: the proxy for claude.ai connectors (MCP
/// servers such as Gmail or Drive, its `MCP_PROXY_URL`) and the log intake
/// its telemetry is sent to. Both stay blocked.
const OPTIONAL_HOSTS: [OptionalHost; 2] = [
    OptionalHost {
        host: "mcp-proxy.anthropic.com",
        purpose: "Claude Code's claude.ai connectors, not needed",
    },
    OptionalHost {
        host: "http-intake.logs.us5.datadoghq.com",
        purpose: "Claude Code's telemetry, not needed",
    },
];

/// How long `claude auth status` may take.
const LOGIN_STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// Claude Code shortens sanitised paths longer than this, adding a hash.
const MAX_SANITISED_LEN: usize = 200;

const MODELS: [ModelChoice; 4] = [
    ("fable", "Fable"),
    ("opus", "Opus"),
    ("sonnet", "Sonnet"),
    ("haiku", "Haiku"),
];

/// Claude Code's command-line agent.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeCode;

/// The Claude configuration folder a run uses, and where GitEZ found it.
/// The login is kept per folder, so a run in the wrong one is logged out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDir {
    pub path: PathBuf,
    pub source: ConfigDirSource,
}

/// Where the Claude configuration folder came from, in the order GitEZ
/// looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigDirSource {
    /// Chosen by GitEZ ([`ConfigDirChoice::Folder`]): an account, or the
    /// agent entry in Settings.
    Settings,
    /// `CLAUDE_CONFIG_DIR` in GitEZ's own environment.
    Environment,
    /// `CLAUDE_CONFIG_DIR` in the user's login shell.
    LoginShell,
    /// Claude Code's default, `~/.claude`, with `CLAUDE_CONFIG_DIR` unset.
    Default,
}

impl ConfigDir {
    /// The folder as a choice that runs in it again whatever GitEZ's
    /// environment is then: [`ConfigDirChoice::Default`] for Claude
    /// Code's default (the variable unset), else this folder.
    pub fn choice(&self) -> ConfigDirChoice {
        match self.source {
            ConfigDirSource::Default => ConfigDirChoice::Default,
            _ => ConfigDirChoice::Folder(self.path.clone()),
        }
    }

    /// The value of `CLAUDE_CONFIG_DIR` for it; `None`: unset.
    pub fn variable(&self) -> Option<&Path> {
        (self.source != ConfigDirSource::Default).then_some(self.path.as_path())
    }
}

/// The command that signs Claude Code in with the folder `config_dir`
/// (`None`: its default): `claude auth login`, with `CLAUDE_CONFIG_DIR=…`
/// before it for any other folder. The folder is quoted for a POSIX shell
/// when it needs it.
pub fn login_command(config_dir: Option<&Path>) -> String {
    match config_dir {
        None => "claude auth login".to_owned(),
        Some(dir) => format!(
            "{CONFIG_DIR_VAR}={} claude auth login",
            shell_quote(&dir.to_string_lossy())
        ),
    }
}

/// `text` as one word for a POSIX shell.
fn shell_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+:@%,=".contains(&b));
    if plain {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

impl ClaudeCode {
    pub const ID: &'static str = "claude-code";

    /// The configuration folder runs with `config` use: the folder chosen
    /// ([`ConfigDirChoice::Folder`], an account's or the agent entry's);
    /// `~/.claude` for [`ConfigDirChoice::Default`]; else (for
    /// [`ConfigDirChoice::Inherit`]) `CLAUDE_CONFIG_DIR` in GitEZ's
    /// environment, else in the user's login shell (asked once per
    /// process; an app started from Finder does not inherit it), else
    /// `~/.claude`. A leading `~/` is the home folder. May block for a few
    /// seconds the first time.
    pub fn config_dir(config: &AgentConfig) -> ConfigDir {
        resolve_config_dir(
            &config.config_dir,
            || std::env::var_os(CONFIG_DIR_VAR),
            || login_shell_var(CONFIG_DIR_VAR),
            home().as_deref(),
        )
    }

    /// Takes out of `command` the argument that turns Claude Code's own
    /// sandbox off, for a run that is not inside GitEZ's sandbox (the
    /// review panel's): there the user's own sandbox setting applies.
    pub fn keep_own_sandbox(command: &mut CommandSpec) {
        if let Some(at) = command
            .args
            .windows(2)
            .position(|pair| pair[0] == SANDBOX_OFF[0] && pair[1] == SANDBOX_OFF[1])
        {
            command.args.drain(at..at + 2);
        }
    }

    /// The environment every `claude` GitEZ starts gets, on top of its own,
    /// and the variables removed from its own first. Claude Code's default
    /// folder runs with `CLAUDE_CONFIG_DIR` removed: set, even to
    /// `~/.claude`, it names another keychain item, another login.
    fn env(config: &AgentConfig, program: &Path) -> (Vec<(String, String)>, Vec<String>) {
        let mut env = Vec::new();
        let mut remove: Vec<String> = ENV_REMOVE.map(str::to_owned).into();
        let config_dir = Self::config_dir(config);
        match config_dir.variable() {
            Some(path) => env.push((
                CONFIG_DIR_VAR.to_owned(),
                path.to_string_lossy().into_owned(),
            )),
            None => remove.push(CONFIG_DIR_VAR.to_owned()),
        }
        if let Some(path) = agent_path(program) {
            env.push(("PATH".to_owned(), path.to_string_lossy().into_owned()));
        }
        // The account decides the folder; the entry's own variables do not.
        env.extend(
            config
                .env
                .iter()
                .filter(|(name, _)| name != CONFIG_DIR_VAR)
                .cloned(),
        );
        (env, remove)
    }
}

impl Agent for ClaudeCode {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn name(&self) -> &'static str {
        "Claude Code"
    }

    fn models(&self) -> &'static [ModelChoice] {
        &MODELS
    }

    fn default_model(&self) -> &'static str {
        "opus"
    }

    /// The agent entry's program; else `claude` on the `PATH` or where its
    /// installers put it. An app started from Finder does not get the
    /// shell's `PATH`.
    fn locate(&self, config: &AgentConfig) -> Result<PathBuf> {
        if let Some(program) = config
            .program
            .as_ref()
            .filter(|p| !p.as_os_str().is_empty())
        {
            let program = expand_home(program, None);
            return if program.is_file() {
                Ok(program)
            } else {
                Err(Error::NotAProgram { path: program })
            };
        }
        let candidates: Vec<PathBuf> = home()
            .into_iter()
            .flat_map(|home| {
                [
                    home.join(".local/bin/claude"),
                    home.join(".claude/local/claude"),
                ]
            })
            .chain(["/opt/homebrew/bin/claude", "/usr/local/bin/claude"].map(PathBuf::from))
            .collect();
        find_program(PROGRAM, std::env::var_os("PATH").as_deref(), &candidates).ok_or_else(|| {
            Error::NotInstalled {
                name: "Claude Code",
                program: PROGRAM,
                searched: candidates
                    .iter()
                    .map(|candidate| candidate.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            }
        })
    }

    fn command(&self, config: &AgentConfig, run: &RunSpec<'_>) -> Result<CommandSpec> {
        let program = self.locate(config)?;
        let mut args: Vec<String> = ARGS.iter().chain(&SANDBOX_OFF).map(|&a| a.into()).collect();
        if !run.images.is_empty() {
            args.extend(STREAM_INPUT.map(String::from));
        }
        if let Some(model) = run.model.filter(|model| !model.is_empty()) {
            args.extend(["--model".into(), model.into()]);
        }
        match run.conversation {
            Conversation::New { session_id } => {
                args.extend(["--session-id".into(), session_id.into()]);
            }
            Conversation::Resume { session_id } => {
                args.extend(["--resume".into(), session_id.into()]);
            }
        }
        if let Some(prompt) = run.system_prompt.filter(|prompt| !prompt.is_empty()) {
            args.extend(["--append-system-prompt".into(), prompt.into()]);
        }
        args.extend(config.args.iter().cloned());
        let (env, env_remove) = Self::env(config, &program);
        Ok(CommandSpec {
            env,
            program,
            args,
            env_remove,
            cwd: run.cwd.to_owned(),
            stdin: Some(if run.images.is_empty() {
                run.prompt.to_owned()
            } else {
                user_message(run)
            }),
        })
    }

    /// The temporary files of its shell tool (`<tmp>/claude-*-cwd`), its
    /// file-watch probe, and its version lock folder.
    fn needs(&self, config: &AgentConfig) -> AgentNeeds {
        let home = home();
        let mut write_dirs = Vec::new();
        let tmp = temp_root(config);
        if let Some(uid) = uid() {
            write_dirs.push(tmp.join(format!("claude-{uid}")).join("fswatch-probe-**"));
        }
        write_dirs.push(tmp.join("claude-*-cwd"));
        if let Some(home) = &home {
            write_dirs.push(home.join(".local/state/claude/locks"));
        }
        AgentNeeds {
            write_dirs,
            write_files: Vec::new(),
            create_dirs: Vec::new(),
            hosts: HOSTS.map(str::to_owned).into(),
        }
    }

    fn optional_hosts(&self) -> &'static [OptionalHost] {
        &OPTIONAL_HOSTS
    }

    /// Adds, for the run's folder: its transcripts (`projects/<folder>`,
    /// needed to resume), its temporary folder and its log folder; and for
    /// its session: `session-env/<id>` and `file-history/<id>`.
    ///
    /// In a folder shared with the user's own sessions
    /// ([`RunSpec::shared_folder`]) the run gets only its own conversation
    /// there: `projects/<folder>/<id>.jsonl` and `projects/<folder>/<id>`
    /// (its subagents and tool results), and `<id>` in the temporary
    /// folder. The rest of `projects/<folder>` holds the user's other
    /// conversations and the memory their later sessions load.
    fn needs_for_run(&self, config: &AgentConfig, run: &RunSpec<'_>) -> AgentNeeds {
        let mut needs = self.needs(config);
        let config_dir = Self::config_dir(config).path;
        let tmp = temp_root(config);
        let cache = cache_dir();
        let session_id = run.conversation.session_id();
        // An id is a UUID; anything else could name a folder outside these.
        let session_id = (!session_id.is_empty()
            && session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .then_some(session_id);
        for name in project_names(run.cwd) {
            let project = config_dir.join("projects").join(&name);
            let project_tmp = uid().map(|uid| tmp.join(format!("claude-{uid}")).join(&name));
            if !run.shared_folder {
                needs.write_dirs.push(project);
                needs.write_dirs.extend(project_tmp);
            } else if let Some(session_id) = session_id {
                needs
                    .write_files
                    .push(project.join(format!("{session_id}.jsonl")));
                needs.write_dirs.push(project.join(session_id));
                needs
                    .write_dirs
                    .extend(project_tmp.map(|tmp| tmp.join(session_id)));
                needs.create_dirs.push(project);
            }
            if let Some(cache) = &cache {
                needs.write_dirs.push(cache.join(&name));
            }
        }
        if let Some(session_id) = session_id {
            needs
                .write_dirs
                .push(config_dir.join("session-env").join(session_id));
            needs
                .write_dirs
                .push(config_dir.join("file-history").join(session_id));
        }
        needs
    }

    /// Every Claude model reads images.
    fn accepts_images(&self, _model: Option<&str>) -> bool {
        true
    }

    /// Without the base64 data of images: the ones it reads with its tools,
    /// and any it echoes.
    fn transcript_line<'l>(&self, line: &'l str) -> Cow<'l, str> {
        omit_image_data(line)
    }

    fn parse(&self, line: &str) -> Option<AgentEvent> {
        parse(line)
    }

    /// Runs `claude auth status --json` in the configuration folder runs
    /// use, and reads only its `loggedIn` field. The output is never kept,
    /// printed or logged.
    fn login_status(&self, config: &AgentConfig) -> LoginStatus {
        let program = match self.locate(config) {
            Ok(program) => program,
            Err(error) => return LoginStatus::Unknown(error.to_string()),
        };
        let mut command = Command::new(&program);
        command
            .args(["auth", "status", "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let (env, remove) = Self::env(config, &program);
        for name in remove {
            command.env_remove(name);
        }
        command.envs(env);
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return LoginStatus::Unknown(format!(
                    "could not start {}: {error}",
                    program.display()
                ));
            }
        };
        let Some(mut stdout) = child.stdout.take() else {
            child.kill().ok();
            child.wait().ok();
            return LoginStatus::Unknown("no pipe to Claude Code".to_owned());
        };
        let reader = std::thread::spawn(move || {
            let mut out = Vec::new();
            stdout.read_to_end(&mut out).ok();
            out
        });
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() < LOGIN_STATUS_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    // The whole group, so nothing it started keeps running.
                    #[cfg(unix)]
                    if let Some(pid) = i32::try_from(child.id())
                        .ok()
                        .and_then(rustix::process::Pid::from_raw)
                    {
                        rustix::process::kill_process_group(pid, rustix::process::Signal::KILL)
                            .ok();
                    }
                    child.kill().ok();
                    child.wait().ok();
                    return LoginStatus::Unknown(format!(
                        "Claude Code did not answer within {} seconds",
                        LOGIN_STATUS_TIMEOUT.as_secs()
                    ));
                }
                Err(error) => return LoginStatus::Unknown(error.to_string()),
            }
        };
        let out = reader.join().unwrap_or_default();
        match logged_in(&out) {
            Some(true) => LoginStatus::SignedIn,
            Some(false) => LoginStatus::SignedOut,
            None if !status.success() => LoginStatus::SignedOut,
            None => LoginStatus::Unknown("Claude Code did not report its login status".to_owned()),
        }
    }
}

/// The prompt and images of `run` as one stream-JSON user message, on one
/// line: the images first, then the text (left out when empty, as the API
/// refuses an empty text block).
fn user_message(run: &RunSpec<'_>) -> String {
    let mut content: Vec<Value> = run
        .images
        .iter()
        .map(|image| {
            json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": image.media_type,
                    "data": base64::engine::general_purpose::STANDARD.encode(image.data),
                },
            })
        })
        .collect();
    if !run.prompt.trim().is_empty() {
        content.push(json!({ "type": "text", "text": run.prompt }));
    }
    let message = json!({
        "type": "user",
        "message": { "role": "user", "content": content },
        "parent_tool_use_id": null,
    });
    let mut line = message.to_string();
    line.push('\n');
    line
}

/// `line` without the data of images: in an event that is JSON, every
/// base64 `source` (`{"type":"base64","data":…}`) gets [`IMAGE_OMITTED`]
/// as its data; in a line that is not (cut short at the line limit), every
/// long run of base64 after `"data":"` is replaced. Lines with no `"data"`
/// key stay as they are.
fn omit_image_data(line: &str) -> Cow<'_, str> {
    if !line.contains("\"data\"") {
        return Cow::Borrowed(line);
    }
    match serde_json::from_str::<Value>(line) {
        Ok(mut event) => {
            if omit_base64_sources(&mut event) {
                Cow::Owned(event.to_string())
            } else {
                Cow::Borrowed(line)
            }
        }
        Err(_) => omit_long_data(line),
    }
}

/// Replaces the data of every base64 source inside `value`; whether any.
fn omit_base64_sources(value: &mut Value) -> bool {
    match value {
        Value::Object(map) => {
            let mut changed = false;
            if map.get("type").and_then(Value::as_str) == Some("base64")
                && let Some(data) = map.get_mut("data")
                && data.is_string()
            {
                *data = Value::String(IMAGE_OMITTED.to_owned());
                changed = true;
            }
            for (_, child) in map.iter_mut() {
                changed |= omit_base64_sources(child);
            }
            changed
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |changed, item| omit_base64_sources(item) | changed),
        _ => false,
    }
}

/// `line` with every run of at least [`OMIT_FROM`] base64 characters after
/// `"data":"` replaced by [`IMAGE_OMITTED`]; the rest stays byte for byte.
fn omit_long_data(line: &str) -> Cow<'_, str> {
    const KEY: &str = "\"data\":\"";
    let is_base64 = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=');
    let bytes = line.as_bytes();
    let mut out = String::new();
    let mut copied = 0;
    let mut from = 0;
    while let Some(found) = line[from..].find(KEY) {
        let start = from + found + KEY.len();
        let end = start
            + bytes[start..]
                .iter()
                .position(|&byte| !is_base64(byte))
                .unwrap_or(bytes.len() - start);
        if end - start >= OMIT_FROM {
            out.push_str(&line[copied..start]);
            out.push_str(IMAGE_OMITTED);
            copied = end;
        }
        from = end;
    }
    if copied == 0 {
        return Cow::Borrowed(line);
    }
    out.push_str(&line[copied..]);
    Cow::Owned(out)
}

/// The configuration folder `choice` names; for
/// [`ConfigDirChoice::Inherit`], the first of these that is set.
fn resolve_config_dir(
    choice: &ConfigDirChoice,
    environment: impl FnOnce() -> Option<OsString>,
    login_shell: impl FnOnce() -> Option<OsString>,
    home: Option<&Path>,
) -> ConfigDir {
    let set = |path: PathBuf| {
        (!path.as_os_str().is_empty()).then(|| expand_home(&path, home.map(Path::to_owned)))
    };
    let configured = match choice {
        ConfigDirChoice::Folder(path) => Some(path),
        ConfigDirChoice::Default => {
            return ConfigDir {
                path: home.unwrap_or(Path::new("/")).join(".claude"),
                source: ConfigDirSource::Default,
            };
        }
        ConfigDirChoice::Inherit => None,
    };
    let found = configured
        .and_then(|path| set(path.to_owned()))
        .map(|path| (path, ConfigDirSource::Settings))
        .or_else(|| {
            environment()
                .and_then(|path| set(path.into()))
                .map(|path| (path, ConfigDirSource::Environment))
        })
        .or_else(|| {
            login_shell()
                .and_then(|path| set(path.into()))
                .map(|path| (path, ConfigDirSource::LoginShell))
        });
    match found {
        Some((path, source)) => ConfigDir { path, source },
        None => ConfigDir {
            path: home.unwrap_or(Path::new("/")).join(".claude"),
            source: ConfigDirSource::Default,
        },
    }
}

/// `path` with a leading `~/` (or a lone `~`) replaced by `home`
/// (`None`: `$HOME`). The rest is kept as written, a trailing `/`
/// included: Claude Code names its keychain item after the exact string.
pub fn expand_home(path: &Path, home: Option<PathBuf>) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    let rest = match text.strip_prefix("~") {
        Some("") => None,
        Some(rest) if rest.starts_with('/') => Some(rest.to_owned()),
        _ => return path.to_owned(),
    };
    let Some(home) = home.or_else(self::home) else {
        return path.to_owned();
    };
    match rest {
        None => home,
        Some(rest) => {
            let mut joined = home.into_os_string();
            joined.push(rest);
            PathBuf::from(joined)
        }
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

#[cfg(unix)]
fn uid() -> Option<u32> {
    Some(rustix::process::getuid().as_raw())
}

#[cfg(not(unix))]
fn uid() -> Option<u32> {
    None
}

/// Where Claude Code keeps temporary files: `CLAUDE_CODE_TMPDIR` (the
/// agent entry's, else GitEZ's), else `/tmp`, with symbolic links resolved
/// (`/private/tmp` on macOS), as the sandbox sees paths.
fn temp_root(config: &AgentConfig) -> PathBuf {
    let root = config
        .env
        .iter()
        .rev()
        .find(|(name, _)| name == TMPDIR_VAR)
        .map(|(_, value)| PathBuf::from(value))
        .or_else(|| std::env::var_os(TMPDIR_VAR).map(PathBuf::from))
        .filter(|root| root.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    std::fs::canonicalize(&root).unwrap_or(root)
}

/// Where Claude Code keeps its logs, one folder per project inside.
fn cache_dir() -> Option<PathBuf> {
    let home = home()?;
    if cfg!(target_os = "macos") {
        return Some(home.join("Library/Caches/claude-cli-nodejs"));
    }
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|cache| cache.is_absolute())
        .unwrap_or_else(|| home.join(".cache"));
    Some(cache.join("claude-cli-nodejs"))
}

/// The names Claude Code gives the project in `cwd`: its path as given and
/// with symbolic links resolved (it uses the latter), sanitised.
fn project_names(cwd: &Path) -> Vec<String> {
    let mut names = vec![sanitise(&cwd.to_string_lossy())];
    if let Ok(real) = std::fs::canonicalize(cwd) {
        let real = sanitise(&real.to_string_lossy());
        if !names.contains(&real) {
            names.insert(0, real);
        }
    }
    names
}

/// A path as Claude Code turns it into a folder name: every character that
/// is not an ASCII letter or digit becomes `-` (one per UTF-16 unit, as in
/// JavaScript); longer than 200 units, it is cut and a hash added.
fn sanitise(path: &str) -> String {
    let mut name = String::with_capacity(path.len());
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c);
        } else {
            name.extend(std::iter::repeat_n('-', c.len_utf16()));
        }
    }
    if name.len() <= MAX_SANITISED_LEN {
        return name;
    }
    // Java's string hash over UTF-16 units, as Claude Code computes it.
    let hash = path.encode_utf16().fold(0_i32, |hash, unit| {
        hash.wrapping_shl(5)
            .wrapping_sub(hash)
            .wrapping_add(i32::from(unit))
    });
    name.truncate(MAX_SANITISED_LEN);
    format!("{name}-{}", base36(i64::from(hash).unsigned_abs()))
}

fn base36(mut value: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut digits = Vec::new();
    loop {
        digits.push(DIGITS[(value % 36) as usize]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    digits.reverse();
    String::from_utf8_lossy(&digits).into_owned()
}

/// A complete `PATH` for the agent and the commands it runs. Its shell
/// cannot source the user's rc files inside the sandbox, so it gets the
/// login shell's `PATH`, GitEZ's own, the agent's folder and the usual
/// install folders.
fn agent_path(program: &Path) -> Option<OsString> {
    let login = login_shell_var("PATH").unwrap_or_default();
    let current = std::env::var_os("PATH").unwrap_or_default();
    let home = home();
    let well_known =
        [
            "/opt/homebrew/bin",
            "/opt/homebrew/sbin",
            "/usr/local/bin",
            "/usr/local/sbin",
            "/opt/local/bin",
            "/nix/var/nix/profiles/default/bin",
            "/home/linuxbrew/.linuxbrew/bin",
        ]
        .map(PathBuf::from)
        .into_iter()
        .chain(home.iter().flat_map(|home| {
            [".local/bin", ".cargo/bin", ".nix-profile/bin"].map(|d| home.join(d))
        }))
        .chain(["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from))
        .filter(|dir| dir.is_dir());
    join_path(
        std::env::split_paths(&login)
            .chain(std::env::split_paths(&current))
            .chain(program.parent().map(Path::to_owned))
            .chain(well_known),
    )
}

/// The `loggedIn` field of `claude auth status --json`, if the output has
/// one.
fn logged_in(out: &[u8]) -> Option<bool> {
    let out = String::from_utf8_lossy(out);
    let field = |text: &str| {
        serde_json::from_str::<Value>(text)
            .ok()?
            .get("loggedIn")?
            .as_bool()
    };
    field(out.trim()).or_else(|| out.lines().find_map(|line| field(line.trim())))
}

/// The event a line of `claude -p --output-format stream-json` reports.
fn parse(line: &str) -> Option<AgentEvent> {
    let event: Value = serde_json::from_str(line).ok()?;
    let text = |value: &Value| value.as_str().map(str::to_owned);
    match event.get("type")?.as_str()? {
        "system" if event.get("subtype")?.as_str()? == "init" => Some(AgentEvent::Started {
            session_id: event.get("session_id").and_then(text),
            model: event.get("model").and_then(text),
        }),
        "assistant" => {
            // One content block per message in practice; the first that
            // shows something is reported.
            content(&event)?
                .iter()
                .find_map(|block| match block.get("type")?.as_str()? {
                    "text" => {
                        let text = block.get("text")?.as_str()?.trim();
                        (!text.is_empty()).then(|| AgentEvent::Text(text.to_owned()))
                    }
                    "tool_use" => Some(AgentEvent::Tool {
                        name: block.get("name")?.as_str()?.to_owned(),
                        detail: tool_detail(block.get("input")),
                    }),
                    _ => None,
                })
        }
        "user" => content(&event)?.iter().find_map(|block| {
            let failed = block.get("type")?.as_str()? == "tool_result"
                && block.get("is_error")?.as_bool()?;
            failed.then(|| AgentEvent::ToolFailed(first_line(&result_text(block.get("content")))))
        }),
        "rate_limit_event" => {
            let info = event.get("rate_limit_info")?;
            (info.get("status")?.as_str()? == "rejected").then(|| {
                AgentEvent::UsageLimit(UsageLimit {
                    resets_at: info.get("resetsAt").and_then(Value::as_i64),
                })
            })
        }
        "result" => {
            let is_error = event
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut text = event.get("result").and_then(text).unwrap_or_default();
            let usage_limit = if is_error && text.to_lowercase().contains("usage limit") {
                let resets_at = split_reset_time(&text)
                    .map(|(message, resets_at)| (message.to_owned(), resets_at));
                Some(UsageLimit {
                    resets_at: resets_at.map(|(message, resets_at)| {
                        text = message;
                        resets_at
                    }),
                })
            } else {
                None
            };
            Some(AgentEvent::Finished(Finished {
                success: event.get("subtype").and_then(Value::as_str) == Some("success")
                    && !is_error,
                text,
                session_id: event
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                cost_usd: event.get("total_cost_usd").and_then(Value::as_f64),
                turns: event.get("num_turns").and_then(Value::as_u64),
                duration_ms: event.get("duration_ms").and_then(Value::as_u64),
                usage_limit,
            }))
        }
        _ => None,
    }
}

/// "Claude AI usage limit reached|1794545400": the message and the reset
/// time after the bar, which is not part of the message.
fn split_reset_time(text: &str) -> Option<(&str, i64)> {
    let (message, time) = text.rsplit_once('|')?;
    let time = time.trim();
    if time.is_empty() || !time.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((message.trim_end(), time.parse().ok()?))
}

fn content(event: &Value) -> Option<&Vec<Value>> {
    event.get("message")?.get("content")?.as_array()
}

/// What a tool was used on: the command, file, pattern or description.
fn tool_detail(input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    [
        "command",
        "file_path",
        "notebook_path",
        "pattern",
        "url",
        "description",
    ]
    .iter()
    .find_map(|key| input.get(*key)?.as_str())
    .map(first_line)
    .unwrap_or_default()
}

/// A tool result's text: a string, or the text of its blocks.
fn result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text")?.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn first_line(text: &str) -> String {
    text.trim().lines().next().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests;
