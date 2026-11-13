//! Helpers shared by the integration tests: temporary repositories driven
//! through the git CLI with no user or system configuration, and a data
//! folder whose path has a space, as GitEZ's has on macOS.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use gitez_agent_thread::{NewAgentThread, Start, Store, Timestamp};

/// `git <args>` in `dir` with a fixed identity and no user or system
/// config; returns its trimmed output.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf-8 output")
        .trim()
        .to_owned()
}

/// A repository with one commit on `main`, and a GitEZ data folder, both
/// in one temporary directory and both with a space in their path.
pub struct Fixture {
    _dir: tempfile::TempDir,
    /// The main worktree.
    pub root: PathBuf,
    /// `…/Application Support/GitEZ`.
    pub data_dir: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("temporary directory");
        // Resolve `/var` to `/private/var` on macOS, so paths compare
        // equal to the ones git reports.
        let base = dir.path().canonicalize().expect("canonical temp dir");
        let root = base.join("my repo");
        let data_dir = base.join("Application Support").join("GitEZ");
        std::fs::create_dir_all(&root).expect("create repository folder");
        std::fs::create_dir_all(&data_dir).expect("create data folder");
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        // The library's own git commands read the user's configuration
        // too; these keep it from running hooks or signing.
        let hooks = base.join("no hooks");
        std::fs::create_dir_all(&hooks).expect("create hooks folder");
        for (key, value) in [
            ("user.name", "Test"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", hooks.to_str().expect("utf-8 path")),
        ] {
            git(&root, &["config", key, value]);
        }
        std::fs::write(root.join("README.md"), "hello\n").expect("write");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "--quiet", "-m", "First"]);
        Self {
            _dir: dir,
            root,
            data_dir,
        }
    }

    pub fn common_dir(&self) -> PathBuf {
        self.root.join(".git")
    }

    pub fn store(&self) -> Store {
        Store::new(&self.common_dir())
    }

    pub fn head(&self) -> String {
        git(&self.root, &["rev-parse", "HEAD"])
    }

    pub fn tip(&self, branch: &str) -> String {
        git(&self.root, &["rev-parse", &format!("refs/heads/{branch}")])
    }

    pub fn new_thread(&self, prompt: &str) -> NewAgentThread {
        NewAgentThread {
            prompt: prompt.to_owned(),
            agent: "claude-code".to_owned(),
            model: Some("haiku".to_owned()),
            start: Start {
                branch: "main".to_owned(),
                commit: self.head(),
            },
            network: Default::default(),
            secrets: vec!["GITHUB_TOKEN".to_owned()],
        }
    }

    /// Creates an agent thread for `prompt` in the store.
    pub fn create(&self, prompt: &str) -> gitez_agent_thread::AgentThread {
        self.store()
            .create(
                self.new_thread(prompt),
                &self.data_dir,
                "my repo",
                |_| false,
                at("2026-11-13T09:41:07Z"),
            )
            .expect("create thread")
    }
}

pub fn at(time: &str) -> Timestamp {
    time.parse().expect("a valid time")
}

/// The running thread of DESIGN.md C.2, verbatim.
pub const EXAMPLE: &str = r#"{
  "version": 1,
  "id": "add-a-blame-view-to-the-commit-detail",
  "title": "Add a blame view to the commit detail",
  "agent": "claude-code",
  "model": "sonnet",
  "state": "running",
  "ending": null,
  "created": "2026-11-13T09:41:07Z",
  "updated": "2026-11-13T09:55:12Z",
  "start": { "branch": "main", "commit": "8f4a2b6c0d9e1f2a3b4c5d6e7f8091a2b3c4d5e6" },
  "branch": "agent/add-a-blame-view-to-the-commit-detail",
  "worktree": "/Users/me/Library/Application Support/GitEZ/worktrees/gitez-1a2b3c4d/add-a-blame-view-to-the-commit-detail",
  "prepared": true,
  "network": "packages",
  "secrets": ["GITHUB_TOKEN"],
  "session_id": "6f1c2b8e-3a1d-4c55-9b0e-2d7f1e4a9c10",
  "turns": [
    {
      "kind": "start",
      "prompt": "Add a blame view to the commit detail: clicking a file …",
      "requested": "2026-11-13T09:41:07Z",
      "resume": false,
      "started": "2026-11-13T09:41:31Z",
      "ended": null,
      "transcript": "2026-11-13T09-41-31Z.jsonl",
      "result": null
    }
  ],
  "activity": { "tool": "Bash", "detail": "cargo test -p gitez-git blame", "steps": 31, "updated": "2026-11-13T09:55:12Z" },
  "warnings": [],
  "supervisor": { "pid": 48211, "child_group": 48230, "started": "2026-11-13T09:41:08Z" }
}
"#;
