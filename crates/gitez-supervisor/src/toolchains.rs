//! The package and build caches of the toolchains a thread's commands run.
//!
//! The user's own caches (`~/.cargo/registry`, `~/.npm/_cacache`, …) stay
//! read-only in the sandbox: what is written there is later read, and
//! compiled or run, by the user's own builds outside the sandbox, so a
//! command that could write them could plant code that runs unsandboxed
//! (Cargo, for one, never checks extracted sources again). A command that
//! may download (network profile Packages or Open) gets caches of its own
//! instead, in the thread's caches folder: each one a copy-on-write clone
//! of the user's (APFS `clonefile`, about half a second for a 1.5 GB Cargo
//! registry), so nothing already downloaded is fetched again. Once a
//! thread has its own cache, every later command of the thread uses it,
//! whatever the profile, so what it downloaded stays available. Without
//! network the user's caches are read, and Cargo and Go are told they are
//! offline, so they fail at once with their own clear message instead of
//! retrying a refused host.
//!
//! Some caches are written by every build, not only by downloads (Go's
//! build cache): those are always the thread's own.
//!
//! The cost: Cargo fingerprints a dependency by the path of its sources,
//! which are in the Cargo home, so a thread with its own Cargo home
//! compiles its dependencies once more even with a warm cloned `target/`
//! (measured: every crate of the test project again; with the user's
//! home, none). A user who prefers speed can make the user's cache
//! writable in Settings (`sandbox.writable_paths`, such as
//! `~/.cargo/registry` and `~/.cargo/git`): a cache covered there is used
//! directly, shared with the user's own builds.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// One tool's cache.
#[derive(Debug, Clone, Copy)]
struct Cache {
    /// The variable that points the tool at its cache.
    var: &'static str,
    /// Its folder in the thread's caches folder.
    name: &'static str,
    /// Where the user's cache is.
    user: fn(&Lookup) -> Option<PathBuf>,
    /// What of the user's cache is cloned: these parts of it, or the whole
    /// folder when empty.
    parts: &'static [&'static str],
    /// Files copied from the user's cache (configuration).
    files: &'static [&'static str],
    /// More variables pointing into the thread's cache: name, path in it.
    more: &'static [(&'static str, &'static str)],
    /// Written by every run, not only by downloads.
    always: bool,
}

/// What the user's cache locations depend on.
#[derive(Debug, Clone)]
pub(crate) struct Lookup {
    pub(crate) home: Option<PathBuf>,
    /// The supervisor's own environment (the GUI's).
    pub(crate) var: fn(&str) -> Option<String>,
}

const CACHES: [Cache; 6] = [
    // Cargo: its home holds the registry index, downloaded crates, their
    // extracted sources and git checkouts; `bin/` and credentials stay out.
    Cache {
        var: "CARGO_HOME",
        name: "cargo",
        user: |lookup| {
            lookup
                .dir_var("CARGO_HOME")
                .or_else(|| lookup.home(".cargo"))
        },
        parts: &["registry", "git"],
        files: &["config.toml", "config"],
        more: &[],
        always: false,
    },
    Cache {
        var: "npm_config_cache",
        name: "npm",
        user: |lookup| lookup.home(".npm"),
        parts: &["_cacache"],
        files: &[],
        more: &[],
        always: false,
    },
    Cache {
        var: "PIP_CACHE_DIR",
        name: "pip",
        user: |lookup| {
            lookup.dir_var("PIP_CACHE_DIR").or_else(|| {
                if cfg!(target_os = "macos") {
                    lookup.home("Library/Caches/pip")
                } else {
                    lookup.xdg_cache("pip")
                }
            })
        },
        parts: &[],
        files: &[],
        more: &[],
        always: false,
    },
    // The package managers Node's corepack downloads (pnpm, yarn).
    Cache {
        var: "COREPACK_HOME",
        name: "corepack",
        user: |lookup| {
            lookup
                .dir_var("COREPACK_HOME")
                .or_else(|| lookup.xdg_cache("node/corepack"))
        },
        parts: &[],
        files: &[],
        more: &[],
        always: false,
    },
    // Go keeps downloaded modules in `$GOPATH/pkg/mod` (unless
    // `GOMODCACHE` says otherwise) and the checksum database it verifies
    // them with in `$GOPATH/pkg/sumdb`; `go install` writes `$GOPATH/bin`,
    // which is not cloned.
    Cache {
        var: "GOPATH",
        name: "go",
        user: |lookup| lookup.dir_var("GOPATH").or_else(|| lookup.home("go")),
        parts: &["pkg/mod", "pkg/sumdb"],
        files: &[],
        more: &[("GOMODCACHE", "pkg/mod")],
        always: false,
    },
    // `go build` fails when it cannot write its build cache.
    Cache {
        var: "GOCACHE",
        name: "go-build",
        user: |lookup| {
            lookup.dir_var("GOCACHE").or_else(|| {
                if cfg!(target_os = "macos") {
                    lookup.home("Library/Caches/go-build")
                } else {
                    lookup.xdg_cache("go-build")
                }
            })
        },
        parts: &[],
        files: &[],
        more: &[],
        always: true,
    },
];

/// Set when the command has no network for packages: the tools fail at
/// once rather than retry a refused host.
const OFFLINE: [(&str, &str); 2] = [("CARGO_NET_OFFLINE", "true"), ("GOPROXY", "off")];

/// Set for every command: no update checks that the sandbox would refuse.
const QUIET: [(&str, &str); 1] = [("npm_config_update_notifier", "false")];

impl Lookup {
    pub(crate) fn system() -> Self {
        Self {
            home: std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from),
            var: |name| std::env::var(name).ok(),
        }
    }

    fn home(&self, relative: &str) -> Option<PathBuf> {
        self.home.as_ref().map(|home| home.join(relative))
    }

    /// An absolute folder from the environment.
    fn dir_var(&self, name: &str) -> Option<PathBuf> {
        (self.var)(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    }

    fn xdg_cache(&self, relative: &str) -> Option<PathBuf> {
        self.dir_var("XDG_CACHE_HOME")
            .or_else(|| self.home(".cache"))
            .map(|cache| cache.join(relative))
    }
}

/// What [`prepare`] found: the environment for the command, and what it
/// did, for the supervisor's log.
#[derive(Debug, Default)]
pub(crate) struct Prepared {
    pub(crate) env: Vec<(String, String)>,
    pub(crate) log: Vec<String>,
}

/// The cache environment of a command of the thread whose caches folder is
/// `dir`: `downloads` when its network profile reaches package registries;
/// `writable` the folders Settings make writable. Creates the thread's
/// caches it needs (cloned from the user's, see the module documentation).
/// A cache that cannot be cloned starts empty; one that cannot be created
/// at all is left to the user's cache, read-only.
pub(crate) fn prepare(
    dir: &Path,
    downloads: bool,
    writable: &[PathBuf],
    lookup: &Lookup,
) -> Prepared {
    let mut prepared = Prepared::default();
    for cache in CACHES {
        if shared(cache, writable, lookup) {
            continue;
        }
        let own = dir.join(cache.name);
        let exists = fs::symlink_metadata(&own).is_ok_and(|meta| meta.is_dir());
        if !exists && (downloads || cache.always) {
            let started = Instant::now();
            match create(&own, cache, lookup) {
                Ok(cloned) => prepared.log.push(format!(
                    "created the thread's {} cache in {} ms ({})",
                    cache.name,
                    started.elapsed().as_millis(),
                    if cloned.is_empty() {
                        "empty".to_owned()
                    } else {
                        format!("cloned {}", cloned.join(", "))
                    }
                )),
                Err(error) => {
                    prepared.log.push(format!(
                        "could not create the thread's {} cache {}: {error}",
                        cache.name,
                        own.display()
                    ));
                    continue;
                }
            }
        } else if !exists {
            continue;
        }
        prepared
            .env
            .push((cache.var.to_owned(), own.to_string_lossy().into_owned()));
        for (var, path) in cache.more {
            prepared.env.push((
                (*var).to_owned(),
                own.join(path).to_string_lossy().into_owned(),
            ));
        }
    }
    if !downloads {
        prepared.env.extend(
            OFFLINE
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );
    }
    prepared.env.extend(
        QUIET
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );
    prepared
}

/// Whether the user made `cache` writable in Settings: the user's cache,
/// or every part of it that is cloned, is in a writable folder.
fn shared(cache: Cache, writable: &[PathBuf], lookup: &Lookup) -> bool {
    let Some(user) = (cache.user)(lookup) else {
        return false;
    };
    let covered = |path: &Path| writable.iter().any(|dir| path.starts_with(dir));
    if cache.parts.is_empty() {
        covered(&user)
    } else {
        cache.parts.iter().all(|part| covered(&user.join(part)))
    }
}

/// Creates `own` for `cache`: the user's cache (or its parts) cloned, its
/// configuration files copied. Returns what was cloned. A part that cannot
/// be cloned is left out (the tool downloads it again).
fn create(own: &Path, cache: Cache, lookup: &Lookup) -> std::io::Result<Vec<String>> {
    let user = (cache.user)(lookup).filter(|user| user.is_dir() && user != own);
    let mut cloned = Vec::new();
    if let (Some(user), true) = (&user, cache.parts.is_empty()) {
        fs::create_dir_all(own.parent().unwrap_or(own))?;
        if clone(user, own) {
            cloned.push(user.display().to_string());
        }
    }
    fs::create_dir_all(own)?;
    let Some(user) = user else {
        return Ok(cloned);
    };
    for part in cache.parts {
        let source = user.join(part);
        let target = own.join(part);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        if source.is_dir() && clone(&source, &target) {
            cloned.push(source.display().to_string());
        }
    }
    for file in cache.files {
        let source = user.join(file);
        if source.is_file() {
            match fs::copy(&source, own.join(file)) {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(cloned)
}

/// Clones `source` to `target`; on failure removes what was left.
fn clone(source: &Path, target: &Path) -> bool {
    if gitez_agent_thread::clone_tree(source, target).is_ok() {
        return true;
    }
    let _ = fs::remove_dir_all(target);
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(home: &Path) -> Lookup {
        Lookup {
            home: Some(home.to_owned()),
            var: |_| None,
        }
    }

    fn value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// A home folder with a Cargo home and an npm cache.
    fn home(dir: &Path) -> PathBuf {
        let home = dir.join("home");
        let crate_src = home.join(".cargo/registry/src/index/serde-1.0.0");
        fs::create_dir_all(&crate_src).expect("create");
        fs::write(crate_src.join("lib.rs"), "// serde\n").expect("write");
        fs::create_dir_all(home.join(".cargo/bin")).expect("create");
        fs::write(home.join(".cargo/bin/cargo"), "#!/bin/sh\n").expect("write");
        fs::write(home.join(".cargo/config.toml"), "[net]\n").expect("write");
        fs::write(home.join(".cargo/credentials.toml"), "token = \"x\"\n").expect("write");
        fs::create_dir_all(home.join(".npm/_cacache/index-v5")).expect("create");
        fs::create_dir_all(home.join(".npm/_logs")).expect("create");
        fs::create_dir_all(home.join("go/pkg/sumdb/sum.golang.org")).expect("create");
        fs::create_dir_all(home.join("go/bin")).expect("create");
        home
    }

    #[test]
    fn without_downloads_the_users_caches_are_read_and_tools_are_offline() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = home(dir.path());
        let caches = dir.path().join("caches/repo-x");
        let prepared = prepare(&caches, false, &[], &lookup(&home));
        assert_eq!(value(&prepared.env, "CARGO_HOME"), None);
        assert_eq!(value(&prepared.env, "npm_config_cache"), None);
        assert_eq!(value(&prepared.env, "CARGO_NET_OFFLINE"), Some("true"));
        assert_eq!(value(&prepared.env, "GOPROXY"), Some("off"));
        assert_eq!(
            value(&prepared.env, "npm_config_update_notifier"),
            Some("false")
        );
        // Go's build cache is written by every build: always the thread's.
        let go_build = caches.join("go-build");
        assert_eq!(
            value(&prepared.env, "GOCACHE"),
            Some(go_build.to_str().expect("utf-8"))
        );
        assert!(go_build.is_dir());
        assert!(!caches.join("cargo").exists());
    }

    #[test]
    fn downloads_get_the_threads_own_clones_without_binaries_or_credentials() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = home(dir.path());
        let caches = dir.path().join("caches/repo-x");
        let prepared = prepare(&caches, true, &[], &lookup(&home));
        let cargo = caches.join("cargo");
        assert_eq!(
            value(&prepared.env, "CARGO_HOME"),
            Some(cargo.to_str().expect("utf-8"))
        );
        assert_eq!(
            fs::read_to_string(cargo.join("registry/src/index/serde-1.0.0/lib.rs"))
                .expect("cloned source"),
            "// serde\n"
        );
        assert!(cargo.join("config.toml").is_file(), "configuration is kept");
        assert!(!cargo.join("bin").exists(), "binaries are not");
        assert!(
            !cargo.join("credentials.toml").exists(),
            "credentials are not"
        );
        assert!(caches.join("npm/_cacache/index-v5").is_dir());
        assert!(!caches.join("npm/_logs").exists());
        assert_eq!(value(&prepared.env, "CARGO_NET_OFFLINE"), None);
        assert_eq!(value(&prepared.env, "GOPROXY"), None);
        // The user's own cache is untouched by what the thread writes.
        fs::write(cargo.join("registry/src/index/serde-1.0.0/lib.rs"), "evil").expect("write");
        assert_eq!(
            fs::read_to_string(home.join(".cargo/registry/src/index/serde-1.0.0/lib.rs"))
                .expect("read"),
            "// serde\n"
        );
        // Go: modules and the checksum database, and both variables.
        assert_eq!(
            value(&prepared.env, "GOMODCACHE"),
            Some(caches.join("go/pkg/mod").to_str().expect("utf-8"))
        );
        assert!(caches.join("go/pkg/sumdb/sum.golang.org").is_dir());
        assert!(!caches.join("go/bin").exists());
        // Without a user cache the thread's starts empty.
        assert!(caches.join("pip").is_dir());
        assert_eq!(
            value(&prepared.env, "PIP_CACHE_DIR"),
            Some(caches.join("pip").to_str().expect("utf-8"))
        );
    }

    #[test]
    fn once_the_thread_has_its_own_cache_it_keeps_using_it_offline() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = home(dir.path());
        let caches = dir.path().join("caches/repo-x");
        prepare(&caches, true, &[], &lookup(&home));
        fs::write(caches.join("cargo/registry/new.crate"), "downloaded").expect("write");
        let offline = prepare(&caches, false, &[], &lookup(&home));
        assert_eq!(
            value(&offline.env, "CARGO_HOME"),
            Some(caches.join("cargo").to_str().expect("utf-8"))
        );
        assert_eq!(value(&offline.env, "CARGO_NET_OFFLINE"), Some("true"));
        assert!(
            caches.join("cargo/registry/new.crate").is_file(),
            "not cloned again"
        );
    }

    #[test]
    fn a_cache_the_user_made_writable_is_shared() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = home(dir.path());
        let caches = dir.path().join("caches/repo-x");
        // Only the registry: Cargo's git checkouts would still be cloned.
        let partly = prepare(
            &caches,
            true,
            &[home.join(".cargo/registry")],
            &lookup(&home),
        );
        assert!(value(&partly.env, "CARGO_HOME").is_some());
        let caches = dir.path().join("caches/repo-y");
        let writable = [
            home.join(".cargo/registry"),
            home.join(".cargo/git"),
            home.join(".npm"),
        ];
        let shared = prepare(&caches, true, &writable, &lookup(&home));
        assert_eq!(value(&shared.env, "CARGO_HOME"), None);
        assert_eq!(value(&shared.env, "npm_config_cache"), None);
        assert!(!caches.join("cargo").exists());
        assert!(value(&shared.env, "PIP_CACHE_DIR").is_some());
    }

    #[test]
    fn user_cache_locations_follow_the_environment() {
        let lookup = Lookup {
            home: Some("/Users/me".into()),
            var: |name| match name {
                "CARGO_HOME" => Some("/opt/cargo".to_owned()),
                "GOPATH" => Some("/work/go".to_owned()),
                "XDG_CACHE_HOME" => Some("relative/is/ignored".to_owned()),
                _ => None,
            },
        };
        let user = |name: &str| {
            CACHES
                .iter()
                .find(|cache| cache.name == name)
                .and_then(|cache| (cache.user)(&lookup))
        };
        assert_eq!(user("cargo"), Some(PathBuf::from("/opt/cargo")));
        assert_eq!(user("npm"), Some(PathBuf::from("/Users/me/.npm")));
        assert_eq!(user("go"), Some(PathBuf::from("/work/go")));
        assert_eq!(
            user("corepack"),
            Some(PathBuf::from("/Users/me/.cache/node/corepack"))
        );
    }
}
