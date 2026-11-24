# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What GitEZ is

A native, visual desktop git client (macOS and Linux; Windows later) written
in Rust on GPUI. Its distinguishing feature is **local pull
requests**: a branch can be reviewed like a PR without leaving the machine. The
reviewer comments on lines of the diff, and GitEZ turns the comments into a
prompt for a coding agent (Claude Code in v1) that can be copied or sent
directly. Review data is stored under the repository's `.git` directory
(`.git/gitez/reviews`, see "Review storage" below), never in the working
tree. GitEZ also runs **agent threads**: a coding agent works in the
background on its own `agent/<slug>` branch and linked worktree, inside a
sandbox, and its result is reviewed as a local pull request; or it only
discusses, reading the working tree and changing nothing (see "Agent
threads" below).

The reference design is a Design canvas at
<https://claude.ai/artifact/ELtu5gwJExRfcVQHVNCGs8>. Read it with the Artifact
tool (`action: "read"`, then the `project/*.dc.html` files) before building a
screen. Artboards:

| Artboard | Screen |
| --- | --- |
| `Main.dc.html` | History, main window: toolbar, sidebar (branches, remotes, tags, stashes, local PRs), commit graph, commit detail |
| `Commit.dc.html` | Commit, scroll-through review of the working tree |
| `Palette.dc.html` | Command palette (`⌘K`), runs any git command |
| `LocalPR.dc.html` | Local pull request: files, inline comments, agent prompt panel |
| `Merge.dc.html` | Drag-and-drop merge |
| `CherryPick.dc.html` | Cherry-pick selection |
| `Rewrite.dc.html` | Reword and rewrite history |
| `Conflicts.dc.html` | Merge conflict editor |

The screens of agent threads follow a separate prototype canvas at
<https://claude.ai/artifact/2zzcufhPFe5WNNfmUUwwuw>, read the same way. Only
the `C` boards apply (the `A`, `B` and `D` concepts were rejected); their file
names still say "Session", and their sample data are placeholders:

| Board | Screen |
| --- | --- |
| `C-1-Start.dc.html` | "Ask an agent…" composer in the commit graph |
| `C-2-Sessions.dc.html` | Active threads as live lanes; toolbar Agents popover |
| `C-3-Session.dc.html` | One thread in the detail panel |
| `C-4-Sandbox.dc.html` | Settings: sandbox and secrets |
| `C-5-Home.dc.html` | Home: launcher and all threads |
| `C-6-Thread.dc.html` | Home: a finished thread |

### Design tokens (dark theme)

Fonts: Geist (UI), JetBrains Mono (code, SHAs, keyboard hints). Base size 13px.

| Token | Hex |
| --- | --- |
| background | `#121214` |
| header / panel background | `#161618` |
| row hover | `#19191d` |
| border | `#26262b` |
| button background / border | `#1f1f23` / `#2e2e34` |
| text | `#e7e5e1` |
| text secondary | `#cfcbc0` |
| text muted | `#9a988f` / `#8d8b83` |
| accent (amber) | `#e8b04b`, hover `#f0bf63`, on-accent text `#1a1408` |
| selected row | `#2a2418` |
| branch colours | blue `#5fa8f5`, violet `#c08cf0`, teal `#4fc6b0` |
| diff added / removed | `#6cc58a` / `#f08a7e` |

Keep these in a theme module; do not scatter hex literals through views.

## Architecture decisions

Decided 2026-10-23; change only with the owner's agreement.

- **Git backend: gitoxide for reads, git CLI for writes.** Use `gix` in-process
  for everything that reads the repository (commit graph, refs, diffs, blame,
  status), because it is fast and does not block on a subprocess. Run the
  user's own `git` binary for every mutating operation (commit, merge, rebase,
  cherry-pick, push, fetch, stash) so hooks, signing and credential helpers
  behave exactly as they do in the terminal. Never reimplement a write path
  that git already provides.
- **UI toolkit: gpui-kit** (GPUI Component, <https://github.com/longbridge/gpui-kit>,
  docs at <https://gpui-kit.com>) on top of GPUI for inputs, lists, virtual
  lists, data tables, scrollbars, popovers, menus, dock layout and a code
  editor. Style it with the GitEZ theme tokens via its theme system. `gpui-kit`
  pins the GPUI snapshot it is built against and re-exports it; the app never
  depends on `gpui` directly.
- **Agent integration v1: run Claude Code headless from GitEZ.** "Send to
  Claude Code" spawns the `claude` CLI in print mode (`claude -p`, streaming
  JSON output) in the repository's working directory and shows the streamed
  output in a panel. "Copy" puts the same prompt on the clipboard. Keep the
  agent behind a trait so other agents can be added later. It runs with
  `--permission-mode bypassPermissions` (decided 2026-10-24), since a
  headless run cannot ask for permission; it starts only when the pull
  request's head branch is checked out — except for a pull request an agent
  thread created (and that thread is not completed), whose review comments
  continue that thread (`--resume`) in its own worktree, sandboxed, whatever
  is checked out; merging or closing such a pull request completes the
  thread, and merging it (any method) also deletes the thread's branch once
  its worktree is removed, unless its tip is neither the merged head nor in
  the base, or it is checked out (`deleted_branch` in `thread.json` records
  the tip).
- **Review storage** (decided 2026-10-24). Local pull requests live in
  `<git common dir>/gitez/reviews/`, so linked worktrees share them and
  nothing reaches the working tree, a commit or a push. The `gitez-review`
  crate owns the format; nothing else reads or writes these files.
  - One folder per pull request, named by its number (the highest existing
    number plus one): `reviews/<n>/review.json` holds everything about it,
    `reviews/<n>/runs/<UTC time>.jsonl` holds one transcript per agent run.
  - `review.json` is one JSON document with a `version` field: number,
    title, description, state (`open`, `merged`, `closed`), base and head as
    branch plus the last commit seen, created and updated times (RFC 3339,
    UTC), viewed files (path to blob id), prompt options, and threads.
  - A thread has an id, an anchor (commit, path, `old` or `new` side, first
    and last line, and a copy of those lines), whether it is in the agent
    prompt, its resolution (`null`, or by `auto` or `you` with commit and
    time) and its comments (id, body, time, and the time of the last edit
    if it was edited). When the head moves, a thread
    whose lines changed resolves itself; the others are re-anchored to the
    new commit.
  - A transcript starts with a GitEZ header line (time, agent, prompt), then
    the agent's stream-JSON events verbatim, one per line.
  - Writes go to `review.json.lock`, created exclusively, then renamed over
    `review.json`, as git does with its own files. A newer `version` than
    GitEZ knows is refused, never overwritten.
  - Plain file I/O, not the git CLI: git has no command for this data.
- **Agent threads** (decided 2026-11-13). A thread is a prompt given to a
  coding agent that works in the background, and the conversation that
  follows. Claude Code is the only agent; others come behind the `Agent`
  trait and the `Registry` of `gitez-agent`.
  - Modes (decided 2026-11-16): a thread is **Implement** (the default;
    everything below, unless it says Discuss) or **Discuss** (`mode` in
    `thread.json`, left out for Implement). A Discuss thread runs in the
    user's checked-out working tree (`work_dir`) and changes nothing: no
    branch, no worktree (`branch` and `worktree` are absent), no Preparing,
    no setup command, no commit of leftovers, no pull request; every turn
    ends as an answer. It needs a branch checked out, and has `tmp/` and
    `caches/` folders of its own (`scratch`). The composer remembers the
    mode used last (`threads.last_mode` in `settings.json`). An older GitEZ
    cannot read a Discuss record and leaves it alone; `version` stays 1.
  - Auto (decided 2026-11-17) is a third composer choice, not a third
    mode: it starts a Discuss thread marked `auto` in `thread.json`, with
    Discuss's requirements and no From picker. The composer remembers it as
    `threads.last_auto` beside `last_mode` (which stays `discuss`, so a
    GitEZ without Auto still reads the file). Its agent's system prompt
    adds one rule: when a message (the first or any reply) is a clear
    request to change code and it is clear what to do, answer with the line
    `HANDOFF_MARKER` (`[gitez:implement]`) and then a brief in the
    `BRIEF_PROMPT` format, saying when it depends on uncommitted changes;
    otherwise answer as a discussion does. The supervisor strips the marker
    from the turn's message and marks the turn with `handoff`; the marker
    is never shown. GitEZ then starts the Implement thread itself, without
    asking, through `AgentThreads::start`'s path (the brief as prompt, its
    `# heading` as title, `origin` set, the discussion's branch at its
    current tip, its agent, model, effort, network and secrets). It first claims
    the turn's handoff under the thread's lock (`claim_handoff`), then
    records the new thread's id (or why none started, such as a branch
    that is gone) in it, so each marked turn starts at most one thread,
    also when GitEZ was closed as the turn ended (it hands off at the next
    launch). "Implement…" works as in any discussion.
  - Handoff: "Implement…" in a Discuss thread asks the agent for a brief
    (`BRIEF_PROMPT`, a turn of kind `brief`), then opens the composer with
    it as the prompt of a new Implement thread, which starts with a fresh
    context and names the discussion in `origin`. The brief's `# heading`
    becomes the new thread's title.
  - An Implement thread is one branch, `agent/<id>` (the id is a slug of
    the title
    taken from the prompt, with `-2`, `-3`… when taken; the title, not the
    id, becomes the pull request's when the supervisor creates it), and one
    linked worktree at
    `<data folder>/worktrees/<repository name>-<8 hex digits of a hash of the
    common dir>/<id>`, never inside the repository. GitEZ makes it with `git
    worktree add`, never `claude --worktree`. The data folder is
    `gitez_settings::data_dir()`: `$GITEZ_DATA_DIR`, else
    `~/Library/Application Support/GitEZ` on macOS and
    `$XDG_STATE_HOME/gitez` elsewhere. Tests use a data folder of their own,
    never the owner's.
  - Storage: `<git common dir>/gitez/threads/<id>/thread.json` (one JSON
    document with a `version` field) and `runs/<UTC time>.jsonl` (one
    transcript per turn: a GitEZ header line, then the agent's events), with
    the same lock-and-rename writes and refusal of a newer `version` as
    review storage. The `gitez-agent-thread` crate owns the format and the
    worktrees; nothing else reads or writes these files.
  - Images (decided 2026-11-13): pasted or dropped into a prompt or reply,
    made fit to send by `gitez_agent_thread::prepare_image` (at most 5 a
    turn, long side 2,576 px, 3,932,160 bytes, else scaled down to PNG or
    JPEG), kept in `threads/<id>/images/` under names GitEZ makes
    (`<UTC time>-<n>.<ext>`, never the clipboard's), and listed by name in
    the turn (`images`) and in the transcript's header line. Only a turn
    with images changes the agent's input: Claude Code gets
    `--input-format stream-json` and one user message on standard input,
    the images first. Transcripts never keep image data:
    `Agent::transcript_line` replaces it with `[image omitted by GitEZ]`.
    Whether a model takes images is `Agent::accepts_images`.
    `<data folder>/threads.json` indexes the repositories that have threads,
    so Home lists threads of repositories that are not open.
  - Every run (start, reply, review comments, retry) goes through a detached
    `gitez supervise <start|reply|retry> --git-dir <common dir> --thread
    <id>` process in its own session, so a thread outlives the window and
    GitEZ itself. The GUI writes the thread's record and its pending turn,
    launches the supervisor, watches the thread files and can stop the
    supervisor (SIGTERM; the supervisor then sends SIGINT to the
    agent's process group, and SIGKILL after 20 s). Success is read from the
    agent's `result` event, never from an exit code. At most
    `threads.max_running` agents (default 3) run at once across all
    repositories; the rest wait as Queued, coordinated through lock files in
    `<data folder>/queue/`.
  - Every run is sandboxed: the agent, the setup command (`gitez.setupCommand`
    in the repository's git config, run with the Packages network), the
    checkout of a branch the agent already wrote, and GitEZ's commit of the
    agent's leftovers (no network). There is no unsandboxed option in the UI
    or in release builds; `GITEZ_TEST_SANDBOX=passthrough` works only in
    debug builds, for tests. The sandbox is behind the `Sandbox` trait of
    `gitez-sandbox`: Anthropic's `srt`, pinned to
    `@anthropic-ai/sandbox-runtime@0.0.82` (Node.js 22.12 or newer), taken
    from the `PATH` or installed by Settings with `npm install --prefix
    <data folder>/sandbox`, never globally. Network profiles: `agent_only`
    (the default, "Agent API only"), `packages` (adds the hosts in Settings
    and the hosts of the repository's remotes) and `open`. `srt` cannot
    express Open, so Open runs under a GitEZ-generated `sandbox-exec`
    profile with the same file rules on macOS (bubblewrap without a network
    namespace on Linux, untested). Claude Code's own Bash sandbox is turned
    off (`--settings '{"sandbox":{"enabled":false}}'`): it cannot nest. No
    JavaScript ships with GitEZ; `srt` is an external program.
  - Local ports (decided 2026-11-24): a per-thread switch, off by default,
    that lets the agent (never the setup command or the commit) listen on
    local ports and connect to localhost, to run a dev server and test it
    (`Policy::local_ports`, srt's `network.allowLocalBinding`). It is all
    or nothing: on macOS it opens every service on the user's localhost
    (and with it any local proxy past the host list), and binds `*:*`, so
    the UI says so wherever it is turned on. Open allows it anyway; on
    Linux srt ignores it (the namespace has its own loopback). `local_ports`
    in `thread.json` and `sandbox.default_local_ports` in `settings.json`
    are left out when off. The composer has a Local ports picker beside the
    others (not inside the network, which is Settings'); an Auto handoff
    keeps the discussion's choice.
  - A thread's network can change after it started (decided 2026-11-24),
    from the profile in the thread view's sandbox line. The change never
    starts over: `AgentThread::request_network_change` sets `network` and
    queues a Retry turn that resumes the conversation with
    `network_changed_prompt` (for a failed or stopped thread it is
    `request_retry`, with that prompt when it continues). An active thread
    is stopped first (`AgentThreads::change_network` waits up to 60 s for
    it to end), then continued. A completed thread, or one that ended
    without a conversation, cannot change. Local ports change the same way,
    from the switch beside the profile: both are a `SandboxChange`
    (`request_sandbox_change`, `AgentThreads::change_sandbox`), and a
    local ports change is told with `local_ports_changed_prompt`.
  - A thread's worktree is hostile once the agent has run in it. The policy
    lets the agent write its worktree, the object store, its own branch's
    ref, reflog and lock, `packed-refs.lock`, its own `tmp/` and `caches/`
    folders in the data folder, the per-project and per-session paths of its
    configuration folder, and the folders the user adds in Settings. A
    Discuss thread may write only its own `tmp/` and `caches/` and what
    the agent needs of its configuration folder; there, because the
    working tree's per-project folder is shared with the user's own
    Claude Code sessions, only this conversation's files
    (`projects/<folder>/<session id>.jsonl` and
    `projects/<folder>/<session id>/`), never the rest of that folder (the
    memory the user's sessions load). It runs with `GIT_OPTIONAL_LOCKS=0`.
    Both modes are denied
    the common dir's `hooks`, `config` and `gitez/`, the object
    store's alternates and commit graphs, the worktree's `.git` file, the
    `commondir`, `gitdir`, `config`, `config.worktree` and `locked` files of
    the worktree's git dir, and GitEZ's `settings.json` and `threads.json`;
    a writable path that is itself a symbolic link is left out. The user's
    package caches stay read-only: commands that may download get
    copy-on-write clones in the thread's `caches/`. Before GitEZ runs git in
    a worktree outside the sandbox it checks the `.git` pointer and the git
    dir's files against what it recorded when it made the worktree
    (`check_worktree`; on a mismatch it runs nothing there). Status and the
    commit of leftovers run inside the sandbox. The rebase of a thread's
    branch ("Rebase and merge") runs with hooks, fsmonitor, submodule
    recursion and credential helpers off (`Git::rebase_onto_untrusted`).
    Every command run for a thread gets `commit.gpgsign=false`, an empty
    `credential.helper`, `gc.auto=0` and `maintenance.auto=false` through
    `GIT_CONFIG_*`.
  - Secrets: values only in the OS keychain (`gitez-secret`, service
    `GitEZ`, or `GITEZ_KEYCHAIN_SERVICE` in tests); names, scope and the
    default switch in `settings.json`; names only in `thread.json`. The GUI
    passes the values to the supervisor as JSON on standard input, never on
    a command line. The supervisor puts them in the agent's environment and
    replaces each value with `[secret NAME]` in every transcript, log and
    message it writes; values shorter than 4 characters are refused.
  - The agent uses the user's own Claude Code login and configuration.
    GitEZ starts the user's unmodified `claude`, lets the sandbox reach its
    login, and asks `claude auth status --json` only whether it is signed
    in. GitEZ never reads, stores or forwards the credential, and never
    makes the whole configuration folder writable.
  - Naming, in the UI and in code: the noun is "thread" (never "session",
    "task" or "job"); the verb is "Complete" (never "archive" or
    "dismiss"); the modes are "Discuss" and "Implement" (never "chat",
    "plan" or "ask"), the composer's third choice is "Auto" (never
    "smart" or "automatic"; in code `ThreadChoice::Auto` and `auto`), and
    what goes from one to the other is a "brief";
    the states are one word each: Queued, Preparing, Running,
    Waiting, Finished, Completed, and a finished thread ended as a pull
    request, an answer, failed or stopped. In code an agent thread is
    always `AgentThread…` (`AgentThread`, `AgentThreadId`, `AgentThreadKey`,
    `AgentThreads`), so it is never confused with `gitez_review::Thread`, a
    review comment thread.
- **Milestone 1: open a repository and show its history.** Repository picker,
  sidebar (branches, remotes, tags, stashes) and a virtualised commit graph.
  Local PR review and the working-tree commit flow follow.

## Stack and conventions

- Rust stable, edition 2024. The toolchain is pinned in `rust-toolchain.toml`.
- GPUI arrives through `gpui-kit`, which depends on the `gpui-pre` crates.io
  snapshot of GPUI (`=0.3.9` for gpui-kit 0.8.0) and re-exports everything as
  `gpui_kit::*`. Import `use gpui_kit::*;` for GPUI types and
  `use gpui_kit::component::*;` for widgets. GPUI is pre-1.0 with frequent
  breaking changes; the API reference is the `gpui-pre` source for the pinned
  version (`cargo doc --open` or docs.rs), not older blog posts.
  `gpui_kit::application()` creates the app, `gpui_kit::init(cx)` must run
  first, `gpui_kit::open_window` opens a window with the gpui-kit `Root`
  mounted, views are structs implementing `Render`, elements are built with the
  `div()` builder and Tailwind-like methods (`.v_flex()`, `.h_flex()`, `.gap_2()`).
- Cargo workspace. `crates/gitez` is the binary. Add new functionality as
  crates under `crates/` when it has a clear boundary (git backend, review
  storage, agent integration, UI components); keep the binary crate thin.
- Lints are set in the workspace `Cargo.toml` (`clippy::all`, `unsafe_code`
  warns). Code must be `cargo fmt` clean and `cargo clippy --all-targets
  --workspace` clean before it is considered done; without `--workspace`
  clippy checks only the default member, the `gitez` binary.
- Performance is a primary goal. Never block the UI thread on git or disk I/O;
  use GPUI's background executor and stream results. Virtualise long lists
  (commit graph, diffs). Prefer measuring to guessing.
- Errors: `anyhow` at the application boundary, typed errors inside library
  crates.
- No `unwrap()` outside tests and `main`; use `expect` with a reason or return
  an error.

## Commands

```sh
cargo run                      # build and launch (first GPUI build takes minutes)
cargo build --release
cargo fmt --all
cargo clippy --all-targets --workspace
cargo test --workspace
```

`cargo test --workspace` never calls the real `claude` (tests use a fake agent
program); the `gitez-secret` tests use the OS keychain under a service of
their own (`GitEZ-test-<pid>-<n>`) and remove what they add. Gated tests
return at once unless their variables are set:

```sh
# Render real views to PNG (macOS; GPUI's test platform and Metal, no window).
# Look at the PNGs with the Read tool. Helpers: crates/gitez/src/screenshot.rs.
GITEZ_SCREENSHOTS=<folder> cargo test -p gitez screenshot_

# End to end with a fake agent and a real supervisor, without a sandbox.
cargo build -p gitez
GITEZ_SCREENSHOTS=<folder> GITEZ_E2E=1 cargo test -p gitez e2e_

# The real srt and Seatbelt, and real toolchains inside them (srt installed in
# <data>/sandbox; `real_install` installs it with npm).
GITEZ_SANDBOX_TEST_DATA_DIR=<data> cargo test -p gitez-sandbox -- --ignored --test-threads=1
GITEZ_SANDBOX_TEST_DATA_DIR=<data> cargo test -p gitez-supervisor --test toolchains -- --ignored --nocapture
```

`GITEZ_E2E_REAL=1` (with `GITEZ_E2E_SCRATCH`, `GITEZ_E2E_SANDBOX`,
`GITEZ_E2E_CLAUDE` and optionally `GITEZ_E2E_CLAUDE_CONFIG`) runs the `e2e_`
tests with the real Claude Code on the owner's account; run it only when asked,
with the `haiku` model, in throwaway repositories. `GITEZ_DEBUG_THREADS=1`
logs the app's agent-thread events to standard error.

## Updating GPUI

GPUI moves only when `gpui-kit` moves.

1. Pick a new `gpui-kit` release (<https://crates.io/crates/gpui-kit>) and read
   its release notes for breaking changes.
2. Change the exact version of `gpui-kit` in the root `Cargo.toml`; if the
   `gpui-pre` snapshot changed, update the `[profile.dev.package.gpui-pre*]`
   entries only if their names changed.
3. If gpui-kit requires a newer stable Rust, update `rust-toolchain.toml`.
4. `cargo build`, fix API breakage, run the app.

## Working rules

- Match the design artboards; when the design is silent, follow the existing
  tokens and spacing rather than inventing a new style.
- Do not commit or push unless asked. Do not add the `target/` directory,
  secrets or local settings to git.
- Do not introduce a second UI framework, a web view, or JavaScript.
- Keep `README.md` in step with user-facing changes (requirements, commands,
  features).
