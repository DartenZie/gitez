# GitEZ

GitEZ is a fast, native, visual git client for macOS and Linux with one
addition: **local pull requests**.

When a coding agent produces a stream of small branches, opening a real pull
request for each one buries your team's PRs in noise. GitEZ lets you review that
work the way you would review a PR, but locally and privately:

- Open any local branch as a *local PR* against its base branch.
- Read the diff, mark files as viewed, and comment directly on lines of code.
- Every comment is collected into an **agent prompt** that you can copy or send
  straight to the agent. Claude Code is the supported agent in v1.
- Comment threads resolve themselves when a new commit on the branch changes
  the lines they point at.

Review data lives inside the repository's `.git` directory, so it never shows
up as untracked files and is never committed by accident.

Everything else is what you expect from a git client: a commit graph with
drag-and-drop merge, staging and committing, cherry-pick, rebase and reword,
a merge conflict editor, stashes, tags, remotes, undo for git actions, and a
command palette (`⌘K`) that runs any git command.

The design is at <https://claude.ai/artifact/ELtu5gwJExRfcVQHVNCGs8>.

> **Status:** early development. The current build opens a repository and
> shows its history as a commit graph, with branches, remotes, tags and
> stashes in the sidebar and the selected commit's details beside it. Press
> Enter or double-click a commit to review its full diff, file by file; `[`
> and `]` step to the newer and older commit, Esc goes back. Uncommitted
> changes appear as a row above the current commit; open it the same way to
> review them, with each file marked staged, partly staged or untracked.
> Stage or unstage files there and commit them with a message (⌘↵), or
> amend the last commit, with a warning when it is already pushed. Commits
> run your own `git`, so hooks and signing work as in the terminal, and a
> hook that rejects the commit shows its output. Changes made in an
> editor, a terminal or another git client show up as they happen, in the
> open review too. Nothing else can be changed from GitEZ yet.

## Technology

- [Rust](https://www.rust-lang.org/) (stable, edition 2024)
- [GPUI](https://gpui.rs), the GPU-accelerated UI framework behind the Zed editor,
  through [gpui-kit](https://gpui-kit.com) (GPUI Component), which provides the
  widget set and pins the matching GPUI release.
- [gitoxide](https://github.com/GitoxideLabs/gitoxide) for reading repositories;
  the user's own `git` binary for every operation that changes them, so hooks,
  signing and credential helpers behave exactly as in the terminal.

Performance is a primary goal: large repositories must stay smooth.

## Requirements

### macOS

1. Install Rust with [rustup](https://rustup.rs). The `rust-toolchain.toml` in
   this repository selects the exact toolchain automatically.
2. Install Xcode from the App Store, launch it once and let it install the
   macOS components. GPUI renders with Metal and needs the Metal shader
   compiler. Since Xcode 26 that compiler is a separate download:

   ```sh
   xcodebuild -downloadComponent MetalToolchain
   ```

   If a build fails with `cannot execute tool 'metal'`, this step is missing.

### Linux

1. Install Rust with [rustup](https://rustup.rs).
2. Install the system libraries GPUI needs. On Debian or Ubuntu:

   ```sh
   sudo apt install gcc g++ clang lld cmake make pkg-config \
     libfontconfig-dev libglib2.0-dev libssl-dev libvulkan1 \
     libwayland-dev libx11-xcb-dev libxkbcommon-x11-dev libzstd-dev \
     libasound2-dev libsqlite3-dev
   ```

   On Fedora:

   ```sh
   sudo dnf install gcc clang lld cmake fontconfig-devel glib2-devel \
     openssl-devel vulkan-loader wayland-devel libxcb-devel \
     libxkbcommon-x11-devel libzstd-devel alsa-lib-devel sqlite-devel
   ```

   A Vulkan-capable GPU driver is required at runtime. Both Wayland and X11 are
   supported.

   GitEZ watches every directory of an open repository with inotify. In very
   large working trees this can exceed the default watch limit; raise it with
   `sudo sysctl fs.inotify.max_user_watches=524288` (and in
   `/etc/sysctl.conf` to keep it after a restart).

Windows support is planned but not yet available.

### Fonts (optional)

GitEZ is designed for [Geist](https://vercel.com/font) and
[JetBrains Mono](https://www.jetbrains.com/lp/mono/). If they are installed,
GitEZ uses them; otherwise it falls back to the system UI and monospace fonts.

## Build and run

```sh
cargo run                         # then open a repository with ⌘O (Ctrl+O on Linux)
cargo run -- <path-to-repository> # open a repository straight away
```

The first build compiles GPUI and gpui-kit from source and takes several
minutes. Later builds are incremental.

Other useful commands:

```sh
cargo build --release    # optimised build in target/release/gitez
cargo fmt                # format
cargo clippy --all-targets   # lint
cargo test               # run tests
```

## Project layout

```
Cargo.toml          workspace: dependency versions, lints, build profiles
crates/gitez/       the application binary
crates/gitez-git/   read-only repository access on gitoxide: refs, history, graph layout,
                    commit messages, changed files and diffs, working-tree status,
                    watching for changes
crates/gitez-command/ changes made by running the user's git binary: staging,
                    unstaging, committing, amending
rust-toolchain.toml pinned Rust toolchain
CLAUDE.md           instructions for Claude Code when working in this repository
```

More crates (review storage, agent integration, UI components) will be added
under `crates/` as the application grows.

To time reading refs, history, commit details and the working-tree status on a
repository:

```sh
cargo run --release -p gitez-git --example history -- <path-to-repository>
```

## License

GitEZ is released under the [MIT License](LICENSE).
