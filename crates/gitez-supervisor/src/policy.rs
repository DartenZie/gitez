//! What the sandbox allows a thread's commands, and the environment they
//! get (DESIGN D.7).

use std::path::{Path, PathBuf};

use gitez_agent::AgentNeeds;
use gitez_sandbox::{Network, NetworkProfile, Policy, WritePath};
use gitez_settings::Settings;

use crate::Secrets;

/// Variables that would point git at another repository than the one the
/// command runs in; GitEZ may have been started from a git hook.
pub(crate) const ENV_REMOVE: [&str; 5] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
];

/// Where a thread works.
#[derive(Debug, Clone)]
pub(crate) struct Place {
    pub(crate) common_dir: PathBuf,
    pub(crate) worktree: PathBuf,
    /// The linked worktree's own git dir, `<common>/worktrees/<name>`.
    pub(crate) worktree_git_dir: PathBuf,
    pub(crate) branch: String,
    /// `<data>/tmp/<repo key>-<id>`, the commands' `TMPDIR`.
    pub(crate) tmp_dir: PathBuf,
    pub(crate) data_dir: PathBuf,
}

/// The policy for a command of the thread at `place`: it may write its
/// worktree, the objects and its own branch's ref files of the repository,
/// its temporary folder and what the agent needs; never the hooks, the
/// git config, GitEZ's own files in the git dir or GitEZ's settings.
pub(crate) fn policy(
    place: &Place,
    needs: &AgentNeeds,
    settings: &Settings,
    network: Network,
) -> Policy {
    let common = real(&place.common_dir);
    let branch_ref = Path::new("refs/heads").join(&place.branch);
    let mut write = vec![
        WritePath::Tree(real(&place.worktree)),
        WritePath::Tree(common.join("objects")),
        WritePath::Tree(real(&place.worktree_git_dir)),
        WritePath::File(real(&common.join(&branch_ref))),
        WritePath::File(real(&common.join(format!("{}.lock", branch_ref.display())))),
        WritePath::File(real(&common.join("logs").join(&branch_ref))),
        // git locks packed-refs during a commit and reports an error the
        // agent could take for a failure when it cannot; packed-refs itself
        // stays read-only.
        WritePath::File(common.join("packed-refs.lock")),
        WritePath::Tree(real(&place.tmp_dir)),
    ];
    write.extend(
        needs
            .write_dirs
            .iter()
            .map(|dir| WritePath::Tree(real(dir))),
    );
    write.extend(
        needs
            .write_files
            .iter()
            .map(|file| WritePath::File(real(file))),
    );
    let data = real(&place.data_dir);
    Policy {
        write,
        deny_write: vec![
            common.join("hooks"),
            common.join("config"),
            common.join("gitez"),
            data.join("settings.json"),
            data.join("threads.json"),
        ],
        deny_read: settings
            .hidden_paths()
            .iter()
            .map(|path| real(path))
            .collect(),
        network,
    }
}

/// The hosts a network profile allows: the agent's own; with Packages also
/// the package registries in Settings and the hosts of the repository's
/// remotes.
pub(crate) fn network(
    profile: NetworkProfile,
    agent_hosts: &[String],
    settings: &Settings,
    remote_urls: &[String],
) -> Network {
    let mut hosts: Vec<String> = Vec::new();
    let mut add = |host: &str| {
        let host = host.trim();
        if !host.is_empty() && !hosts.iter().any(|known| known == host) {
            hosts.push(host.to_owned());
        }
    };
    match profile {
        NetworkProfile::Open => return Network::Open,
        NetworkProfile::AgentOnly => agent_hosts.iter().for_each(|host| add(host)),
        NetworkProfile::Packages => {
            agent_hosts.iter().for_each(|host| add(host));
            settings
                .sandbox
                .package_hosts
                .iter()
                .for_each(|host| add(host));
            remote_urls
                .iter()
                .filter_map(|url| remote_host(url))
                .for_each(|host| add(&host));
        }
    }
    Network::Hosts(hosts)
}

/// The environment GitEZ adds to every command it runs for a thread, after
/// the agent's own: its temporary folder, no terminal prompts, no
/// background gc or maintenance (they would write outside what the sandbox
/// allows), no credential helper (so `git push` has no credentials), no
/// commit signing (the sandbox hides the keys); then the secrets.
pub(crate) fn env(tmp_dir: &Path, secrets: &Secrets) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = [
        ("TMPDIR", tmp_dir.to_string_lossy().as_ref()),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_CONFIG_COUNT", "4"),
        ("GIT_CONFIG_KEY_0", "gc.auto"),
        ("GIT_CONFIG_VALUE_0", "0"),
        ("GIT_CONFIG_KEY_1", "maintenance.auto"),
        ("GIT_CONFIG_VALUE_1", "false"),
        ("GIT_CONFIG_KEY_2", "credential.helper"),
        ("GIT_CONFIG_VALUE_2", ""),
        ("GIT_CONFIG_KEY_3", "commit.gpgsign"),
        ("GIT_CONFIG_VALUE_3", "false"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .collect();
    env.extend(secrets.env());
    env
}

/// The host of a git remote URL: `https://host/…`, `ssh://user@host:22/…`,
/// or scp-like `user@host:path`. `None` for local paths.
pub(crate) fn remote_host(url: &str) -> Option<String> {
    let url = url.trim();
    let authority = if let Some((scheme, rest)) = url.split_once("://") {
        if scheme == "file" {
            return None;
        }
        rest.split('/').next()?
    } else {
        // scp-like: a colon before any slash.
        let (host, _) = url.split_once(':')?;
        if host.contains('/') {
            return None;
        }
        host
    };
    let host = authority.rsplit('@').next()?;
    let host = if let Some(v6) = host.strip_prefix('[') {
        v6.split(']').next()?
    } else {
        host.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// `path` with symbolic links resolved as far as it exists (the sandbox
/// sees real paths: `/private/var`, not `/var`), the rest appended.
pub(crate) fn real(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(mut real) = existing.canonicalize() {
            real.extend(rest.iter().rev());
            return real;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return path.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_hosts() {
        assert_eq!(
            remote_host("https://github.com/a/b.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            remote_host("ssh://git@GitLab.example.com:2222/a/b").as_deref(),
            Some("gitlab.example.com")
        );
        assert_eq!(
            remote_host("git@github.com:a/b.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            remote_host("https://user:pw@[::1]:8080/x").as_deref(),
            Some("::1")
        );
        assert_eq!(remote_host("/srv/git/repo.git"), None);
        assert_eq!(remote_host("../other"), None);
        assert_eq!(remote_host("file:///srv/repo"), None);
    }

    #[test]
    fn profiles_widen_the_host_list() {
        let settings = Settings::default();
        let agent = vec!["api.anthropic.com".to_owned()];
        let remotes = vec!["git@example.org:me/repo.git".to_owned()];
        assert_eq!(
            network(NetworkProfile::AgentOnly, &agent, &settings, &remotes),
            Network::Hosts(agent.clone())
        );
        let Network::Hosts(packages) =
            network(NetworkProfile::Packages, &agent, &settings, &remotes)
        else {
            panic!("packages is a host list");
        };
        assert_eq!(packages[0], "api.anthropic.com");
        assert!(packages.iter().any(|host| host == "crates.io"));
        assert!(packages.iter().any(|host| host == "example.org"));
        assert_eq!(
            network(NetworkProfile::Open, &agent, &settings, &remotes),
            Network::Open
        );
    }

    #[test]
    fn the_policy_allows_only_the_threads_own_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let base = dir.path().canonicalize().expect("canonical");
        let common = base.join("repo/.git");
        std::fs::create_dir_all(common.join("refs/heads/agent")).expect("dirs");
        let place = Place {
            common_dir: common.clone(),
            worktree: base.join("wt"),
            worktree_git_dir: common.join("worktrees/wt"),
            branch: "agent/x".to_owned(),
            tmp_dir: base.join("data/tmp/repo-x"),
            data_dir: base.join("data"),
        };
        let needs = AgentNeeds {
            write_dirs: vec![base.join("config/projects/p")],
            write_files: vec![base.join("lock")],
            hosts: Vec::new(),
        };
        let policy = policy(&place, &needs, &Settings::default(), Network::Open);
        let has = |write: WritePath| policy.write.contains(&write);
        assert!(has(WritePath::Tree(base.join("wt"))));
        assert!(has(WritePath::Tree(common.join("objects"))));
        assert!(has(WritePath::File(common.join("refs/heads/agent/x"))));
        assert!(has(WritePath::File(common.join("refs/heads/agent/x.lock"))));
        assert!(has(WritePath::File(common.join("logs/refs/heads/agent/x"))));
        assert!(has(WritePath::Tree(base.join("data/tmp/repo-x"))));
        assert!(has(WritePath::Tree(base.join("config/projects/p"))));
        assert!(has(WritePath::File(base.join("lock"))));
        assert!(
            !policy
                .write
                .iter()
                .any(|write| write.path() == common.join("refs/heads/main"))
        );
        for denied in ["hooks", "config", "gitez"] {
            assert!(policy.deny_write.contains(&common.join(denied)));
        }
        assert!(policy.deny_write.contains(&base.join("data/settings.json")));
        assert!(!policy.deny_read.is_empty());
    }

    #[test]
    fn env_turns_off_signing_and_credentials_and_adds_secrets() {
        let secrets = Secrets::new([("TOKEN".to_owned(), "value-123".to_owned())].into());
        let env = env(Path::new("/tmp/t"), &secrets);
        let get = |name: &str| {
            env.iter()
                .rev()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(get("TMPDIR"), Some("/tmp/t"));
        assert_eq!(get("GIT_CONFIG_COUNT"), Some("4"));
        assert_eq!(get("GIT_CONFIG_KEY_3"), Some("commit.gpgsign"));
        assert_eq!(get("GIT_CONFIG_VALUE_2"), Some(""));
        assert_eq!(env.last().map(|(name, _)| name.as_str()), Some("TOKEN"));
    }
}
