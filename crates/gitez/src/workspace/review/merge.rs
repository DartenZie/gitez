//! Merging one branch into another, as in the dialog of the `Merge.dc.html`
//! artboard: how the branches compare, whether they would conflict, and
//! the ways to bring the branch in. It opens for a local pull request, or
//! when a branch is dropped on another in the history.

use gitez_command::{Git, MergeMethod, MergeOutcome, ResetMode, WorktreeProblem};
use gitez_git::{Head, MergePreview, Oid, RefKind, RefLabel, Repository};
use gitez_review::{PullRequest, State, Store, Timestamp};
use gpui_kit::assets::IconName;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Review, ReviewEvent, Subject, follow_head, muted};
use crate::theme::{self, color};
use crate::undo::{self, Recorded, Recorder, ReviewChange, ReviewTips};
use crate::workspace::labels;

/// What the dialog asks of its owner.
pub(crate) enum MergeEvent {
    /// The branch was merged, and the pull request merged with it, if any,
    /// saved as merged. `step` is the undo step.
    Merged {
        pull_request: Option<Box<PullRequest>>,
        step: Option<u64>,
    },
    /// The branch was rebased onto the other; `step` is the undo step.
    Rebased { step: Option<u64> },
    /// The merge stopped with conflicts to resolve.
    Stopped,
    /// Review the branch as a local pull request: this open one, or a new
    /// one.
    Review(Option<u32>),
}

/// How a merge or rebase ended, before it is recorded for undo.
enum Done {
    Merged(Option<Box<PullRequest>>),
    Rebased,
    Stopped,
}

/// The branches as the dialog merges them.
pub(crate) struct Merging {
    /// The open local pull request of these branches, which a merge marks
    /// merged.
    pub pull_request: Option<MergedPullRequest>,
    pub head: String,
    pub base: String,
    pub head_tip: Oid,
    pub base_tip: Oid,
}

pub(crate) struct MergedPullRequest {
    pub number: u32,
    pub title: String,
    pub description: String,
}

/// One of the dialog's options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Merge(MergeMethod),
    /// Rebase the branch onto the other, which stays where it is.
    Rebase,
    /// Review it as a local pull request first.
    Review,
}

pub(crate) struct MergeDialog {
    repository: Repository,
    store: Store,
    merging: Merging,
    /// Opened by dropping a branch on another, rather than for a pull
    /// request: the branch can be rebased alone, or reviewed first.
    dropped: bool,
    /// `None` while working it out.
    preview: Option<Result<MergePreview, SharedString>>,
    /// The option being carried out now.
    running: Option<Choice>,
    /// Why the last attempt failed: git's own words.
    error: Option<SharedString>,
    _task: Task<()>,
}

impl EventEmitter<MergeEvent> for MergeDialog {}

impl MergeDialog {
    pub(crate) fn new(
        repository: Repository,
        store: Store,
        merging: Merging,
        dropped: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let (base, head) = (merging.base_tip, merging.head_tip);
        let read = repository.clone();
        let task = cx.spawn(async move |this, cx| {
            let preview = cx
                .background_executor()
                .spawn(async move { read.merge_preview(base, head) })
                .await
                .map_err(|error| {
                    format!(
                        "Could not compare the branches: {:#}",
                        anyhow::Error::from(error)
                    )
                    .into()
                });
            this.update(cx, |this, cx| {
                this.preview = Some(preview);
                cx.notify();
            })
            .ok();
        });
        Self {
            repository,
            store,
            merging,
            dropped,
            preview: None,
            running: None,
            error: None,
            _task: task,
        }
    }

    /// Carries out `choice` in the background; a merge saves the pull
    /// request, if any, as merged.
    fn choose(&mut self, choice: Choice, cx: &mut Context<Self>) {
        if self.running.is_some() || self.why_not(choice).is_some() {
            return;
        }
        let how = match choice {
            Choice::Review => {
                let number = self.merging.pull_request.as_ref().map(|pr| pr.number);
                cx.emit(MergeEvent::Review(number));
                return;
            }
            Choice::Merge(how) => Some(how),
            Choice::Rebase => None,
        };
        let Some(dir) = self.repository.work_dir() else {
            return;
        };
        let git = Git::new(dir);
        let (repository, store) = (self.repository.clone(), self.store.clone());
        let merging = &self.merging;
        let number = merging.pull_request.as_ref().map(|pr| pr.number);
        let (head, base) = (merging.head.clone(), merging.base.clone());
        let message = how.map(|how| message(merging, how, self.ahead()));
        let base_before = merging.base_tip;
        let label = match how {
            Some(_) => match number {
                Some(number) => format!("Merge #{number} into {base}"),
                None => format!("Merge {head} into {base}"),
            },
            None => format!("Rebase {head} onto {base}"),
        };
        self.running = Some(choice);
        self.error = None;
        self._task = cx.spawn(async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn(async move {
                    carry_out(
                        &repository,
                        &store,
                        &git,
                        Carry {
                            how,
                            message,
                            head,
                            base,
                            number,
                            base_before,
                        },
                    )
                })
                .await;
            this.update(cx, |this, cx| {
                this.running = None;
                match done {
                    Ok((done, recorded)) => {
                        let step = undo::record(cx, recorded, label);
                        cx.emit(match done {
                            Done::Merged(pull_request) => MergeEvent::Merged { pull_request, step },
                            Done::Rebased => MergeEvent::Rebased { step },
                            Done::Stopped => MergeEvent::Stopped,
                        })
                    }
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    fn conflicts(&self) -> bool {
        matches!(&self.preview, Some(Ok(preview)) if !preview.conflicts.is_empty())
    }

    fn ahead(&self) -> usize {
        match &self.preview {
            Some(Ok(preview)) => preview.ahead,
            _ => 0,
        }
    }

    /// Why `choice` is not possible, if it is not.
    fn why_not(&self, choice: Choice) -> Option<String> {
        let preview = match &self.preview {
            None => return Some("Comparing the branches…".into()),
            Some(Err(_)) => return Some("The branches could not be compared.".into()),
            Some(Ok(preview)) => preview,
        };
        let (head, base) = (&self.merging.head, &self.merging.base);
        if preview.ahead == 0 {
            return Some(format!("{base} already has every commit of {head}"));
        }
        if choice == Choice::Review {
            return None;
        }
        if self.repository.work_dir().is_none() {
            return Some("A bare repository has no working tree to merge in".into());
        }
        match choice {
            Choice::Merge(MergeMethod::FastForward) if preview.behind > 0 => {
                Some("Not possible — the branches have diverged".into())
            }
            Choice::Merge(MergeMethod::FastForward) => None,
            Choice::Rebase if preview.behind == 0 => {
                Some(format!("{head} is already on top of {base}"))
            }
            // The conflict editor opens to resolve them.
            Choice::Merge(MergeMethod::Merge) => None,
            _ if !preview.conflicts.is_empty() => Some(format!(
                "Would conflict — merge into {base} instead and resolve {} there",
                if preview.conflicts.len() == 1 {
                    "it"
                } else {
                    "them"
                }
            )),
            _ => None,
        }
    }

    fn render_option(
        &self,
        choice: Choice,
        icon: IconName,
        title: String,
        detail: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let why_not = self.why_not(choice);
        let enabled = why_not.is_none() && self.running.is_none();
        let primary = choice == Choice::Merge(MergeMethod::Merge) && enabled;
        div()
            .id(match choice {
                Choice::Merge(MergeMethod::Merge) => "merge-commit",
                Choice::Merge(MergeMethod::Squash) => "merge-squash",
                Choice::Merge(MergeMethod::Rebase) => "merge-rebase",
                Choice::Merge(MergeMethod::FastForward) => "merge-fast-forward",
                Choice::Rebase => "rebase",
                Choice::Review => "review",
            })
            .h_flex()
            .items_start()
            .gap(px(12.))
            .px(px(12.))
            .py(px(10.))
            .rounded(px(8.))
            .when(primary, |this| this.bg(rgb(color::SELECTED)))
            .when(enabled, |this| {
                this.cursor_pointer()
                    .hover(|style| style.bg(rgb(color::MERGE_OPTION_HOVER)))
                    .on_click(cx.listener(move |this, _, _, cx| this.choose(choice, cx)))
            })
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(30.))
                    .rounded(px(7.))
                    .bg(rgb(if primary {
                        color::MERGE_ICON_PRIMARY
                    } else {
                        color::MERGE_ICON
                    }))
                    .child(Icon::new(icon).size(px(16.)).text_color(rgb(if primary {
                        color::CHIP_TEXT
                    } else if enabled {
                        color::TEXT
                    } else {
                        color::TEXT_FAINT
                    }))),
            )
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .font_weight(if primary {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::MEDIUM
                            })
                            .text_color(rgb(if enabled {
                                color::TEXT
                            } else {
                                color::TEXT_FAINT
                            }))
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(if enabled {
                                color::TEXT_MUTED
                            } else {
                                color::TEXT_FAINT
                            }))
                            .child(why_not.unwrap_or(detail)),
                    ),
            )
            .when(self.running == Some(choice), |this| {
                this.child(
                    Icon::new(IconName::LoaderCircle)
                        .size(px(16.))
                        .text_color(rgb(color::TEXT_MUTED))
                        .with_animation(
                            "merging",
                            Animation::new(std::time::Duration::from_secs(1)).repeat(),
                            |icon, delta| icon.transform(Transformation::rotate(percentage(delta))),
                        ),
                )
            })
    }
}

impl Render for MergeDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let merging = &self.merging;
        let branch = |name: &str| {
            labels::ref_label(&RefLabel {
                kind: RefKind::Branch,
                name: name.to_owned(),
                is_head: false,
            })
            .into_any_element()
        };
        let (head, base) = (merging.head.clone(), merging.base.clone());
        let ahead = self.ahead();
        let commits = count(ahead, "commit", "commits");
        let summary = match &self.preview {
            None => vec![muted("Comparing the branches…").into_any_element()],
            Some(Err(error)) => vec![
                div()
                    .text_color(rgb(color::DIFF_REMOVED))
                    .child(error.clone())
                    .into_any_element(),
            ],
            Some(Ok(preview)) => {
                let mut parts = vec![
                    div().child(format!("{commits} in")).into_any_element(),
                    div()
                        .child(count(preview.files, "file", "files"))
                        .into_any_element(),
                ];
                if preview.behind > 0 {
                    parts.push(
                        div()
                            .child(format!("{base} is {} ahead", preview.behind))
                            .into_any_element(),
                    );
                }
                if !preview.conflicts.is_empty() {
                    let names: Vec<&str> = preview
                        .conflicts
                        .iter()
                        .map(|path| path.rsplit('/').next().unwrap_or(path))
                        .collect();
                    parts.push(
                        div()
                            .h_flex()
                            .gap(px(5.))
                            .text_color(rgb(color::CHIP_TEXT))
                            .child(Icon::new(IconName::CircleAlert).size(px(12.)))
                            .child(format!(
                                "{} expected · {}",
                                count(names.len(), "conflict", "conflicts"),
                                names.join(", ")
                            ))
                            .into_any_element(),
                    );
                }
                parts
            }
        };

        div()
            .v_flex()
            .gap(px(4.))
            .child(
                div()
                    .v_flex()
                    .gap(px(8.))
                    .pb(px(12.))
                    .border_b_1()
                    .border_color(rgb(color::MERGE_DIVIDER))
                    .child(
                        div()
                            .h_flex()
                            .flex_wrap()
                            .gap(px(8.))
                            .child(branch(&head))
                            .child(
                                Icon::new(IconName::ArrowRight)
                                    .size(px(16.))
                                    .text_color(rgb(color::TEXT_MUTED)),
                            )
                            .child(branch(&base)),
                    )
                    .child(
                        div()
                            .h_flex()
                            .flex_wrap()
                            .gap(px(12.))
                            .text_size(px(12.))
                            .text_color(rgb(color::AUTHOR_TEXT))
                            .children(summary),
                    ),
            )
            .child(
                div()
                    .v_flex()
                    .gap(px(2.))
                    .py(px(4.))
                    .child(self.render_option(
                        Choice::Merge(MergeMethod::Merge),
                        IconName::GitMerge,
                        format!("Merge into {base}"),
                        if self.conflicts() {
                            "Creates a merge commit. The conflict editor opens next.".into()
                        } else {
                            "Creates a merge commit.".into()
                        },
                        cx,
                    ))
                    .child(self.render_option(
                        Choice::Merge(MergeMethod::Squash),
                        IconName::Combine,
                        "Squash and merge".into(),
                        if ahead == 1 {
                            format!("The commit is copied onto {base} as a new commit")
                        } else {
                            format!("All {commits} become one commit on {base}")
                        },
                        cx,
                    ))
                    .map(|this| {
                        if self.dropped {
                            this.child(self.render_option(
                                Choice::Rebase,
                                IconName::GitFork,
                                format!("Rebase {head} onto {base}"),
                                format!("Replays {commits} on top of {base}. Linear history."),
                                cx,
                            ))
                            .child(self.render_option(
                                Choice::Review,
                                IconName::GitPullRequestArrow,
                                match &merging.pull_request {
                                    Some(pull_request) => {
                                        format!("Open local pull request #{}", pull_request.number)
                                    }
                                    None => "Open as local pull request".into(),
                                },
                                "Review the diff and leave comments before merging".into(),
                                cx,
                            ))
                        } else {
                            this.child(self.render_option(
                                Choice::Merge(MergeMethod::Rebase),
                                IconName::GitFork,
                                format!("Rebase onto {base} and merge"),
                                format!("Replays {commits} on top of {base}. Linear history."),
                                cx,
                            ))
                        }
                    })
                    .child(self.render_option(
                        Choice::Merge(MergeMethod::FastForward),
                        IconName::ArrowRightToLine,
                        format!("Fast-forward {base}"),
                        format!("Moves {base} to the branch’s last commit"),
                        cx,
                    )),
            )
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
                muted(match (&merging.pull_request, self.dropped) {
                    (Some(_), false) => format!(
                        "Runs your git. Afterwards {base} is checked out and the pull request \
                         is marked merged; if git stops, nothing is changed."
                    ),
                    (Some(pull_request), true) => format!(
                        "Runs your git. A merge leaves {base} checked out and marks pull \
                         request #{} merged; if git stops, nothing is changed.",
                        pull_request.number
                    ),
                    (None, _) => format!(
                        "Runs your git. A merge leaves {base} checked out; if git stops, \
                         nothing is changed."
                    ),
                })
                .pt(px(8.))
                .border_t_1()
                .border_color(rgb(color::MERGE_DIVIDER))
                .text_size(theme::SMALL_TEXT_SIZE)
                .line_height(relative(1.5)),
            )
    }
}

impl Review {
    /// Opens the merge dialog for the pull request reviewed.
    pub(super) fn open_merge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Subject::PullRequest(subject) = &self.subject else {
            return;
        };
        let (Some(head_tip), Some(base_tip)) = (subject.tips.head, subject.tips.base) else {
            return;
        };
        let pull_request = &subject.pull_request;
        let merging = Merging {
            pull_request: Some(MergedPullRequest {
                number: pull_request.number,
                title: pull_request.title.clone(),
                description: pull_request.description.clone(),
            }),
            head: pull_request.head.branch.clone(),
            base: pull_request.base.branch.clone(),
            head_tip,
            base_tip,
        };
        let (repository, store) = (self.repository.clone(), subject.store.clone());
        let dialog = cx.new(|cx| MergeDialog::new(repository, store, merging, false, cx));
        let subscription =
            cx.subscribe_in(&dialog, window, |this, _, event, window, cx| match event {
                MergeEvent::Merged {
                    pull_request: Some(pull_request),
                    step,
                } => this.merged(pull_request.as_ref().clone(), *step, window, cx),
                MergeEvent::Stopped => {
                    window.close_dialog(cx);
                    cx.emit(ReviewEvent::MergeStopped);
                }
                // Not offered for a pull request.
                MergeEvent::Merged {
                    pull_request: None, ..
                }
                | MergeEvent::Rebased { .. }
                | MergeEvent::Review(_) => {}
            });
        if let Subject::PullRequest(subject) = &mut self.subject {
            subject.merge_dialog = Some(subscription);
        }
        let review = cx.entity().downgrade();
        window.open_dialog(cx, move |modal, _, _| {
            let review = review.clone();
            modal
                .w(px(460.))
                .child(dialog.clone())
                .on_close(move |_, _, cx| {
                    review
                        .update(cx, |this, _| {
                            if let Subject::PullRequest(subject) = &mut this.subject {
                                subject.merge_dialog = None;
                            }
                        })
                        .ok();
                })
        });
    }

    /// Shows `pull_request`, just merged.
    fn merged(
        &mut self,
        pull_request: PullRequest,
        step: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Subject::PullRequest(subject) = &mut self.subject else {
            return;
        };
        window.close_dialog(cx);
        cx.emit(ReviewEvent::Merged {
            message: format!(
                "Merged #{} into {}",
                pull_request.number, pull_request.base.branch
            )
            .into(),
            step,
        });
        let (number, tips, store) = (pull_request.number, subject.tips, subject.store.clone());
        subject.pull_request = pull_request;
        self._load = Self::load_pull_request(&self.repository, &store, number, tips, cx);
        cx.notify();
    }
}

/// A merge or rebase for [`carry_out`].
struct Carry {
    /// `None` rebases the head onto the base without merging.
    how: Option<MergeMethod>,
    message: Option<String>,
    head: String,
    base: String,
    /// The pull request the merge marks merged.
    number: Option<u32>,
    base_before: Oid,
}

/// Merges or rebases as `carry` says with `git`, in the main working tree,
/// and saves the pull request, if any, as merged. Blocks; run it on a
/// background thread.
fn carry_out(
    repository: &Repository,
    store: &Store,
    git: &Git,
    carry: Carry,
) -> anyhow::Result<(Done, Option<Recorded>)> {
    let Carry {
        how,
        message,
        head,
        base,
        number,
        base_before,
    } = carry;
    let return_to = match repository.refs()?.head {
        Some(Head::Branch { name, .. }) => Some(name),
        _ => None,
    };
    // A branch checked out in a linked worktree, such as an agent thread's,
    // can be rebased only there: git refuses to check it out here. The
    // worktree's working tree is the agent's, so the rebase does not run
    // `git` there directly; `rebase_in_worktree` checks the worktree's
    // `.git` pointer and git dir first and hardens the rebase.
    // The main working tree is left alone, as before: a branch checked
    // out there while a linked one is open is git's to refuse.
    let head_worktree = match git.worktree_of(&head)? {
        Some(worktree) if !worktree.linked => None,
        Some(worktree) if worktree.problem == Some(WorktreeProblem::Missing) => {
            // Its folder was deleted: git holds the branch for it until
            // pruned.
            git.worktree_prune()?;
            None
        }
        Some(worktree) => Some(worktree.path),
        None => None,
    };
    let common_dir = repository.common_dir();
    // What GitEZ recorded of the worktree when it made it for a thread, so
    // the check compares against that rather than only the git files.
    // Found by the branch, so nothing in the worktree is looked at.
    let record = head_worktree.as_ref().and_then(|_| {
        gitez_agent_thread::Store::new(&common_dir)
            .load_all()
            .0
            .into_iter()
            .find(|thread| thread.branch == head)
            .and_then(|thread| thread.worktree_record)
    });
    let rebase_head = |onto: &str| -> anyhow::Result<()> {
        match &head_worktree {
            Some(dir) => {
                gitez_agent_thread::rebase_in_worktree(&common_dir, dir, record.as_ref(), onto)?;
                Ok(())
            }
            None => Ok(git.rebase_branch(&head, onto, return_to.as_deref())?),
        }
    };
    let recorder = Recorder::start(git.work_dir());
    let (Some(how), Some(message)) = (how, message) else {
        rebase_head(&base)?;
        return Ok((Done::Rebased, recorder.finish(ResetMode::Keep)));
    };
    let how = match (how, &head_worktree) {
        (MergeMethod::Rebase, Some(_)) => {
            rebase_head(&base)?;
            MergeMethod::FastForward
        }
        (how, _) => how,
    };
    match git.merge_branch(&head, &base, how, &message, return_to.as_deref())? {
        MergeOutcome::Merged => {
            let merged = number
                .map(|number| record_merge(repository, store, number, base_before))
                .transpose()?;
            let mut recorded = recorder.finish(ResetMode::Keep);
            let pull_request = merged.map(|(pull_request, review)| {
                recorded = recorded.take().map(|recorded| recorded.with_review(review));
                Box::new(pull_request)
            });
            Ok((Done::Merged(pull_request), recorded))
        }
        // Recorded once the merge is committed.
        MergeOutcome::Conflicts => Ok((Done::Stopped, None)),
    }
}

/// Pull request `number` saved as merged into its base, which was on
/// `base_before` until then, with what undoing that puts back. Its threads
/// follow the head if a rebase moved it. Blocks; run it on a background
/// thread.
pub(crate) fn record_merge(
    repository: &Repository,
    store: &Store,
    number: u32,
    base_before: Oid,
) -> anyhow::Result<(PullRequest, ReviewChange)> {
    let mut pull_request = store.load(number)?;
    let before = ReviewTips::of(&pull_request);
    let head = repository
        .refs()?
        .branches
        .into_iter()
        .find(|branch| branch.name == pull_request.head.branch)
        .map(|branch| branch.target);
    let now = Timestamp::now();
    if let Some(head) = head {
        follow_head(repository, &mut pull_request, head, now)?;
    }
    // Its changes stay what they were before the merge: from where it left
    // the base to its head.
    pull_request.base.commit = base_before.to_string();
    pull_request.state = State::Merged;
    pull_request.updated = now;
    store.save(&pull_request)?;
    let review = ReviewChange {
        store: store.clone(),
        number,
        before,
        after: ReviewTips::of(&pull_request),
    };
    Ok((pull_request, review))
}

/// The commit message for merging `merging` the way `how` says, with
/// `ahead` commits.
fn message(merging: &Merging, how: MergeMethod, ahead: usize) -> String {
    let (head, base) = (&merging.head, &merging.base);
    let pull_request = merging.pull_request.as_ref();
    let description = pull_request.map_or("", |pull_request| pull_request.description.trim());
    let mut message = match (how, pull_request) {
        (MergeMethod::Merge, Some(pull_request)) => format!(
            "Merge branch '{head}'\n\nLocal pull request #{}: {}",
            pull_request.number, pull_request.title
        ),
        // As git words it: the default branch goes unnamed.
        (MergeMethod::Merge, None) if DEFAULT_BRANCHES.contains(&base.as_str()) => {
            format!("Merge branch '{head}'")
        }
        (MergeMethod::Merge, None) => format!("Merge branch '{head}' into {base}"),
        (MergeMethod::Squash, Some(pull_request)) => {
            format!("{} (#{})", pull_request.title, pull_request.number)
        }
        (MergeMethod::Squash, None) => format!("Squash branch '{head}'"),
        // No commit of its own.
        (MergeMethod::Rebase | MergeMethod::FastForward, _) => return String::new(),
    };
    if !description.is_empty() {
        message.push_str("\n\n");
        message.push_str(description);
    }
    if how == MergeMethod::Squash {
        message.push_str(&format!(
            "\n\nSquashed from {} on {}.",
            count(ahead, "commit", "commits"),
            head
        ));
    }
    message
}

/// Branches git leaves out of its merge messages (`merge.suppressDest`).
const DEFAULT_BRANCHES: [&str; 2] = ["main", "master"];

fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use gitez_command::{Git, MergeMethod};
    use gitez_git::{Oid, Repository};
    use gitez_review::{NewPullRequest, State, Store, Timestamp, Tip};

    use super::{Carry, Done, MergedPullRequest, Merging, carry_out, message};

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git runs");
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout)
            .expect("utf-8")
            .trim()
            .to_owned()
    }

    /// A repository with `main` and `agent/fix` checked out in a linked
    /// worktree, each a commit past where they split, and an open pull
    /// request of `agent/fix` into `main`.
    fn thread_repository(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf, u32) {
        let main = dir.join("repo");
        std::fs::create_dir_all(&main).expect("folder");
        git(&main, &["init", "-q", "-b", "main"]);
        for (key, value) in [
            ("user.name", "Ada Lovelace"),
            ("user.email", "ada@example.com"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "/dev/null"),
        ] {
            git(&main, &["config", key, value]);
        }
        std::fs::write(main.join("a.txt"), "a\n").expect("write");
        git(&main, &["add", "."]);
        git(&main, &["commit", "-qm", "First"]);
        let start = git(&main, &["rev-parse", "HEAD"]);
        let worktree = dir.join("worktree");
        git(&main, &["branch", "agent/fix"]);
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                worktree.to_str().expect("utf-8"),
                "agent/fix",
            ],
        );
        std::fs::write(worktree.join("b.txt"), "b\n").expect("write");
        git(&worktree, &["add", "."]);
        git(&worktree, &["commit", "-qm", "The agent's"]);
        let head = git(&worktree, &["rev-parse", "HEAD"]);
        std::fs::write(main.join("c.txt"), "c\n").expect("write");
        git(&main, &["add", "."]);
        git(&main, &["commit", "-qm", "Meanwhile on main"]);
        let common_dir = Repository::open(&main).expect("open").common_dir();
        let pull_request = Store::new(&common_dir)
            .create(
                NewPullRequest {
                    title: "Fix it".into(),
                    description: String::new(),
                    base: Tip {
                        branch: "main".into(),
                        commit: start,
                    },
                    head: Tip {
                        branch: "agent/fix".into(),
                        commit: head,
                    },
                },
                Timestamp::now(),
            )
            .expect("pull request");
        (main, worktree, pull_request.number)
    }

    fn carry(main: &Path, how: Option<MergeMethod>, number: u32) -> Carry {
        let base_before = Repository::open(main)
            .expect("open")
            .resolve_commit("refs/heads/main")
            .expect("resolve")
            .expect("main");
        Carry {
            how,
            message: how.map(|_| String::new()),
            head: "agent/fix".into(),
            base: "main".into(),
            number: Some(number),
            base_before,
        }
    }

    /// Rebasing a branch checked out in a linked worktree (an agent
    /// thread's) happens in that worktree; git refuses it anywhere else.
    #[test]
    fn a_branch_of_a_linked_worktree_is_rebased_there_and_merged() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (main, worktree, number) = thread_repository(dir.path());
        let repository = Repository::open(&main).expect("open");
        let found = Git::new(&main)
            .worktree_of("agent/fix")
            .expect("read")
            .expect("checked out in the worktree");
        assert!(found.linked);
        assert_eq!(
            found.path.canonicalize().expect("canonical"),
            worktree.canonicalize().expect("canonical")
        );
        let store = Store::new(&repository.common_dir());
        let (done, _) = carry_out(
            &repository,
            &store,
            &Git::new(&main),
            carry(&main, Some(MergeMethod::Rebase), number),
        )
        .expect("rebased and merged");
        assert!(matches!(done, Done::Merged(Some(_))));
        assert_eq!(store.load(number).expect("load").state, State::Merged);
        // main moved to the rebased branch, which the worktree still has.
        let tip = git(&main, &["rev-parse", "main"]);
        assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), tip);
        assert_eq!(git(&main, &["rev-parse", "agent/fix"]), tip);
        assert_eq!(git(&main, &["log", "--format=%s", "-3"]).lines().count(), 3);
        assert_eq!(git(&main, &["branch", "--show-current"]), "main");
        assert_eq!(git(&worktree, &["status", "--porcelain"]), "");
    }

    /// Rebasing alone (a branch dropped on another) too; and a worktree
    /// whose folder was deleted no longer holds its branch.
    #[test]
    fn a_branch_of_a_deleted_worktree_is_rebased_here() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (main, worktree, number) = thread_repository(dir.path());
        let repository = Repository::open(&main).expect("open");
        let store = Store::new(&repository.common_dir());
        carry_out(
            &repository,
            &store,
            &Git::new(&main),
            carry(&main, None, number),
        )
        .expect("rebased in the worktree");
        let main_tip = git(&main, &["rev-parse", "main"]);
        assert_eq!(git(&worktree, &["rev-parse", "HEAD~1"]), main_tip);

        std::fs::remove_dir_all(&worktree).expect("delete the worktree");
        std::fs::write(main.join("d.txt"), "d\n").expect("write");
        git(&main, &["add", "."]);
        git(&main, &["commit", "-qm", "More on main"]);
        let (done, _) = carry_out(
            &repository,
            &store,
            &Git::new(&main),
            carry(&main, Some(MergeMethod::Rebase), number),
        )
        .expect("rebased here and merged");
        assert!(matches!(done, Done::Merged(Some(_))));
        assert_eq!(
            Git::new(&main).worktree_of("agent/fix").expect("read"),
            None
        );
        assert_eq!(git(&main, &["branch", "--show-current"]), "main");
    }

    fn merging(description: &str) -> Merging {
        Merging {
            pull_request: Some(MergedPullRequest {
                number: 3,
                title: "Anchor comments to hunks".into(),
                description: description.into(),
            }),
            head: "feature/review".into(),
            base: "main".into(),
            head_tip: commit(),
            base_tip: commit(),
        }
    }

    fn commit() -> Oid {
        Oid::from_hex(b"4b825dc642cb6eb9a060e54bf8d69288fbee4904").expect("an id")
    }

    #[test]
    fn merge_messages_name_the_pull_request() {
        assert_eq!(
            message(&merging(""), MergeMethod::Merge, 2),
            "Merge branch 'feature/review'\n\nLocal pull request #3: Anchor comments to hunks"
        );
        assert_eq!(
            message(&merging("Why.\n"), MergeMethod::Squash, 2),
            "Anchor comments to hunks (#3)\n\nWhy.\n\nSquashed from 2 commits on feature/review."
        );
        assert_eq!(message(&merging("Why."), MergeMethod::Rebase, 2), "");
    }

    #[test]
    fn branch_merge_messages_follow_git() {
        let mut branches = merging("");
        branches.pull_request = None;
        assert_eq!(
            message(&branches, MergeMethod::Merge, 2),
            "Merge branch 'feature/review'"
        );
        assert_eq!(
            message(&branches, MergeMethod::Squash, 2),
            "Squash branch 'feature/review'\n\nSquashed from 2 commits on feature/review."
        );
        branches.base = "develop".into();
        assert_eq!(
            message(&branches, MergeMethod::Merge, 1),
            "Merge branch 'feature/review' into develop"
        );
    }
}
