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
> Enter or double-click a commit to see its full diff, file by file, or
> click a file in its details to jump straight to that file; `[` and `]`
> step to the newer and older commit, Esc goes back. Diffs show
> unified or split side by side, optionally hiding whitespace changes
> (single hunks can't be staged then), and show the unchanged lines
> between hunks one gap at a time or all at once. Mark a file viewed to
> collapse it and track progress; it opens again if it changes.
> Uncommitted changes appear as a row above the current commit; open it the same way to
> review them, with each file marked staged, partly staged or untracked.
> Stage or unstage whole files or single hunks there (staged hunks are
> marked and listed first) and commit them with a message (⌘↵), or
> amend the last commit, with a warning when it is already pushed. Commits
> run your own `git`, so hooks and signing work as in the terminal, and a
> hook that rejects the commit shows its output. Changes made in an
> editor, a terminal or another git client show up as they happen, in the
> open review too. **New local PR** creates a local pull request for a
> branch against its base (`main` by default); the sidebar lists them,
> and opening one shows the branch's changes since it left the base,
> with the files marked viewed kept between sessions. Hover a line and
> click **+** (or drag from it across several lines) to comment; threads
> show under their lines, with replies, Resolve and whether they go into
> the agent prompt. A thread whose lines change in a new commit on the
> branch resolves itself. The agent prompt panel beside the diff shows
> the prompt built from the included threads, with the agent, a template
> (fix the comments, or explain without changing anything) and whether to
> quote code and run tests. The test command is guessed from the project
> (`cargo test`, `pnpm test`, `go test ./...` and so on) and can be set
> per repository; it is kept as `gitez.testCommand` in the repository's
> git config. **Copy** puts it on the clipboard; **Send to
> Claude Code** runs `claude` headless in the working tree, with all
> permissions, and streams what it does into the panel (it needs the
> branch checked out, which **Checkout branch** does). Each run's
> transcript is kept, and the last one shows when you come back.
> **Merge into …** merges the branch into its base with a merge commit,
> a squash, a rebase or a fast-forward, and warns beforehand about files
> that would conflict. A merge commit that conflicts opens the **conflict
> editor**: for each conflict, our and their lines side by side (and the
> base, if you want it), lines ticked into the result or the result typed
> by hand, then **Commit merge** or **Abort merge**. A merge that stopped
> in a terminal shows a banner that opens the same editor. The other ways
> of merging undo themselves if git stops. The pull request
> is then marked merged and keeps showing the changes it had. **Edit**
> changes a pull request's title and description; **Close** sets it aside
> without merging, keeping its changes and comments, and **Reopen** brings
> it back on its branches as they are now. In the history, drag a local
> branch's label onto another branch to merge it there: the commits it
> would bring in are highlighted, and dropping it offers a merge commit,
> a squash, a rebase of the dragged branch onto the other, a fast-forward,
> or opening the two as a local pull request (Esc cancels the drag). A
> merge of branches that have an open pull request marks it merged.
> ⌘-click (Ctrl-click on Linux) commits in the history to select several,
> ⇧-click for a range; **Cherry-pick onto…** lists every branch with a
> trial run of the pick against it (applies cleanly, already there, or
> conflicts), and applies the commits oldest first to the branch you
> choose, optionally noting the source commit (`-x`), only staging the
> changes (`-n`), or stashing your uncommitted changes around it. A
> cherry-pick that conflicts is given up, leaving everything as it was.
> The commit details also offer **Cherry-pick…** for one commit and
> **Reword**, which rewrites the history of the branch checked out from
> that commit up: each commit, newest first, can be picked, reworded
> (subject and body), stopped at to edit, squashed or folded as a fixup
> into the commit below, or dropped, and dragged into a new order.
> **Autosquash fixups** folds `fixup!` and `squash!` commits into the
> commits they name. A summary says what will happen and warns when
> commits already pushed would change. The rewrite runs `git rebase -i`
> with your uncommitted changes stashed around it; the old tip is kept as
> `refs/gitez/backup/<branch>`. A
> rewrite that conflicts is given up; one stopped at an edit, or any
> rebase stopped in a terminal, shows a banner to **Continue** or
> **Abort** it. **⌘K** (Ctrl+K on Linux), or the search field in the
> toolbar, opens the command palette: type to find an action, a commit (by
> message, id, tag or branch), a branch to check out or a changed file,
> with tabs to show only one kind. A query starting with `git` is a git
> command. GitEZ offers its own screen for the ones it knows
> (`git rebase -i <commit>` opens the history editor, `--autosquash`
> folding the fixups in; `git switch <branch>` checks it out), and **⌘↵**
> runs any of them exactly as typed with your own `git`, the output
> streaming into the palette. Tab fills in the selected branch, commit or
> file as the next argument. Nothing can prompt or open an editor there.
> **Fetch** fetches every remote, **Pull** brings the upstream's commits
> into the branch checked out as your git configuration says, and
> **Push** sends it to its upstream, publishing a new branch to `origin`
> first. They run your own `git`, so credential helpers and hooks apply;
> the buttons count the commits to pull and to push, and the status bar
> says when the repository last fetched. A push the remote rejects offers
> to pull first or to force push with `--force-with-lease`; a pull of
> diverged branches with no `pull.rebase` set offers to merge them.
> **Branch** in the toolbar, **Branch…** on a commit, or the palette
> creates a branch at the selected commit (or `HEAD`) and checks it out
> unless you untick that; the name is checked as you type. Double-click a
> branch in the sidebar to check it out, or right-click it to start a
> branch from it, rename it or delete it. A branch whose commits are on
> no other branch is deleted only after you confirm. Branches of an open
> local pull request cannot be renamed or deleted. Click a remote in the
> sidebar to list its branches; double-click one, or find it in the
> palette, to check it out as a local branch that tracks it, or
> right-click it to start a branch from it or delete it from the remote
> (after you confirm; that one cannot be undone). **+** beside Remotes,
> or **Add remote…** in the palette, adds a remote and fetches it;
> right-click a remote to fetch it alone, rename it or change its URL,
> or remove it (after you confirm). **Set upstream…** on a branch's
> right-click menu, or in the palette for the branch checked out, picks
> the remote branch it pulls from and pushes to, or none. Remote and
> upstream changes are not undone. **Tag…** on a commit,
> or the palette, tags it: with a message the tag is annotated, without
> one it is lightweight. Click a tag in the sidebar to show its commit, or
> right-click it to push it to the remote, delete it, or delete it from
> the remote (after you confirm). **Revert** on a commit adds a commit to
> the branch checked out that takes it back (a merge against its first
> parent); one that conflicts is given up. **Reset…** moves the branch
> checked out to the commit, keeping what the later commits changed
> staged or unstaged, or discarding it along with your uncommitted
> changes to tracked files. **Stash** puts your uncommitted changes aside, new
> files too unless you untick that, with an optional message; right-click
> a stash in the sidebar to pop it, apply it and keep it, or drop it. A
> stash that conflicts with your files is kept, with the conflicts marked
> in them. In the working tree, **Discard** on a file throws away its
> changes, staged or not, and deletes it if git does not track it yet;
> **Discard hunk** throws away one unstaged hunk, and **Discard all**
> every uncommitted change. **Undo** (`⌘Z`, the arrows in the toolbar, the palette, or the
> button on the notification) takes back what GitEZ last did: a commit or
> amend (its changes come back staged), a checkout, a branch created,
> renamed or deleted, a tag created or deleted, a pull, merge, rebase,
> cherry-pick, revert, reset or rewrite, a stash made or dropped, changes discarded, and a local pull request
> marked merged. **Redo**
> (`⇧⌘Z`) does it again. GitEZ keeps its own list of these steps while it
> runs, and refuses one when a branch it would move has moved since, so
> nothing done elsewhere is overwritten. Fetch, push, remote and upstream
> changes, and commands run as typed in the palette are not undone, and a branch brought back loses its
> upstream setting.

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

To send prompts to an agent, install
[Claude Code](https://docs.claude.com/en/docs/claude-code). GitEZ looks for
`claude` on the `PATH` and in the places its installers use, such as
`~/.local/bin`.

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
cargo clippy --all-targets --workspace   # lint every crate
cargo test               # run tests
```

## Project layout

```
Cargo.toml          workspace: dependency versions, lints, build profiles
crates/gitez/       the application binary
crates/gitez-git/   read-only repository access on gitoxide: refs, history, graph layout,
                    commit messages, changed files and diffs, working-tree status,
                    merge previews, merges in progress and their conflict markers,
                    cherry-pick trial runs, commits to rewrite and rebases in progress,
                    watching for changes
crates/gitez-command/ changes made by running the user's git binary: staging,
                    unstaging files or hunks, committing, amending, switching branches,
                    merging and resolving conflicts, cherry-picking, rewriting history,
                    adding and changing remotes, setting config values
crates/gitez-review/ local pull requests, their comment threads and agent run
                    transcripts, stored in .git/gitez/reviews
crates/gitez-agent/ coding agents a review prompt is sent to: Claude Code run as
                    `claude -p` with its streamed events parsed; guessing a project's
                    test command
rust-toolchain.toml pinned Rust toolchain
CLAUDE.md           instructions for Claude Code when working in this repository
```

More crates (UI components) will be added
under `crates/` as the application grows.

To time reading refs, history, commit details and the working-tree status on a
repository:

```sh
cargo run --release -p gitez-git --example history -- <path-to-repository>
```

## License

GitEZ is released under the [MIT License](LICENSE).
