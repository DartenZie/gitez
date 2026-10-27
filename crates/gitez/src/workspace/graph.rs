//! Painting one row of the commit graph.
//!
//! Each row is painted on its own from a [`Row`] of the layout: lines that
//! pass through, lines that end at the commit from above, lines that leave it
//! downwards, and the commit node in the middle. Rows of a virtual list are
//! painted independently, so nothing here looks at neighbouring rows.

use gitez_git::graph::{Row, Segment};
use gpui_kit::*;

use crate::theme;

/// Horizontal distance between lanes.
pub const LANE_WIDTH: Pixels = px(22.);
/// Distance from the left edge of the row to the centre of lane 0.
pub const FIRST_LANE: Pixels = px(18.);
/// Space kept right of the last lane.
const TRAILING: Pixels = px(14.);

const LINE_WIDTH: Pixels = px(2.);
const NODE_RADIUS: Pixels = px(6.);
/// The ring drawn around the commit `HEAD` points at.
const HEAD_RING_RADIUS: Pixels = px(10.);
const HEAD_RING_WIDTH: Pixels = px(2.);
/// The hollow, dashed node of the uncommitted-changes row and the dashed
/// line joining it to the `HEAD` commit.
const WORKING_TREE_RADIUS: Pixels = px(5.5);
const WORKING_TREE_BORDER: Pixels = px(1.6);
const DASHES: [Pixels; 2] = [px(3.), px(4.)];

/// Width needed to show a graph `lanes` lanes wide in full.
pub fn content_width(lanes: usize) -> Pixels {
    FIRST_LANE + LANE_WIDTH * lanes.saturating_sub(1) as f32 + TRAILING
}

/// Width of the graph column for a graph `lanes` lanes wide, kept between
/// `min` and `max`. A wider graph scrolls within the column.
pub fn column_width(lanes: usize, min: Pixels, max: Pixels) -> Pixels {
    content_width(lanes).max(min).min(max)
}

/// The horizontal scroll that brings `lane` into a column `width` wide,
/// starting from `scroll`. Leaves `scroll` alone if the lane is visible.
pub fn scroll_to_lane(lane: usize, scroll: Pixels, width: Pixels) -> Pixels {
    let centre = FIRST_LANE + LANE_WIDTH * lane as f32;
    if centre - FIRST_LANE < scroll {
        centre - FIRST_LANE
    } else if centre + TRAILING > scroll + width {
        centre + TRAILING - width
    } else {
        scroll
    }
}

/// How a commit row relates to `HEAD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Head {
    No,
    /// `HEAD` points at this commit.
    Yes,
    /// `HEAD` points at this commit, and the dashed line from the
    /// uncommitted-changes row comes in at the top of the row in `lane`.
    BelowWorkingTree {
        lane: usize,
    },
}

/// An element that paints `row` across its bounds, with the graph moved
/// `scroll` to the left. `working_tree` is the lane and colour of the dashed
/// line from the uncommitted-changes row, when it passes through this row on
/// its way down to `HEAD`.
pub fn cell(
    row: Row,
    head: Head,
    working_tree: Option<(usize, usize)>,
    width: Pixels,
    scroll: Pixels,
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                paint_row(&row, head, bounds, scroll, window);
                if let Some((lane, color)) = working_tree {
                    let x = lane_x(bounds, scroll, lane);
                    if let Some(path) =
                        dashed_line(point(x, bounds.top()), point(x, bounds.bottom()))
                    {
                        window.paint_path(path, lane_color(color));
                    }
                }
            });
        },
    )
    .w(width)
    .h_full()
    .flex_none()
}

/// An element that paints the uncommitted-changes row, the first row of the
/// history: its node in `lane`, and with `has_head` a dashed line going down
/// towards the commit `HEAD` points at. Without `has_head` (an unborn branch,
/// or `HEAD` not read yet) only the node is drawn.
pub fn working_tree_cell(
    lane: usize,
    color: usize,
    has_head: bool,
    width: Pixels,
    scroll: Pixels,
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                paint_working_tree(lane, color, has_head, bounds, scroll, window);
            });
        },
    )
    .w(width)
    .h_full()
    .flex_none()
}

/// The x coordinate of `lane`'s centre in a row at `bounds`.
fn lane_x(bounds: Bounds<Pixels>, scroll: Pixels, lane: usize) -> Pixels {
    bounds.left() - scroll + FIRST_LANE + LANE_WIDTH * lane as f32
}

fn paint_working_tree(
    lane: usize,
    color: usize,
    has_head: bool,
    bounds: Bounds<Pixels>,
    scroll: Pixels,
    window: &mut Window,
) {
    let node = point(lane_x(bounds, scroll, lane), bounds.center().y);
    if has_head && let Some(path) = dashed_line(node, point(node.x, bounds.bottom())) {
        window.paint_path(path, lane_color(color));
    }
    window.paint_quad(quad(
        circle_bounds(node, WORKING_TREE_RADIUS),
        WORKING_TREE_RADIUS,
        rgb(theme::color::BACKGROUND),
        WORKING_TREE_BORDER,
        lane_color(color),
        BorderStyle::Dashed,
    ));
}

fn paint_row(row: &Row, head: Head, bounds: Bounds<Pixels>, scroll: Pixels, window: &mut Window) {
    let top = bounds.top();
    let bottom = bounds.bottom();
    let middle = bounds.center().y;
    let x = |lane: usize| lane_x(bounds, scroll, lane);
    let node = point(x(row.lane), middle);

    for segment in &row.segments {
        let (color, path) = match *segment {
            Segment::Through { lane, color } => {
                (color, line(point(x(lane), top), point(x(lane), bottom)))
            }
            Segment::In { lane, color } => (color, curve(point(x(lane), top), node)),
            Segment::Out { lane, color } => (color, curve(node, point(x(lane), bottom))),
        };
        if let Some(path) = path {
            window.paint_path(path, lane_color(color));
        }
    }

    let color = lane_color(row.color);
    if let Head::BelowWorkingTree { lane } = head
        && let Some(path) = dashed_curve(point(x(lane), top), node)
    {
        window.paint_path(path, color);
    }
    if head != Head::No {
        window.paint_quad(quad(
            circle_bounds(node, HEAD_RING_RADIUS),
            HEAD_RING_RADIUS,
            transparent_black(),
            HEAD_RING_WIDTH,
            color,
            BorderStyle::default(),
        ));
    }
    window.paint_quad(fill(circle_bounds(node, NODE_RADIUS), color).corner_radii(NODE_RADIUS));
}

fn lane_color(color: usize) -> Rgba {
    rgb(theme::GRAPH_COLORS[color % theme::GRAPH_COLORS.len()])
}

fn circle_bounds(center: Point<Pixels>, radius: Pixels) -> Bounds<Pixels> {
    Bounds::centered_at(center, size(radius * 2., radius * 2.))
}

fn line(from: Point<Pixels>, to: Point<Pixels>) -> Option<Path<Pixels>> {
    let mut builder = PathBuilder::stroke(LINE_WIDTH);
    builder.move_to(from);
    builder.line_to(to);
    builder.build().ok()
}

fn dashed_line(from: Point<Pixels>, to: Point<Pixels>) -> Option<Path<Pixels>> {
    let mut builder = PathBuilder::stroke(LINE_WIDTH).dash_array(&DASHES);
    builder.move_to(from);
    builder.line_to(to);
    builder.build().ok()
}

/// A line from `from` to `to`, straight when they share a lane and otherwise
/// an S-curve that leaves and arrives vertically.
fn curve(from: Point<Pixels>, to: Point<Pixels>) -> Option<Path<Pixels>> {
    if from.x == to.x {
        return line(from, to);
    }
    s_curve(PathBuilder::stroke(LINE_WIDTH), from, to)
}

/// [`curve`], dashed.
fn dashed_curve(from: Point<Pixels>, to: Point<Pixels>) -> Option<Path<Pixels>> {
    if from.x == to.x {
        return dashed_line(from, to);
    }
    s_curve(
        PathBuilder::stroke(LINE_WIDTH).dash_array(&DASHES),
        from,
        to,
    )
}

fn s_curve(
    mut builder: PathBuilder,
    from: Point<Pixels>,
    to: Point<Pixels>,
) -> Option<Path<Pixels>> {
    let bend = (from.y + to.y) / 2.;
    builder.move_to(from);
    builder.cubic_bezier_to(to, point(from.x, bend), point(to.x, bend));
    builder.build().ok()
}

#[cfg(test)]
mod tests {
    // Not `super::*`: it brings in GPUI's `test` macro, which shadows Rust's.
    use super::{column_width, scroll_to_lane};
    use gpui_kit::px;

    #[test]
    fn column_width_grows_with_lanes_within_limits() {
        let (min, max) = (px(112.), px(300.));
        assert_eq!(column_width(1, min, max), min);
        assert_eq!(column_width(4, min, max), min);
        assert_eq!(column_width(5, min, max), px(18. + 4. * 22. + 14.));
        assert_eq!(column_width(100, min, max), max);
    }

    #[test]
    fn scrolling_to_a_lane_moves_only_as_far_as_needed() {
        let width = px(120.);
        // Lane 2 (centre 62) is visible unscrolled.
        assert_eq!(scroll_to_lane(2, px(0.), width), px(0.));
        // Lane 10 (centre 238) needs its trailing space in view.
        assert_eq!(scroll_to_lane(10, px(0.), width), px(238. + 14. - 120.));
        // Back to lane 1 (centre 40) from far right.
        assert_eq!(scroll_to_lane(1, px(200.), width), px(22.));
        assert_eq!(scroll_to_lane(0, px(200.), width), px(0.));
    }
}
