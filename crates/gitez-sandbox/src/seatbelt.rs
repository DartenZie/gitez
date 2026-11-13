//! A GitEZ-generated Seatbelt (`sandbox-exec`) profile, for the Open
//! network profile on macOS, which the `srt` command cannot express.
//!
//! The profile is the one `srt` 0.0.82 generates (dumped in the sandbox
//! experiment), with the same file rules, the same mandatory denies next to
//! the working folder, and `(allow network*)` in place of the rule that
//! allows only `srt`'s local proxy. It keeps the Mach services the Keychain
//! needs (`com.apple.SecurityServer`, `com.apple.securityd.xpc`), so an
//! agent reaches its own login, and adds `com.apple.trustd.agent`, which
//! `srt` allows only with weaker network isolation and which programs that
//! verify certificates themselves (Go) need to reach any host.

use std::path::{Component, Path, PathBuf};

use crate::policy::{Policy, WritePath, escape_regex, is_pattern, pattern_base, pattern_regex};

/// Reported by Seatbelt with every refused operation.
const LOG_TAG: &str = "GITEZ_SANDBOX";

/// Writable whatever the policy says, as `srt` makes them: the standard
/// streams and devices, and `srt`'s shared temporary folder.
const OWN_WRITE_PATHS: [&str; 7] = [
    "/dev/stdout",
    "/dev/stderr",
    "/dev/null",
    "/dev/tty",
    "/dev/dtracehelper",
    "/dev/autofs_nowait",
    "/private/tmp/claude",
];

/// Writable folders in the home folder, unless a read deny covers them.
const HOME_WRITE_DIRS: [&str; 2] = [".npm/_logs", ".claude/debug"];

/// Never writable in the working folder or below it, as in `srt`: files and
/// folders that make other programs run code or change their behaviour.
pub(crate) const MANDATORY_DENIES: [&str; 15] = [
    ".gitconfig",
    ".gitmodules",
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".ripgreprc",
    ".mcp.json",
    ".vscode",
    ".idea",
    ".claude/commands",
    ".claude/agents",
    ".git/hooks",
    ".git/config",
];

/// The fixed part of the profile, as `srt` writes it.
const PREAMBLE: &str = r#"; Essential permissions - based on Chrome sandbox policy
; Process permissions
(allow process-exec)
(allow process-fork)
(allow process-info* (target same-sandbox))
(allow signal (target same-sandbox))
(allow mach-priv-task-port (target same-sandbox))

; User preferences
(allow user-preference-read)

; Mach IPC - specific services only (no wildcard)
(allow mach-lookup
  (global-name "com.apple.audio.systemsoundserver")
  (global-name "com.apple.distributed_notifications@Uv3")
  (global-name "com.apple.FontObjectsServer")
  (global-name "com.apple.fonts")
  (global-name "com.apple.logd")
  (global-name "com.apple.lsd.mapdb")
  (global-name "com.apple.PowerManagement.control")
  (global-name "com.apple.system.logger")
  (global-name "com.apple.system.notification_center")
  (global-name "com.apple.system.opendirectoryd.libinfo")
  (global-name "com.apple.system.opendirectoryd.membership")
  (global-name "com.apple.bsd.dirhelper")
  (global-name "com.apple.securityd.xpc")
  (global-name "com.apple.coreservices.launchservicesd")
)

; Certificate verification for programs that do it themselves (Go)
(allow mach-lookup (global-name "com.apple.trustd.agent"))

; POSIX IPC - shared memory
(allow ipc-posix-shm)

; POSIX IPC - semaphores for Python multiprocessing
(allow ipc-posix-sem)

; IOKit - specific operations only
(allow iokit-open
  (iokit-registry-entry-class "IOSurfaceRootUserClient")
  (iokit-registry-entry-class "RootDomainUserClient")
  (iokit-user-client-class "IOSurfaceSendRight")
)

; IOKit properties
(allow iokit-get-properties)

; Specific safe system-sockets, doesn't allow network access
(allow system-socket (require-all (socket-domain AF_SYSTEM) (socket-protocol 2)))

; sysctl - specific sysctls only
(allow sysctl-read
  (sysctl-name "hw.activecpu")
  (sysctl-name "hw.busfrequency_compat")
  (sysctl-name "hw.byteorder")
  (sysctl-name "hw.cacheconfig")
  (sysctl-name "hw.cachelinesize_compat")
  (sysctl-name "hw.cpufamily")
  (sysctl-name "hw.cpufrequency")
  (sysctl-name "hw.cpufrequency_compat")
  (sysctl-name "hw.cputype")
  (sysctl-name "hw.l1dcachesize_compat")
  (sysctl-name "hw.l1icachesize_compat")
  (sysctl-name "hw.l2cachesize_compat")
  (sysctl-name "hw.l3cachesize_compat")
  (sysctl-name "hw.logicalcpu")
  (sysctl-name "hw.logicalcpu_max")
  (sysctl-name "hw.machine")
  (sysctl-name "hw.memsize")
  (sysctl-name "hw.ncpu")
  (sysctl-name "hw.nperflevels")
  (sysctl-name "hw.packages")
  (sysctl-name "hw.pagesize_compat")
  (sysctl-name "hw.pagesize")
  (sysctl-name "hw.physicalcpu")
  (sysctl-name "hw.physicalcpu_max")
  (sysctl-name "hw.tbfrequency_compat")
  (sysctl-name "hw.vectorunit")
  (sysctl-name "kern.argmax")
  (sysctl-name "kern.bootargs")
  (sysctl-name "kern.hostname")
  (sysctl-name "kern.maxfiles")
  (sysctl-name "kern.maxfilesperproc")
  (sysctl-name "kern.maxproc")
  (sysctl-name "kern.ngroups")
  (sysctl-name "kern.osproductversion")
  (sysctl-name "kern.osrelease")
  (sysctl-name "kern.ostype")
  (sysctl-name "kern.osvariant_status")
  (sysctl-name "kern.osversion")
  (sysctl-name "kern.secure_kernel")
  (sysctl-name "kern.tcsm_available")
  (sysctl-name "kern.tcsm_enable")
  (sysctl-name "kern.usrstack64")
  (sysctl-name "kern.version")
  (sysctl-name "kern.willshutdown")
  (sysctl-name "machdep.cpu.brand_string")
  (sysctl-name "machdep.ptrauth_enabled")
  (sysctl-name "security.mac.lockdown_mode_state")
  (sysctl-name "sysctl.proc_cputype")
  (sysctl-name "vm.loadavg")
  (sysctl-name-prefix "hw.optional.arm")
  (sysctl-name-prefix "hw.optional.arm.")
  (sysctl-name-prefix "hw.optional.armv8_")
  (sysctl-name-prefix "hw.perflevel")
  (sysctl-name-prefix "kern.proc.all")
  (sysctl-name-prefix "kern.proc.pgrp.")
  (sysctl-name-prefix "kern.proc.pid.")
  (sysctl-name-prefix "machdep.cpu.")
  (sysctl-name-prefix "net.routetable.")
)

; V8 thread calculations
(allow sysctl-write
  (sysctl-name "kern.tcsm_enable")
)

; Distributed notifications
(allow distributed-notification-post)

; Specific mach-lookup permissions for security operations
(allow mach-lookup (global-name "com.apple.SecurityServer"))

; File I/O on device files
(allow file-ioctl (literal "/dev/null"))
(allow file-ioctl (literal "/dev/zero"))
(allow file-ioctl (literal "/dev/random"))
(allow file-ioctl (literal "/dev/urandom"))
(allow file-ioctl (literal "/dev/dtracehelper"))
(allow file-ioctl (literal "/dev/tty"))

(allow file-ioctl file-read-data file-write-data
  (require-all
    (literal "/dev/null")
    (vnode-type CHARACTER-DEVICE)
  )
)
"#;

/// A path rule, with its path as Seatbelt sees it (symbolic links of its
/// folders resolved: `/tmp` is `/private/tmp`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// A path and everything below it.
    Path(String),
    /// Paths matching a pattern; `base` is the folder it starts from.
    Pattern { regex: String, base: String },
    /// Paths matching an anchored regular expression and everything below
    /// them; `base` is the folder it starts from.
    Below { regex: String, base: String },
}

impl Entry {
    /// For an allow rule.
    fn allow(path: &Path) -> Self {
        if is_pattern(path) {
            let (regex, base) = compile(path);
            Self::Pattern { regex, base }
        } else {
            Self::Path(display(&resolve_name(path)))
        }
    }

    /// For a deny rule: the path as written and as resolved, when they
    /// differ, so a deny holds whichever way the path is reached.
    fn deny(path: &Path) -> Vec<Self> {
        if is_pattern(path) {
            let (regex, base) = compile(path);
            return vec![Self::Below {
                regex: below(&regex),
                base,
            }];
        }
        let resolved = display(&resolve_all(path));
        let written = display(&lexical(path));
        if written == resolved {
            vec![Self::Path(resolved)]
        } else {
            vec![Self::Path(written), Self::Path(resolved)]
        }
    }

    /// The SBPL filter matching what an allow of this entry allows.
    fn allow_filter(&self) -> String {
        match self {
            Self::Path(path) => format!("(subpath {})", quote(path)),
            Self::Pattern { regex, .. } | Self::Below { regex, .. } => {
                format!("(regex {})", quote(regex))
            }
        }
    }

    /// The SBPL filter matching what a deny of this entry denies: a pattern
    /// also covers everything below each match.
    fn deny_filter(&self) -> String {
        match self {
            Self::Path(path) => format!("(subpath {})", quote(path)),
            Self::Pattern { regex, .. } => format!("(regex {})", quote(&below(regex))),
            Self::Below { regex, .. } => format!("(regex {})", quote(regex)),
        }
    }

    /// The folder the entry starts from: its path, or a pattern's base.
    fn base(&self) -> &str {
        match self {
            Self::Path(path) => path,
            Self::Pattern { base, .. } | Self::Below { base, .. } => base,
        }
    }

    fn is_pattern(&self) -> bool {
        !matches!(self, Self::Path(_))
    }
}

/// The Seatbelt profile for `policy` with no network limit, for a command
/// that runs in `cwd`. `home` is the user's home folder.
pub(crate) fn profile(policy: &Policy, cwd: &Path, home: Option<&Path>) -> String {
    let mut out = vec![
        "(version 1)".to_owned(),
        format!("(deny default (with message {}))", quote(LOG_TAG)),
        String::new(),
        PREAMBLE.to_owned(),
        "; Network: open".to_owned(),
        "(allow network*)".to_owned(),
        String::new(),
    ];

    let read_denies: Vec<Entry> = policy
        .deny_read
        .iter()
        .flat_map(|path| Entry::deny(path))
        .collect();
    let write_roots = write_roots(policy, home, &read_denies);

    out.push("; File read".to_owned());
    out.push("(allow file-read*)".to_owned());
    if !read_denies.is_empty() {
        let denies = read_denies.iter().map(Entry::deny_filter).collect();
        out.extend(rule("deny", "file-read*", denies));
        out.push("(allow file-read-metadata".to_owned());
        out.push("  (vnode-type DIRECTORY))".to_owned());
        out.extend(rule(
            "deny",
            "file-write-unlink file-write-create",
            move_blocking(&read_denies),
        ));
        let roots = write_roots.iter().map(Entry::allow_filter).collect();
        out.extend(rule("allow", "file-write-unlink file-write-create", roots));
    }
    out.push(String::new());

    out.push("; File write".to_owned());
    let roots = write_roots.iter().map(Entry::allow_filter).collect();
    out.extend(rule("allow", "file-write*", roots));
    let write_denies = write_denies(policy, cwd);
    let denies = write_denies.iter().map(Entry::deny_filter).collect();
    out.extend(rule("deny", "file-write*", denies));
    out.extend(rule(
        "deny",
        "file-write-unlink file-write-create",
        move_blocking(&write_denies),
    ));

    let keep = keep_in_place(&read_denies, &write_roots);
    if !keep.is_empty() {
        out.push(String::new());
        out.push("; File read: keep read-denied paths inside write roots in place".to_owned());
        out.extend(rule("deny", "file-write-unlink", keep));
    }
    out.push(String::new());
    out.join("\n")
}

/// Everything writable: the sandbox's own paths, the home folder's
/// convenience folders (unless a read deny covers them) and the policy's.
fn write_roots(policy: &Policy, home: Option<&Path>, read_denies: &[Entry]) -> Vec<Entry> {
    let mut roots: Vec<Entry> = OWN_WRITE_PATHS
        .iter()
        .map(|path| Entry::Path((*path).to_owned()))
        .collect();
    if let Some(home) = home {
        for dir in HOME_WRITE_DIRS {
            let entry = Entry::allow(&home.join(dir));
            let hidden = read_denies.iter().any(|deny| match deny {
                Entry::Path(denied) => is_at_or_under(entry.base(), denied),
                _ => false,
            });
            if !hidden {
                push_unique(&mut roots, entry);
            }
        }
    }
    for write in &policy.write {
        let entry = match write {
            WritePath::Tree(path) | WritePath::File(path) => Entry::allow(path),
        };
        push_unique(&mut roots, entry);
    }
    roots
}

/// The policy's write denies and the mandatory ones next to `cwd`: each
/// name in `cwd` itself and anywhere below it.
fn write_denies(policy: &Policy, cwd: &Path) -> Vec<Entry> {
    let mut denies: Vec<Entry> = Vec::new();
    for path in &policy.deny_write {
        for entry in Entry::deny(path) {
            push_unique(&mut denies, entry);
        }
    }
    let cwd = display(&resolve_all(cwd));
    for name in MANDATORY_DENIES {
        push_unique(&mut denies, Entry::Path(format!("{cwd}/{name}")));
        push_unique(
            &mut denies,
            Entry::Below {
                regex: format!(
                    "^{}/(.*/)?{}(/.*)?$",
                    escape_regex(&cwd),
                    escape_regex(name)
                ),
                base: cwd.clone(),
            },
        );
    }
    denies
}

/// Filters that keep `entries` from being moved or deleted, which would
/// lift their rule: the entries, a pattern's base and every folder above.
fn move_blocking(entries: &[Entry]) -> Vec<String> {
    let mut filters = Vec::new();
    for entry in entries {
        push_unique(&mut filters, entry.deny_filter());
        let base = entry.base();
        if entry.is_pattern() {
            if base == "/" {
                continue;
            }
            push_unique(&mut filters, format!("(literal {})", quote(base)));
        }
        for ancestor in ancestors(base) {
            push_unique(&mut filters, format!("(literal {})", quote(&ancestor)));
        }
    }
    filters
}

/// Read-denied paths inside a write root could be renamed out from under
/// their deny once the write root's unlink is allowed again: deny unlink
/// of them, and of the folders between them and the write root.
fn keep_in_place(read_denies: &[Entry], write_roots: &[Entry]) -> Vec<String> {
    let roots: Vec<&str> = write_roots
        .iter()
        .filter_map(|root| match root {
            Entry::Path(path) => Some(path.as_str()),
            _ => None,
        })
        .collect();
    let strictly_below_root = |path: &str| roots.iter().any(|root| is_strictly_under(path, root));
    let mut filters = Vec::new();
    for deny in read_denies {
        match deny {
            Entry::Path(path) => {
                if !strictly_below_root(path) {
                    continue;
                }
                let carve_outs: Vec<String> = write_roots
                    .iter()
                    .filter(|root| is_strictly_under(root.base(), path))
                    .map(|root| format!("(require-not {})", root.allow_filter()))
                    .collect();
                push_unique(&mut filters, carve(deny.deny_filter(), &carve_outs));
                for ancestor in ancestors(path) {
                    if strictly_below_root(&ancestor) {
                        push_unique(&mut filters, format!("(literal {})", quote(&ancestor)));
                    }
                }
            }
            Entry::Pattern { base, .. } | Entry::Below { base, .. } => {
                let intersects = roots
                    .iter()
                    .any(|root| is_at_or_under(base, root) || is_strictly_under(root, base));
                if !intersects {
                    continue;
                }
                push_unique(&mut filters, deny.deny_filter());
                if base != "/" {
                    for dir in std::iter::once(base.clone()).chain(ancestors(base)) {
                        if strictly_below_root(&dir) {
                            push_unique(&mut filters, format!("(literal {})", quote(&dir)));
                        }
                    }
                }
            }
        }
    }
    filters
}

fn carve(filter: String, carve_outs: &[String]) -> String {
    if carve_outs.is_empty() {
        filter
    } else {
        format!("(require-all {filter} {})", carve_outs.join(" "))
    }
}

/// One rule for `operations` on any path matching one of `filters`;
/// nothing for no filters (a rule without a filter matches everything).
fn rule(action: &str, operations: &str, filters: Vec<String>) -> Vec<String> {
    if filters.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!("({action} {operations}")];
    lines.extend(filters.into_iter().map(|filter| format!("  {filter}")));
    lines.push(format!("  (with message {}))", quote(LOG_TAG)));
    lines
}

/// A pattern's anchored regular expression, with the folder it starts from
/// resolved, and that folder.
fn compile(pattern: &Path) -> (String, String) {
    let base = pattern_base(pattern);
    let resolved_base = display(&resolve_all(&base));
    let rest = pattern.strip_prefix(&base).unwrap_or(pattern);
    let tail = pattern_regex(&format!("/{}", rest.display()));
    let joined = if resolved_base == "/" {
        ""
    } else {
        &resolved_base
    };
    let regex = format!(
        "^{}{}",
        escape_regex(joined),
        tail.strip_prefix('^').unwrap_or(&tail)
    );
    (regex, resolved_base)
}

/// A regular expression for what `regex` matches and everything below.
fn below(regex: &str) -> String {
    format!("{}(/.*)?$", regex.strip_suffix('$').unwrap_or(regex))
}

/// An SBPL string literal.
fn quote(text: &str) -> String {
    serde_json::to_string(text).expect("a string serialises")
}

fn push_unique<T: PartialEq>(items: &mut Vec<T>, item: T) {
    if !items.contains(&item) {
        items.push(item);
    }
}

/// The folders above `path`, nearest first, without the root.
fn ancestors(path: &str) -> Vec<String> {
    Path::new(path)
        .ancestors()
        .skip(1)
        .filter(|ancestor| *ancestor != Path::new("/") && !ancestor.as_os_str().is_empty())
        .map(display)
        .collect()
}

fn is_at_or_under(path: &str, dir: &str) -> bool {
    Path::new(path).starts_with(dir)
}

fn is_strictly_under(path: &str, dir: &str) -> bool {
    path != dir && is_at_or_under(path, dir)
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `path` without `.` and `..` (taken textually).
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// `path` with every symbolic link resolved, as far as it exists.
fn resolve_all(path: &Path) -> PathBuf {
    let path = lexical(path);
    let mut existing = path.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(resolved) = existing.canonicalize() {
            let mut out = resolved;
            out.extend(rest.iter().rev());
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return path,
        }
    }
}

/// `path` with its folders resolved but its own name kept, so that a link
/// planted at an allowed name does not open what it points to.
fn resolve_name(path: &Path) -> PathBuf {
    let path = lexical(path);
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => resolve_all(parent).join(name),
        _ => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Network;

    fn policy() -> Policy {
        Policy {
            write: vec![
                WritePath::Tree("/work/wt".into()),
                WritePath::File("/work/repo/.git/refs/heads/agent/x".into()),
                WritePath::File("/work/repo/.git/refs/heads/agent/x.lock".into()),
                WritePath::File("/private/tmp/claude-*-cwd".into()),
                WritePath::Tree("/private/tmp/claude-501/fswatch-probe-**".into()),
            ],
            deny_write: vec![
                "/work/repo/.git/hooks".into(),
                "/work/repo/.git/config".into(),
            ],
            deny_read: vec!["/Users/u/.ssh".into(), "/work/wt/secret".into()],
            network: Network::Open,
        }
    }

    fn generated() -> String {
        profile(
            &policy(),
            Path::new("/work/wt"),
            Some(Path::new("/Users/u")),
        )
    }

    #[test]
    fn has_no_network_limit_and_keeps_the_keychain_services() {
        let text = generated();
        assert!(text.contains("\n(allow network*)\n"));
        assert!(!text.contains("localhost"));
        assert!(text.contains(r#"(global-name "com.apple.SecurityServer")"#));
        assert!(text.contains(r#"(global-name "com.apple.securityd.xpc")"#));
        assert!(text.starts_with("(version 1)\n(deny default"));
    }

    #[test]
    fn allows_writes_to_trees_files_and_patterns() {
        let text = generated();
        let allow = section(&text, "(allow file-write*");
        assert!(allow.contains(r#"(subpath "/work/wt")"#));
        assert!(allow.contains(r#"(subpath "/work/repo/.git/refs/heads/agent/x.lock")"#));
        assert!(allow.contains(r#"(regex "^/private/tmp/claude-[^/]*-cwd$")"#));
        assert!(allow.contains(r#"(regex "^/private/tmp/claude-501/fswatch-probe-.*$")"#));
        assert!(allow.contains(r#"(subpath "/dev/null")"#));
        assert!(allow.contains(r#"(subpath "/Users/u/.npm/_logs")"#));
    }

    #[test]
    fn denies_writes_after_allowing_them() {
        let text = generated();
        let allow_at = text.find("(allow file-write*").expect("an allow rule");
        let deny_at = text.find("(deny file-write*").expect("a deny rule");
        assert!(allow_at < deny_at, "the deny comes last and wins");
        let deny = section(&text, "(deny file-write*");
        assert!(deny.contains(r#"(subpath "/work/repo/.git/hooks")"#));
        assert!(deny.contains(r#"(subpath "/work/repo/.git/config")"#));
        assert!(deny.contains(r#"(subpath "/work/wt/.mcp.json")"#));
        assert!(deny.contains(r#"(regex "^/work/wt/(.*/)?\\.git/hooks(/.*)?$")"#));
        let moves = section(
            &text,
            "(deny file-write-unlink file-write-create\n  (subpath \"/work/repo",
        );
        assert!(moves.contains(r#"(literal "/work/repo/.git")"#));
        assert!(moves.contains(r#"(literal "/work")"#));
    }

    #[test]
    fn hides_denied_reads() {
        let text = generated();
        let deny = section(&text, "(deny file-read*");
        assert!(deny.contains(r#"(subpath "/Users/u/.ssh")"#));
        assert!(deny.contains(r#"(subpath "/work/wt/secret")"#));
        assert!(text.contains("(allow file-read-metadata\n  (vnode-type DIRECTORY))"));
        let keep = section(&text, "(deny file-write-unlink\n");
        assert!(keep.contains(r#"(subpath "/work/wt/secret")"#));
        assert!(!keep.contains(".ssh"));
    }

    #[test]
    fn leaves_home_folders_writable_unless_hidden() {
        let mut policy = policy();
        policy.deny_read.push("/Users/u/.npm".into());
        let text = profile(&policy, Path::new("/work/wt"), Some(Path::new("/Users/u")));
        assert!(!section(&text, "(allow file-write*").contains(".npm/_logs"));
        assert!(section(&text, "(allow file-write*").contains(".claude/debug"));
    }

    #[test]
    fn without_read_denies_reads_are_open() {
        let mut policy = policy();
        policy.deny_read.clear();
        let text = profile(&policy, Path::new("/work/wt"), None);
        assert!(!text.contains("(deny file-read*"));
        assert!(!text.contains("vnode-type DIRECTORY"));
    }

    #[test]
    fn resolves_linked_folders() {
        // `/tmp` is a link to `/private/tmp` on macOS; elsewhere it is not.
        let resolved = resolve_name(Path::new("/tmp/gitez-no-such-dir/x"));
        let tmp = Path::new("/tmp").canonicalize().expect("/tmp exists");
        assert_eq!(resolved, tmp.join("gitez-no-such-dir/x"));
        assert_eq!(lexical(Path::new("/a/./b/../c")), Path::new("/a/c"));
    }

    /// The rule that starts with `start`, up to its closing line.
    fn section<'a>(text: &'a str, start: &str) -> &'a str {
        let at = text
            .find(start)
            .unwrap_or_else(|| panic!("no rule {start}"));
        let rest = &text[at..];
        let end = rest
            .find("(with message")
            .expect("a rule ends with its message");
        &rest[..end]
    }
}
