use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    time::Duration,
};

use collections::FxHashMap;
use scheduler::Instant;

use crate::{App, Pixels, Point, Window, point, px};

const HALF_LIFE: Duration = Duration::from_millis(24);
// A stalled UI frame should not spend the entire scroll distance at once.
const MAX_FRAME_STEP: Duration = Duration::from_millis(25);
const SETTLE_DISTANCE: f32 = 0.5;
const NAVIGATION_DURATION: Duration = Duration::from_millis(140);

type ApplyScroll =
    dyn FnMut(Point<Pixels>, Point<Pixels>, &mut Window, &mut App) -> Option<Point<Pixels>>;

/// Identifies a scroll viewport across frames of wheel scrolling or navigation.
#[derive(Clone, Debug, Default)]
pub struct SmoothScroll(Rc<ScrollOwner>);

#[derive(Debug, Default)]
struct ScrollOwner {
    navigation: RefCell<Weak<Cell<bool>>>,
}

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

    pub(crate) fn cancel_wheel(&mut self) {
        self.active
            .retain(|_, animation| animation.navigation.is_some());
    }

    pub(crate) fn cancel_navigation(&mut self) {
        self.active
            .retain(|_, animation| animation.navigation.is_none());
    }
}

struct ScrollAnimation {
    owner: Weak<ScrollOwner>,
    target: Point<Pixels>,
    last: Point<Pixels>,
    sampled_at: Instant,
    navigation: Option<Navigation>,
    apply: Box<ApplyScroll>,
}

struct Navigation {
    running: Rc<Cell<bool>>,
    start: Point<Pixels>,
    elapsed: Duration,
}

impl SmoothScroll {
    /// Whether this viewport has an in-progress navigation scroll.
    pub fn is_navigating(&self) -> bool {
        self.0
            .navigation
            .borrow()
            .upgrade()
            .is_some_and(|running| running.get())
    }

    /// Yield control of the viewport to another operation.
    pub fn cancel_navigation(&self) {
        if let Some(running) = std::mem::take(&mut *self.0.navigation.borrow_mut()).upgrade() {
            running.set(false);
        }
    }

    /// Scroll to a resolved location using the shared navigation animation.
    /// For a distant destination, apply the start of the arrival segment in
    /// the current update, before the next frame can be painted.
    pub fn navigate(
        &self,
        current: Point<Pixels>,
        target: Point<Pixels>,
        viewport_height: Pixels,
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
        self.cancel_navigation();
        let id = Rc::as_ptr(&self.0) as usize;
        window.smooth_scroll.active.remove(&id);
        if current == target {
            return false;
        }
        if !cx.smooth_scrolling() || cx.reduce_motion() || viewport_height <= px(0.) {
            apply(current, target, window, cx);
            return false;
        }

        let distance = target.y - current.y;
        let start = if distance.abs() > viewport_height * 2.5 {
            let y = target.y - viewport_height * (0.75 * distance.as_f32().signum());
            let Some(start) = apply(current, point(current.x, y), window, cx) else {
                return false;
            };
            start
        } else {
            current
        };
        let running = Rc::new(Cell::new(true));
        *self.0.navigation.borrow_mut() = Rc::downgrade(&running);
        window.smooth_scroll.active.insert(
            id,
            ScrollAnimation {
                owner: Rc::downgrade(&self.0),
                target,
                last: start,
                sampled_at: cx.background_executor().now(),
                navigation: Some(Navigation {
                    running,
                    start,
                    elapsed: Duration::ZERO,
                }),
                apply: Box::new(apply),
            },
        );
        schedule_frame(window);
        true
    }

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
            .filter(|animation| animation.navigation.is_none() && animation.last == current)
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
            && active.navigation.is_none()
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
                navigation: None,
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
        let (next, finished) = if let Some(navigation) = &mut animation.navigation {
            if !navigation.running.get() {
                continue;
            }
            navigation.elapsed += elapsed;
            let finished = navigation.elapsed >= NAVIGATION_DURATION
                || !cx.smooth_scrolling()
                || cx.reduce_motion();
            let remaining_time =
                1. - navigation.elapsed.as_secs_f32() / NAVIGATION_DURATION.as_secs_f32();
            let next = if finished {
                animation.target
            } else {
                animation.target - (animation.target - navigation.start) * remaining_time.powi(3)
            };
            (next, finished)
        } else {
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
            (next, finished)
        };
        if let Some(actual) = (animation.apply)(animation.last, next, window, cx)
            && !finished
            && actual != animation.target
            && (animation.navigation.is_some()
                || actual != animation.last
                || next == animation.last)
            && window.smooth_scroll.generation == generation
        {
            animation.last = actual;
            window.smooth_scroll.active.entry(id).or_insert(animation);
        }
    }
    schedule_frame(window);
}
