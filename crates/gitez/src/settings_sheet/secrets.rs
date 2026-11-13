//! Secrets: environment variables whose values are in the OS keychain.
//! The table lists every secret in the settings; a value is written once
//! (Add, Replace) and never read back to be shown. Whether each value is
//! still in the keychain is checked without reading it, so a secret whose
//! keychain item went missing says so and can be set again.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use gitez_secret::SecretStore;
use gitez_settings::{SecretEntry, SecretScope, Settings, is_env_name};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{SettingsSheet, card, description, error_text, heading, keychain_name, muted};
use crate::theme::color;

/// Columns of the table: name, scope, the default switch, actions.
const SCOPE_WIDTH: f32 = 150.;
const DEFAULT_WIDTH: f32 = 92.;
const ACTIONS_WIDTH: f32 = 64.;

pub(super) struct Secrets {
    name: Entity<InputState>,
    value: Entity<InputState>,
    /// A new secret is for all repositories (else for the open one).
    all_repositories: bool,
    by_default: bool,
    /// Why the secret cannot be added.
    error: Option<SharedString>,
    adding: bool,
    /// Whether each listed secret's value is in the keychain, by account.
    presence: HashMap<String, Presence>,
    /// The secrets `presence` was checked for.
    checked: Option<Vec<SecretEntry>>,
    _checking: Option<Task<()>>,
    /// The row being replaced or deleted.
    edit: Option<RowEdit>,
    /// The account being written, with the keychain busy.
    busy: Option<String>,
    /// What went wrong with a row, by account.
    row_error: Option<(String, SharedString)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Presence {
    Stored,
    Missing,
    Unknown(SharedString),
}

enum RowEdit {
    /// A write-only field for the new value.
    Replace {
        account: String,
        value: Entity<InputState>,
    },
    /// "Delete NAME?" with Cancel and Delete.
    Delete { account: String },
}

impl Secrets {
    pub(super) fn new(
        window: &mut Window,
        cx: &mut Context<SettingsSheet>,
        subscriptions: &mut Vec<Subscription>,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("NAME"));
        let value = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("Stored in the keychain")
        });
        subscriptions.push(cx.subscribe_in(
            &name,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    this.secrets.error = this.name_problem(cx);
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.add_secret(window, cx),
                _ => {}
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &value,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.add_secret(window, cx);
                }
            },
        ));
        Self {
            name,
            value,
            all_repositories: false,
            by_default: false,
            error: None,
            adding: false,
            presence: HashMap::new(),
            checked: None,
            _checking: None,
            edit: None,
            busy: None,
            row_error: None,
        }
    }

    /// For tests: whether the keychain has each listed secret's value.
    #[cfg(test)]
    pub(super) fn presence(&self, account: &str) -> Option<&Presence> {
        self.presence.get(account)
    }
}

/// "All repositories", "This repository", or another repository's name.
fn scope_label(scope: &SecretScope, current: Option<&Path>) -> SharedString {
    match scope {
        SecretScope::All => "All repositories".into(),
        SecretScope::Repository(dir) if Some(dir.as_path()) == current => "This repository".into(),
        SecretScope::Repository(dir) => format!("Only {}", repository_name(dir)).into(),
    }
}

/// The folder name of the repository whose git directory is `common_dir`.
fn repository_name(common_dir: &Path) -> String {
    let dir = if common_dir.file_name().is_some_and(|name| name == ".git") {
        common_dir.parent().unwrap_or(common_dir)
    } else {
        common_dir
    };
    dir.file_name()
        .map(|name| name.to_string_lossy().trim_end_matches(".git").to_owned())
        .unwrap_or_else(|| dir.display().to_string())
}

impl SettingsSheet {
    fn store(&self, cx: &App) -> Arc<dyn SecretStore> {
        self.model.read(cx).secret_store().clone()
    }

    fn current_common_dir(&self, cx: &App) -> Option<std::path::PathBuf> {
        self.model
            .read(cx)
            .current_repository()
            .map(|current| current.common_dir.clone())
    }

    /// The scope a new secret gets.
    fn new_scope(&self, cx: &App) -> SecretScope {
        match self.current_common_dir(cx) {
            Some(dir) if !self.secrets.all_repositories => SecretScope::Repository(dir),
            _ => SecretScope::All,
        }
    }

    /// What is wrong with the typed name, once something is typed.
    fn name_problem(&self, cx: &App) -> Option<SharedString> {
        let name = self.secrets.name.read(cx).value().trim().to_owned();
        if name.is_empty() || is_env_name(&name) {
            None
        } else {
            Some(
                format!(
                    "“{name}” cannot be an environment variable name: use capital letters, digits \
                     and _, not starting with a digit."
                )
                .into(),
            )
        }
    }

    /// Checks in the background which listed secrets have their value in
    /// the keychain, when the list changed.
    pub(super) fn check_secrets(&mut self, cx: &mut Context<Self>) {
        let entries = self.model.read(cx).settings().secrets.clone();
        if self.secrets.checked.as_ref() == Some(&entries) {
            return;
        }
        self.secrets.checked = Some(entries.clone());
        let store = self.store(cx);
        let checked = cx.background_executor().spawn(async move {
            entries
                .iter()
                .map(|entry| {
                    let account = entry.account();
                    let presence = match store.contains(&account) {
                        Ok(true) => Presence::Stored,
                        Ok(false) => Presence::Missing,
                        Err(error) => Presence::Unknown(error.to_string().into()),
                    };
                    (account, presence)
                })
                .collect::<HashMap<_, _>>()
        });
        self.secrets._checking = Some(cx.spawn(async move |this, cx| {
            let presence = checked.await;
            this.update(cx, |this, cx| {
                this.secrets.presence = presence;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Stores the typed value in the keychain, then lists the secret.
    fn add_secret(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.secrets.adding {
            return;
        }
        let name = self.secrets.name.read(cx).value().trim().to_owned();
        let value = self.secrets.value.read(cx).value().to_string();
        let scope = self.new_scope(cx);
        let settings = self.model.read(cx).settings().clone();
        let problem = if name.is_empty() {
            Some("Type a name, such as GITHUB_TOKEN.".into())
        } else if let Some(problem) = self.name_problem(cx) {
            Some(problem)
        } else if settings
            .secrets
            .iter()
            .any(|entry| entry.name == name && entry.scope == scope)
        {
            Some(
                format!(
                    "{name} exists already for {}. Replace its value in the table instead.",
                    scope_label(&scope, self.current_common_dir(cx).as_deref()).to_lowercase()
                )
                .into(),
            )
        } else if value.is_empty() {
            Some("Type the value. It goes to the keychain and is not shown again.".into())
        } else {
            None
        };
        if let Some(problem) = problem {
            self.secrets.error = Some(problem);
            cx.notify();
            return;
        }
        let entry = SecretEntry {
            name: name.clone(),
            scope,
            default: self.secrets.by_default,
        };
        let account = entry.account();
        let store = self.store(cx);
        let model = self.model.clone();
        self.secrets.adding = true;
        self.secrets.error = None;
        cx.notify();
        let stored = cx
            .background_executor()
            .spawn(async move { store.set(&account, &value) });
        // Detached: the secret is listed even if the sheet closes meanwhile.
        cx.spawn_in(window, async move |this, cx| {
            let stored = stored.await;
            if stored.is_ok() {
                model.update(cx, |model, cx| {
                    model.update_settings(|settings| settings.secrets.push(entry), cx);
                });
            }
            this.update_in(cx, |this, window, cx| {
                this.secrets.adding = false;
                match stored {
                    Ok(()) => {
                        for input in [&this.secrets.name, &this.secrets.value] {
                            input.update(cx, |input, cx| input.set_value("", window, cx));
                        }
                        this.secrets.error = None;
                    }
                    Err(error) => {
                        this.secrets.error = Some(
                            format!("Could not store {name} in {}: {error}", keychain_name())
                                .into(),
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn start_replace(&mut self, account: String, window: &mut Window, cx: &mut Context<Self>) {
        let value = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("New value, stored in the keychain")
        });
        self._subscriptions.push(cx.subscribe_in(
            &value,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.save_replacement(window, cx),
                InputEvent::Change => {
                    this.secrets.row_error = None;
                    cx.notify();
                }
                _ => {}
            },
        ));
        value.update(cx, |input, cx| input.focus(window, cx));
        self.secrets.row_error = None;
        self.secrets.edit = Some(RowEdit::Replace { account, value });
        cx.notify();
    }

    fn save_replacement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(RowEdit::Replace { account, value }) = &self.secrets.edit else {
            return;
        };
        if self.secrets.busy.is_some() {
            return;
        }
        let account = account.clone();
        let text = value.read(cx).value().to_string();
        if text.is_empty() {
            self.secrets.row_error = Some((account, "Type the new value.".into()));
            cx.notify();
            return;
        }
        let store = self.store(cx);
        self.secrets.busy = Some(account.clone());
        cx.notify();
        let stored = cx.background_executor().spawn({
            let account = account.clone();
            async move { store.set(&account, &text) }
        });
        cx.spawn_in(window, async move |this, cx| {
            let stored = stored.await;
            this.update_in(cx, |this, _, cx| {
                this.secrets.busy = None;
                match stored {
                    Ok(()) => {
                        this.secrets.edit = None;
                        this.secrets.row_error = None;
                        this.secrets.presence.insert(account, Presence::Stored);
                    }
                    Err(error) => {
                        this.secrets.row_error = Some((
                            account,
                            format!("Could not store the value in {}: {error}", keychain_name())
                                .into(),
                        ));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Deletes the value from the keychain, then the secret from the list.
    fn delete_secret(&mut self, account: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.secrets.busy.is_some() {
            return;
        }
        let store = self.store(cx);
        let model = self.model.clone();
        self.secrets.busy = Some(account.clone());
        cx.notify();
        let deleted = cx.background_executor().spawn({
            let account = account.clone();
            async move { store.delete(&account) }
        });
        cx.spawn_in(window, async move |this, cx| {
            let deleted = deleted.await;
            if deleted.is_ok() {
                let gone = account.clone();
                model.update(cx, |model, cx| {
                    model.update_settings(
                        |settings| settings.secrets.retain(|entry| entry.account() != gone),
                        cx,
                    );
                });
            }
            this.update_in(cx, |this, _, cx| {
                this.secrets.busy = None;
                match deleted {
                    Ok(()) => {
                        this.secrets.edit = None;
                        this.secrets.row_error = None;
                        this.secrets.presence.remove(&account);
                    }
                    Err(error) => {
                        this.secrets.row_error = Some((
                            account,
                            format!(
                                "Could not delete the value from {}: {error}",
                                keychain_name()
                            )
                            .into(),
                        ));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_secret_default(&mut self, account: &str, on: bool, cx: &mut Context<Self>) {
        let account = account.to_owned();
        self.model.update(cx, |model, cx| {
            model.update_settings(
                |settings| {
                    for entry in &mut settings.secrets {
                        if entry.account() == account {
                            entry.default = on;
                        }
                    }
                },
                cx,
            );
        });
    }

    /// Moves a secret to `scope`: its value to the new keychain account
    /// (read here, never shown), then the list.
    fn move_secret(
        &mut self,
        account: String,
        scope: SecretScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings: Arc<Settings> = self.model.read(cx).settings().clone();
        let Some(entry) = settings
            .secrets
            .iter()
            .find(|entry| entry.account() == account)
            .cloned()
        else {
            return;
        };
        if entry.scope == scope || self.secrets.busy.is_some() {
            return;
        }
        let moved = SecretEntry {
            scope: scope.clone(),
            ..entry.clone()
        };
        if settings
            .secrets
            .iter()
            .any(|other| other.name == entry.name && other.scope == scope)
        {
            let current = self.current_common_dir(cx);
            self.secrets.row_error = Some((
                account,
                format!(
                    "{} exists already for {}; delete one of them first.",
                    entry.name,
                    scope_label(&scope, current.as_deref()).to_lowercase()
                )
                .into(),
            ));
            cx.notify();
            return;
        }
        let store = self.store(cx);
        let model = self.model.clone();
        self.secrets.busy = Some(account.clone());
        self.secrets.row_error = None;
        cx.notify();
        let new_account = moved.account();
        let copied = cx.background_executor().spawn({
            let (account, new_account) = (account.clone(), new_account.clone());
            async move {
                let value = store
                    .get(&account)
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "its value is not in the keychain; set it first".to_owned())?;
                store
                    .set(&new_account, &value)
                    .map_err(|error| error.to_string())?;
                store.delete(&account).map_err(|error| error.to_string())
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let copied = copied.await;
            if copied.is_ok() {
                let old = account.clone();
                model.update(cx, |model, cx| {
                    model.update_settings(
                        |settings| {
                            for entry in &mut settings.secrets {
                                if entry.account() == old {
                                    entry.scope = moved.scope.clone();
                                }
                            }
                        },
                        cx,
                    );
                });
            }
            this.update_in(cx, |this, _, cx| {
                this.secrets.busy = None;
                match copied {
                    Ok(()) => {
                        this.secrets.presence.remove(&account);
                        this.secrets.presence.insert(new_account, Presence::Stored);
                    }
                    Err(error) => {
                        this.secrets.row_error =
                            Some((account, format!("Could not move it: {error}.").into()));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl SettingsSheet {
    pub(super) fn render_secrets(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let settings = self.model.read(cx).settings().clone();
        let current = self.current_common_dir(cx);
        let count = settings.secrets.len();
        let missing = settings
            .secrets
            .iter()
            .filter(|entry| self.secrets.presence.get(&entry.account()) == Some(&Presence::Missing))
            .count();
        let summary = match (count, missing) {
            (0, _) => "None yet".to_owned(),
            (count, 0) => format!("{count} in {}", keychain_name()),
            (count, missing) => format!("{count} in {}, {missing} missing", keychain_name()),
        };
        let head = div()
            .h_flex()
            .gap(px(10.))
            .h(px(28.))
            .px(px(12.))
            .border_b_1()
            .border_color(rgb(color::BORDER))
            .text_size(px(10.5))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(color::TEXT_FAINT))
            .child(div().flex_1().child("NAME"))
            .child(div().w(px(SCOPE_WIDTH)).child("SCOPE"))
            .child(div().w(px(DEFAULT_WIDTH)).child("NEW THREADS"))
            .child(div().w(px(ACTIONS_WIDTH)));
        let mut table = card().child(head);
        if settings.secrets.is_empty() {
            table = table.child(
                div()
                    .px(px(12.))
                    .py(px(14.))
                    .border_b_1()
                    .border_color(rgb(color::ROW_DIVIDER))
                    .child(muted(
                        "No secrets yet. A thread you give a secret gets it as an environment \
                         variable; add one below.",
                    )),
            );
        }
        for (index, entry) in settings.secrets.iter().enumerate() {
            table =
                table.child(self.render_secret_row(index, entry, current.as_deref(), window, cx));
        }
        table = table.child(self.render_add_secret(current.is_some(), cx));
        div()
            .v_flex()
            .gap(px(10.))
            .child(
                div()
                    .h_flex()
                    .items_baseline()
                    .gap(px(8.))
                    .child(heading("Secrets"))
                    .child(muted(summary)),
            )
            .child(description(
                "Environment variables a thread can be given. You can replace a value but never \
                 read it back, and values stay out of the repository, the prompt and transcripts. \
                 With the Packages or Open network, a token lets the agent use that service; \
                 GitEZ tells it never to push.",
            ))
            .child(table)
    }

    fn render_secret_row(
        &self,
        index: usize,
        entry: &SecretEntry,
        current: Option<&Path>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let mono = cx.theme().mono_font_family.clone();
        let account = entry.account();
        let presence = self.secrets.presence.get(&account).cloned();
        let busy = self.secrets.busy.as_deref() == Some(account.as_str());
        let view = cx.entity().downgrade();
        let value_line = match &presence {
            Some(Presence::Missing) => div()
                .text_size(px(11.))
                .text_color(rgb(color::DIFF_REMOVED))
                .child("Not in the keychain: set its value again"),
            Some(Presence::Unknown(error)) => div()
                .text_size(px(11.))
                .text_color(rgb(color::TEXT_MUTED))
                .child(format!("Keychain not checked: {error}")),
            _ => div()
                .font_family(mono.clone())
                .text_size(px(10.5))
                .text_color(rgb(color::TEXT_MUTED))
                .child("••••••••••••"),
        };
        let name = div()
            .flex_1()
            .min_w_0()
            .v_flex()
            .gap(px(2.))
            .child(
                div()
                    .font_family(mono)
                    .text_size(px(12.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(entry.name.clone()),
            )
            .child(value_line);

        let scope_text = scope_label(&entry.scope, current);
        let movable = current.is_some()
            && match &entry.scope {
                SecretScope::All => true,
                SecretScope::Repository(dir) => Some(dir.as_path()) == current,
            };
        let scope: AnyElement = if movable && let Some(current) = current {
            let current = current.to_owned();
            let on_all = entry.scope == SecretScope::All;
            let (view, account) = (view.clone(), account.clone());
            Button::new(("secret-scope", index))
                .compact()
                .w_full()
                .h(px(28.))
                .px(px(8.))
                .text_size(px(12.))
                .dropdown_caret(true)
                .disabled(busy)
                .child(div().flex_1().text_left().child(scope_text))
                .dropdown_menu(move |mut menu, _, _| {
                    for (all, label) in [(true, "All repositories"), (false, "This repository")] {
                        let (view, account, current) =
                            (view.clone(), account.clone(), current.clone());
                        menu =
                            menu.item(PopupMenuItem::new(label).checked(all == on_all).on_click(
                                move |_, window, cx| {
                                    let scope = if all {
                                        SecretScope::All
                                    } else {
                                        SecretScope::Repository(current.clone())
                                    };
                                    view.update(cx, |this, cx| {
                                        this.move_secret(account.clone(), scope, window, cx)
                                    })
                                    .ok();
                                },
                            ));
                    }
                    menu
                })
                .into_any_element()
        } else {
            div()
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_SECONDARY))
                .child(scope_text)
                .into_any_element()
        };

        let default = {
            let (view, account) = (view.clone(), account.clone());
            Checkbox::new(("secret-default", index))
                .checked(entry.default)
                .label("Given")
                .text_size(px(12.))
                .on_click(move |checked, _, cx| {
                    view.update(cx, |this, cx| {
                        this.set_secret_default(&account, *checked, cx)
                    })
                    .ok();
                })
        };
        let missing = presence == Some(Presence::Missing);
        let actions = div()
            .h_flex()
            .justify_end()
            .gap(px(2.))
            .child(
                icon_button(
                    ("secret-replace", index),
                    IconName::Pencil,
                    if missing {
                        format!("Set the value of {}", entry.name)
                    } else {
                        format!("Replace the value of {}", entry.name)
                    },
                )
                .disabled(busy)
                .on_click({
                    let (view, account) = (view.clone(), account.clone());
                    move |_, window, cx| {
                        view.update(cx, |this, cx| {
                            this.start_replace(account.clone(), window, cx)
                        })
                        .ok();
                    }
                }),
            )
            .child(
                icon_button(
                    ("secret-delete", index),
                    IconName::Trash,
                    format!("Delete {}", entry.name),
                )
                .disabled(busy)
                .on_click({
                    let (view, account) = (view.clone(), account.clone());
                    move |_, _, cx| {
                        view.update(cx, |this, cx| {
                            this.secrets.edit = Some(RowEdit::Delete {
                                account: account.clone(),
                            });
                            this.secrets.row_error = None;
                            cx.notify();
                        })
                        .ok();
                    }
                }),
            );
        let row = div()
            .h_flex()
            .gap(px(10.))
            .min_h(px(46.))
            .px(px(12.))
            .py(px(6.))
            .child(name)
            .child(div().w(px(SCOPE_WIDTH)).child(scope))
            .child(div().w(px(DEFAULT_WIDTH)).child(default))
            .child(div().w(px(ACTIONS_WIDTH)).child(actions));

        let editing = match &self.secrets.edit {
            Some(RowEdit::Replace {
                account: editing,
                value,
            }) if *editing == account => Some(self.render_replace(index, value, busy, cx)),
            Some(RowEdit::Delete { account: editing }) if *editing == account => {
                Some(self.render_confirm_delete(index, &entry.name, &account, busy, cx))
            }
            _ => None,
        };
        let error = self
            .secrets
            .row_error
            .as_ref()
            .filter(|(errored, _)| *errored == account)
            .map(|(_, error)| error.clone());
        div()
            .v_flex()
            .border_b_1()
            .border_color(rgb(color::ROW_DIVIDER))
            .child(row)
            .children(editing)
            .when_some(error, |this, error| {
                this.child(div().px(px(12.)).pb(px(8.)).child(error_text(error)))
            })
    }

    fn render_replace(
        &self,
        index: usize,
        value: &Entity<InputState>,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let view = cx.entity().downgrade();
        div()
            .h_flex()
            .gap(px(8.))
            .px(px(12.))
            .pb(px(10.))
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(value)
                        .small()
                        .mask_toggle()
                        .bg(rgb(color::FIELD)),
                ),
            )
            .child(
                Button::new(("secret-replace-cancel", index))
                    .ghost()
                    .small()
                    .label("Cancel")
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            view.update(cx, |this, cx| {
                                this.secrets.edit = None;
                                this.secrets.row_error = None;
                                cx.notify();
                            })
                            .ok();
                        }
                    }),
            )
            .child(
                Button::new(("secret-replace-save", index))
                    .primary()
                    .small()
                    .label("Save value")
                    .loading(busy)
                    .on_click(move |_, window, cx| {
                        view.update(cx, |this, cx| this.save_replacement(window, cx))
                            .ok();
                    }),
            )
    }

    fn render_confirm_delete(
        &self,
        index: usize,
        name: &str,
        account: &str,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let view = cx.entity().downgrade();
        let account = account.to_owned();
        div()
            .h_flex()
            .gap(px(8.))
            .px(px(12.))
            .pb(px(10.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.))
                    .text_color(rgb(color::TEXT_SECONDARY))
                    .child(format!(
                        "Delete {name} and its value in the keychain? Threads that were given it \
                         run without it."
                    )),
            )
            .child(
                Button::new(("secret-delete-cancel", index))
                    .ghost()
                    .small()
                    .label("Cancel")
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            view.update(cx, |this, cx| {
                                this.secrets.edit = None;
                                cx.notify();
                            })
                            .ok();
                        }
                    }),
            )
            .child(
                Button::new(("secret-delete-confirm", index))
                    .danger()
                    .small()
                    .label("Delete")
                    .loading(busy)
                    .on_click(move |_, window, cx| {
                        view.update(cx, |this, cx| {
                            this.delete_secret(account.clone(), window, cx)
                        })
                        .ok();
                    }),
            )
    }

    fn render_add_secret(&self, has_repository: bool, cx: &mut Context<Self>) -> Div {
        let mono = cx.theme().mono_font_family.clone();
        let view = cx.entity().downgrade();
        let field = |label: &'static str, input: AnyElement| {
            div()
                .flex_1()
                .min_w_0()
                .v_flex()
                .gap(px(4.))
                .text_size(px(11.5))
                .text_color(rgb(color::TEXT_MUTED))
                .child(label)
                .child(input)
        };
        let all = self.secrets.all_repositories || !has_repository;
        let scope: AnyElement = if has_repository {
            let view = view.clone();
            Button::new("secret-new-scope")
                .compact()
                .h(px(28.))
                .px(px(8.))
                .text_size(px(12.))
                .dropdown_caret(true)
                .label(if all {
                    "All repositories"
                } else {
                    "This repository"
                })
                .dropdown_menu(move |mut menu, _, _| {
                    for (on_all, label) in [(false, "This repository"), (true, "All repositories")]
                    {
                        let view = view.clone();
                        menu =
                            menu.item(PopupMenuItem::new(label).checked(on_all == all).on_click(
                                move |_, _, cx| {
                                    view.update(cx, |this, cx| {
                                        this.secrets.all_repositories = on_all;
                                        cx.notify();
                                    })
                                    .ok();
                                },
                            ));
                    }
                    menu
                })
                .into_any_element()
        } else {
            div()
                .text_color(rgb(color::TEXT_SECONDARY))
                .child("All repositories")
                .into_any_element()
        };
        div()
            .v_flex()
            .gap(px(10.))
            .p(px(12.))
            .bg(rgb(color::SHEET_FORM))
            .rounded_b(px(8.))
            .child(
                div()
                    .h_flex()
                    .items_start()
                    .gap(px(10.))
                    .child(field(
                        "Name",
                        Input::new(&self.secrets.name)
                            .small()
                            .font_family(mono)
                            .bg(rgb(color::FIELD))
                            .into_any_element(),
                    ))
                    .child(field(
                        "Value",
                        Input::new(&self.secrets.value)
                            .small()
                            .mask_toggle()
                            .bg(rgb(color::FIELD))
                            .into_any_element(),
                    )),
            )
            .child(
                div()
                    .h_flex()
                    .flex_wrap()
                    .gap(px(10.))
                    .child(
                        div()
                            .h_flex()
                            .gap(px(6.))
                            .text_size(px(12.))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child("Scope")
                            .child(scope),
                    )
                    .child(
                        Checkbox::new("secret-new-default")
                            .checked(self.secrets.by_default)
                            .label("Give to new threads by default")
                            .text_size(px(12.))
                            .on_click({
                                let view = view.clone();
                                move |checked, _, cx| {
                                    view.update(cx, |this, cx| {
                                        this.secrets.by_default = *checked;
                                        cx.notify();
                                    })
                                    .ok();
                                }
                            }),
                    )
                    .child(
                        Button::new("secret-add")
                            .ml_auto()
                            .small()
                            .icon(Icon::new(IconName::Plus).size(px(13.)))
                            .label("Add secret")
                            .loading(self.secrets.adding)
                            .on_click(move |_, window, cx| {
                                view.update(cx, |this, cx| this.add_secret(window, cx)).ok();
                            }),
                    ),
            )
            .when_some(self.secrets.error.clone(), |this, error| {
                this.child(error_text(error))
            })
    }
}

/// A square ghost button with an icon, named for screen readers.
fn icon_button(id: impl Into<ElementId>, icon: IconName, label: String) -> Button {
    Button::new(id)
        .ghost()
        .size(px(26.))
        .text_color(rgb(color::TEXT_SECONDARY))
        .icon(Icon::new(icon).size(px(13.)))
        .accessibility_label(label.clone())
        .tooltip(label)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use gitez_settings::SecretScope;

    use super::scope_label;

    #[test]
    fn scopes_are_named_for_the_open_repository() {
        let here = Path::new("/w/gitez/.git");
        let there = std::path::PathBuf::from("/w/other.git");
        assert_eq!(
            scope_label(&SecretScope::All, Some(here)),
            "All repositories"
        );
        assert_eq!(
            scope_label(&SecretScope::Repository(here.to_owned()), Some(here)),
            "This repository"
        );
        assert_eq!(
            scope_label(&SecretScope::Repository(here.to_owned()), None),
            "Only gitez"
        );
        assert_eq!(
            scope_label(&SecretScope::Repository(there), Some(here)),
            "Only other"
        );
    }
}
