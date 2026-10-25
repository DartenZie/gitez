<div align="center">

# GitEZ

**A fast, native, visual git client with local pull requests.**

[![Rust](https://img.shields.io/badge/Rust-1.99-orange?logo=rust)](rust-toolchain.toml)
[![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Linux-blue)](#requirements)
[![License](https://img.shields.io/badge/license-MIT-green)](LICENSE)

[Features](#features) • [Requirements](#requirements) • [Getting started](#getting-started) • [Usage](#usage) • [Development](#development)

</div>

GitEZ is a desktop git client for macOS and Linux, written in Rust on
[GPUI](https://gpui.rs). It has one feature that other clients do not:
**local pull requests**.

When a coding agent produces many small branches, a real pull request for each
one buries your team's PRs in noise. With GitEZ, you review that work as you
would review a PR, but on your own machine. You comment on lines of the diff,
and GitEZ turns the comments into a prompt for the agent. You can copy the
prompt, or send it straight to [Claude Code](https://docs.claude.com/en/docs/claude-code).

> [!WARNING]
> GitEZ is in early development. Expect missing features and breaking changes.

## Features

### Local pull requests

- Open any local branch as a pull request against its base branch (`main` by default).
- Read the diff in unified or split view, and mark files as viewed. A viewed
  file opens again when it changes.
- Comment on one line or a range of lines. Threads support replies and
  **Resolve**, and a thread resolves itself when a new commit changes its lines.
- Build an **agent prompt** from the threads you include. Choose a template
  (fix the comments, or explain without changing anything) and whether to
  quote code and run tests. GitEZ guesses the test command (`cargo test`,
  `pnpm test`, `go test ./...` and others), and you can set it per repository.
- **Copy** the prompt, or **Send to Claude Code** to run `claude` headless in
  the working tree and stream its output into the panel. GitEZ keeps the
  transcript of each run.
- Merge into the base with a merge commit, a squash, a rebase or a
  fast-forward. GitEZ warns you about conflicting files before the merge starts.

Review data is stored in `.git/gitez/reviews`. It is never in the working
tree, so it cannot be committed or pushed by accident. Linked worktrees share it.

### Everything else you expect from a git client

- **History**: a virtualised commit graph, with branches, remotes, tags,
  stashes and local PRs in the sidebar, and commit details and full diffs
  beside it.
- **Working tree**: stage, unstage or discard files and single hunks. Commit,
  amend, sign off or skip hooks. Changes made in other tools appear as they happen.
- **Drag-and-drop merge**: drag a branch label onto another branch to merge,
  squash, rebase, fast-forward, or open the two branches as a local PR.
- **Conflict editor**: see our and their lines side by side (with the base if
  you want it), and tick lines into the result or type it by hand.
- **Cherry-pick**: select commits and pick them onto any branch. GitEZ does a
  trial run against each branch first.
- **Rewrite history**: reword, edit, squash, fixup, drop and reorder commits,
  and autosquash `fixup!` commits. The old tip is kept as `refs/gitez/backup/<branch>`.
- **Branches, remotes, tags and stashes**: create, rename, delete, check out,
  pull, push, fetch, set upstreams, revert and reset.
- **Undo and redo** for most actions: commits, checkouts, merges, rebases,
  resets, stashes, discarded changes and more.
- **Command palette** (`⌘K`): find actions, commits, branches and files, or
  run any `git` command and see its output stream in.
- **Clone or create** a repository from the start screen, the File menu or the palette.

> [!NOTE]
> GitEZ reads the repository with [gitoxide](https://github.com/GitoxideLabs/gitoxide),
> in-process. Every change runs your own `git` binary, so hooks, commit signing
> and credential helpers work exactly as they do in the terminal.

## Requirements

- [Rust](https://rustup.rs), installed with rustup. The `rust-toolchain.toml`
  file selects the correct toolchain automatically.
- `git` on the `PATH`.
- Optional: [Claude Code](https://docs.claude.com/en/docs/claude-code), to send
  prompts to an agent. GitEZ looks for `claude` on the `PATH` and in the
  installers' default locations, such as `~/.local/bin`.
- Optional: the [Geist](https://vercel.com/font) and
  [JetBrains Mono](https://www.jetbrains.com/lp/mono/) fonts. Without them,
  GitEZ uses the system UI and monospace fonts.

Windows is not supported yet.

### macOS

Install Xcode from the App Store, open it once, and let it install the macOS
components. GPUI renders with Metal and needs the Metal shader compiler, which
is a separate download since Xcode 26:

```sh
xcodebuild -downloadComponent MetalToolchain
```

> [!TIP]
> If the build fails with `cannot execute tool 'metal'`, run the command above.

### Linux

Install the system libraries that GPUI needs.

Debian or Ubuntu:

```sh
sudo apt install gcc g++ clang lld cmake make pkg-config \
  libfontconfig-dev libglib2.0-dev libssl-dev libvulkan1 \
  libwayland-dev libx11-xcb-dev libxkbcommon-x11-dev libzstd-dev \
  libasound2-dev libsqlite3-dev
```

Fedora:

```sh
sudo dnf install gcc clang lld cmake fontconfig-devel glib2-devel \
  openssl-devel vulkan-loader wayland-devel libxcb-devel \
  libxkbcommon-x11-devel libzstd-devel alsa-lib-devel sqlite-devel
```

You need a Vulkan-capable GPU driver. Wayland and X11 are both supported.

> [!TIP]
> GitEZ watches every directory of an open repository with inotify. In a very
> large working tree, raise the watch limit with
> `sudo sysctl fs.inotify.max_user_watches=524288`. To keep the setting after a
> restart, add it to `/etc/sysctl.conf`.

## Getting started

```sh
git clone https://github.com/miroslavpasek/gitez.git
cd gitez
cargo run                          # open, clone or create a repository from the start screen
cargo run -- <path-to-repository>  # open a repository directly
```

> [!NOTE]
> The first build compiles GPUI from source and takes several minutes. Later
> builds are incremental.

For an optimised build, run `cargo build --release`. The binary is
`target/release/gitez`.

## Usage

### Review a branch as a local PR

1. Click **New local PR**, and select a branch and its base.
2. Open the pull request from the sidebar.
3. Hover a line and click **+**, or drag across several lines, to comment.
4. In the agent prompt panel, select the threads, template and options.
5. Click **Copy**, or click **Send to Claude Code**.

> [!IMPORTANT]
> **Send to Claude Code** runs `claude -p` with `--permission-mode bypassPermissions`,
> because a headless run cannot ask for permission. It starts only when the
> pull request's head branch is checked out. Click **Checkout branch** first if
> necessary.

### Keyboard shortcuts

On Linux, use `Ctrl` instead of `⌘`.

| Shortcut | Action |
| --- | --- |
| `⌘O` | Open a repository |
| `⌘K` | Open the command palette |
| `⌘Z` / `⇧⌘Z` | Undo / redo |
| `↑` `↓` or `k` `j` | Move through the history |
| `Enter` | Show the selected commit's full diff |
| `[` / `]` | Step to the newer / older commit in a diff |
| `Esc` | Go back, or cancel a drag |
| `⌘↵` | Commit, or run the typed `git` command in the palette |
| `Tab` | In the palette, fill in the selected item as the next argument |
| `⌘`-click / `⇧`-click | Select several commits / a range of commits |

## Development

```sh
cargo fmt --all
cargo clippy --all-targets --workspace
cargo test --workspace
```

To measure how long it takes to read refs, history, commit details and the
working-tree status of a repository:

```sh
cargo run --release -p gitez-git --example history -- <path-to-repository>
```

### Project layout

| Path | Contents |
| --- | --- |
| `crates/gitez` | The application binary and its views |
| `crates/gitez-git` | Read-only repository access on gitoxide: refs, history, graph layout, diffs, status, merge previews, file watching |
| `crates/gitez-command` | Changes made with the user's `git` binary: staging, committing, merging, cherry-picking, rebasing, remotes, cloning |
| `crates/gitez-review` | Local pull requests, comment threads and agent transcripts in `.git/gitez/reviews` |
| `crates/gitez-agent` | Coding agents: Claude Code run as `claude -p` with its streamed events parsed, and test command detection |

The UI uses [gpui-kit](https://gpui-kit.com), which pins the matching GPUI
release. [CLAUDE.md](CLAUDE.md) describes the architecture decisions, the
design tokens and the process to update GPUI.

The reference design is available as a
[design canvas](https://claude.ai/artifact/ELtu5gwJExRfcVQHVNCGs8).
