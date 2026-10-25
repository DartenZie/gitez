//! Starting on a repository GitEZ cannot open yet: cloning one, or creating
//! a new one.

use std::path::{Path, PathBuf};

use gitez_command::Git;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{CloneRepository, InitRepository, Workspace};
use crate::theme::color;

/// The folder a clone of `url` gets by default, as `git clone` names it:
/// the last part of the URL without `.git`.
pub fn clone_name(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    let last = url.rsplit(['/', ':', '\\']).next().unwrap_or(url);
    last.strip_suffix(".git").unwrap_or(last).to_owned()
}

/// `path` with a leading `~` standing for the home folder.
fn expand_home(path: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match (path.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(path),
    }
}

/// What a filled-in clone form asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneRequest {
    pub url: String,
    /// The folder to clone into; it must not exist yet, or be empty.
    pub dest: PathBuf,
}

/// What the form asks of the window.
pub enum CloneFormEvent {
    /// Enter was pressed in a field.
    Submit,
}

pub struct CloneForm {
    url: Entity<InputState>,
    /// The folder the clone goes in.
    folder: Entity<InputState>,
    /// The clone's own folder, inside `folder`.
    name: Entity<InputState>,
    /// The name was typed, so it no longer follows the URL. Setting it
    /// from the URL sends no change event, so only typing sets this.
    name_typed: bool,
    /// Why the form cannot be used, shown on submitting.
    error: Option<SharedString>,
    _events: [Subscription; 3],
}

impl EventEmitter<CloneFormEvent> for CloneForm {}

impl CloneForm {
    /// `folder` is where the clone goes unless another is chosen.
    pub fn new(folder: &Path, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let url = cx.new(|cx| {
            InputState::new(window, cx).placeholder("https://github.com/owner/repository.git")
        });
        let folder =
            cx.new(|cx| InputState::new(window, cx).default_value(folder.display().to_string()));
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("repository"));
        let url_events =
            cx.subscribe_in(&url, window, |this, url, event, window, cx| match event {
                InputEvent::PressEnter { .. } => cx.emit(CloneFormEvent::Submit),
                InputEvent::Change if !this.name_typed => {
                    let name = clone_name(&url.read(cx).value());
                    this.name
                        .update(cx, |input, cx| input.set_value(name, window, cx));
                }
                _ => {}
            });
        let folder_events = cx.subscribe_in(&folder, window, |_, _, event, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                cx.emit(CloneFormEvent::Submit);
            }
        });
        let name_events = cx.subscribe_in(&name, window, |this, _, event, _, cx| match event {
            InputEvent::PressEnter { .. } => cx.emit(CloneFormEvent::Submit),
            InputEvent::Change => this.name_typed = true,
            _ => {}
        });
        Self {
            url,
            folder,
            name,
            name_typed: false,
            error: None,
            _events: [url_events, folder_events, name_events],
        }
    }

    /// The URL box, focused when the dialog opens.
    pub fn url_input(&self) -> &Entity<InputState> {
        &self.url
    }

    /// Asks for the folder to clone into.
    fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.folder.update(cx, |input, cx| {
                    input.set_value(path.display().to_string(), window, cx)
                });
            })
            .ok();
        })
        .detach();
    }

    /// What the form asks for; `None` after showing why when it cannot be
    /// used. Whether the folder can take the clone is left to git.
    pub fn submit(&mut self, cx: &mut Context<Self>) -> Option<CloneRequest> {
        let url = self.url.read(cx).value().trim().to_owned();
        let folder = self.folder.read(cx).value().trim().to_owned();
        let mut name = self.name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            name = clone_name(&url);
        }
        let problem = if url.is_empty() {
            Some("Give the URL to clone.")
        } else if folder.is_empty() {
            Some("Choose the folder to clone into.")
        } else if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            Some("Give the clone a folder name, without slashes.")
        } else {
            None
        };
        if let Some(problem) = problem {
            self.error = Some(problem.into());
            cx.notify();
            return None;
        }
        Some(CloneRequest {
            url,
            dest: expand_home(&folder).join(name),
        })
    }
}

impl Render for CloneForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .gap(px(14.))
            .pb(px(4.))
            .child(field("URL", Input::new(&self.url)))
            .child(field(
                "Folder",
                div()
                    .h_flex()
                    .gap(px(8.))
                    .child(div().flex_1().child(Input::new(&self.folder)))
                    .child(
                        Button::new("clone-choose-folder")
                            .label("Choose…")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.choose_folder(window, cx)),
                            ),
                    ),
            ))
            .child(field("Name", Input::new(&self.name)))
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .text_size(px(12.5))
                        .text_color(rgb(color::DIFF_REMOVED))
                        .child(error),
                )
            })
    }
}

/// A caption above a control.
fn field(caption: &'static str, control: impl IntoElement) -> impl IntoElement {
    div()
        .v_flex()
        .gap(px(6.))
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_MUTED))
                .child(caption),
        )
        .child(control)
}

impl Workspace {
    /// Where a new clone or repository goes by default: beside the open
    /// repository, or in the home folder.
    fn default_folder(&self) -> PathBuf {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.path.parent().map(Path::to_path_buf))
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    /// Asks for a repository to clone.
    pub(super) fn clone_repository(
        &mut self,
        _: &CloneRepository,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let folder = self.default_folder();
        let form = cx.new(|cx| CloneForm::new(&folder, window, cx));
        let subscription =
            cx.subscribe_in(&form, window, |this, _, event, window, cx| match event {
                CloneFormEvent::Submit => this.submit_clone_form(window, cx),
            });
        self.clone_form = Some((form.clone(), subscription));
        let workspace = cx.entity().downgrade();
        let focus_url = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if focus_url.replace(false) {
                let url = form.read(cx).url_input().focus_handle(cx);
                window.focus(&url, cx);
            }
            let submit = workspace.clone();
            dialog
                .title("Clone repository")
                .w(px(520.))
                .child(form.clone())
                .footer(
                    div()
                        .h_flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            Button::new("clone-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("clone-submit")
                                .primary()
                                .label("Clone")
                                .on_click(move |_, window, cx| {
                                    submit
                                        .update(cx, |this, cx| this.submit_clone_form(window, cx))
                                        .ok();
                                }),
                        ),
                )
                .on_close({
                    let workspace = workspace.clone();
                    move |_, _, cx| {
                        workspace.update(cx, |this, _| this.clone_form = None).ok();
                    }
                })
        });
    }

    /// Clones what the open form describes, then opens the clone.
    fn submit_clone_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((form, _)) = &self.clone_form else {
            return;
        };
        let Some(CloneRequest { url, dest }) = form.update(cx, |form, cx| form.submit(cx)) else {
            return;
        };
        window.close_dialog(cx);
        self.clone_form = None;
        let run = cx.background_executor().spawn({
            let (url, dest) = (url.clone(), dest.clone());
            async move { Git::clone_repository(&url, &dest) }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = run.await;
            this.update_in(cx, |this, window, cx| {
                this.cloning = None;
                match result {
                    Ok(git) => {
                        let name = git
                            .work_dir()
                            .file_name()
                            .map_or_else(|| url.clone(), |name| name.to_string_lossy().into());
                        window
                            .push_notification(Notification::success(format!("Cloned {name}")), cx);
                        this.open(git.work_dir().to_path_buf(), window, cx);
                    }
                    Err(error) => window.push_notification(
                        Notification::error(format!("Could not clone: {error}")),
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        });
        self.cloning = Some((dest, task));
        cx.notify();
    }

    /// Asks for a folder and makes it a new repository, then opens it. A
    /// folder that is a repository already is just opened.
    pub(super) fn init_repository(
        &mut self,
        _: &InitRepository,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Create Repository".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            if let Some(dir) = paths.into_iter().next() {
                this.update_in(cx, |this, window, cx| {
                    this.create_repository(dir, window, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    /// Makes folder `dir` a new repository and opens it; one that is a
    /// repository already is just opened.
    fn create_repository(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let run = cx.background_executor().spawn({
            let dir = dir.clone();
            async move {
                if dir.join(".git").exists() {
                    return Ok(false);
                }
                Git::init_repository(&dir).map(|_| true)
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = run.await;
            this.update_in(cx, |this, window, cx| {
                let name = dir.file_name().map_or_else(
                    || dir.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                );
                match result {
                    Ok(created) => {
                        let notification = if created {
                            Notification::success(format!("Created a repository in {name}"))
                        } else {
                            Notification::info(format!("{name} is a repository already"))
                        };
                        window.push_notification(notification, cx);
                        this.open(dir, window, cx);
                    }
                    Err(error) => window.push_notification(
                        Notification::error(format!(
                            "Could not create a repository in {name}: {error}"
                        )),
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{clone_name, expand_home};

    #[test]
    fn a_clone_is_named_after_the_end_of_its_url() {
        assert_eq!(
            clone_name("https://github.com/zed-industries/zed.git"),
            "zed"
        );
        assert_eq!(clone_name("https://github.com/owner/repo/"), "repo");
        assert_eq!(clone_name("git@github.com:owner/tool.git"), "tool");
        assert_eq!(clone_name("host:project"), "project");
        assert_eq!(clone_name("/srv/git/site.git"), "site");
        assert_eq!(clone_name(""), "");
    }

    #[test]
    fn a_tilde_is_the_home_folder() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/code"), home.join("code"));
        assert_eq!(expand_home("~other/code"), PathBuf::from("~other/code"));
        assert_eq!(expand_home("/srv"), PathBuf::from("/srv"));
    }
}
