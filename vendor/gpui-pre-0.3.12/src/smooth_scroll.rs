use std::{
    rc::{Rc, Weak},
    time::Duration,
};

use collections::FxHashMap;
use scheduler::Instant;

use crate::{App, Pixels, Point, Window, point};

const HALF_LIFE: Duration = Duration::from_millis(24);
// A stalled UI frame should not spend the entire scroll distance at once.
const MAX_FRAME_STEP: Duration = Duration::from_millis(25);
const SETTLE_DISTANCE: f32 = 0.5;

type ApplyScroll =
    dyn FnMut(Point<Pixels>, Point<Pixels>, &mut Window, &mut App) -> Option<Point<Pixels>>;

/// Identifies a scroll viewport across frames of discrete wheel scrolling.
#[derive(Clone, Debug, Default)]
pub struct SmoothScroll(Rc<()>);

#[derive(Default)]
pub(crate) struct ScrollAnimations {
    active: FxHashMap<usize, ScrollAnimation>,
    frame_pending: bool,
    generation: u64,
}

impl ScrollAnimations {
    pub(crate) fn cancel(&mut self) {
        self.active.clear();
        self.generation = self.generation.wrapping_add(1);
    }
}

struct ScrollAnimation {
    owner: Weak<()>,
    target: Point<Pixels>,
    last: Point<Pixels>,
    sampled_at: Instant,
    apply: Box<ApplyScroll>,
}

impl SmoothScroll {
    /// Adds a wheel delta to any pending movement in the same direction.
    /// Reversing direction starts at the currently visible offset.
    pub fn target(
        &self,
        current: Point<Pixels>,
        delta: Point<Pixels>,
        window: &Window,
    ) -> Point<Pixels> {
        let Some(animation) = window
            .smooth_scroll
            .active
            .get(&(Rc::as_ptr(&self.0) as usize))
            .filter(|animation| animation.last == current)
        else {
            return current + delta;
        };
        let add = |current: Pixels, target: Pixels, delta: Pixels| {
            let remaining = (target - current).as_f32();
            if remaining * delta.as_f32() >= 0. {
                target + delta
            } else {
                current + delta
            }
        };
        point(
            add(current.x, animation.target.x, delta.x),
            add(current.y, animation.target.y, delta.y),
        )
    }

    /// Moves from the current offset to a clamped wheel target. Each frame
    /// receives the last applied offset and the next one. Return the actual
    /// offset after applying it, or `None` if the viewport was moved elsewhere
    /// or removed.
    pub fn animate(
        &self,
        current: Point<Pixels>,
        target: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
        mut apply: impl FnMut(
            Point<Pixels>,
            Point<Pixels>,
            &mut Window,
            &mut App,
        ) -> Option<Point<Pixels>>
        + 'static,
    ) -> bool {
        if !target.x.as_f32().is_finite() || !target.y.as_f32().is_finite() {
            return false;
        }
        let id = Rc::as_ptr(&self.0) as usize;
        if !cx.smooth_scrolling() {
            window.smooth_scroll.active.remove(&id);
            return current != target && apply(current, target, window, cx).is_some();
        }
        if current == target {
            window.smooth_scroll.active.remove(&id);
            return false;
        }
        if let Some(active) = window.smooth_scroll.active.get_mut(&id)
            && active.last == current
        {
            // Keep the frame clock across wheel packets so a burst can extend
            // the target without restarting interpolation between presents.
            active.target = target;
            active.apply = Box::new(apply);
            return true;
        }
        window.smooth_scroll.active.insert(
            id,
            ScrollAnimation {
                owner: Rc::downgrade(&self.0),
                target,
                last: current,
                sampled_at: cx.background_executor().now(),
                apply: Box::new(apply),
            },
        );
        schedule_frame(window);
        true
    }
}

fn schedule_frame(window: &mut Window) {
    if !window.smooth_scroll.frame_pending && !window.smooth_scroll.active.is_empty() {
        window.smooth_scroll.frame_pending = true;
        window.on_next_frame(tick);
    }
}

fn tick(window: &mut Window, cx: &mut App) {
    window.smooth_scroll.frame_pending = false;
    let generation = window.smooth_scroll.generation;
    let now = cx.background_executor().now();
    for (id, mut animation) in std::mem::take(&mut window.smooth_scroll.active) {
        if window.smooth_scroll.generation != generation {
            break;
        }
        if animation.owner.strong_count() == 0 || window.smooth_scroll.active.contains_key(&id) {
            continue;
        }
        let elapsed = now
            .saturating_duration_since(animation.sampled_at)
            .min(MAX_FRAME_STEP);
        animation.sampled_at = now;
        let remaining = animation.target - animation.last;
        let finished = remaining.x.as_f32().abs().max(remaining.y.as_f32().abs())
            <= SETTLE_DISTANCE
            || !cx.smooth_scrolling();
        let next = if finished {
            animation.target
        } else {
            let weight = 1. - (-elapsed.as_secs_f32() / HALF_LIFE.as_secs_f32()).exp2();
            animation.last + remaining * weight
        };
        if let Some(actual) = (animation.apply)(animation.last, next, window, cx)
            && !finished
            && actual != animation.target
            && (actual != animation.last || next == animation.last)
            && window.smooth_scroll.generation == generation
        {
            animation.last = actual;
            window.smooth_scroll.active.entry(id).or_insert(animation);
        }
    }
    schedule_frame(window);
}
