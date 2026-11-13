//! bubblewrap arguments for the Open network profile on Linux, which the
//! `srt` command cannot express: the same file rules as `srt` on Linux, and
//! no network namespace. Untested: GitEZ is developed on macOS.
//!
//! bubblewrap binds existing paths only, so a file the policy allows is
//! made writable through the folder that holds it (git replaces ref and
//! lock files by renaming them, which a bound file would not allow), and
//! patterns are left out. A file whose folder holds a path the policy
//! denies (such as `packed-refs.lock`, in a git dir with `hooks` and
//! `config`) is left out too: binding that folder would make everything
//! else in it writable (`packed-refs`, `HEAD`, `info/attributes`, …).

use std::path::{Path, PathBuf};

use crate::policy::{Policy, WritePath, is_pattern};
use crate::seatbelt::MANDATORY_DENIES;

/// The folders `policy` makes writable on Linux: its trees, and the folder
/// of each file unless that folder holds (or is) a path the policy denies.
/// Patterns are left out.
pub(crate) fn write_dirs(policy: &Policy) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for write in &policy.write {
        if is_pattern(write.path()) {
            continue;
        }
        let dir = match write {
            WritePath::Tree(path) => Some(path.clone()),
            WritePath::File(path) => path
                .parent()
                .filter(|folder| {
                    !policy
                        .deny_write
                        .iter()
                        .any(|denied| !is_pattern(denied) && denied.starts_with(folder))
                })
                .map(Path::to_path_buf),
        };
        if let Some(dir) = dir
            && !dirs.contains(&dir)
        {
            dirs.push(dir);
        }
    }
    dirs
}

/// The bubblewrap options (before `--` and the command) that run a command
/// in `cwd` under `policy` with the host's network. Paths that do not exist
/// are left out, as bubblewrap cannot bind them.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn args(
    policy: &Policy,
    cwd: &Path,
    home: Option<&Path>,
    exists: impl Fn(&Path) -> Option<bool>,
) -> Vec<String> {
    let mut args: Vec<String> = ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]
        .map(String::from)
        .to_vec();
    let mut writable = vec![PathBuf::from("/tmp/claude")];
    if let Some(home) = home {
        writable.extend([home.join(".npm/_logs"), home.join(".claude/debug")]);
    }
    writable.extend(write_dirs(policy));
    for dir in &writable {
        if exists(dir).is_some() {
            bind(&mut args, "--bind", dir, dir);
        }
    }

    let mandatory = MANDATORY_DENIES.iter().map(|name| cwd.join(name));
    for path in policy.deny_write.iter().cloned().chain(mandatory) {
        if !is_pattern(&path) && exists(&path).is_some() {
            bind(&mut args, "--ro-bind", &path, &path);
        }
    }

    for path in &policy.deny_read {
        if is_pattern(path) {
            continue;
        }
        match exists(path) {
            Some(true) => {
                args.push("--tmpfs".to_owned());
                args.push(path.to_string_lossy().into_owned());
            }
            Some(false) => bind(&mut args, "--ro-bind", Path::new("/dev/null"), path),
            None => {}
        }
    }

    args.push("--chdir".to_owned());
    args.push(cwd.to_string_lossy().into_owned());
    args
}

fn bind(args: &mut Vec<String>, option: &str, from: &Path, to: &Path) {
    args.push(option.to_owned());
    args.push(from.to_string_lossy().into_owned());
    args.push(to.to_string_lossy().into_owned());
}

/// Whether `path` exists, and if so whether it is a folder.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn on_disk(path: &Path) -> Option<bool> {
    std::fs::metadata(path)
        .ok()
        .map(|metadata| metadata.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Network;

    #[test]
    fn binds_trees_and_the_folders_of_files_then_hides_what_is_denied() {
        let policy = Policy {
            write: vec![
                WritePath::Tree("/w/wt".into()),
                WritePath::File("/w/repo/.git/refs/heads/agent/x".into()),
                WritePath::File("/w/repo/.git/refs/heads/agent/x.lock".into()),
                WritePath::File("/tmp/claude-*-cwd".into()),
            ],
            deny_write: vec!["/w/repo/.git/hooks".into()],
            deny_read: vec!["/h/.ssh".into(), "/h/.netrc".into(), "/h/.aws".into()],
            network: Network::Open,
            certificate_service: false,
        };
        let exists = |path: &Path| match path.to_str() {
            Some("/h/.ssh") | Some("/w/wt") | Some("/w/repo/.git/refs/heads/agent") => Some(true),
            Some("/w/repo/.git/hooks") | Some("/tmp/claude") => Some(true),
            Some("/h/.netrc") | Some("/w/wt/.mcp.json") => Some(false),
            _ => None,
        };
        let args = args(&policy, Path::new("/w/wt"), Some(Path::new("/h")), exists).join(" ");
        assert_eq!(
            args,
            "--ro-bind / / --dev /dev --proc /proc \
             --bind /tmp/claude /tmp/claude \
             --bind /w/wt /w/wt \
             --bind /w/repo/.git/refs/heads/agent /w/repo/.git/refs/heads/agent \
             --ro-bind /w/repo/.git/hooks /w/repo/.git/hooks \
             --ro-bind /w/wt/.mcp.json /w/wt/.mcp.json \
             --tmpfs /h/.ssh \
             --ro-bind /dev/null /h/.netrc \
             --chdir /w/wt"
        );
        assert!(!args.contains("--unshare-net"));
    }

    /// M1: a thread's policy on Linux binds its own refs' folders but never
    /// the common git dir, whatever lock file in it the policy allows.
    #[test]
    fn a_file_beside_denied_paths_does_not_open_its_folder() {
        let git = Path::new("/w/repo/.git");
        let policy = Policy {
            write: vec![
                WritePath::Tree("/w/wt".into()),
                WritePath::Tree(git.join("objects")),
                WritePath::Tree(git.join("worktrees/t")),
                WritePath::File(git.join("refs/heads/agent/t")),
                WritePath::File(git.join("refs/heads/agent/t.lock")),
                WritePath::File(git.join("logs/refs/heads/agent/t")),
                WritePath::File(git.join("packed-refs.lock")),
            ],
            deny_write: vec![
                git.join("hooks"),
                git.join("config"),
                git.join("gitez"),
                git.join("objects/info/alternates"),
                git.join("worktrees/t/commondir"),
                "/w/wt/.git".into(),
            ],
            deny_read: Vec::new(),
            network: Network::Open,
            certificate_service: false,
        };
        let dirs = write_dirs(&policy);
        assert_eq!(
            dirs,
            [
                PathBuf::from("/w/wt"),
                git.join("objects"),
                git.join("worktrees/t"),
                git.join("refs/heads/agent"),
                git.join("logs/refs/heads/agent"),
            ]
        );
        assert!(!dirs.iter().any(|dir| dir == git), "{dirs:?}");
        let exists = |path: &Path| {
            let text = path.to_string_lossy();
            (text.starts_with("/w/") || text == "/tmp/claude")
                .then_some(!text.ends_with("commondir"))
        };
        let args = args(&policy, Path::new("/w/wt"), None, exists).join(" ");
        assert!(!args.contains("--bind /w/repo/.git /w/repo/.git"), "{args}");
        for denied in [
            "/w/repo/.git/hooks",
            "/w/repo/.git/config",
            "/w/repo/.git/objects/info/alternates",
            "/w/repo/.git/worktrees/t/commondir",
            "/w/wt/.git",
        ] {
            assert!(
                args.contains(&format!("--ro-bind {denied} {denied}")),
                "{denied} read-only in {args}"
            );
        }
    }
}
