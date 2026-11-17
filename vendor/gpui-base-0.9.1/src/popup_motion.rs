use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window, px,
};

/// An application modal sits above the normal popup layers.
pub const MODAL_POPUP_PRIORITY: usize = 300;

/// Travel from the trigger side to the resting position.
pub const DROPDOWN_TRAVEL: Pixels = px(4.);

/// Per-kind defaults for the common popup transition. The material is shared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PopupMotionKind {
    #[default]
    Popover,
    Menu,
    Hover,
    Dropdown,
}

impl PopupMotionKind {
    /// Default time for a full entrance or exit.
    pub fn duration(self, opening: bool) -> Duration {
        Duration::from_millis(match (self, opening) {
            (Self::Popover, true) => 150,
            (Self::Popover, false) => 110,
            (Self::Menu, true) => 100,
            (Self::Menu, false) => 75,
            (Self::Dropdown, _) => 80,
            (Self::Hover, true) => 120,
            (Self::Hover, false) => 90,
        })
    }
}

/// An interruptible fade. A closing popup remains paintable but is inert.
pub struct PopupMotion {
    kind: PopupMotionKind,
    target: bool,
    start: Instant,
    from: f32,
}

impl PopupMotion {
    /// Start an entrance from zero opacity.
    pub fn new(kind: PopupMotionKind, now: Instant) -> Self {
        Self {
            kind,
            target: true,
            start: now,
            from: 0.,
        }
    }

    /// Reverse from the current sample when the target changes.
    pub fn retarget(&mut self, open: bool, now: Instant) {
        if self.target != open {
            self.from = self.sample(now, false).0;
            self.start = now;
            self.target = open;
        }
    }

    /// Opacity and whether another frame is needed.
    pub fn sample(&self, now: Instant, reduced_motion: bool) -> (f32, bool) {
        let target = f32::from(self.target);
        let distance = (target - self.from).abs();
        let duration = self.kind.duration(self.target).mul_f32(distance);
        let elapsed = now.saturating_duration_since(self.start);
        if reduced_motion || elapsed >= duration {
            return (target, false);
        }
        let t = elapsed.as_secs_f32() / duration.as_secs_f32();
        let eased = if self.kind == PopupMotionKind::Dropdown {
            if self.target {
                crate::animation::cubic_bezier(0.215, 0.61, 0.355, 1.)(t)
            } else {
                crate::animation::cubic_bezier(0.55, 0.085, 0.68, 0.53)(t)
            }
        } else if self.target {
            1. - (1. - t).powi(3)
        } else {
            t * t
        };
        (self.from + (target - self.from) * eased, true)
    }

    /// Sample opacity and request another frame only while moving.
    pub fn opacity(&self, window: &Window, cx: &App) -> f32 {
        let (opacity, running) = self.sample(Instant::now(), cx.reduce_motion());
        if running {
            window.request_animation_frame();
        }
        opacity
    }
}

/// Keep an outgoing popup's drawing while removing its interactive subtree.
pub struct PopupFrame {
    child: AnyElement,
    closing: bool,
}

impl PopupFrame {
    pub fn new(child: impl IntoElement, closing: bool) -> Self {
        Self {
            child: child.into_any_element(),
            closing,
        }
    }
}

impl IntoElement for PopupFrame {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for PopupFrame {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.closing {
            window.with_paint_only(|window| self.child.prepaint(window, cx));
        } else {
            self.child.prepaint(window, cx);
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.closing {
            window.with_paint_only(|window| self.child.paint(window, cx));
        } else {
            self.child.paint(window, cx);
        }
    }
}
