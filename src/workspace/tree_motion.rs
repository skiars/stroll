use std::{
    collections::HashMap,
    hash::Hash,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{VirtualListScrollHandle, animation::ease_in_out_cubic, v_virtual_list};

pub(super) const ROW_HEIGHT: Pixels = px(28.);
const DURATION: Duration = Duration::from_millis(180);

pub(super) struct TreeMotion<R> {
    last: Option<Rc<Vec<R>>>,
    transition: Option<Transition<R>>,
    requested: bool,
}

impl<R> Default for TreeMotion<R> {
    fn default() -> Self {
        Self {
            last: None,
            transition: None,
            requested: false,
        }
    }
}

impl<R: Clone + 'static> TreeMotion<R> {
    pub(super) fn start(&mut self) {
        self.requested = true;
    }

    pub(super) fn frame<K: Eq + Hash>(
        &mut self,
        rows: &Rc<Vec<R>>,
        key: impl Fn(&R) -> K,
        enabled: bool,
        window: &Window,
        cx: &App,
    ) -> Option<Rc<TreeFrame<R>>> {
        let now = cx.background_executor().now();
        self.update(rows, &key, enabled && !cx.reduce_motion(), false, now);
        let frame = self.current_frame(now, rows.len())?;
        window.request_animation_frame();
        Some(Rc::new(frame))
    }

    pub(super) fn refresh<K: Eq + Hash>(
        &mut self,
        previous: &Rc<Vec<R>>,
        rows: &Rc<Vec<R>>,
        key: impl Fn(&R) -> K,
        scroll: Option<&UniformListScrollHandle>,
        cx: &App,
    ) {
        let now = cx.background_executor().now();
        let same_target = self
            .last
            .as_ref()
            .is_some_and(|last| Rc::ptr_eq(last, previous));
        let before = scroll
            .filter(|scroll| scroll.0.borrow().base_handle.offset().y < px(0.))
            .filter(|_| same_target)
            .and_then(|_| self.current_frame(now, previous.len()));
        self.update(
            rows,
            &key,
            scroll.is_some() && !cx.reduce_motion(),
            same_target,
            now,
        );
        let Some(scroll) = scroll else { return };
        let mut state = scroll.0.borrow_mut();
        let offset = state.base_handle.offset();
        if offset.y >= px(0.) && state.deferred_scroll_to_item.is_none() {
            return;
        }
        let indices: HashMap<_, _> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| (key(row), index))
            .collect();
        let new_index = |row: &R| indices.get(&key(row)).copied();
        state.deferred_scroll_to_item = state.deferred_scroll_to_item.and_then(|mut request| {
            request.item_index = new_index(previous.get(request.item_index)?)?;
            Some(request)
        });

        if offset.y < px(0.) {
            let top = match &before {
                Some(frame) => frame.anchor_index(-offset.y),
                None => Some((-offset.y / ROW_HEIGHT) as usize),
            };
            let Some(top) = top.filter(|top| *top < previous.len()) else {
                return;
            };
            let after = self.current_frame(now, rows.len());
            let row_top = |frame: &Option<TreeFrame<R>>, index| match frame {
                Some(frame) => frame.row_top(index),
                None => Some(ROW_HEIGHT * index),
            };
            // Prefer the top row, then the nearest surviving row below it.
            let anchor = (top..previous.len()).chain((0..top).rev()).find_map(|old| {
                let new = new_index(&previous[old])?;
                Some(row_top(&before, old)? - row_top(&after, new)?)
            });
            if let Some(delta) = anchor {
                state
                    .base_handle
                    .set_offset(point(offset.x, (offset.y + delta).min(px(0.))));
            }
        }
    }

    fn update<K: Eq + Hash>(
        &mut self,
        rows: &Rc<Vec<R>>,
        key: impl Fn(&R) -> K,
        enabled: bool,
        refresh: bool,
        now: Instant,
    ) {
        let requested = std::mem::take(&mut self.requested);
        let previous = self.last.replace(rows.clone());
        if !enabled {
            self.transition = None;
            return;
        }
        self.transition.take_if(|motion| motion.progress(now) >= 1.);
        if let Some(previous) = previous.filter(|previous| !Rc::ptr_eq(previous, rows)) {
            let current = self.transition.take();
            self.transition = if requested {
                let mut next = Transition::new(now);
                if let Some(motion) = current {
                    let progress = motion.progress(now);
                    let current = motion
                        .rows
                        .iter()
                        .zip(&motion.layout)
                        .map(|(row, layout)| (row, layout.height(progress)));
                    next.retarget(current, rows, &key);
                } else {
                    next.retarget(previous.iter().map(|row| (row, 1.)), rows, &key);
                }
                Some(next)
            } else if refresh {
                current.map(|motion| motion.refresh(rows, &key))
            } else {
                None
            }
            .filter(Transition::moves);
        }
    }

    fn current_frame(&self, now: Instant, row_count: usize) -> Option<TreeFrame<R>> {
        let motion = self.transition.as_ref()?;
        let progress = motion.progress(now);
        (progress < 1.).then(|| TreeFrame::new(motion, progress, row_count))
    }
}

struct Transition<R> {
    started: Instant,
    rows: Rc<Vec<R>>,
    layout: Vec<RowMotion>,
}

impl<R: Clone> Transition<R> {
    fn new(started: Instant) -> Self {
        Self {
            started,
            rows: Rc::new(Vec::new()),
            layout: Vec::new(),
        }
    }

    fn progress(&self, now: Instant) -> f32 {
        ease_in_out_cubic(
            now.saturating_duration_since(self.started).as_secs_f32() / DURATION.as_secs_f32(),
        )
    }

    fn moves(&self) -> bool {
        self.layout.iter().any(|row| row.from != row.to)
    }

    fn retarget<'a, K: Eq + Hash>(
        &mut self,
        current: impl DoubleEndedIterator<Item = (&'a R, f32)>,
        target: &[R],
        key: impl Fn(&R) -> K,
    ) where
        R: 'a,
    {
        self.place_rows(current, target, key, 0., |height, index| {
            (index.is_some() || height > 0.).then(|| RowMotion::new(index, height))
        });
        let mut remaining = &mut self.layout[..];
        while let Some(first) = remaining.first() {
            let direction = (first.to - first.from).signum();
            let len = remaining
                .iter()
                .take_while(|row| (row.to - row.from).signum() == direction && row.to != row.from)
                .count()
                .max(1);
            let (group, rest) = remaining.split_at_mut(len);
            remaining = rest;
            let total = group.iter().map(RowMotion::delta).sum();
            let mut offset = 0.;
            for row in group {
                row.span = total;
                row.offset = if direction > 0. {
                    offset
                } else {
                    total - offset - row.delta()
                };
                offset += row.delta();
            }
        }
    }

    fn refresh<K: Eq + Hash>(self, target: &[R], key: impl Fn(&R) -> K) -> Self {
        let mut next = Self::new(self.started);
        // Keep the original timing and height of each animated row across a passive refresh.
        next.place_rows(
            self.rows.iter().zip(self.layout),
            target,
            key,
            1.,
            |mut motion, index| match (motion.target, index) {
                (Some(_), None) => None,
                (None, Some(_)) => Some(RowMotion::new(index, 1.)),
                _ => {
                    motion.target = index;
                    Some(motion)
                }
            },
        );
        next
    }

    fn place_rows<'a, M, K: Eq + Hash>(
        &mut self,
        current: impl DoubleEndedIterator<Item = (&'a R, M)>,
        target: &[R],
        key: impl Fn(&R) -> K,
        inserted_height: f32,
        motion_for_row: impl Fn(M, Option<usize>) -> Option<RowMotion>,
    ) where
        R: 'a,
    {
        let indices: HashMap<_, _> = target
            .iter()
            .enumerate()
            .map(|(index, row)| (key(row), index))
            .collect();
        let mut entrances: Vec<_> = (0..target.len()).map(|_| None).collect();
        let mut exits: Vec<_> = (0..=target.len()).map(|_| Vec::new()).collect();
        let mut before = target.len();
        // Keep an exiting row before its next surviving neighbor.
        for (row, motion) in current.rev() {
            let index = indices.get(&key(row)).copied();
            let Some(motion) = motion_for_row(motion, index) else {
                continue;
            };
            if let Some(index) = index {
                entrances[index] = Some(motion);
                before = index;
            } else {
                exits[before].push((row, motion));
            }
        }
        let rows = Rc::get_mut(&mut self.rows).unwrap();
        for (index, exiting) in exits.into_iter().enumerate() {
            for (row, motion) in exiting.into_iter().rev() {
                rows.push(row.clone());
                self.layout.push(motion);
            }
            if let Some(row) = target.get(index) {
                rows.push(row.clone());
                self.layout.push(
                    entrances[index]
                        .take()
                        .unwrap_or_else(|| RowMotion::new(Some(index), inserted_height)),
                );
            }
        }
    }
}

struct RowMotion {
    target: Option<usize>,
    from: f32,
    to: f32,
    span: f32,
    offset: f32,
}

impl RowMotion {
    fn new(target: Option<usize>, from: f32) -> Self {
        Self {
            target,
            from,
            to: f32::from(target.is_some()),
            span: 0.,
            offset: 0.,
        }
    }

    fn delta(&self) -> f32 {
        (self.to - self.from).abs()
    }

    fn height(&self, progress: f32) -> f32 {
        let distance = (self.span * progress - self.offset).clamp(0., self.delta());
        (self.from + distance.copysign(self.to - self.from)).clamp(0., 1.)
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct RowVisual {
    pub exiting: bool,
}

pub(super) struct TreeFrame<R> {
    pub rows: Rc<Vec<R>>,
    visible: Vec<PlacedRow>,
    positions: Vec<Option<(usize, Pixels)>>,
    height: Pixels,
}

struct PlacedRow {
    index: usize,
    target: Option<usize>,
    top: Pixels,
    height: Pixels,
}

impl<R: Clone> TreeFrame<R> {
    fn new(motion: &Transition<R>, progress: f32, count: usize) -> Self {
        let mut visible = Vec::new();
        let mut positions = vec![None; count];
        let mut top = px(0.);
        for (index, row) in motion.layout.iter().enumerate() {
            let height = ROW_HEIGHT * row.height(progress);
            // Skip fully clipped rows so a large subtree never makes the
            // virtual list render a run of zero-height elements.
            if height <= px(0.) {
                continue;
            }
            if let Some(target) = row.target {
                positions[target] = Some((visible.len(), top));
            }
            visible.push(PlacedRow {
                index,
                target: row.target,
                top,
                height,
            });
            top += height;
        }
        Self {
            rows: motion.rows.clone(),
            visible,
            positions,
            height: top,
        }
    }

    pub(super) fn row_top(&self, index: usize) -> Option<Pixels> {
        Some(self.positions.get(index)?.as_ref()?.1)
    }

    pub(super) fn row_at(&self, offset: Pixels) -> Option<usize> {
        let index = self
            .visible
            .partition_point(|row| row.top + row.height <= offset);
        self.visible.get(index)?.target
    }

    fn anchor_index(&self, offset: Pixels) -> Option<usize> {
        let index = self
            .visible
            .partition_point(|row| row.top + row.height <= offset);
        self.visible[index..]
            .iter()
            .chain(self.visible[..index].iter().rev())
            .find_map(|row| row.target)
    }

    pub(super) fn height(&self) -> Pixels {
        self.height
    }
}

pub(super) fn tree_list<V: Render, R: Clone + 'static, E: IntoElement>(
    view: Entity<V>,
    id: &'static str,
    count: usize,
    scroll: &UniformListScrollHandle,
    horizontal: bool,
    frame: Option<Rc<TreeFrame<R>>>,
    render: impl Fn(usize, RowVisual, &mut App) -> Option<E> + 'static,
) -> AnyElement {
    if let Some(frame) = frame {
        let mut state = scroll.0.borrow_mut();
        let handle = VirtualListScrollHandle::from(state.base_handle.clone());
        if let Some(request) = state.deferred_scroll_to_item
            && let Some(Some((index, _))) = frame.positions.get(request.item_index)
        {
            handle.scroll_to_item(*index, request.strategy);
            // Apply the sticky header inset once the rows return to their uniform heights.
            if request.offset == 0 {
                state.deferred_scroll_to_item = None;
            }
        }
        drop(state);
        let sizes = Rc::new(
            frame
                .visible
                .iter()
                .map(|row| size(px(0.), row.height))
                .collect(),
        );
        v_virtual_list(
            view,
            (ElementId::from(id), "motion"),
            sizes,
            move |_, range, _, cx| {
                range
                    .map(|ix| {
                        let row = &frame.visible[ix];
                        let visual = RowVisual {
                            exiting: row.target.is_none(),
                        };
                        div()
                            .relative()
                            .w_full()
                            .h(row.height)
                            .overflow_hidden()
                            .children(render(row.index, visual, cx))
                            .when(visual.exiting, |row| {
                                row.child(div().absolute().inset_0().block_mouse_except_scroll())
                            })
                    })
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&handle)
        .size_full()
        .into_any_element()
    } else {
        uniform_list(id, count, move |range, _, cx| {
            range
                .filter_map(|ix| render(ix, RowVisual::default(), cx))
                .collect::<Vec<_>>()
        })
        .when(horizontal, |mut list| {
            list.style().restrict_scroll_to_axis = Some(true);
            list
        })
        .track_scroll(scroll)
        .size_full()
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn reversing_a_large_tree_reveal_preserves_the_current_geometry() {
        // A large subtree between two permanently visible rows.
        let closed = [0usize, 10_001];
        let target = (0..=10_001).collect::<Vec<_>>();
        let now = Instant::now();
        let mut opening = Transition {
            started: now,
            rows: Rc::new(Vec::new()),
            layout: Vec::new(),
        };
        opening.retarget(closed.iter().map(|id| (id, 1.)), &target, |id| *id);

        let halfway = TreeFrame::new(&opening, 0.5, target.len());
        assert_eq!(halfway.visible.len(), 5_002);
        assert_eq!(halfway.row_top(10_001), Some(ROW_HEIGHT * 5_001));

        let current = opening
            .rows
            .iter()
            .zip(&opening.layout)
            .map(|(id, row)| (id, row.height(0.5)));
        let mut closing = Transition {
            started: now,
            rows: Rc::new(Vec::new()),
            layout: Vec::new(),
        };
        closing.retarget(current, &closed, |id| *id);
        let reversed = TreeFrame::new(&closing, 0., closed.len());
        assert_eq!(reversed.row_top(1), halfway.row_top(10_001));
        assert!(
            reversed.visible[1..5_001]
                .iter()
                .all(|row| row.target.is_none())
        );
        let settled = TreeFrame::new(&closing, 1., closed.len());
        assert_eq!(settled.visible.len(), 2);
        assert_eq!(settled.row_top(1), Some(ROW_HEIGHT));
    }
}
