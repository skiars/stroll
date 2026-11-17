use std::{collections::HashSet, rc::Rc, sync::Arc};

use gpui::*;
use gpui_component::ThemeMode;

use crate::git::Commit;

const LANES: usize = 8;
const LANE_WIDTH: f32 = 12.;
const FIRST_X: f32 = 8.;
pub(super) const WORKTREE_WIDTH: Pixels = px(LANE_WIDTH + FIRST_X);
const DOT_RADIUS: f32 = 2.75;
const MERGE_RADIUS: f32 = 4.;
const CORNER: f32 = 4.;
const TRACK_WIDTH: f32 = 1.35;
const DECORATED_HEIGHT: f32 = 65.;
const ROW_EDGE_OVERLAP: f32 = 0.5;
const LIGHT: [u32; 9] = [
    0x008252, 0xa66a00, 0xce244e, 0x2459d8, 0x567800, 0x6940d5, 0xc04f09, 0x007e9c, 0xad259d,
];
const DARK: [u32; 9] = [
    0x38db92, 0xffc34d, 0xff6085, 0x6e9dff, 0xb7e84a, 0xb399ff, 0xff914a, 0x2bd9ee, 0xf378df,
];

#[derive(Clone)]
pub(super) struct GraphRow {
    lane: usize,
    width: usize,
    color: usize,
    merge: bool,
    #[cfg(test)]
    edges: Vec<Edge>,
    height: Pixels,
    tracks: Arc<[(usize, Path<Pixels>)]>,
}

#[derive(Clone, Copy)]
struct Edge {
    from: usize,
    to: usize,
    color: usize,
    section: Section,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Incoming,
    Outgoing,
    Through,
}

#[derive(Clone)]
struct Pending {
    oid: String,
    color: usize,
    joins: bool,
}

fn branch_color(next: &mut usize, used: &mut HashSet<usize>) -> usize {
    let color = (0..8)
        .map(|offset| (*next + offset) % 8 + 1)
        .find(|color| !used.contains(color))
        .unwrap_or(*next + 1);
    *next = color % 8;
    used.insert(color);
    color
}

#[derive(Clone, Default)]
pub(super) struct Graph {
    pending: Vec<Pending>,
    next_color: usize,
    started: bool,
}

impl Graph {
    /// Continues the same lane assignment across pages, including merge joins
    /// whose targets are in a later page.
    pub(super) fn append(&mut self, commits: &[Commit], filtered: bool) -> Vec<GraphRow> {
        commits
            .iter()
            .map(|commit| {
                let oid = commit.oid.as_str();
                let merge = commit.parents.len() > 1;
                if filtered {
                    return GraphRow::new(0, 1, 0, merge, Vec::new());
                }
                let pending = &mut self.pending;
                let next_color = &mut self.next_color;
                let first = !std::mem::replace(&mut self.started, true);
                let mut used = pending
                    .iter()
                    .map(|lane| lane.color)
                    .collect::<HashSet<_>>();
                let lane = pending
                    .iter()
                    .position(|lane| lane.oid == oid && !lane.joins)
                    .unwrap_or_else(|| {
                        let color = if first {
                            0
                        } else {
                            branch_color(next_color, &mut used)
                        };
                        pending.push(Pending {
                            oid: oid.to_owned(),
                            color,
                            joins: false,
                        });
                        pending.len() - 1
                    });
                let before = std::mem::take(pending);
                let color = before[lane].color;
                let insertion = before[..lane].iter().filter(|item| item.oid != oid).count();
                pending.extend(before.iter().filter(|item| item.oid != oid).cloned());
                let mut parent_ids = HashSet::new();
                let parents = commit
                    .parents
                    .iter()
                    .filter(|oid| parent_ids.insert(*oid))
                    .enumerate()
                    .map(|(ix, oid)| Pending {
                        oid: oid.clone(),
                        color: if ix == 0 {
                            color
                        } else {
                            branch_color(next_color, &mut used)
                        },
                        joins: pending.iter().any(|item| item.oid == *oid),
                    })
                    .collect::<Vec<_>>();
                let mut edges = Vec::new();
                let mut through = 0;
                for (ix, item) in before.iter().enumerate() {
                    let (to, section) = if item.oid == oid {
                        (lane, Section::Incoming)
                    } else {
                        let to = through
                            + if through >= insertion {
                                parents.len()
                            } else {
                                0
                            };
                        through += 1;
                        (to, Section::Through)
                    };
                    edges.push(Edge {
                        from: ix,
                        to,
                        color: item.color,
                        section,
                    });
                }
                edges.extend(parents.iter().enumerate().map(|(ix, parent)| Edge {
                    from: lane,
                    to: insertion + ix,
                    color: parent.color,
                    section: Section::Outgoing,
                }));
                pending.splice(insertion..insertion, parents);
                edges.retain(|edge| edge.from < LANES || edge.to < LANES);
                for edge in &mut edges {
                    edge.from = edge.from.min(LANES);
                    edge.to = edge.to.min(LANES);
                }
                GraphRow::new(
                    lane,
                    before.len().max(pending.len()).clamp(1, LANES),
                    color,
                    merge,
                    edges,
                )
            })
            .collect()
    }
}

impl GraphRow {
    fn new(lane: usize, width: usize, color: usize, merge: bool, edges: Vec<Edge>) -> Self {
        let radius = if merge { MERGE_RADIUS } else { DOT_RADIUS };
        let lead_from_node = radius + 2.;
        let mut height: f32 = if merge { DECORATED_HEIGHT } else { 54. };
        for edge in edges.iter().filter(|edge| edge.from != edge.to) {
            let span = edge.from.abs_diff(edge.to) as f32 * LANE_WIDTH + 2. * CORNER;
            height = height.max(if edge.section == Section::Through {
                span + 4.
            } else {
                2. * (span + lead_from_node + 1.)
            });
        }
        let middle = height / 2.;
        let ring = if merge { radius } else { 0. };
        let lane_x = |lane: usize| FIRST_X + lane as f32 * LANE_WIDTH;
        let straight_through_node = !merge
            && edges.iter().any(|edge| {
                edge.section == Section::Incoming && edge.from == lane && edge.to == lane
            })
            && edges.iter().any(|edge| {
                edge.section == Section::Outgoing && edge.from == lane && edge.to == lane
            });
        let tracks = edges
            .iter()
            .filter_map(|edge| {
                let x1 = lane_x(edge.from);
                let x2 = lane_x(edge.to);
                let section = if straight_through_node && edge.from == lane && edge.to == lane {
                    if edge.section == Section::Outgoing {
                        return None;
                    }
                    Section::Through
                } else {
                    edge.section
                };
                let top = if section == Section::Outgoing {
                    middle
                } else {
                    0.
                };
                let bottom = if section == Section::Incoming {
                    middle
                } else {
                    height
                };
                let path_top = if section == Section::Outgoing {
                    top + ring
                } else {
                    top
                };
                let path_bottom = if section == Section::Incoming {
                    bottom - ring
                } else {
                    bottom
                };
                let mut path = track(LineCap::Butt);
                path.move_to(point(px(x1), px(path_top)));
                if x1 == x2 {
                    path.line_to(point(px(x2), px(path_bottom)));
                    return Some((edge.color, path.build().ok()?));
                }

                let diagonal = (x2 - x1).abs();
                let slack = bottom - top - diagonal - 2. * CORNER;
                let lead = match section {
                    Section::Outgoing => lead_from_node,
                    Section::Incoming => slack - lead_from_node,
                    Section::Through => slack / 2.,
                };
                let start = top + lead + CORNER;
                let end = start + diagonal;
                let inset = (x2 - x1).signum() * CORNER;
                path.line_to(point(px(x1), px(start - CORNER)));
                path.curve_to(
                    point(px(x1 + inset), px(start + CORNER)),
                    point(px(x1), px(start)),
                );
                path.line_to(point(px(x2 - inset), px(end - CORNER)));
                path.curve_to(point(px(x2), px(end + CORNER)), point(px(x2), px(end)));
                path.line_to(point(px(x2), px(path_bottom)));
                Some((edge.color, path.build().ok()?))
            })
            .collect();
        Self {
            lane,
            width,
            color,
            merge,
            #[cfg(test)]
            edges,
            height: px(height),
            tracks,
        }
    }

    pub(super) fn width(&self) -> Pixels {
        px(self.width as f32 * LANE_WIDTH + FIRST_X)
    }

    pub(super) fn color(&self, mode: ThemeMode) -> Hsla {
        color(mode, self.color)
    }

    pub(super) fn height(&self, has_references: bool) -> Pixels {
        if has_references {
            self.height.max(px(DECORATED_HEIGHT))
        } else {
            self.height
        }
    }
}

pub(super) fn graph(
    rows: Rc<Vec<GraphRow>>,
    ix: usize,
    mode: ThemeMode,
) -> impl IntoElement + Styled {
    let width = rows[ix].width();
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let row = &rows[ix];
            let center = bounds.center().y;
            let center_offset = center - row.height / 2.;
            let mut clip = window.content_mask();
            clip.bounds.origin.y = bounds.top();
            clip.bounds.size.height = bounds.size.height;
            // Stroke both straights and curves with the same fractional width. Let the
            // row's actual device-aligned edges clip the leads, so overlaps don't darken.
            let place = |vertex: Point<Pixels>| {
                let y = if vertex.y <= px(0.) {
                    bounds.top() - px(ROW_EDGE_OVERLAP)
                } else if vertex.y >= row.height {
                    bounds.bottom() + px(ROW_EDGE_OVERLAP)
                } else {
                    vertex.y + center_offset
                };
                point(vertex.x + bounds.left(), y)
            };
            let device_pixel = px(1. / window.scale_factor());
            window.with_content_mask(Some(clip), |window| {
                for (lane_color, track) in row.tracks.iter() {
                    let mut path = track.clone();
                    path.bounds = Bounds::from_corners(
                        place(path.bounds.origin),
                        place(path.bounds.bottom_right()),
                    )
                    // Include the pixels that contain only partial stroke coverage.
                    .dilate(device_pixel);
                    for vertex in &mut path.vertices {
                        vertex.xy_position = place(vertex.xy_position);
                    }
                    window.paint_path(path, color(mode, *lane_color));
                }
            });
            if row.lane < LANES {
                paint_node(
                    window,
                    bounds.origin
                        + point(
                            px(FIRST_X + row.lane as f32 * LANE_WIDTH),
                            bounds.size.height / 2.,
                        ),
                    if row.merge { MERGE_RADIUS } else { DOT_RADIUS },
                    row.merge,
                    color(mode, row.color),
                );
            }
        },
    )
    .w(width)
    .h_full()
    .flex_shrink_0()
}

pub(super) fn worktree(connected: bool, mode: ThemeMode) -> impl IntoElement + Styled {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let center = bounds.origin + point(px(FIRST_X), bounds.size.height / 2.);
            let color = main_color(mode);
            let radius = 3.5;
            paint_node(window, center, radius, true, color);
            if connected {
                let mut path = track(LineCap::Round).dash_array(&[px(2.), px(2.)]);
                path.move_to(point(center.x, bounds.bottom()));
                path.line_to(point(center.x, center.y + px(radius)));
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            }
        },
    )
    .w(WORKTREE_WIDTH)
    .h_full()
    .flex_shrink_0()
}

fn track(cap: LineCap) -> PathBuilder {
    PathBuilder::stroke(px(TRACK_WIDTH)).with_style(PathStyle::Stroke(
        StrokeOptions::default()
            .with_line_width(TRACK_WIDTH)
            .with_line_cap(cap)
            .with_line_join(LineJoin::Round),
    ))
}

pub(super) fn main_color(mode: ThemeMode) -> Hsla {
    color(mode, 0)
}

fn color(mode: ThemeMode, index: usize) -> Hsla {
    rgb(if mode == ThemeMode::Dark {
        DARK[index]
    } else {
        LIGHT[index]
    })
    .into()
}

fn paint_node(window: &mut Window, center: Point<Pixels>, radius: f32, hollow: bool, color: Hsla) {
    let radius = px(radius);
    window.paint_quad(quad(
        Bounds::new(
            center - point(radius, radius),
            size(radius * 2., radius * 2.),
        ),
        radius,
        if hollow { transparent_black() } else { color },
        px(if hollow { 1.5 } else { 0. }),
        color,
        BorderStyle::default(),
    ));
}

#[cfg(test)]
mod tests {
    use super::{Commit, Graph, Section};
    #[test]
    fn merge_tracks_join_at_the_common_ancestor_and_search_disconnects_them() {
        let commit = |oid: &str, parents: &[&str]| Commit {
            oid: oid.into(),
            short: oid.into(),
            parents: parents.iter().map(|parent| (*parent).into()).collect(),
            author: String::new(),
            author_email: String::new(),
            authored_at: String::new(),
            committer: String::new(),
            committer_email: String::new(),
            committed_at: String::new(),
            subject: String::new(),
            body: String::new(),
        };
        let commits = [
            commit("merge", &["left", "right"]),
            commit("left", &["base"]),
            commit("right", &["base"]),
            commit("base", &["root"]),
            commit("root", &[]),
        ];
        let mut layout = Graph::default();
        let mut graph = layout.append(&commits[..2], false);
        graph.extend(layout.append(&commits[2..], false));
        assert_eq!(graph[0].width, 2);
        assert_eq!(graph[2].lane, 1);
        assert!(graph[3].edges.iter().any(|edge| {
            edge.from == 1 && edge.to == 0 && edge.section == Section::Incoming && edge.color == 1
        }));
        assert_eq!(graph[4].width, 1);
        assert!(
            Graph::default()
                .append(&commits, true)
                .iter()
                .all(|row| row.edges.is_empty())
        );
    }
}
