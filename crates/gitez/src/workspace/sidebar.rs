//! The repository sidebar: views, then local PRs, branches, remotes, tags and
//! stashes.

use std::collections::HashSet;
use std::sync::Arc;

use gitez_git::{Branch, Oid, RemoteBranch, Stash, Tag};
use gitez_review::{PullRequest, State};
use gpui_kit::assets::IconName;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::history::{DraggedBranch, DraggedLabel};
use super::remote::{RemoteOp, RemoteTarget};
use crate::repository::{Snapshot, Tracking};
use crate::theme::{self, color};
use crate::time;

/// Tags start collapsed when a repository has more than this many, so a long
/// release history does not push the stashes out of sight.
const COLLAPSE_TAGS_OVER: usize = 30;

/// Text size of sidebar rows.
const ROW_TEXT_SIZE: Pixels = px(12.5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Section {
    LocalPullRequests,
    Branches,
    Remotes,
    Tags,
    Stashes,
}

/// What the sidebar asks of the window.
#[derive(Clone)]
pub enum SidebarEvent {
    /// Show the local pull request with this number.
    OpenPullRequest(u32),
    /// Check out the branch with this name.
    CheckOut(String),
    /// Ask for a new branch starting where this one is.
    NewBranchFrom(Oid),
    RenameBranch(String),
    DeleteBranch(String),
    /// Branch `head` was dropped on branch `base`, to merge it there.
    MergeBranch {
        head: String,
        head_tip: Oid,
        base: String,
        base_tip: Oid,
    },
    /// Check out remote branch `remote`/`name` as a local branch tracking
    /// it.
    CheckOutRemote {
        remote: String,
        name: String,
    },
    /// Delete branch `name` on `remote`.
    DeleteRemoteBranch {
        remote: String,
        name: String,
    },
    /// Show this commit in the history.
    SelectCommit(Oid),
    /// Send the tag with this name to the remote.
    PushTag(String),
    DeleteTag(String),
    /// Delete the tag with this name on the remote.
    DeleteRemoteTag(String),
    /// Bring stash `index` back, dropping it if `pop`.
    Unstash {
        index: usize,
        pop: bool,
    },
    DropStash(usize),
    /// Ask for a remote to add.
    AddRemote,
    /// Fetch the remote with this name alone.
    FetchRemote(String),
    /// Ask for a new name or URL for the remote with this name.
    EditRemote(String),
    RemoveRemote(String),
    /// Ask which remote branch the branch with this name tracks.
    SetUpstream(String),
    /// Pull or push the branch with this name, checked out or not.
    PullBranch(String),
    PushBranch(String),
    /// End the fetch, pull or push running.
    CancelRemote,
}

/// The fetch, pull or push running, for the row it works on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningRow {
    pub op: RemoteOp,
    pub target: RemoteTarget,
    /// Whether it was cancelled and is ending.
    pub cancelling: bool,
}

impl RunningRow {
    /// Whether it works on local branch `name`.
    fn on_branch(&self, name: &str) -> bool {
        matches!(&self.target, RemoteTarget::Branch(branch) if branch == name)
    }

    /// Whether it works on remote `name`.
    fn on_remote(&self, name: &str) -> bool {
        match &self.target {
            RemoteTarget::Remotes => true,
            RemoteTarget::Remote(remote) => remote == name,
            RemoteTarget::Branch(_) => false,
        }
    }
}

pub struct Sidebar {
    snapshot: Option<Arc<Snapshot>>,
    /// Files with uncommitted changes, once read.
    changed_files: Option<usize>,
    collapsed: HashSet<Section>,
    /// Remotes whose branches are listed.
    expanded_remotes: HashSet<String>,
    /// The local branch being dragged, here or in the history.
    dragging: Option<SharedString>,
    /// The fetch, pull or push running, shown on its row.
    running: Option<RunningRow>,
}

impl Sidebar {
    pub fn new() -> Self {
        Self {
            snapshot: None,
            changed_files: None,
            collapsed: HashSet::new(),
            expanded_remotes: HashSet::new(),
            dragging: None,
            running: None,
        }
    }

    /// Shows `running`, the fetch, pull or push in progress, on the row it
    /// works on, with a button that cancels it; `None` once it has ended.
    pub fn set_running(&mut self, running: Option<RunningRow>, cx: &mut Context<Self>) {
        if self.running != running {
            self.running = running;
            cx.notify();
        }
    }

    /// Shows `count` beside "Working tree"; nothing for `None` or zero.
    pub fn set_changed_files(&mut self, count: Option<usize>, cx: &mut Context<Self>) {
        if self.changed_files != count {
            self.changed_files = count;
            cx.notify();
        }
    }

    /// Shows `snapshot`. Collapsed sections reset when the repository changes.
    pub fn set_snapshot(&mut self, snapshot: Arc<Snapshot>, cx: &mut Context<Self>) {
        let same_repository = self
            .snapshot
            .as_ref()
            .is_some_and(|current| current.path == snapshot.path);
        if !same_repository {
            self.collapsed.clear();
            self.expanded_remotes.clear();
            if snapshot.refs.tags.len() > COLLAPSE_TAGS_OVER {
                self.collapsed.insert(Section::Tags);
            }
        }
        self.snapshot = Some(snapshot);
        cx.notify();
    }

    fn toggle_remote(&mut self, remote: &str, cx: &mut Context<Self>) {
        if !self.expanded_remotes.remove(remote) {
            self.expanded_remotes.insert(remote.to_owned());
        }
        cx.notify();
    }

    fn toggle(&mut self, section: Section, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&section) {
            self.collapsed.insert(section);
        }
        cx.notify();
    }

    /// A titled, collapsible group of rows. `rows` is only called while the
    /// section is expanded.
    fn section(
        &self,
        section: Section,
        title: &'static str,
        count: usize,
        empty: &'static str,
        rows: impl FnOnce() -> Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.section_with(section, title, count, empty, None, rows, cx)
    }

    /// A section whose header also offers `action`, a button.
    #[allow(clippy::too_many_arguments)]
    fn section_with(
        &self,
        section: Section,
        title: &'static str,
        count: usize,
        empty: &'static str,
        action: Option<AnyElement>,
        rows: impl FnOnce() -> Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let collapsed = self.collapsed.contains(&section);
        div()
            .v_flex()
            .child(
                div()
                    .id(("sidebar-section", section as usize))
                    .h_flex()
                    .gap(px(6.))
                    .pt(px(14.))
                    .px(px(14.))
                    .pb(px(6.))
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(color::TEXT_FAINT))
                    .hover(|style| style.text_color(rgb(color::TEXT_SECONDARY)))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(section, cx)))
                    .child(div().flex_1().child(title.to_uppercase()))
                    .when(count > 0, |this| {
                        this.child(
                            div()
                                .font_weight(FontWeight::NORMAL)
                                .child(count.to_string()),
                        )
                    })
                    .children(action)
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(px(12.)),
                    ),
            )
            .when(!collapsed, |this| {
                if count == 0 {
                    this.child(row().text_color(rgb(color::TEXT_FAINT)).child(empty))
                } else {
                    this.children(rows())
                }
            })
    }
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Dropped, or released away from any branch.
        if self.dragging.is_some() && !cx.has_active_drag() {
            self.dragging = None;
        }
        let snapshot = self.snapshot.clone();
        let dragging = self.dragging.clone();
        let running = self.running.clone();

        div()
            .id("sidebar")
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<DraggedBranch>, _, cx| {
                    let name = event.drag(cx).name();
                    if this.dragging.as_ref() != Some(name) {
                        this.dragging = Some(name.clone());
                        cx.notify();
                    }
                }),
            )
            .v_flex()
            .size_full()
            .bg(rgb(color::SIDEBAR))
            .overflow_y_scroll()
            .pb(px(16.))
            .child(
                div()
                    .v_flex()
                    .gap(px(1.))
                    .pt(px(10.))
                    .px(px(8.))
                    .pb(px(4.))
                    .child(nav_item(
                        "nav-history",
                        IconName::GitGraph,
                        "History",
                        None,
                        true,
                    ))
                    .child(nav_item(
                        "nav-working-tree",
                        IconName::ListTree,
                        "Working tree",
                        self.changed_files.filter(|&count| count > 0),
                        false,
                    )),
            )
            .when_some(snapshot, |this, snapshot| {
                let refs = &snapshot.refs;
                let mut remotes: Vec<(&str, Vec<&RemoteBranch>)> = Vec::new();
                for branch in &refs.remote_branches {
                    match remotes.last_mut() {
                        Some((remote, branches)) if *remote == branch.remote => {
                            branches.push(branch)
                        }
                        _ => remotes.push((&branch.remote, vec![branch])),
                    }
                }
                let remote = snapshot
                    .remotes
                    .iter()
                    .find(|remote| *remote == "origin")
                    .or(snapshot.remotes.first())
                    .cloned();
                let tag_rows = refs
                    .tags
                    .iter()
                    .map(|tag| tag_row(tag, remote.clone(), cx).into_any_element())
                    .collect::<Vec<_>>();
                let mut remote_rows = Vec::new();
                for (remote, branches) in &remotes {
                    let expanded = self.expanded_remotes.contains(*remote);
                    let running = running.as_ref().filter(|running| running.on_remote(remote));
                    remote_rows.push(
                        remote_row(remote, branches.len(), expanded, running, cx)
                            .into_any_element(),
                    );
                    if expanded {
                        remote_rows.extend(
                            branches
                                .iter()
                                .map(|branch| remote_branch_row(branch, cx).into_any_element()),
                        );
                    }
                }
                let now = time::now();

                let pull_request_rows = snapshot
                    .pull_requests
                    .iter()
                    .map(|pull_request| pull_request_row(pull_request, cx).into_any_element())
                    .collect::<Vec<_>>();
                let stash_rows = refs
                    .stashes
                    .iter()
                    .map(|stash| stash_row(stash, now, cx).into_any_element())
                    .collect::<Vec<_>>();
                let branch_rows = refs
                    .branches
                    .iter()
                    .map(|branch| {
                        let lifted = dragging.as_deref() == Some(branch.name.as_str());
                        let running = running
                            .as_ref()
                            .filter(|running| running.on_branch(&branch.name));
                        let tracking = snapshot.tracking.get(&branch.name);
                        branch_row(branch, tracking, lifted, running, cx).into_any_element()
                    })
                    .collect::<Vec<_>>();
                this.child(self.section(
                    Section::LocalPullRequests,
                    "Local pull requests",
                    snapshot.pull_requests.len(),
                    "No local pull requests",
                    || pull_request_rows,
                    cx,
                ))
                .child(self.section(
                    Section::Branches,
                    "Branches",
                    refs.branches.len(),
                    "No branches",
                    || branch_rows,
                    cx,
                ))
                .child(self.section_with(
                    Section::Remotes,
                    "Remotes",
                    remotes.len(),
                    "No remotes",
                    Some(add_remote_button(cx).into_any_element()),
                    || remote_rows,
                    cx,
                ))
                .child(self.section(
                    Section::Tags,
                    "Tags",
                    refs.tags.len(),
                    "No tags",
                    || tag_rows,
                    cx,
                ))
                .child(self.section(
                    Section::Stashes,
                    "Stashes",
                    refs.stashes.len(),
                    "No stashes",
                    || stash_rows,
                    cx,
                ))
            })
    }
}

/// A view switcher entry at the top of the sidebar.
fn nav_item(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    count: Option<usize>,
    active: bool,
) -> impl IntoElement {
    div()
        .id(id)
        .h_flex()
        .h(theme::NAV_ROW_HEIGHT)
        .px(px(6.))
        .gap(px(8.))
        .rounded(theme::RADIUS)
        .text_size(ROW_TEXT_SIZE)
        .map(|this| {
            if active {
                this.bg(rgb(color::NAV_ACTIVE)).text_color(rgb(color::TEXT))
            } else {
                this.text_color(rgb(color::TEXT_SECONDARY))
                    .hover(|style| style.bg(rgb(color::NAV_HOVER)).text_color(rgb(color::TEXT)))
            }
        })
        .child(Icon::new(icon).size(px(15.)))
        .child(label)
        .when_some(count, |this, count| {
            this.child(
                div()
                    .ml_auto()
                    .text_size(theme::SMALL_TEXT_SIZE)
                    .text_color(rgb(color::TEXT_MUTED))
                    .child(count.to_string()),
            )
        })
}

/// A single-line sidebar row.
fn row() -> Div {
    div()
        .h_flex()
        .h(theme::NAV_ROW_HEIGHT)
        .px(px(14.))
        .gap(px(8.))
        .text_size(ROW_TEXT_SIZE)
        .text_color(rgb(color::TEXT_SECONDARY))
        .hover(|style| style.bg(rgb(color::NAV_HOVER)).text_color(rgb(color::TEXT)))
}

/// Text that takes the free width of a row and is cut with an ellipsis.
fn label(text: impl Into<SharedString>) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .child(text.into())
}

/// Right-aligned detail at the end of a row: counts, ahead/behind.
fn trailing(text: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .text_size(theme::SMALL_TEXT_SIZE)
        .text_color(rgb(color::TEXT_FAINT))
        .child(text.into())
}

/// The end of the row a fetch, pull or push is `running` on: that it
/// runs, and the button that cancels it.
fn running_end(running: &RunningRow, cx: &mut Context<Sidebar>) -> impl IntoElement {
    let noun = running.op.noun();
    div()
        .h_flex()
        .flex_none()
        .gap(px(4.))
        .child(Spinner::new().xsmall().color(rgb(color::TEXT_MUTED).into()))
        .when(!running.cancelling, |this| {
            this.child(
                div()
                    .id("cancel-remote")
                    .debug_selector(|| "sidebar-cancel-remote".into())
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(16.))
                    .rounded(px(3.))
                    .text_color(rgb(color::TEXT_MUTED))
                    .hover(|style| {
                        style
                            .bg(rgb(color::BUTTON_HOVER))
                            .text_color(rgb(color::TEXT))
                    })
                    .tooltip(move |window, cx| {
                        Tooltip::new(format!("Cancel the {noun}")).build(window, cx)
                    })
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.stop_propagation();
                        cx.emit(SidebarEvent::CancelRemote);
                    }))
                    .child(Icon::new(IconName::Close).size(px(12.))),
            )
        })
}

/// A local branch: double-click checks it out, right-click shows what else
/// can be done with it. It can be dragged onto another branch, here or in the
/// history, to merge it there; `lifted` while it is. While a pull or push of
/// it is `running`, the row says so in place of its ahead and behind counts.
fn branch_row(
    branch: &Branch,
    tracking: Option<&Tracking>,
    lifted: bool,
    running: Option<&RunningRow>,
    cx: &mut Context<Sidebar>,
) -> impl IntoElement {
    let branch_color = rgb(theme::branch_color(&branch.name));
    let dot = div().size(px(8.)).rounded_full().bg(branch_color);
    let marker = div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(14.))
        .rounded_full()
        .when(branch.is_head, |this| {
            this.border_1().border_color(branch_color)
        })
        .child(dot);

    let ahead_behind = tracking
        .map(|tracking| {
            let mut parts = Vec::new();
            if tracking.ahead > 0 {
                parts.push(format!("↑{}", tracking.ahead));
            }
            if tracking.behind > 0 {
                parts.push(format!("↓{}", tracking.behind));
            }
            parts.join(" ")
        })
        .filter(|text| !text.is_empty() && running.is_none());
    let running = running.map(|running| running_end(running, cx).into_any_element());

    row()
        .gap(px(5.))
        .when(branch.is_head, |this| {
            this.text_color(rgb(color::TEXT))
                .font_weight(FontWeight::MEDIUM)
        })
        .child(marker)
        .child(label(branch.name.clone()))
        .when_some(ahead_behind, |this, text| {
            this.child(trailing(text).font_weight(FontWeight::NORMAL))
        })
        .children(running)
        .id(SharedString::from(format!("branch-{}", branch.name)))
        .when(lifted, |this| this.opacity(0.35))
        .on_drag(
            DraggedBranch::new(branch.name.clone(), branch.target),
            |dragged, _, _, cx| cx.new(|_| DraggedLabel(dragged.name().clone())),
        )
        .can_drop({
            let name = branch.name.clone();
            move |dragged, _, _| {
                dragged
                    .downcast_ref::<DraggedBranch>()
                    .is_some_and(|dragged| *dragged.name() != name)
            }
        })
        .drag_over::<DraggedBranch>(|style, _, _, _| {
            style
                .bg(rgba(color::TARGET_ROW))
                .text_color(rgb(color::ACCENT))
        })
        .on_drop({
            let (base, base_tip) = (branch.name.clone(), branch.target);
            cx.listener(move |_, dragged: &DraggedBranch, _, cx| {
                cx.emit(SidebarEvent::MergeBranch {
                    head: dragged.name().to_string(),
                    head_tip: dragged.tip(),
                    base: base.clone(),
                    base_tip,
                });
            })
        })
        .on_click({
            let (name, is_head) = (branch.name.clone(), branch.is_head);
            cx.listener(move |_, event: &ClickEvent, _, cx| {
                if event.click_count() == 2 && !is_head {
                    cx.emit(SidebarEvent::CheckOut(name.clone()));
                }
            })
        })
        .context_menu({
            let sidebar = cx.entity().downgrade();
            let (name, tip, is_head) = (branch.name.clone(), branch.target, branch.is_head);
            let tracked = branch.upstream.is_some();
            move |menu, _, _| {
                let item = |label: &'static str, event: fn(&str, Oid) -> SidebarEvent| {
                    let (sidebar, name) = (sidebar.clone(), name.clone());
                    PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        sidebar.update(cx, |_, cx| cx.emit(event(&name, tip))).ok();
                    })
                };
                menu.item(
                    item("Check out", |name, _| {
                        SidebarEvent::CheckOut(name.to_owned())
                    })
                    .disabled(is_head),
                )
                .separator()
                .item(
                    item("Pull", |name, _| SidebarEvent::PullBranch(name.to_owned()))
                        .disabled(!tracked),
                )
                .item(item(if tracked { "Push" } else { "Publish" }, |name, _| {
                    SidebarEvent::PushBranch(name.to_owned())
                }))
                .separator()
                .item(item("New branch from here…", |_, tip| {
                    SidebarEvent::NewBranchFrom(tip)
                }))
                .item(item("Rename…", |name, _| {
                    SidebarEvent::RenameBranch(name.to_owned())
                }))
                .item(item("Set upstream…", |name, _| {
                    SidebarEvent::SetUpstream(name.to_owned())
                }))
                .separator()
                .item(
                    item("Delete", |name, _| {
                        SidebarEvent::DeleteBranch(name.to_owned())
                    })
                    .disabled(is_head),
                )
            }
        })
}

/// A tag: click to show its commit, right-click to push or delete it.
/// `remote` is where tags are pushed, if anywhere.
fn tag_row(tag: &Tag, remote: Option<String>, cx: &mut Context<Sidebar>) -> impl IntoElement {
    let (name, target) = (tag.name.clone(), tag.target);
    row()
        .cursor_pointer()
        .child(label(tag.name.clone()))
        .id(SharedString::from(format!("tag-{}", tag.name)))
        .on_click(cx.listener(move |_, _, _, cx| cx.emit(SidebarEvent::SelectCommit(target))))
        .context_menu({
            let sidebar = cx.entity().downgrade();
            move |menu, _, _| {
                let item = |label: SharedString, event: SidebarEvent| {
                    let sidebar = sidebar.clone();
                    PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        sidebar.update(cx, |_, cx| cx.emit(event.clone())).ok();
                    })
                };
                let to = remote.clone().unwrap_or_else(|| "a remote".to_owned());
                menu.item(
                    item(
                        format!("Push to {to}").into(),
                        SidebarEvent::PushTag(name.clone()),
                    )
                    .disabled(remote.is_none()),
                )
                .separator()
                .item(item("Delete".into(), SidebarEvent::DeleteTag(name.clone())))
                .item(
                    item(
                        format!("Delete from {to}…").into(),
                        SidebarEvent::DeleteRemoteTag(name.clone()),
                    )
                    .disabled(remote.is_none()),
                )
            }
        })
}

/// "#3  Title / In review · 3 open comments", as in the `Main.dc.html`
/// artboard.
fn pull_request_row(pull_request: &PullRequest, cx: &mut Context<Sidebar>) -> impl IntoElement {
    let open = pull_request.state == State::Open;
    let detail = match pull_request.state {
        State::Open => match pull_request.open_threads() {
            0 => "In review".to_owned(),
            1 => "In review · 1 open comment".to_owned(),
            count => format!("In review · {count} open comments"),
        },
        State::Merged => "Merged".to_owned(),
        State::Closed => "Closed".to_owned(),
    };
    let number = pull_request.number;
    div()
        .id(("sidebar-pull-request", number as usize))
        .h_flex()
        .items_start()
        .gap(px(8.))
        .py(px(6.))
        .px(px(14.))
        .text_size(ROW_TEXT_SIZE)
        .text_color(rgb(if open {
            color::TEXT_SECONDARY
        } else {
            color::TEXT_MUTED
        }))
        .hover(|style| style.bg(rgb(color::NAV_HOVER)).text_color(rgb(color::TEXT)))
        .cursor_pointer()
        .on_click(cx.listener(move |_, _, _, cx| cx.emit(SidebarEvent::OpenPullRequest(number))))
        .child(
            div()
                .flex_none()
                .pt(px(1.))
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(theme::SMALL_TEXT_SIZE)
                .text_color(rgb(if open {
                    color::PULL_REQUEST_OPEN_TEXT
                } else {
                    color::TEXT_FAINT
                }))
                .child(format!("#{number}")),
        )
        .child(
            div()
                .v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(2.))
                .child(label(pull_request.title.clone()))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(color::TEXT_FAINT))
                        .child(detail),
                ),
        )
}

/// A remote; click to list its branches. While a fetch from it or a push
/// to it is `running`, the row says so in place of its branch count.
fn remote_row(
    remote: &str,
    branches: usize,
    expanded: bool,
    running: Option<&RunningRow>,
    cx: &mut Context<Sidebar>,
) -> impl IntoElement {
    let count = if branches == 1 {
        "1 branch".to_owned()
    } else {
        format!("{branches} branches")
    };
    let name = remote.to_owned();
    let end = match running {
        Some(running) => running_end(running, cx).into_any_element(),
        None => trailing(count).into_any_element(),
    };
    row()
        .id(SharedString::from(format!("remote-{remote}")))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_remote(&name, cx)))
        .child(Icon::new(IconName::Cloud).size(px(14.)))
        .child(label(remote.to_owned()))
        .child(end)
        .child(
            Icon::new(if expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .size(px(12.))
            .text_color(rgb(color::TEXT_FAINT)),
        )
        .context_menu({
            let sidebar = cx.entity().downgrade();
            let name = remote.to_owned();
            move |menu, _, _| {
                let item = |label: &'static str, event: fn(String) -> SidebarEvent| {
                    let (sidebar, name) = (sidebar.clone(), name.clone());
                    PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        sidebar
                            .update(cx, |_, cx| cx.emit(event(name.clone())))
                            .ok();
                    })
                };
                menu.item(item("Fetch", SidebarEvent::FetchRemote))
                    .item(item("Edit…", SidebarEvent::EditRemote))
                    .separator()
                    .item(item("Remove…", SidebarEvent::RemoveRemote))
            }
        })
}

/// The Remotes header's button that adds a remote.
fn add_remote_button(cx: &mut Context<Sidebar>) -> impl IntoElement {
    div()
        .id("add-remote")
        .flex()
        .items_center()
        .justify_center()
        .size(px(16.))
        .rounded(px(3.))
        .hover(|style| style.bg(rgb(color::NAV_HOVER)).text_color(rgb(color::TEXT)))
        .tooltip(|window, cx| Tooltip::new("Add remote").build(window, cx))
        .on_click(cx.listener(|_, _, _, cx| {
            cx.stop_propagation();
            cx.emit(SidebarEvent::AddRemote);
        }))
        .child(Icon::new(IconName::Plus).size(px(12.)))
}

/// A branch on a remote, under its remote: double-click checks it out as a
/// local branch tracking it, right-click shows what else can be done.
fn remote_branch_row(branch: &RemoteBranch, cx: &mut Context<Sidebar>) -> impl IntoElement {
    let (remote, name, tip) = (branch.remote.clone(), branch.name.clone(), branch.target);
    row()
        .pl(px(36.))
        .child(label(branch.name.clone()))
        .id(SharedString::from(format!(
            "remote-branch-{}",
            branch.short_name()
        )))
        .on_click({
            let (remote, name) = (remote.clone(), name.clone());
            cx.listener(move |_, event: &ClickEvent, _, cx| {
                if event.click_count() == 2 {
                    cx.emit(SidebarEvent::CheckOutRemote {
                        remote: remote.clone(),
                        name: name.clone(),
                    });
                }
            })
        })
        .context_menu({
            let sidebar = cx.entity().downgrade();
            move |menu, _, _| {
                let item = |label: SharedString, event: SidebarEvent| {
                    let sidebar = sidebar.clone();
                    PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        sidebar.update(cx, |_, cx| cx.emit(event.clone())).ok();
                    })
                };
                menu.item(item(
                    "Check out".into(),
                    SidebarEvent::CheckOutRemote {
                        remote: remote.clone(),
                        name: name.clone(),
                    },
                ))
                .item(item(
                    "New branch from here…".into(),
                    SidebarEvent::NewBranchFrom(tip),
                ))
                .separator()
                .item(item(
                    format!("Delete from {remote}…").into(),
                    SidebarEvent::DeleteRemoteBranch {
                        remote: remote.clone(),
                        name: name.clone(),
                    },
                ))
            }
        })
}

/// A stash entry; right-click to pop, apply or drop it.
fn stash_row(stash: &Stash, now: i64, cx: &mut Context<Sidebar>) -> impl IntoElement {
    let when = time::relative(stash.time, now);
    let detail = match &stash.branch {
        Some(branch) => format!("on {branch} · {when}"),
        None => when,
    };
    div()
        .v_flex()
        .gap(px(2.))
        .py(px(5.))
        .px(px(14.))
        .text_size(ROW_TEXT_SIZE)
        .text_color(rgb(color::TEXT_SECONDARY))
        .hover(|style| style.bg(rgb(color::NAV_HOVER)).text_color(rgb(color::TEXT)))
        .child(label(stash.message.clone()))
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(color::TEXT_FAINT))
                .child(detail),
        )
        .id(("stash", stash.index))
        .context_menu({
            let sidebar = cx.entity().downgrade();
            let index = stash.index;
            move |menu, _, _| {
                let item = |label: &'static str, event: SidebarEvent| {
                    let sidebar = sidebar.clone();
                    let event = std::cell::RefCell::new(Some(event));
                    PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        if let Some(event) = event.borrow_mut().take() {
                            sidebar.update(cx, |_, cx| cx.emit(event)).ok();
                        }
                    })
                };
                menu.item(item("Pop", SidebarEvent::Unstash { index, pop: true }))
                    .item(item(
                        "Apply and keep",
                        SidebarEvent::Unstash { index, pop: false },
                    ))
                    .separator()
                    .item(item("Drop", SidebarEvent::DropStash(index)))
            }
        })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::Path;
    use std::process::Command;
    use std::rc::Rc;
    use std::sync::Arc;

    use gpui_kit::{Modifiers, TestAppContext, VisualTestContext, px, size};

    use super::{RunningRow, Sidebar, SidebarEvent};
    use crate::repository::Snapshot;
    use crate::workspace::remote::{RemoteOp, RemoteTarget};

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?}");
    }

    #[gpui_kit::test]
    fn the_row_of_a_running_pull_or_fetch_offers_to_cancel_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "a\n").expect("write");
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "start"]);
        git(root, &["remote", "add", "origin", "../nowhere.git"]);
        // A remote is listed once it has a branch.
        git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);

        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::init(cx);
        });
        let snapshot = Arc::new(Snapshot::load(root).expect("read the repository"));
        let window = cx.open_window(size(px(280.), px(700.)), |_, _| Sidebar::new());
        let sidebar = window
            .update(cx, |_, _, cx| cx.entity())
            .expect("window open");
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let cancels = Rc::new(Cell::new(0));
        let _subscription = cx.update(|_, cx| {
            let cancels = cancels.clone();
            cx.subscribe(&sidebar, move |_, event: &SidebarEvent, _| {
                if matches!(event, SidebarEvent::CancelRemote) {
                    cancels.set(cancels.get() + 1);
                }
            })
        });
        let show = |running: Option<RunningRow>, cx: &mut VisualTestContext| {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.set_snapshot(snapshot.clone(), cx);
                sidebar.set_running(running, cx);
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.debug_bounds("sidebar-cancel-remote")
        };

        // Nothing runs: nothing to cancel.
        assert_eq!(show(None, &mut cx), None);

        // A pull of the branch: its row has the button, and it asks to
        // cancel.
        let pull = RunningRow {
            op: RemoteOp::Pull,
            target: RemoteTarget::Branch("main".to_owned()),
            cancelling: false,
        };
        let button = show(Some(pull.clone()), &mut cx).expect("cancel drawn on the branch");
        cx.simulate_click(button.center(), Modifiers::default());
        assert_eq!(cancels.get(), 1);

        // Once cancelled it only waits.
        let ending = RunningRow {
            cancelling: true,
            ..pull.clone()
        };
        assert_eq!(show(Some(ending), &mut cx), None);

        // A pull of another branch is not shown on this one.
        let other = RunningRow {
            target: RemoteTarget::Branch("other".to_owned()),
            ..pull
        };
        assert_eq!(show(Some(other), &mut cx), None);

        // A fetch from every remote is shown on the remote, lower down.
        let fetch = RunningRow {
            op: RemoteOp::Fetch,
            target: RemoteTarget::Remotes,
            cancelling: false,
        };
        let on_remote = show(Some(fetch), &mut cx).expect("cancel drawn on the remote");
        assert!(on_remote.top() > button.top());
        cx.simulate_click(on_remote.center(), Modifiers::default());
        assert_eq!(cancels.get(), 2);
    }
}
