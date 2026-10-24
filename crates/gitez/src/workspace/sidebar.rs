//! The repository sidebar: views, then local PRs, branches, remotes, tags and
//! stashes.

use std::collections::HashSet;
use std::sync::Arc;

use gitez_git::{Branch, Oid, Stash};
use gitez_review::{PullRequest, State};
use gpui_kit::assets::IconName;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

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
pub enum SidebarEvent {
    /// Show the local pull request with this number.
    OpenPullRequest(u32),
    /// Check out the branch with this name.
    CheckOut(String),
    /// Ask for a new branch starting where this one is.
    NewBranchFrom(Oid),
    RenameBranch(String),
    DeleteBranch(String),
}

pub struct Sidebar {
    snapshot: Option<Arc<Snapshot>>,
    /// Files with uncommitted changes, once read.
    changed_files: Option<usize>,
    collapsed: HashSet<Section>,
}

impl Sidebar {
    pub fn new() -> Self {
        Self {
            snapshot: None,
            changed_files: None,
            collapsed: HashSet::new(),
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
            if snapshot.refs.tags.len() > COLLAPSE_TAGS_OVER {
                self.collapsed.insert(Section::Tags);
            }
        }
        self.snapshot = Some(snapshot);
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
        let snapshot = self.snapshot.clone();

        div()
            .id("sidebar")
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
                let mut remotes: Vec<(&str, usize)> = Vec::new();
                for branch in &refs.remote_branches {
                    match remotes.last_mut() {
                        Some((remote, count)) if *remote == branch.remote => *count += 1,
                        _ => remotes.push((&branch.remote, 1)),
                    }
                }
                let now = time::now();

                let pull_request_rows = snapshot
                    .pull_requests
                    .iter()
                    .map(|pull_request| pull_request_row(pull_request, cx).into_any_element())
                    .collect::<Vec<_>>();
                let branch_rows = refs
                    .branches
                    .iter()
                    .map(|branch| {
                        branch_row(branch, snapshot.tracking.get(&branch.name), cx)
                            .into_any_element()
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
                .child(self.section(
                    Section::Remotes,
                    "Remotes",
                    remotes.len(),
                    "No remotes",
                    || {
                        remotes
                            .iter()
                            .map(|&(remote, count)| remote_row(remote, count).into_any_element())
                            .collect()
                    },
                    cx,
                ))
                .child(self.section(
                    Section::Tags,
                    "Tags",
                    refs.tags.len(),
                    "No tags",
                    || {
                        refs.tags
                            .iter()
                            .map(|tag| row().child(label(tag.name.clone())).into_any_element())
                            .collect()
                    },
                    cx,
                ))
                .child(self.section(
                    Section::Stashes,
                    "Stashes",
                    refs.stashes.len(),
                    "No stashes",
                    || {
                        refs.stashes
                            .iter()
                            .map(|stash| stash_row(stash, now).into_any_element())
                            .collect()
                    },
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

/// A local branch: double-click checks it out, right-click shows what else
/// can be done with it.
fn branch_row(
    branch: &Branch,
    tracking: Option<&Tracking>,
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
        .filter(|text| !text.is_empty());

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
        .id(SharedString::from(format!("branch-{}", branch.name)))
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
                .item(item("New branch from here…", |_, tip| {
                    SidebarEvent::NewBranchFrom(tip)
                }))
                .item(item("Rename…", |name, _| {
                    SidebarEvent::RenameBranch(name.to_owned())
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

fn remote_row(remote: &str, branches: usize) -> impl IntoElement {
    let count = if branches == 1 {
        "1 branch".to_owned()
    } else {
        format!("{branches} branches")
    };
    row()
        .child(Icon::new(IconName::Cloud).size(px(14.)))
        .child(label(remote.to_owned()))
        .child(trailing(count))
}

fn stash_row(stash: &Stash, now: i64) -> impl IntoElement {
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
}
