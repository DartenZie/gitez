//! Cherry-picking the commits picked in the history onto a branch, as in
//! the panel of the `CherryPick.dc.html` artboard: the commits in the order
//! they apply, the branches to put them on with a trial run against each,
//! and git's options.

use std::collections::HashMap;

use gitez_command::{Git, PickOptions};
use gitez_git::{Commit, Head, Oid, PickTrial, Repository};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::repository::Snapshot;
use crate::theme::{self, color};

/// What the panel asks of the workspace.
pub enum CherryPickEvent {
    Close,
    /// The commits were applied; the text says how.
    Done(SharedString),
}

/// A branch the commits can go onto.
struct Target {
    name: String,
    tip: Oid,
}

pub struct CherryPick {
    repository: Repository,
    /// The commits to apply, oldest first.
    commits: Vec<Commit>,
    targets: Vec<Target>,
    /// The branch checked out, to return to if git stops.
    checked_out: Option<String>,
    /// How the commits would apply to each branch, by name, once tried.
    trials: HashMap<String, Result<PickTrial, SharedString>>,
    /// The branch chosen.
    target: Option<String>,
    filter: Entity<InputState>,
    note_source: bool,
    no_commit: bool,
    stash: bool,
    /// Files with uncommitted changes.
    uncommitted: usize,
    running: bool,
    /// Why the last attempt failed: git's own words.
    error: Option<SharedString>,
    _trials: Task<()>,
    _run: Option<Task<()>>,
    _filter: Subscription,
}

impl EventEmitter<CherryPickEvent> for CherryPick {}

impl CherryPick {
    pub fn new(
        snapshot: &Snapshot,
        commits: Vec<Commit>,
        uncommitted: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Find a branch…"));
        let filter_changes = cx.subscribe(&filter, |_, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                cx.notify();
            }
        });
        let mut this = Self {
            repository: snapshot.repository.clone(),
            commits: Vec::new(),
            targets: snapshot
                .refs
                .branches
                .iter()
                .map(|branch| Target {
                    name: branch.name.clone(),
                    tip: branch.target,
                })
                .collect(),
            checked_out: match &snapshot.refs.head {
                Some(Head::Branch { name, .. }) => Some(name.clone()),
                _ => None,
            },
            trials: HashMap::new(),
            target: None,
            filter,
            note_source: true,
            no_commit: false,
            stash: true,
            uncommitted,
            running: false,
            error: None,
            _trials: Task::ready(()),
            _run: None,
            _filter: filter_changes,
        };
        this.set_commits(commits, cx);
        this
    }

    /// Applies `commits`, oldest first, instead, trying them against every
    /// branch again.
    pub fn set_commits(&mut self, commits: Vec<Commit>, cx: &mut Context<Self>) {
        self.commits = commits;
        self.trials.clear();
        self.error = None;
        let ids: Vec<Oid> = self.commits.iter().map(|commit| commit.id).collect();
        let targets: Vec<(String, Oid)> = self
            .targets
            .iter()
            .map(|target| (target.name.clone(), target.tip))
            .collect();
        let repository = self.repository.clone();
        self._trials = cx.spawn(async move |this, cx| {
            let trials = cx
                .background_executor()
                .spawn(async move {
                    targets
                        .into_iter()
                        .map(|(name, tip)| {
                            let trial = repository.cherry_pick_trial(tip, &ids).map_err(|error| {
                                format!("{:#}", anyhow::Error::from(error)).into()
                            });
                            (name, trial)
                        })
                        .collect::<HashMap<_, _>>()
                })
                .await;
            this.update(cx, |this, cx| {
                this.trials = trials;
                if this.target.is_none() {
                    this.target = this.default_target();
                }
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    /// The branch checked out if the commits apply to it, or else the
    /// first branch they apply to.
    fn default_target(&self) -> Option<String> {
        let applies = |name: &String| {
            matches!(
                self.trials.get(name),
                Some(Ok(PickTrial::Clean { applied, .. })) if !applied.is_empty()
            )
        };
        self.checked_out
            .iter()
            .chain(self.targets.iter().map(|target| &target.name))
            .find(|name| applies(name))
            .cloned()
    }

    /// The merge commits picked: git needs to be told which parent to
    /// pick them against, which GitEZ does not ask.
    fn merges(&self) -> Vec<&Commit> {
        self.commits
            .iter()
            .filter(|commit| commit.parents.len() > 1)
            .collect()
    }

    /// The commits to hand to git, if the chosen branch takes them.
    fn ready(&self) -> Option<(&str, &[Oid])> {
        if self.running || !self.merges().is_empty() {
            return None;
        }
        let target = self.target.as_deref()?;
        match self.trials.get(target)? {
            Ok(PickTrial::Clean { applied, .. }) if !applied.is_empty() => Some((target, applied)),
            _ => None,
        }
    }

    fn run(&mut self, cx: &mut Context<Self>) {
        let Some((target, applied)) = self.ready() else {
            return;
        };
        let Some(dir) = self.repository.work_dir() else {
            self.error = Some("A bare repository has no working tree to cherry-pick in.".into());
            cx.notify();
            return;
        };
        let git = Git::new(dir);
        let target = target.to_owned();
        let ids: Vec<String> = applied.iter().map(Oid::to_string).collect();
        let options = PickOptions {
            note_source: self.note_source,
            no_commit: self.no_commit,
            stash: self.stash && self.uncommitted > 0,
        };
        let return_to = self.checked_out.clone();
        let done = match (ids.len(), self.no_commit) {
            (1, false) => format!("Cherry-picked 1 commit onto {target}"),
            (n, false) => format!("Cherry-picked {n} commits onto {target}"),
            (_, true) => format!("Staged the changes on {target}; commit them when ready"),
        };
        self.running = true;
        self.error = None;
        self._run = Some(cx.spawn(async move |this, cx| {
            let picked = cx
                .background_executor()
                .spawn(async move { git.cherry_pick(&ids, &target, options, return_to.as_deref()) })
                .await;
            this.update(cx, |this, cx| {
                this.running = false;
                match picked {
                    Ok(()) => cx.emit(CherryPickEvent::Done(done.into())),
                    Err(error) => this.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_commits(&self) -> impl IntoElement {
        div()
            .v_flex()
            .gap(px(6.))
            .children(self.commits.iter().enumerate().map(|(index, commit)| {
                div()
                    .h_flex()
                    .gap(px(10.))
                    .px(px(10.))
                    .py(px(8.))
                    .rounded(px(7.))
                    .bg(rgb(color::PICK_COMMIT))
                    .border_1()
                    .border_color(rgb(color::PICK_COMMIT_BORDER))
                    .child(mono(index + 1).text_color(rgb(color::TEXT_MUTED)))
                    .child(mono(short(commit.id)).text_color(rgb(color::BRANCH_BLUE)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(commit.summary.clone()),
                    )
            }))
    }

    fn render_target(&self, target: &Target, cx: &mut Context<Self>) -> impl IntoElement {
        let chosen = self.target.as_deref() == Some(target.name.as_str());
        let (status, status_color) = match self.trials.get(&target.name) {
            None => ("checking…".to_owned(), color::TEXT_MUTED),
            Some(Err(_)) => ("could not try".to_owned(), color::TEXT_MUTED),
            Some(Ok(PickTrial::Clean { applied, .. })) if applied.is_empty() => {
                ("already has them".to_owned(), color::TEXT_MUTED)
            }
            Some(Ok(PickTrial::Clean { skipped: 0, .. })) => {
                ("applies cleanly".to_owned(), color::TEXT_MUTED)
            }
            Some(Ok(PickTrial::Clean { skipped, .. })) => (
                format!("applies cleanly · {skipped} already there"),
                color::TEXT_MUTED,
            ),
            Some(Ok(PickTrial::Conflict { files, .. })) => (
                count(files.len(), "conflict", "conflicts"),
                color::CHIP_TEXT,
            ),
        };
        let name = target.name.clone();
        div()
            .id(SharedString::from(format!("onto-{name}")))
            .h_flex()
            .gap(px(10.))
            .h(px(36.))
            .px(px(10.))
            .rounded(px(7.))
            .border_1()
            .cursor_pointer()
            .map(|this| {
                if chosen {
                    this.bg(rgb(color::SELECTED))
                        .border_color(rgb(color::PICK_TARGET_BORDER))
                } else {
                    this.border_color(transparent_black())
                        .hover(|style| style.bg(rgb(color::PICK_TARGET_HOVER)))
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.target = Some(name.clone());
                this.error = None;
                cx.notify();
            }))
            .child(
                div()
                    .flex_none()
                    .size(px(8.))
                    .rounded_full()
                    .bg(rgb(theme::branch_color(&target.name))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(target.name.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.5))
                    .text_color(rgb(status_color))
                    .child(status),
            )
            .when(
                chosen
                    && matches!(
                        self.trials.get(&target.name),
                        Some(Ok(PickTrial::Clean { applied, .. })) if !applied.is_empty()
                    ),
                |this| {
                    this.child(
                        Icon::new(IconName::Check)
                            .size(px(14.))
                            .text_color(rgb(color::DIFF_ADDED)),
                    )
                },
            )
    }

    fn render_option(
        &self,
        id: &'static str,
        checked: bool,
        title: &'static str,
        detail: String,
        change: fn(&mut Self, bool),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .h_flex()
            .items_start()
            .gap(px(8.))
            .child(
                Checkbox::new(id)
                    .checked(checked)
                    .mt(px(2.))
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        change(this, *checked);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(title)
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(detail),
                    ),
            )
    }

    /// What the trial run against the chosen branch found.
    fn render_trial(&self) -> Option<Div> {
        let target = self.target.as_deref()?;
        let merges = self.merges();
        let (text, clean) = if let Some(merge) = merges.first() {
            (
                format!(
                    "{} “{}” is a merge commit, which GitEZ cannot cherry-pick; \
                     leave it out with ⌘-click.",
                    short(merge.id),
                    merge.summary
                ),
                false,
            )
        } else {
            match self.trials.get(target)? {
                Err(error) => (format!("The trial run failed: {error}"), false),
                Ok(PickTrial::Clean { applied, .. }) if applied.is_empty() => (
                    format!("{target} already has these changes; there is nothing to pick."),
                    false,
                ),
                Ok(PickTrial::Clean { applied, skipped }) => {
                    let n = applied.len();
                    let what = match n {
                        1 => "the commit applies".to_owned(),
                        2 => "both commits apply".to_owned(),
                        n => format!("all {n} commits apply"),
                    };
                    let mut text =
                        format!("Trial run in memory: {what} to {target} without conflicts.");
                    if *skipped > 0 {
                        text.push_str(&format!(
                            " {} already on {target} and left out.",
                            count(*skipped, "commit is", "commits are")
                        ));
                    }
                    let tip = self
                        .targets
                        .iter()
                        .find(|candidate| candidate.name == target)
                        .map(|candidate| short(candidate.tip));
                    if self.no_commit {
                        text.push_str(&format!(" The changes will be staged on {target}."));
                    } else if let Some(tip) = tip {
                        text.push_str(&format!(
                            " {target} will move from {tip} to {}.",
                            count(n, "new commit", "new commits")
                        ));
                    }
                    (text, true)
                }
                Ok(PickTrial::Conflict { commit, files }) => {
                    let summary = self
                        .commits
                        .iter()
                        .find(|candidate| candidate.id == *commit)
                        .map(|candidate| candidate.summary.as_str())
                        .unwrap_or_default();
                    (
                        format!(
                            "Trial run in memory: {} “{summary}” conflicts with {target} in \
                             {}. GitEZ gives a cherry-pick up when it conflicts, so choose \
                             another branch, or merge instead.",
                            short(*commit),
                            files.join(", ")
                        ),
                        false,
                    )
                }
            }
        };
        Some(
            div()
                .h_flex()
                .items_start()
                .gap(px(10.))
                .px(px(12.))
                .py(px(10.))
                .rounded(px(8.))
                .border_1()
                .text_size(px(12.5))
                .line_height(relative(1.5))
                .map(|this| {
                    if clean {
                        this.bg(rgb(color::TRIAL_CLEAN))
                            .border_color(rgb(color::TRIAL_CLEAN_BORDER))
                            .text_color(rgb(color::TRIAL_CLEAN_TEXT))
                    } else {
                        this.bg(rgb(color::TRIAL_CONFLICT))
                            .border_color(rgb(color::PICK_TARGET_BORDER))
                            .text_color(rgb(color::CHIP_TEXT))
                    }
                })
                .child(
                    Icon::new(if clean {
                        IconName::Check
                    } else {
                        IconName::CircleAlert
                    })
                    .size(px(16.))
                    .mt(px(2.))
                    .text_color(rgb(if clean {
                        color::DIFF_ADDED
                    } else {
                        color::CHIP_TEXT
                    })),
                )
                .child(div().flex_1().min_w_0().child(text)),
        )
    }
}

impl Render for CherryPick {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let filter = self.filter.read(cx).value().trim().to_lowercase();
        let targets: Vec<AnyElement> = self
            .targets
            .iter()
            .filter(|target| filter.is_empty() || target.name.to_lowercase().contains(&filter))
            .map(|target| self.render_target(target, cx).into_any_element())
            .collect();
        let title = match self.commits.len() {
            1 => "Cherry-pick 1 commit".to_owned(),
            n => format!("Cherry-pick {n} commits"),
        };
        let action = match &self.target {
            Some(target) => format!("Cherry-pick onto {target}"),
            None => "Cherry-pick".to_owned(),
        };
        let uncommitted = self.uncommitted;

        div()
            .v_flex()
            .size_full()
            .bg(rgb(color::PANEL))
            .child(
                div()
                    .v_flex()
                    .flex_none()
                    .gap(px(4.))
                    .px(px(18.))
                    .pt(px(16.))
                    .pb(px(12.))
                    .border_b_1()
                    .border_color(rgb(color::BORDER))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child("Applied oldest first, as new commits on the target."),
                    ),
            )
            .child(
                div()
                    .id("cherry-pick-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .v_flex()
                    .gap(px(18.))
                    .px(px(18.))
                    .py(px(14.))
                    .child(self.render_commits())
                    .child(
                        div()
                            .v_flex()
                            .gap(px(6.))
                            .child(section("Onto"))
                            .child(Input::new(&self.filter).small())
                            .children(targets),
                    )
                    .child(
                        div()
                            .v_flex()
                            .gap(px(10.))
                            .child(section("Options"))
                            .child(self.render_option(
                                "pick-note-source",
                                self.note_source,
                                "Note the source commit",
                                "Adds “cherry picked from …” to each message (-x)".into(),
                                |this, checked| this.note_source = checked,
                                cx,
                            ))
                            .child(self.render_option(
                                "pick-no-commit",
                                self.no_commit,
                                "Stage only, don’t commit",
                                "Leaves the changes in the index (-n)".into(),
                                |this, checked| this.no_commit = checked,
                                cx,
                            ))
                            .when(uncommitted > 0, |this| {
                                this.child(self.render_option(
                                    "pick-stash",
                                    self.stash,
                                    "Stash my working changes first",
                                    format!(
                                        "You have {}; they come back afterwards",
                                        count(uncommitted, "uncommitted file", "uncommitted files")
                                    ),
                                    |this, checked| this.stash = checked,
                                    cx,
                                ))
                            }),
                    )
                    .children(self.render_trial())
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
                    }),
            )
            .child(
                div()
                    .h_flex()
                    .flex_none()
                    .gap(px(8.))
                    .px(px(18.))
                    .py(px(14.))
                    .border_t_1()
                    .border_color(rgb(color::BORDER))
                    .child(
                        Button::new("cherry-pick-cancel")
                            .label("Cancel")
                            .flex_1()
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(CherryPickEvent::Close))),
                    )
                    .child(
                        Button::new("cherry-pick-run")
                            .primary()
                            .label(action)
                            // Twice the width of Cancel.
                            .flex_grow(2.)
                            .flex_basis(px(0.))
                            .h(px(36.))
                            .loading(self.running)
                            .disabled(self.ready().is_none())
                            .on_click(cx.listener(|this, _, _, cx| this.run(cx))),
                    ),
            )
    }
}

/// A heading over a group of the panel.
fn section(title: &str) -> impl IntoElement {
    div()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(color::TEXT_FAINT))
        .child(title.to_uppercase())
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

fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}
