//! Editing a file of a local pull request's head branch from its diff: the
//! file as it is in the working tree, saved back there and, if asked,
//! committed on its own so the pull request shows the change.

use gitez_command::{Git, ResetMode};
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Review, ReviewEvent, Subject};
use crate::theme::{self, color};
use crate::undo::{self, Recorder};

/// Height of the editor in the dialog.
const EDITOR_HEIGHT: Pixels = px(460.);

/// What the form tells the review.
pub(super) enum FileEditEvent {
    /// The file was saved, and committed when `committed`; `step` is the
    /// undo step of the commit.
    Saved { committed: bool, step: Option<u64> },
}

/// The "Edit file" form: the file's text and the message to commit it with.
pub(super) struct FileEditForm {
    git: Git,
    path: String,
    editor: Entity<EditorState>,
    message: Entity<InputState>,
    /// `None` while reading the file; an error when it cannot be edited.
    loaded: Option<Result<(), SharedString>>,
    /// Saving, or saving and committing.
    running: Option<Task<()>>,
    /// Why the last save failed.
    error: Option<SharedString>,
    _load: Task<()>,
    _message_events: Subscription,
}

impl EventEmitter<FileEditEvent> for FileEditForm {}

impl FileEditForm {
    /// Reads `path` of the working tree of `git` in the background.
    fn new(git: Git, path: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| EditorState::new(window, cx));
        let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
        let message = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("Commit message");
            input.set_value(format!("Update {name}"), window, cx);
            input
        });
        let message_events = cx.subscribe_in(&message, window, |this, _, event, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.save(true, window, cx);
            }
        });
        let file = git.work_dir().join(&path);
        let load = cx.spawn_in(window, async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move { std::fs::read(&file) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.loaded = Some(match read {
                    Ok(bytes) => match String::from_utf8(bytes) {
                        Ok(text) => {
                            this.editor.update(cx, |editor, cx| {
                                editor.set_value(text, window, cx);
                                editor.focus(window, cx);
                            });
                            Ok(())
                        }
                        Err(_) => Err("This file is not text, so it cannot be edited here.".into()),
                    },
                    Err(error) => Err(format!("Could not read {}: {error}", this.path).into()),
                });
                cx.notify();
            })
            .ok();
        });
        Self {
            git,
            path,
            editor,
            message,
            loaded: None,
            running: None,
            error: None,
            _load: load,
            _message_events: message_events,
        }
    }

    fn can_save(&self) -> bool {
        matches!(self.loaded, Some(Ok(()))) && self.running.is_none()
    }

    /// Writes the text to the file and, with `commit`, commits that file
    /// alone with the message.
    fn save(&mut self, commit: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_save() {
            return;
        }
        let text = self.editor.read(cx).value().to_string();
        let message = self.message.read(cx).value().trim().to_owned();
        if commit && message.is_empty() {
            self.error = Some("Write a commit message first.".into());
            cx.notify();
            return;
        }
        let (git, path) = (self.git.clone(), self.path.clone());
        self.error = None;
        self.running = Some(cx.spawn_in(window, async move |this, cx| {
            let label = format!("Commit “{}”", message.lines().next().unwrap_or_default());
            let saved = cx
                .background_executor()
                .spawn(async move {
                    std::fs::write(git.work_dir().join(&path), text)
                        .map_err(|error| anyhow::anyhow!("Could not write {path}: {error}"))?;
                    if !commit {
                        return anyhow::Ok(None);
                    }
                    let recorder = Recorder::start(git.work_dir());
                    git.commit_only(&message, &[path])?;
                    // Undoing the commit brings the edit back staged.
                    Ok(recorder.finish(ResetMode::Soft))
                })
                .await;
            this.update(cx, |this, cx| {
                this.running = None;
                match saved {
                    Ok(recorded) => {
                        let step = undo::record(cx, recorded, label);
                        cx.emit(FileEditEvent::Saved {
                            committed: commit,
                            step,
                        });
                    }
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

impl Render for FileEditForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let (can_save, running) = (self.can_save(), self.running.is_some());
        let editor = match &self.loaded {
            None => div()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(color::TEXT_MUTED))
                .child("Reading the file…")
                .into_any_element(),
            Some(Err(error)) => div()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(color::DIFF_REMOVED))
                .child(error.clone())
                .into_any_element(),
            Some(Ok(())) => Editor::new(&self.editor).into_any_element(),
        };
        div()
            .v_flex()
            .gap(px(12.))
            .child(
                div()
                    .h(EDITOR_HEIGHT)
                    .rounded(theme::RADIUS)
                    .border_1()
                    .border_color(rgb(color::BORDER))
                    .overflow_hidden()
                    .font_family(mono.clone())
                    .child(div().size_full().v_flex().child(editor)),
            )
            .child(
                div()
                    .v_flex()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child("Commit message, for Save and commit"),
                    )
                    .child(Input::new(&self.message)),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .font_family(mono)
                        .text_size(theme::SMALL_TEXT_SIZE)
                        .text_color(rgb(color::DIFF_REMOVED))
                        .child(error),
                )
            })
            // The buttons live here rather than in the dialog's footer, so
            // they follow the file being read and saved.
            .child(
                div()
                    .h_flex()
                    .justify_end()
                    .gap(px(8.))
                    .pb(px(4.))
                    .child(
                        Button::new("edit-file-cancel")
                            .ghost()
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("edit-file-save")
                            .label("Save")
                            .tooltip("Save to the working tree; commit it later")
                            .disabled(!can_save)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save(false, window, cx)),
                            ),
                    )
                    .child(
                        Button::new("edit-file-commit")
                            .primary()
                            .label("Save and commit")
                            .tooltip("Commit this file alone, so the pull request shows it")
                            .disabled(!can_save)
                            .loading(running)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save(true, window, cx)),
                            ),
                    ),
            )
    }
}

impl Review {
    /// Opens the dialog to edit `path` of the pull request's head branch,
    /// which must be checked out.
    pub(super) fn open_file_edit(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Subject::PullRequest(subject) = &self.subject else {
            return;
        };
        let Some(dir) = self.repository.work_dir() else {
            return;
        };
        if !subject.tips.checked_out {
            return;
        }
        let branch = subject.pull_request.head.branch.clone();
        let form = cx.new(|cx| FileEditForm::new(Git::new(dir), path.clone(), window, cx));
        let subscription = cx.subscribe_in(&form, window, |_, _, event, window, cx| {
            let FileEditEvent::Saved { committed, step } = event;
            window.close_dialog(cx);
            cx.emit(ReviewEvent::FileEdited {
                committed: *committed,
                step: *step,
            });
        });
        if let Subject::PullRequest(subject) = &mut self.subject {
            subject.file_edit_dialog = Some(subscription);
        }
        let review = cx.entity().downgrade();
        let title: SharedString = format!("Edit {path} on {branch}").into();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title(title.clone())
                .w(px(900.))
                .child(form.clone())
                .on_close({
                    let review = review.clone();
                    move |_, _, cx| {
                        review
                            .update(cx, |this, _| {
                                if let Subject::PullRequest(subject) = &mut this.subject {
                                    subject.file_edit_dialog = None;
                                }
                            })
                            .ok();
                    }
                })
        });
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use gitez_command::Git;
    use gpui_kit::{TestAppContext, VisualTestContext, px, size};

    use super::{FileEditEvent, FileEditForm};

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).expect("utf-8")
    }

    #[gpui_kit::test]
    fn saving_and_committing_commits_only_the_edited_file(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        git(root, &["init", "-q", "-b", "feature"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["config", "user.email", "test@example.com"]);
        std::fs::write(root.join("a.txt"), "one\n").expect("write");
        std::fs::write(root.join("b.txt"), "b\n").expect("write");
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "start"]);
        // Something else staged stays out of the commit.
        std::fs::write(root.join("b.txt"), "b, staged\n").expect("write");
        git(root, &["add", "b.txt"]);

        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::init(cx);
        });
        let git_ = Git::new(root);
        let window = cx.open_window(size(px(1000.), px(800.)), |window, cx| {
            FileEditForm::new(git_, "a.txt".to_owned(), window, cx)
        });
        let form = window
            .update(cx, |_, _, cx| cx.entity())
            .expect("window open");
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        let text = form.read_with(&cx, |form, cx| form.editor.read(cx).value().to_string());
        assert_eq!(text, "one\n");

        let saved = std::rc::Rc::new(std::cell::Cell::new(None));
        let _subscription = cx.update(|_, cx| {
            let saved = saved.clone();
            cx.subscribe(&form, move |_, event: &FileEditEvent, _| {
                let FileEditEvent::Saved { committed, .. } = event;
                saved.set(Some(*committed));
            })
        });
        form.update_in(&mut cx, |form, window, cx| {
            form.editor
                .update(cx, |editor, cx| editor.set_value("one\ntwo\n", window, cx));
            form.message
                .update(cx, |message, cx| message.set_value("Add two", window, cx));
            form.save(true, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(saved.get(), Some(true));
        assert_eq!(git(root, &["log", "-1", "--format=%s"]).trim(), "Add two");
        assert_eq!(
            git(root, &["show", "--format=", "--name-only"]).trim(),
            "a.txt"
        );
        assert_eq!(git(root, &["show", "HEAD:a.txt"]), "one\ntwo\n");
        assert_eq!(git(root, &["status", "--porcelain"]).trim(), "M  b.txt");
    }
}
