//! `Repository::worktree_include_files` on repositories built with the git
//! CLI in temporary directories.

use std::path::{Path, PathBuf};
use std::process::Command;

use gitez_git::Repository;

/// Runs git in `dir` with a fixed identity and no user or system config.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn write(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create folders");
    std::fs::write(path, contents).expect("write file");
}

/// A repository with tracked, untracked and ignored files, in a folder
/// whose name has a space.
fn repository() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temporary directory");
    let root = dir.path().join("my repo");
    std::fs::create_dir(&root).expect("create repository folder");
    git(&root, &["init", "--quiet", "--initial-branch=main"]);
    write(
        &root,
        ".gitignore",
        ".env\n.env.*\ntarget/\nnode_modules/\nsecrets/\nconfig/\n*.log\n",
    );
    write(&root, "src/main.rs", "fn main() {}\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "--quiet", "-m", "init"]);

    write(&root, ".env", "KEY=1\n");
    write(&root, ".env.local", "KEY=2\n");
    write(&root, "src/.env", "KEY=3\n");
    write(&root, "untracked.txt", "not ignored\n");
    write(&root, "target/debug/app", "binary\n");
    write(&root, "target/debug/.env", "built\n");
    write(&root, "node_modules/x/index.js", "x\n");
    write(&root, "secrets/one.key", "1\n");
    write(&root, "secrets/two.key", "2\n");
    write(&root, "config/local.toml", "local\n");
    write(&root, "config/other.toml", "other\n");
    write(&root, "debug.log", "log\n");
    (dir, root)
}

fn paths(paths: &[&str]) -> Vec<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
}

#[test]
fn no_include_file_lists_nothing() {
    let (_dir, root) = repository();
    let repo = Repository::open(&root).expect("open");
    assert!(repo.worktree_include_files().expect("list").is_empty());
}

#[test]
fn lists_ignored_files_and_folders_that_match() {
    let (_dir, root) = repository();
    write(
        &root,
        ".worktreeinclude",
        "# Local settings\n.env\n.env.*\n!.env.example\nsecrets/\nconfig/local.toml\nuntracked.txt\nsrc/main.rs\n",
    );
    write(&root, ".env.example", "KEY=\n");
    let repo = Repository::open(&root).expect("open");
    assert_eq!(
        repo.worktree_include_files().expect("list"),
        // Not `untracked.txt` (not ignored), not `src/main.rs` (tracked),
        // not `.env.example` (negated), not `target/debug/.env` (inside an
        // ignored folder no pattern leads into); `secrets/` once.
        paths(&[
            ".env",
            ".env.local",
            "config/local.toml",
            "secrets",
            "src/.env",
        ])
    );
}

#[test]
fn a_whole_ignored_folder_can_be_included() {
    let (_dir, root) = repository();
    write(&root, ".worktreeinclude", "node_modules\n/target/\n*.log\n");
    let repo = Repository::open(&root).expect("open");
    assert_eq!(
        repo.worktree_include_files().expect("list"),
        paths(&["debug.log", "node_modules", "target"])
    );
}
