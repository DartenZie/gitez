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

/// Width of the graph column for a graph `lanes` lanes wide, kept between
/// `min` and `max`. Lanes past `max` are clipped.
pub fn column_width(lanes: usize, min: Pixels, max: Pixels) -> Pixels {
    let needed = FIRST_LANE + LANE_WIDTH * lanes.saturating_sub(1) as f32 + TRAILING;
    needed.max(min).min(max)
}

/// An element that paints `row` across its bounds. `is_head` adds the ring
/// that marks the commit `HEAD` points at.
pub fn cell(row: Row, is_head: bool, width: Pixels) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                paint_row(&row, is_head, bounds, window);
            });
        },
    )
    .w(width)
    .h_full()
    .flex_none()
}

fn paint_row(row: &Row, is_head: bool, bounds: Bounds<Pixels>, window: &mut Window) {
    let top = bounds.top();
    let bottom = bounds.bottom();
    let middle = bounds.center().y;
    let x = |lane: usize| bounds.left() + FIRST_LANE + LANE_WIDTH * lane as f32;
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
    if is_head {
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

/// A line from `from` to `to`, straight when they share a lane and otherwise
/// an S-curve that leaves and arrives vertically.
fn curve(from: Point<Pixels>, to: Point<Pixels>) -> Option<Path<Pixels>> {
    if from.x == to.x {
        return line(from, to);
    }
    let bend = (from.y + to.y) / 2.;
    let mut builder = PathBuilder::stroke(LINE_WIDTH);
    builder.move_to(from);
    builder.cubic_bezier_to(to, point(from.x, bend), point(to.x, bend));
    builder.build().ok()
}

#[cfg(test)]
mod tests {
    // Not `super::*`: it brings in GPUI's `test` macro, which shadows Rust's.
    use super::column_width;
    use gpui_kit::px;

    #[test]
    fn column_width_grows_with_lanes_within_limits() {
        let (min, max) = (px(112.), px(300.));
        assert_eq!(column_width(1, min, max), min);
        assert_eq!(column_width(4, min, max), min);
        assert_eq!(column_width(5, min, max), px(18. + 4. * 22. + 14.));
        assert_eq!(column_width(100, min, max), max);
    }
}
