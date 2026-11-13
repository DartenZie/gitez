//! The sandbox: whether it can run (and installing it into GitEZ's data
//! folder when that is the fix), the network profile new threads start
//! with, the hosts "Packages" adds, and the paths hidden from the agent.

use gitez_sandbox::{NetworkProfile, SRT_PACKAGE, Srt, Status};
use gitez_settings::{DEFAULT_HIDDEN_PATHS, DEFAULT_PACKAGE_HOSTS};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::edit::{format_hosts, hidden_path, parse_hosts};
use super::{
    SettingsSheet, card, chip, description, error_text, label, muted, section, small_button,
};
use crate::theme::color;

/// Hosts shown in the Packages card before "+N more".
const HOSTS_SHOWN: usize = 6;

#[derive(Default)]
pub(super) struct SandboxPanel {
    check: Option<SandboxCheck>,
    _checking: Option<Task<()>>,
    install: Install,
    /// Seconds the running install has taken, counted by a timer.
    install_seconds: u64,
    _ticking: Option<Task<()>>,
    /// The allowlist editor, while open: one host per line.
    hosts: Option<Entity<TextareaState>>,
    hosts_error: Option<SharedString>,
    /// The "Hide a path" field, while open.
    hide: Option<Entity<InputState>>,
    hide_error: Option<SharedString>,
}

#[derive(Debug, Clone)]
struct SandboxCheck {
    status: Status,
    installable: bool,
    /// The hosts every thread may reach: the agents' own.
    agent_hosts: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) enum Install {
    #[default]
    Idle,
    Running,
    Failed(SharedString),
}

impl SettingsSheet {
    /// For tests: where the sandbox install is, and whether the sandbox
    /// was last found ready.
    #[cfg(test)]
    pub(super) fn install_state(&self) -> (Install, Option<bool>) {
        let ready = self
            .sandbox
            .check
            .as_ref()
            .map(|check| matches!(check.status, Status::Ready { .. }));
        (self.sandbox.install.clone(), ready)
    }

    /// Asks the sandbox in the background whether it can run.
    pub(super) fn check_sandbox(&mut self, cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let sandbox = model.sandbox().clone();
        let agents = model.agents().clone();
        let settings = model.settings().clone();
        let checked = cx.background_executor().spawn(async move {
            let mut agent_hosts: Vec<String> = Vec::new();
            for agent in agents.all() {
                for host in agent.needs(&settings.agent(agent.id()).config()).hosts {
                    if !agent_hosts.contains(&host) {
                        agent_hosts.push(host);
                    }
                }
            }
            SandboxCheck {
                status: sandbox.status(),
                installable: sandbox.installable(),
                agent_hosts,
            }
        });
        self.sandbox._checking = Some(cx.spawn(async move |this, cx| {
            let check = checked.await;
            this.update(cx, |this, cx| {
                this.sandbox.check = Some(check);
                this.sandbox._checking = None;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Installs the sandbox on a thread of its own (npm can take
    /// minutes), then checks it again and tells the other views.
    fn install_sandbox(&mut self, cx: &mut Context<Self>) {
        if self.sandbox.install == Install::Running {
            return;
        }
        let model = self.model.clone();
        let sandbox = model.read(cx).sandbox().clone();
        let (done, installed) = futures::channel::oneshot::channel();
        let started = std::thread::Builder::new()
            .name("gitez-sandbox-install".into())
            .spawn(move || {
                done.send(sandbox.install()).ok();
            });
        if let Err(error) = started {
            self.sandbox.install =
                Install::Failed(format!("Could not start the install: {error}").into());
            cx.notify();
            return;
        }
        self.sandbox.install = Install::Running;
        self.sandbox.install_seconds = 0;
        self.sandbox._ticking = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                let running = this
                    .update(cx, |this, cx| {
                        this.sandbox.install_seconds += 1;
                        cx.notify();
                        this.sandbox.install == Install::Running
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        }));
        // Detached: the other views learn of the install even if the sheet
        // closed meanwhile.
        cx.spawn(async move |this, cx| {
            let installed = installed.await;
            model.update(cx, |model, cx| model.environment_changed(cx));
            this.update(cx, |this, cx| {
                this.sandbox.install = match installed {
                    Ok(Ok(())) => Install::Idle,
                    Ok(Err(error)) => Install::Failed(error.to_string().into()),
                    Err(_) => Install::Failed("The install stopped without an answer.".into()),
                };
                this.sandbox._ticking = None;
                this.check_sandbox(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn set_default_network(&mut self, profile: NetworkProfile, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.update_settings(|settings| settings.sandbox.default_network = profile, cx);
        });
    }

    fn edit_hosts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = format_hosts(&self.model.read(cx).settings().sandbox.package_hosts);
        let editor = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(4, 12)
                .placeholder("One host per line, such as crates.io or *.example.com")
                .default_value(text)
        });
        self._subscriptions.push(cx.subscribe_in(
            &editor,
            window,
            |this, _, event: &InputEvent, _, cx| {
                if let InputEvent::Change = event {
                    this.sandbox.hosts_error = None;
                    cx.notify();
                }
            },
        ));
        editor.update(cx, |editor, cx| editor.focus(window, cx));
        self.sandbox.hosts = Some(editor);
        self.sandbox.hosts_error = None;
        cx.notify();
    }

    fn save_hosts(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.sandbox.hosts else {
            return;
        };
        match parse_hosts(&editor.read(cx).value()) {
            Ok(hosts) => {
                self.model.update(cx, |model, cx| {
                    model.update_settings(|settings| settings.sandbox.package_hosts = hosts, cx);
                });
                self.sandbox.hosts = None;
                self.sandbox.hosts_error = None;
            }
            Err(error) => self.sandbox.hosts_error = Some(error.into()),
        }
        cx.notify();
    }

    fn reset_hosts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let defaults: Vec<String> = DEFAULT_PACKAGE_HOSTS
            .iter()
            .map(|h| (*h).to_owned())
            .collect();
        if let Some(editor) = &self.sandbox.hosts {
            let text = format_hosts(&defaults);
            editor.update(cx, |editor, cx| editor.set_value(text, window, cx));
        }
        self.model.update(cx, |model, cx| {
            model.update_settings(|settings| settings.sandbox.package_hosts = defaults, cx);
        });
        self.sandbox.hosts_error = None;
        cx.notify();
    }

    fn start_hiding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("~/.config/tool, or a full path"));
        self._subscriptions.push(cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.hide_typed_path(window, cx),
                InputEvent::Change => {
                    this.sandbox.hide_error = None;
                    cx.notify();
                }
                _ => {}
            },
        ));
        input.update(cx, |input, cx| input.focus(window, cx));
        self.sandbox.hide = Some(input);
        self.sandbox.hide_error = None;
        cx.notify();
    }

    fn hide_typed_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = &self.sandbox.hide else {
            return;
        };
        let typed = input.read(cx).value().to_string();
        self.hide_path(&typed, window, cx);
    }

    fn hide_path(&mut self, typed: &str, _window: &mut Window, cx: &mut Context<Self>) {
        let path = match hidden_path(typed, dirs::home_dir().as_deref()) {
            Ok(path) => path,
            Err(error) => {
                self.sandbox.hide_error = Some(error.into());
                cx.notify();
                return;
            }
        };
        if self
            .model
            .read(cx)
            .settings()
            .sandbox
            .hidden_paths
            .contains(&path)
        {
            self.sandbox.hide_error = Some(format!("{path} is hidden already.").into());
            cx.notify();
            return;
        }
        self.model.update(cx, |model, cx| {
            model.update_settings(|settings| settings.sandbox.hidden_paths.push(path), cx);
        });
        self.sandbox.hide = None;
        self.sandbox.hide_error = None;
        cx.notify();
    }

    /// Picks a folder or file to hide with the system's file dialog.
    fn choose_hidden_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some("Hide".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.hide_path(&path.to_string_lossy(), window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn unhide_path(&mut self, path: &str, cx: &mut Context<Self>) {
        let path = path.to_owned();
        self.model.update(cx, |model, cx| {
            model.update_settings(
                |settings| {
                    settings
                        .sandbox
                        .hidden_paths
                        .retain(|hidden| *hidden != path)
                },
                cx,
            );
        });
    }

    fn reset_hidden_paths(&mut self, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.update_settings(
                |settings| {
                    settings.sandbox.hidden_paths = DEFAULT_HIDDEN_PATHS
                        .iter()
                        .map(|p| (*p).to_owned())
                        .collect();
                },
                cx,
            );
        });
        self.sandbox.hide_error = None;
    }
}

impl SettingsSheet {
    pub(super) fn render_sandbox(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let name = self.model.read(cx).sandbox().name();
        section("Sandbox")
            .child(description(
                "Each thread runs inside a sandbox around the whole agent process (Seatbelt on \
                 macOS). Nothing asks for permission during a run; blocked hosts are listed on the \
                 thread.",
            ))
            .child(self.render_sandbox_status(name, cx))
            .child(label("Network for new threads").mt(px(4.)))
            .child(self.render_network(window, cx))
            .child(label("Files").mt(px(6.)))
            .child(self.render_files(cx))
    }

    fn render_sandbox_status(&self, name: &'static str, cx: &mut Context<Self>) -> Div {
        let mono = cx.theme().mono_font_family.clone();
        let view = cx.entity().downgrade();
        let data_dir = self.model.read(cx).data_dir().map(Srt::install_dir);
        let (dot, state): (u32, SharedString) = match &self.sandbox.check {
            None => (color::TEXT_FAINT, "Checking…".into()),
            Some(SandboxCheck {
                status: Status::Ready { version, .. },
                ..
            }) => (
                color::DIFF_ADDED,
                match version {
                    Some(version) => format!("Ready · {version}").into(),
                    None => "Ready".into(),
                },
            ),
            Some(SandboxCheck {
                installable: true, ..
            }) => (color::DIFF_REMOVED, "Not installed".into()),
            Some(_) => (color::DIFF_REMOVED, "Cannot run".into()),
        };
        let mut body = div().flex_1().min_w_0().v_flex().gap(px(4.)).child(
            div()
                .h_flex()
                .gap(px(8.))
                .child(div().font_weight(FontWeight::MEDIUM).child(name))
                .child(
                    div()
                        .h_flex()
                        .gap(px(5.))
                        .text_size(px(12.))
                        .text_color(rgb(dot))
                        .child(div().size(px(6.)).rounded_full().bg(rgb(dot)))
                        .child(state),
                ),
        );
        let install_button = |label: &'static str, loading: bool| {
            let view = view.clone();
            Button::new("sandbox-install")
                .primary()
                .small()
                .label(label)
                .loading(loading)
                .on_click(move |_, _, cx| {
                    view.update(cx, |this, cx| this.install_sandbox(cx)).ok();
                })
        };
        let mut action = None;
        match (&self.sandbox.check, &self.sandbox.install) {
            (_, Install::Running) => {
                body = body.child(muted(format!(
                    "npm is installing {SRT_PACKAGE} into {}… {} s",
                    data_dir
                        .as_deref()
                        .map(crate::recent::display)
                        .unwrap_or_default(),
                    self.sandbox.install_seconds
                )));
                action = Some(install_button("Installing…", true));
            }
            (
                Some(SandboxCheck {
                    status: Status::Ready { program, .. },
                    ..
                }),
                _,
            ) => {
                body = body.child(
                    div()
                        .font_family(mono)
                        .text_size(px(11.))
                        .text_color(rgb(color::TEXT_MUTED))
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(crate::recent::display(program)),
                );
            }
            (
                Some(SandboxCheck {
                    status: Status::Missing { fix },
                    installable,
                    ..
                }),
                install,
            ) => {
                if *installable {
                    body = body.child(muted(format!(
                        "Threads cannot start without it. GitEZ installs {SRT_PACKAGE} with npm \
                         into its own data folder{}; nothing is installed globally.",
                        data_dir
                            .as_deref()
                            .map(|dir| format!(" ({})", crate::recent::display(dir)))
                            .unwrap_or_default()
                    )));
                    action = Some(install_button(
                        if matches!(install, Install::Failed(_)) {
                            "Try again"
                        } else {
                            "Install"
                        },
                        false,
                    ));
                } else {
                    body = body.child(error_text(format!("Threads cannot start: {fix}")));
                }
            }
            (None, _) => {}
        }
        if let Install::Failed(error) = &self.sandbox.install {
            body = body.child(error_text(format!("The install failed: {error}")));
        }
        card().child(
            div()
                .h_flex()
                .items_start()
                .gap(px(12.))
                .p(px(12.))
                .child(
                    Icon::new(IconName::ShieldCheck)
                        .size(px(16.))
                        .mt(px(1.))
                        .text_color(rgb(color::ACCENT)),
                )
                .child(body)
                .children(action),
        )
    }

    fn render_network(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let settings = self.model.read(cx).settings().clone();
        let chosen = settings.sandbox.default_network;
        let view = cx.entity().downgrade();
        let mut cards = div().v_flex().gap(px(8.));
        for profile in NetworkProfile::ALL {
            let on = profile == chosen;
            let text = match profile {
                NetworkProfile::AgentOnly => {
                    "Only the agent’s own API. Builds that download packages fail."
                }
                NetworkProfile::Packages => {
                    "Agent API, package registries and this repository’s remotes."
                }
                NetworkProfile::Open => "Any host. Use only for threads that need the wider web.",
            };
            let mut content = div().flex_1().min_w_0().v_flex().gap(px(6.)).child(
                div()
                    .v_flex()
                    .gap(px(3.))
                    .child(div().font_weight(FontWeight::MEDIUM).child(profile.label()))
                    .child(muted(text)),
            );
            if profile == NetworkProfile::Packages {
                content = content.child(self.render_host_chips(cx));
            }
            let radio = div()
                .size(px(14.))
                .mt(px(2.))
                .flex_none()
                .rounded_full()
                .border_1()
                .border_color(rgb(if on {
                    color::ACCENT
                } else {
                    color::PICK_BORDER
                }))
                .flex()
                .items_center()
                .justify_center()
                .when(on, |this| {
                    this.child(div().size(px(6.)).rounded_full().bg(rgb(color::ACCENT)))
                });
            let view = view.clone();
            cards = cards.child(
                div()
                    .id(SharedString::from(format!("network-{}", profile.label())))
                    .h_flex()
                    .items_start()
                    .gap(px(10.))
                    .px(px(12.))
                    .py(px(10.))
                    .rounded(px(8.))
                    .border_1()
                    .map(|this| {
                        if on {
                            this.border_color(rgba(color::PICKER_ON_BORDER))
                                .bg(rgb(color::OPTION_ON))
                        } else {
                            this.border_color(rgb(color::CARD_BORDER))
                                .bg(rgb(color::PANEL))
                                .hover(|this| this.border_color(rgb(color::REMOTE_LABEL_BORDER)))
                        }
                    })
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| this.set_default_network(profile, cx))
                            .ok();
                    })
                    .child(radio)
                    .child(content),
            );
        }
        if let Some(editor) = &self.sandbox.hosts {
            cards = cards.child(self.render_hosts_editor(editor, window, cx));
        }
        cards.child(
            div()
                .h_flex()
                .items_start()
                .gap(px(8.))
                .px(px(2.))
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .size(px(13.))
                        .mt(px(2.))
                        .text_color(rgb(color::ACCENT)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.))
                        .line_height(relative(1.5))
                        .text_color(rgb(color::WARNING_TEXT))
                        .child(
                            "Packages and Open: the sandbox cannot separate the agent from the \
                             commands it runs, so with a wider network the agent could send out \
                             whatever it can read, including its own login.",
                        ),
                ),
        )
    }

    fn render_host_chips(&self, cx: &mut Context<Self>) -> Div {
        let settings = self.model.read(cx).settings().clone();
        let hosts = &settings.sandbox.package_hosts;
        let agent_hosts = self
            .sandbox
            .check
            .as_ref()
            .map(|check| check.agent_hosts.clone())
            .unwrap_or_default();
        let view = cx.entity().downgrade();
        let mut chips = div().h_flex().flex_wrap().gap(px(4.));
        for host in agent_hosts.iter().take(1) {
            chips = chips.child(chip(host.clone(), cx));
        }
        for host in hosts.iter().take(HOSTS_SHOWN) {
            chips = chips.child(chip(host.clone(), cx));
        }
        if hosts.len() > HOSTS_SHOWN {
            chips = chips.child(muted(format!("+{} more", hosts.len() - HOSTS_SHOWN)));
        }
        if hosts.is_empty() {
            chips = chips.child(muted("No package hosts"));
        }
        chips = chips.child(chip("remotes · this repository", cx));
        if self.sandbox.hosts.is_none() {
            chips = chips.child(small_button("hosts-edit", "Edit allowlist…").on_click(
                move |_, window, cx| {
                    cx.stop_propagation();
                    view.update(cx, |this, cx| this.edit_hosts(window, cx)).ok();
                },
            ));
        }
        chips
    }

    fn render_hosts_editor(
        &self,
        editor: &Entity<TextareaState>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let mono = cx.theme().mono_font_family.clone();
        let view = cx.entity().downgrade();
        card()
            .p(px(12.))
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(color::TEXT_SECONDARY))
                    .child(
                        "Hosts Packages adds, one per line. The agent’s own API is always allowed.",
                    ),
            )
            .child(
                Textarea::new(editor)
                    .font_family(mono)
                    .text_size(px(12.))
                    .bg(rgb(color::FIELD)),
            )
            .when_some(self.sandbox.hosts_error.clone(), |this, error| {
                this.child(error_text(error))
            })
            .child(
                div()
                    .h_flex()
                    .gap(px(8.))
                    .child(
                        Button::new("hosts-reset")
                            .ghost()
                            .small()
                            .icon(Icon::new(IconName::RotateCcw).size(px(12.)))
                            .label("Reset to defaults")
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    view.update(cx, |this, cx| this.reset_hosts(window, cx))
                                        .ok();
                                }
                            }),
                    )
                    .child(
                        Button::new("hosts-cancel")
                            .ghost()
                            .small()
                            .ml_auto()
                            .label("Cancel")
                            .on_click({
                                let view = view.clone();
                                move |_, _, cx| {
                                    view.update(cx, |this, cx| {
                                        this.sandbox.hosts = None;
                                        this.sandbox.hosts_error = None;
                                        cx.notify();
                                    })
                                    .ok();
                                }
                            }),
                    )
                    .child(
                        Button::new("hosts-save")
                            .primary()
                            .small()
                            .label("Save hosts")
                            .on_click(move |_, _, cx| {
                                view.update(cx, |this, cx| this.save_hosts(cx)).ok();
                            }),
                    ),
            )
    }

    fn render_files(&self, cx: &mut Context<Self>) -> Div {
        let settings = self.model.read(cx).settings().clone();
        let hidden = &settings.sandbox.hidden_paths;
        let view = cx.entity().downgrade();
        let defaults: Vec<String> = DEFAULT_HIDDEN_PATHS
            .iter()
            .map(|p| (*p).to_owned())
            .collect();
        let writes = div()
            .h_flex()
            .gap(px(12.))
            .min_h(px(44.))
            .px(px(12.))
            .border_b_1()
            .border_color(rgb(color::ROW_DIVIDER))
            .text_size(px(12.5))
            .child(
                div()
                    .w(px(96.))
                    .flex_none()
                    .text_color(rgb(color::TEXT_MUTED))
                    .child("Writes"),
            )
            .child(div().flex_1().child("Only the thread’s worktree"))
            .child(
                div()
                    .h_flex()
                    .gap(px(5.))
                    .text_size(px(11.5))
                    .text_color(rgb(color::TEXT_MUTED))
                    .child(Icon::new(IconName::Lock).size(px(12.)))
                    .child("Always"),
            );
        let mut chips = div().h_flex().flex_wrap().gap(px(4.));
        for (index, path) in hidden.iter().enumerate() {
            let (view, path_text) = (view.clone(), path.clone());
            chips = chips.child(
                chip(path.clone(), cx).pr(px(2.)).gap(px(2.)).child(
                    Button::new(("unhide", index))
                        .ghost()
                        .size(px(16.))
                        .text_color(rgb(color::TEXT_MUTED))
                        .icon(Icon::new(IconName::Close).size(px(10.)))
                        .accessibility_label(format!("Stop hiding {path}"))
                        .tooltip(format!("Stop hiding {path}"))
                        .on_click(move |_, _, cx| {
                            view.update(cx, |this, cx| this.unhide_path(&path_text, cx))
                                .ok();
                        }),
                ),
            );
        }
        if hidden.is_empty() {
            chips = chips.child(muted("Nothing is hidden."));
        }
        if self.sandbox.hide.is_none() {
            let view = view.clone();
            chips = chips.child(
                Button::new("hide-path")
                    .ghost()
                    .compact()
                    .h(px(22.))
                    .px(px(6.))
                    .text_size(px(12.))
                    .text_color(rgb(color::TEXT_SECONDARY))
                    .icon(Icon::new(IconName::Plus).size(px(11.)))
                    .label("Hide a path")
                    .on_click(move |_, window, cx| {
                        view.update(cx, |this, cx| this.start_hiding(window, cx))
                            .ok();
                    }),
            );
        }
        if *hidden != defaults {
            let view = view.clone();
            chips = chips.child(small_button("hidden-reset", "Reset to defaults").on_click(
                move |_, _, cx| {
                    view.update(cx, |this, cx| {
                        this.reset_hidden_paths(cx);
                        cx.notify();
                    })
                    .ok();
                },
            ));
        }
        let mut reads_value = div()
            .flex_1()
            .min_w_0()
            .v_flex()
            .gap(px(6.))
            .child("The rest of the disk, read-only, except these hidden paths")
            .child(chips);
        if let Some(input) = &self.sandbox.hide {
            reads_value = reads_value.child(
                div()
                    .h_flex()
                    .gap(px(6.))
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(input)
                                .small()
                                .font_family(cx.theme().mono_font_family.clone())
                                .bg(rgb(color::FIELD)),
                        ),
                    )
                    .child(
                        Button::new("hide-choose")
                            .small()
                            .label("Choose…")
                            .on_click({
                                let view = view.clone();
                                move |_, window, cx| {
                                    view.update(cx, |this, cx| this.choose_hidden_path(window, cx))
                                        .ok();
                                }
                            }),
                    )
                    .child(
                        Button::new("hide-cancel")
                            .ghost()
                            .small()
                            .label("Cancel")
                            .on_click({
                                let view = view.clone();
                                move |_, _, cx| {
                                    view.update(cx, |this, cx| {
                                        this.sandbox.hide = None;
                                        this.sandbox.hide_error = None;
                                        cx.notify();
                                    })
                                    .ok();
                                }
                            }),
                    )
                    .child(
                        Button::new("hide-save")
                            .primary()
                            .small()
                            .label("Hide")
                            .on_click(move |_, window, cx| {
                                view.update(cx, |this, cx| this.hide_typed_path(window, cx))
                                    .ok();
                            }),
                    ),
            );
        }
        if let Some(error) = self.sandbox.hide_error.clone() {
            reads_value = reads_value.child(error_text(error));
        }
        let reads = div()
            .h_flex()
            .items_start()
            .gap(px(12.))
            .px(px(12.))
            .py(px(10.))
            .text_size(px(12.5))
            .child(
                div()
                    .w(px(96.))
                    .flex_none()
                    .text_color(rgb(color::TEXT_MUTED))
                    .child("Reads"),
            )
            .child(reads_value);
        card().child(writes).child(reads)
    }
}
