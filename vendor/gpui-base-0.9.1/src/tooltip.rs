use std::time::Instant;
use std::{rc::Rc, time::Duration};

use gpui::{
    AnyElement, AnyView, App, Bounds, Context, Div, ElementId, Global, InteractiveElement,
    IntoElement, ParentElement, Pixels, Point, Render, RenderOnce, Role, Stateful,
    StatefulInteractiveElement, Styled, Task, Window, deferred, div, prelude::FluentBuilder as _,
    px,
};

use crate::{Placement, Positioner};

const TOOLTIP_PRIORITY: usize = 200;
const WINDOW_MARGIN: Pixels = px(4.);
const ENTER_DURATION: Duration = Duration::from_millis(150);
const ENTER_OFFSET_Y: Pixels = px(4.);
const SLIDE_DURATION: Duration = Duration::from_millis(200);

type TooltipBuilder = Rc<dyn Fn(&mut Window, &mut App) -> AnyView>;
type TooltipRenderer = Rc<dyn Fn(AnyView, TooltipTransition, &mut Window, &mut App) -> AnyElement>;

/// An unstyled tooltip popup.
///
/// This corresponds to Base UI's `Tooltip.Popup`: it owns the accessible
/// tooltip role and accepts application-owned content and presentation.
#[derive(IntoElement)]
pub struct Tooltip {
    base: Stateful<Div>,
}

impl Tooltip {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            base: div().id(id).role(Role::Tooltip),
        }
    }
}

impl Styled for Tooltip {
    fn style(&mut self) -> &mut gpui::StyleRefinement {
        self.base.style()
    }
}

impl ParentElement for Tooltip {
    fn extend(&mut self, children: impl IntoIterator<Item = AnyElement>) {
        self.base.extend(children);
    }
}

impl RenderOnce for Tooltip {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.base
    }
}

/// Application-wide timing for tooltips shown through [`TooltipOverlay`].
///
/// Read on every show and hide request, so installing new defaults takes
/// effect in windows that are already open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooltipDefaults {
    show_delay: Duration,
    grace_period: Duration,
}

impl Global for TooltipDefaults {}

impl TooltipDefaults {
    /// Creates the Base defaults: a 500 ms show delay and a 300 ms grace period.
    pub fn new() -> Self {
        Self {
            show_delay: Duration::from_millis(500),
            grace_period: Duration::from_millis(300),
        }
    }

    /// Sets how long the pointer must rest on a trigger before its tooltip shows.
    pub fn with_show_delay(mut self, delay: Duration) -> Self {
        self.show_delay = delay;
        self
    }

    /// Sets how long a tooltip stays after the pointer leaves its trigger.
    ///
    /// Entering another trigger within this period switches to its tooltip
    /// without waiting for the show delay.
    pub fn with_grace_period(mut self, period: Duration) -> Self {
        self.grace_period = period;
        self
    }

    /// How long the pointer must rest on a trigger before its tooltip shows.
    pub fn show_delay(&self) -> Duration {
        self.show_delay
    }

    /// How long a tooltip stays after the pointer leaves its trigger.
    pub fn grace_period(&self) -> Duration {
        self.grace_period
    }

    /// Installs these defaults for the whole application.
    pub fn install(self, cx: &mut App) {
        cx.set_global(self);
    }

    /// Returns the installed defaults, or the Base ones when none were.
    pub fn global(cx: &App) -> Self {
        cx.try_global::<Self>().copied().unwrap_or_default()
    }
}

impl Default for TooltipDefaults {
    fn default() -> Self {
        Self::new()
    }
}

/// Content requested by a tooltip trigger.
#[derive(Clone)]
pub struct TooltipRequest {
    build: TooltipBuilder,
    trigger_bounds: Bounds<Pixels>,
    preferred_placement: Option<Placement>,
    show_delay: Option<Duration>,
    restart_delay: bool,
    positioner: Option<Rc<dyn Fn() -> Positioner>>,
    paint_only: bool,
    interactive: bool,
}

impl TooltipRequest {
    pub fn new(
        trigger_bounds: Bounds<Pixels>,
        build: impl Fn(&mut Window, &mut App) -> AnyView + 'static,
    ) -> Self {
        Self {
            build: Rc::new(build),
            trigger_bounds,
            preferred_placement: None,
            show_delay: None,
            restart_delay: false,
            positioner: None,
            paint_only: false,
            interactive: false,
        }
    }

    /// Prefers a side for the tooltip, falling back when it does not fit.
    pub fn with_placement(mut self, placement: Placement) -> Self {
        self.preferred_placement = Some(placement);
        self
    }

    /// Supplies a positioner when preferred-side placement is insufficient.
    pub fn with_positioner(mut self, positioner: impl Fn() -> Positioner + 'static) -> Self {
        self.positioner = Some(Rc::new(positioner));
        self
    }

    /// Allow moving from the trigger into the popup to interact with its contents.
    pub fn interactive(mut self) -> Self {
        self.interactive = true;
        self
    }

    /// Keeps the tooltip out of input handling for its entire lifetime.
    pub fn paint_only(mut self) -> Self {
        self.paint_only = true;
        self
    }

    #[deprecated(note = "use `with_placement`")]
    pub fn placement(self, placement: Placement) -> Self {
        self.with_placement(placement)
    }

    /// Overrides [`TooltipDefaults::show_delay`] for this trigger.
    pub fn with_show_delay(mut self, delay: Duration) -> Self {
        self.show_delay = Some(delay);
        self
    }

    /// Waits out a full show delay on every trigger entry.
    ///
    /// By default a tooltip switches immediately when another trigger is
    /// entered while a tooltip is visible or within the grace period. A
    /// restart request instead dismisses the previous tooltip and starts its
    /// own delay, so dense rows of triggers do not flash titles as the pointer
    /// crosses them.
    pub fn with_restart_delay(mut self, restart: bool) -> Self {
        self.restart_delay = restart;
        self
    }
}

/// Presentation transition requested by the Base tooltip lifecycle.
#[derive(Clone, Copy, Debug)]
pub enum TooltipTransition {
    Enter {
        epoch: usize,
    },
    Switch {
        epoch: usize,
        previous: Bounds<Pixels>,
        current: Bounds<Pixels>,
    },
}

/// Per-window tooltip provider and overlay.
///
/// Show requests are ignored on iOS and Android, where touch input must not
/// open hover tooltips. This does not control GPUI's native `.tooltip()` API.
pub struct TooltipOverlay {
    enabled: bool,
    content: Option<TooltipRequest>,
    motion: Option<crate::PopupMotion>,
    origin_motion: crate::motion::TransitionState<Point<Pixels>>,
    leaving: bool,
    previous_bounds: Option<Bounds<Pixels>>,
    epoch: usize,
    had_recent_tooltip: bool,
    animation_epoch: usize,
    is_switching: bool,
    show_task: Option<Task<()>>,
    pending: Option<TooltipRequest>,
    hide_task: Option<Task<()>>,
    popup_hovered: bool,
    renderer: TooltipRenderer,
}

impl TooltipOverlay {
    pub fn new() -> Self {
        Self {
            enabled: !crate::is_mobile(),
            content: None,
            motion: None,
            origin_motion: Default::default(),
            leaving: false,
            previous_bounds: None,
            epoch: 0,
            had_recent_tooltip: false,
            animation_epoch: 0,
            is_switching: false,
            show_task: None,
            pending: None,
            hide_task: None,
            popup_hovered: false,
            renderer: Rc::new(|view, _, _, _| div().child(view).into_any_element()),
        }
    }

    pub fn render_with(
        mut self,
        renderer: impl Fn(AnyView, TooltipTransition, &mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        self.renderer = Rc::new(renderer);
        self
    }

    fn next_epoch(&mut self) -> usize {
        self.epoch += 1;
        self.epoch
    }

    pub fn request_show(
        &mut self,
        content: TooltipRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Gate both delayed display and the immediate grace-period switch.
        // Keep this in Base so every managed component shares the policy.
        if !self.enabled {
            return;
        }
        self.hide_task = None;
        self.popup_hovered = false;
        let restart_delay = content.restart_delay;
        let show_delay = content
            .show_delay
            .unwrap_or_else(|| TooltipDefaults::global(cx).show_delay);
        // An outgoing tooltip is still paintable but no longer eligible for a handoff.
        let was_visible = self.content.is_some() && !self.leaving;
        if (!restart_delay && (was_visible || self.had_recent_tooltip)) || show_delay.is_zero() {
            self.previous_bounds = self.content.as_ref().map(|content| content.trigger_bounds);
            self.content = Some(content);
            self.open_motion(was_visible);
            self.show_task = None;
            self.pending = None;
            self.is_switching = was_visible;
            self.animation_epoch += 1;
            cx.notify();
            return;
        }

        if restart_delay {
            // Each entry pays the full delay. Fade the previous tooltip out
            // now rather than leaving it up until the new request resolves.
            self.hide(cx);
        }
        self.had_recent_tooltip = false;

        let epoch = self.next_epoch();
        self.pending = Some(content);
        self.show_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(show_delay).await;
            let _ = this.update_in(cx, |this, _, cx| {
                if this.epoch == epoch {
                    this.content = this.pending.take();
                    this.open_motion(false);
                    this.previous_bounds = None;
                    this.is_switching = false;
                    this.animation_epoch += 1;
                    cx.notify();
                }
            });
        }));
    }

    /// Updates the current trigger's anchor without restarting its delay or motion.
    pub fn refresh_trigger(&mut self, request: TooltipRequest, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            self.pending = Some(request);
        } else if self.content.is_some() && !self.leaving {
            self.content = Some(request);
            cx.notify();
        }
    }

    /// Restart the delay only while the pointer is waiting to open this tooltip.
    pub fn restart_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(request) = self.pending.take() {
            self.request_show(request, window, cx);
        }
    }

    /// Keep the current interactive popup open on return to its trigger.
    pub fn resume_interactive(&mut self) -> bool {
        if !self.leaving && self.content.as_ref().is_some_and(|content| content.interactive) {
            self.hide_task = None;
            true
        } else {
            false
        }
    }

    pub fn request_hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_task = None;
        self.pending = None;
        if self.content.is_none() || self.popup_hovered {
            return;
        }
        let epoch = self.next_epoch();
        let grace_period = TooltipDefaults::global(cx).grace_period;
        self.had_recent_tooltip = true;
        self.hide_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(grace_period).await;
            let _ = this.update_in(cx, |this, _, cx| {
                if this.epoch == epoch {
                    this.hide(cx);
                }
            });
        }));
    }

    fn open_motion(&mut self, handoff: bool) {
        if !handoff {
            self.origin_motion = Default::default();
        }
        self.leaving = false;
        let now = Instant::now();
        self.motion
            .get_or_insert_with(|| crate::PopupMotion::new(crate::PopupMotionKind::Hover, now))
            .retarget(true, now);
    }

    pub fn hide(&mut self, cx: &mut Context<Self>) {
        self.had_recent_tooltip = false;
        self.popup_hovered = false;
        self.show_task = None;
        self.pending = None;
        self.hide_task = None;
        if self.content.is_some() && !self.leaving {
            self.leaving = true;
            if let Some(motion) = self.motion.as_mut() {
                motion.retarget(false, Instant::now());
            }
            cx.notify();
        }
    }
}

impl Default for TooltipOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for TooltipOverlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let opacity = self
            .motion
            .as_ref()
            .map_or(1., |motion| motion.opacity(window, cx));
        if self.leaving && opacity <= 0. {
            self.content = None;
            self.motion = None;
            self.previous_bounds = None;
            self.is_switching = false;
        }
        let Some(content) = self.content.as_ref() else {
            return div().into_any_element();
        };
        let view = (content.build)(window, cx);
        let transition = match (self.is_switching, self.previous_bounds) {
            (true, Some(previous)) => TooltipTransition::Switch {
                epoch: self.animation_epoch,
                previous,
                current: content.trigger_bounds,
            },
            _ => TooltipTransition::Enter {
                epoch: self.animation_epoch,
            },
        };
        let rendered = (self.renderer)(view, transition, window, cx);
        let positioner = content.positioner.as_ref().map_or_else(
            || {
                TooltipPositioner::new(content.trigger_bounds)
                    .when_some(content.preferred_placement, |this, placement| {
                        this.placement(placement)
                    })
                    .into_element()
            },
            |positioner| positioner(),
        );
        // Sample entrance and handoff through the same positioned origin, so
        // a switch can retarget an entrance that is still translating.
        use crate::{animation, motion::Transition};
        let (policy, enter_offset_y) = match transition {
            TooltipTransition::Enter { .. } => (
                Transition::new(ENTER_DURATION).ease(animation::ease_out_cubic),
                ENTER_OFFSET_Y,
            ),
            TooltipTransition::Switch {
                previous, current, ..
            } if (current.top() - previous.top()).abs() < px(10.)
                || (current.left() - previous.left()).abs() < px(10.) =>
            {
                (
                    Transition::new(SLIDE_DURATION).ease(animation::ease_in_out_cubic),
                    px(0.),
                )
            }
            _ => (Transition::new(Duration::ZERO), px(0.)),
        };
        let anchor = content.trigger_bounds;
        let interactive = content.interactive && !self.leaving;
        let popup = div().opacity(opacity).child(rendered);
        let popup = if interactive {
            let overlay = cx.weak_entity();
            let epoch = self.animation_epoch;
            popup
                .id(("interactive-tooltip", epoch))
                .on_hover(move |hovered, window, cx| {
                    _ = overlay.update(cx, |overlay, cx| {
                        if overlay.animation_epoch != epoch || overlay.leaving {
                            return;
                        }
                        overlay.popup_hovered = *hovered;
                        if *hovered {
                            overlay.hide_task = None;
                        } else if !anchor.contains(&window.mouse_position()) {
                            overlay.request_hide(window, cx);
                        }
                    });
                })
                .into_any_element()
        } else {
            popup.into_any_element()
        };
        deferred(
            positioner
                .transition_origin(self.origin_motion.clone(), policy, enter_offset_y)
                .child(crate::PopupFrame::new(
                    popup,
                    self.leaving || content.paint_only,
                )),
        )
        .with_priority(TOOLTIP_PRIORITY)
        .into_any_element()
    }
}

/// An unstyled tooltip positioner with viewport-aware flipping and clamping.
///
/// This is a tooltip-named view of [`crate::Positioner`]'s side placement. It
/// adds no element of its own; the shared positioner is what gets rendered.
pub struct TooltipPositioner(Positioner);

impl TooltipPositioner {
    pub fn new(trigger_bounds: Bounds<Pixels>) -> Self {
        Self(Positioner::side(trigger_bounds).margin(WINDOW_MARGIN))
    }

    pub fn placement(mut self, placement: Placement) -> Self {
        self.0 = self.0.placement(placement);
        self
    }
}

impl ParentElement for TooltipPositioner {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.0.extend(elements);
    }
}

impl IntoElement for TooltipPositioner {
    type Element = Positioner;

    fn into_element(self) -> Self::Element {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, point, size};

    fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
    }

    #[gpui::test]
    fn provider_owns_grace_switch_and_dismiss(cx: &mut gpui::TestAppContext) {
        let state = cx.update(|cx| cx.new(|_| TooltipOverlay::new()));
        let cx = cx.add_empty_window();
        cx.update(|window, cx| {
            state.update(cx, |tooltip, cx| {
                tooltip.had_recent_tooltip = true;
                tooltip.request_show(
                    TooltipRequest::new(bounds(0., 0., 20., 20.), |_, _| {
                        panic!("content is not rendered by this lifecycle test")
                    }),
                    window,
                    cx,
                );
            });
        });
        cx.update(|_, cx| assert!(state.read(cx).content.is_some()));

        cx.update(|_, cx| {
            state.update(cx, |tooltip, cx| tooltip.hide(cx));
        });
        cx.update(|_, cx| assert!(state.read(cx).content.is_none()));
    }

    #[gpui::test]
    fn show_delay_follows_defaults_and_request_override(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            TooltipDefaults::new()
                .with_show_delay(Duration::from_millis(100))
                .install(cx)
        });
        // The delayed show updates the overlay in the window it was drawn in.
        let (state, cx) = cx.add_window_view(|_, _| TooltipOverlay::new());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let request = || {
            TooltipRequest::new(bounds(0., 0., 20., 20.), |_, cx| {
                cx.new(|_| gpui::Empty).into()
            })
        };

        cx.update(|window, cx| {
            state.update(cx, |tooltip, cx| {
                tooltip.request_show(request(), window, cx)
            });
        });
        cx.executor().advance_clock(Duration::from_millis(99));
        cx.run_until_parked();
        cx.update(|_, cx| assert!(state.read(cx).content.is_none()));
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        cx.update(|_, cx| assert!(state.read(cx).content.is_some()));

        cx.update(|_, cx| state.update(cx, |tooltip, cx| tooltip.hide(cx)));
        cx.update(|window, cx| {
            state.update(cx, |tooltip, cx| {
                tooltip.request_show(request().with_show_delay(Duration::ZERO), window, cx);
                assert!(tooltip.content.is_some());
                assert!(tooltip.show_task.is_none());
            });
        });
    }

    #[test]
    fn tooltip_priority_exceeds_popup_layer() {
        assert!(TOOLTIP_PRIORITY > crate::POPUP_PRIORITY);
    }

    #[gpui::test]
    fn restart_requests_pay_their_own_delay_instead_of_switching(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            TooltipDefaults::new()
                .with_show_delay(Duration::from_millis(100))
                .install(cx)
        });
        let (state, cx) = cx.add_window_view(|_, _| TooltipOverlay::new());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let request = |x: f32, restart: bool| {
            TooltipRequest::new(bounds(x, 0., 20., 20.), |_, cx| {
                cx.new(|_| gpui::Empty).into()
            })
            .with_restart_delay(restart)
        };

        // Show the first trigger at once so the overlay has visible content.
        cx.update(|window, cx| {
            state.update(cx, |tooltip, cx| {
                tooltip.request_show(
                    request(0., true).with_show_delay(Duration::ZERO),
                    window,
                    cx,
                )
            });
        });
        cx.update(|_, cx| assert!(state.read(cx).content.is_some()));

        // A restart request from another trigger must not reuse the visible
        // tooltip; it starts fading the old one out and waits its own delay
        // before taking its place.
        cx.update(|window, cx| {
            state.update(cx, |tooltip, cx| {
                tooltip.request_show(request(100., true), window, cx)
            });
        });
        cx.executor().advance_clock(Duration::from_millis(99));
        cx.run_until_parked();
        cx.update(|_, cx| {
            let tooltip = state.read(cx);
            let content = tooltip
                .content
                .as_ref()
                .expect("the old tooltip is still fading");
            assert_eq!(content.trigger_bounds, bounds(0., 0., 20., 20.));
        });

        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        cx.update(|_, cx| {
            let tooltip = state.read(cx);
            let content = tooltip.content.as_ref().expect("the new tooltip shows");
            assert_eq!(content.trigger_bounds, bounds(100., 0., 20., 20.));
        });

        // Requests that do not ask for a restart keep the immediate switch.
        cx.update(|window, cx| {
            state.update(cx, |tooltip, cx| {
                tooltip.request_show(request(200., false), window, cx)
            });
        });
        cx.update(|_, cx| {
            let tooltip = state.read(cx);
            let content = tooltip.content.as_ref().expect("the switch is immediate");
            assert_eq!(content.trigger_bounds, bounds(200., 0., 20., 20.));
        });
    }

    #[gpui::test]
    fn disabled_provider_ignores_delayed_and_immediate_requests(cx: &mut gpui::TestAppContext) {
        let state = cx.update(|cx| {
            cx.new(|_| TooltipOverlay {
                enabled: false,
                ..TooltipOverlay::new()
            })
        });
        let cx = cx.add_empty_window();
        for had_recent_tooltip in [false, true] {
            cx.update(|window, cx| {
                state.update(cx, |tooltip, cx| {
                    tooltip.had_recent_tooltip = had_recent_tooltip;
                    tooltip.request_show(
                        TooltipRequest::new(bounds(0., 0., 20., 20.), |_, _| {
                            panic!("disabled tooltips must not build content")
                        }),
                        window,
                        cx,
                    );
                    assert!(tooltip.content.is_none());
                    assert!(tooltip.show_task.is_none());
                    assert!(tooltip.hide_task.is_none());
                    assert_eq!(tooltip.animation_epoch, 0);
                });
            });
        }
    }
}
