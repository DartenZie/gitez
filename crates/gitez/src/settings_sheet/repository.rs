//! "In this repository": the setup command new worktrees run first
//! (`gitez.setupCommand` in the repository's git config, written with the
//! git CLI), the Claude account its agents run with (`gitez.agentAccount`,
//! which the user's global git config may also set, through an
//! `includeIf "gitdir:…"` rule say), and where the threads' worktrees
//! live.

use std::path::PathBuf;

use gitez_command::Git;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use gitez_agent_thread::Account;
use gitez_settings::ACCOUNT_KEY;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};

use super::accounts::account_names;
use super::edit::guess_setup_command;
use super::{SAVE_AFTER, SettingsSheet, card, error_text, kv_row, muted};
use crate::theme::color;

/// The git config key of the setup command; the supervisor reads it.
pub(super) const SETUP_COMMAND_KEY: &str = "gitez.setupCommand";

pub(super) struct RepositoryForm {
    work_dir: PathBuf,
    setup: Entity<InputState>,
    /// Follows `setup`; dropped with the form.
    _setup_changes: Subscription,
    /// The command in git config, once read; `None` when unset.
    saved: Option<Option<String>>,
    /// What GitEZ suggests, shown but never run or saved by itself.
    guess: Option<String>,
    error: Option<SharedString>,
    /// Counts edits, so only the last one is saved.
    generation: u64,
    saving: bool,
    /// `<data dir>/worktrees/<repository>`, and whether it exists yet.
    worktrees: Option<(PathBuf, bool)>,
    /// The account named in git config, once read: `None` when unset,
    /// else the name and whether the repository's own config sets it.
    account: Option<Option<(String, bool)>>,
    account_saving: bool,
    account_error: Option<SharedString>,
    _loading: Option<Task<()>>,
}

/// What is read about the repository in the background.
struct Read {
    setup: Option<String>,
    account: Option<(String, bool)>,
    guess: Option<String>,
    error: Option<String>,
    worktrees_exist: bool,
}

impl SettingsSheet {
    /// Builds the form for the repository open now, if it changed, and
    /// reads its setup command.
    pub(super) fn sync_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let current = model.current_repository().map(|current| {
            (
                current.work_dir.clone(),
                current.common_dir.clone(),
                current.name.clone(),
            )
        });
        let data_dir = model.data_dir().map(PathBuf::from);
        let same = match (&self.repository, &current) {
            (Some(form), Some((work_dir, _, _))) => form.work_dir == *work_dir,
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        let Some((work_dir, common_dir, name)) = current else {
            self.repository = None;
            return;
        };
        let setup = cx.new(|cx| InputState::new(window, cx));
        let setup_changes =
            cx.subscribe_in(&setup, window, |this, _, event: &InputEvent, _, cx| {
                if let InputEvent::Change = event {
                    this.setup_typed(cx);
                }
            });
        let worktrees = data_dir.map(|data_dir| {
            data_dir
                .join("worktrees")
                .join(gitez_agent_thread::repository_key(&name, &common_dir))
        });
        let read = cx.background_executor().spawn({
            let (work_dir, worktrees) = (work_dir.clone(), worktrees.clone());
            async move {
                let (setup, account, error) = match gitez_git::Repository::open(&work_dir) {
                    Ok(repository) => (
                        repository.config_string(SETUP_COMMAND_KEY),
                        repository.config_string_with_origin(ACCOUNT_KEY),
                        None,
                    ),
                    Err(error) => (
                        None,
                        None,
                        Some(format!("{:#}", anyhow::Error::from(error))),
                    ),
                };
                Read {
                    setup,
                    account,
                    guess: guess_setup_command(|file| work_dir.join(file).exists()),
                    error,
                    worktrees_exist: worktrees.is_some_and(|dir| dir.is_dir()),
                }
            }
        });
        let loading = cx.spawn_in(window, async move |this, cx| {
            let read = read.await;
            this.update_in(cx, |this, window, cx| {
                let Some(form) = &mut this.repository else {
                    return;
                };
                let text = read.setup.clone().unwrap_or_default();
                let placeholder = match &read.guess {
                    Some(guess) => guess.clone(),
                    None => "None: nothing runs before the agent".to_owned(),
                };
                form.setup.update(cx, |input, cx| {
                    input.set_placeholder(placeholder, window, cx);
                    input.set_value(text, window, cx);
                });
                form.saved = Some(read.setup);
                form.account = Some(read.account);
                form.guess = read.guess;
                form.error = read.error.map(|error| {
                    format!("Could not read the repository’s git config: {error}").into()
                });
                if let Some((_, exists)) = &mut form.worktrees {
                    *exists = read.worktrees_exist;
                }
                form._loading = None;
                cx.notify();
            })
            .ok();
        });
        self.repository = Some(RepositoryForm {
            work_dir,
            setup,
            saved: None,
            guess: None,
            error: None,
            generation: 0,
            saving: false,
            worktrees: worktrees.map(|dir| (dir, false)),
            account: None,
            account_saving: false,
            account_error: None,
            _loading: Some(loading),
            _setup_changes: setup_changes,
        });
    }

    /// Saves the setup command in git config once typing pauses; an empty
    /// one is removed.
    fn setup_typed(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &mut self.repository else {
            return;
        };
        let Some(saved) = &form.saved else {
            // Still reading what is set.
            return;
        };
        let typed = form.setup.read(cx).value().trim().to_owned();
        let typed = (!typed.is_empty()).then_some(typed);
        if typed == *saved {
            return;
        }
        form.generation += 1;
        let generation = form.generation;
        let git = Git::new(&form.work_dir);
        form.saving = true;
        // Detached, so closing the sheet right after typing still saves.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_AFTER).await;
            let latest = this
                .read_with(cx, |this, _| {
                    this.repository
                        .as_ref()
                        .is_some_and(|form| form.generation == generation)
                })
                .unwrap_or(true);
            if !latest {
                return;
            }
            let value = typed.clone();
            let saved = cx
                .background_executor()
                .spawn(async move { git.set_config(SETUP_COMMAND_KEY, value.as_deref()) })
                .await;
            this.update(cx, |this, cx| {
                let Some(form) = &mut this.repository else {
                    return;
                };
                if form.generation == generation {
                    form.saving = false;
                }
                match saved {
                    Ok(()) => {
                        form.saved = Some(typed);
                        form.error = None;
                    }
                    Err(error) => {
                        form.error = Some(
                            format!(
                                "Could not save the setup command: {:#}",
                                anyhow::Error::from(error)
                            )
                            .into(),
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Makes `name` the account of the repository's agents, in its own git
    /// config. The default account is written only when the user's global
    /// config names another; otherwise the key is removed.
    pub(super) fn choose_account(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(form) = &mut self.repository else {
            return;
        };
        let work_dir = form.work_dir.clone();
        form.account_saving = true;
        form.account_error = None;
        let saved = cx.background_executor().spawn(async move {
            let git = Git::new(&work_dir);
            if Account::is_default_name(&name) {
                git.set_config(ACCOUNT_KEY, None)?;
                let elsewhere = gitez_git::Repository::open(&work_dir)
                    .ok()
                    .and_then(|repository| repository.config_string(ACCOUNT_KEY));
                if elsewhere.is_some_and(|other| !Account::is_default_name(&other)) {
                    git.set_config(ACCOUNT_KEY, Some(Account::DEFAULT_NAME))?;
                }
            } else {
                git.set_config(ACCOUNT_KEY, Some(&name))?;
            }
            let read = gitez_git::Repository::open(&work_dir)
                .ok()
                .and_then(|repository| repository.config_string_with_origin(ACCOUNT_KEY));
            Ok::<_, gitez_command::Error>(read)
        });
        cx.spawn(async move |this, cx| {
            let saved = saved.await;
            this.update(cx, |this, cx| {
                let Some(form) = &mut this.repository else {
                    return;
                };
                form.account_saving = false;
                match saved {
                    Ok(read) => form.account = Some(read),
                    Err(error) => {
                        form.account_error = Some(
                            format!(
                                "Could not save the account: {:#}",
                                anyhow::Error::from(error)
                            )
                            .into(),
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// The "Claude account" row: the account the repository's threads
    /// start with, where that comes from, and a picker.
    fn render_repository_account(&self, form: &RepositoryForm, cx: &mut Context<Self>) -> Div {
        let names = account_names(self.model.read(cx));
        let settings = self.model.read(cx).settings().clone();
        let view = cx.entity().downgrade();
        let effective = form.account.as_ref().map(|read| match read {
            Some((name, _)) if Account::is_default_name(name) => Account::DEFAULT_NAME.to_owned(),
            Some((name, _)) => name.clone(),
            None => Account::DEFAULT_NAME.to_owned(),
        });
        let unknown = effective.as_ref().is_some_and(|name| {
            !Account::is_default_name(name) && settings.account_named(name).is_none()
        });
        let note: SharedString = match &form.account {
            None => "Reading…".into(),
            Some(None) => {
                "Not set: new threads run with the default account. Kept in this repository’s \
                 git config as gitez.agentAccount."
                    .into()
            }
            Some(Some((_, true))) => {
                "Set in this repository’s git config (gitez.agentAccount). Threads keep the \
                 account they started with."
                    .into()
            }
            Some(Some((_, false))) => {
                "From your global git config (gitez.agentAccount), not this repository’s. \
                 Picking one here sets it for this repository only."
                    .into()
            }
        };
        let chosen = effective.clone().unwrap_or_default();
        let picker = Button::new("repository-account")
            .compact()
            .h(px(28.))
            .px(px(8.))
            .min_w(px(160.))
            .text_size(px(12.))
            .dropdown_caret(true)
            .loading(form.account_saving)
            .disabled(form.account.is_none())
            .child(
                div()
                    .flex_1()
                    .text_left()
                    .child(effective.clone().unwrap_or_else(|| "…".to_owned())),
            )
            .dropdown_menu(move |mut menu, _, _| {
                for name in &names {
                    let view = view.clone();
                    let pick = name.clone();
                    menu = menu.item(
                        PopupMenuItem::new(name.clone())
                            .checked(name.eq_ignore_ascii_case(&chosen))
                            .on_click(move |_, _, cx| {
                                view.update(cx, |this, cx| this.choose_account(pick.clone(), cx))
                                    .ok();
                            }),
                    );
                }
                menu
            });
        kv_row(
            "Claude account",
            div()
                .v_flex()
                .gap(px(4.))
                .child(picker)
                .child(muted(note).text_size(px(11.5)))
                .when(unknown, |this| {
                    this.child(error_text(format!(
                        "Settings have no account “{}”: new threads cannot start here until \
                         you add it or pick another.",
                        effective.clone().unwrap_or_default()
                    )))
                })
                .when_some(form.account_error.clone(), |this, error| {
                    this.child(error_text(error))
                }),
        )
    }

    pub(super) fn render_repository(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let form = self.repository.as_ref()?;
        let mono = cx.theme().mono_font_family.clone();
        let typed_empty = form.setup.read(cx).value().trim().is_empty();
        let note = match (&form.guess, typed_empty) {
            (Some(guess), true) => format!(
                "Runs in each new worktree first. Empty: nothing runs; type {guess} to use the \
                 suggestion."
            ),
            _ => "Runs in each new worktree first, before the agent. Kept in this repository’s \
                  git config."
                .to_owned(),
        };
        let setup = div()
            .v_flex()
            .gap(px(4.))
            .child(
                Input::new(&form.setup)
                    .small()
                    .font_family(mono.clone())
                    .bg(rgb(color::FIELD)),
            )
            .child(muted(note).text_size(px(11.5)))
            .when_some(form.error.clone(), |this, error| {
                this.child(error_text(error))
            });
        let worktrees = form.worktrees.as_ref().map(|(dir, exists)| {
            let dir = dir.clone();
            let exists = *exists;
            kv_row(
                "Worktrees",
                div()
                    .h_flex()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .pt(px(4.))
                            .font_family(mono)
                            .text_size(px(12.))
                            .text_color(rgb(color::TEXT_SECONDARY))
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(crate::recent::display(&dir)),
                    )
                    .child(
                        Button::new("worktrees-reveal")
                            .small()
                            .label(if cfg!(target_os = "macos") {
                                "Reveal"
                            } else {
                                "Show"
                            })
                            .disabled(!exists)
                            .tooltip(if exists {
                                "Show the folder"
                            } else {
                                "Made with this repository’s first thread"
                            })
                            .on_click(move |_, _, cx| cx.reveal_path(&dir)),
                    ),
            )
            .border_b_0()
        });
        let account = self.render_repository_account(form, cx);
        Some(
            card()
                .child(kv_row("Setup", setup))
                .child(account)
                .children(worktrees),
        )
    }

    /// For tests: whether the setup command is still being read or saved.
    #[cfg(test)]
    pub(super) fn repository_busy(&self) -> bool {
        self.repository
            .as_ref()
            .is_some_and(|form| form._loading.is_some() || form.saving || form.account_saving)
    }
}
