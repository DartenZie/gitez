//! Anthropic's sandbox runtime, `srt` (`@anthropic-ai/sandbox-runtime`),
//! run as `<node> <cli.js> --settings <file> -- <command>`.
//!
//! What the sandbox experiment found and this module relies on:
//!
//! - srt parses options anywhere on its command line, so `--` always comes
//!   before the wrapped command.
//! - The wrapped command becomes srt's direct child in srt's process group.
//!   srt forwards SIGINT and SIGTERM to it, so SIGINT sent to the process
//!   group reaches the command twice (once from the kernel, once from srt).
//!   SIGKILL to srt alone orphans the command: hard stops must kill the
//!   whole group, which reaches every process started here.
//! - srt exits 0 when the command dies of SIGINT or SIGTERM: read success
//!   from the command's own output, not from the exit code.
//! - srt sets the command's `TMPDIR` to `/tmp/claude`, shared by every srt
//!   sandbox, unless `CLAUDE_CODE_TMPDIR` is set. When the command sets
//!   `TMPDIR`, [`Srt::wrap`] passes it to srt as `CLAUDE_CODE_TMPDIR`, which
//!   gives each command its own temporary folder. Claude Code reads the same
//!   variable for its own temporary files. `/tmp/claude` stays writable.
//! - The **Open** network profile cannot be written in an srt settings file
//!   (`allowedDomains` is required and `*` is refused). It runs under a
//!   GitEZ-generated Seatbelt profile on macOS and under bubblewrap without
//!   a network namespace on Linux, with the same file rules.
//!
//! srt reports refused hosts only in its debug output, on its standard
//! error, which the command shares. The command line therefore runs srt
//! with `--debug` through `bash`, whose process substitution passes srt's
//! standard error through `awk`: debug lines are dropped, except the hosts
//! srt refused, which go to a file in the run folder; every other line
//! reaches the caller's standard error unchanged.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use gitez_agent::CommandSpec;
use serde_json::{Value, json};

use crate::locate::{Search, is_program, node_version, parse_version};
use crate::policy::{Network, Policy, WritePath, is_pattern};
use crate::{Error, Result, Sandbox, Status, Wrapped};

/// The pinned package Settings installs.
pub const SRT_PACKAGE: &str = "@anthropic-ai/sandbox-runtime@0.0.82";
const PACKAGE_NAME: &str = "@anthropic-ai/sandbox-runtime";
const PACKAGE_VERSION: &str = "0.0.82";
/// The package's folder in an install folder.
const PACKAGE_DIR: &str = "node_modules/@anthropic-ai/sandbox-runtime";
/// srt's command, in the package folder.
const CLI: &str = "dist/cli.js";
/// The oldest Node.js srt 0.0.82 runs on (its `engines`).
const NODE_MIN: (u32, u32, u32) = (22, 12, 0);

const NO_NODE: &str = "The sandbox needs Node.js. Install it (for example from nodejs.org), \
                       then install the sandbox in Settings.";
const NOT_INSTALLED: &str = "Install the sandbox in Settings.";

/// The shared temporary folder srt makes writable and hands out as `TMPDIR`.
const SHARED_TMP: &str = "/tmp/claude";

/// Runs srt (arguments: refused-hosts file, awk, filter program, then srt's
/// own command line) with its standard error passed through the filter.
/// The filter ignores SIGINT and SIGTERM so that it outlives the command
/// and passes on what the command writes while it stops.
const WRAPPER: &str = r#"blocked=$1 awk=$2 filter=$3 echoed=$4
shift 4
exec "$@" 2> >(trap '' INT TERM; exec "$awk" -v blocked="$blocked" -v echoed="$echoed" "$filter" >&2)"#;

/// Drops srt's debug lines, keeps the `host:port` of refused connections,
/// and passes every other line on. Two debug messages span several lines:
/// the network settings as JSON, and the command, echoed once with its
/// arguments (`echoed` is the number of lines after its first).
const FILTER: &str = r#"
json { if ($0 == "}" || $0 == "]") json = 0; next }
skip > 0 { skip--; next }
/^\[SandboxDebug\] / {
  if ($0 == "[SandboxDebug] {" || $0 == "[SandboxDebug] [") json = 1
  else if (!echo_seen && index($0, "[SandboxDebug] Original command: ") == 1) {
    echo_seen = 1
    skip = echoed + 0
  }
  else if (match($0, /(No matching rule, denying|Connection blocked to):? /)) {
    print substr($0, RSTART + RLENGTH) >> blocked
    fflush(blocked)
  }
  next
}
{ print; fflush() }
"#;

/// srt, found on the `PATH` or installed in GitEZ's data folder.
#[derive(Debug, Clone)]
pub struct Srt {
    data_dir: PathBuf,
    search: Search,
}

/// How to start srt.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Found {
    /// The program, and the script it runs if it is Node.js.
    launch: Vec<PathBuf>,
    /// srt's own command.
    program: PathBuf,
    version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Os {
    Mac,
    Linux,
}

impl Srt {
    pub fn new(data_dir: &Path) -> Self {
        Self::with_search(data_dir, Search::system())
    }

    pub(crate) fn with_search(data_dir: &Path, search: Search) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            search,
        }
    }

    /// Where Settings installs it: `<data_dir>/sandbox`.
    pub fn install_dir(data_dir: &Path) -> PathBuf {
        data_dir.join("sandbox")
    }

    /// Runs `npm install --prefix <install_dir> --no-audit --no-fund
    /// --omit=optional --ignore-scripts SRT_PACKAGE` with the npm next to
    /// the Node.js found on the PATH (or in the usual install places).
    /// Blocks for up to minutes. Never installs globally. Returns srt's
    /// command (`…/dist/cli.js`).
    pub fn install(data_dir: &Path) -> Result<PathBuf> {
        install(data_dir, &Search::system())
    }

    /// The srt to run, or what the user must do first.
    fn locate(&self) -> std::result::Result<Found, String> {
        platform_requirements(&self.search)?;
        let node = self.search.find("node");
        let installed = Self::install_dir(&self.data_dir)
            .join(PACKAGE_DIR)
            .join(CLI);
        let program = match self.search.on_path("srt") {
            Some(on_path) => on_path.canonicalize().unwrap_or(on_path),
            None if installed.is_file() => installed,
            None => {
                let node = node.ok_or(NO_NODE)?;
                check_node(&node)?;
                return Err(NOT_INSTALLED.to_owned());
            }
        };
        let version = package_version(&program);
        if !is_node_script(&program) {
            return Ok(Found {
                launch: vec![program.clone()],
                program,
                version,
            });
        }
        let node = node.ok_or(NO_NODE)?;
        check_node(&node)?;
        Ok(Found {
            launch: vec![node, program.clone()],
            program,
            version,
        })
    }

    fn wrap_srt(
        &self,
        found: &Found,
        command: CommandSpec,
        policy: &Policy,
        hosts: &[String],
    ) -> Result<Wrapped> {
        let run = run_dir()?;
        let settings_path = run.path().join("srt-settings.json");
        let text = serde_json::to_string_pretty(&settings(policy, hosts, Os::current()))
            .expect("settings serialise");
        write(&settings_path, &text)?;
        if Os::current() == Os::Linux {
            create_write_dirs(policy)?;
        }
        let blocked = run.path().join("blocked-hosts");

        let mut spec = command;
        temp_dir_env(&mut spec, false)?;
        spec.env_remove.push("SRT_DEBUG".to_owned());
        let program = std::mem::take(&mut spec.program);
        let args = std::mem::take(&mut spec.args);
        spec.program = PathBuf::from("/bin/bash");
        spec.args = vec![
            "-c".to_owned(),
            WRAPPER.to_owned(),
            "gitez-sandbox".to_owned(),
            text_of(&blocked),
            awk(),
            FILTER.to_owned(),
        ];
        let echoed = std::iter::once(&program.to_string_lossy().into_owned())
            .chain(&args)
            .map(|arg| arg.matches('\n').count())
            .sum::<usize>();
        spec.args.push(echoed.to_string());
        spec.args
            .extend(found.launch.iter().map(|part| text_of(part)));
        spec.args.extend(
            [
                "--debug",
                "--settings",
                &text_of(&settings_path),
                "--",
                // srt's debug switch is in its environment; keep it from
                // the command.
                "/usr/bin/env",
                "-u",
                "SRT_DEBUG",
            ]
            .map(String::from),
        );
        spec.args.push(text_of(&program));
        spec.args.extend(args);
        Ok(Wrapped::with_blocked(
            spec,
            Box::new(run),
            Box::new(move || read_blocked(&blocked)),
        ))
    }

    /// The Open network profile: a Seatbelt profile with the same file rules
    /// and no network limit.
    #[cfg(target_os = "macos")]
    fn wrap_open(&self, command: CommandSpec, policy: &Policy) -> Result<Wrapped> {
        let run = run_dir()?;
        let profile_path = run.path().join("profile.sb");
        let text = crate::seatbelt::profile(policy, &command.cwd, self.search.home());
        write(&profile_path, &text)?;
        let mut spec = command;
        temp_dir_env(&mut spec, true)?;
        let program = std::mem::take(&mut spec.program);
        let args = std::mem::take(&mut spec.args);
        spec.program = PathBuf::from("/usr/bin/sandbox-exec");
        spec.args = vec!["-f".to_owned(), text_of(&profile_path), text_of(&program)];
        spec.args.extend(args);
        Ok(Wrapped::new(spec, Box::new(run)))
    }

    /// The Open network profile: bubblewrap with the same file rules and the
    /// host's network. Untested.
    #[cfg(target_os = "linux")]
    fn wrap_open(&self, command: CommandSpec, policy: &Policy) -> Result<Wrapped> {
        let bwrap = self.search.find("bwrap").ok_or_else(|| {
            Error::Unavailable(
                "The Open network profile needs bubblewrap (bwrap). Install it with your \
                 package manager."
                    .to_owned(),
            )
        })?;
        create_write_dirs(policy)?;
        let mut spec = command;
        temp_dir_env(&mut spec, true)?;
        let program = std::mem::take(&mut spec.program);
        let args = std::mem::take(&mut spec.args);
        spec.program = bwrap;
        spec.args =
            crate::bwrap::args(policy, &spec.cwd, self.search.home(), crate::bwrap::on_disk);
        spec.args.push("--".to_owned());
        spec.args.push(text_of(&program));
        spec.args.extend(args);
        Ok(Wrapped::new(spec, Box::new(())))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn wrap_open(&self, _command: CommandSpec, _policy: &Policy) -> Result<Wrapped> {
        Err(Error::Unavailable(
            "GitEZ has no sandbox for this system.".to_owned(),
        ))
    }
}

impl Sandbox for Srt {
    fn name(&self) -> &'static str {
        "srt (Anthropic sandbox runtime)"
    }

    fn status(&self) -> Status {
        match self.locate() {
            Ok(found) => Status::Ready {
                program: found.program,
                version: found.version,
            },
            Err(fix) => Status::Missing { fix },
        }
    }

    /// See the module documentation for how signals, exit codes and the
    /// temporary folder behave.
    fn wrap(&self, command: CommandSpec, policy: &Policy) -> Result<Wrapped> {
        match &policy.network {
            Network::Open => self.wrap_open(command, policy),
            Network::Hosts(hosts) => {
                let found = self.locate().map_err(Error::Unavailable)?;
                self.wrap_srt(&found, command, policy, hosts)
            }
        }
    }
}

impl Os {
    fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Mac
        }
    }
}

/// What the system must have besides srt and Node.js.
fn platform_requirements(search: &Search) -> std::result::Result<(), String> {
    if cfg!(target_os = "macos") {
        for program in ["/usr/bin/sandbox-exec", "/bin/bash"] {
            if !is_program(Path::new(program)) {
                return Err(format!(
                    "The sandbox needs {program}, which this Mac does not have."
                ));
            }
        }
        Ok(())
    } else if cfg!(target_os = "linux") {
        let missing: Vec<&str> = [
            ("bwrap", "bubblewrap"),
            ("socat", "socat"),
            ("rg", "ripgrep"),
        ]
        .into_iter()
        .filter(|(program, _)| search.find(program).is_none())
        .map(|(_, package)| package)
        .collect();
        if !is_program(Path::new("/bin/bash")) {
            return Err("The sandbox needs bash in /bin/bash.".to_owned());
        }
        if missing.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "The sandbox needs {}. Install them with your package manager.",
                missing.join(", ")
            ))
        }
    } else {
        Err("GitEZ has no sandbox for this system.".to_owned())
    }
}

/// Whether Node.js at `node` is new enough for srt.
fn check_node(node: &Path) -> std::result::Result<(), String> {
    let version = node_version(node).ok_or_else(|| {
        format!(
            "{} does not run. Reinstall Node.js, then try again.",
            node.display()
        )
    })?;
    match parse_version(&version) {
        Some(found) if found >= NODE_MIN => Ok(()),
        _ => Err(format!(
            "The sandbox needs Node.js 22.12 or newer; {} is {version}. Update Node.js, \
             then try again.",
            node.display()
        )),
    }
}

/// Whether `program` is a Node.js script (rather than a compiled srt).
fn is_node_script(program: &Path) -> bool {
    if program
        .extension()
        .is_some_and(|extension| extension == "js" || extension == "mjs")
    {
        return true;
    }
    let mut head = [0u8; 128];
    let read = std::fs::File::open(program)
        .and_then(|mut file| std::io::Read::read(&mut file, &mut head))
        .unwrap_or(0);
    let head = String::from_utf8_lossy(&head[..read]);
    head.starts_with("#!")
        && head
            .lines()
            .next()
            .is_some_and(|line| line.contains("node"))
}

/// The version of the srt package `cli` belongs to.
fn package_version(cli: &Path) -> Option<String> {
    let manifest = cli.parent()?.parent()?.join("package.json");
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(manifest).ok()?).ok()?;
    if manifest.get("name")?.as_str()? != PACKAGE_NAME {
        return None;
    }
    Some(manifest.get("version")?.as_str()?.to_owned())
}

/// The srt settings file for `policy` with network limited to `hosts`.
///
/// On macOS a folder is a path and a file is a literal entry (srt then never
/// reads it as a pattern); a path with `*` stays a pattern. On Linux srt
/// binds existing paths only, so a file is made writable through its folder
/// and patterns are left out (see `bwrap::write_dirs`).
fn settings(policy: &Policy, hosts: &[String], os: Os) -> Value {
    let allow_write: Vec<Value> = match os {
        Os::Mac => policy
            .write
            .iter()
            .map(|write| match write {
                WritePath::Tree(path) => path_entry(path),
                WritePath::File(path) if is_pattern(path) => path_entry(path),
                WritePath::File(path) => literal(path),
            })
            .collect(),
        Os::Linux => crate::bwrap::write_dirs(policy)
            .iter()
            .map(|path| path_entry(path))
            .collect(),
    };
    json!({
        "network": {
            "allowedDomains": hosts,
            "deniedDomains": [],
        },
        "filesystem": {
            "denyRead": policy.deny_read.iter().map(|path| path_entry(path)).collect::<Vec<_>>(),
            "allowWrite": allow_write,
            "denyWrite": policy.deny_write.iter().map(|path| path_entry(path)).collect::<Vec<_>>(),
        },
    })
}

/// A path as srt reads it: a pattern if it has `*`; a literal entry if it
/// has other characters srt would read as a pattern; else the plain path.
fn path_entry(path: &Path) -> Value {
    let text = text_of(path);
    if !is_pattern(path) && text.contains(['?', '[', ']']) {
        literal(path)
    } else {
        Value::String(text)
    }
}

fn literal(path: &Path) -> Value {
    json!({ "path": text_of(path), "literal": true })
}

/// Sets up the command's temporary folder. When the command sets `TMPDIR`,
/// the folder is created (private) if missing and passed on as
/// `CLAUDE_CODE_TMPDIR` too, which srt hands to the command as its
/// `TMPDIR`. Otherwise srt's shared folder is used: `set_tmpdir` sets
/// `TMPDIR` to it for back ends that are not srt.
fn temp_dir_env(spec: &mut CommandSpec, set_tmpdir: bool) -> Result<()> {
    let value = |name: &str| {
        spec.env
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let own_override = value("CLAUDE_CODE_TMPDIR");
    match value("TMPDIR") {
        Some(tmp) => {
            create_private_dir(Path::new(&tmp))?;
            if own_override.is_none() {
                spec.env.push(("CLAUDE_CODE_TMPDIR".to_owned(), tmp));
            }
        }
        None => {
            create_private_dir(Path::new(SHARED_TMP))?;
            if own_override.is_none() {
                spec.env_remove
                    .extend(["CLAUDE_CODE_TMPDIR", "CLAUDE_TMPDIR"].map(String::from));
            }
            if set_tmpdir {
                spec.env.push(("TMPDIR".to_owned(), SHARED_TMP.to_owned()));
            }
        }
    }
    if set_tmpdir {
        spec.env
            .push(("SANDBOX_RUNTIME".to_owned(), "1".to_owned()));
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if path.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|source| Error::Profile {
            path: path.to_path_buf(),
            source,
        })
}

/// On Linux only existing paths can be made writable: create the folders.
fn create_write_dirs(policy: &Policy) -> Result<()> {
    for dir in crate::bwrap::write_dirs(policy) {
        std::fs::create_dir_all(&dir).map_err(|source| Error::Profile { path: dir, source })?;
    }
    Ok(())
}

/// A private folder for one run's files, removed when dropped.
fn run_dir() -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("gitez-sandbox-")
        .tempdir()
        .map_err(|source| Error::Profile {
            path: std::env::temp_dir(),
            source,
        })
}

fn write(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).map_err(|source| Error::Profile {
        path: path.to_path_buf(),
        source,
    })
}

fn awk() -> String {
    ["/usr/bin/awk", "/bin/awk"]
        .into_iter()
        .find(|path| is_program(Path::new(path)))
        .unwrap_or("awk")
        .to_owned()
}

fn text_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The refused hosts recorded in `path`, without ports, each once.
fn read_blocked(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut hosts: Vec<String> = Vec::new();
    for line in text.lines() {
        let host = without_port(line.trim());
        if !host.is_empty() && !hosts.iter().any(|known| known == host) {
            hosts.push(host.to_owned());
        }
    }
    hosts
}

/// `host` of `host:port` or `[v6]:port`.
fn without_port(address: &str) -> &str {
    if let Some(rest) = address.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match address.rsplit_once(':') {
        Some((host, port))
            if !host.contains(':')
                && !port.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            host
        }
        _ => address,
    }
}

fn install(data_dir: &Path, search: &Search) -> Result<PathBuf> {
    let node = search
        .find("node")
        .ok_or_else(|| Error::Install(NO_NODE.to_owned()))?;
    check_node(&node).map_err(Error::Install)?;
    let node_dir = node.parent().map(Path::to_path_buf).unwrap_or_default();
    let npm = Some(node_dir.join("npm"))
        .filter(|npm| is_program(npm))
        .or_else(|| search.find("npm"))
        .ok_or_else(|| {
            Error::Install(format!(
                "npm was not found next to {} or on the PATH. Reinstall Node.js with npm.",
                node.display()
            ))
        })?;
    let dir = Srt::install_dir(data_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|error| Error::Install(format!("could not create {}: {error}", dir.display())))?;

    // npm is a Node.js script: put this Node.js first on its PATH.
    let mut path = vec![node_dir];
    if let Some(inherited) = std::env::var_os("PATH") {
        path.extend(std::env::split_paths(&inherited));
    }
    let path = std::env::join_paths(path)
        .map_err(|error| Error::Install(format!("could not build a PATH for npm: {error}")))?;
    let output = Command::new(&npm)
        .args(["install", "--prefix"])
        .arg(&dir)
        .args([
            "--no-global",
            "--no-audit",
            "--no-fund",
            "--omit=optional",
            "--ignore-scripts",
            SRT_PACKAGE,
        ])
        .current_dir(&dir)
        .env("PATH", path)
        .env("npm_config_update_notifier", "false")
        .env_remove("npm_config_global")
        .env_remove("NPM_CONFIG_GLOBAL")
        .env_remove("npm_config_prefix")
        .env_remove("NPM_CONFIG_PREFIX")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error::Install(format!("could not run {}: {error}", npm.display())))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(8).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        return Err(Error::Install(format!(
            "npm failed ({}): {}",
            output.status,
            tail.join("\n")
        )));
    }
    let cli = dir.join(PACKAGE_DIR).join(CLI);
    match package_version(&cli) {
        Some(version) if version == PACKAGE_VERSION && cli.is_file() => Ok(cli),
        found => Err(Error::Install(format!(
            "npm finished, but {} is not srt {PACKAGE_VERSION} (found {})",
            cli.display(),
            found.as_deref().unwrap_or("nothing")
        ))),
    }
}

#[cfg(test)]
mod tests;
