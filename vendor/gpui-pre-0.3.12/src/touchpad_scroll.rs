//! Frame-paced input shaping for touchpad scrolling.
//!
//! Wayland reports touchpad movement as precise pixel deltas. Applying every
//! `wl_pointer` packet immediately makes the visible motion follow the
//! compositor's packet cadence rather than the render frame, and it stops the
//! moment the finger lifts because the protocol has no inertia. This module
//! turns that stream into at most one scroll event per frame and adds a short
//! momentum phase after the gesture ends.

use std::{collections::VecDeque, time::Duration};

use scheduler::Instant;

use crate::{
    App, Modifiers, Pixels, PlatformInput, Point, ScrollDelta, ScrollEvent, ScrollSource,
    TouchPhase, Window, point, px,
};

/// How quickly the visible offset catches up with input that has not been
/// painted yet. Short enough to stay within one frame of the finger.
const TRACK_HALF_LIFE: Duration = Duration::from_millis(7);

/// Time scale of the exponential fling decay.
const FLING_TAU: Duration = Duration::from_millis(350);

/// Below this release speed the gesture stops without momentum.
const MIN_FLING_SPEED: f32 = 50.;

/// 限制异常输入造成的惯性速度尖峰，单位为逻辑像素每秒。
const MAX_FLING_SPEED: f32 = 3_000.;

/// A stalled UI frame must not consume the whole accumulated distance at once.
const MAX_FRAME_STEP: Duration = Duration::from_millis(25);

/// Snap sub-pixel remainders instead of scheduling frames forever.
const SETTLE_DISTANCE: f32 = 0.5;

/// The window used to estimate release velocity.
const SPEED_WINDOW: Duration = Duration::from_millis(50);

/// A finger that has not moved for this long before lifting has no speed.
const VELOCITY_STALE: Duration = Duration::from_millis(80);

/// 尚无上一帧时使用的时间步长。
const FIRST_FRAME: Duration = Duration::from_millis(8);

/// A gap this long without a new packet ends a gesture that never received
/// an `AxisStop`, so the next packet starts cleanly instead of continuing the
/// old gesture.
const GESTURE_GAP: Duration = Duration::from_millis(120);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Mode {
    #[default]
    Idle,
    Fling {
        velocity: Point<f32>,
    },
}

#[derive(Clone, Copy)]
enum InputTime {
    Platform(u32),
    Local(Instant),
}

impl InputTime {
    fn elapsed_since(self, earlier: Self) -> Option<Duration> {
        match (self, earlier) {
            (Self::Platform(now), Self::Platform(earlier)) => {
                Some(Duration::from_millis(u64::from(now.wrapping_sub(earlier))))
            }
            (Self::Local(now), Self::Local(earlier)) => {
                Some(now.saturating_duration_since(earlier))
            }
            _ => None,
        }
    }
}

#[derive(Default)]
pub(crate) struct TouchpadScroll {
    /// Raw input received but not yet handed to the scroll containers.
    pending: Point<Pixels>,
    samples: VecDeque<(InputTime, Point<Pixels>)>,
    position: Point<Pixels>,
    modifiers: Modifiers,
    mode: Mode,
    /// A finger is down and may still send movement.
    active: bool,
    /// The platform ended the gesture; wait for pending movement and momentum.
    ending: bool,
    /// Whether the first synthetic event of this gesture has been emitted.
    started: bool,
    frame_pending: bool,
    /// Set while synthetic events are being dispatched, so they are not fed
    /// back into the resampler.
    dispatching: bool,
    last_input_at: Option<InputTime>,
    last_tick_at: Option<Instant>,
    /// Consecutive momentum frames that produced no repaint, such as at the
    /// end of a scrollable range.
    idle_frames: u8,
}

impl TouchpadScroll {
    pub(crate) fn dispatching(&self) -> bool {
        self.dispatching
    }

    /// Drop the current gesture and any momentum.
    pub(crate) fn cancel(&mut self) {
        let dispatching = self.dispatching;
        let frame_pending = self.frame_pending;
        *self = Self::default();
        self.dispatching = dispatching;
        self.frame_pending = frame_pending;
    }

    pub(crate) fn queue(&mut self, event: ScrollEvent, window: &mut Window, cx: &mut App) {
        let now = cx.background_executor().now();
        self.push(event, now);
        self.schedule(window);
    }

    fn schedule(&mut self, window: &mut Window) {
        if !self.frame_pending {
            self.frame_pending = true;
            window.on_next_frame(tick);
        }
    }

    fn push(&mut self, event: ScrollEvent, now: Instant) {
        let ScrollDelta::Pixels(delta) = event.delta else {
            return;
        };
        let input_time = match event.source {
            ScrollSource::TimedTouchpad(milliseconds) => InputTime::Platform(milliseconds),
            _ => InputTime::Local(now),
        };

        if event.touch_phase == TouchPhase::Started
            || self.last_input_at.is_some_and(|last| {
                input_time
                    .elapsed_since(last)
                    .is_none_or(|gap| gap > GESTURE_GAP)
            })
        {
            // 新手势从新的目标和位移开始，避免继承上次手势未绘制的尾巴。
            self.cancel();
        }

        match event.touch_phase {
            TouchPhase::Started | TouchPhase::Moved => {
                if !self.active {
                    self.cancel();
                    self.active = true;
                    self.position = event.position;
                }
            }
            TouchPhase::Ended if self.active => {
                self.active = false;
                self.ending = true;
                let velocity = self.release_velocity(input_time);
                self.mode = if speed(velocity) >= MIN_FLING_SPEED {
                    Mode::Fling { velocity }
                } else {
                    Mode::Idle
                };
            }
            TouchPhase::Ended => return,
            TouchPhase::Cancelled => {
                self.active = false;
                self.ending = true;
                self.pending = Point::default();
                self.mode = Mode::Idle;
                return;
            }
        }

        if delta != Point::default() {
            self.pending += delta;
            self.sample(delta, input_time);
        }

        self.modifiers = event.modifiers;
        if self.last_tick_at.is_none() {
            self.last_tick_at = Some(now);
        }
        self.last_input_at = Some(input_time);
    }

    fn sample(&mut self, delta: Point<Pixels>, now: InputTime) {
        if self.samples.back().is_some_and(|(_, last)| {
            last.x.as_f32() * delta.x.as_f32() + last.y.as_f32() * delta.y.as_f32() < 0.
        }) {
            // A direction change is a new movement, not a slow-down through
            // zero speed.
            self.samples.clear();
        }
        self.samples.push_back((now, delta));
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.elapsed_since(*at).is_none_or(|gap| gap > SPEED_WINDOW))
        {
            self.samples.pop_front();
        }
    }

    fn release_velocity(&self, now: InputTime) -> Point<f32> {
        let (Some((first, _)), Some((last, _))) = (self.samples.front(), self.samples.back())
        else {
            return Point::default();
        };
        let Some(elapsed) = last
            .elapsed_since(*first)
            .filter(|elapsed| !elapsed.is_zero())
        else {
            return Point::default();
        };
        if now
            .elapsed_since(*last)
            .is_none_or(|age| age > VELOCITY_STALE)
        {
            return Point::default();
        }
        // 首包的位移发生在采样窗口开始之前，计入会放大短手势的速度。
        let sum = self
            .samples
            .iter()
            .skip(1)
            .fold(Point::<f32>::default(), |sum, (_, delta)| {
                point(sum.x + delta.x.as_f32(), sum.y + delta.y.as_f32())
            });
        let velocity = sum * elapsed.as_secs_f32().recip();
        let magnitude = speed(velocity);
        if magnitude > MAX_FLING_SPEED {
            velocity * (MAX_FLING_SPEED / magnitude)
        } else {
            velocity
        }
    }

    pub(crate) fn advance(&mut self, now: Instant) -> (Vec<ScrollEvent>, bool) {
        let dt = self
            .last_tick_at
            .replace(now)
            .map(|last| now.saturating_duration_since(last))
            .unwrap_or(FIRST_FRAME)
            .min(MAX_FRAME_STEP);
        let seconds = dt.as_secs_f32();
        let mut events = Vec::new();
        let mut movement = Point::default();

        if self.pending != Point::default() {
            let half_life = TRACK_HALF_LIFE.as_secs_f32();
            let weight = 1. - (-seconds / half_life).exp2();
            let mut step = self.pending * weight;
            if (self.pending - step).magnitude() <= f64::from(SETTLE_DISTANCE) {
                step = self.pending;
            }
            self.pending -= step;
            movement = step;
        }

        if let Mode::Fling { mut velocity } = self.mode
            && seconds > 0.
        {
            // 松手所在的帧先消化手指位移，惯性使用这一帧剩余的时间。
            let tracking_time = movement.magnitude() as f32 / speed(velocity);
            let fling_time = (seconds - tracking_time).max(0.);
            let tau = FLING_TAU.as_secs_f32();
            let decay = (-fling_time / tau).exp();
            let step = velocity * (tau * (1. - decay));
            velocity = velocity * decay;
            if speed(velocity) < MIN_FLING_SPEED {
                velocity = Point::default();
            }
            self.mode = if velocity == Point::default() {
                Mode::Idle
            } else {
                Mode::Fling { velocity }
            };
            movement += point(px(step.x), px(step.y));
        }

        if movement != Point::default() {
            self.emit(movement, &mut events);
        }

        if self.ending && self.pending == Point::default() && matches!(self.mode, Mode::Idle) {
            if self.started {
                events.push(self.event(Point::default(), TouchPhase::Ended));
            }
            self.cancel();
        }

        let more = self.pending != Point::default()
            || matches!(self.mode, Mode::Fling { .. })
            || (self.ending && self.started);
        (events, more)
    }

    fn emit(&mut self, delta: Point<Pixels>, events: &mut Vec<ScrollEvent>) {
        let phase = if self.started {
            TouchPhase::Moved
        } else {
            self.started = true;
            TouchPhase::Started
        };
        events.push(self.event(delta, phase));
    }

    fn event(&self, delta: Point<Pixels>, phase: TouchPhase) -> ScrollEvent {
        ScrollEvent {
            position: self.position,
            delta: ScrollDelta::Pixels(delta),
            modifiers: self.modifiers,
            touch_phase: phase,
            source: ScrollSource::Touchpad,
        }
    }
}

fn speed(velocity: Point<f32>) -> f32 {
    (velocity.x * velocity.x + velocity.y * velocity.y).sqrt()
}

fn tick(window: &mut Window, cx: &mut App) {
    window.touchpad_scroll.frame_pending = false;
    if cx.reduce_motion() || !cx.smooth_scrolling() {
        window.touchpad_scroll.cancel();
        return;
    }
    let now = cx.background_executor().now();
    let (events, more) = window.touchpad_scroll.advance(now);
    if !events.is_empty() {
        let before = window.invalidator.update_count();
        window.touchpad_scroll.dispatching = true;
        for event in events {
            window.dispatch_event(PlatformInput::Scroll(event), cx);
        }
        window.touchpad_scroll.dispatching = false;
        let scrolled = window.invalidator.update_count() > before;
        if scrolled {
            window.touchpad_scroll.idle_frames = 0;
        } else if matches!(window.touchpad_scroll.mode, Mode::Fling { .. }) {
            window.touchpad_scroll.idle_frames += 1;
            if window.touchpad_scroll.idle_frames >= 2 {
                window.touchpad_scroll.cancel();
                return;
            }
        }
    }
    if more && !window.touchpad_scroll.frame_pending {
        window.touchpad_scroll.frame_pending = true;
        window.on_next_frame(tick);
    }
}
