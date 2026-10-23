//! Lane layout for the commit graph.
//!
//! [`GraphLayout`] turns commits, fed one at a time in history order (every
//! child before its parents), into [`Row`]s that say which lane each commit
//! sits in and which line segments cross the row. It keeps only the currently
//! open lanes, so it works on a stream of commits as they are read.
//!
//! Lanes never move sideways: a line stays in its column from the commit that
//! opens it to the commit that closes it, and freed columns are reused. Lines
//! that lead to the same parent stay separate until they meet at that parent,
//! so a fork point is drawn where its branches converge.
//!
//! Each row is drawn from its top edge to its bottom edge with the commit
//! node in the middle. Lanes are column indices from the left; colours are
//! identities to map onto a palette (`palette[color % palette.len()]`).

use crate::Oid;

/// A line segment within one row of the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Segment {
    /// A line passes straight through the row in `lane`, past the commit.
    Through { lane: usize, color: usize },
    /// A line comes in at the top of the row in `lane` and ends at the
    /// commit: the commit is that line's parent.
    In { lane: usize, color: usize },
    /// A line leaves the commit and goes out at the bottom of the row in
    /// `lane`, towards one of the commit's parents.
    Out { lane: usize, color: usize },
}

/// The graph for one commit row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The lane the commit node sits in.
    pub lane: usize,
    /// Colour of the commit node, the same as its first-parent line.
    pub color: usize,
    /// Segments crossing this row, in no particular order.
    pub segments: Vec<Segment>,
    /// Lanes in use at the top or bottom of the row. Lanes `0..width` may
    /// hold lines; the graph column must be at least this wide.
    pub width: usize,
}

#[derive(Debug, Clone, Copy)]
struct Line {
    /// The commit this line leads to.
    parent: Oid,
    color: usize,
}

/// Incremental lane assignment; see the [module docs](self).
#[derive(Debug, Default, Clone)]
pub struct GraphLayout {
    lanes: Vec<Option<Line>>,
    next_color: usize,
}

impl GraphLayout {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lays out the next commit. Call in history order, with every child
    /// before its parents.
    pub fn push(&mut self, id: Oid, parents: &[Oid]) -> Row {
        let mut segments = Vec::with_capacity(self.lanes.len() + parents.len());
        let top_width = self.lanes.len();

        // The leftmost line arriving at this commit becomes its lane; a commit
        // nothing points at yet (a branch tip) opens a new one.
        let arriving = self
            .lanes
            .iter()
            .position(|line| line.is_some_and(|l| l.parent == id));
        let (lane, color) = match arriving {
            Some(lane) => {
                let color = self.lanes[lane].map_or(0, |line| line.color);
                (lane, color)
            }
            None => (self.free_lane(), self.new_color()),
        };

        for (index, slot) in self.lanes.iter_mut().enumerate() {
            let Some(line) = *slot else { continue };
            if line.parent == id {
                segments.push(Segment::In {
                    lane: index,
                    color: line.color,
                });
                *slot = None;
            } else {
                segments.push(Segment::Through {
                    lane: index,
                    color: line.color,
                });
            }
        }

        for (nth, &parent) in parents.iter().enumerate() {
            if parents[..nth].contains(&parent) {
                continue;
            }
            let (out_lane, out_color) = if nth == 0 {
                // The first parent continues the commit's own line.
                self.set(lane, Line { parent, color });
                (lane, color)
            } else if let Some((existing, line)) = self.line_to(parent) {
                // Another line already leads to this parent; join it.
                (existing, line.color)
            } else {
                let new_lane = self.free_lane();
                let new_color = self.new_color();
                self.set(
                    new_lane,
                    Line {
                        parent,
                        color: new_color,
                    },
                );
                (new_lane, new_color)
            };
            segments.push(Segment::Out {
                lane: out_lane,
                color: out_color,
            });
        }

        while self.lanes.last().is_some_and(Option::is_none) {
            self.lanes.pop();
        }

        Row {
            lane,
            color,
            segments,
            width: top_width.max(self.lanes.len()).max(lane + 1),
        }
    }

    /// Number of lanes open below the last row.
    pub fn open_lanes(&self) -> usize {
        self.lanes.iter().flatten().count()
    }

    fn free_lane(&self) -> usize {
        self.lanes
            .iter()
            .position(Option::is_none)
            .unwrap_or(self.lanes.len())
    }

    fn set(&mut self, lane: usize, line: Line) {
        if lane >= self.lanes.len() {
            self.lanes.resize(lane + 1, None);
        }
        self.lanes[lane] = Some(line);
    }

    fn line_to(&self, parent: Oid) -> Option<(usize, Line)> {
        self.lanes
            .iter()
            .enumerate()
            .find_map(|(lane, line)| line.filter(|l| l.parent == parent).map(|l| (lane, l)))
    }

    fn new_color(&mut self) -> usize {
        let color = self.next_color;
        self.next_color += 1;
        color
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Segment::*;

    fn oid(n: u8) -> Oid {
        Oid::from_bytes_or_panic(&[n; 20])
    }

    /// Lays out `(id, parents)` pairs, returning rows with sorted segments.
    fn layout(commits: &[(u8, &[u8])]) -> Vec<Row> {
        let mut layout = GraphLayout::new();
        let rows = commits
            .iter()
            .map(|(id, parents)| {
                let parents: Vec<Oid> = parents.iter().copied().map(oid).collect();
                let mut row = layout.push(oid(*id), &parents);
                row.segments.sort_by_key(|s| format!("{s:?}"));
                row
            })
            .collect();
        assert_eq!(layout.open_lanes(), 0, "every line should end at a commit");
        rows
    }

    fn lanes(rows: &[Row]) -> Vec<usize> {
        rows.iter().map(|row| row.lane).collect()
    }

    #[test]
    fn linear_history_stays_in_one_lane() {
        let rows = layout(&[(3, &[2]), (2, &[1]), (1, &[])]);
        assert_eq!(lanes(&rows), [0, 0, 0]);
        assert_eq!(rows[0].segments, [Out { lane: 0, color: 0 }]);
        assert_eq!(
            rows[1].segments,
            [In { lane: 0, color: 0 }, Out { lane: 0, color: 0 }]
        );
        assert_eq!(rows[2].segments, [In { lane: 0, color: 0 }]);
        assert!(rows.iter().all(|row| row.width == 1 && row.color == 0));
    }

    #[test]
    fn branches_converge_at_the_fork_point() {
        // 4 (feature) and 3 (main) both branch off 1.
        //   4   lane 0
        //   | 3 lane 1
        //   2 | lane 0
        //   1   lanes 0 and 1 meet
        let rows = layout(&[(4, &[2]), (3, &[1]), (2, &[1]), (1, &[])]);
        assert_eq!(lanes(&rows), [0, 1, 0, 0]);
        assert_eq!(
            rows[1].segments,
            [Out { lane: 1, color: 1 }, Through { lane: 0, color: 0 }]
        );
        assert_eq!(
            rows[3].segments,
            [In { lane: 0, color: 0 }, In { lane: 1, color: 1 }]
        );
        assert_eq!(rows[3].width, 2);
    }

    #[test]
    fn merge_opens_a_lane_for_the_second_parent() {
        // 4 merges 3 (main) and 2 (feature), both off 1.
        let rows = layout(&[(4, &[3, 2]), (3, &[1]), (2, &[1]), (1, &[])]);
        assert_eq!(lanes(&rows), [0, 0, 1, 0]);
        assert_eq!(
            rows[0].segments,
            [Out { lane: 0, color: 0 }, Out { lane: 1, color: 1 }]
        );
        assert_eq!(
            rows[2].segments,
            [
                In { lane: 1, color: 1 },
                Out { lane: 1, color: 1 },
                Through { lane: 0, color: 0 }
            ]
        );
        assert_eq!(rows[3].width, 2);
    }

    #[test]
    fn merge_joins_a_line_already_heading_to_its_parent() {
        // 5 (a tip) and 4 (a merge) both lead to 2; the merge's second parent
        // line joins the lane that already leads there.
        let rows = layout(&[(5, &[2]), (4, &[3, 2]), (3, &[1]), (2, &[1]), (1, &[])]);
        assert_eq!(lanes(&rows), [0, 1, 1, 0, 0]);
        assert_eq!(
            rows[1].segments,
            [
                Out { lane: 0, color: 0 },
                Out { lane: 1, color: 1 },
                Through { lane: 0, color: 0 }
            ]
        );
    }

    #[test]
    fn octopus_merge_opens_a_lane_per_parent() {
        let rows = layout(&[(5, &[4, 3, 2]), (4, &[1]), (3, &[1]), (2, &[1]), (1, &[])]);
        assert_eq!(lanes(&rows), [0, 0, 1, 2, 0]);
        assert_eq!(rows[0].width, 3);
        assert_eq!(
            rows[4].segments,
            [
                In { lane: 0, color: 0 },
                In { lane: 1, color: 1 },
                In { lane: 2, color: 2 }
            ]
        );
    }

    #[test]
    fn freed_lanes_are_reused_and_trailing_lanes_trimmed() {
        // Two unrelated histories one after the other: the second starts in
        // lane 0 again once the first has ended.
        let rows = layout(&[(4, &[3]), (3, &[]), (2, &[1]), (1, &[])]);
        assert_eq!(lanes(&rows), [0, 0, 0, 0]);
        assert_ne!(rows[0].color, rows[2].color);
        assert!(rows.iter().all(|row| row.width == 1));
    }

    #[test]
    fn duplicate_parents_draw_one_line() {
        let rows = layout(&[(2, &[1, 1]), (1, &[])]);
        assert_eq!(rows[0].segments, [Out { lane: 0, color: 0 }]);
    }

    #[test]
    fn lines_to_missing_parents_stay_open() {
        // A commit whose parent is never walked (hidden or shallow) keeps its
        // line open; the layout cannot know it will not arrive.
        let mut layout = GraphLayout::new();
        layout.push(oid(2), &[oid(1)]);
        assert_eq!(layout.open_lanes(), 1);
    }
}
