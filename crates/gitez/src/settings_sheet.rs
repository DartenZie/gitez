//! Settings for agent threads, as in the `C-4-Sandbox` board: secrets,
//! agent entries, the sandbox and its network and file rules, the
//! running-agent limit, and this repository's setup command. A modal sheet
//! over the window, opened with [`OpenSettings`] (the Agents popover's
//! gear, the app menu, `⌘,`, the composer's notices).
//!
//! Every change is saved as it is made: the settings through
//! [`AgentThreads::update_settings`] (`settings.json`), secret values in
//! the OS keychain (never shown again, never in a file), the setup command
//! in the repository's git config with the git CLI. Keychain calls, agent
//! and sandbox checks and git run off the UI thread.

mod agents;
mod edit;
mod repository;
mod sandbox;
mod secrets;
#[cfg(all(test, target_os = "macos"))]
mod shots;

use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::input::{
    InputEvent, InputState, NumberInput, NumberInputEvent, StepAction,
};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::agent_threads::{AgentThreads, OpenSettings};
use crate::theme::color;

/// The sheet's width; narrower windows shrink it.
const WIDTH: f32 = 1100.;
/// Space above and below the sheet.
const MARGIN: f32 = 32.;
/// Typing pauses this long before a text field is saved.
const SAVE_AFTER: Duration = Duration::from_millis(600);
/// The running-agent limit Settings allows.
const LIMIT_RANGE: std::ops::RangeInclusive<u32> = 1..=16;

/// Registers [`OpenSettings`] for every window and its key binding, the
/// platform's usual Settings shortcut (`⌘,`, `ctrl-,`).
pub fn init(cx: &mut App) {
    cx.on_action(|_: &OpenSettings, cx| {
        let window = cx
            .active_window()
            .or_else(|| cx.windows().into_iter().next());
        // A key binding or a view dispatches it while its window is being
        // updated; open the sheet once that is done.
        if let Some(window) = window {
            cx.defer(move |cx| {
                window.update(cx, |_, window, cx| open(window, cx)).ok();
            });
        }
    });
    cx.bind_keys([KeyBinding::new("secondary-,", OpenSettings, None)]);
}

/// The sheet open in each window, so a second `⌘,` does not stack another.
#[derive(Default)]
struct OpenSheets(Vec<(WindowId, WeakEntity<SettingsSheet>)>);

impl Global for OpenSheets {}

/// Opens the settings sheet over `window`, unless it is open there.
pub fn open(window: &mut Window, cx: &mut App) {
    let id = window.window_handle().window_id();
    // A closed sheet can outlive its dialog until the window draws again;
    // with no dialog shown, none is open here.
    let shown = window.has_active_dialog(cx);
    let open = cx.default_global::<OpenSheets>();
    open.0
        .retain(|(window, sheet)| sheet.upgrade().is_some() && (shown || *window != id));
    if open.0.iter().any(|(window, _)| *window == id) {
        return;
    }
    let sheet = cx.new(|cx| SettingsSheet::new(window, cx));
    cx.global_mut::<OpenSheets>()
        .0
        .push((id, sheet.downgrade()));
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .w(px(WIDTH))
            .margin_top(px(MARGIN))
            .close_button(false)
            .p_0()
            .bg(rgb(color::SHEET))
            .border_color(rgb(color::SHEET_BORDER))
            .rounded(px(14.))
            // Enter in a field (a secret's value, a hidden path) confirms
            // that field; it must not close the sheet as a dialog's OK.
            .on_ok(|_, _, _| false)
            .child(sheet.clone())
    });
}

/// The settings sheet. It holds what is being typed; what is saved lives
/// in [`AgentThreads`], the keychain and git config.
pub struct SettingsSheet {
    model: Entity<AgentThreads>,
    scroll: ScrollHandle,
    secrets: secrets::Secrets,
    agents: Vec<agents::AgentForm>,
    sandbox: sandbox::SandboxPanel,
    limit: Entity<InputState>,
    limit_error: Option<SharedString>,
    repository: Option<repository::RepositoryForm>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsSheet {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = AgentThreads::global(cx);
        let mut subscriptions = Vec::new();
        let limit_value = model.read(cx).settings().threads.max_running.to_string();
        let limit = cx.new(|cx| InputState::new(window, cx).default_value(limit_value));
        subscriptions.push(cx.subscribe_in(
            &limit,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::Change = event {
                    this.limit_typed(window, cx);
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &limit,
            window,
            |this, _, event: &NumberInputEvent, window, cx| {
                let NumberInputEvent::Step(action) = event;
                this.step_limit(*action, window, cx);
            },
        ));
        subscriptions.push(cx.observe_in(&model, window, |this, _, window, cx| {
            this.sync(window, cx);
        }));
        let secrets = secrets::Secrets::new(window, cx, &mut subscriptions);
        let agents = agents::AgentForm::all(&model, window, cx, &mut subscriptions);
        let mut sheet = Self {
            model,
            scroll: ScrollHandle::new(),
            secrets,
            agents,
            sandbox: sandbox::SandboxPanel::default(),
            limit,
            limit_error: None,
            repository: None,
            _subscriptions: subscriptions,
        };
        sheet.check_secrets(cx);
        sheet.check_agents(false, cx);
        sheet.check_sandbox(cx);
        sheet.sync_repository(window, cx);
        sheet
    }

    /// Follows the settings and the open repository: what changed
    /// elsewhere (or was just saved) is shown unless its field is being
    /// edited, and what depends on it is checked again.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.check_secrets(cx);
        self.sync_agents(window, cx);
        self.check_agents(false, cx);
        let limit = self.model.read(cx).settings().threads.max_running;
        let typed = self.limit.read(cx).value().trim().parse::<u32>().ok();
        if typed != Some(limit) && !is_focused(&self.limit, window, cx) {
            self.limit.update(cx, |input, cx| {
                input.set_value(limit.to_string(), window, cx)
            });
            self.limit_error = None;
        }
        self.sync_repository(window, cx);
        cx.notify();
    }

    fn limit_typed(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self.limit.read(cx).value().trim().to_owned();
        match text.parse::<u32>() {
            Ok(limit) if LIMIT_RANGE.contains(&limit) => {
                self.limit_error = None;
                self.set_limit(limit, cx);
            }
            _ if text.is_empty() => self.limit_error = None,
            _ => {
                self.limit_error =
                    Some(format!("From {} to {}.", LIMIT_RANGE.start(), LIMIT_RANGE.end()).into());
            }
        }
        cx.notify();
    }

    fn step_limit(&mut self, action: StepAction, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.model.read(cx).settings().threads.max_running;
        let next = match action {
            StepAction::Increment => current.saturating_add(1),
            StepAction::Decrement => current.saturating_sub(1),
        }
        .clamp(*LIMIT_RANGE.start(), *LIMIT_RANGE.end());
        self.limit.update(cx, |input, cx| {
            input.set_value(next.to_string(), window, cx)
        });
        self.limit_error = None;
        self.set_limit(next, cx);
    }

    fn set_limit(&mut self, limit: u32, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.update_settings(|settings| settings.threads.max_running = limit, cx);
        });
    }
}

impl Render for SettingsSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let height = (window.viewport_size().height - px(MARGIN * 2.)).max(px(320.));
        let error = self.model.read(cx).settings_error().cloned();
        let left = div()
            .flex_1()
            .min_w_0()
            .v_flex()
            .gap(px(22.))
            .child(self.render_secrets(window, cx))
            .child(self.render_agents(window, cx));
        let right = div()
            .flex_1()
            .min_w_0()
            .v_flex()
            .gap(px(22.))
            .child(self.render_sandbox(window, cx))
            .child(self.render_threads(window, cx));
        let body = div()
            .id("settings-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                div()
                    .v_flex()
                    .gap(px(16.))
                    .pt(px(18.))
                    .px(px(20.))
                    .pb(px(20.))
                    .when_some(error, |this, error| this.child(banner(error)))
                    .child(
                        div()
                            .h_flex()
                            .items_start()
                            .gap(px(28.))
                            .child(left)
                            .child(right),
                    ),
            );
        div()
            .id("settings-sheet")
            .debug_selector(|| "settings-sheet".into())
            .w_full()
            .h(height)
            .v_flex()
            .text_size(px(13.))
            .text_color(rgb(color::TEXT))
            .child(render_header())
            .child(body)
            .child(render_footer())
    }
}

fn render_header() -> Div {
    div()
        .h_flex()
        .gap(px(12.))
        .pl(px(20.))
        .pr(px(16.))
        .py(px(14.))
        .border_b_1()
        .border_color(rgb(color::SEPARATOR))
        .child(
            div()
                .size(px(32.))
                .flex_none()
                .rounded(px(8.))
                .bg(rgb(color::AGENT_MARK_BACKGROUND))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Icon::new(IconName::ShieldCheck)
                        .size(px(16.))
                        .text_color(rgb(color::ACCENT)),
                ),
        )
        .child(
            div()
                .v_flex()
                .gap(px(2.))
                .min_w_0()
                .child(
                    div()
                        .text_size(px(16.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Agents, sandbox and secrets"),
                )
                .child(muted(
                    "New threads use these settings. Running threads keep the ones they \
                     started with.",
                )),
        )
        .child(
            Button::new("settings-close")
                .ghost()
                .ml_auto()
                .size(px(30.))
                .text_color(rgb(color::TEXT_SECONDARY))
                .icon(Icon::new(IconName::Close).size(px(16.)))
                .accessibility_label("Close")
                .on_click(|_, window, cx| window.close_dialog(cx)),
        )
}

fn render_footer() -> Div {
    div()
        .h_flex()
        .gap(px(8.))
        .pl(px(20.))
        .pr(px(16.))
        .py(px(12.))
        .border_t_1()
        .border_color(rgb(color::SEPARATOR))
        .child(muted("Saved as you change them"))
        .child(
            Button::new("settings-done")
                .primary()
                .ml_auto()
                .label("Done")
                .on_click(|_, window, cx| window.close_dialog(cx)),
        )
}

impl SettingsSheet {
    /// Threads: the running-agent limit, and the open repository's own
    /// settings.
    fn render_threads(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let limit = kv_row(
            "Run at most",
            div()
                .v_flex()
                .gap(px(4.))
                .child(
                    div()
                        .h_flex()
                        .flex_wrap()
                        .gap(px(8.))
                        .child(
                            NumberInput::new(&self.limit)
                                .small()
                                .w(px(96.))
                                .accessibility_label("Agents running at once"),
                        )
                        .child(
                            div()
                                .text_size(px(12.5))
                                .child("agents at once, across all repositories"),
                        ),
                )
                .child(
                    muted("More threads wait in the queue until one finishes.").text_size(px(11.5)),
                )
                .when_some(self.limit_error.clone(), |this, error| {
                    this.child(error_text(error))
                }),
        );
        section("Threads").child(card().child(limit)).when_some(
            self.render_repository(window, cx),
            |this, repository| {
                this.child(label("In this repository").mt(px(2.)))
                    .child(repository)
            },
        )
    }
}

/// Whether `input` has the keyboard focus.
fn is_focused(input: &Entity<InputState>, window: &Window, cx: &App) -> bool {
    input.read(cx).focus_handle(cx).is_focused(window)
}

/// A section: its heading and what follows, spaced as on the board.
fn section(title: &'static str) -> Div {
    div().v_flex().gap(px(10.)).child(heading(title))
}

fn heading(title: &'static str) -> Div {
    div()
        .text_size(px(14.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(title)
}

/// A paragraph explaining a section.
fn description(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.5))
        .line_height(relative(1.55))
        .text_color(rgb(color::DESCRIPTION))
        .child(text.into())
}

/// A small uppercase label over a group.
fn label(text: &'static str) -> Div {
    div()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(color::TEXT_FAINT))
        .child(text.to_uppercase())
}

fn muted(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.))
        .text_color(rgb(color::TEXT_MUTED))
        .child(text.into())
}

fn error_text(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.))
        .line_height(relative(1.45))
        .text_color(rgb(color::DIFF_REMOVED))
        .child(text.into())
}

/// A bordered panel holding rows.
fn card() -> Div {
    div()
        .v_flex()
        .rounded(px(8.))
        .border_1()
        .border_color(rgb(color::CARD_BORDER))
        .bg(rgb(color::PANEL))
}

/// A row of a card: a muted key and its value.
fn kv_row(key: &'static str, value: impl IntoElement) -> Div {
    div()
        .h_flex()
        .items_start()
        .gap(px(12.))
        .min_h(px(44.))
        .px(px(12.))
        .py(px(8.))
        .border_b_1()
        .border_color(rgb(color::ROW_DIVIDER))
        .text_size(px(12.5))
        .child(
            div()
                .w(px(96.))
                .flex_none()
                .pt(px(6.))
                .text_color(rgb(color::TEXT_MUTED))
                .child(key),
        )
        .child(div().flex_1().min_w_0().child(value))
}

/// A host name or path in a chip.
fn chip(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .h_flex()
        .h(px(20.))
        .px(px(6.))
        .rounded(px(4.))
        .bg(rgb(color::HOST_CHIP))
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(px(10.5))
        .text_color(rgb(color::TEXT_SECONDARY))
        .whitespace_nowrap()
        .child(text.into())
}

/// A message across the sheet: settings that could not be read or saved.
fn banner(text: SharedString) -> Div {
    div()
        .h_flex()
        .items_start()
        .gap(px(10.))
        .px(px(12.))
        .py(px(10.))
        .rounded(px(8.))
        .border_1()
        .border_color(rgb(color::WARNING_BORDER))
        .bg(rgb(color::TRIAL_CONFLICT))
        .child(
            Icon::new(IconName::TriangleAlert)
                .size(px(14.))
                .mt(px(2.))
                .text_color(rgb(color::ACCENT)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(12.5))
                .line_height(relative(1.5))
                .text_color(rgb(color::WARNING_TEXT))
                .child(text),
        )
}

/// A small ghost button for actions inside a section.
fn small_button(id: impl Into<ElementId>, text: &'static str) -> Button {
    Button::new(id)
        .ghost()
        .compact()
        .h(px(22.))
        .px(px(6.))
        .text_size(px(12.))
        .text_color(rgb(color::TEXT_SECONDARY))
        .label(text)
}

/// The keychain the values are in, as the user knows it.
fn keychain_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "the macOS keychain"
    } else {
        "the keychain"
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use gpui_kit::component::WindowExt as _;
    use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext, WindowOptions};

    use super::SettingsSheet;
    use crate::agent_threads::{AgentThreads, Options, init_with};

    /// The sheet in a window of its own, with GitEZ's model on a scratch
    /// data folder and secrets kept in memory.
    pub(super) fn sheet(
        cx: &mut TestAppContext,
    ) -> (tempfile::TempDir, Entity<SettingsSheet>, VisualTestContext) {
        let dir = tempfile::tempdir().expect("temp dir");
        let data_dir = dir.path().join("data");
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::init(cx);
            crate::recent::init_in_memory(cx);
            init_with(
                Options {
                    data_dir: Some(data_dir),
                    sandbox: Arc::new(gitez_sandbox::Passthrough),
                    // No agent: nothing runs a real `claude` to check it.
                    agents: gitez_agent::Registry::with(Vec::new()),
                    secrets: Arc::new(gitez_secret::Memory::default()),
                    exe: Err("no supervisor in tests".into()),
                    watch: false,
                },
                cx,
            );
        });
        cx.run_until_parked();
        let (window, sheet) = cx
            .update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| SettingsSheet::new(window, cx))
                })
            })
            .expect("window open");
        let cx = VisualTestContext::from_window(window, cx);
        (dir, sheet, cx)
    }

    fn settings(cx: &mut VisualTestContext) -> gitez_settings::Settings {
        cx.update(|_, cx| (**AgentThreads::global(cx).read(cx).settings()).clone())
    }

    #[gpui_kit::test]
    fn writable_folders_are_added_and_removed(cx: &mut TestAppContext) {
        let (_dir, sheet, mut cx) = sheet(cx);
        sheet.update(&mut cx, |sheet, cx| {
            sheet.add_writable("/opt/gradle/caches/", cx)
        });
        assert_eq!(
            settings(&mut cx).sandbox.writable_paths,
            ["/opt/gradle/caches"]
        );
        // The whole disk is refused, and said why.
        sheet.update(&mut cx, |sheet, cx| sheet.add_writable("/", cx));
        assert_eq!(
            settings(&mut cx).sandbox.writable_paths,
            ["/opt/gradle/caches"]
        );
        sheet.read_with(&cx, |sheet, _| {
            assert!(sheet.sandbox_write_error().is_some());
        });
        sheet.update(&mut cx, |sheet, cx| {
            sheet.remove_writable("/opt/gradle/caches", cx)
        });
        assert!(settings(&mut cx).sandbox.writable_paths.is_empty());
    }

    #[gpui_kit::test]
    fn unhiding_a_credential_folder_asks_first(cx: &mut TestAppContext) {
        let (_dir, sheet, mut cx) = sheet(cx);
        assert!(
            settings(&mut cx)
                .sandbox
                .hidden_paths
                .contains(&"~/.aws".to_owned())
        );
        sheet.update_in(&mut cx, |sheet, window, cx| {
            sheet.ask_unhide_path("~/.aws", window, cx)
        });
        cx.run_until_parked();
        // Nothing changes until the user agrees.
        assert!(
            settings(&mut cx)
                .sandbox
                .hidden_paths
                .contains(&"~/.aws".to_owned())
        );
        assert!(cx.update(|window, cx| window.has_active_dialog(cx)));
    }
}
