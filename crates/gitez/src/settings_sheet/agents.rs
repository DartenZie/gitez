//! Agent entries: for each agent GitEZ knows, whether it is installed and
//! signed in, and the fields of its entry (binary, configuration folder,
//! environment, launch arguments, default model). GitEZ never handles the
//! login: it asks the agent (`claude auth status --json`, only whether it
//! is signed in) and says how to sign in.

use std::path::PathBuf;

use gitez_agent::{Availability, ClaudeCode, ConfigDir, ConfigDirSource, LoginStatus, ModelChoice};
use gitez_settings::AgentEntry;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::edit::{format_env, join_args, parse_env, split_args};
use super::{SAVE_AFTER, SettingsSheet, card, error_text, kv_row, muted, section, small_button};
use crate::agent_threads::AgentThreads;
use crate::theme::color;

pub(super) struct AgentForm {
    id: &'static str,
    name: &'static str,
    models: &'static [ModelChoice],
    program: Entity<InputState>,
    config_dir: Entity<InputState>,
    env: Entity<TextareaState>,
    args: Entity<InputState>,
    /// Why the typed fields cannot be saved.
    error: Option<SharedString>,
    /// Counts edits, so only the last one is saved.
    generation: u64,
    check: Option<AgentCheck>,
    /// The entry `check` was made with.
    checked_for: Option<AgentEntry>,
    _checking: Option<Task<()>>,
}

/// What was found out about an agent in the background.
#[derive(Debug, Clone)]
struct AgentCheck {
    availability: Availability,
    /// Only for an installed agent.
    login: Option<LoginStatus>,
    /// Claude Code's configuration folder, and where it was found.
    config_dir: Option<ConfigDir>,
}

/// The fields as typed, ready to save.
struct Typed {
    program: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    env: std::collections::BTreeMap<String, String>,
    args: Vec<String>,
}

fn optional_path(text: &str) -> Option<PathBuf> {
    let text = text.trim();
    (!text.is_empty()).then(|| PathBuf::from(text))
}

fn path_text(path: &Option<PathBuf>) -> String {
    path.as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

impl AgentForm {
    /// A form for each agent of the registry.
    pub(super) fn all(
        model: &Entity<AgentThreads>,
        window: &mut Window,
        cx: &mut Context<SettingsSheet>,
        subscriptions: &mut Vec<Subscription>,
    ) -> Vec<Self> {
        let agents = model.read(cx).agents().all().to_vec();
        let settings = model.read(cx).settings().clone();
        agents
            .iter()
            .enumerate()
            .map(|(index, agent)| {
                let entry = settings.agent(agent.id());
                let program = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder("Found on the PATH or where its installer puts it")
                        .default_value(path_text(&entry.program))
                });
                let config_dir = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(format!("Optional · {}’s default", agent.name()))
                        .default_value(path_text(&entry.config_dir))
                });
                let env = cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .auto_grow(1, 6)
                        .placeholder("NAME=value, one per line")
                        .default_value(format_env(&entry.env))
                });
                let args = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder("Added after GitEZ’s own, such as --max-turns 40")
                        .default_value(join_args(&entry.args))
                });
                for input in [&program, &config_dir, &args] {
                    subscriptions.push(cx.subscribe_in(
                        input,
                        window,
                        move |this, _, event: &InputEvent, _, cx| {
                            if let InputEvent::Change = event {
                                this.agent_typed(index, cx);
                            }
                        },
                    ));
                }
                subscriptions.push(cx.subscribe_in(
                    &env,
                    window,
                    move |this, _, event: &InputEvent, _, cx| {
                        if let InputEvent::Change = event {
                            this.agent_typed(index, cx);
                        }
                    },
                ));
                Self {
                    id: agent.id(),
                    name: agent.name(),
                    models: agent.models(),
                    program,
                    config_dir,
                    env,
                    args,
                    error: None,
                    generation: 0,
                    check: None,
                    checked_for: None,
                    _checking: None,
                }
            })
            .collect()
    }

    fn typed(&self, cx: &App) -> Result<Typed, String> {
        Ok(Typed {
            program: optional_path(&self.program.read(cx).value()),
            config_dir: optional_path(&self.config_dir.read(cx).value()),
            env: parse_env(&self.env.read(cx).value())?,
            args: split_args(&self.args.read(cx).value())
                .map_err(|error| format!("Launch arguments: {error}"))?,
        })
    }
}

impl SettingsSheet {
    /// Saves agent `index`'s fields once typing pauses, if they are valid.
    fn agent_typed(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(form) = self.agents.get_mut(index) else {
            return;
        };
        let typed = match form.typed(cx) {
            Ok(typed) => typed,
            Err(error) => {
                form.error = Some(error.into());
                cx.notify();
                return;
            }
        };
        form.error = None;
        let entry = self.model.read(cx).settings().agent(form.id);
        if entry.program == typed.program
            && entry.config_dir == typed.config_dir
            && entry.env == typed.env
            && entry.args == typed.args
        {
            cx.notify();
            return;
        }
        form.generation += 1;
        let generation = form.generation;
        let id = form.id;
        let model = self.model.clone();
        // Detached, so closing the sheet right after typing still saves.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_AFTER).await;
            let latest = this
                .read_with(cx, |this, _| this.agents[index].generation == generation)
                .unwrap_or(true);
            if !latest {
                return;
            }
            model.update(cx, |model, cx| {
                model.update_settings(
                    |settings| {
                        let entry = settings.agents.entry(id.to_owned()).or_default();
                        entry.program = typed.program;
                        entry.config_dir = typed.config_dir;
                        entry.env = typed.env;
                        entry.args = typed.args;
                    },
                    cx,
                );
            });
        })
        .detach();
        cx.notify();
    }

    fn set_agent_model(&mut self, id: &'static str, alias: &str, cx: &mut Context<Self>) {
        let model = (!alias.is_empty()).then(|| alias.to_owned());
        self.model.update(cx, |threads, cx| {
            threads.update_settings(
                |settings| settings.agents.entry(id.to_owned()).or_default().model = model,
                cx,
            );
        });
    }

    /// Shows entries changed elsewhere in fields that are not being edited.
    pub(super) fn sync_agents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.model.read(cx).settings().clone();
        for form in &self.agents {
            let entry = settings.agent(form.id);
            let fields = [
                (&form.program, path_text(&entry.program)),
                (&form.config_dir, path_text(&entry.config_dir)),
                (&form.args, join_args(&entry.args)),
            ];
            for (input, text) in fields {
                let shown = input.read(cx).value();
                let same = match (input == &form.args, input == &form.program) {
                    (true, _) => split_args(&shown).ok().as_deref() == Some(&entry.args[..]),
                    _ => optional_path(&shown) == optional_path(&text),
                };
                if !same && !input.read(cx).focus_handle(cx).is_focused(window) {
                    input.update(cx, |input, cx| input.set_value(text, window, cx));
                }
            }
            let shown_env = parse_env(&form.env.read(cx).value()).ok();
            if shown_env.as_ref() != Some(&entry.env)
                && !form.env.read(cx).focus_handle(cx).is_focused(window)
            {
                let text = format_env(&entry.env);
                form.env
                    .update(cx, |input, cx| input.set_value(text, window, cx));
            }
        }
    }

    /// Finds out in the background whether each agent is installed and
    /// signed in: when its entry changed, or always with `force`.
    pub(super) fn check_agents(&mut self, force: bool, cx: &mut Context<Self>) {
        let settings = self.model.read(cx).settings().clone();
        let registry = self.model.read(cx).agents().clone();
        for index in 0..self.agents.len() {
            let id = self.agents[index].id;
            let entry = settings.agent(id);
            if !force && self.agents[index].checked_for.as_ref() == Some(&entry) {
                continue;
            }
            self.agents[index].checked_for = Some(entry.clone());
            let Some(agent) = registry.get(id) else {
                continue;
            };
            let checked = cx.background_executor().spawn(async move {
                let config = entry.config();
                let availability = match agent.locate(&config) {
                    Ok(program) => Availability::Installed { program },
                    Err(error) => Availability::Unavailable {
                        reason: error.to_string(),
                    },
                };
                let login = matches!(availability, Availability::Installed { .. })
                    .then(|| agent.login_status(&config));
                let config_dir = (id == ClaudeCode::ID).then(|| ClaudeCode::config_dir(&config));
                AgentCheck {
                    availability,
                    login,
                    config_dir,
                }
            });
            self.agents[index]._checking = Some(cx.spawn(async move |this, cx| {
                let check = checked.await;
                this.update(cx, |this, cx| {
                    if let Some(form) = this.agents.get_mut(index) {
                        form.check = Some(check);
                        form._checking = None;
                    }
                    cx.notify();
                })
                .ok();
            }));
        }
        cx.notify();
    }
}

impl SettingsSheet {
    pub(super) fn render_agents(&self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let mut section = section("Agents");
        for (index, form) in self.agents.iter().enumerate() {
            section = section.child(self.render_agent(index, form, cx));
        }
        section.child(
            div()
                .h_flex()
                .gap(px(12.))
                .p(px(12.))
                .rounded(px(8.))
                .border_1()
                .border_dashed()
                .border_color(rgb(color::PLACEHOLDER_BORDER))
                .text_color(rgb(color::TEXT_MUTED))
                .child(
                    div()
                        .size(px(32.))
                        .flex_none()
                        .rounded(px(8.))
                        .border_1()
                        .border_dashed()
                        .border_color(rgb(color::TAG_LABEL_BORDER)),
                )
                .child(
                    div()
                        .v_flex()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_color(rgb(color::TEXT_SECONDARY))
                                .child("More agents later"),
                        )
                        .child(div().text_size(px(12.)).child(
                            "Other coding agents will be listed here, each with its own login.",
                        )),
                ),
        )
    }

    fn render_agent(&self, index: usize, form: &AgentForm, cx: &mut Context<Self>) -> Div {
        let mono = cx.theme().mono_font_family.clone();
        let view = cx.entity().downgrade();
        let checking = form._checking.is_some();
        let (dot, status): (u32, SharedString) = match &form.check {
            None => (color::TEXT_FAINT, "checking…".into()),
            Some(AgentCheck {
                availability: Availability::Unavailable { .. },
                ..
            }) => (color::DIFF_REMOVED, "not installed".into()),
            Some(AgentCheck {
                login: Some(LoginStatus::SignedIn),
                ..
            }) => (color::DIFF_ADDED, "signed in".into()),
            Some(AgentCheck {
                login: Some(LoginStatus::SignedOut),
                ..
            }) => (color::DIFF_REMOVED, "not signed in".into()),
            Some(_) => (color::TEXT_FAINT, "login unknown".into()),
        };
        let code = |text: &'static str| {
            div()
                .font_family(mono.clone())
                .text_size(px(11.5))
                .px(px(4.))
                .rounded(px(3.))
                .bg(rgb(color::BUTTON))
                .text_color(rgb(color::TEXT_SECONDARY))
                .child(text)
        };
        let signed_out = matches!(
            form.check,
            Some(AgentCheck {
                login: Some(LoginStatus::SignedOut),
                ..
            })
        );
        let login_line = div()
            .h_flex()
            .flex_wrap()
            .gap(px(4.))
            .text_size(px(12.))
            .text_color(rgb(if signed_out {
                color::TEXT_SECONDARY
            } else {
                color::TEXT_MUTED
            }))
            .child(format!(
                "{}{}’s own login. To sign in, run",
                if signed_out {
                    "Not signed in. GitEZ uses "
                } else {
                    "Uses "
                },
                form.name
            ))
            .child(code("claude auth login"))
            .child(".");
        let where_line: Option<Div> = match &form.check {
            Some(AgentCheck {
                availability: Availability::Installed { program },
                ..
            }) => Some(
                div()
                    .text_size(px(11.5))
                    .text_color(rgb(color::TEXT_MUTED))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(format!("Installed: {}", crate::recent::display(program))),
            ),
            Some(AgentCheck {
                availability: Availability::Unavailable { reason },
                ..
            }) => Some(error_text(format!(
                "{}. Install it, or set its binary below.",
                capitalise(reason)
            ))),
            None => None,
        };
        let unknown = match &form.check {
            Some(AgentCheck {
                login: Some(LoginStatus::Unknown(why)),
                ..
            }) => Some(muted(format!(
                "Could not tell whether it is signed in: {why}"
            ))),
            _ => None,
        };
        let header = div()
            .h_flex()
            .items_start()
            .gap(px(12.))
            .p(px(12.))
            .child(agent_mark(form.id))
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
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(form.name))
                            .child(muted("·"))
                            .child(
                                div()
                                    .h_flex()
                                    .gap(px(5.))
                                    .text_size(px(12.))
                                    .text_color(rgb(dot))
                                    .child(div().size(px(6.)).rounded_full().bg(rgb(dot)))
                                    .child(status),
                            )
                            .child(
                                small_button(("agent-recheck", index), "Check again")
                                    .ml_auto()
                                    .loading(checking)
                                    .on_click({
                                        let view = view.clone();
                                        move |_, _, cx| {
                                            view.update(cx, |this, cx| this.check_agents(true, cx))
                                                .ok();
                                        }
                                    }),
                            )
                            .when(index == 0, |this| {
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
                    .child(login_line)
                    .children(where_line)
                    .children(unknown),
            );

        let input = |state: &Entity<InputState>| {
            Input::new(state)
                .small()
                .font_family(mono.clone())
                .bg(rgb(color::FIELD))
        };
        let config_note = form
            .check
            .as_ref()
            .and_then(|check| check.config_dir.as_ref())
            .map(|dir| {
                let source = match dir.source {
                    ConfigDirSource::Settings => "set here",
                    ConfigDirSource::Environment => "from CLAUDE_CONFIG_DIR in GitEZ’s environment",
                    ConfigDirSource::LoginShell => "from CLAUDE_CONFIG_DIR in your login shell",
                    ConfigDirSource::Default => "Claude Code’s default",
                };
                muted(format!(
                    "In use: {} · {source}",
                    crate::recent::display(&dir.path)
                ))
                .text_size(px(11.5))
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
            });
        let chosen = self
            .model
            .read(cx)
            .settings()
            .agent(form.id)
            .model
            .unwrap_or_default();
        let chosen_label = form
            .models
            .iter()
            .find(|(alias, _)| *alias == chosen)
            .map_or_else(|| chosen.clone(), |(_, label)| (*label).to_owned());
        let models = form.models;
        let id = form.id;
        let model_picker = Button::new(("agent-model", index))
            .compact()
            .h(px(28.))
            .px(px(8.))
            .min_w(px(160.))
            .text_size(px(12.))
            .dropdown_caret(true)
            .child(div().flex_1().text_left().child(chosen_label))
            .dropdown_menu(move |mut menu, _, _| {
                for &(alias, label) in models {
                    let view = view.clone();
                    menu = menu.item(PopupMenuItem::new(label).checked(alias == chosen).on_click(
                        move |_, _, cx| {
                            view.update(cx, |this, cx| this.set_agent_model(id, alias, cx))
                                .ok();
                        },
                    ));
                }
                menu
            });

        card()
            .child(header)
            .child(div().h(px(1.)).bg(rgb(color::ROW_DIVIDER)))
            .child(kv_row("Binary", input(&form.program)))
            .child(kv_row(
                "Config folder",
                div()
                    .v_flex()
                    .gap(px(4.))
                    .child(input(&form.config_dir))
                    .children(config_note),
            ))
            .child(kv_row(
                "Environment",
                div()
                    .v_flex()
                    .gap(px(4.))
                    .child(
                        Textarea::new(&form.env)
                            .font_family(mono.clone())
                            .text_size(px(12.))
                            .bg(rgb(color::FIELD)),
                    )
                    .child(
                        muted("Kept in settings.json as written. Put tokens in Secrets.")
                            .text_size(px(11.5)),
                    ),
            ))
            .child(kv_row("Arguments", input(&form.args)))
            .child(kv_row("Default model", model_picker))
            .when_some(form.error.clone(), |this, error| {
                this.child(div().px(px(12.)).py(px(8.)).child(error_text(error)))
            })
    }
}

/// `text` with its first letter in capitals.
fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The agent's mark in a 32 px square, as the threads show it smaller.
fn agent_mark(agent: &str) -> Div {
    let mark = if agent == ClaudeCode::ID {
        svg()
            .path("agents/claude.svg")
            .size(px(16.))
            .text_color(rgb(color::ACCENT))
            .into_any_element()
    } else {
        Icon::new(IconName::Bot)
            .size(px(16.))
            .text_color(rgb(color::ACCENT))
            .into_any_element()
    };
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(32.))
        .rounded(px(8.))
        .bg(rgb(color::AGENT_MARK_BACKGROUND))
        .child(mark)
}
