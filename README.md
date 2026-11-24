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

GitEZ can also start that work: **agent threads** run Claude Code in the
background, each on its own branch and worktree and inside a sandbox, and
bring the result back as a local pull request. A thread can also only
**discuss**: the agent reads your working tree, answers and plans, and
changes nothing until you tell it to implement.

> [!WARNING]
> GitEZ is in early development. Expect missing features and breaking changes.

## Features

### Local pull requests

- Open any local branch as a pull request against its base branch (`main` by default).
  The description sits above the diff and scrolls away with it, while the
  title and buttons stay in place. A long description shows its first six
  lines until you select **Show more**.
- Read the diff in unified or split view, and mark files as viewed. A viewed
  file opens again when it changes. Swipe sideways, or hold Shift and scroll,
  to read long lines.
- The diff shows only what the branch itself changes. Changes that are also on
  the base branch do not show, whether the base was merged into the branch,
  squashed into it or cherry-picked.
- Comment on one line or a range of lines. Threads support replies and
  **Resolve**, and a thread resolves itself when a new commit changes its lines.
  Hover a comment to edit or delete it.
- Fix small things yourself: **Edit** on a file opens it in an editor. **Save**
  writes it to the working tree, and **Save and commit** commits that file
  alone on the branch. The pull request's head branch must be checked out.
- Build an **agent prompt** from the threads you include. Choose a template
  (fix the comments, or explain without changing anything) and whether to
  quote code and run tests. GitEZ guesses the test command (`cargo test`,
  `pnpm test`, `go test ./...` and others), and you can set it per repository.
- **Copy** the prompt, or **Send to Claude Code** to run `claude` headless in
  the working tree and stream its output into the panel. GitEZ keeps the
  transcript of each run.
- Merge into the base with a merge commit, a squash, a rebase or a
  fast-forward. GitEZ warns you about conflicting files before the merge starts.
  A squash commit credits the other authors of the branch's commits, and the
  people in their `Co-authored-by` trailers, with `Co-authored-by` trailers of
  its own.
- The sidebar lists and counts only the pull requests in review. Merged and
  closed ones are finished and are not shown. A closed pull request that you
  reopen, or a merge that you undo, puts it back in the list.

Review data is stored in `.git/gitez/reviews`. It is never in the working
tree, so it cannot be committed or pushed by accident. Linked worktrees share it.

### Agent threads

A thread is a prompt you give a coding agent (Claude Code, for now) and the
conversation that follows. Every thread runs in a
[sandbox](#the-agent-sandbox), in one of two modes:

- **Implement**: the agent works on its own branch, `agent/<name>`, in its
  own linked worktree in GitEZ's data folder, never inside your repository.
  What it commits becomes a local pull request.
- **Discuss**: the agent reads your checked-out working tree, with your
  uncommitted changes, and answers. It cannot change anything: the sandbox
  refuses every write in the repository. There is no branch, no worktree
  and no pull request. Use it to ask a question, explore the code or agree
  on a plan.

A third choice, **Auto**, lets the agent decide: the thread starts as a
Discuss thread, and the agent reads the code before it answers. When your
message is a clear request to change code and it is clear what to do, the
agent writes a brief instead of an answer, and GitEZ starts an Implement
thread from it at once. Otherwise it answers as a discussion does.

What follows describes an Implement thread, unless it says otherwise.

- **Start a thread** from the "Ask an agent…" row under the uncommitted
  changes in the history (`⌘⇧A`, or File > Ask an Agent…), or from Home.
  Write the prompt, choose **Discuss**, **Auto** or **Implement**, and pick the agent,
  the model, the effort level (Claude Code's `--effort`; **Default** leaves
  it to the model) and the branch to start from. **Local ports** says whether
  the agent may run servers on localhost (see Network below); it starts as set
  in Settings. The thread gets the network
  profile and the default secrets set in Settings. An Implement thread starts from the branch's
  last commit; your uncommitted changes are not included. A Discuss thread
  reads the branch you have checked out, so it has no branch to pick and
  needs one checked out; so does an Auto thread. GitEZ remembers the mode
  you used last. GitEZ
  returns at once and the agent works in the background.
- **From a discussion to an implementation**: **Implement…** in a Discuss
  thread asks the agent to write a brief of what you agreed, and opens the
  composer with it. Read it, change it if you want, and send it: a new
  Implement thread starts from the brief alone, without the discussion's
  context, and each thread links to the other. The discussion stays open
  for more questions.
- **Auto handoff**: in an Auto thread, any answer can be a brief, also one
  to a reply such as "ok, do it". GitEZ starts one Implement thread from
  each such brief, from the last commit of the branch the discussion read,
  with the same agent, model, effort, network profile, local ports and secrets, and the heading
  of the brief as its title. The agent says in the brief when the change
  depends on your uncommitted changes, which the new thread does not see.
  If GitEZ was closed when the brief arrived, it starts the thread at its
  next launch. If the branch is gone, it starts nothing and says so;
  **Implement…** still opens the brief in the composer.
- **Images**: paste images (`⌘V`) or drop image files into the prompt or a
  reply to show them to the agent, for example a screenshot of a bug. A
  prompt takes up to five images, as PNG, JPEG, GIF or WebP; GitEZ converts
  BMP and TIFF to PNG. It scales down an image whose long side is over
  2,576 pixels or whose data is too large for the agent (about 3.9 MB) and
  says so in the thumbnail's tooltip. An image can be the whole prompt. The
  thread keeps the images it sent beside its record, shows them under the
  prompt, and opens one in your image viewer when you click it. GitEZ
  attaches an image only if the agent and model take images; Claude Code
  takes them with every model.
- Before the agent starts, GitEZ prepares the worktree: it copies the ignored
  files that `.worktreeinclude` lists from your working tree, clones
  `target/` and `node_modules/` copy-on-write where the file system can
  (APFS on macOS, reflinks on Linux), and runs the repository's setup command
  if you set one in Settings.
- **Live lanes**: a thread that is preparing or running is a dashed lane at
  the top of the history, with what its agent does now, joined to the commit
  its branch is at. A running discussion has a grey lane, joined to the
  commit it started from. In the sidebar, the `agent/` branches are one
  folder under Branches.
- **Thread view**: click a thread to open it in the detail panel: the
  prompt, the agent's steps (collapsed to one row), its final message, and
  its branch and pull request. **Reply** continues the same conversation in
  the same worktree. **Stop** ends the agent's turn. **Retry** runs a failed
  or stopped thread again, with another network profile if you choose one.
  What it does depends on where the thread stopped; its tooltip says which:
  - If the agent never started (for example, the setup command failed),
    the first prompt runs again in a new conversation.
  - If the agent ran, it continues the same conversation: with your reply or
    review comments if they never reached it, otherwise with a request to
    continue where it left off.
  - If the agent finished but GitEZ could not commit its work or create the
    pull request, GitEZ does those steps again and the agent does not run.

  **Open transcript** shows the agent's transcript in Finder.
- **Blocked requests**: below the thread's title, the sandbox line shows the
  network profile and how many hosts the sandbox blocked in the last run, in
  red. Click the count to show or hide the hosts; the list is open when the
  count is not zero. Claude Code contacts two hosts in every
  run that a thread does not need: `mcp-proxy.anthropic.com` (its claude.ai
  connectors) and `http-intake.logs.us5.datadoghq.com` (its telemetry). The
  sandbox blocks them too, but they are not counted: they are listed apart,
  muted, as "not needed", and they never suggest a wider network. A red
  count means that the agent or a command could not reach a host that it
  probably needed, such as a package registry; **Retry with Packages** is
  then offered when the Packages profile allows that host.
- **Change a thread's network**: the profile in the sandbox line is a menu.
  Pick another profile to continue the thread where it left off, in the same
  conversation, instead of starting it again. The agent is told that the
  network changed, and to try again what was blocked:
  - **Continue with …** for a thread that ended with a pull request or an
    answer (for example, one that could not download its dependencies).
  - **Retry with …** for a thread that failed or was stopped, as Retry does.
  - **Restart with …** for a thread that is queued or running: GitEZ stops it,
    then continues it under the new profile.

  The hosts that Packages allows come from Settings when the run starts, so
  after you correct that list, continuing a thread uses the new list.
- **Local ports of a thread**: beside the profile, the sandbox line says
  **Local ports on** or **off**, and is a menu that turns them on or off the
  same way, continuing, retrying or restarting the thread in the same
  conversation. The agent is told what changed.
- At most three agents run at once across all repositories (you can change
  the limit in Settings); other threads wait as Queued. A thread that reaches
  the agent's usage limit waits and continues by itself after the reset.
- When the agent stops, GitEZ commits what it left uncommitted on the
  thread's branch. If the branch has commits of its own, GitEZ opens a
  **local pull request** against the starting branch, with the agent's final
  message as its description. The agent names the change: the title is the
  heading its final message starts with, else the subject of its first
  commit, else the first sentence of your prompt. The thread takes the same
  title; its branch keeps the name it got from the prompt.
- **Review loop**: on a pull request that a thread opened, **Send to Claude
  Code** sends your review comments back to that thread, which continues in
  its own worktree. The head branch does not have to be checked out. Merging
  or closing the pull request completes the thread.
- **Merging** a thread's pull request (merge commit, squash or rebase) also
  deletes the thread's branch, `agent/<name>`, once its worktree is removed,
  unless the branch has commits that were not merged or is checked out
  somewhere; the thread then says why it kept it. The thread records the
  commit the branch was at (`git branch agent/<name> <commit>` brings it
  back), and the merged pull request still shows its changes. **Closing** a
  pull request keeps the branch: its commits are nowhere else.
- **Complete** removes the thread's worktree, and its branch if the branch
  has no commits of its own. The thread stays readable. Completing a Discuss
  thread only ends it: there is nothing to remove. If the worktree has
  uncommitted changes, GitEZ asks first: discard them, or complete and keep
  the worktree to remove later. A merge or close that completes a thread
  always keeps a worktree with uncommitted changes, and its branch with it;
  **Remove worktree** removes them later, the branch by the rule above.
- **Agents indicator**: the toolbar shows how many threads run and how many
  wait for you. Click it for this repository's threads, grouped by what they
  need, each with the action that fits (review, stop, retry, complete), and a
  link to all threads in Home. The Home button at the left end of the
  toolbar, and File > Show All Agent Threads, close the repository and show
  Home.
- **Home** is shown when no repository is open. It has a launcher that starts
  a thread in any repository (`⌘N`), the recent repositories with Open, Clone
  and New repository, and a sidebar with the threads of every repository:
  finished threads first, newest first. The active ones and those waiting
  for a slot or the usage limit are hidden in a collapsed **Active** section
  at the bottom of the sidebar; click it to show them. It opens on its own
  when you start a thread from the launcher. Completed threads are not
  listed. A finished thread with a pull request opens as a thread, or opens
  its pull request in its repository.
- **Moved or deleted repositories**: if a repository's folder is gone, Home's
  recent repositories and the launcher's Repository picker show it muted, as
  "Moved or deleted", and you cannot open or pick it. **Remove from list**
  beside it takes it off the recent repositories. If the repository had
  threads, Home's sidebar also says that its threads are not listed;
  **Remove from list** there stops GitEZ looking for them. Open the
  repository where it is now to list its threads again.
- **Settings** (`⌘,`, the app menu, or the gear in the Agents popover):
  - Claude Code's binary, configuration folder, environment variables,
    launch arguments, default model and default effort level, and whether it
    is signed in.
  - How many agents run at once.
  - The sandbox: whether it is installed, the network profile new threads
    start with, whether they start with local ports on (off by default), the hosts the Packages profile adds, the paths hidden from
    the agent, and extra folders the agent may write in every thread.
  - Secrets: named environment variables whose values are kept in the OS
    keychain (Keychain on macOS, the Secret Service on Linux), never in a
    file. A value is not shown again after you enter it. A secret is for all
    repositories or one, and can be given to new threads by default; a
    thread gets the secrets given by default. In the transcripts and logs that GitEZ
    writes, each value is replaced with `[secret NAME]`.
  - The repository's setup command, kept as `gitez.setupCommand` in its git
    config.

Thread data is stored in `.git/gitez/threads`, beside the reviews.

### The agent sandbox

Every agent thread runs in a sandbox, and there is no option to turn it off.
The sandbox wraps the agent's whole process, so it also applies to every
command the agent runs. GitEZ uses Anthropic's sandbox runtime
([`srt`](https://github.com/anthropics/sandbox-runtime)); for the
Open network profile, which `srt` cannot express, it uses a `sandbox-exec`
profile of its own on macOS and bubblewrap on Linux, with the same file rules.
The setup command, and GitEZ's own commit of what the agent left uncommitted,
run in the same sandbox.

- **Writes**: the agent can write its worktree, the repository's objects and
  its own branch, its own temporary folder and caches in GitEZ's data folder,
  the per-project and per-session files Claude Code keeps in its
  configuration folder, and the folders you add in Settings. It cannot write
  anywhere else: not your home folder, not your main worktree, not other
  branches, and not the repository's `.git/hooks` or `.git/config`. Files in
  the worktree that make other tools run code (`.gitmodules`, `.gitconfig`,
  `.vscode/`, `.idea/`, `.mcp.json`, `.claude/commands`, `.claude/agents` and
  shell startup files) are read-only too. `git stash` does not work in a
  thread.
- **Writes in a Discuss thread**: only its own temporary folder and caches
  in GitEZ's data folder, and the files of its own conversation in Claude
  Code's configuration folder. Not your working tree, not the repository's
  objects or branches, and not the folders you add in Settings. A build that
  writes into the working tree fails; the agent is told to send build output
  to its temporary folder. The setup command does not run.
- **Hidden paths**: by default the agent cannot read `~/.ssh`, `~/.aws`,
  `~/.gnupg`, `~/.config/gh`, `~/.netrc`, `~/.kube`, `~/.docker/config.json`,
  `~/.git-credentials`, `~/.config/git/credentials`, `~/.npmrc`, `~/.pypirc`,
  `~/.cargo/credentials` and `~/.cargo/credentials.toml`. You can change the
  list in Settings.
- **git**: the agent's commits are not signed, because it cannot reach your
  signing keys, and git credential helpers are off for every command of a
  thread.
- **Network**: each thread has one of three profiles.

  | Profile | Allows |
  | --- | --- |
  | Agent API only (default) | The agent's own API (`api.anthropic.com`, `platform.claude.com`). Downloads fail; Cargo and Go run offline. |
  | Packages | Also package registries and code hosts (crates.io, npm, PyPI, GitHub, the Go module proxy and others, editable in Settings), and the hosts of the repository's remotes. The setup command always runs with this profile. |
  | Open | Any host. |

  **Local ports** (off by default) lets the agent listen on local ports and
  connect to localhost, so it can run a project's dev server and test it with
  `curl` or a test runner. On macOS this opens every service on your Mac's
  localhost to the agent, not only its own servers (databases without a
  password, Docker's API, a browser's debugging port, a local proxy that could
  carry data past the network profile), and a server it starts on all
  addresses can be reached from your network. The Open profile allows all of
  this anyway. On Linux the sandbox has a loopback of its own, which works
  without the switch and reaches nothing of your computer's. Only the agent
  gets local ports, never the setup command. The agent is asked to stop the
  servers it started before it finishes its turn.

> [!CAUTION]
> Know the limits of the sandbox:
>
> - Apart from the hidden paths, the agent can read everything you can. The
>   sandbox cannot separate the agent from the commands it runs, so with the
>   Packages or Open profile an agent could send out anything it can read,
>   including its own Claude login and the secrets you gave the thread.
> - With Packages or Open, a thread gets its own copy-on-write copy of your
>   package caches (Cargo, npm and others), so that it never writes the
>   caches your own builds use. Cargo then builds the dependencies again once
>   per thread. To share a cache instead, which is faster but lets the agent
>   write files your own builds use, add it in Settings (for example
>   `~/.cargo/registry` and `~/.cargo/git`).
> - The agent writes the thread's worktree. Treat it as an untrusted checkout
>   when you open it in an editor, a terminal or another tool. GitEZ checks
>   the worktree's git files before it runs git there.
> - A Discuss thread runs in your working tree, so Claude Code keeps its
>   conversation with your own Claude Code conversations for that folder:
>   `claude --resume` there lists it. The agent cannot write the rest of
>   that folder, such as the memory your own sessions load.

### Everything else you expect from a git client

- **History**: a virtualised commit graph, with branches, remotes, tags,
  stashes and local PRs in the sidebar, and commit details and full diffs
  beside it. Branch and tag labels sit in their own column left of the graph,
  joined to their commit by a line; a laptop marks a local branch and a cloud
  a branch on a remote, so `main` and `origin/main` at the same commit share
  one label. People credited with a `Co-authored-by:` trailer are listed under
  the commit's author, with the mark of a known agent or bot (Claude, Codex,
  Copilot, Cursor, GitHub Actions, Dependabot and others).
- **Working tree**: stage, unstage or discard files and single hunks. Commit,
  amend, sign off or skip hooks; type the commit summary in the panel or right
  in the uncommitted-changes row, which is always at the top of the graph.
  Changes made in other tools appear as they happen.
- **Drag-and-drop merge**: drag a branch, from its label in the graph or its
  row in the sidebar, onto another branch in either place to merge, squash,
  rebase, fast-forward, or open the two branches as a local PR.
- **Conflict editor**: see our and their lines side by side (with the base if
  you want it), and tick lines into the result, or a side's box to take all
  of it; the side you tick first goes first. Click a result line to edit it
  by hand. "Why these lines conflict" shows the commits each branch
  made since the two split, picks out those that changed the conflict's lines,
  and labels each line with its commit. Click a commit to read its message
  and see what it did to those lines.
- **Cherry-pick**: select commits and pick them onto any branch. GitEZ does a
  trial run against the branch you choose before it picks. A branch checked
  out in another worktree is picked onto in that worktree.
- **Rewrite history**: reword, edit, squash, fixup, drop and reorder commits,
  and autosquash `fixup!` commits. The old tip is kept as `refs/gitez/backup/<branch>`.
- **Branches, remotes, tags and stashes**: create, rename, delete, check out,
  pull, push, fetch, set upstreams, revert and reset. Pulling a branch that is
  not checked out fast-forwards it. If the branch is checked out in another
  worktree, GitEZ pulls it there, so that worktree's files follow; it changes
  nothing when the branch has commits of its own, when uncommitted changes
  are in the way, or when the worktree is missing or locked, and says which.
  An agent thread's branch is not pulled while it is checked out in the
  thread's worktree: GitEZ runs `git` there only inside the sandbox.
  When a push of the branch checked out is rejected because the remote has
  new commits, GitEZ offers to pull them first, by merge or by rebase, and
  then pushes again. When the branches have diverged, it also offers to
  force push.
- **Automatic fetch**: GitEZ fetches from every remote in the background
  when the last fetch is 5 minutes old, so new commits on the remotes show up
  without asking. The status bar shows "Fetching…" while it runs. It gives
  way to a fetch, pull or push you start. It never asks for a password: if a
  remote needs one, GitEZ says once that the fetch failed. Turn it off with
  `git config gitez.autoFetch false`. Change the interval with
  `git config gitez.autoFetchMinutes 15`.
- **Cancel a fetch, pull or push**: while one runs, **Cancel** is in the
  toolbar and on the branch's or remote's row in the sidebar. It ends `git`
  and every process it started, such as `ssh`: they are asked to stop and
  killed if they are still there 2 seconds later. If `git` writes a message
  and then neither continues nor exits for 3 seconds, GitEZ shows the message
  while the operation is still running, with **Cancel** beside it. A `git`
  command run from the palette has **Cancel** too. **Stop** on an agent
  thread likewise ends the `git` that GitEZ runs to make its worktree, with
  any hook it waits for.
- **Undo and redo** for most actions: commits, checkouts, merges, rebases,
  resets, stashes, discarded changes and more.
- **Syntax highlighting** in diffs, the conflict editor and the file editor,
  for about 200 languages (bat's grammars on syntect). Whole files are
  highlighted, so multi-line strings and comments are coloured right; in a
  diff of a file over 1 MB, each hunk is highlighted on its own. In diffs and
  the conflict editor, binary files and lines over 2,000 bytes stay plain, and
  a conflicted file over 1 MB is not highlighted.
- **Command palette** (`⌘K`): find actions, commits, branches and files, or
  run any `git` command and see its output stream in.
- **Clone or create** a repository from Home, the File menu or the palette.
- **Project icon**: the top left of the window shows the open repository's favicon, app icon (`.icns` included) or logo if a tracked file looks like one, and otherwise a badge with the first letter of its name. Home's recent repositories and the launcher's Repository picker show the same icon for each repository.
- **Reopen where you left off**: GitEZ opens the repository that was open when it last quit, and Home, File > Open Recent and the repository name at the top left list the last ten; click the name to switch repository or open another, and hover a repository in that list to remove it with its × button. File also has Reveal in Finder, Close Repository and Close Window.

> [!NOTE]
> GitEZ reads the repository with [gitoxide](https://github.com/GitoxideLabs/gitoxide),
> in-process. Every change runs your own `git` binary, so hooks, commit signing
> and credential helpers work exactly as they do in the terminal.

## Requirements

- [Rust](https://rustup.rs), installed with rustup. The `rust-toolchain.toml`
  file selects the correct toolchain automatically.
- `git` on the `PATH`. When GitEZ starts from Finder or a desktop launcher, it
  takes the `PATH` from your login shell, so git filters and helpers such as
  `git-crypt` and `git-lfs` are found as they are in the terminal.
- Optional: [Claude Code](https://docs.claude.com/en/docs/claude-code), to send
  prompts to an agent. GitEZ looks for `claude` on the `PATH` and in the
  installers' default locations, such as `~/.local/bin`.
- For agent threads:
  - Claude Code, signed in (`claude auth login`). GitEZ runs your own
    `claude` with your own login and configuration, and never reads or
    stores the login. A thread can instead get an `ANTHROPIC_API_KEY` secret.
  - [Node.js](https://nodejs.org) 22.12 or newer, with `npm`, for the
    sandbox. Settings installs the sandbox runtime
    (`@anthropic-ai/sandbox-runtime@0.0.82`) into GitEZ's own data folder
    with `npm install --prefix`, never globally. An `srt` already on the
    `PATH` is used instead.
  - On Linux, also `bubblewrap`, `socat` and `ripgrep`.
- Optional: the [Geist](https://vercel.com/font) and
  [JetBrains Mono](https://www.jetbrains.com/lp/mono/) fonts. Without them,
  GitEZ uses the system UI and monospace fonts.

Windows is not supported yet.

> [!NOTE]
> Agent threads have been tested on macOS only. Their Linux code (the
> bubblewrap sandbox for the Open profile, `srt` on Linux, and secrets in the
> Secret Service) is untested.

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
cargo run                          # reopen the last repository, or start from Home
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
> necessary. A pull request that an agent thread opened is the exception: its
> comments go back to that thread, which runs sandboxed in its own worktree,
> whatever is checked out.

### Run an agent thread

1. Open Settings (`⌘,`). Check that Claude Code is signed in and that the
   sandbox is ready; click **Install** if it is not.
2. In a repository, press `⌘⇧A`, or in Home, press `⌘N`.
3. Write the prompt, pick the branch to start from, and press `⌘↵`.
4. Follow the thread in its lane or in the Agents popover. When it has
   finished, review its local pull request, reply in the thread, or click
   **Complete**.

### Keyboard shortcuts

On Linux, use `Ctrl` instead of `⌘`.

| Shortcut | Action |
| --- | --- |
| `⌘O` | Open a repository |
| `⌘W` | Close the window |
| `⌘K` | Open the command palette |
| `⌘⇧A` or `⌘N` | Ask an agent: the composer in a repository, the launcher in Home |
| `⌘,` | Open Settings for agents, the sandbox and secrets |
| `⌘Z` / `⇧⌘Z` | Undo / redo |
| `↑` `↓` or `k` `j` | Move through the history |
| `Enter` | Show the selected commit's full diff |
| `[` / `]` | Step to the newer / older commit in a diff |
| `Esc` | Go back, cancel a drag, or close the agent composer |
| `⌘↵` | Commit, run the typed `git` command in the palette, or send a prompt or reply to an agent |
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

### How an agent thread runs

GitEZ runs each thread in a separate process,
`gitez supervise start|reply|retry --git-dir <git common dir> --thread <id>`,
started detached from the window with the thread's secret values on its
standard input. The process waits for a free slot, prepares the worktree,
runs the agent in the sandbox, writes the transcript, commits what the agent
left and opens the pull request. A thread therefore keeps running after you
close the window or quit GitEZ. GitEZ starts this command itself; it is not
meant to be run by hand.

Where the data is:

| Path | Contents |
| --- | --- |
| `<git common dir>/gitez/threads/<id>/` | `thread.json`, one transcript per run in `runs/`, and the logs `supervisor.log` and `setup.log` |
| `<data folder>/settings.json` | Settings, with the names of secrets but never their values |
| `<data folder>/threads.json` | The repositories that have threads, so Home can list them |
| `<data folder>/worktrees/<repository>-<hash>/<id>/` | The threads' worktrees |
| `<data folder>/tmp/`, `<data folder>/caches/` | Each thread's temporary folder and package caches |
| `<data folder>/sandbox/` | The sandbox runtime that Settings installs |
| `<data folder>/queue/` | The lock files that limit how many agents run at once |

The data folder is `~/Library/Application Support/GitEZ` on macOS and
`$XDG_STATE_HOME/gitez` (`~/.local/state/gitez`) on Linux. It also holds
`recent.json`, the recent repositories.

### Environment variables

| Variable | Effect |
| --- | --- |
| `GITEZ_DATA_DIR` | Use this folder as the data folder, for tests and trial runs |
| `GITEZ_KEYCHAIN_SERVICE` | Keep secret values under this keychain service instead of `GitEZ` |
| `GITEZ_DEBUG_THREADS` | Log what the app does with agent threads to standard error |
| `GITEZ_TEST_SANDBOX=passthrough` | Debug builds only: run threads without a sandbox, for tests. The thread shows a warning. Release builds ignore it. |
| `GITEZ_SCREENSHOTS` | A folder: run the `screenshot_` and `e2e_` tests, which render views to PNG files there (macOS only). Without it they return at once. |
| `GITEZ_E2E=1` | With `GITEZ_SCREENSHOTS`: run the end-to-end tests with a fake agent and a real supervisor. Build `target/debug/gitez` first. |
| `GITEZ_E2E_REAL=1` | With `GITEZ_SCREENSHOTS`: run the end-to-end tests with the real Claude Code (model `haiku`, on your own account) in the real sandbox. They also need `GITEZ_E2E_SCRATCH` (a folder for the test repositories), `GITEZ_E2E_SANDBOX` (a `sandbox` folder with `srt` installed) and `GITEZ_E2E_CLAUDE` (the path of `claude`). |

```sh
cargo build -p gitez
GITEZ_SCREENSHOTS=/tmp/gitez-shots GITEZ_E2E=1 cargo test -p gitez e2e_
```

### Project layout

| Path | Contents |
| --- | --- |
| `crates/gitez` | The application binary and its views |
| `crates/gitez-git` | Read-only repository access on gitoxide: refs, history, graph layout, diffs, status, merge previews, file watching |
| `crates/gitez-command` | Changes made with the user's `git` binary: staging, committing, merging, cherry-picking, rebasing, remotes, cloning |
| `crates/gitez-review` | Local pull requests, comment threads and agent transcripts in `.git/gitez/reviews` |
| `crates/gitez-agent` | Coding agents: Claude Code run as `claude -p` with its streamed events parsed, and test command detection |
| `crates/gitez-highlight` | Syntax highlighting without GPUI: language detection and per-line token spans from syntect and bat's grammars |
| `crates/gitez-sandbox` | The sandboxes agents run in: `srt` (finding and installing it), the Seatbelt and bubblewrap profiles for the Open network profile, and the file and network policy |
| `crates/gitez-secret` | Secret values in the OS keychain |
| `crates/gitez-settings` | GitEZ's data folder and `settings.json`: agent entries, secret names and scopes, sandbox defaults, the running-agent limit |
| `crates/gitez-agent-thread` | Agent threads in `.git/gitez/threads`, the index of repositories with threads, and the threads' worktrees: creating, preparing, checking and removing them |
| `crates/gitez-supervisor` | The `gitez supervise` process that runs one thread: the queue, the run, the sandbox policy and environment, the commit of leftovers and the pull request |

The UI uses [gpui-kit](https://gpui-kit.com), which pins the matching GPUI
release. [CLAUDE.md](CLAUDE.md) describes the architecture decisions, the
design tokens and the process to update GPUI.
