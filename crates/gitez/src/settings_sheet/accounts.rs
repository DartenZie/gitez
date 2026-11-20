//! Claude accounts: the default one, which is the configuration folder
//! GitEZ always used, and those added here, each a name and a folder of its
//! own. Claude Code keeps its login per folder, so a folder is an account.
//! For each, whether Claude Code is signed in there (`claude auth status`,
//! run with that folder) and the command that signs it in. GitEZ never
//! copies anything between folders and never touches a login.

use std::collections::HashMap;
use std::path::PathBuf;

use gitez_agent::{ClaudeCode, ConfigDir, ConfigDirSource, LoginStatus, login_command};
use gitez_agent_thread::Account;
use gitez_settings::{AccountEntry, AgentEntry, accounts_problem};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::secrets::icon_button;
use super::{SettingsSheet, card, description, error_text, label, muted, small_button};
use crate::agent_threads::AgentThreads;
use crate::theme::color;

pub(super) struct AccountsPanel {
    name: Entity<InputState>,
    folder: Entity<InputState>,
    /// Why the account typed cannot be added.
    error: Option<SharedString>,
    check: Option<AccountsCheck>,
    /// The accounts and agent entry `check` was made with.
    checked_for: Option<(Vec<AccountEntry>, AgentEntry)>,
    _checking: Option<Task<()>>,
    /// The sign-in command copied last, to say so.
    copied: Option<String>,
}

/// What was found out about the accounts in the background.
#[derive(Debug, Clone, Default)]
struct AccountsCheck {
    /// The default account's folder, and where GitEZ found it.
    default: Option<ConfigDir>,
    /// By account name, the default's included. Empty when Claude Code is
    /// not known to GitEZ.
    logins: HashMap<String, LoginStatus>,
}

/// The folder suggested for an account named `name`: `~/.claude-<name>`,
/// which also works from a terminal.
pub(super) fn suggested_folder(name: &str) -> String {
    let slug: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "~/.claude-<name>".to_owned()
    } else {
        format!("~/.claude-{slug}")
    }
}

impl AccountsPanel {
    pub(super) fn new(
        window: &mut Window,
        cx: &mut Context<SettingsSheet>,
        subscriptions: &mut Vec<Subscription>,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Work"));
        let folder = cx.new(|cx| InputState::new(window, cx).placeholder(suggested_folder("")));
        subscriptions.push(cx.subscribe_in(
            &name,
            window,
            |this, input, event: &InputEvent, window, cx| {
                if let InputEvent::Change = event {
                    let suggestion = suggested_folder(&input.read(cx).value());
                    this.accounts.folder.update(cx, |folder, cx| {
                        folder.set_placeholder(suggestion, window, cx)
                    });
                    this.accounts.error = None;
                    cx.notify();
                }
            },
        ));
        Self {
            name,
            folder,
            error: None,
            check: None,
            checked_for: None,
            _checking: None,
            copied: None,
        }
    }
}

impl SettingsSheet {
    /// Adds the account typed, its folder the suggested one when none is
    /// typed. The folder is made, empty, when it does not exist: Claude
    /// Code fills it when the account signs in.
    pub(super) fn add_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.accounts.name.read(cx).value().trim().to_owned();
        let typed = self.accounts.folder.read(cx).value().trim().to_owned();
        let folder = if typed.is_empty() {
            suggested_folder(&name)
        } else {
            typed
        };
        let entry = AccountEntry {
            name: name.clone(),
            folder: PathBuf::from(&folder),
        };
        let mut accounts = self.model.read(cx).settings().accounts.clone();
        accounts.push(entry.clone());
        if let Some(problem) = accounts_problem(&accounts) {
            self.accounts.error = Some(problem.into());
            cx.notify();
            return;
        }
        self.accounts.error = None;
        let path = entry.account().folder;
        cx.background_executor()
            .spawn(async move {
                if let Some(path) = path.filter(|path| !path.exists()) {
                    std::fs::create_dir_all(&path).ok();
                }
            })
            .detach();
        self.model.update(cx, |model, cx| {
            model.update_settings(|settings| settings.accounts.push(entry), cx);
        });
        for input in [&self.accounts.name, &self.accounts.folder] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        cx.notify();
    }

    /// Removes account `name` from Settings. Its folder and login stay;
    /// threads that ran with it stop until an account has its folder
    /// again.
    pub(super) fn remove_account(&mut self, name: &str, cx: &mut Context<Self>) {
        let name = name.to_owned();
        self.model.update(cx, |model, cx| {
            model.update_settings(|settings| settings.accounts.retain(|a| a.name != name), cx);
        });
    }

    /// Finds out in the background whether Claude Code is signed in to
    /// each account: when the accounts or Claude Code's entry changed, or
    /// always with `force`.
    pub(super) fn check_accounts(&mut self, force: bool, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let settings = model.settings().clone();
        let entry = settings.agent(ClaudeCode::ID);
        let key = (settings.accounts.clone(), entry);
        if !force && self.accounts.checked_for.as_ref() == Some(&key) {
            return;
        }
        self.accounts.checked_for = Some(key);
        let agent = model.agents().get(ClaudeCode::ID);
        let checked = cx.background_executor().spawn(async move {
            let default = ClaudeCode::config_dir(&settings.agent(ClaudeCode::ID).config());
            let mut logins = HashMap::new();
            if let Some(agent) = agent {
                let accounts = std::iter::once(settings.default_account(ClaudeCode::ID))
                    .chain(settings.accounts.iter().map(AccountEntry::account));
                for account in accounts {
                    let config = settings.account_config(ClaudeCode::ID, &account);
                    logins.insert(account.name, agent.login_status(&config));
                }
            }
            AccountsCheck {
                default: Some(default),
                logins,
            }
        });
        self.accounts._checking = Some(cx.spawn(async move |this, cx| {
            let check = checked.await;
            this.update(cx, |this, cx| {
                this.accounts.check = Some(check);
                this.accounts._checking = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn copy_login(&mut self, command: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(command.clone()));
        self.accounts.copied = Some(command);
        cx.notify();
    }

    /// For tests: whether every account's login was checked.
    #[cfg(test)]
    pub(super) fn accounts_checked(&self) -> bool {
        self.accounts._checking.is_none() && self.accounts.check.is_some()
    }
}

impl SettingsSheet {
    pub(super) fn render_accounts(&self, cx: &mut Context<Self>) -> Div {
        let settings = self.model.read(cx).settings().clone();
        let view = cx.entity().downgrade();
        let check = self.accounts.check.clone().unwrap_or_default();
        let checking = self.accounts._checking.is_some();
        let default_dir = check.default.clone();
        // The default runs with the variable unset unless GitEZ found a
        // folder in its own settings; a terminal has the rest itself.
        let default_command = login_command(
            default_dir
                .as_ref()
                .filter(|dir| dir.source == ConfigDirSource::Settings)
                .map(|dir| dir.path.as_path()),
        );
        let default_note = default_dir.as_ref().map(|dir| match dir.source {
            ConfigDirSource::Settings => "the folder set for Claude Code above",
            ConfigDirSource::Environment => "CLAUDE_CONFIG_DIR in GitEZ’s environment",
            ConfigDirSource::LoginShell => "CLAUDE_CONFIG_DIR in your login shell",
            ConfigDirSource::Default => "Claude Code’s own default",
        });
        let mut table = card().child(self.render_account_row(
            0,
            Account::DEFAULT_NAME,
            default_dir.as_ref().map(|dir| dir.path.clone()),
            default_note,
            default_command,
            check.logins.get(Account::DEFAULT_NAME),
            checking,
            cx,
        ));
        for (index, entry) in settings.accounts.iter().enumerate() {
            let account = entry.account();
            let command = login_command(account.folder.as_deref());
            table = table.child(self.render_account_row(
                index + 1,
                &entry.name,
                Some(entry.folder.clone()),
                None,
                command,
                check.logins.get(&account.name),
                checking,
                cx,
            ));
        }
        table = table.child(self.render_add_account(cx));
        div()
            .v_flex()
            .gap(px(8.))
            .child(
                div()
                    .h_flex()
                    .gap(px(8.))
                    .child(label("Claude accounts"))
                    .child(
                        small_button("accounts-recheck", "Check again")
                            .ml_auto()
                            .loading(checking)
                            .on_click(move |_, _, cx| {
                                view.update(cx, |this, cx| this.check_accounts(true, cx))
                                    .ok();
                            }),
                    ),
            )
            .child(description(
                "Claude Code keeps its login per configuration folder, so each folder is an \
                 account. A repository picks one in “In this repository”; the others run with \
                 the default. A thread keeps the account it started with.",
            ))
            .child(table)
    }

    #[allow(clippy::too_many_arguments)]
    fn render_account_row(
        &self,
        index: usize,
        name: &str,
        folder: Option<PathBuf>,
        note: Option<&'static str>,
        command: String,
        login: Option<&LoginStatus>,
        checking: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let mono = cx.theme().mono_font_family.clone();
        let view = cx.entity().downgrade();
        let is_default = index == 0;
        let (dot, status): (u32, SharedString) = match login {
            None if checking => (color::TEXT_FAINT, "checking…".into()),
            None => (color::TEXT_FAINT, "login unknown".into()),
            Some(LoginStatus::SignedIn) => (color::DIFF_ADDED, "signed in".into()),
            Some(LoginStatus::SignedOut) => (color::DIFF_REMOVED, "not signed in".into()),
            Some(LoginStatus::Unknown(_)) => (color::TEXT_FAINT, "login unknown".into()),
        };
        let signed_in = matches!(login, Some(LoginStatus::SignedIn));
        let copied = self.accounts.copied.as_deref() == Some(command.as_str());
        let folder_line = div()
            .h_flex()
            .gap(px(6.))
            .min_w_0()
            .text_size(px(11.5))
            .text_color(rgb(color::TEXT_MUTED))
            .child(
                div()
                    .min_w_0()
                    .font_family(mono.clone())
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(match &folder {
                        Some(folder) => crate::recent::display(folder),
                        None => "…".to_owned(),
                    }),
            )
            .when_some(note, |this, note| {
                this.child(div().flex_none().child(format!("· {note}")))
            });
        let sign_in = (!signed_in).then(|| {
            div()
                .h_flex()
                .flex_wrap()
                .gap(px(6.))
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_SECONDARY))
                .child("To sign in, run")
                .child(
                    div()
                        .font_family(mono.clone())
                        .text_size(px(11.5))
                        .px(px(4.))
                        .rounded(px(3.))
                        .bg(rgb(color::BUTTON))
                        .child(command.clone()),
                )
                .child(
                    small_button(
                        ("account-copy", index),
                        if copied { "Copied" } else { "Copy" },
                    )
                    .on_click({
                        let (view, command) = (view.clone(), command.clone());
                        move |_, _, cx| {
                            view.update(cx, |this, cx| this.copy_login(command.clone(), cx))
                                .ok();
                        }
                    }),
                )
        });
        let unknown = match login {
            Some(LoginStatus::Unknown(why)) => Some(muted(format!(
                "Could not tell whether it is signed in: {why}"
            ))),
            _ => None,
        };
        let name_owned = name.to_owned();
        div()
            .h_flex()
            .items_start()
            .gap(px(10.))
            .px(px(12.))
            .py(px(10.))
            .border_b_1()
            .border_color(rgb(color::ROW_DIVIDER))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .v_flex()
                    .gap(px(3.))
                    .child(
                        div()
                            .h_flex()
                            .gap(px(8.))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(name_owned.clone()),
                            )
                            .child(
                                div()
                                    .h_flex()
                                    .gap(px(5.))
                                    .text_size(px(12.))
                                    .text_color(rgb(dot))
                                    .child(div().size(px(6.)).rounded_full().bg(rgb(dot)))
                                    .child(status),
                            )
                            .when(is_default, |this| {
                                this.child(
                                    div()
                                        .h(px(18.))
                                        .px(px(7.))
                                        .rounded(px(9.))
                                        .bg(rgb(color::THREAD_QUEUED))
                                        .text_size(px(11.))
                                        .text_color(rgb(color::THREAD_QUEUED_TEXT))
                                        .flex()
                                        .items_center()
                                        .child("Default"),
                                )
                            }),
                    )
                    .child(folder_line)
                    .children(sign_in)
                    .children(unknown),
            )
            .when(!is_default, |this| {
                this.child(
                    icon_button(
                        ("account-remove", index),
                        IconName::Trash,
                        format!("Remove {name_owned}"),
                    )
                    .on_click({
                        let name = name_owned.clone();
                        move |_, _, cx| {
                            view.update(cx, |this, cx| this.remove_account(&name, cx))
                                .ok();
                        }
                    }),
                )
            })
    }

    fn render_add_account(&self, cx: &mut Context<Self>) -> Div {
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
        div()
            .v_flex()
            .gap(px(8.))
            .p(px(12.))
            .bg(rgb(color::SHEET_FORM))
            .rounded_b(px(8.))
            .child(
                div()
                    .h_flex()
                    .items_end()
                    .gap(px(10.))
                    .child(field(
                        "Name",
                        Input::new(&self.accounts.name)
                            .small()
                            .bg(rgb(color::FIELD))
                            .into_any_element(),
                    ))
                    .child(field(
                        "Folder",
                        Input::new(&self.accounts.folder)
                            .small()
                            .font_family(mono)
                            .bg(rgb(color::FIELD))
                            .into_any_element(),
                    ))
                    .child(
                        Button::new("account-add")
                            .small()
                            .icon(Icon::new(IconName::Plus).size(px(13.)))
                            .label("Add account")
                            .on_click(move |_, window, cx| {
                                view.update(cx, |this, cx| this.add_account(window, cx))
                                    .ok();
                            }),
                    ),
            )
            .child(
                muted(
                    "An existing folder, or a new one that GitEZ makes empty. GitEZ copies \
                     nothing into it; sign in there with the command shown.",
                )
                .text_size(px(11.5)),
            )
            .when_some(self.accounts.error.clone(), |this, error| {
                this.child(error_text(error))
            })
    }
}

/// The account names a repository can choose, the default first.
pub(super) fn account_names(model: &AgentThreads) -> Vec<String> {
    std::iter::once(Account::DEFAULT_NAME.to_owned())
        .chain(
            model
                .settings()
                .accounts
                .iter()
                .map(|entry| entry.name.trim().to_owned()),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::suggested_folder;

    #[test]
    fn the_folder_suggested_follows_the_name() {
        assert_eq!(suggested_folder("Work"), "~/.claude-work");
        assert_eq!(suggested_folder(" My Client "), "~/.claude-my-client");
        assert_eq!(suggested_folder(""), "~/.claude-<name>");
    }
}
