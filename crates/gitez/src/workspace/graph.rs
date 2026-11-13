//! Painting one row of the commit graph.
//!
//! Each row is painted on its own from a [`Row`] of the layout: lines that
//! pass through, lines that end at the commit from above, lines that leave it
//! downwards, and the commit node in the middle. Rows of a virtual list are
//! painted independently, so nothing here looks at neighbouring rows.
//!
//! Rows that are not commits (the uncommitted changes, the "Ask an agent…"
//! row and active agent threads) have a [`Node`] of their own, joined by a
//! dashed line to the commit they start from.

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
/// The hollow, dashed node of the uncommitted-changes row.
const WORKING_TREE_RADIUS: Pixels = px(5.5);
const WORKING_TREE_BORDER: Pixels = px(1.6);
/// The node of the "Ask an agent…" row.
const COMPOSER_RADIUS: Pixels = px(6.5);
const COMPOSER_BORDER: Pixels = px(1.4);
/// The border of an active agent thread's node, and the dot of a running
/// one.
const LIVE_BORDER: Pixels = px(2.2);
const LIVE_DOT: Pixels = px(2.2);
/// Dash and gap of the dashed line. Each row paints its piece of the line
/// separately and the pattern restarts at every piece, so the period (6px)
/// must divide half the row height (18px), the piece from the node to the
/// row's edge, for the dashes to stay evenly spaced across rows.
const DASHES: [Pixels; 2] = [px(3.), px(3.)];

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

/// A dashed line in the graph: its lane and its colour (`0xRRGGBB`).
/// Dashed lines join rows that are not commits, the uncommitted changes and
/// active agent threads, to the commit they start from.
pub type Dashed = (usize, u32);

/// The node of a row that is not a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Node {
    /// The uncommitted changes: hollow and dashed.
    WorkingTree,
    /// The "Ask an agent…" row: a faint hollow node with a plus.
    Composer,
    /// An agent thread whose worktree is being prepared: hollow and dashed.
    Preparing,
    /// An agent thread whose agent runs: a ring with a dot in the middle.
    Running,
}

/// An element that paints `row` across its bounds, with the graph moved
/// `scroll` to the left. `through` are the dashed lines that pass this row
/// on their way down; `ending` those that end at its commit, coming in from
/// the top of the row. With `head`, a ring marks the commit `HEAD` points
/// at. With `labelled`, a line in the commit's colour joins the left edge,
/// where the row's branch label ends, to the node.
pub fn cell(
    row: Row,
    head: bool,
    through: Vec<Dashed>,
    ending: Vec<Dashed>,
    labelled: bool,
    width: Pixels,
    scroll: Pixels,
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                if labelled {
                    let y = bounds.center().y;
                    let node = lane_x(bounds, scroll, row.lane);
                    if let Some(path) = line(point(bounds.left(), y), point(node, y)) {
                        window.paint_path(path, lane_color(row.color));
                    }
                }
                paint_through(&through, bounds, scroll, window);
                let node = point(lane_x(bounds, scroll, row.lane), bounds.center().y);
                for &(lane, color) in &ending {
                    let from = point(lane_x(bounds, scroll, lane), bounds.top());
                    if let Some(path) = dashed_curve(from, node) {
                        window.paint_path(path, rgb(color));
                    }
                }
                paint_row(&row, head, bounds, scroll, window);
            });
        },
    )
    .w(width)
    .h_full()
    .flex_none()
}

/// An element that paints a row that is not a commit: `node` in `lane`, in
/// `color`, and with `down` a dashed line from it to the bottom of the row,
/// towards the commit it starts from. `through` are the dashed lines of
/// the rows above that pass this one.
pub fn node_cell(
    node: Node,
    lane: usize,
    color: u32,
    down: bool,
    through: Vec<Dashed>,
    width: Pixels,
    scroll: Pixels,
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                paint_through(&through, bounds, scroll, window);
                let center = point(lane_x(bounds, scroll, lane), bounds.center().y);
                if down && let Some(path) = dashed_line(center, point(center.x, bounds.bottom())) {
                    window.paint_path(path, rgb(color));
                }
                paint_node(node, center, color, window);
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

/// The dashed lines `through` from the top of the row to its bottom.
fn paint_through(through: &[Dashed], bounds: Bounds<Pixels>, scroll: Pixels, window: &mut Window) {
    for &(lane, color) in through {
        let x = lane_x(bounds, scroll, lane);
        if let Some(path) = dashed_line(point(x, bounds.top()), point(x, bounds.bottom())) {
            window.paint_path(path, rgb(color));
        }
    }
}

fn paint_node(node: Node, center: Point<Pixels>, color: u32, window: &mut Window) {
    let background = rgb(theme::color::BACKGROUND);
    match node {
        Node::WorkingTree => window.paint_quad(quad(
            circle_bounds(center, WORKING_TREE_RADIUS),
            WORKING_TREE_RADIUS,
            background,
            WORKING_TREE_BORDER,
            rgb(color),
            BorderStyle::Dashed,
        )),
        Node::Composer => {
            window.paint_quad(quad(
                circle_bounds(center, COMPOSER_RADIUS),
                COMPOSER_RADIUS,
                background,
                COMPOSER_BORDER,
                rgb(color),
                BorderStyle::Dashed,
            ));
            let arm = px(3.);
            let plus = rgb(theme::color::TEXT_MUTED);
            for (from, to) in [
                (
                    point(center.x - arm, center.y),
                    point(center.x + arm, center.y),
                ),
                (
                    point(center.x, center.y - arm),
                    point(center.x, center.y + arm),
                ),
            ] {
                let mut builder = PathBuilder::stroke(COMPOSER_BORDER);
                builder.move_to(from);
                builder.line_to(to);
                if let Ok(path) = builder.build() {
                    window.paint_path(path, plus);
                }
            }
        }
        Node::Preparing => window.paint_quad(quad(
            circle_bounds(center, NODE_RADIUS),
            NODE_RADIUS,
            background,
            LIVE_BORDER,
            rgb(color),
            BorderStyle::Dashed,
        )),
        Node::Running => {
            // A faint halo stands for the board's pulse: the history is not
            // animated, so idle windows cost nothing.
            window.paint_quad(quad(
                circle_bounds(center, HEAD_RING_RADIUS),
                HEAD_RING_RADIUS,
                transparent_black(),
                HEAD_RING_WIDTH,
                theme::with_alpha(color, 0.35),
                BorderStyle::default(),
            ));
            window.paint_quad(quad(
                circle_bounds(center, NODE_RADIUS),
                NODE_RADIUS,
                background,
                LIVE_BORDER,
                rgb(color),
                BorderStyle::default(),
            ));
            window.paint_quad(
                fill(circle_bounds(center, LIVE_DOT), rgb(color)).corner_radii(LIVE_DOT),
            );
        }
    }
}

fn paint_row(row: &Row, head: bool, bounds: Bounds<Pixels>, scroll: Pixels, window: &mut Window) {
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
    if head {
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
    rgb(theme::graph_color(color))
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
