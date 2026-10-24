//! The command palette (`⌘K`), as in the `Palette.dc.html` artboard: one
//! line to find an action, a commit, a branch or a changed file, or to run
//! any git command. A git command GitEZ has a screen for is offered there;
//! any other runs exactly as typed, its output streaming into the palette.

use gitez_command::Git;
use gitez_git::{Oid, RefKind, Repository};
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::history::History;
use super::labels::{self, SECONDARY_KEY};
use super::remote::RemoteActions;
use crate::theme::{self, color};

actions!(palette, [SelectPrevious, SelectNext, FillArgument, Dismiss]);

/// Key bindings of the palette. They apply in its input, over the input's
/// own use of these keys.
pub fn key_bindings() -> Vec<KeyBinding> {
    const CONTEXT: Option<&str> = Some("Palette > Input");
    vec![
        KeyBinding::new("up", SelectPrevious, CONTEXT),
        KeyBinding::new("down", SelectNext, CONTEXT),
        KeyBinding::new("tab", FillArgument, CONTEXT),
        KeyBinding::new("escape", Dismiss, CONTEXT),
    ]
}

/// Items of one kind shown when the palette shows every kind.
const SHOWN_PER_KIND: usize = 6;
/// Items of one kind shown when the palette shows only that kind.
const SHOWN_OF_ONE_KIND: usize = 50;
/// Lines of a command's output kept; older ones are dropped.
const MAX_OUTPUT_LINES: usize = 5_000;

/// What the palette asks of the workspace.
pub enum PaletteEvent {
    Dismiss,
    Command(PaletteCommand),
    /// A git command run from the palette finished; `ok` says whether it
    /// succeeded.
    Finished {
        command: String,
        ok: bool,
    },
}

/// Something the workspace does for the palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteCommand {
    OpenRepository,
    NewPullRequest,
    CommitChanges,
    RewordLast,
    ContinueRebase,
    AbortRebase,
    ResolveConflicts,
    CherryPick,
    NewBranch,
    RenameBranch,
    StashChanges,
    PopStash,
    Fetch,
    Pull,
    Push,
    /// Rewrite the commits after `base`, a revision as typed, folding in
    /// fixups if `autosquash`.
    RewriteSince {
        base: String,
        autosquash: bool,
    },
    SelectCommit(Oid),
    Switch(String),
    OpenFile(String),
}

/// What the repository is like when the palette opens: which actions can
/// run, and the branches, tags and files to find.
#[derive(Default)]
pub struct PaletteContext {
    /// The branch checked out, if one is.
    pub branch: Option<String>,
    /// The commit `HEAD` is on.
    pub head: Option<Oid>,
    pub merging: bool,
    pub rebasing: bool,
    /// Commits picked in the history.
    pub picked: usize,
    /// Files with uncommitted changes.
    pub changed: Vec<String>,
    /// Local branches.
    pub branches: Vec<String>,
    /// What fetch, pull and push would do.
    pub remote: Option<RemoteActions>,
    /// Entries in the stash.
    pub stashes: usize,
}

/// The kinds of item, one tab each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    All,
    Actions,
    Commits,
    Branches,
    Files,
}

const TABS: [Tab; 5] = [
    Tab::All,
    Tab::Actions,
    Tab::Commits,
    Tab::Branches,
    Tab::Files,
];

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::All => "All",
            Tab::Actions => "Actions",
            Tab::Commits => "Commits",
            Tab::Branches => "Branches",
            Tab::Files => "Files",
        }
    }
}

/// What an item does when chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Run {
    Command(PaletteCommand),
    /// Run the git command typed.
    Typed,
}

/// One line of the results.
struct Item {
    /// The heading of the group it is in.
    section: SharedString,
    kind: Tab,
    icon: IconName,
    /// The first, recognised suggestion: drawn in the accent.
    primary: bool,
    title: SharedString,
    /// A short commit id before the title.
    sha: Option<SharedString>,
    /// A line under the title.
    subtitle: Option<SharedString>,
    /// Muted text at the end of the line.
    detail: Option<SharedString>,
    /// Why it cannot run now, shown instead of `detail`.
    disabled: Option<SharedString>,
    /// Keys shown at the end of the line, when not selected.
    keys: &'static [&'static str],
    run: Run,
    /// What Tab puts in the command for it.
    fill: Option<String>,
}

/// An action offered by name, and the git subcommands it relates to.
struct Action {
    title: &'static str,
    icon: IconName,
    keywords: &'static [&'static str],
    command: PaletteCommand,
}

fn actions() -> Vec<Action> {
    vec![
        Action {
            title: "Commit changes",
            icon: IconName::Check,
            keywords: &["commit", "add", "stage", "status"],
            command: PaletteCommand::CommitChanges,
        },
        Action {
            title: "Reword last commit",
            icon: IconName::Pencil,
            keywords: &["commit", "amend", "reword", "rebase"],
            command: PaletteCommand::RewordLast,
        },
        Action {
            title: "Continue rebase",
            icon: IconName::Play,
            keywords: &["rebase", "continue"],
            command: PaletteCommand::ContinueRebase,
        },
        Action {
            title: "Abort rebase",
            icon: IconName::Undo2,
            keywords: &["rebase", "abort"],
            command: PaletteCommand::AbortRebase,
        },
        Action {
            title: "Resolve conflicts",
            icon: IconName::GitMerge,
            keywords: &["merge", "conflicts", "mergetool"],
            command: PaletteCommand::ResolveConflicts,
        },
        Action {
            title: "Cherry-pick selected commits",
            icon: IconName::Cherry,
            keywords: &["cherry-pick", "pick"],
            command: PaletteCommand::CherryPick,
        },
        Action {
            title: "New branch…",
            icon: IconName::GitBranch,
            keywords: &["branch", "switch", "checkout", "create"],
            command: PaletteCommand::NewBranch,
        },
        Action {
            title: "Rename branch…",
            icon: IconName::Pencil,
            keywords: &["branch", "rename", "move"],
            command: PaletteCommand::RenameBranch,
        },
        Action {
            title: "Stash changes…",
            icon: IconName::Archive,
            keywords: &["stash", "push", "save", "wip"],
            command: PaletteCommand::StashChanges,
        },
        Action {
            title: "Pop latest stash",
            icon: IconName::Archive,
            keywords: &["stash", "pop", "apply", "unstash"],
            command: PaletteCommand::PopStash,
        },
        Action {
            title: "Fetch",
            icon: IconName::RefreshCw,
            keywords: &["fetch", "remote", "update"],
            command: PaletteCommand::Fetch,
        },
        Action {
            title: "Pull",
            icon: IconName::ArrowDown,
            keywords: &["pull", "fetch", "merge", "update"],
            command: PaletteCommand::Pull,
        },
        Action {
            title: "Push",
            icon: IconName::ArrowUp,
            keywords: &["push", "publish", "remote"],
            command: PaletteCommand::Push,
        },
        Action {
            title: "New local pull request",
            icon: IconName::GitPullRequestArrow,
            keywords: &["pr", "pull request", "review", "request-pull"],
            command: PaletteCommand::NewPullRequest,
        },
        Action {
            title: "Open repository…",
            icon: IconName::FolderOpen,
            keywords: &["open", "init", "clone"],
            command: PaletteCommand::OpenRepository,
        },
    ]
}

/// A git command run from the palette and what it wrote.
struct Console {
    command: String,
    lines: Vec<SharedString>,
    /// `None` while it runs.
    ok: Option<bool>,
    scroll: UniformListScrollHandle,
    _run: Task<()>,
}

pub struct Palette {
    repository: Repository,
    history: Entity<History>,
    input: Entity<InputState>,
    context: PaletteContext,
    tab: Tab,
    items: Vec<Item>,
    selected: usize,
    scroll: ScrollHandle,
    /// The last command run, shown until the query changes.
    console: Option<Console>,
    _input: Subscription,
}

impl EventEmitter<PaletteEvent> for Palette {}

impl Palette {
    pub fn new(
        repository: Repository,
        history: Entity<History>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Find a commit, a branch or an action, or type a git command")
        });
        let subscription =
            cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                InputEvent::Change => {
                    if this
                        .console
                        .as_ref()
                        .is_some_and(|console| console.ok.is_some())
                    {
                        this.console = None;
                    }
                    this.refresh(cx);
                }
                InputEvent::PressEnter { secondary, .. } => {
                    if *secondary {
                        this.run_typed(window, cx);
                    } else {
                        this.choose(this.selected, window, cx);
                    }
                }
                _ => {}
            });
        Self {
            repository,
            history,
            input,
            context: PaletteContext::default(),
            tab: Tab::All,
            items: Vec::new(),
            selected: 0,
            scroll: ScrollHandle::new(),
            console: None,
            _input: subscription,
        }
    }

    /// Opens with what the repository is like now, starting over unless a
    /// command is still running.
    pub fn open(&mut self, context: PaletteContext, window: &mut Window, cx: &mut Context<Self>) {
        self.context = context;
        if self
            .console
            .as_ref()
            .is_none_or(|console| console.ok.is_some())
        {
            self.console = None;
            self.tab = Tab::All;
            self.input
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.refresh(cx);
    }

    pub fn input_focus(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }

    fn query(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    /// Builds the results for the query again.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let query = self.query(cx);
        self.items = match git_command(&query) {
            Some(args) => self.git_items(&query, args, cx),
            None => self.search_items(query.trim(), cx),
        };
        self.items
            .retain(|item| self.tab == Tab::All || item.kind == self.tab);
        self.selected = self
            .items
            .iter()
            .position(|item| item.disabled.is_none())
            .unwrap_or(0);
        cx.notify();
    }

    /// Why action `command` cannot run now, if it cannot.
    fn unavailable(&self, command: &PaletteCommand) -> Option<SharedString> {
        let context = &self.context;
        let remote = context.remote.as_ref();
        let reason = match command {
            PaletteCommand::Fetch => remote.and_then(|remote| remote.fetch.clone().err()),
            PaletteCommand::Pull => remote.and_then(|remote| remote.pull.clone().err()),
            PaletteCommand::Push => remote.and_then(|remote| remote.push.clone().err()),
            _ => None,
        };
        if let Some(reason) = reason {
            return Some(lowercase_first(&reason).into());
        }
        let reason = match command {
            PaletteCommand::CommitChanges if context.changed.is_empty() => {
                Some("nothing to commit")
            }
            PaletteCommand::RewordLast | PaletteCommand::RenameBranch
                if context.branch.is_none() =>
            {
                Some("no branch checked out")
            }
            PaletteCommand::NewBranch if context.head.is_none() => Some("no commits yet"),
            PaletteCommand::StashChanges if context.changed.is_empty() => Some("nothing to stash"),
            PaletteCommand::PopStash if context.stashes == 0 => Some("no stashes"),
            PaletteCommand::ContinueRebase | PaletteCommand::AbortRebase if !context.rebasing => {
                Some("no rebase in progress")
            }
            PaletteCommand::ResolveConflicts if !context.merging => Some("no merge in progress"),
            PaletteCommand::CherryPick if context.picked == 0 => {
                Some("⌘-click commits in the history first")
            }
            _ => None,
        };
        reason.map(Into::into)
    }

    fn action_item(&self, action: Action, section: &str) -> Item {
        let disabled = self.unavailable(&action.command);
        let remote = self.context.remote.as_ref();
        let detail = match &action.command {
            PaletteCommand::Fetch => remote.and_then(|remote| remote.fetch.clone().ok()),
            PaletteCommand::Pull => remote.and_then(|remote| {
                remote.pull.as_ref().ok().map(|pull| match remote.behind {
                    0 => pull.clone(),
                    behind => format!("↓{behind} · {pull}"),
                })
            }),
            PaletteCommand::Push => remote.and_then(|remote| {
                remote.push.as_ref().ok().map(|plan| match remote.ahead {
                    0 => plan.describe(),
                    ahead => format!("↑{ahead} · {}", plan.describe()),
                })
            }),
            PaletteCommand::RewordLast => self.context.head.map(short),
            PaletteCommand::RenameBranch => self.context.branch.clone(),
            PaletteCommand::CommitChanges if !self.context.changed.is_empty() => Some(count(
                self.context.changed.len(),
                "changed file",
                "changed files",
            )),
            PaletteCommand::CherryPick if self.context.picked > 0 => Some(count(
                self.context.picked,
                "commit selected",
                "commits selected",
            )),
            _ => None,
        };
        Item {
            section: section.to_owned().into(),
            kind: Tab::Actions,
            icon: action.icon,
            primary: false,
            title: action.title.into(),
            sha: None,
            subtitle: None,
            detail: detail.map(Into::into),
            disabled,
            keys: &[],
            run: Run::Command(action.command),
            fill: None,
        }
    }

    /// The results for a search: actions, commits, branches and changed
    /// files whose names have every word of `query`.
    fn search_items(&self, query: &str, cx: &App) -> Vec<Item> {
        let words: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let matches = |text: &str| {
            let text = text.to_lowercase();
            words.iter().all(|word| text.contains(word.as_str()))
        };
        let limit = self.limit();
        let mut items: Vec<Item> = actions()
            .into_iter()
            .filter(|action| matches(&format!("{} {}", action.title, action.keywords.join(" "))))
            .map(|action| self.action_item(action, "Actions"))
            .collect();
        items.extend(self.commit_items(query, limit, cx));
        items.extend(self.branch_items(query, limit));
        items.extend(self.file_items(query, limit));
        items
    }

    /// The results for `git <args>`: what GitEZ makes of it, running it as
    /// typed, actions related to it and what its last word could be.
    fn git_items(&self, query: &str, args: Result<Vec<String>, String>, cx: &App) -> Vec<Item> {
        let mut items = Vec::new();
        let args = match args {
            Ok(args) => args,
            Err(problem) => {
                items.push(typed_item(Some(problem)));
                return items;
            }
        };
        let Some(subcommand) = args.first() else {
            items.push(typed_item(Some("type a git command".to_owned())));
            return items;
        };
        items.extend(self.recognise(&args));
        items.push(typed_item(None));
        items.extend(
            actions()
                .into_iter()
                .filter(|action| action.keywords.contains(&subcommand.as_str()))
                .map(|action| self.action_item(action, "Related actions")),
        );
        // What the argument being typed could be; none while the
        // subcommand is.
        let word = if query.ends_with(char::is_whitespace) {
            Some("")
        } else if args.len() > 1 {
            args.last().map(String::as_str)
        } else {
            None
        };
        if let Some(word) = word.filter(|word| !word.starts_with('-')) {
            // Branches first: the likeliest argument.
            let limit = self.limit();
            items.extend(self.branch_items(word, limit));
            if !word.is_empty() {
                items.extend(self.commit_items(word, limit, cx));
                items.extend(self.file_items(word, limit));
            }
        }
        items
    }

    /// A git command GitEZ has a screen for.
    fn recognise(&self, args: &[String]) -> Option<Item> {
        let (subcommand, rest) = args.split_first()?;
        let operands: Vec<&String> = rest.iter().filter(|arg| !arg.starts_with('-')).collect();
        let has = |flags: &[&str]| rest.iter().any(|arg| flags.contains(&arg.as_str()));
        let item = |icon, title: String, subtitle: String, command| Item {
            section: format!("Recognised as git {subcommand}").into(),
            kind: Tab::Actions,
            icon,
            primary: true,
            title: title.into(),
            sha: None,
            subtitle: Some(subtitle.into()),
            detail: None,
            disabled: self
                .unavailable(&PaletteCommand::RewordLast)
                .filter(|_| matches!(command, PaletteCommand::RewriteSince { .. })),
            keys: &[],
            run: Run::Command(command),
            fill: None,
        };
        match subcommand.as_str() {
            "rebase" if has(&["-i", "--interactive"]) && operands.len() == 1 => {
                let base = operands[0].clone();
                let autosquash = has(&["--autosquash"]);
                Some(item(
                    IconName::ListTree,
                    "Open in the history editor".to_owned(),
                    format!(
                        "The commits since {base}{}",
                        if autosquash { ", fixups folded in" } else { "" }
                    ),
                    PaletteCommand::RewriteSince { base, autosquash },
                ))
            }
            "switch" | "checkout"
                if operands.len() == 1
                    && rest.len() == 1
                    && self.context.branches.contains(operands[0]) =>
            {
                let branch = operands[0].clone();
                let checked_out = self.context.branch.as_ref() == Some(&branch);
                Some(Item {
                    disabled: checked_out.then(|| "checked out".into()),
                    ..item(
                        IconName::GitBranch,
                        format!("Check out {branch}"),
                        "Uncommitted changes come along, unless they would be overwritten"
                            .to_owned(),
                        PaletteCommand::Switch(branch),
                    )
                })
            }
            _ => None,
        }
    }

    fn limit(&self) -> usize {
        if self.tab == Tab::All {
            SHOWN_PER_KIND
        } else {
            SHOWN_OF_ONE_KIND
        }
    }

    /// Commits whose message, id or ref names contain `query`, newest
    /// first.
    fn commit_items(&self, query: &str, limit: usize, cx: &App) -> Vec<Item> {
        let history = self.history.read(cx);
        let query = query.to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        let mut items = Vec::new();
        for commit in history.commits() {
            if items.len() == limit {
                break;
            }
            let labels = history.labels_of(commit.id);
            let id = commit.id.to_string();
            let summary = commit.summary.to_lowercase();
            let label = labels.iter().find(|label| {
                words
                    .iter()
                    .all(|word| label.name.to_lowercase().contains(word))
            });
            let found = words.is_empty()
                || label.is_some()
                || (words.len() == 1 && id.starts_with(words[0]))
                || words.iter().all(|word| summary.contains(word));
            if !found {
                continue;
            }
            let shown_label = label.or_else(|| labels.first());
            items.push(Item {
                section: "Commits".into(),
                kind: Tab::Commits,
                icon: IconName::GitCommitVertical,
                primary: false,
                title: commit.summary.clone().into(),
                sha: Some(short(commit.id).into()),
                subtitle: None,
                detail: shown_label.map(|label| {
                    let kind = match label.kind {
                        RefKind::Tag => "tag",
                        RefKind::RemoteBranch => "remote",
                        _ => "branch",
                    };
                    format!("{kind} {}", label.name).into()
                }),
                disabled: None,
                keys: &[],
                run: Run::Command(PaletteCommand::SelectCommit(commit.id)),
                fill: Some(match label {
                    Some(label) => label.name.clone(),
                    None => short(commit.id),
                }),
            });
        }
        items
    }

    fn branch_items(&self, query: &str, limit: usize) -> Vec<Item> {
        let query = query.to_lowercase();
        self.context
            .branches
            .iter()
            .filter(|branch| branch.to_lowercase().contains(&query))
            .take(limit)
            .map(|branch| {
                let checked_out = self.context.branch.as_ref() == Some(branch);
                Item {
                    section: "Branches".into(),
                    kind: Tab::Branches,
                    icon: IconName::GitBranch,
                    primary: false,
                    title: branch.clone().into(),
                    sha: None,
                    subtitle: None,
                    detail: (!checked_out).then(|| "check out".into()),
                    disabled: checked_out.then(|| "checked out".into()),
                    keys: &[],
                    run: Run::Command(PaletteCommand::Switch(branch.clone())),
                    fill: Some(branch.clone()),
                }
            })
            .collect()
    }

    fn file_items(&self, query: &str, limit: usize) -> Vec<Item> {
        let query = query.to_lowercase();
        self.context
            .changed
            .iter()
            .filter(|path| path.to_lowercase().contains(&query))
            .take(limit)
            .map(|path| Item {
                section: "Changed files".into(),
                kind: Tab::Files,
                icon: IconName::File,
                primary: false,
                title: path.clone().into(),
                sha: None,
                subtitle: None,
                detail: Some("uncommitted".into()),
                disabled: None,
                keys: &[],
                run: Run::Command(PaletteCommand::OpenFile(path.clone())),
                fill: Some(path.clone()),
            })
            .collect()
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.items.len() && index != self.selected {
            self.selected = index;
            self.scroll_to_selected();
            cx.notify();
        }
    }

    /// Scrolls the results to show the selected item. Each section's
    /// heading is a child of the list too.
    fn scroll_to_selected(&self) {
        let headings = self.items[..=self.selected.min(self.items.len().saturating_sub(1))]
            .iter()
            .enumerate()
            .filter(|(index, item)| *index == 0 || self.items[index - 1].section != item.section)
            .count();
        self.scroll.scroll_to_item(self.selected + headings);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected.checked_sub(1) {
            self.select(index, cx);
        }
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected + 1, cx);
    }

    /// Puts what the selected item stands for in place of the word being
    /// typed, for the next argument.
    fn fill_argument(&mut self, _: &FillArgument, window: &mut Window, cx: &mut Context<Self>) {
        let Some(fill) = self
            .items
            .get(self.selected)
            .and_then(|item| item.fill.clone())
        else {
            return;
        };
        let query = self.query(cx);
        // Everything up to the word being typed stays.
        let kept = query
            .rfind(char::is_whitespace)
            .map_or("", |end| &query[..=end]);
        let text = format!("{kept}{} ", quote(&fill));
        self.input
            .update(cx, |input, cx| input.set_value(text, window, cx));
        // Setting the text is not a change the input reports.
        self.refresh(cx);
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PaletteEvent::Dismiss);
    }

    fn choose(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(index) else {
            return;
        };
        if item.disabled.is_some() {
            return;
        }
        match item.run.clone() {
            Run::Typed => self.run_typed(window, cx),
            Run::Command(command) => cx.emit(PaletteEvent::Command(command)),
        }
    }

    /// Runs the git command typed, showing its output as it comes.
    fn run_typed(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self
            .console
            .as_ref()
            .is_some_and(|console| console.ok.is_none())
        {
            return;
        }
        let query = self.query(cx);
        let Some(Ok(args)) = git_command(&query) else {
            return;
        };
        if args.is_empty() {
            return;
        }
        let Some(dir) = self.repository.work_dir() else {
            return;
        };
        let git = Git::new(dir);
        let command = format!(
            "git {}",
            args.iter()
                .map(|arg| quote(arg))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let (sender, mut lines) = futures::channel::mpsc::unbounded::<String>();
        let run = cx.background_executor().spawn(async move {
            git.run_as_typed(&args, |line| drop(sender.unbounded_send(line)))
        });
        let task = cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(line) = lines.next().await {
                let mut batch = vec![line];
                while let Ok(line) = lines.try_recv() {
                    batch.push(line);
                }
                let updated = this.update(cx, |this, cx| {
                    if let Some(console) = &mut this.console {
                        console
                            .lines
                            .extend(batch.into_iter().map(SharedString::from));
                        let excess = console.lines.len().saturating_sub(MAX_OUTPUT_LINES);
                        console.lines.drain(..excess);
                        let last = console.lines.len().saturating_sub(1);
                        console.scroll.scroll_to_item(last, ScrollStrategy::Bottom);
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    return;
                }
            }
            let ok = match run.await {
                Ok(ok) => ok,
                Err(error) => {
                    this.update(cx, |this, _| {
                        if let Some(console) = &mut this.console {
                            console.lines.push(error.to_string().into());
                        }
                    })
                    .ok();
                    false
                }
            };
            this.update(cx, |this, cx| {
                if let Some(console) = &mut this.console {
                    console.ok = Some(ok);
                    if console.lines.is_empty() && ok {
                        console.lines.push("Done; git printed nothing.".into());
                    }
                    let command = console.command.clone();
                    cx.emit(PaletteEvent::Finished { command, ok });
                }
                cx.notify();
            })
            .ok();
        });
        self.console = Some(Console {
            command,
            lines: Vec::new(),
            ok: None,
            scroll: UniformListScrollHandle::new(),
            _run: task,
        });
        cx.notify();
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .h_flex()
            .flex_wrap()
            .gap(px(6.))
            .px(px(16.))
            .pt(px(10.))
            .pb(px(4.))
            .children(TABS.iter().map(|&tab| {
                let on = tab == self.tab;
                div()
                    .id(SharedString::from(format!("palette-tab-{}", tab.label())))
                    .h(px(26.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .rounded(px(13.))
                    .border_1()
                    .text_size(px(12.))
                    .cursor_pointer()
                    .map(|this| {
                        if on {
                            this.bg(rgb(color::TEXT))
                                .border_color(rgb(color::TEXT))
                                .text_color(rgb(color::BACKGROUND))
                                .font_weight(FontWeight::MEDIUM)
                        } else {
                            this.border_color(rgb(color::BUTTON_BORDER))
                                .text_color(rgb(color::TEXT_MUTED))
                                .hover(|style| style.text_color(rgb(color::TEXT)))
                        }
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab = tab;
                        this.refresh(cx);
                    }))
                    .child(tab.label())
            }))
    }

    fn render_items(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut children: Vec<AnyElement> = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            if index == 0 || self.items[index - 1].section != item.section {
                children.push(
                    div()
                        .px(px(12.))
                        .pt(px(10.))
                        .pb(px(4.))
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(color::TEXT_FAINT))
                        .child(item.section.to_uppercase())
                        .into_any_element(),
                );
            }
            children.push(self.render_item(index, item, cx).into_any_element());
        }
        if self.items.is_empty() {
            children.push(
                div()
                    .px(px(12.))
                    .py(px(16.))
                    .text_color(rgb(color::TEXT_MUTED))
                    .child("Nothing matches. Start with git to run it as a command.")
                    .into_any_element(),
            );
        }
        div()
            .id("palette-items")
            .max_h(px(440.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px(px(6.))
            .pt(px(4.))
            .pb(px(8.))
            .children(children)
    }

    fn render_item(&self, index: usize, item: &Item, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = index == self.selected;
        let disabled = item.disabled.is_some();
        let keys: &[&str] = if selected && !disabled {
            &["↵"]
        } else {
            item.keys
        };
        let end = item.disabled.clone().or_else(|| item.detail.clone());
        div()
            .id(("palette-item", index))
            .h_flex()
            .gap(px(12.))
            .min_h(px(44.))
            .px(px(12.))
            .py(px(6.))
            .rounded(px(8.))
            .map(|this| {
                if disabled {
                    this.text_color(rgb(color::TEXT_FAINT))
                } else if selected {
                    this.bg(rgb(color::SELECTED)).cursor_pointer()
                } else {
                    this.cursor_pointer()
                        .hover(|style| style.bg(rgb(color::PALETTE_HOVER)))
                }
            })
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if this.selected != index {
                    this.selected = index;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| this.choose(index, window, cx)))
            .child(
                div()
                    .flex_none()
                    .size(px(28.))
                    .rounded(px(7.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(if item.primary {
                        color::REWORD_ON
                    } else if disabled {
                        color::PALETTE_ICON_DISABLED
                    } else {
                        color::PALETTE_HOVER
                    }))
                    .text_color(rgb(if item.primary {
                        color::CHIP_TEXT
                    } else {
                        color::TEXT_SECONDARY
                    }))
                    .child(Icon::new(item.icon).size(px(15.))),
            )
            .when_some(item.sha.clone(), |this, sha| {
                this.child(
                    div()
                        .flex_none()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(px(12.))
                        .text_color(rgb(color::CHIP_TEXT))
                        .child(sha),
                )
            })
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .when(item.primary, |this| this.font_weight(FontWeight::MEDIUM))
                            .child(item.title.clone()),
                    )
                    .when_some(item.subtitle.clone(), |this, subtitle| {
                        this.child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(color::TEXT_MUTED))
                                .child(subtitle),
                        )
                    }),
            )
            .when_some(end.filter(|_| keys.is_empty() || disabled), |this, end| {
                this.child(
                    div()
                        .flex_none()
                        .text_size(px(12.))
                        .when(!disabled, |this| this.text_color(rgb(color::TEXT_MUTED)))
                        .child(end),
                )
            })
            .when(!keys.is_empty() && !disabled, |this| {
                this.child(
                    div()
                        .h_flex()
                        .flex_none()
                        .gap(px(3.))
                        .children(keys.iter().map(|key| labels::key_cap(key, cx))),
                )
            })
    }

    fn render_console(&self, console: &Console, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let lines = console.lines.clone();
        let (status, status_color) = match console.ok {
            None => ("running…", color::TEXT_MUTED),
            Some(true) => ("done", color::DIFF_ADDED),
            Some(false) => ("failed", color::DIFF_REMOVED),
        };
        div()
            .v_flex()
            .gap(px(8.))
            .px(px(16.))
            .py(px(12.))
            .child(
                div()
                    .h_flex()
                    .gap(px(10.))
                    .text_size(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_family(mono.clone())
                            .text_color(rgb(color::TEXT_SECONDARY))
                            .child(format!("$ {}", console.command)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(rgb(status_color))
                            .child(status),
                    ),
            )
            .child(
                div()
                    .h(px(320.))
                    .rounded(px(8.))
                    .bg(rgb(color::PROMPT))
                    .px(px(12.))
                    .py(px(8.))
                    .child(
                        uniform_list("palette-output", lines.len(), move |range, _, _| {
                            range
                                .map(|index| {
                                    div()
                                        .h(px(18.))
                                        .whitespace_nowrap()
                                        .font_family(mono.clone())
                                        .text_size(px(12.))
                                        .text_color(rgb(color::DIFF_TEXT))
                                        .child(lines[index].clone())
                                })
                                .collect()
                        })
                        .size_full()
                        .track_scroll(&console.scroll),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(color::TEXT_MUTED))
                    .child(if console.ok.is_none() {
                        "Esc hides the palette; the command keeps running."
                    } else {
                        "Type to search again, or ⌘↵ to run the command once more."
                    }),
            )
    }
}

impl Focusable for Palette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for Palette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let body = match &self.console {
            Some(console) => self.render_console(console, cx).into_any_element(),
            None => div()
                .child(self.render_tabs(cx))
                .child(self.render_items(cx))
                .into_any_element(),
        };
        let footer_key = |key: &'static str| labels::key_cap(key, cx);
        div()
            .id("palette")
            .key_context("Palette")
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::fill_argument))
            .on_action(cx.listener(Self::dismiss))
            .occlude()
            .w(px(680.))
            .v_flex()
            .rounded(px(14.))
            .border_1()
            .border_color(rgb(color::PALETTE_BORDER))
            .bg(rgb(color::PALETTE))
            .shadow_2xl()
            .overflow_hidden()
            .child(
                div()
                    .h_flex()
                    .gap(px(10.))
                    .px(px(16.))
                    .py(px(10.))
                    .border_b_1()
                    .border_color(rgb(color::SEPARATOR))
                    .child(
                        div()
                            .font_family(mono.clone())
                            .text_size(px(14.))
                            .text_color(rgb(color::ACCENT))
                            .child("$"),
                    )
                    .child(
                        Input::new(&self.input)
                            .appearance(false)
                            .flex_1()
                            .font_family(mono)
                            .text_size(px(15.)),
                    )
                    .child(labels::key_cap("esc", cx)),
            )
            .child(body)
            .child(
                div()
                    .h_flex()
                    .flex_wrap()
                    .gap(px(14.))
                    .px(px(16.))
                    .py(px(10.))
                    .border_t_1()
                    .border_color(rgb(color::SEPARATOR))
                    .text_size(px(11.5))
                    .text_color(rgb(color::TEXT_MUTED))
                    .child(
                        div()
                            .h_flex()
                            .gap(px(5.))
                            .child(footer_key("↑"))
                            .child(footer_key("↓"))
                            .child("move"),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap(px(5.))
                            .child(footer_key("tab"))
                            .child("fill argument"),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap(px(5.))
                            .child(footer_key(SECONDARY_KEY))
                            .child(footer_key("↵"))
                            .child("run as typed"),
                    )
                    .child(
                        div()
                            .h_flex()
                            .ml_auto()
                            .gap(px(4.))
                            .child("Start with")
                            .child(
                                div()
                                    .font_family(theme::MONO_FONT)
                                    .text_color(rgb(color::TEXT_SECONDARY))
                                    .child("git"),
                            )
                            .child("to run any command"),
                    ),
            )
    }
}

/// "Run exactly as typed", or why the command cannot run.
fn typed_item(problem: Option<String>) -> Item {
    Item {
        section: "Git command".into(),
        kind: Tab::Actions,
        icon: IconName::Terminal,
        primary: false,
        title: "Run exactly as typed".into(),
        sha: None,
        subtitle: Some("Output streams here; the graph refreshes when it finishes".into()),
        detail: None,
        disabled: problem.map(Into::into),
        keys: &[SECONDARY_KEY, "↵"],
        run: Run::Typed,
        fill: None,
    }
}

/// The arguments of `query` if it is a git command (`git` and what
/// follows), split as a shell would; an error says what is wrong with them.
fn git_command(query: &str) -> Option<Result<Vec<String>, String>> {
    let query = query.trim_start();
    let rest = query.strip_prefix("git")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(split_args(rest))
}

/// Splits `text` into arguments as `sh` would: on whitespace, with single
/// and double quotes and backslashes. Nothing is expanded.
fn split_args(text: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current: Option<String> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if let Some(arg) = current.take() {
                    args.push(arg);
                }
            }
            '\'' => {
                let arg = current.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => arg.push(c),
                        None => return Err("unclosed quote".to_owned()),
                    }
                }
            }
            '"' => {
                let arg = current.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => arg.push(c),
                            Some(c) => {
                                arg.push('\\');
                                arg.push(c);
                            }
                            None => return Err("unclosed quote".to_owned()),
                        },
                        Some(c) => arg.push(c),
                        None => return Err("unclosed quote".to_owned()),
                    }
                }
            }
            '\\' => {
                let arg = current.get_or_insert_with(String::new);
                if let Some(c) = chars.next() {
                    arg.push(c);
                }
            }
            c => current.get_or_insert_with(String::new).push(c),
        }
    }
    if let Some(arg) = current {
        args.push(arg);
    }
    Ok(args)
}

/// `arg` quoted for the command line when it needs to be.
fn quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_alphanumeric() || "-_./:@^~=+,%".contains(c));
    if plain {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

fn short(id: Oid) -> String {
    id.to_hex_with_len(7).to_string()
}

fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// `text` with its first letter in lower case, to follow other words.
fn lowercase_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{git_command, quote, split_args};

    #[test]
    fn splits_arguments_as_a_shell_does() {
        assert_eq!(
            split_args(r#" commit -m "Fix \"it\"" --author='A B' a\ b "#),
            Ok(vec![
                "commit".to_owned(),
                "-m".to_owned(),
                r#"Fix "it""#.to_owned(),
                "--author=A B".to_owned(),
                "a b".to_owned(),
            ])
        );
        assert_eq!(split_args("log 'oops"), Err("unclosed quote".to_owned()));
        assert_eq!(
            split_args("commit -m ''"),
            Ok(vec!["commit".to_owned(), "-m".to_owned(), String::new()])
        );
    }

    #[test]
    fn recognises_git_commands() {
        assert_eq!(git_command("git"), Some(Ok(vec![])));
        assert_eq!(
            git_command("  git status"),
            Some(Ok(vec!["status".to_owned()]))
        );
        assert_eq!(git_command("gitignore"), None);
        assert_eq!(git_command("fix typo"), None);
    }

    #[test]
    fn quotes_only_what_needs_it() {
        assert_eq!(quote("feature/graph"), "feature/graph");
        assert_eq!(quote("HEAD~2"), "HEAD~2");
        assert_eq!(quote("two words"), "'two words'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}
