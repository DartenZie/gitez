# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What GitEZ is

A native, visual desktop git client (macOS and Linux; Windows later) written
in Rust on GPUI. Its distinguishing feature is **local pull
requests**: a branch can be reviewed like a PR without leaving the machine. The
reviewer comments on lines of the diff, and GitEZ turns the comments into a
prompt for a coding agent (Claude Code in v1) that can be copied or sent
directly. Review data is stored under the repository's `.git` directory
(planned: `.git/gitez/reviews`), never in the working tree.

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
  agent behind a trait so other agents can be added later.
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
  warns). Code must be `cargo fmt` clean and `cargo clippy --all-targets` clean
  before it is considered done.
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
cargo clippy --all-targets
cargo test --workspace
```

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
