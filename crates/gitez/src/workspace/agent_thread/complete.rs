//! Completing an agent thread by hand, the same way from every place that
//! offers it (the thread view, Home's rows, the Agents popover): one click
//! when the worktree has nothing uncommitted; a question naming what would
//! be lost when it has; a message, and no destructive action, when GitEZ no
//! longer trusts the worktree. Also removing the worktree a completed
//! thread kept.

use std::rc::Rc;

use gitez_agent_thread::{AgentThread, CompleteReport, IfUncommitted, Kept};
use gpui_kit::component::button::*;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::agent_threads::{AgentThreadKey, AgentThreads};
use crate::theme::color;

/// Told `true` when a completion starts in the background and `false` when
/// it ends, so the caller's button can wait.
pub type Busy = Rc<dyn Fn(bool, &mut Window, &mut App)>;

/// How many of the uncommitted paths a question lists.
const PATHS_SHOWN: usize = 5;

/// Completes thread `key`: asks first when its worktree has uncommitted
/// changes or is not trusted, otherwise removes it at once.
pub fn complete(key: AgentThreadKey, busy: Busy, window: &mut Window, cx: &mut App) {
    let model = AgentThreads::global(cx);
    let (thread, facts) = {
        let model = model.read(cx);
        (model.thread(&key).cloned(), model.facts(&key))
    };
    let Some(thread) = thread else {
        return;
    };
    if let Some(problem) = facts.untrusted {
        ask_untrusted(key, &thread, problem, busy, window, cx);
    } else if thread.has_uncommitted() == Some(true) {
        ask_uncommitted(key, &thread, busy, window, cx);
    } else {
        run(key, IfUncommitted::Refuse, busy, window, cx);
    }
}

/// Asks the model to complete `key` the way `how` says, then says what
/// happened; asks if the worktree turned out to have uncommitted changes
/// or not to be trusted.
fn run(key: AgentThreadKey, how: IfUncommitted, busy: Busy, window: &mut Window, cx: &mut App) {
    let model = AgentThreads::global(cx);
    let task = model.update(cx, |model, cx| model.complete(&key, how, cx));
    // Deferred: the caller may be the view `busy` updates, mid-update.
    let started = busy.clone();
    window.defer(cx, move |window, cx| started(true, window, cx));
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            cx.update(|window, cx| {
                busy(false, window, cx);
                match result {
                    Ok(report) => window.push_notification(report_toast(&report), cx),
                    Err(error) => {
                        let thread = AgentThreads::global(cx).read(cx).thread(&key).cloned();
                        match (error.downcast_ref::<gitez_agent_thread::Error>(), thread) {
                            (Some(gitez_agent_thread::Error::Uncommitted { .. }), Some(thread)) => {
                                ask_uncommitted(key, &thread, busy, window, cx)
                            }
                            (
                                Some(gitez_agent_thread::Error::Tampered { problem, .. }),
                                Some(thread),
                            ) => {
                                let problem = problem.clone();
                                ask_untrusted(key, &thread, problem, busy, window, cx)
                            }
                            _ => window
                                .push_notification(crate::toast::error(format!("{error:#}")), cx),
                        }
                    }
                }
            })
            .ok();
        })
        .detach();
}

/// What a completion did, as a toast.
pub fn report_toast(report: &CompleteReport) -> Notification {
    match &report.kept {
        Some(Kept::Uncommitted { count }) => crate::toast::warning(match count {
            Some(1) => "Completed; the worktree is kept with its 1 uncommitted change".to_owned(),
            Some(count) => {
                format!("Completed; the worktree is kept with its {count} uncommitted changes")
            }
            None => "Completed; the worktree is kept with its uncommitted changes".to_owned(),
        }),
        Some(Kept::Untrusted(_)) => crate::toast::warning(
            "Completed; the worktree is kept because GitEZ no longer trusts it",
        ),
        None if report.nothing_to_remove => crate::toast::success("Completed"),
        None => crate::toast::success(match (report.worktree_removed, report.branch_deleted) {
            (true, true) => "Completed; the worktree and its empty branch are removed",
            (true, false) => "Completed; the worktree is removed and the branch kept",
            (false, true) => {
                "Completed; the worktree was already gone, the empty branch is removed"
            }
            (false, false) => "Completed; the worktree was already gone",
        }),
    }
}

/// "3 uncommitted changes".
fn changes(count: u32) -> String {
    match count {
        1 => "1 uncommitted change".to_owned(),
        count => format!("{count} uncommitted changes"),
    }
}

/// The uncommitted paths `thread` recorded, a few, in monospace.
fn paths(thread: &AgentThread, cx: &App) -> Option<Div> {
    let uncommitted = thread.uncommitted.as_ref()?;
    if uncommitted.paths.is_empty() {
        return None;
    }
    let mono = cx.theme().mono_font_family.clone();
    let more =
        (uncommitted.count as usize).saturating_sub(PATHS_SHOWN.min(uncommitted.paths.len()));
    Some(
        div()
            .v_flex()
            .gap(px(2.))
            .px(px(10.))
            .py(px(8.))
            .rounded(px(6.))
            .bg(rgb(color::FIELD))
            .font_family(mono)
            .text_size(px(11.5))
            .text_color(rgb(color::TEXT_SECONDARY))
            .children(
                uncommitted
                    .paths
                    .iter()
                    .take(PATHS_SHOWN)
                    .map(|path| div().child(path.trim().to_owned())),
            )
            .when(more > 0, |this| {
                this.child(
                    div()
                        .text_color(rgb(color::TEXT_MUTED))
                        .child(format!("and {more} more")),
                )
            }),
    )
}

/// A dialog's paragraph.
fn paragraph(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.5))
        .line_height(relative(1.5))
        .text_color(rgb(color::TEXT_SECONDARY))
        .child(text.into())
}

/// Asks whether to discard the uncommitted changes of `thread`'s worktree
/// or keep the worktree; keeping is the default.
fn ask_uncommitted(
    key: AgentThreadKey,
    thread: &AgentThread,
    busy: Busy,
    window: &mut Window,
    cx: &mut App,
) {
    let count = thread
        .uncommitted
        .as_ref()
        .map_or(0, |uncommitted| uncommitted.count);
    let what = if count > 0 {
        changes(count)
    } else {
        "uncommitted changes".to_owned()
    };
    let text = format!(
        "“{}” left {what} in its worktree at {}. Completing removes the worktree, and they are \
         lost. Keep the worktree to complete the thread and look at them later; it stays listed \
         with the thread until you remove it.",
        thread.title,
        thread.worktree_display()
    );
    let commit_error = super::commit_error_text(thread);
    let thread = thread.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let paths = paths(&thread, cx);
        let (keep_key, keep_busy) = (key.clone(), busy.clone());
        let (discard_key, discard_busy) = (key.clone(), busy.clone());
        dialog
            .title("Discard the uncommitted changes?")
            .w(px(500.))
            .child(
                div()
                    .v_flex()
                    .gap(px(10.))
                    .child(paragraph(text.clone()))
                    .children(paths)
                    .children(
                        commit_error
                            .clone()
                            .map(|error| paragraph(error).text_color(rgb(color::WARNING_TEXT))),
                    ),
            )
            .footer(
                div()
                    .h_flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("complete-discard")
                            .debug_selector(|| "complete-discard".into())
                            .danger()
                            .label("Discard and complete")
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                run(
                                    discard_key.clone(),
                                    IfUncommitted::Discard,
                                    discard_busy.clone(),
                                    window,
                                    cx,
                                );
                            }),
                    )
                    .child(
                        Button::new("complete-keep")
                            .debug_selector(|| "complete-keep".into())
                            .primary()
                            .label("Keep the worktree")
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                run(
                                    keep_key.clone(),
                                    IfUncommitted::Keep,
                                    keep_busy.clone(),
                                    window,
                                    cx,
                                );
                            }),
                    ),
            )
    });
}

/// Says that GitEZ no longer trusts `thread`'s worktree, so it will not
/// remove it, and offers only to complete the thread keeping it.
fn ask_untrusted(
    key: AgentThreadKey,
    thread: &AgentThread,
    problem: String,
    busy: Busy,
    window: &mut Window,
    cx: &mut App,
) {
    let text = untrusted_text(thread, &problem);
    window.open_dialog(cx, move |dialog, _, _| {
        let (key, busy) = (key.clone(), busy.clone());
        dialog
            .title("GitEZ no longer trusts this worktree")
            .w(px(500.))
            .child(paragraph(text.clone()))
            .footer(
                div()
                    .h_flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("complete-untrusted-cancel")
                            .ghost()
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("complete-untrusted-keep")
                            .debug_selector(|| "complete-untrusted-keep".into())
                            .primary()
                            .label("Complete and keep the worktree")
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                run(key.clone(), IfUncommitted::Keep, busy.clone(), window, cx);
                            }),
                    ),
            )
    });
}

/// What GitEZ says of a worktree it no longer trusts.
pub fn untrusted_text(thread: &AgentThread, problem: &str) -> String {
    format!(
        "The git files of the worktree at {} changed since GitEZ made it: {problem} GitEZ runs no \
         git there and will not delete it, as that could act on another repository. Look at the \
         folder yourself and remove it when you are sure.",
        thread.worktree_display()
    )
}

/// Removes the worktree completed thread `key` kept, after asking: its
/// uncommitted changes are discarded. A worktree GitEZ no longer trusts is
/// not offered (see [`untrusted_text`]).
pub fn remove_worktree(key: AgentThreadKey, busy: Busy, window: &mut Window, cx: &mut App) {
    let Some(thread) = AgentThreads::global(cx).read(cx).thread(&key).cloned() else {
        return;
    };
    let what = match thread.uncommitted.as_ref().map(|found| found.count) {
        Some(count) if count > 0 => format!("its {}", changes(count)),
        _ => "what is in it".to_owned(),
    };
    // A merged pull request's branch goes with the worktree, as when the
    // merge completed the thread.
    let merged = AgentThreads::global(cx).read(cx).facts(&key).pull_request
        == Some(gitez_review::State::Merged);
    let text = if merged {
        format!(
            "This deletes the worktree at {} and {what}, and the branch {} unless it has commits \
             that the pull request did not merge. The thread stays readable.",
            thread.worktree_display(),
            thread.branch_name()
        )
    } else {
        format!(
            "This deletes the worktree at {} and {what}. The thread stays readable.",
            thread.worktree_display()
        )
    };
    window.open_dialog(cx, move |dialog, _, cx| {
        let paths = paths(&thread, cx);
        let (key, busy) = (key.clone(), busy.clone());
        dialog
            .title("Remove the kept worktree?")
            .w(px(500.))
            .child(
                div()
                    .v_flex()
                    .gap(px(10.))
                    .child(paragraph(text.clone()))
                    .children(paths),
            )
            .footer(
                div()
                    .h_flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("remove-worktree-cancel")
                            .ghost()
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("remove-worktree-confirm")
                            .debug_selector(|| "remove-worktree-confirm".into())
                            .danger()
                            .label("Remove worktree")
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                let (key, busy) = (key.clone(), busy.clone());
                                let task = AgentThreads::global(cx)
                                    .update(cx, |model, cx| model.remove_worktree(&key, cx));
                                let started = busy.clone();
                                window.defer(cx, move |window, cx| started(true, window, cx));
                                window
                                    .spawn(cx, async move |cx| {
                                        let result = task.await;
                                        cx.update(|window, cx| {
                                            busy(false, window, cx);
                                            let toast = match result {
                                                Ok(report) if report.branch_deleted => {
                                                    crate::toast::success(
                                                        "The worktree and its branch are removed",
                                                    )
                                                }
                                                Ok(_) => {
                                                    crate::toast::success("The worktree is removed")
                                                }
                                                Err(error) => crate::toast::error(format!(
                                                    "Could not remove the worktree: {error:#}"
                                                )),
                                            };
                                            window.push_notification(toast, cx);
                                        })
                                        .ok();
                                    })
                                    .detach();
                            }),
                    ),
            )
    });
}
