//! Rewriting the recent history of the branch checked out, as in the
//! `Rewrite.dc.html` artboard: each commit, newest first, with what to do
//! with it (pick, reword, edit, squash, fixup or drop), dragged into a new
//! order, and a summary of what the rewrite will do.

use std::collections::HashMap;
use std::sync::Arc;

use gitez_command::{Git, ResetMode, RewriteAction, RewriteOutcome, RewriteStep};
use gitez_git::{Oid, RefKind, RefLabel, Repository, Rewritable, RewriteBase, RewriteCommit};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::labels;
use crate::repository::Snapshot;
use crate::theme::{self, color};
use crate::undo::{self, Recorder};

/// Subjects up to this long read well in one-line logs.
const SUBJECT_LENGTH: usize = 50;

/// What the screen asks of the workspace.
pub enum RewriteEvent {
    Close,
    /// The branch was rewritten; `step` is the undo step.
    Done {
        message: SharedString,
        step: Option<u64>,
    },
    /// The rewrite stopped at an edit; the text says where.
    Stopped(SharedString),
}

/// What to do with one commit, as in `git rebase -i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Pick,
    Reword,
    Edit,
    Squash,
    Fixup,
    Drop,
}

const ACTIONS: [Action; 6] = [
    Action::Pick,
    Action::Reword,
    Action::Edit,
    Action::Squash,
    Action::Fixup,
    Action::Drop,
];

impl Action {
    fn label(self) -> &'static str {
        match self {
            Action::Pick => "Pick",
            Action::Reword => "Reword",
            Action::Edit => "Edit",
            Action::Squash => "Squash",
            Action::Fixup => "Fixup",
            Action::Drop => "Drop",
        }
    }

    /// Whether the commit is folded into the one before it.
    fn folds(self) -> bool {
        matches!(self, Action::Squash | Action::Fixup)
    }
}

/// The message boxes of a reworded commit.
struct Message {
    subject: Entity<InputState>,
    body: Entity<TextareaState>,
}

/// One commit and what to do with it.
struct Entry {
    commit: RewriteCommit,
    action: Action,
    /// Made the first time the commit is reworded, and kept when another
    /// action is chosen, so the new message is not lost.
    message: Option<Message>,
}

impl Entry {
    /// The message the commit ends up with.
    fn message(&self, cx: &App) -> String {
        match (&self.message, self.action) {
            (Some(message), Action::Reword) => {
                let subject = message.subject.read(cx).value().trim().to_owned();
                let body = message.body.read(cx).value().trim().to_owned();
                if body.is_empty() {
                    subject
                } else {
                    format!("{subject}\n\n{body}")
                }
            }
            _ => self.commit.message.clone(),
        }
    }

    /// Whether the commit gets a new message.
    fn reworded(&self, cx: &App) -> bool {
        self.action == Action::Reword && self.message(cx).trim() != self.commit.message.trim()
    }

    /// Whether the commit changes, its place aside.
    fn changes(&self, cx: &App) -> bool {
        match self.action {
            Action::Pick => false,
            Action::Reword => self.reworded(cx),
            _ => true,
        }
    }
}

/// A commit being dragged to a new place.
#[derive(Clone)]
struct DraggedCommit {
    index: usize,
    label: SharedString,
}

impl Render for DraggedCommit {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.))
            .py(px(6.))
            .rounded(px(7.))
            .border_1()
            .border_color(rgb(color::PICK_TARGET_BORDER))
            .bg(rgb(color::REWORD_CARD))
            .text_color(rgb(color::TEXT))
            .text_size(px(12.5))
            .child(self.label.clone())
    }
}

/// Reading the commits, then editing what to do with them.
enum Load {
    Reading,
    Failed(SharedString),
    Ready,
}

pub struct Rewrite {
    focus_handle: FocusHandle,
    repository: Repository,
    snapshot: Arc<Snapshot>,
    branch: String,
    /// The branch's upstream, such as `origin/main`, if it has one.
    upstream: Option<String>,
    load: Load,
    /// The commit before the oldest one, which stays as it is; `None` when
    /// the oldest is a root commit.
    base: Option<RewriteBase>,
    /// The commits, newest first, in the order they are now.
    entries: Vec<Entry>,
    /// The commits' ids, newest first, in the order they were.
    original: Vec<Oid>,
    /// Files with uncommitted changes.
    uncommitted: usize,
    running: bool,
    /// Why the last attempt failed: git's own words.
    error: Option<SharedString>,
    _load: Task<()>,
    _run: Option<Task<()>>,
    _messages: Vec<Subscription>,
}

impl EventEmitter<RewriteEvent> for Rewrite {}

impl Rewrite {
    /// Rewrites the commits of `branch`, checked out on `tip`, from `from`
    /// up; commit `reword`, if given, starts out reworded, and with
    /// `autosquash` the fixups start out folded in.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        snapshot: Arc<Snapshot>,
        branch: String,
        tip: Oid,
        from: Oid,
        reword: Option<Oid>,
        autosquash: bool,
        uncommitted: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let upstream = snapshot
            .refs
            .branches
            .iter()
            .find(|candidate| candidate.name == branch)
            .and_then(|branch| branch.upstream.clone());
        let upstream_tip = upstream.as_ref().and_then(|upstream| {
            snapshot
                .refs
                .remote_branches
                .iter()
                .find(|remote| &remote.short_name() == upstream)
                .map(|remote| remote.target)
        });
        let repository = snapshot.repository.clone();
        let read = cx.background_executor().spawn({
            let repository = repository.clone();
            async move { repository.rewritable(tip, from, upstream_tip) }
        });
        let load = cx.spawn_in(window, async move |this, cx| {
            let result = read.await;
            this.update_in(cx, |this, window, cx| {
                this.finish_load(result, reword, autosquash, window, cx)
            })
            .ok();
        });
        Self {
            focus_handle: cx.focus_handle(),
            repository,
            snapshot,
            branch,
            upstream,
            load: Load::Reading,
            base: None,
            entries: Vec::new(),
            original: Vec::new(),
            uncommitted,
            running: false,
            error: None,
            _load: load,
            _run: None,
            _messages: Vec::new(),
        }
    }

    fn finish_load(
        &mut self,
        result: gitez_git::Result<Rewritable>,
        reword: Option<Oid>,
        autosquash: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(Rewritable::Commits { commits, base }) => {
                self.base = base;
                self.original = commits.iter().map(|commit| commit.id).collect();
                self.entries = commits
                    .into_iter()
                    .map(|commit| Entry {
                        commit,
                        action: Action::Pick,
                        message: None,
                    })
                    .collect();
                self.load = Load::Ready;
                if let Some(index) = reword.and_then(|id| self.index_of(id)) {
                    self.set_action(index, Action::Reword, window, cx);
                }
                if autosquash {
                    self.autosquash(cx);
                }
            }
            Ok(Rewritable::Elsewhere) => {
                self.load = Load::Failed(
                    format!(
                        "That commit is not in the history of {}, the branch checked out. \
                         Check out its branch to rewrite it.",
                        self.branch
                    )
                    .into(),
                );
            }
            Ok(Rewritable::Merge { id, summary }) => {
                self.load = Load::Failed(
                    format!(
                        "Merge commit {} “{summary}” is between that commit and the tip of {}. \
                         GitEZ rewrites only a straight line of commits; choose a commit after \
                         the merge.",
                        short(id),
                        self.branch
                    )
                    .into(),
                );
            }
            Err(error) => {
                self.load = Load::Failed(
                    format!(
                        "Could not read the commits: {:#}",
                        anyhow::Error::from(error)
                    )
                    .into(),
                );
            }
        }
        cx.notify();
    }

    fn index_of(&self, id: Oid) -> Option<usize> {
        self.entries.iter().position(|entry| entry.commit.id == id)
    }

    fn set_action(
        &mut self,
        index: usize,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        entry.action = action;
        if action == Action::Reword && entry.message.is_none() {
            let message = &entry.commit.message;
            let (subject, body) = match message.split_once('\n') {
                Some((subject, body)) => (subject, body.trim()),
                None => (message.as_str(), ""),
            };
            let subject = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(subject.to_owned())
                    .placeholder("Subject")
            });
            let body = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(3, 12)
                    .default_value(body.to_owned())
                    .placeholder("Why the change was made")
            });
            self._messages
                .push(cx.subscribe(&subject, notify_on_change));
            self._messages.push(cx.subscribe(&body, notify_on_change));
            window.focus(&subject.focus_handle(cx), cx);
            entry.message = Some(Message { subject, body });
        }
        self.error = None;
        cx.notify();
    }

    /// Moves the commit at `from` to `to`, both newest-first places.
    fn move_entry(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from == to || from >= self.entries.len() || to >= self.entries.len() {
            return;
        }
        let entry = self.entries.remove(from);
        self.entries.insert(to, entry);
        self.error = None;
        cx.notify();
    }

    /// Every commit back to pick, in its old place.
    fn reset(&mut self, cx: &mut Context<Self>) {
        let mut entries: HashMap<Oid, Entry> = self
            .entries
            .drain(..)
            .map(|entry| (entry.commit.id, entry))
            .collect();
        self.entries = self
            .original
            .iter()
            .filter_map(|id| entries.remove(id))
            .map(|entry| Entry {
                action: Action::Pick,
                message: None,
                ..entry
            })
            .collect();
        self._messages.clear();
        self.error = None;
        cx.notify();
    }

    /// The commits made by `git commit --fixup` or `--squash`, whose
    /// subject names an older commit here.
    fn autosquash_candidates(&self) -> Vec<(usize, usize, Action)> {
        let mut candidates = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            let summary = &entry.commit.summary;
            let (target, action) = if let Some(target) = summary.strip_prefix("fixup! ") {
                (target, Action::Fixup)
            } else if let Some(target) = summary.strip_prefix("amend! ") {
                (target, Action::Fixup)
            } else if let Some(target) = summary.strip_prefix("squash! ") {
                (target, Action::Squash)
            } else {
                continue;
            };
            // The nearest older commit with that subject.
            if let Some(offset) = self.entries[index + 1..]
                .iter()
                .position(|older| older.commit.summary == target)
            {
                candidates.push((index, index + 1 + offset, action));
            }
        }
        candidates
    }

    /// Moves each fixup and squash commit just above the commit it names
    /// and folds it in, as `git rebase --autosquash` does.
    fn autosquash(&mut self, cx: &mut Context<Self>) {
        let candidates = self.autosquash_candidates();
        if candidates.is_empty() {
            return;
        }
        let targets: HashMap<Oid, (Oid, Action)> = candidates
            .iter()
            .map(|&(index, target, action)| {
                let id = |index: usize| self.entries[index].commit.id;
                (id(index), (id(target), action))
            })
            .collect();
        // Oldest first, which is the order git applies them in.
        let mut entries: Vec<Entry> = self.entries.drain(..).rev().collect();
        let mut folded = Vec::new();
        let mut index = 0;
        while index < entries.len() {
            if targets.contains_key(&entries[index].commit.id) {
                folded.push(entries.remove(index));
            } else {
                index += 1;
            }
        }
        for mut entry in folded {
            let (target, action) = targets[&entry.commit.id];
            let Some(mut place) = entries.iter().position(|entry| entry.commit.id == target) else {
                continue;
            };
            // After the target and what is already folded into it.
            while entries
                .get(place + 1)
                .is_some_and(|next| targets.get(&next.commit.id).map(|(t, _)| *t) == Some(target))
            {
                place += 1;
            }
            entry.action = action;
            entries.insert(place + 1, entry);
        }
        entries.reverse();
        self.entries = entries;
        self.error = None;
        cx.notify();
    }

    /// For each commit, newest first, the commit it is folded into: the
    /// next older one kept, or what that one is folded into.
    fn fold_targets(&self) -> Vec<Option<usize>> {
        let mut targets = vec![None; self.entries.len()];
        let mut group = None;
        for (index, entry) in self.entries.iter().enumerate().rev() {
            match entry.action {
                Action::Drop => {}
                action if action.folds() => targets[index] = group,
                _ => group = Some(index),
            }
        }
        targets
    }

    /// Whether the commit at `index` has an older commit kept to fold into.
    fn can_fold(&self, index: usize) -> bool {
        self.entries[index + 1..]
            .iter()
            .any(|entry| entry.action != Action::Drop)
    }

    /// Why the rewrite cannot run as it stands, if it cannot.
    fn problem(&self, cx: &App) -> Option<String> {
        if self
            .entries
            .iter()
            .all(|entry| entry.action == Action::Drop)
        {
            return Some(
                "Every commit is dropped. Keep at least one; to remove them all, reset the \
                 branch instead."
                    .to_owned(),
            );
        }
        let targets = self.fold_targets();
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.action.folds() && targets[index].is_none() {
                return Some(format!(
                    "{} has no older commit to fold into. Move it up, or pick it.",
                    short(entry.commit.id)
                ));
            }
            if entry.action == Action::Reword
                && entry
                    .message
                    .as_ref()
                    .is_some_and(|message| message.subject.read(cx).value().trim().is_empty())
            {
                return Some(format!("{} needs a subject.", short(entry.commit.id)));
            }
        }
        None
    }

    /// How many commits git rewrites: every commit from the oldest that
    /// changes or moves, up to the tip.
    fn rewritten(&self, cx: &App) -> usize {
        let count = self.entries.len();
        let first_change = self
            .entries
            .iter()
            .rev()
            .zip(self.original.iter().rev())
            .position(|(entry, original)| entry.commit.id != *original || entry.changes(cx));
        first_change.map_or(0, |oldest| count - oldest)
    }

    /// The commits rewritten that the upstream already has.
    fn pushed_rewritten(&self, cx: &App) -> Vec<Oid> {
        let rewritten = self.rewritten(cx);
        let pushed: Vec<Oid> = self
            .entries
            .iter()
            .filter(|entry| entry.commit.pushed)
            .map(|entry| entry.commit.id)
            .collect();
        self.original[..rewritten]
            .iter()
            .filter(|id| pushed.contains(id))
            .copied()
            .collect()
    }

    fn backup(&self) -> String {
        format!("refs/gitez/backup/{}", self.branch)
    }

    fn run(&mut self, cx: &mut Context<Self>) {
        if self.running || self.problem(cx).is_some() || self.rewritten(cx) == 0 {
            return;
        }
        let Some(dir) = self.repository.work_dir() else {
            self.error = Some("A bare repository has no working tree to rewrite in.".into());
            cx.notify();
            return;
        };
        let git = Git::new(dir);
        let steps: Vec<RewriteStep> = self
            .entries
            .iter()
            .rev()
            .map(|entry| RewriteStep {
                commit: entry.commit.id.to_string(),
                action: match entry.action {
                    Action::Pick => RewriteAction::Pick,
                    Action::Reword if entry.reworded(cx) => {
                        RewriteAction::Reword(format!("{}\n", entry.message(cx)))
                    }
                    Action::Reword => RewriteAction::Pick,
                    Action::Edit => RewriteAction::Edit,
                    Action::Squash => RewriteAction::Squash,
                    Action::Fixup => RewriteAction::Fixup,
                    Action::Drop => RewriteAction::Drop,
                },
            })
            .collect();
        let base = self.base.as_ref().map(|base| base.id.to_string());
        let branch = self.branch.clone();
        let backup = self.backup();
        let (before, after) = (self.entries.len(), self.kept());
        let edit = self
            .entries
            .iter()
            .rev()
            .find(|entry| entry.action == Action::Edit)
            .map(|entry| short(entry.commit.id));
        self.running = true;
        self.error = None;
        self._run = Some(cx.spawn(async move |this, cx| {
            let rewritten = cx
                .background_executor()
                .spawn({
                    let (branch, backup) = (branch.clone(), backup.clone());
                    async move {
                        let recorder = Recorder::start(git.work_dir());
                        git.rewrite(&branch, base.as_deref(), &steps, &backup)
                            .map(|outcome| (outcome, recorder.finish(ResetMode::Keep)))
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.running = false;
                match rewritten {
                    Ok((RewriteOutcome::Done, recorded)) => {
                        let message = if before == after {
                            format!("Rewrote {branch}")
                        } else {
                            format!("Rewrote {branch}: {before} → {after} commits")
                        };
                        let step = undo::record(cx, recorded, format!("Rewrite {branch}"));
                        cx.emit(RewriteEvent::Done {
                            message: message.into(),
                            step,
                        });
                    }
                    Ok((RewriteOutcome::Stopped, _)) => cx.emit(RewriteEvent::Stopped(
                        format!(
                            "Stopped at {} so you can change it. Amend the commit, then \
                             continue.",
                            edit.unwrap_or_else(|| "a commit".to_owned())
                        )
                        .into(),
                    )),
                    Err(error) => this.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// How many commits the branch has afterwards, of those shown.
    fn kept(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| !matches!(entry.action, Action::Drop) && !entry.action.folds())
            .count()
    }

    /// Tags and branches on the base commit, to name it by.
    fn base_labels(&self) -> Vec<RefLabel> {
        let Some(base) = &self.base else {
            return Vec::new();
        };
        let refs = &self.snapshot.refs;
        let tags = refs
            .tags
            .iter()
            .filter(|tag| tag.target == base.id)
            .map(|tag| RefLabel {
                kind: RefKind::Tag,
                name: tag.name.clone(),
                is_head: false,
            });
        let branches = refs
            .branches
            .iter()
            .filter(|branch| branch.target == base.id)
            .map(|branch| RefLabel {
                kind: RefKind::Branch,
                name: branch.name.clone(),
                is_head: false,
            });
        tags.chain(branches).take(2).collect()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.entries.len();
        let base_labels = self.base_labels();
        let can_autosquash = !self.autosquash_candidates().is_empty();
        let ready = matches!(self.load, Load::Ready);
        div()
            .h_flex()
            .flex_none()
            .flex_wrap()
            .gap(px(12.))
            .px(px(24.))
            .py(px(12.))
            .border_b_1()
            .border_color(rgb(color::BORDER))
            .bg(rgb(color::PANEL))
            .child(
                Button::new("rewrite-back")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::ArrowLeft).size(px(14.)))
                    .label("History")
                    .text_color(rgb(color::AUTHOR_TEXT))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(RewriteEvent::Close))),
            )
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Rewrite history"),
            )
            .child(labels::ref_label(&RefLabel {
                kind: RefKind::Branch,
                name: self.branch.clone(),
                is_head: false,
            }))
            .when(ready, |this| {
                this.child(
                    div()
                        .text_size(px(12.5))
                        .text_color(rgb(color::TEXT_MUTED))
                        .child(match (&self.base, count) {
                            (None, _) => "every commit, from the first".to_owned(),
                            (Some(_), 1) => "1 commit since".to_owned(),
                            (Some(_), count) => format!("{count} commits since"),
                        }),
                )
                .when_some(self.base.as_ref(), |this, base| {
                    if base_labels.is_empty() {
                        this.child(mono(short(base.id)).text_color(rgb(color::CHIP_TEXT)))
                    } else {
                        this.children(base_labels.iter().map(labels::ref_label))
                    }
                })
            })
            .child(
                div()
                    .h_flex()
                    .ml_auto()
                    .gap(px(6.))
                    .child(
                        Button::new("rewrite-autosquash")
                            .ghost()
                            .small()
                            .label("Autosquash fixups")
                            .text_color(rgb(color::AUTHOR_TEXT))
                            .tooltip(
                                "Fold each “fixup!” and “squash!” commit into the commit it names",
                            )
                            .disabled(!can_autosquash || self.running)
                            .on_click(cx.listener(|this, _, _, cx| this.autosquash(cx))),
                    )
                    .child(
                        Button::new("rewrite-reset")
                            .ghost()
                            .small()
                            .label("Reset all to pick")
                            .text_color(rgb(color::AUTHOR_TEXT))
                            .disabled(!ready || self.running)
                            .on_click(cx.listener(|this, _, _, cx| this.reset(cx))),
                    ),
            )
    }

    fn render_entry(
        &self,
        index: usize,
        fold_target: Option<usize>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entry = &self.entries[index];
        let action = entry.action;
        let can_fold = self.can_fold(index);
        let note = fold_target
            .map(|target| format!("folds into {}", short(self.entries[target].commit.id)));
        let summary = match (&entry.message, action) {
            (Some(message), Action::Reword) => message.subject.read(cx).value().to_string(),
            _ => entry.commit.summary.clone(),
        };
        let label: SharedString =
            format!("{} {}", short(entry.commit.id), entry.commit.summary).into();

        div()
            .id(("rewrite-commit", index))
            .v_flex()
            .rounded(px(9.))
            .border_1()
            .map(|this| match action {
                Action::Reword => this
                    .bg(rgb(color::REWORD_CARD))
                    .border_color(rgb(color::PICK_TARGET_BORDER)),
                _ => this
                    .bg(rgb(color::PANEL))
                    .border_color(rgb(color::PICK_COMMIT_BORDER)),
            })
            .when(action.folds(), |this| this.ml(px(36.)))
            .when(action == Action::Drop, |this| this.opacity(0.6))
            .drag_over::<DraggedCommit>(|style, _, _, _| style.border_color(rgb(color::ACCENT)))
            .on_drop(cx.listener(move |this, dragged: &DraggedCommit, _, cx| {
                this.move_entry(dragged.index, index, cx)
            }))
            .child(
                div()
                    .h_flex()
                    .flex_wrap()
                    .gap(px(10.))
                    .pl(px(6.))
                    .pr(px(10.))
                    .py(px(8.))
                    .child(
                        div()
                            .id(("rewrite-drag", index))
                            .flex_none()
                            .w(px(22.))
                            .h(px(30.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_grab()
                            .text_color(rgb(color::TEXT_FAINT))
                            .child(Icon::new(IconName::GripVertical).size(px(14.)))
                            .on_drag(DraggedCommit { index, label }, |dragged, _, _, cx| {
                                cx.new(|_| dragged.clone())
                            }),
                    )
                    .child(
                        mono(short(entry.commit.id))
                            .w(px(58.))
                            .text_color(rgb(color::FIXUP_TEXT)),
                    )
                    .child(
                        div()
                            .h_flex()
                            .flex_1()
                            .min_w(px(200.))
                            .gap(px(8.))
                            .child(
                                div()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .when(action == Action::Drop, |this| this.line_through())
                                    .child(summary),
                            )
                            .when_some(note, |this, note| {
                                this.child(pill(note, color::FIXUP_ON, color::FIXUP_TEXT))
                            })
                            .when(entry.commit.pushed, |this| {
                                this.child(pill(
                                    match &self.upstream {
                                        Some(upstream) => format!("on {}", remote_of(upstream)),
                                        None => "pushed".to_owned(),
                                    },
                                    color::PUSHED_CHIP,
                                    color::CHIP_TEXT,
                                ))
                            }),
                    )
                    .child(self.render_actions(index, action, can_fold, cx)),
            )
            .when_some(
                entry.message.as_ref().filter(|_| action == Action::Reword),
                |this, message| this.child(render_message(message, cx)),
            )
    }

    /// The row of buttons choosing what to do with the commit at `index`.
    fn render_actions(
        &self,
        index: usize,
        current: Action,
        can_fold: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let running = self.running;
        div()
            .h_flex()
            .flex_none()
            .rounded(px(7.))
            .border_1()
            .border_color(rgb(color::BUTTON_BORDER))
            .bg(rgb(color::BACKGROUND))
            .overflow_hidden()
            .children(ACTIONS.iter().enumerate().map(|(position, &action)| {
                let on = action == current;
                let enabled = !running && (can_fold || !action.folds());
                let (background, text) = match action {
                    Action::Reword => (color::REWORD_ON, color::CHIP_TEXT),
                    Action::Fixup => (color::FIXUP_ON, color::FIXUP_TEXT),
                    Action::Drop => (color::DROP_ON, color::DROP_TEXT),
                    _ => (color::ACTION_ON, color::TEXT),
                };
                div()
                    .id(SharedString::from(format!(
                        "rewrite-{index}-{}",
                        action.label()
                    )))
                    .h(px(28.))
                    .px(px(9.))
                    .flex()
                    .items_center()
                    .text_size(px(12.))
                    .when(position + 1 < ACTIONS.len(), |this| {
                        this.border_r_1().border_color(rgb(color::BORDER))
                    })
                    .map(|this| {
                        if on {
                            this.bg(rgb(background))
                                .text_color(rgb(text))
                                .font_weight(FontWeight::MEDIUM)
                        } else if enabled {
                            this.text_color(rgb(color::TEXT_MUTED))
                                .cursor_pointer()
                                .hover(|style| style.text_color(rgb(color::TEXT)))
                        } else {
                            this.text_color(rgb(color::TEXT_FAINT)).opacity(0.5)
                        }
                    })
                    .when(enabled && !on, |this| {
                        this.on_click(cx.listener(move |this, _, window, cx| {
                            this.set_action(index, action, window, cx)
                        }))
                    })
                    .child(action.label())
            }))
    }

    /// What the rewrite will do, the warnings, and the buttons.
    fn render_summary(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let targets = self.fold_targets();
        let mut lines: Vec<(String, u32, String)> = Vec::new();
        let reworded = self
            .entries
            .iter()
            .filter(|entry| entry.reworded(cx))
            .count();
        if reworded > 0 {
            lines.push((
                reworded.to_string(),
                color::CHIP_TEXT,
                if reworded == 1 {
                    "commit gets a new message".to_owned()
                } else {
                    "commits get new messages".to_owned()
                },
            ));
        }
        for (index, entry) in self.entries.iter().enumerate().rev() {
            let Some(target) = targets[index] else {
                continue;
            };
            let target = short(self.entries[target].commit.id);
            lines.push(match entry.action {
                Action::Squash => (
                    "1".to_owned(),
                    color::FIXUP_TEXT,
                    format!("squash folds into {target}, joining the messages"),
                ),
                _ => (
                    "1".to_owned(),
                    color::FIXUP_TEXT,
                    format!("fixup folds into {target}"),
                ),
            });
        }
        for entry in self.entries.iter().rev() {
            if entry.action == Action::Edit {
                lines.push((
                    "1".to_owned(),
                    color::TEXT,
                    format!("stops at {} so you can change it", short(entry.commit.id)),
                ));
            }
        }
        let dropped = self
            .entries
            .iter()
            .filter(|entry| entry.action == Action::Drop)
            .count();
        if dropped > 0 {
            lines.push((
                dropped.to_string(),
                color::DROP_TEXT,
                if dropped == 1 {
                    "commit is dropped".to_owned()
                } else {
                    "commits are dropped".to_owned()
                },
            ));
        }
        let moved = self
            .entries
            .iter()
            .zip(&self.original)
            .any(|(entry, original)| entry.commit.id != *original);
        if moved {
            lines.push((
                "↕".to_owned(),
                color::TEXT_MUTED,
                "commits change order".to_owned(),
            ));
        }
        let rewritten = self.rewritten(cx);
        let (before, after) = (self.entries.len(), self.kept());
        if rewritten > 0 {
            lines.push((
                format!("{before} → {after}"),
                color::TEXT_MUTED,
                "commits on the branch afterwards".to_owned(),
            ));
        }
        let pushed = self.pushed_rewritten(cx);
        let problem = self.problem(cx);
        let backup = self.backup();
        let backup = backup.trim_start_matches("refs/").to_owned();
        let action = match rewritten {
            0 => "Rewrite".to_owned(),
            1 => "Rewrite 1 commit".to_owned(),
            n => format!("Rewrite {n} commits"),
        };
        let can_run = !self.running && rewritten > 0 && problem.is_none();
        let uncommitted = self.uncommitted;

        div()
            .v_flex()
            .w(px(360.))
            .flex_none()
            .gap(px(14.))
            .child(
                div()
                    .v_flex()
                    .gap(px(12.))
                    .px(px(16.))
                    .py(px(14.))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(rgb(color::PICK_COMMIT_BORDER))
                    .bg(rgb(color::PANEL))
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("What will happen"),
                    )
                    .map(|this| {
                        if lines.is_empty() {
                            this.child(
                                div()
                                    .text_size(px(12.5))
                                    .text_color(rgb(color::TEXT_MUTED))
                                    .child(
                                        "Nothing yet. Choose what to do with each commit, or \
                                         drag commits into a new order.",
                                    ),
                            )
                        } else {
                            this.child(div().v_flex().gap(px(8.)).children(lines.into_iter().map(
                                |(count, count_color, text)| {
                                    div()
                                        .h_flex()
                                        .items_start()
                                        .gap(px(12.))
                                        .text_size(px(12.5))
                                        .child(
                                            mono(count).min_w(px(14.)).text_color(rgb(count_color)),
                                        )
                                        .child(div().flex_1().min_w_0().child(text))
                                },
                            )))
                        }
                    })
                    .when(!pushed.is_empty(), |this| {
                        let upstream = self.upstream.clone().unwrap_or_default();
                        let which = match pushed.as_slice() {
                            [one] => format!("{} is", short(*one)),
                            more => format!("{} of these commits are", more.len()),
                        };
                        this.child(notice(
                            IconName::TriangleAlert,
                            format!(
                                "{which} already on {upstream}. Pushing afterwards needs a \
                                 force-push; use --force-with-lease so nobody else’s work is \
                                 overwritten."
                            ),
                        ))
                    })
                    .when(rewritten > 0, |this| {
                        this.child(
                            div()
                                .h_flex()
                                .items_start()
                                .gap(px(10.))
                                .text_size(px(12.5))
                                .line_height(relative(1.5))
                                .text_color(rgb(color::AUTHOR_TEXT))
                                .child(
                                    Icon::new(IconName::ShieldCheck)
                                        .size(px(16.))
                                        .mt(px(2.))
                                        .text_color(rgb(color::DIFF_ADDED)),
                                )
                                .child(div().flex_1().min_w_0().child(format!(
                                    "The current tip is saved as {backup}. Undo, offered when \
                                     the rewrite is done, restores it in one step."
                                ))),
                        )
                    })
                    .when(rewritten > 0 && uncommitted > 0, |this| {
                        this.child(
                            div()
                                .text_size(px(12.5))
                                .text_color(rgb(color::TEXT_MUTED))
                                .child(if uncommitted == 1 {
                                    "Your uncommitted file is stashed and comes back afterwards."
                                        .to_owned()
                                } else {
                                    format!(
                                        "Your {uncommitted} uncommitted files are stashed and \
                                         come back afterwards."
                                    )
                                }),
                        )
                    }),
            )
            .when_some(problem, |this, problem| {
                this.child(notice(IconName::CircleAlert, problem))
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(theme::RADIUS)
                        .bg(rgb(color::THREAD))
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(theme::SMALL_TEXT_SIZE)
                        .line_height(relative(1.5))
                        .text_color(rgb(color::DIFF_REMOVED))
                        .child(error),
                )
            })
            .child(
                div()
                    .h_flex()
                    .gap(px(8.))
                    .child(
                        Button::new("rewrite-cancel")
                            .label("Cancel")
                            .flex_1()
                            .h(px(36.))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(RewriteEvent::Close))),
                    )
                    .child(
                        Button::new("rewrite-run")
                            .primary()
                            .label(action)
                            // Twice the width of Cancel.
                            .flex_grow(2.)
                            .flex_basis(px(0.))
                            .h(px(36.))
                            .loading(self.running)
                            .disabled(!can_run)
                            .on_click(cx.listener(|this, _, _, cx| this.run(cx))),
                    ),
            )
    }

    fn render_base(&self) -> impl IntoElement {
        div()
            .h_flex()
            .gap(px(10.))
            .px(px(12.))
            .py(px(10.))
            .text_size(px(12.))
            .text_color(rgb(color::TEXT_MUTED))
            .child(Icon::new(IconName::GitCommitVertical).size(px(14.)))
            .map(|this| match &self.base {
                Some(base) => this
                    .child("Base")
                    .child(mono(short(base.id)).text_color(rgb(color::CHIP_TEXT)))
                    .child(format!("{} — stays as it is", base.summary)),
                None => this.child("The first commit of the history; nothing comes before it."),
            })
    }
}

impl Focusable for Rewrite {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Rewrite {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .track_focus(&self.focus_handle)
            .v_flex()
            .size_full()
            .bg(rgb(color::BACKGROUND))
            .child(self.render_header(cx));
        match &self.load {
            Load::Reading => {
                return root.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(rgb(color::TEXT_MUTED))
                        .child("Reading commits…"),
                );
            }
            Load::Failed(error) => {
                return root.child(
                    div()
                        .flex_1()
                        .v_flex()
                        .items_center()
                        .justify_center()
                        .gap(px(6.))
                        .p(px(24.))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("This history can’t be rewritten here"),
                        )
                        .child(
                            div()
                                .max_w(px(480.))
                                .text_center()
                                .text_color(rgb(color::TEXT_MUTED))
                                .child(error.clone()),
                        ),
                );
            }
            Load::Ready => {}
        }
        let targets = self.fold_targets();
        let entries: Vec<AnyElement> = (0..self.entries.len())
            .map(|index| {
                self.render_entry(index, targets[index], cx)
                    .into_any_element()
            })
            .collect();

        root.child(
            div()
                .h_flex()
                .flex_1()
                .min_h_0()
                .items_start()
                .gap(px(24.))
                .px(px(24.))
                .pt(px(20.))
                .child(
                    div()
                        .id("rewrite-commits")
                        .v_flex()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .overflow_y_scroll()
                        .gap(px(8.))
                        .pb(px(32.))
                        .child(
                            div()
                                .px(px(4.))
                                .pb(px(4.))
                                .text_size(px(12.))
                                .text_color(rgb(color::TEXT_MUTED))
                                .child("Newest at the top. Drag to reorder."),
                        )
                        .children(entries)
                        .child(self.render_base()),
                )
                .child(self.render_summary(cx)),
        )
    }
}

/// Draws the screen again when a message box changes: the summary and
/// the subject's length follow it.
fn notify_on_change<T>(
    _: &mut Rewrite,
    _: Entity<T>,
    event: &InputEvent,
    cx: &mut Context<Rewrite>,
) {
    if let InputEvent::Change = event {
        cx.notify();
    }
}

/// The subject and body boxes of a reworded commit.
fn render_message(message: &Message, cx: &mut Context<Rewrite>) -> impl IntoElement {
    let length = message.subject.read(cx).value().trim().chars().count();
    div()
        .v_flex()
        .gap(px(10.))
        .pl(px(44.))
        .pr(px(16.))
        .pt(px(4.))
        .pb(px(16.))
        .child(
            div()
                .v_flex()
                .gap(px(4.))
                .child(
                    div()
                        .h_flex()
                        .text_size(px(11.5))
                        .child(div().text_color(rgb(color::TEXT_MUTED)).child("Subject"))
                        .child(
                            mono(format!("{length} / {SUBJECT_LENGTH}"))
                                .ml_auto()
                                .text_color(rgb(if length <= SUBJECT_LENGTH {
                                    color::DIFF_ADDED
                                } else {
                                    color::CHIP_TEXT
                                })),
                        ),
                )
                .child(Input::new(&message.subject).bg(rgb(color::BACKGROUND))),
        )
        .child(
            div()
                .v_flex()
                .gap(px(4.))
                .child(
                    div()
                        .h_flex()
                        .text_size(px(11.5))
                        .text_color(rgb(color::TEXT_MUTED))
                        .child("Body")
                        .child(div().ml_auto().child("wrap lines at 72")),
                )
                .child(
                    Textarea::new(&message.body)
                        .bg(rgb(color::BACKGROUND))
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(px(12.5)),
                ),
        )
        .child(
            div()
                .h_flex()
                .justify_end()
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_MUTED))
                .child("Author and date stay as they were"),
        )
}

/// An amber box with a warning.
fn notice(icon: IconName, text: String) -> impl IntoElement {
    div()
        .h_flex()
        .items_start()
        .gap(px(10.))
        .px(px(12.))
        .py(px(10.))
        .rounded(px(8.))
        .border_1()
        .border_color(rgb(color::WARNING_BORDER))
        .bg(rgb(color::TRIAL_CONFLICT))
        .text_size(px(12.5))
        .line_height(relative(1.5))
        .text_color(rgb(color::WARNING_TEXT))
        .child(
            Icon::new(icon)
                .size(px(16.))
                .mt(px(2.))
                .text_color(rgb(color::ACCENT)),
        )
        .child(div().flex_1().min_w_0().child(text))
}

/// A small rounded chip beside a commit's message.
fn pill(text: String, background: u32, foreground: u32) -> impl IntoElement {
    div()
        .flex_none()
        .h(px(20.))
        .px(px(7.))
        .flex()
        .items_center()
        .rounded(px(10.))
        .bg(rgb(background))
        .text_color(rgb(foreground))
        .text_size(px(11.))
        .child(text)
}

/// The remote of an upstream such as `origin/main`.
fn remote_of(upstream: &str) -> &str {
    upstream
        .split_once('/')
        .map_or(upstream, |(remote, _)| remote)
}

fn mono(text: impl ToString) -> Div {
    div()
        .flex_none()
        .font_family(theme::MONO_FONT)
        .text_size(px(11.5))
        .child(text.to_string())
}

fn short(id: Oid) -> String {
    id.to_hex_with_len(7).to_string()
}
