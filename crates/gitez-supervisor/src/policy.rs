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

/// The working tree a thread's commands run in.
#[derive(Debug, Clone)]
pub(crate) enum Work {
    /// An Implement thread's own linked worktree and branch, which it
    /// writes.
    Own {
        worktree: PathBuf,
        /// The linked worktree's own git dir, `<common>/worktrees/<name>`.
        worktree_git_dir: PathBuf,
        branch: String,
    },
    /// A Discuss thread: the user's working tree, which it only reads.
    ReadOnly { work_dir: PathBuf },
}

/// Where a thread works.
#[derive(Debug, Clone)]
pub(crate) struct Place {
    pub(crate) common_dir: PathBuf,
    pub(crate) work: Work,
    /// `<data>/tmp/<repo key>-<id>`, the commands' `TMPDIR`.
    pub(crate) tmp_dir: PathBuf,
    /// `<data>/caches/<repo key>-<id>`, the thread's own toolchain caches
    /// (see `toolchains`).
    pub(crate) caches_dir: PathBuf,
    pub(crate) data_dir: PathBuf,
}

impl Place {
    /// The folder its commands run in.
    pub(crate) fn dir(&self) -> &Path {
        match &self.work {
            Work::Own { worktree, .. } => worktree,
            Work::ReadOnly { work_dir } => work_dir,
        }
    }

    /// Whether the thread only reads its working tree (a Discuss thread).
    pub(crate) fn read_only(&self) -> bool {
        matches!(self.work, Work::ReadOnly { .. })
    }
}

/// The policy for a command of the thread at `place`: it may write its
/// worktree, the objects and its own branch's ref files of the repository,
/// its temporary folder and caches, what the agent needs and the folders
/// the user made writable in Settings; never the hooks, the git config,
/// GitEZ's own files in the git dir or GitEZ's settings. The certificate
/// service is open when the network reaches more than the agent's API
/// (`profile` Packages or Open): there the host list already lets data
/// out, and without it pip, Go and other programs that verify
/// certificates through macOS reach no host.
///
/// The files git trusts to find the repository are never writable: the
/// worktree's `.git`, and the `commondir`, `gitdir`, `config`,
/// `config.worktree` and `locked` files of its own git dir (an agent that
/// could change them would point GitEZ's later git commands at a
/// repository of its own); so are the object store's pointers to other
/// stores and its commit graph. The sandbox also keeps these from being
/// moved, replaced or hard linked, and blocks moving the folders above
/// them, the worktree folder included.
///
/// A Discuss thread ([`Work::ReadOnly`]) may write only its temporary
/// folder, its caches and what the agent needs: nothing in the working
/// tree it reads, nothing in the repository's git dir, and not the folders
/// made writable in Settings.
///
/// A writable path whose own name is a symbolic link is left out and
/// returned: the sandbox would follow it, and whoever could write the
/// folder it is in (an earlier run of the agent, for a folder such as its
/// temporary one) could point it anywhere.
pub(crate) fn policy(
    place: &Place,
    needs: &AgentNeeds,
    settings: &Settings,
    network: Network,
    profile: Option<NetworkProfile>,
) -> (Policy, Vec<PathBuf>) {
    let common = real(&place.common_dir);
    let data = real(&place.data_dir);
    let mut write = Vec::new();
    let mut deny_write = Vec::new();
    match &place.work {
        Work::Own {
            worktree,
            worktree_git_dir,
            branch,
        } => {
            let worktree = real(worktree);
            let git_dir = real(worktree_git_dir);
            let branch_ref = Path::new("refs/heads").join(branch);
            write.extend([
                WritePath::Tree(worktree.clone()),
                WritePath::Tree(common.join("objects")),
                WritePath::Tree(git_dir.clone()),
                WritePath::File(real(&common.join(&branch_ref))),
                WritePath::File(real(&common.join(format!("{}.lock", branch_ref.display())))),
                WritePath::File(real(&common.join("logs").join(&branch_ref))),
                // git locks packed-refs during a commit and reports an error
                // the agent could take for a failure when it cannot;
                // packed-refs itself stays read-only.
                WritePath::File(common.join("packed-refs.lock")),
            ]);
            deny_write.push(worktree.join(".git"));
            deny_write.extend(GIT_DIR_POINTERS.iter().map(|name| git_dir.join(name)));
        }
        // Nothing of the working tree or of the git dir is on the list.
        Work::ReadOnly { .. } => {}
    }
    write.extend([
        WritePath::Tree(real(&place.tmp_dir)),
        WritePath::Tree(real(&place.caches_dir)),
    ]);
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
    if !place.read_only() {
        write.extend(
            settings
                .writable_paths()
                .iter()
                .map(|path| WritePath::Tree(real(path))),
        );
    }
    let (write, refused): (Vec<WritePath>, Vec<WritePath>) =
        write.into_iter().partition(|write| !is_link(write.path()));
    deny_write.extend([
        common.join("hooks"),
        common.join("config"),
        common.join("gitez"),
        common.join("objects/info/alternates"),
        common.join("objects/info/http-alternates"),
        common.join("objects/info/commit-graph"),
        common.join("objects/info/commit-graphs"),
        data.join("settings.json"),
        data.join("threads.json"),
    ]);
    let policy = Policy {
        write,
        deny_write,
        deny_read: settings
            .hidden_paths()
            .iter()
            .map(|path| real(path))
            .collect(),
        network,
        certificate_service: downloads(profile),
    };
    (
        policy,
        refused
            .into_iter()
            .map(|write| write.path().to_owned())
            .collect(),
    )
}

/// The files in a linked worktree's git dir that tell git where the
/// repository and the worktree are, or configure it; none of them is
/// written by git after `git worktree add`.
const GIT_DIR_POINTERS: [&str; 5] = ["commondir", "gitdir", "config", "config.worktree", "locked"];

/// Whether `path` itself (not a folder above it) is a symbolic link.
fn is_link(path: &Path) -> bool {
    let pattern = path.to_string_lossy().contains('*');
    !pattern && std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

/// Whether commands with `profile` reach package registries (`None`: no
/// network at all).
pub(crate) fn downloads(profile: Option<NetworkProfile>) -> bool {
    matches!(
        profile,
        Some(NetworkProfile::Packages | NetworkProfile::Open)
    )
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

/// `path` with the symbolic links of its folders resolved as far as they
/// exist (the sandbox sees real paths: `/private/var`, not `/var`), its
/// own name kept: a link there is not followed (see [`policy`]).
pub(crate) fn real(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => real_all(parent).join(name),
        _ => real_all(path),
    }
}

/// `path` with every symbolic link resolved as far as it exists, the rest
/// appended.
fn real_all(path: &Path) -> PathBuf {
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
            work: Work::Own {
                worktree: base.join("wt"),
                worktree_git_dir: common.join("worktrees/wt"),
                branch: "agent/x".to_owned(),
            },
            tmp_dir: base.join("data/tmp/repo-x"),
            caches_dir: base.join("data/caches/repo-x"),
            data_dir: base.join("data"),
        };
        let needs = AgentNeeds {
            write_dirs: vec![base.join("config/projects/p")],
            write_files: vec![base.join("lock")],
            ..AgentNeeds::default()
        };
        let mut settings = Settings::default();
        settings.sandbox.writable_paths = vec![base.join("gradle").to_string_lossy().into_owned()];
        let (policy, refused) = policy(
            &place,
            &needs,
            &settings,
            Network::Hosts(Vec::new()),
            Some(NetworkProfile::AgentOnly),
        );
        assert!(refused.is_empty());
        let has = |write: WritePath| policy.write.contains(&write);
        assert!(has(WritePath::Tree(base.join("wt"))));
        assert!(has(WritePath::Tree(common.join("objects"))));
        assert!(has(WritePath::File(common.join("refs/heads/agent/x"))));
        assert!(has(WritePath::File(common.join("refs/heads/agent/x.lock"))));
        assert!(has(WritePath::File(common.join("logs/refs/heads/agent/x"))));
        assert!(has(WritePath::Tree(base.join("data/tmp/repo-x"))));
        assert!(has(WritePath::Tree(base.join("data/caches/repo-x"))));
        assert!(has(WritePath::Tree(base.join("gradle"))));
        assert!(has(WritePath::Tree(base.join("config/projects/p"))));
        assert!(has(WritePath::File(base.join("lock"))));
        assert!(
            !policy
                .write
                .iter()
                .any(|write| write.path() == common.join("refs/heads/main"))
        );
        for denied in [
            "hooks",
            "config",
            "gitez",
            "objects/info/alternates",
            "objects/info/commit-graph",
        ] {
            assert!(policy.deny_write.contains(&common.join(denied)), "{denied}");
        }
        // (a), (b): the files git trusts to find the repository.
        assert!(policy.deny_write.contains(&base.join("wt/.git")));
        for name in ["commondir", "gitdir", "config", "config.worktree", "locked"] {
            assert!(
                policy
                    .deny_write
                    .contains(&common.join("worktrees/wt").join(name)),
                "{name}"
            );
        }
        assert!(policy.deny_write.contains(&base.join("data/settings.json")));
        assert!(!policy.deny_read.is_empty());
        assert!(!policy.certificate_service, "closed without package hosts");
    }

    #[test]
    fn a_discuss_thread_may_write_nothing_in_the_repository() {
        let dir = tempfile::tempdir().expect("temp dir");
        let base = dir.path().canonicalize().expect("canonical");
        let work_dir = base.join("repo");
        let common = work_dir.join(".git");
        std::fs::create_dir_all(common.join("refs/heads")).expect("dirs");
        let place = Place {
            common_dir: common.clone(),
            work: Work::ReadOnly {
                work_dir: work_dir.clone(),
            },
            tmp_dir: base.join("data/tmp/repo-x"),
            caches_dir: base.join("data/caches/repo-x"),
            data_dir: base.join("data"),
        };
        assert!(place.read_only());
        assert_eq!(place.dir(), work_dir);
        let session = base.join("config/projects/p/6f1c.jsonl");
        let needs = AgentNeeds {
            write_dirs: vec![base.join("config/projects/p/6f1c")],
            write_files: vec![session.clone()],
            ..AgentNeeds::default()
        };
        let mut settings = Settings::default();
        settings.sandbox.writable_paths = vec![base.join("extra").display().to_string()];
        let (policy, refused) = policy(
            &place,
            &needs,
            &settings,
            Network::Hosts(Vec::new()),
            Some(NetworkProfile::AgentOnly),
        );
        assert!(refused.is_empty());
        // Its own scratch folders and the agent's conversation: all of it.
        assert_eq!(
            policy.write,
            [
                WritePath::Tree(base.join("data/tmp/repo-x")),
                WritePath::Tree(base.join("data/caches/repo-x")),
                WritePath::Tree(base.join("config/projects/p/6f1c")),
                WritePath::File(session),
            ]
        );
        for write in &policy.write {
            assert!(
                !write.path().starts_with(&work_dir) && !work_dir.starts_with(write.path()),
                "{write:?} reaches the working tree"
            );
        }
        // The folders of Settings are for threads that build in their own
        // worktree.
        assert!(
            !policy
                .write
                .iter()
                .any(|write| write.path() == base.join("extra"))
        );
        // GitEZ's own files stay denied, should a needed path ever hold
        // them.
        for denied in ["hooks", "config", "gitez"] {
            assert!(policy.deny_write.contains(&common.join(denied)), "{denied}");
        }
        assert!(policy.deny_write.contains(&base.join("data/settings.json")));
        assert!(policy.deny_write.contains(&base.join("data/threads.json")));
        assert!(!policy.deny_read.is_empty(), "hidden paths stay hidden");
    }

    #[test]
    fn the_certificate_service_opens_with_the_package_hosts() {
        let dir = tempfile::tempdir().expect("temp dir");
        let base = dir.path().canonicalize().expect("canonical");
        let place = Place {
            common_dir: base.join("repo/.git"),
            work: Work::Own {
                worktree: base.join("wt"),
                worktree_git_dir: base.join("repo/.git/worktrees/wt"),
                branch: "agent/x".to_owned(),
            },
            tmp_dir: base.join("data/tmp/repo-x"),
            caches_dir: base.join("data/caches/repo-x"),
            data_dir: base.join("data"),
        };
        let open = |profile| {
            policy(
                &place,
                &AgentNeeds::default(),
                &Settings::default(),
                Network::Hosts(Vec::new()),
                profile,
            )
            .0
            .certificate_service
        };
        assert!(!open(None));
        assert!(!open(Some(NetworkProfile::AgentOnly)));
        assert!(open(Some(NetworkProfile::Packages)));
        assert!(open(Some(NetworkProfile::Open)));
    }

    /// (e) A writable folder replaced by a link is not made writable: the
    /// sandbox would follow the link wherever it points.
    #[test]
    fn a_linked_write_path_is_left_out() {
        let dir = tempfile::tempdir().expect("temp dir");
        let base = dir.path().canonicalize().expect("canonical");
        let home = base.join("home");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(base.join("data/tmp")).expect("tmp");
        let tmp = base.join("data/tmp/repo-x");
        std::os::unix::fs::symlink(&home, &tmp).expect("link");
        let lock = base.join("locks");
        std::os::unix::fs::symlink(&home, &lock).expect("link");
        let place = Place {
            common_dir: base.join("repo/.git"),
            work: Work::Own {
                worktree: base.join("wt"),
                worktree_git_dir: base.join("repo/.git/worktrees/wt"),
                branch: "agent/x".to_owned(),
            },
            tmp_dir: tmp.clone(),
            caches_dir: base.join("data/caches/repo-x"),
            data_dir: base.join("data"),
        };
        let needs = AgentNeeds {
            write_dirs: vec![lock.clone()],
            ..AgentNeeds::default()
        };
        let (policy, refused) = policy(
            &place,
            &needs,
            &Settings::default(),
            Network::Hosts(Vec::new()),
            None,
        );
        assert_eq!(refused, [tmp.clone(), lock.clone()]);
        for write in &policy.write {
            assert!(!write.path().starts_with(&home), "{write:?}");
        }
        assert_eq!(real(&tmp), tmp, "the link itself is named, not followed");
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
