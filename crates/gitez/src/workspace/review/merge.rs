//! Merging a local pull request into its base, as in the dialog of the
//! `Merge.dc.html` artboard: how the branches compare, whether they would
//! conflict, and the ways to bring the branch in.

use gitez_command::{Git, MergeMethod};
use gitez_git::{Head, MergePreview, Oid, RefKind, RefLabel, Repository};
use gitez_review::{PullRequest, State, Store, Timestamp};
use gpui_kit::assets::IconName;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Review, ReviewEvent, Subject, follow_head, muted};
use crate::theme::{self, color};
use crate::workspace::labels;

/// What the dialog asks of the review.
pub(super) enum MergeEvent {
    /// The branch was merged and the pull request saved as merged.
    Merged(Box<PullRequest>),
}

/// The pull request as the dialog merges it.
pub(super) struct Merging {
    pub number: u32,
    pub title: String,
    pub description: String,
    pub head: String,
    pub base: String,
    pub head_tip: Oid,
    pub base_tip: Oid,
    /// Files the branch changes.
    pub files: usize,
}

pub(super) struct MergeDialog {
    repository: Repository,
    store: Store,
    merging: Merging,
    /// `None` while working it out.
    preview: Option<Result<MergePreview, SharedString>>,
    /// The way being merged now.
    running: Option<MergeMethod>,
    /// Why the last attempt failed: git's own words.
    error: Option<SharedString>,
    _task: Task<()>,
}

impl EventEmitter<MergeEvent> for MergeDialog {}

impl MergeDialog {
    pub(super) fn new(
        repository: Repository,
        store: Store,
        merging: Merging,
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
            preview: None,
            running: None,
            error: None,
            _task: task,
        }
    }

    /// Merges the way `how` says in the background, then saves the pull
    /// request as merged.
    fn merge(&mut self, how: MergeMethod, cx: &mut Context<Self>) {
        if self.running.is_some() || self.why_not(how).is_some() {
            return;
        }
        let Some(dir) = self.repository.work_dir() else {
            return;
        };
        let git = Git::new(dir);
        let (repository, store) = (self.repository.clone(), self.store.clone());
        let merging = &self.merging;
        let (number, head, base) = (merging.number, merging.head.clone(), merging.base.clone());
        let message = message(merging, how, self.ahead());
        let base_before = merging.base_tip;
        self.running = Some(how);
        self.error = None;
        self._task = cx.spawn(async move |this, cx| {
            let merged = cx
                .background_executor()
                .spawn(async move {
                    let return_to = match repository.refs()?.head {
                        Some(Head::Branch { name, .. }) => Some(name),
                        _ => None,
                    };
                    git.merge_branch(&head, &base, how, &message, return_to.as_deref())?;
                    record_merge(&repository, &store, number, base_before)
                })
                .await;
            this.update(cx, |this, cx| {
                this.running = None;
                match merged {
                    Ok(pull_request) => cx.emit(MergeEvent::Merged(Box::new(pull_request))),
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    fn ahead(&self) -> usize {
        match &self.preview {
            Some(Ok(preview)) => preview.ahead,
            _ => 0,
        }
    }

    /// Why merging the way `how` says is not possible, if it is not.
    fn why_not(&self, how: MergeMethod) -> Option<String> {
        let preview = match &self.preview {
            None => return Some("Comparing the branches…".into()),
            Some(Err(_)) => return Some("The branches could not be compared.".into()),
            Some(Ok(preview)) => preview,
        };
        let base = &self.merging.base;
        if preview.ahead == 0 {
            return Some(format!("{base} already has every commit of this branch"));
        }
        if self.repository.work_dir().is_none() {
            return Some("A bare repository has no working tree to merge in".into());
        }
        match how {
            MergeMethod::FastForward if preview.behind > 0 => {
                Some("Not possible — the branches have diverged".into())
            }
            MergeMethod::FastForward => None,
            _ if !preview.conflicts.is_empty() => Some(format!(
                "Would conflict — merge {base} into the branch and resolve {} first",
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
        how: MergeMethod,
        icon: IconName,
        title: String,
        detail: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let why_not = self.why_not(how);
        let enabled = why_not.is_none() && self.running.is_none();
        let primary = how == MergeMethod::Merge && enabled;
        div()
            .id(match how {
                MergeMethod::Merge => "merge-commit",
                MergeMethod::Squash => "merge-squash",
                MergeMethod::Rebase => "merge-rebase",
                MergeMethod::FastForward => "merge-fast-forward",
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
                    .on_click(cx.listener(move |this, _, _, cx| this.merge(how, cx)))
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
            .when(self.running == Some(how), |this| {
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
                        .child(count(merging.files, "file", "files"))
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
                        MergeMethod::Merge,
                        IconName::GitMerge,
                        format!("Merge into {base}"),
                        "Creates a merge commit.".into(),
                        cx,
                    ))
                    .child(self.render_option(
                        MergeMethod::Squash,
                        IconName::Combine,
                        "Squash and merge".into(),
                        if ahead == 1 {
                            format!("The commit is copied onto {base} as a new commit")
                        } else {
                            format!("All {commits} become one commit on {base}")
                        },
                        cx,
                    ))
                    .child(self.render_option(
                        MergeMethod::Rebase,
                        IconName::GitFork,
                        format!("Rebase onto {base} and merge"),
                        format!("Replays {commits} on top of {base}. Linear history."),
                        cx,
                    ))
                    .child(self.render_option(
                        MergeMethod::FastForward,
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
                muted(format!(
                    "Runs your git. Afterwards {base} is checked out and the pull request \
                     is marked merged; if git stops, nothing is changed."
                ))
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
            number: pull_request.number,
            title: pull_request.title.clone(),
            description: pull_request.description.clone(),
            head: pull_request.head.branch.clone(),
            base: pull_request.base.branch.clone(),
            head_tip,
            base_tip,
            files: match &self.files {
                Some(Ok(files)) => files.len(),
                _ => 0,
            },
        };
        let (repository, store) = (self.repository.clone(), subject.store.clone());
        let dialog = cx.new(|cx| MergeDialog::new(repository, store, merging, cx));
        let subscription = cx.subscribe_in(&dialog, window, |this, _, event, window, cx| {
            let MergeEvent::Merged(pull_request) = event;
            this.merged(pull_request.as_ref().clone(), window, cx);
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
    fn merged(&mut self, pull_request: PullRequest, window: &mut Window, cx: &mut Context<Self>) {
        let Subject::PullRequest(subject) = &mut self.subject else {
            return;
        };
        window.close_dialog(cx);
        window.push_notification(
            Notification::success(format!(
                "Merged #{} into {}",
                pull_request.number, pull_request.base.branch
            )),
            cx,
        );
        let (number, tips, store) = (pull_request.number, subject.tips, subject.store.clone());
        subject.pull_request = pull_request;
        self._load = Self::load_pull_request(&self.repository, &store, number, tips, cx);
        // The branches moved and another one is checked out.
        cx.emit(ReviewEvent::CheckedOut);
        cx.notify();
    }
}

/// Pull request `number` saved as merged into its base, which was on
/// `base_before` until then. Its threads follow the head if a rebase moved
/// it. Blocks; run it on a background thread.
fn record_merge(
    repository: &Repository,
    store: &Store,
    number: u32,
    base_before: Oid,
) -> anyhow::Result<PullRequest> {
    let mut pull_request = store.load(number)?;
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
    Ok(pull_request)
}

/// The commit message for merging `merging` the way `how` says, with
/// `ahead` commits.
fn message(merging: &Merging, how: MergeMethod, ahead: usize) -> String {
    let description = merging.description.trim();
    let mut message = match how {
        MergeMethod::Merge => format!(
            "Merge branch '{}'\n\nLocal pull request #{}: {}",
            merging.head, merging.number, merging.title
        ),
        MergeMethod::Squash => format!("{} (#{})", merging.title, merging.number),
        // No commit of its own.
        MergeMethod::Rebase | MergeMethod::FastForward => return String::new(),
    };
    if !description.is_empty() {
        message.push_str("\n\n");
        message.push_str(description);
    }
    if how == MergeMethod::Squash {
        message.push_str(&format!(
            "\n\nSquashed from {} on {}.",
            count(ahead, "commit", "commits"),
            merging.head
        ));
    }
    message
}

fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

#[cfg(test)]
mod tests {
    use gitez_command::MergeMethod;
    use gitez_git::Oid;

    use super::{Merging, message};

    fn merging(description: &str) -> Merging {
        Merging {
            number: 3,
            title: "Anchor comments to hunks".into(),
            description: description.into(),
            head: "feature/review".into(),
            base: "main".into(),
            head_tip: commit(),
            base_tip: commit(),
            files: 2,
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
}
