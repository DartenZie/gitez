//! The repository sidebar: views, then local PRs, branches, remotes, tags and
//! stashes.

use gpui_kit::assets::IconName;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::theme::{self, color};

#[derive(IntoElement)]
pub struct Sidebar;

impl RenderOnce for Sidebar {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
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
                    .child(nav_item("nav-history", IconName::GitGraph, "History", true))
                    .child(nav_item(
                        "nav-working-tree",
                        IconName::ListTree,
                        "Working tree",
                        false,
                    )),
            )
            .child(section("Local pull requests", "No local pull requests"))
            .child(section("Branches", "No branches"))
            .child(section("Remotes", "No remotes"))
            .child(section("Tags", "No tags"))
            .child(section("Stashes", "No stashes"))
    }
}

/// A view switcher entry at the top of the sidebar.
fn nav_item(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    active: bool,
) -> impl IntoElement {
    div()
        .id(id)
        .h_flex()
        .h(theme::NAV_ROW_HEIGHT)
        .px(px(6.))
        .gap(px(8.))
        .rounded(theme::RADIUS)
        .text_size(px(12.5))
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
}

/// A titled group of sidebar rows. Shows `empty` while the group has none.
fn section(title: &'static str, empty: &'static str) -> impl IntoElement {
    div()
        .v_flex()
        .child(
            div()
                .pt(px(14.))
                .px(px(14.))
                .pb(px(6.))
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(color::TEXT_FAINT))
                .child(title.to_uppercase()),
        )
        .child(
            div()
                .h_flex()
                .h(theme::NAV_ROW_HEIGHT)
                .px(px(14.))
                .text_size(px(12.5))
                .text_color(rgb(color::TEXT_FAINT))
                .child(empty),
        )
}
