//! Home, shown while no repository is open (the `C-5-Home` and
//! `C-6-Thread` boards): the one place to start agent threads in any
//! repository and to review what all of them produced.
//!
//! The sidebar lists the threads that need the owner (Finished: a pull
//! request ready, an answer, a failure or a stop), newest first, and below
//! them one collapsed section with the active, waiting and completed ones.
//! The main area is the launcher with the recent repositories and Open,
//! Clone and New repository, or the thread picked in the sidebar.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gitez_agent_thread::{AgentThread, State, Timestamp};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::agent_thread::{Look, ThreadView, ThreadViewEvent, ago, finished_at, state_icon};
use super::composer::{Composer, ComposerEvent, ComposerMode};
use super::labels;
use super::toolbar::agents::{
    Item, Part, RowAction, agents_indicator, complete_tooltip, elapsed, indicator_parts,
    render_notices, retry_tooltip, row_actions, status_line, status_line_element,
};
use super::toolbar::{home_button, repository_menu};
use super::{CloneRepository, InitRepository, OpenRepository};
use crate::agent_threads::{
    AgentThreadKey, AgentThreads, Counts, Notice, OpenSettings, StartRequest,
};
use crate::motion;
use crate::theme::{self, color};

#[cfg(all(test, target_os = "macos"))]
mod shots;

actions!(home, [NewThread]);

/// `⌘N` shows the launcher. The workspace handles it while Home is shown,
/// wherever the focus is in the window.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![KeyBinding::new("secondary-n", NewThread, Some("Workspace"))]
}

/// Width of the threads sidebar.
const SIDEBAR_WIDTH: Pixels = px(300.);
/// Width of the launcher and the recent repositories.
const MAIN_WIDTH: Pixels = px(860.);
/// Height of the buttons in a thread's row.
const ROW_BUTTON_HEIGHT: Pixels = px(22.);

/// What Home asks of the window.
pub enum HomeEvent {
    /// Open the repository at this path.
    OpenRepository(PathBuf),
    /// Open the repository whose git common dir this is, on local pull
    /// request `number`.
    OpenPullRequest { common_dir: PathBuf, number: u32 },
}

/// The threads of the sidebar, each list in the order shown.
#[derive(Default)]
pub(crate) struct Listed {
    /// Finished, not completed: what needs the owner. Newest first.
    pub finished: Vec<Item>,
    /// Preparing or running, newest first.
    pub active: Vec<Item>,
    /// Queued (in the order they start) or waiting for the usage limit.
    pub waiting: Vec<Item>,
    /// Completed, newest first.
    pub completed: Vec<Item>,
}

impl Listed {
    /// How many threads "Active and completed" holds.
    fn more(&self) -> usize {
        self.active.len() + self.waiting.len() + self.completed.len()
    }
}

/// The threads of every repository, or of the one whose git common dir is
/// `filter`, sorted into the sidebar's lists.
pub(crate) fn list(threads: &AgentThreads, filter: Option<&Path>) -> Listed {
    let mut listed = Listed::default();
    for repository in threads.repositories() {
        if filter.is_some_and(|filter| {
            threads
                .repository(filter)
                .is_none_or(|known| known.common_dir != repository.common_dir)
        }) {
            continue;
        }
        for thread in &repository.threads {
            let item = Item {
                common_dir: repository.common_dir.clone(),
                repository: repository.name.clone(),
                thread: thread.clone(),
                facts: repository
                    .facts
                    .get(&thread.id)
                    .cloned()
                    .unwrap_or_default(),
            };
            match thread.state {
                State::Finished => listed.finished.push(item),
                State::Preparing | State::Running => listed.active.push(item),
                State::Queued | State::Waiting => listed.waiting.push(item),
                State::Completed => listed.completed.push(item),
            }
        }
    }
    listed
        .finished
        .sort_by_key(|item| std::cmp::Reverse(finished_at(&item.thread)));
    listed
        .active
        .sort_by_key(|item| std::cmp::Reverse(item.thread.created));
    // Waiting for the usage limit first: they hold their place. Then the
    // queue, in the order it starts threads.
    listed
        .waiting
        .sort_by_key(|item| (item.thread.state == State::Queued, item.thread.created));
    listed.completed.sort_by_key(|item| {
        std::cmp::Reverse(item.thread.completed.unwrap_or(item.thread.updated))
    });
    listed
}

/// The place in the queue of every queued thread, across repositories:
/// 1 starts next.
fn queue_places(threads: &AgentThreads) -> HashMap<AgentThreadKey, usize> {
    let mut queued: Vec<(Timestamp, AgentThreadKey)> = threads
        .repositories()
        .iter()
        .flat_map(|repository| {
            repository
                .threads
                .iter()
                .filter(|thread| thread.state == State::Queued)
                .map(|thread| {
                    (
                        thread.created,
                        AgentThreadKey {
                            common_dir: repository.common_dir.clone(),
                            id: thread.id.clone(),
                        },
                    )
                })
        })
        .collect();
    queued.sort_by_key(|(created, _)| *created);
    queued
        .into_iter()
        .enumerate()
        .map(|(index, (_, key))| (key, index + 1))
        .collect()
}

/// "next", "2nd", "3rd", "11th": a place in the queue.
fn ordinal(place: usize) -> String {
    let suffix = match (place % 10, place % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    match place {
        1 => "next".to_owned(),
        _ => format!("{place}{suffix}"),
    }
}

/// What the thread started last from the launcher is doing now, for the
/// line under it.
fn started_line(thread: Option<&AgentThread>) -> &'static str {
    match thread.map(|thread| thread.state) {
        None => "Sent to the background. It is listed in the sidebar once it is read.",
        Some(State::Queued) => {
            "Sent to the background. It waits for a free slot and is listed under Waiting."
        }
        Some(State::Waiting) => {
            "Sent to the background. It waits for the agent’s usage limit and is listed under \
             Waiting."
        }
        Some(State::Preparing | State::Running) => {
            "Sent to the background. It runs on its own branch and is listed under Active."
        }
        Some(State::Finished) => "It has finished and is listed under Finished.",
        Some(State::Completed) => "It is completed and listed under Completed.",
    }
}

/// Home.
pub struct Home {
    focus_handle: FocusHandle,
    model: Entity<AgentThreads>,
    composer: Entity<Composer>,
    /// The git common dir of the repository whose threads are listed;
    /// `None`: every repository.
    filter: Option<PathBuf>,
    /// Whether "Active and completed" is expanded.
    more_open: bool,
    /// The thread shown instead of the launcher.
    thread: Option<(Entity<ThreadView>, Subscription)>,
    /// The thread started last from the launcher, marked in the sidebar.
    started: Option<AgentThreadKey>,
    /// A thread is being created.
    starting: bool,
    /// Threads a row's Retry or Complete runs for now.
    busy: HashSet<AgentThreadKey>,
    /// Why the last repository could not be opened.
    error: Option<SharedString>,
    /// The sidebar's entries as last drawn, and their virtual list: only
    /// the rows in view are laid out, however many threads there are.
    entries: Vec<SidebarEntry>,
    rows: ListState,
    /// Queued threads' places in the queue, for their rows.
    places: HashMap<AgentThreadKey, usize>,
    _subscriptions: Vec<Subscription>,
}

/// An entry of the sidebar's list.
#[derive(Debug, Clone, PartialEq)]
enum SidebarEntry {
    /// What could not be read, watched or completed.
    Notices(Vec<Notice>),
    /// "Finished" with how many.
    Finished(usize),
    Note(&'static str),
    Row(Box<Item>, Kind),
    /// "Active and completed" with how many, to expand or collapse.
    More(usize),
    Group(&'static str, &'static str),
}

impl EventEmitter<HomeEvent> for Home {}

impl Home {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = AgentThreads::global(cx);
        let composer = cx.new(|cx| Composer::new(ComposerMode::Home, window, cx));
        let subscriptions = vec![
            cx.subscribe_in(
                &composer,
                window,
                |this, _, event, window, cx| match event {
                    ComposerEvent::Submit(request) => this.start((**request).clone(), window, cx),
                    // Esc in the launcher: nothing to close.
                    ComposerEvent::Cancel => {}
                },
            ),
            cx.observe(&model, |this, model, cx| {
                // A filtered repository whose threads are gone lists all.
                if this
                    .filter
                    .as_ref()
                    .is_some_and(|filter| model.read(cx).repository(filter).is_none())
                {
                    this.filter = None;
                }
                cx.notify();
            }),
            cx.observe_global::<crate::recent::Recent>(|_, cx| cx.notify()),
        ];
        Self {
            focus_handle: cx.focus_handle(),
            model,
            composer,
            filter: None,
            more_open: false,
            thread: None,
            started: None,
            starting: false,
            busy: HashSet::new(),
            error: None,
            entries: Vec::new(),
            rows: ListState::new(0, ListAlignment::Top, px(300.)),
            places: HashMap::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Home is on screen again: the agents, the sandbox and the launcher's
    /// repository are read again, and the cursor goes to the prompt unless
    /// a thread is shown.
    pub fn shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A recent repository may have been moved or deleted since.
        crate::recent::check(cx);
        self.composer.update(cx, |composer, cx| {
            composer.check(cx);
            composer.refresh(cx);
        });
        if self.thread.is_none() {
            self.composer
                .update(cx, |composer, cx| composer.focus(window, cx));
        }
        cx.notify();
    }

    /// Shows why the last repository could not be opened; `None` hides it.
    pub fn set_error(&mut self, error: Option<SharedString>, cx: &mut Context<Self>) {
        self.error = error;
        cx.notify();
    }

    /// `⌘N`, "New thread": the launcher, with the cursor in its prompt.
    pub fn new_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.thread = None;
        self.composer
            .update(cx, |composer, cx| composer.focus(window, cx));
        cx.notify();
    }

    /// The thread shown instead of the launcher, if one is.
    pub fn shown_thread(&self, cx: &App) -> Option<AgentThreadKey> {
        self.thread
            .as_ref()
            .map(|(view, _)| view.read(cx).key().clone())
    }

    /// Shows thread `key` in the main area, wide.
    pub fn show_thread(
        &mut self,
        key: AgentThreadKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shown_thread(cx).as_ref() == Some(&key) {
            return;
        }
        let view = cx.new(|cx| ThreadView::new(key, true, window, cx));
        let subscription = cx.subscribe_in(&view, window, |_, _, event, _, cx| match event {
            ThreadViewEvent::OpenPullRequest { common_dir, number } => {
                cx.emit(HomeEvent::OpenPullRequest {
                    common_dir: common_dir.clone(),
                    number: *number,
                })
            }
            ThreadViewEvent::OpenRepository { work_dir } => {
                cx.emit(HomeEvent::OpenRepository(work_dir.clone()))
            }
        });
        self.thread = Some((view, subscription));
        cx.notify();
    }

    fn set_filter(&mut self, filter: Option<PathBuf>, cx: &mut Context<Self>) {
        self.filter = filter;
        cx.notify();
    }

    fn toggle_more(&mut self, cx: &mut Context<Self>) {
        self.more_open = !self.more_open;
        cx.notify();
    }

    /// Starts the thread the launcher asks for; control returns at once and
    /// the thread shows in the sidebar. If it cannot start, the prompt goes
    /// back into the launcher.
    fn start(&mut self, request: StartRequest, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = request.prompt.clone();
        let common_dir = request.common_dir.clone();
        let start = self.model.update(cx, |model, cx| model.start(request, cx));
        self.starting = true;
        cx.spawn_in(window, async move |this, cx| {
            let started = start.await;
            this.update_in(cx, |this, window, cx| {
                this.starting = false;
                match started {
                    Ok(key) => {
                        this.started = Some(key);
                        // Where it is listed: shown, and not filtered out.
                        this.more_open = true;
                        if this
                            .filter
                            .as_ref()
                            .is_some_and(|filter| *filter != common_dir)
                        {
                            this.filter = None;
                        }
                    }
                    Err(error) => {
                        window.push_notification(
                            crate::toast::error(format!(
                                "Could not start the agent thread: {error:#}"
                            )),
                            cx,
                        );
                        this.composer.update(cx, |composer, cx| {
                            composer.set_prompt(prompt, window, cx);
                            composer.focus(window, cx);
                        });
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// A row's Retry or Complete: asks the model, the row's buttons waiting
    /// until it is done; errors as toasts.
    fn run(
        &mut self,
        action: RowAction,
        key: AgentThreadKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task = match action {
            RowAction::PullRequest(number) => {
                cx.emit(HomeEvent::OpenPullRequest {
                    common_dir: key.common_dir.clone(),
                    number,
                });
                return;
            }
            RowAction::Retry(network) => self
                .model
                .update(cx, |model, cx| model.retry(&key, network, cx)),
            RowAction::Complete => {
                let view = cx.entity().downgrade();
                let busy_key = key.clone();
                let busy: super::agent_thread::complete::Busy =
                    std::rc::Rc::new(move |busy, _, cx| {
                        view.update(cx, |this, cx| {
                            if busy {
                                this.busy.insert(busy_key.clone());
                            } else {
                                this.busy.remove(&busy_key);
                            }
                            cx.notify();
                        })
                        .ok();
                    });
                super::agent_thread::complete::complete(key, busy, window, cx);
                return;
            }
            RowAction::Stop | RowAction::Cancel => {
                self.model.update(cx, |model, cx| model.stop(&key, cx))
            }
        };
        self.busy.insert(key.clone());
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.busy.remove(&key);
                if let Err(error) = result {
                    window.push_notification(crate::toast::error(format!("{error:#}")), cx)
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Focusable for Home {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Which list a row is in, for how it looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Finished,
    Active,
    Waiting,
    Completed,
}

/// A section's heading, with its count at the end.
fn section_heading(title: &'static str, count: Option<usize>, cx: &App) -> Div {
    div()
        .h_flex()
        .gap(px(8.))
        .px(px(16.))
        .pt(px(14.))
        .pb(px(6.))
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(color::TEXT_FAINT))
        .child(title.to_uppercase())
        .when_some(count, |this, count| {
            this.child(
                div()
                    .ml_auto()
                    .font_family(cx.theme().mono_font_family.clone())
                    .font_weight(FontWeight::MEDIUM)
                    .child(count.to_string()),
            )
        })
}

/// A group's heading inside "Active and completed".
fn group_heading(title: &'static str, selector: &'static str) -> Div {
    div()
        .debug_selector(move || selector.into())
        .px(px(16.))
        .pt(px(10.))
        .pb(px(4.))
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(color::TEXT_FAINT))
        .child(title.to_uppercase())
}

/// A muted line in place of an empty list.
fn note(text: &'static str) -> Div {
    div()
        .px(px(16.))
        .pt(px(4.))
        .pb(px(8.))
        .text_size(px(12.))
        .line_height(relative(1.5))
        .text_color(rgb(color::TEXT_MUTED))
        .child(text)
}

/// A rounded square with the first letter of `name`: amber for the
/// repository picked, grey for others.
fn initial_square(name: &str, size: Pixels, amber: bool) -> Div {
    let letter: String = name
        .chars()
        .find(|char| char.is_alphanumeric())
        .map(|char| char.to_uppercase().collect())
        .unwrap_or_default();
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(size)
        .rounded(size * 0.25)
        .bg(rgb(if amber { color::ACCENT } else { color::AVATAR }))
        .text_color(rgb(if amber {
            color::ON_ACCENT
        } else {
            color::TEXT_SECONDARY
        }))
        .text_size(size * 0.46)
        .font_weight(FontWeight::BOLD)
        .child(letter)
}

/// The keycaps of a shortcut such as `⌘N`.
fn shortcut(key: &'static str, cx: &App) -> Div {
    div()
        .h_flex()
        .flex_none()
        .gap(px(3.))
        .child(labels::key_cap(labels::SECONDARY_KEY, cx))
        .child(labels::key_cap(key, cx))
}

/// The folder name of `path`, as a repository's name.
fn folder_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

impl Home {
    fn render_top_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let focus = self.focus_handle.clone();
        let switcher = Button::new("home-repository")
            .debug_selector(|| "home-repository".into())
            .ghost()
            .h(px(30.))
            .pl(px(6.))
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(22.))
                    .rounded(px(6.))
                    .border_1()
                    .border_dashed()
                    .border_color(rgb(color::TAG_LABEL_BORDER))
                    .child(
                        Icon::new(IconName::Folder)
                            .size(px(12.))
                            .text_color(rgb(color::TEXT_MUTED)),
                    ),
            )
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Open repository…"),
            )
            .dropdown_caret(true)
            .tooltip("Open a repository")
            .dropdown_menu(move |menu, _, cx| {
                repository_menu(menu, None, &focus, cx)
                    .menu_with_icon("Clone…", IconName::Download, Box::new(CloneRepository))
                    .menu_with_icon(
                        "New repository…",
                        IconName::FolderPlus,
                        Box::new(InitRepository),
                    )
            });
        div()
            .h_flex()
            .flex_none()
            .h(theme::TOOLBAR_HEIGHT)
            .px(px(14.))
            .gap(px(10.))
            .bg(rgb(color::PANEL))
            .border_b_1()
            .border_color(rgb(color::BORDER))
            .child(home_button(true))
            .child(switcher)
            .child(div().flex_1())
            .child(agents_indicator(
                None,
                self.focus_handle.clone(),
                window,
                cx,
            ))
    }

    /// The sidebar's entries for `listed`.
    fn sidebar_entries(&self, listed: Listed, cx: &App) -> Vec<SidebarEntry> {
        let model = self.model.read(cx);
        let mut entries = Vec::new();
        let notices = model.notices(self.filter.as_deref());
        if !notices.is_empty() {
            entries.push(SidebarEntry::Notices(notices));
        }
        let any_thread = model
            .repositories()
            .iter()
            .any(|repository| !repository.threads.is_empty());
        entries.push(SidebarEntry::Finished(listed.finished.len()));
        if listed.finished.is_empty() {
            entries.push(SidebarEntry::Note(if any_thread {
                "No finished threads"
            } else {
                "No threads yet. Ask an agent in the launcher: each thread works on its own \
                 branch in its own worktree, and shows here when it needs you."
            }));
        }
        let more = listed.more();
        entries.extend(
            listed
                .finished
                .into_iter()
                .map(|item| SidebarEntry::Row(Box::new(item), Kind::Finished)),
        );
        entries.push(SidebarEntry::More(more));
        if self.more_open {
            if more == 0 {
                entries.push(SidebarEntry::Note(
                    "No active, waiting or completed threads",
                ));
            }
            for (title, selector, items, kind) in [
                ("Active", "home-group-active", listed.active, Kind::Active),
                (
                    "Waiting",
                    "home-group-waiting",
                    listed.waiting,
                    Kind::Waiting,
                ),
                (
                    "Completed",
                    "home-group-completed",
                    listed.completed,
                    Kind::Completed,
                ),
            ] {
                if items.is_empty() {
                    continue;
                }
                entries.push(SidebarEntry::Group(title, selector));
                entries.extend(
                    items
                        .into_iter()
                        .map(|item| SidebarEntry::Row(Box::new(item), kind)),
                );
            }
        }
        entries
    }

    /// Takes in the sidebar's entries for this frame: the ones that changed
    /// are measured again, the others keep their place.
    fn set_entries(&mut self, entries: Vec<SidebarEntry>) {
        if entries == self.entries {
            return;
        }
        let prefix = self
            .entries
            .iter()
            .zip(&entries)
            .take_while(|(old, new)| old == new)
            .count();
        let suffix = self.entries[prefix..]
            .iter()
            .rev()
            .zip(entries[prefix..].iter().rev())
            .take_while(|(old, new)| old == new)
            .count();
        if self.rows.item_count() == self.entries.len() {
            self.rows.splice(
                prefix..self.entries.len() - suffix,
                entries.len() - prefix - suffix,
            );
        } else {
            self.rows.reset(entries.len());
        }
        self.entries = entries;
    }

    /// Entry `index` of the sidebar's list.
    fn render_entry(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(entry) = self.entries.get(index) else {
            return div().into_any_element();
        };
        match entry {
            SidebarEntry::Notices(notices) => div()
                .pb(px(4.))
                .child(render_notices(notices.clone(), "home-notice", cx))
                .into_any_element(),
            SidebarEntry::Finished(count) => {
                section_heading("Finished", Some(*count), cx).into_any_element()
            }
            SidebarEntry::Note(text) => note(text).into_any_element(),
            SidebarEntry::Row(item, kind) => self
                .render_row(item, *kind, &self.places, Timestamp::now(), cx)
                .into_any_element(),
            SidebarEntry::More(more) => {
                let toggle = div()
                    .id("home-more")
                    .debug_selector(|| "home-more".into())
                    .h_flex()
                    .gap(px(8.))
                    .h(px(34.))
                    .px(px(16.))
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(color::TEXT_SECONDARY))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(color::NAV_HOVER)).text_color(rgb(color::TEXT)))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_more(cx)))
                    .child(
                        Icon::new(if self.more_open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(px(12.)),
                    )
                    .child("Active and completed")
                    .child(
                        div()
                            .ml_auto()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_size(px(11.))
                            .text_color(rgb(color::TEXT_FAINT))
                            .child(more.to_string()),
                    );
                div()
                    .v_flex()
                    .mt(px(12.))
                    .border_t_1()
                    .border_color(rgb(color::ROW_DIVIDER))
                    .child(toggle)
                    .into_any_element()
            }
            SidebarEntry::Group(title, selector) => {
                group_heading(title, selector).into_any_element()
            }
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> Div {
        let launcher = self.thread.is_none();
        let new_thread = div()
            .id("home-new-thread")
            .debug_selector(|| "home-new-thread".into())
            .h_flex()
            .gap(px(6.))
            .h(px(30.))
            .px(px(10.))
            .rounded(theme::RADIUS)
            .border_1()
            .border_color(if launcher {
                rgba(color::PICKER_ON_BORDER)
            } else {
                rgb(color::BUTTON_BORDER)
            })
            .bg(rgb(if launcher {
                color::SELECTED
            } else {
                color::BUTTON
            }))
            .text_size(px(12.5))
            .cursor_pointer()
            .when(!launcher, |this| {
                this.hover(|style| style.bg(rgb(color::BUTTON_HOVER)))
            })
            .on_click(cx.listener(|this, _, window, cx| this.new_thread(window, cx)))
            .child(
                Icon::new(IconName::Plus)
                    .size(px(14.))
                    .text_color(rgb(color::ACCENT)),
            )
            .child(div().flex_1().child("New thread"))
            .child(shortcut("N", cx));

        let model = self.model.read(cx);
        let filter_label: SharedString = self
            .filter
            .as_ref()
            .and_then(|filter| model.repository(filter))
            .map_or_else(
                || "All repositories".into(),
                |repository| repository.name.clone(),
            );
        let choices: Vec<(PathBuf, SharedString)> = model
            .repositories()
            .iter()
            .map(|repository| (repository.common_dir.clone(), repository.name.clone()))
            .collect();
        let current = self.filter.clone();
        let view = cx.entity().downgrade();
        let filter = Button::new("home-filter")
            .debug_selector(|| "home-filter".into())
            .w_full()
            .h(px(26.))
            .compact()
            .text_size(px(12.))
            .justify_start()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(filter_label),
            )
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu.min_w(px(220.)).check_side(Side::Right);
                let entries = std::iter::once((None, SharedString::from("All repositories")))
                    .chain(
                        choices
                            .iter()
                            .map(|(common_dir, name)| (Some(common_dir.clone()), name.clone())),
                    );
                for (index, (common_dir, name)) in entries.enumerate() {
                    let view = view.clone();
                    let checked = current == common_dir;
                    menu = menu.item(PopupMenuItem::new(name).checked(checked).on_click(
                        move |_, _, cx| {
                            view.update(cx, |this, cx| this.set_filter(common_dir.clone(), cx))
                                .ok();
                        },
                    ));
                    if index == 0 && !choices.is_empty() {
                        menu = menu.separator();
                    }
                }
                menu
            });

        let top = div()
            .v_flex()
            .gap(px(8.))
            .px(px(10.))
            .pt(px(10.))
            .pb(px(4.))
            .child(new_thread)
            .child(
                div()
                    .h_flex()
                    .gap(px(8.))
                    .px(px(2.))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.5))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child("Repository"),
                    )
                    .child(div().flex_1().min_w_0().child(filter)),
            );

        let limit = model.settings().threads.max_running as usize;
        let running = model.running();
        let waiting = model.counts(None).waiting;
        let list = gpui_kit::list(
            self.rows.clone(),
            // Each entry takes the list's width, so long rows shorten
            // their text rather than overflow.
            cx.processor(|this, index: usize, _, cx| {
                div()
                    .w_full()
                    .child(this.render_entry(index, cx))
                    .into_any_element()
            }),
        )
        .w_full()
        .flex_1()
        .min_h_0()
        .pb(px(10.));

        let mut slots = format!("{running} of {limit} agents running");
        if waiting > 0 {
            slots.push_str(&format!(" · {waiting} waiting"));
        }
        let footer = div()
            .h_flex()
            .flex_none()
            .gap(px(8.))
            .h(px(40.))
            .pl(px(16.))
            .pr(px(8.))
            .border_t_1()
            .border_color(rgb(color::BORDER))
            .text_size(px(11.5))
            .text_color(rgb(color::TEXT_MUTED))
            .child(if running > 0 {
                motion::blinking_dot(px(6.), color::BRANCH_BLUE).into_any_element()
            } else {
                div()
                    .flex_none()
                    .size(px(6.))
                    .rounded_full()
                    .bg(rgb(color::TEXT_FAINT))
                    .into_any_element()
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(slots),
            )
            .child(
                Button::new("home-settings")
                    .ghost()
                    .size(px(30.))
                    .text_color(rgb(color::TEXT_SECONDARY))
                    .icon(Icon::new(IconName::Settings).size(px(15.)))
                    .accessibility_label("Agents, sandbox and secrets settings")
                    .tooltip("Agents, sandbox and secrets settings")
                    .on_click(|_, window, cx| {
                        window.dispatch_action(OpenSettings.boxed_clone(), cx)
                    }),
            );

        div()
            .v_flex()
            .flex_none()
            .w(SIDEBAR_WIDTH)
            .h_full()
            .bg(rgb(color::SIDEBAR))
            .border_r_1()
            .border_color(rgb(color::BORDER))
            .child(top)
            .child(list)
            .child(footer)
    }

    /// One thread in the sidebar: its state's mark, title and when, then
    /// its repository and state, and the actions that fit it.
    fn render_row(
        &self,
        item: &Item,
        kind: Kind,
        places: &HashMap<AgentThreadKey, usize>,
        now: Timestamp,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let thread = &item.thread;
        let key = item.key();
        let selected = self.shown_thread(cx).as_ref() == Some(&key);
        let fresh = !selected && self.started.as_ref() == Some(&key);
        let busy = self.busy.contains(&key);
        let mono = cx.theme().mono_font_family.clone();
        let when = match kind {
            Kind::Finished => ago(finished_at(thread), now),
            Kind::Active => elapsed(thread, now),
            Kind::Waiting => places
                .get(&key)
                .map(|place| ordinal(*place))
                .unwrap_or_default(),
            Kind::Completed => ago(thread.completed.unwrap_or(thread.updated), now),
        };
        let title = div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_size(px(12.5))
            .map(|this| {
                if kind == Kind::Completed {
                    this.text_color(rgb(color::TEXT_MUTED))
                } else {
                    this.font_weight(FontWeight::MEDIUM)
                }
            })
            .child(thread.title.clone());
        let first = div().h_flex().gap(px(8.)).min_w_0().child(title).child(
            div()
                .flex_none()
                .text_size(px(11.5))
                .text_color(rgb(color::TEXT_MUTED))
                .child(when),
        );

        let repository = div()
            .flex_none()
            .text_color(rgb(if kind == Kind::Completed {
                color::TEXT_MUTED
            } else {
                color::TEXT_SECONDARY
            }))
            .child(item.repository.clone());
        let dot = || div().flex_none().child("·");
        let ellipsis = || {
            div()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
        };
        let mut second = div()
            .h_flex()
            .gap(px(6.))
            .min_w_0()
            .text_size(px(11.5))
            .text_color(rgb(color::TEXT_MUTED))
            .child(repository);
        let mut buttons = Vec::new();
        match kind {
            Kind::Finished | Kind::Completed => {
                let model = self.model.read(cx);
                let limit = model.settings().threads.max_running as usize;
                let spans = status_line(thread, model.agents(), &item.facts, None, limit, now);
                second = second
                    .min_h(px(22.))
                    .child(dot())
                    .child(status_line_element(spans, cx).flex_1().min_w_0());
                if kind == Kind::Finished {
                    let model = self.model.read(cx);
                    let actions = row_actions(
                        thread,
                        model.agents(),
                        &model.settings().sandbox.package_hosts,
                    );
                    for (index, action) in actions.into_iter().enumerate() {
                        buttons.push(self.row_button(action, index, thread, &key, busy, cx));
                    }
                }
            }
            Kind::Active => {
                let preparing = thread.state == State::Preparing;
                let (tool, detail) = match &thread.activity {
                    Some(activity) => (activity.tool.clone(), activity.detail.clone()),
                    None if preparing => (Some("Setup".to_owned()), "creating the worktree".into()),
                    None => (None, "starting the agent".to_owned()),
                };
                let look = Look::of(thread);
                second = second.child(dot()).child(
                    div()
                        .flex_none()
                        .text_color(rgb(look.text()))
                        .child(look.word()),
                );
                if let Some(tool) = tool {
                    second = second.child(
                        div()
                            .flex_none()
                            .h(px(17.))
                            .px(px(5.))
                            .rounded(px(4.))
                            .border_1()
                            .border_color(rgb(color::TOOL_CHIP_BORDER))
                            .bg(rgb(color::TOOL_CHIP))
                            .font_family(mono.clone())
                            .text_size(px(10.5))
                            .line_height(px(15.))
                            .text_color(rgb(color::TEXT_SECONDARY))
                            .child(tool),
                    );
                }
                second = second.child(
                    ellipsis()
                        .flex_1()
                        .font_family(mono)
                        .text_size(px(11.))
                        .child(detail),
                );
            }
            Kind::Waiting => {
                let model = self.model.read(cx);
                let limit = model.settings().threads.max_running as usize;
                let spans = status_line(thread, model.agents(), &item.facts, None, limit, now);
                second = second
                    .child(dot())
                    .child(status_line_element(spans, cx).flex_1().min_w_0());
            }
        }
        // Retry and Complete go under the line, as in the popover: beside
        // them the state would not fit.
        let model = self.model.read(cx);
        let wide_buttons = row_actions(
            thread,
            model.agents(),
            &model.settings().sandbox.package_hosts,
        )
        .iter()
        .any(|action| matches!(action, RowAction::Retry(_)));
        let text = div()
            .v_flex()
            .flex_1()
            .min_w_0()
            .gap(px(3.))
            .child(first)
            .map(|this| {
                if buttons.is_empty() {
                    this.child(second)
                } else if wide_buttons {
                    this.child(second)
                        .child(div().h_flex().gap(px(4.)).pt(px(2.)).children(buttons))
                } else {
                    this.child(
                        second.child(
                            div()
                                .h_flex()
                                .flex_none()
                                .ml_auto()
                                .gap(px(4.))
                                .children(buttons),
                        ),
                    )
                }
            });

        let id = SharedString::from(format!("home-thread-{}", thread.id.as_str()));
        let open = cx.listener({
            let key = key.clone();
            move |this, _: &ClickEvent, window, cx| this.show_thread(key.clone(), window, cx)
        });
        div()
            .id(id.clone())
            .debug_selector(move || id.to_string())
            .h_flex()
            .items_start()
            .gap(px(10.))
            .mx(px(6.))
            .px(px(10.))
            .py(px(7.))
            .rounded(px(8.))
            .border_1()
            .border_color(if fresh {
                theme::with_alpha(color::ACCENT, 0.35)
            } else {
                transparent_black().into()
            })
            .cursor_pointer()
            .map(|this| {
                if selected {
                    this.bg(rgb(color::SELECTED))
                } else if fresh {
                    this.bg(rgb(color::OPTION_ON))
                } else {
                    this.hover(|style| style.bg(rgb(color::NAV_HOVER)))
                }
            })
            .on_click(open)
            .child(state_icon(thread, item.facts.pull_request, true))
            .child(text)
    }

    /// The button of `action` on the row of `thread`.
    fn row_button(
        &self,
        action: RowAction,
        index: usize,
        thread: &AgentThread,
        key: &AgentThreadKey,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> Button {
        let name = match action {
            RowAction::Retry(_) => "retry",
            RowAction::Complete => "complete",
            RowAction::PullRequest(_) => "pull-request",
            RowAction::Stop => "stop",
            RowAction::Cancel => "cancel",
        };
        let selector = format!("home-action-{}-{name}", thread.id.as_str());
        let button = Button::new(SharedString::from(format!(
            "home-action-{}-{index}",
            thread.id.as_str()
        )))
        .debug_selector(move || selector.clone())
        .compact()
        .h(ROW_BUTTON_HEIGHT)
        .px(px(7.))
        .gap(px(5.))
        .text_size(px(11.5))
        .label(action.label(thread.network))
        .disabled(busy);
        let button = match action {
            RowAction::Retry(network) => button
                .icon(Icon::new(IconName::RotateCcw).size(px(12.)))
                .loading(busy)
                .tooltip(retry_tooltip(thread, network)),
            RowAction::Complete => button
                .icon(Icon::new(IconName::Check).size(px(12.)))
                .loading(busy)
                .tooltip(complete_tooltip(thread)),
            RowAction::PullRequest(number) => button
                .label(format!("PR #{number}"))
                .icon(Icon::new(IconName::GitPullRequestArrow).size(px(12.)))
                .tooltip(format!("Open PR #{number} in its repository")),
            RowAction::Stop | RowAction::Cancel => button.loading(busy),
        };
        let key = key.clone();
        button.on_click(cx.listener(move |this, _, window, cx| {
            cx.stop_propagation();
            this.run(action, key.clone(), window, cx);
        }))
    }

    /// The launcher, the line about the thread started last, and the
    /// recent repositories with Open, Clone and New repository.
    fn render_launcher(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let started = self.started.as_ref().map(|key| {
            let thread = self.model.read(cx).thread(key).cloned();
            let key = key.clone();
            div()
                .h_flex()
                .gap(px(8.))
                .px(px(2.))
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_SECONDARY))
                .child(
                    Icon::new(IconName::Check)
                        .size(px(14.))
                        .text_color(rgb(color::DIFF_ADDED)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(started_line(thread.as_deref())),
                )
                .child(
                    Button::new("home-open-started")
                        .ghost()
                        .compact()
                        .h(px(24.))
                        .text_size(px(12.))
                        .label("Open thread")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.show_thread(key.clone(), window, cx)
                        })),
                )
        });
        let starting = self.starting.then(|| {
            div()
                .px(px(2.))
                .text_size(px(12.))
                .text_color(rgb(color::TEXT_MUTED))
                .child("Creating the thread…")
        });
        let error = self.error.clone().map(|error| {
            div()
                .h_flex()
                .items_start()
                .gap(px(10.))
                .px(px(14.))
                .py(px(10.))
                .rounded(px(8.))
                .border_1()
                .border_color(theme::with_alpha(color::DIFF_REMOVED, 0.4))
                .bg(rgba(color::THREAD_FAILED))
                .child(
                    Icon::new(IconName::CircleAlert)
                        .size(px(15.))
                        .mt(px(1.))
                        .text_color(rgb(color::THREAD_FAILED_TEXT)),
                )
                .child(
                    div()
                        .v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Could not open the repository"),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .line_height(relative(1.5))
                                .text_color(rgb(color::TEXT_SECONDARY))
                                .child(error),
                        ),
                )
                .child(
                    Button::new("home-error-close")
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::Close).size(px(13.)))
                        .accessibility_label("Hide this message")
                        .tooltip("Hide this message")
                        .on_click(cx.listener(|this, _, _, cx| this.set_error(None, cx))),
                )
        });
        let launcher = div()
            .v_flex()
            .gap(px(12.))
            .child(self.composer.clone())
            .children(starting)
            .children(started);

        div()
            .id("home-main")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .child(
                div()
                    .v_flex()
                    .w_full()
                    .max_w(MAIN_WIDTH)
                    .mx_auto()
                    .px(px(24.))
                    .pt(px(if self.error.is_some() { 48. } else { 104. }))
                    .pb(px(48.))
                    .gap(px(36.))
                    .children(error)
                    .child(launcher)
                    .child(self.render_recent(cx)),
            )
    }

    fn render_recent(&self, cx: &mut Context<Self>) -> Div {
        let recent = crate::recent::paths(cx).to_vec();
        let model = self.model.read(cx);
        let mono = cx.theme().mono_font_family.clone();
        let mut section = div().v_flex().gap(px(2.)).child(
            div()
                .px(px(10.))
                .pb(px(6.))
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(color::TEXT_FAINT))
                .child("RECENT REPOSITORIES"),
        );
        if recent.is_empty() {
            section = section.child(
                div()
                    .px(px(10.))
                    .pb(px(4.))
                    .text_size(px(12.5))
                    .text_color(rgb(color::TEXT_MUTED))
                    .child("None yet. Open a repository to see its history, clone one, or create a new one."),
            );
        }
        for (index, path) in recent.iter().enumerate() {
            let name = folder_name(path);
            let badges = model
                .repository_by_work_dir(path)
                .map(|repository| badges(&repository.threads))
                .unwrap_or_default();
            // Moved or deleted: muted, cannot be opened, can be removed.
            let unavailable = crate::recent::unavailable(path, model, cx);
            let open = path.clone();
            let remove = unavailable.is_some().then(|| {
                let path = path.clone();
                Button::new(("home-recent-remove", index))
                    .debug_selector(move || format!("home-recent-remove-{index}"))
                    .compact()
                    .h(px(24.))
                    .text_size(px(11.5))
                    .label("Remove from list")
                    .on_click(move |_, _, cx| crate::recent::remove(&path, cx))
            });
            section = section.child(
                div()
                    .id(("home-recent", index))
                    .debug_selector(move || format!("home-recent-{index}"))
                    .h_flex()
                    .gap(px(12.))
                    .min_h(px(46.))
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(8.))
                    .when(unavailable.is_none(), |this| {
                        this.cursor_pointer()
                            .hover(|style| style.bg(rgb(color::ROW_HOVER)))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(HomeEvent::OpenRepository(open.clone()))
                            }))
                    })
                    .child(
                        div()
                            .flex_none()
                            .when(unavailable.is_some(), |this| this.opacity(0.55))
                            .child(initial_square(&name, px(28.), index == 0)),
                    )
                    .child(
                        div()
                            .v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .when(unavailable.is_some(), |this| this.opacity(0.55))
                            .child(
                                div()
                                    .h_flex()
                                    .gap(px(8.))
                                    .child(div().font_weight(FontWeight::MEDIUM).child(name))
                                    .when_some(unavailable, |this, why| {
                                        this.child(
                                            div()
                                                .text_size(px(11.5))
                                                .text_color(rgb(color::TEXT_MUTED))
                                                .child(why),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .font_family(mono.clone())
                                    .text_size(px(11.5))
                                    .text_color(rgb(color::TEXT_MUTED))
                                    .child(crate::recent::display(path)),
                            ),
                    )
                    .child(
                        div()
                            .h_flex()
                            .flex_none()
                            .gap(px(12.))
                            .text_size(px(11.5))
                            .children(badges.into_iter().map(|(text, hex, live)| {
                                div()
                                    .h_flex()
                                    .gap(px(5.))
                                    .text_color(rgb(hex))
                                    .when(live, |this| {
                                        this.child(motion::blinking_dot(px(6.), color::BRANCH_BLUE))
                                    })
                                    .child(text)
                            })),
                    )
                    .children(remove),
            );
        }
        let button = |id: &'static str, icon: IconName, label: &'static str| {
            Button::new(id)
                .debug_selector(move || id.into())
                .ghost()
                .h(px(30.))
                .text_color(rgb(color::TEXT_SECONDARY))
                .icon(Icon::new(icon).size(px(14.)))
                .label(label)
        };
        section.child(
            div()
                .h_flex()
                .flex_wrap()
                .gap(px(6.))
                .mt(px(10.))
                .pt(px(12.))
                .px(px(4.))
                .border_t_1()
                .border_color(rgb(color::BUTTON))
                .child(
                    button("home-open", IconName::FolderOpen, "Open repository…")
                        .child(div().ml(px(4.)).child(shortcut("O", cx)))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(OpenRepository.boxed_clone(), cx)
                        }),
                )
                .child(button("home-clone", IconName::Download, "Clone…").on_click(
                    |_, window, cx| window.dispatch_action(CloneRepository.boxed_clone(), cx),
                ))
                .child(
                    button("home-init", IconName::FolderPlus, "New repository…").on_click(
                        |_, window, cx| window.dispatch_action(InitRepository.boxed_clone(), cx),
                    ),
                ),
        )
    }
}

/// What a recent repository's threads need, for its row, in the words and
/// colours of the Agents indicator: "3 active", "1 waiting", "2 ready",
/// "1 failed", "1 stopped"; only what is not zero. With whether to put the
/// live dot in front.
fn badges(threads: &[Arc<AgentThread>]) -> Vec<(String, u32, bool)> {
    indicator_parts(Counts::of(threads.iter().map(Arc::as_ref)))
        .into_iter()
        .map(|(part, text)| (text, part.color(), part == Part::Active))
        .collect()
}

impl Render for Home {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let listed = list(self.model.read(cx), self.filter.as_deref());
        self.places = queue_places(self.model.read(cx));
        let entries = self.sidebar_entries(listed, cx);
        self.set_entries(entries);
        let main = match &self.thread {
            Some((view, _)) => div()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(view.clone())
                .into_any_element(),
            None => self.render_launcher(cx).into_any_element(),
        };
        div()
            .key_context("Home")
            .track_focus(&self.focus_handle)
            .v_flex()
            .size_full()
            .bg(rgb(color::BACKGROUND))
            .child(self.render_top_bar(window, cx))
            .child(
                div()
                    .h_flex()
                    .items_start()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(main),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use gitez_agent_thread::{
        AgentThread, AgentThreadId, Ending, IndexedRepository, NewAgentThread, Start, State, Store,
        Timestamp,
    };
    use gitez_sandbox::NetworkProfile;
    use gitez_settings::{AgentEntry, Settings};
    use gpui_kit::{Entity, TestAppContext, VisualTestContext, px, size};

    use std::cell::RefCell;
    use std::rc::Rc;

    use super::{Home, HomeEvent, list};
    use crate::agent_threads::{AgentThreads, Options, init_with};

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    fn minutes_ago(minutes: i64) -> Timestamp {
        Timestamp::now() - jiff::SignedDuration::from_mins(minutes)
    }

    /// A repository called `name` in `base` with one commit, listed in the
    /// data folder; returns its git common dir.
    fn repository(base: &Path, data_dir: &Path, name: &str) -> PathBuf {
        let work_dir = base.join(name);
        std::fs::create_dir(&work_dir).expect("mkdir");
        git(&work_dir, &["init", "-q", "-b", "main"]);
        std::fs::write(work_dir.join("a.txt"), "a\n").expect("write");
        git(&work_dir, &["add", "."]);
        git(&work_dir, &["commit", "-qm", "base"]);
        let common_dir = gitez_git::Repository::open(&work_dir)
            .expect("open")
            .common_dir();
        gitez_agent_thread::Index::new(data_dir)
            .add(IndexedRepository {
                common_dir: common_dir.clone(),
                work_dir,
                name: name.into(),
            })
            .expect("index");
        common_dir
    }

    /// A thread in the repository of `common_dir`, created `ago` minutes
    /// ago and then changed by `change`.
    fn thread(
        common_dir: &Path,
        data_dir: &Path,
        prompt: &str,
        ago: i64,
        change: impl FnOnce(&mut AgentThread),
    ) -> AgentThreadId {
        let store = Store::new(common_dir);
        let created = minutes_ago(ago);
        let thread = store
            .create(
                NewAgentThread {
                    prompt: prompt.into(),
                    agent: gitez_agent::ClaudeCode::ID.into(),
                    model: None,
                    start: Start {
                        branch: "main".into(),
                        commit: "0".repeat(40),
                    },
                    network: NetworkProfile::AgentOnly,
                    secrets: Vec::new(),
                },
                data_dir,
                "repo",
                |_| false,
                created,
            )
            .expect("create");
        store
            .update(&thread.id, |thread| {
                thread.created = created;
                change(thread);
                Ok(())
            })
            .expect("update");
        thread.id
    }

    /// Finished `ago` minutes ago.
    fn finished(ending: Ending, ago: i64) -> impl FnOnce(&mut AgentThread) {
        move |thread| {
            thread.state = State::Finished;
            thread.ending = Some(ending);
            thread.turns[0].started = Some(minutes_ago(ago + 1));
            thread.turns[0].ended = Some(minutes_ago(ago));
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        alpha: PathBuf,
        beta: PathBuf,
        /// alpha's: an answer 30 minutes ago and a failure 90 minutes ago.
        answer: AgentThreadId,
        failed: AgentThreadId,
        /// beta's pull request, 5 minutes ago.
        pull_request: AgentThreadId,
    }

    /// Two repositories with three finished threads, one running, one
    /// waiting for the usage limit and one completed, and Home on them.
    fn home(cx: &mut TestAppContext) -> (Fixture, Entity<Home>, VisualTestContext) {
        let dir = tempfile::tempdir().expect("temp dir");
        let base = dir.path().canonicalize().expect("canonical");
        let data_dir = base.join("data");
        let alpha = repository(&base, &data_dir, "alpha");
        let beta = repository(&base, &data_dir, "beta");
        // Created in another order than they finished.
        let failed = thread(&alpha, &data_dir, "Fix the watcher", 100, |thread| {
            finished(Ending::Failed, 90)(thread);
        });
        let pull_request = thread(&beta, &data_dir, "Add a feed", 200, |thread| {
            finished(Ending::PullRequest, 5)(thread);
            thread.pull_request = Some(1);
        });
        let answer = thread(&alpha, &data_dir, "Explain the lanes", 60, |thread| {
            finished(Ending::Answer, 30)(thread);
        });
        thread(&alpha, &data_dir, "Migrate the build", 8, |thread| {
            thread.state = State::Running;
            thread.turns[0].started = Some(minutes_ago(8));
        });
        thread(&beta, &data_dir, "Update the template", 20, |thread| {
            thread.state = State::Waiting;
            thread.waiting_until = Some(Timestamp::now() + jiff::SignedDuration::from_hours(1));
        });
        thread(&beta, &data_dir, "Add a toggle", 3000, |thread| {
            finished(Ending::PullRequest, 2900)(thread);
            thread.state = State::Completed;
            thread.completed = Some(minutes_ago(2800));
        });
        // Never the real `claude`: tests must not run it.
        Settings {
            agents: [(
                gitez_agent::ClaudeCode::ID.to_owned(),
                AgentEntry {
                    program: Some("/nonexistent/claude".into()),
                    ..AgentEntry::default()
                },
            )]
            .into(),
            ..Settings::default()
        }
        .save(&Settings::path(&data_dir))
        .expect("settings saved");

        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::init(cx);
            crate::recent::init_in_memory(cx);
            init_with(
                Options {
                    data_dir: Some(data_dir),
                    sandbox: Arc::new(gitez_sandbox::Passthrough),
                    agents: gitez_agent::Registry::builtin(),
                    secrets: Arc::new(gitez_secret::Memory::default()),
                    exe: Err("no supervisor in tests".into()),
                    watch: false,
                },
                cx,
            );
        });
        // The model reads the threads in the background.
        cx.run_until_parked();
        let window = cx.open_window(size(px(1440.), px(900.)), Home::new);
        let home = window
            .update(cx, |_, _, cx| cx.entity())
            .expect("window open");
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let fixture = Fixture {
            _dir: dir,
            alpha,
            beta,
            answer,
            failed,
            pull_request,
        };
        (fixture, home, cx)
    }

    fn row(id: &AgentThreadId) -> String {
        format!("home-thread-{}", id.as_str())
    }

    /// The top of the row of thread `id`, if it is drawn.
    fn top(cx: &mut VisualTestContext, id: &AgentThreadId) -> Option<f32> {
        cx.debug_bounds(Box::leak(row(id).into_boxed_str()))
            .map(|bounds| f32::from(bounds.origin.y))
    }

    fn redraw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    /// Home's recent repositories show one that was moved or deleted
    /// muted with "Moved or deleted", as the launcher's picker does (the
    /// same check): it does not open, and "Remove from list" takes it off.
    #[gpui_kit::test]
    fn a_moved_recent_repository_is_marked_and_cannot_be_opened(cx: &mut TestAppContext) {
        let (fixture, home, mut cx) = home(cx);
        let here = fixture.alpha.parent().expect("a work dir").to_owned();
        let gone = here.with_file_name("gone");
        let opened: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
        cx.update(|_, cx| {
            let opened = opened.clone();
            cx.subscribe(&home, move |_, event: &HomeEvent, _| {
                if let HomeEvent::OpenRepository(path) = event {
                    opened.borrow_mut().push(path.clone());
                }
            })
            .detach();
            // Listed first: `here` was opened last.
            crate::recent::opened(&gone, cx);
            crate::recent::opened(&here, cx);
            crate::recent::check(cx);
        });
        redraw(&mut cx);
        cx.update(|_, cx| {
            let model = AgentThreads::global(cx).read(cx);
            assert_eq!(
                crate::recent::unavailable(&gone, model, cx).as_deref(),
                Some("Moved or deleted")
            );
            assert_eq!(crate::recent::unavailable(&here, model, cx), None);
        });
        assert!(cx.debug_bounds("home-recent-remove-0").is_none());
        let remove = cx
            .debug_bounds("home-recent-remove-1")
            .expect("the moved one can be removed");
        for index in [1, 0] {
            let selector: &'static str = if index == 1 {
                "home-recent-1"
            } else {
                "home-recent-0"
            };
            let row = cx.debug_bounds(selector).expect("a row");
            cx.simulate_click(
                gpui_kit::point(row.origin.x + px(40.), row.center().y),
                gpui_kit::Modifiers::none(),
            );
        }
        assert_eq!(
            *opened.borrow(),
            std::slice::from_ref(&here),
            "only the one still there opens"
        );
        cx.simulate_click(remove.center(), gpui_kit::Modifiers::none());
        cx.update(|_, cx| assert_eq!(crate::recent::paths(cx), std::slice::from_ref(&here)));
    }

    /// Times drawing Home with 2,000 and 20,000 finished threads (the
    /// sidebar a virtual list, every row listed and compared each frame):
    /// `GITEZ_MEASURE=1 cargo test -p gitez measure_home -- --nocapture`.
    #[gpui_kit::test]
    fn measure_home(cx: &mut TestAppContext) {
        if std::env::var_os("GITEZ_MEASURE").is_none() {
            return;
        }
        let (fixture, home, mut cx) = home(cx);
        let model = cx.update(|_, cx| AgentThreads::global(cx));
        let template = model.read_with(&cx, |model, _| {
            model
                .repository(&fixture.alpha)
                .and_then(|repository| {
                    repository
                        .threads
                        .iter()
                        .find(|thread| thread.id == fixture.answer)
                        .cloned()
                })
                .expect("the answer")
        });
        for count in [2_000, 20_000] {
            let threads: Vec<Arc<AgentThread>> = (0..count)
                .map(|index| {
                    let mut thread = (*template).clone();
                    thread.id = AgentThreadId::new(format!("measured-{index}")).expect("an id");
                    thread.title = format!("Measured thread number {index}");
                    Arc::new(thread)
                })
                .collect();
            model.update(&mut cx, |model, cx| {
                model.replace_threads(&fixture.alpha, threads);
                cx.notify();
            });
            redraw(&mut cx);
            let frames = 5;
            let started = std::time::Instant::now();
            for _ in 0..frames {
                home.update(&mut cx, |_, cx| cx.notify());
                cx.update(|window, cx| window.draw(cx).clear(cx));
            }
            eprintln!(
                "home {count} threads: {:?} a frame",
                started.elapsed() / frames
            );
        }
    }

    #[gpui_kit::test]
    fn home_lists_finished_threads_newest_first_and_the_rest_on_request(cx: &mut TestAppContext) {
        let (fixture, home, mut cx) = home(cx);
        let model = cx.update(|_, cx| AgentThreads::global(cx));

        // Finished, across both repositories, newest first.
        let order = model.read_with(&cx, |model, _| {
            let listed = list(model, None);
            assert_eq!(listed.active.len(), 1);
            assert_eq!(listed.waiting.len(), 1);
            assert_eq!(listed.completed.len(), 1);
            listed
                .finished
                .iter()
                .map(|item| item.thread.id.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            order,
            [
                fixture.pull_request.clone(),
                fixture.answer.clone(),
                fixture.failed.clone()
            ]
        );
        let tops: Vec<f32> = order
            .iter()
            .map(|id| top(&mut cx, id).expect("a finished row is drawn"))
            .collect();
        assert!(tops.is_sorted(), "drawn in that order: {tops:?}");

        // "Active and completed" is collapsed, and expands into three groups.
        for group in [
            "home-group-active",
            "home-group-waiting",
            "home-group-completed",
        ] {
            assert!(cx.debug_bounds(group).is_none(), "{group} is collapsed");
        }
        let toggle = cx.debug_bounds("home-more").expect("the toggle is drawn");
        cx.simulate_click(toggle.center(), gpui_kit::Modifiers::none());
        redraw(&mut cx);
        let groups: Vec<f32> = [
            "home-group-active",
            "home-group-waiting",
            "home-group-completed",
        ]
        .iter()
        .map(|group| {
            f32::from(
                cx.debug_bounds(group)
                    .unwrap_or_else(|| panic!("{group} is shown"))
                    .origin
                    .y,
            )
        })
        .collect();
        assert!(groups.is_sorted(), "Active, Waiting, Completed: {groups:?}");

        // The filter narrows the list to one repository.
        home.update(&mut cx, |home, cx| {
            home.set_filter(Some(fixture.alpha.clone()), cx)
        });
        redraw(&mut cx);
        assert!(
            top(&mut cx, &fixture.pull_request).is_none(),
            "beta's is hidden"
        );
        let answer = top(&mut cx, &fixture.answer).expect("alpha's is shown");
        let failed = top(&mut cx, &fixture.failed).expect("alpha's is shown");
        assert!(answer < failed);
        assert!(
            cx.debug_bounds("home-group-waiting").is_none(),
            "beta's wait is hidden"
        );
        assert!(cx.debug_bounds("home-group-active").is_some());
        model.read_with(&cx, |model, _| {
            let listed = list(model, Some(&fixture.beta));
            assert_eq!(listed.finished.len(), 1);
            assert_eq!(listed.active.len(), 0);
            assert_eq!(listed.waiting.len(), 1);
        });
    }
}
