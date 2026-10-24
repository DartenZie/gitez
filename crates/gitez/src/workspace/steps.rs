//! Undo and redo in the window: the toolbar buttons, `⌘Z` and `⇧⌘Z`, and
//! the "Undo" on a notification.

use std::path::PathBuf;

use gitez_command::Git;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::*;
use gpui_kit::*;

use super::palette::lowercase_first;
use super::{RedoStep, UndoStep, Workspace};
use crate::undo::{self, Direction, Entry};

impl Workspace {
    /// The key of the open repository's undo log: its git common directory.
    fn undo_key(&self) -> Option<PathBuf> {
        Some(self.snapshot.as_ref()?.repository.common_dir())
    }

    /// The step that undo, or redo, would take now.
    pub(super) fn next_step(&self, direction: Direction, cx: &App) -> Option<Entry> {
        undo::next(cx, &self.undo_key()?, direction)
    }

    pub(super) fn undo_step(&mut self, _: &UndoStep, window: &mut Window, cx: &mut Context<Self>) {
        self.step(Direction::Undo, None, window, cx);
    }

    pub(super) fn redo_step(&mut self, _: &RedoStep, window: &mut Window, cx: &mut Context<Self>) {
        self.step(Direction::Redo, None, window, cx);
    }

    /// Undoes or redoes the next step, if its id is `only` when given.
    pub(super) fn step(
        &mut self,
        direction: Direction,
        only: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.stepping.is_some() {
            return;
        }
        let (Some(key), Some(dir)) = (
            self.undo_key(),
            self.snapshot.as_ref().map(|snapshot| snapshot.path.clone()),
        ) else {
            return;
        };
        let Some(next) = undo::next(cx, &key, direction) else {
            return;
        };
        if only.is_some_and(|id| id != next.id) {
            window.push_notification(
                Notification::info(format!(
                    "Undo “{}” and the steps after it first.",
                    next.label
                )),
                cx,
            );
            return;
        }
        let Some(mut entry) = undo::take(cx, &key, direction, next.id) else {
            return;
        };
        let run = cx.background_executor().spawn(async move {
            let result = entry.change.apply(&Git::new(dir), direction);
            (entry, result)
        });
        self.stepping = Some(cx.spawn_in(window, async move |this, cx| {
            let (entry, result) = run.await;
            this.update_in(cx, |this, window, cx| {
                this.stepping = None;
                let label = entry.label.clone();
                let id = entry.id;
                undo::put_back(cx, &key, direction, entry, result.is_ok());
                match (result, direction) {
                    (Ok(()), Direction::Undo) => this.notify_with_action(
                        Notification::success(format!("Undone: {label}")),
                        "Redo",
                        move |this, window, cx| this.step(Direction::Redo, Some(id), window, cx),
                        window,
                        cx,
                    ),
                    (Ok(()), Direction::Redo) => this.notify_with_action(
                        Notification::success(format!("Redone: {label}")),
                        "Undo",
                        move |this, window, cx| this.step(Direction::Undo, Some(id), window, cx),
                        window,
                        cx,
                    ),
                    (Err(error), Direction::Undo) => window.push_notification(
                        Notification::error(format!(
                            "Could not undo {}: {error:#}",
                            lowercase_first(&label)
                        )),
                        cx,
                    ),
                    (Err(error), Direction::Redo) => window.push_notification(
                        Notification::error(format!(
                            "Could not redo {}: {error:#}",
                            lowercase_first(&label)
                        )),
                        cx,
                    ),
                }
                this.reload(window, cx);
                this.refresh_status(window, cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Shows `notification` about undo step `step`, with an "Undo" button
    /// when there is one.
    pub(super) fn notify_done(
        &mut self,
        notification: Notification,
        step: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match step {
            Some(id) => self.notify_with_action(
                notification,
                "Undo",
                move |this, window, cx| this.step(Direction::Undo, Some(id), window, cx),
                window,
                cx,
            ),
            None => window.push_notification(notification, cx),
        }
    }
}
