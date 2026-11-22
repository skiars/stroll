use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use instant::Instant;

use gpui::{
    Along as _, App, Axis, BorderStyle, Bounds, ContentMask, Edges, Element, ElementId,
    GlobalElementId, Hitbox, Hsla, InteractiveElement as _, IntoElement, IsZero as _, LayoutId,
    OngoingScroll, PaintQuad, ParentElement as _, Point, Position, ScrollEvent, ScrollHandle,
    SmoothScroll, StatefulInteractiveElement as _, Style, StyleRefinement, Styled, TouchPhase,
    Window, div, px, relative,
};
use gpui::{Corners, Pixels};

use crate::{AxisExt, OngoingScrollExt as _, ScrollbarHandle, StyledExt as _};

thread_local! {
    static LAST_HORIZONTAL_SCROLL_CLAIM: Cell<Option<HorizontalScrollClaim>> =
        const { Cell::new(None) };
}

/// Offsets closer than this count as unchanged, so a sub-pixel residue left at
/// an edge does not read as room to scroll.
const CLAIM_EPSILON: Pixels = px(0.5);

fn record_horizontal_scroll(delta_x: Pixels) {
    if delta_x.abs() >= CLAIM_EPSILON {
        LAST_HORIZONTAL_SCROLL_CLAIM.set(Some(HorizontalScrollClaim {
            at: Instant::now(),
            delta_x,
        }));
    }
}

/// A scroll step that a horizontal [`ScrollableMask`] used to move its content.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HorizontalScrollClaim {
    /// When the step was handled.
    at: Instant,
    /// How far the content moved: positive moves it right (reveals what is on
    /// the left), negative moves it left.
    delta_x: Pixels,
}

impl HorizontalScrollClaim {
    /// When the step was handled.
    pub fn at(&self) -> Instant {
        self.at
    }

    /// How far the content moved: positive moves it right, negative moves it left.
    pub fn delta_x(&self) -> Pixels {
        self.delta_x
    }
}

/// The last scroll step a horizontal [`ScrollableMask`] used to move its
/// content, i.e. one that changed the clamped offset rather than pushing
/// against an edge.
///
/// Hosts that put their own pan gesture over GPUI content (e.g. a swipe that
/// opens a side drawer on mobile) use it to let a horizontal strip such as a
/// markdown table scroll first: while the strip still has room in the swipe's
/// direction it claims the swipe; once it reaches that edge the claims stop and
/// the host may take over. Read on the thread that dispatches input.
pub fn last_horizontal_scroll_claim() -> Option<HorizontalScrollClaim> {
    LAST_HORIZONTAL_SCROLL_CLAIM.get()
}

/// Default element id for a mask: the call site, so two masks built in
/// different places never share their per-gesture axis lock.
#[inline]
#[track_caller]
fn caller_id() -> ElementId {
    ElementId::CodeLocation(*std::panic::Location::caller())
}

/// A horizontal document viewport with an overlay scrollbar.
///
/// GPUI's native `overflow_x_scroll` maps vertical wheel input onto horizontal
/// scrolling when there is no vertical overflow. This wrapper keeps the visual
/// clipping and scroll offset, while delegating wheel input to [`ScrollableMask`]
/// so vertical wheel events can continue bubbling to the parent scroller.
pub(crate) fn horizontal_scroll_area(
    id: impl Into<ElementId>,
    scroll_handle: &ScrollHandle,
    style: &StyleRefinement,
    child: impl IntoElement + Styled,
) -> impl IntoElement {
    let id = id.into();

    // The mask must be a sibling of the scrolled element (like in Table), not
    // a child of it: children are prepainted with the scroll offset applied,
    // which would slide the mask away from the viewport as the content
    // scrolls, leaving the uncovered part to the parent scroller.
    //
    // The frame cover is a sibling for the same reason: it keeps the rounded
    // corners clean at every scroll offset (see [`RoundedFrameCover`]).
    let frame = RoundedFrameCover {
        corner_radii: Corners {
            top_left: style.corner_radii.top_left,
            top_right: style.corner_radii.top_right,
            bottom_left: style.corner_radii.bottom_left,
            bottom_right: style.corner_radii.bottom_right,
        },
        backdrop: None,
        frame_border: px(0.),
    };

    div()
        .w_full()
        .relative()
        .child(
            div()
                .id(id.clone())
                .w_full()
                .refine_style(style)
                .overflow_hidden()
                .flex()
                .track_scroll(scroll_handle)
                .child(child.flex_none()),
        )
        .child(ScrollableMask::horizontal_content(scroll_handle).id(id.clone()))
        .child(frame)
        .child(
            div().absolute().inset_0().child(
                crate::Scrollbar::horizontal(scroll_handle)
                    .id((id, "scrollbar"))
                    .mode(crate::ScrollbarMode::Always),
            ),
        )
}

/// Keeps a rounded scroll viewport's corners clean at every scroll offset.
///
/// GPUI clips children with a rectangular content mask, so content scrolled
/// under a rounded frame paints square into the corner notches — the regions
/// inside the frame's rect corners but outside its border arcs. Rounding the
/// content itself (e.g. a table's first row) only helps while its own corners
/// sit at the viewport corners; once the content scrolls, the clipped edge
/// meets the notch square again. The cover fills each notch with the backdrop
/// color, which reads as a rounded clip. The frame's own border needs no help:
/// gpui paints an element's border after its children, so it already draws
/// over both the scrolled content and these covers.
///
/// Like [`ScrollableMask`], it must be a sibling of the scrolled element so it
/// does not move with the scroll offset.
pub struct RoundedFrameCover {
    corner_radii: Corners<Option<gpui::AbsoluteLength>>,
    backdrop: Option<Hsla>,
    frame_border: Pixels,
}

impl RoundedFrameCover {
    /// A frame with the same radius on all four corners.
    pub fn uniform(radius: Pixels) -> Self {
        let radius = Some(radius.into());
        Self {
            corner_radii: Corners {
                top_left: radius,
                top_right: radius,
                bottom_left: radius,
                bottom_right: radius,
            },
            backdrop: None,
            frame_border: px(0.),
        }
    }

    /// The frame's border width, when the cover is a child of the bordered
    /// element itself: the cover's bounds are then the content box, so the
    /// painted notches are grown outward by this width to reach the border
    /// box. The border itself needs no repaint — gpui paints an element's
    /// border after its children, so the frame's own border already draws
    /// over the covers.
    pub fn frame_border(mut self, width: Pixels) -> Self {
        self.frame_border = width;
        self
    }

    /// The color behind the frame, used to fill the corner notches.
    /// Defaults to the theme background.
    pub fn backdrop(mut self, color: Hsla) -> Self {
        self.backdrop = Some(color);
        self
    }
}

impl IntoElement for RoundedFrameCover {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RoundedFrameCover {
    type RequestLayoutState = ();
    type PrepaintState = Bounds<Pixels>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Same layout trick as `ScrollableMask`: absolute and parent-sized.
        let mut style = Style::default();
        style.position = Position::Absolute;
        style.flex_grow = 1.0;
        style.flex_shrink = 1.0;
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();

        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
        // Shift up by one height to cover the viewport, like `ScrollableMask`.
        Bounds {
            origin: Point {
                x: bounds.origin.x,
                y: bounds.origin.y - bounds.size.height,
            },
            size: bounds.size,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        cover_bounds: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        use crate::theme::ActiveTheme as _;

        let mut bounds = *cover_bounds;
        if !self.frame_border.is_zero() {
            bounds.origin.x -= self.frame_border;
            bounds.origin.y -= self.frame_border;
            bounds.size.width += self.frame_border * 2.;
            bounds.size.height += self.frame_border * 2.;
        }
        let rem_size = window.rem_size();
        let resolve = |corner: Option<gpui::AbsoluteLength>| {
            corner
                .map(|radius| radius.to_pixels(rem_size))
                .filter(|radius| !radius.is_zero())
        };
        let radii = Corners {
            top_left: resolve(self.corner_radii.top_left),
            top_right: resolve(self.corner_radii.top_right),
            bottom_left: resolve(self.corner_radii.bottom_left),
            bottom_right: resolve(self.corner_radii.bottom_right),
        };
        if radii.top_left.is_none()
            && radii.top_right.is_none()
            && radii.bottom_left.is_none()
            && radii.bottom_right.is_none()
        {
            return;
        }

        let backdrop = self.backdrop.unwrap_or(cx.theme().tokens.colors.background);
        let corners = [
            (bounds.origin, 1., 1., radii.top_left),
            (bounds.top_right(), -1., 1., radii.top_right),
            (bounds.bottom_right(), -1., -1., radii.bottom_right),
            (
                Point {
                    x: bounds.origin.x,
                    y: bounds.bottom_right().y,
                },
                1.,
                -1.,
                radii.bottom_left,
            ),
        ];
        for (corner, x_dir, y_dir, radius) in corners {
            let Some(radius) = radius else { continue };
            if let Some(path) = corner_notch_path(corner, x_dir, y_dir, radius) {
                window.paint_path(path, backdrop);
            }
        }
    }
}

/// The notch at one frame corner: the square of side `radius` anchored at
/// `corner`, minus the quarter disc of the border arc. `x_dir`/`y_dir` point
/// from the corner toward the frame's inside (`±1`).
fn corner_notch_path(
    corner: Point<Pixels>,
    x_dir: f32,
    y_dir: f32,
    radius: Pixels,
) -> Option<gpui::Path<Pixels>> {
    let mut builder = gpui::PathBuilder::fill();
    builder.move_to(corner);
    builder.line_to(Point {
        x: corner.x + radius * x_dir,
        y: corner.y,
    });
    builder.arc_to(
        Point {
            x: radius,
            y: radius,
        },
        px(0.),
        false,
        // The quarter arc nearest the corner: counterclockwise when the two
        // inward directions agree in sign, clockwise otherwise.
        x_dir * y_dir < 0.,
        Point {
            x: corner.x,
            y: corner.y + radius * y_dir,
        },
    );
    builder.close();
    builder.build().ok()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WheelHandling {
    AlongAxis,
    MapVerticalToHorizontal,
    HorizontalContent,
}

#[derive(Default)]
struct ContentScrollGesture {
    axis: Option<Axis>,
    has_start: bool,
    last_packet: Option<Instant>,
}

impl ContentScrollGesture {
    fn axis(&mut self, delta: Point<Pixels>, phase: TouchPhase, now: Instant) -> Option<Axis> {
        // Some platform events only report Moved. In that case a pause
        // separates gestures. Explicit phases keep the same axis through pauses.
        if phase == TouchPhase::Started
            || !self.has_start
                && self
                    .last_packet
                    .is_some_and(|last| now - last >= Duration::from_millis(100))
        {
            *self = Self {
                has_start: phase == TouchPhase::Started,
                ..Self::default()
            };
        }
        if self.axis.is_none() && !delta.is_zero() {
            self.axis = Some(if delta.x.abs() > delta.y.abs() {
                Axis::Horizontal
            } else {
                Axis::Vertical
            });
        }
        let axis = self.axis;
        self.last_packet = Some(now);
        if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            *self = Self::default();
        }
        axis
    }
}

/// Make a scrollable mask element to cover the parent view with the mouse wheel event listening.
///
/// When the mouse wheel is scrolled, will move the `scroll_handle` scrolling with the `axis` direction.
/// You can use this `scroll_handle` to control what you want to scroll.
/// This is only can handle once axis scrolling.
///
/// Axis-dominant wheel events are consumed in the capture phase, so the mask
/// wins over ancestor scrollers (e.g. `gpui::list`) that register their
/// listeners after their children; events dominated by the other axis keep
/// propagating. The mask stays inert while occluded.
///
/// Dominance is decided per gesture, not per event: a precise (trackpad) delta
/// locks onto the axis its gesture started on, so mid-swipe wobble cannot flip
/// which mask consumes the events. Line deltas keep the per-event comparison.
///
/// At the scroll edge the two axes differ, matching platform scrollers:
/// a vertical mask hands the event over to the ancestor scroller (CSS
/// `overscroll-behavior: auto` chaining), while a horizontal mask keeps
/// consuming it — a bubbled horizontal delta would get mapped onto a
/// vertical-only ancestor by gpui's own wheel listener (see #2468).
///
/// A mouse wheel has no horizontal axis, so a horizontal mask leaves it to the
/// parent. Over a native horizontal scroller, [`Self::vertical_wheel`] scrolls
/// sideways with it instead.
pub struct ScrollableMask<H: ScrollbarHandle + Clone = ScrollHandle> {
    axis: Axis,
    id: ElementId,
    scroll_handle: H,
    wheel_handling: WheelHandling,
    debug: Option<Hsla>,
}

impl<H: ScrollbarHandle + Clone> ScrollableMask<H> {
    /// Create a new scrollable mask element.
    #[track_caller]
    pub fn new(axis: Axis, scroll_handle: &H) -> Self {
        Self {
            scroll_handle: scroll_handle.clone(),
            axis,
            id: caller_id(),
            wheel_handling: WheelHandling::AlongAxis,
            debug: None,
        }
    }

    /// Create a mask over a native horizontal scroller — one with
    /// `overflow_x_scroll` and a locked scroll axis, such as a strip of tabs —
    /// that scrolls it sideways with a plain mouse wheel's vertical lines, the
    /// only axis such a wheel has.
    ///
    /// Once the scroller cannot move that way, the wheel goes on to the parent,
    /// so a page still scrolls past the strip. Trackpad gestures and
    /// horizontal wheels are left to the scroller, with its own axis lock and
    /// bounce.
    #[track_caller]
    pub fn vertical_wheel(scroll_handle: &H) -> Self {
        Self {
            wheel_handling: WheelHandling::MapVerticalToHorizontal,
            ..Self::new(Axis::Horizontal, scroll_handle)
        }
    }

    /// Scroll inline document content horizontally with a horizontal gesture or
    /// Shift-wheel. Leave ordinary vertical input to the document viewport.
    #[track_caller]
    pub fn horizontal_content(scroll_handle: &H) -> Self {
        Self {
            wheel_handling: WheelHandling::HorizontalContent,
            ..Self::new(Axis::Horizontal, scroll_handle)
        }
    }

    /// Set a specific element id, default is the [`std::panic::Location::caller`].
    ///
    /// Only needed when one call site creates several masks of the same axis,
    /// which would otherwise share their gesture axis lock.
    pub fn id(mut self, id: impl Into<ElementId>) -> Self {
        self.id = id.into();
        self
    }

    /// Enable the debug border, to show the mask bounds.
    #[allow(dead_code)]
    pub fn debug(mut self) -> Self {
        self.debug = Some(gpui::yellow());
        self
    }
}

impl<H: ScrollbarHandle + Clone> IntoElement for ScrollableMask<H> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<H: ScrollbarHandle + Clone> Element for ScrollableMask<H> {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    // An id is needed to keep the gesture's axis lock across frames. The axis
    // suffix keeps both masks of one scroller apart when they share an id.
    fn id(&self) -> Option<ElementId> {
        let axis = match self.axis {
            Axis::Horizontal => "horizontal",
            Axis::Vertical => "vertical",
        };

        Some((self.id.clone(), axis).into())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        // Cover the parent whatever its layout: pinned to its top-left corner
        // rather than left at a static position that depends on whether the
        // parent stacks its children in a column or a row.
        style.position = Position::Absolute;
        style.inset.top = px(0.).into();
        style.inset.left = px(0.).into();
        style.flex_grow = 1.0;
        style.flex_shrink = 1.0;
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();

        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
        window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let axis = self.axis;
        let is_horizontal = axis.is_horizontal();
        let vertical_wheel = self.wheel_handling == WheelHandling::MapVerticalToHorizontal;
        let horizontal_content = self.wheel_handling == WheelHandling::HorizontalContent;
        let content_gesture = global_id.filter(|_| horizontal_content).map(|id| {
            window.with_element_state::<Rc<RefCell<ContentScrollGesture>>, _>(id, |state, _| {
                let state = state.unwrap_or_default();
                (state.clone(), state)
            })
        });
        let bounds = hitbox.bounds;
        let ongoing_scroll = global_id
            .filter(|_| !horizontal_content && !vertical_wheel)
            .map(|global_id| {
                window.with_element_state::<Rc<RefCell<OngoingScroll>>, _>(global_id, |state, _| {
                    let state = state.unwrap_or_default();
                    (state.clone(), state)
                })
            });
        let smooth_scroll = global_id.filter(|_| cx.smooth_scrolling()).map(|id| {
            window.with_element_state(id, |state: Option<SmoothScroll>, _| {
                let state = state.unwrap_or_default();
                (state.clone(), state)
            })
        });

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            if let Some(color) = self.debug {
                window.paint_quad(PaintQuad {
                    bounds,
                    border_widths: Edges::all(px(1.0)),
                    border_color: color,
                    background: gpui::transparent_white().into(),
                    corner_radii: Corners::all(px(0.)),
                    border_style: BorderStyle::default(),
                });
            }

            window.on_mouse_event({
                let view_id = window.current_view();
                let scroll_handle = self.scroll_handle.clone();
                let hitbox_id = hitbox.id;
                let ongoing_scroll = ongoing_scroll.clone();

                move |event: &ScrollEvent, phase, window, cx| {
                    // Handle in the capture phase: ancestor scrollers such as
                    // `gpui::list` register their wheel listeners after their
                    // children paint, so in the bubble phase (reverse
                    // registration order) they run first and would consume the
                    // vertical component of a trackpad swipe before this mask
                    // could stop the propagation.
                    //
                    // `should_handle_scroll` (instead of a raw bounds check)
                    // keeps the mask inert when it is occluded, e.g. below an
                    // open dialog or context menu.
                    if !(phase.capture() && hitbox_id.should_handle_scroll(window)) {
                        return;
                    }

                    let mut offset = scroll_handle.offset();
                    let mut delta = event.delta.content_delta();
                    if let Some(gesture) = &content_gesture {
                        if scroll_handle.content_size().width
                            <= scroll_handle.viewport_bounds().size.width
                            || !delta.x.as_f32().is_finite()
                            || !delta.y.as_f32().is_finite()
                        {
                            return;
                        }
                        let horizontal = if event.is_wheel() {
                            // Some backends already supply the Shift-wheel delta on X.
                            if event.shift && delta.x.is_zero() {
                                delta.x = delta.y;
                            }
                            !delta.x.is_zero() && (event.shift || delta.x.abs() > delta.y.abs())
                        } else {
                            gesture.borrow_mut().axis(
                                delta,
                                event.touch_phase,
                                cx.background_executor().now(),
                            ) == Some(Axis::Horizontal)
                        };
                        if !horizontal {
                            return;
                        }
                        // Even a packet at the horizontal boundary belongs to
                        // this gesture. Its vertical component must not move
                        // the page.
                        cx.stop_propagation();
                        if delta.x.is_zero() {
                            return;
                        }
                        delta.y = px(0.);
                    }
                    let animate_wheel = smooth_scroll.is_some() && cx.smooth_scrolling();

                    let mut animate = |axis: Axis, distance: Pixels| {
                        let Some(smooth_scroll) = &smooth_scroll else {
                            return false;
                        };
                        let scroll_max = (scroll_handle.content_size().along(axis)
                            - scroll_handle.viewport_bounds().size.along(axis))
                        .max(px(0.));
                        let delta = match axis {
                            Axis::Horizontal => gpui::point(distance, px(0.)),
                            Axis::Vertical => gpui::point(px(0.), distance),
                        };
                        let target = smooth_scroll
                            .target(offset, delta, window)
                            .apply_along(axis, |value| value.clamp(-scroll_max, px(0.)));
                        let handle = scroll_handle.clone();
                        let handled = smooth_scroll.animate(
                            offset,
                            target,
                            window,
                            cx,
                            move |last, next, _, cx| {
                                if handle.offset() != last {
                                    return None;
                                }
                                let max = (handle.content_size().along(axis)
                                    - handle.viewport_bounds().size.along(axis))
                                .max(px(0.));
                                let next =
                                    next.apply_along(axis, |value| value.clamp(-max, px(0.)));
                                if last != next {
                                    handle.set_offset(next);
                                    cx.notify(view_id);
                                }
                                Some(handle.offset())
                            },
                        );
                        if handled && is_horizontal && !vertical_wheel {
                            record_horizontal_scroll(target.x - offset.x);
                        }
                        handled
                    };

                    // A mouse wheel's vertical deltas move the scroller
                    // sideways; once it is at its edge the parent scrolls
                    // instead, as for a vertical mask.
                    if vertical_wheel {
                        if !event.is_wheel()
                            || !delta.x.is_zero()
                            || delta.y.is_zero()
                            || !delta.y.as_f32().is_finite()
                        {
                            return;
                        }
                        if animate_wheel {
                            if animate(Axis::Horizontal, delta.y) {
                                cx.stop_propagation();
                            }
                            return;
                        }
                        let axis_max = (scroll_handle.content_size().width
                            - scroll_handle.viewport_bounds().size.width)
                            .max(px(0.));
                        let current = offset.x.clamp(-axis_max, px(0.));
                        let new_offset = (current + delta.y).clamp(-axis_max, px(0.));
                        if new_offset == current {
                            return;
                        }

                        offset.x = new_offset;
                        scroll_handle.set_offset(offset);
                        cx.notify(view_id);
                        cx.stop_propagation();
                        return;
                    }

                    // With nothing to scroll sideways, a swipe that moves
                    // vertically at all belongs to the parent scroller.
                    if is_horizontal
                        && !delta.y.is_zero()
                        && scroll_handle.content_size().width
                            <= scroll_handle.viewport_bounds().size.width
                    {
                        return;
                    }

                    // Lock the gesture to the axis it started on, so a diagonal
                    // trackpad swipe cannot flip which mask consumes it from one
                    // event to the next. Line deltas carry no touch phase.
                    if !event.is_wheel()
                        && let Some(ongoing_scroll) = &ongoing_scroll
                    {
                        ongoing_scroll
                            .borrow_mut()
                            .lock_axis(&mut delta, event.touch_phase);
                    }

                    // Limit for only one way scrolling at same time.
                    // When use MacBook touchpad we may get both x and y delta,
                    // only allows the one that more to scroll.
                    if !delta.x.is_zero() && !delta.y.is_zero() {
                        if delta.x.abs() > delta.y.abs() {
                            delta.y = px(0.);
                        } else {
                            delta.x = px(0.);
                        }
                    }

                    if event.is_wheel() && animate_wheel {
                        let distance = delta.along(axis);
                        if !distance.is_zero() && (animate(axis, distance) || is_horizontal) {
                            cx.stop_propagation();
                        }
                        return;
                    }

                    if !is_horizontal {
                        // The current offset must be clamped too: after a
                        // bubbled event, the scrolled element's own listener
                        // pushes the shared offset beyond the edge unclamped
                        // (the div only clamps on prepaint), and that
                        // transient overscroll would read as "room to scroll".
                        // `ScrollbarHandle` has no `max_offset`; recover it from
                        // the definition `content_size = viewport + max_offset`.
                        let axis_max = (scroll_handle.content_size().height
                            - scroll_handle.viewport_bounds().size.height)
                            .max(px(0.));
                        let current = offset.y.clamp(-axis_max, px(0.));
                        let new_offset = (current + delta.y).clamp(-axis_max, px(0.));
                        if new_offset == current {
                            // At the edge or no overflow: bubble to the parent.
                            return;
                        }

                        offset.y = new_offset;
                        scroll_handle.set_offset(offset);
                        cx.notify(view_id);
                        cx.stop_propagation();
                        return;
                    }

                    // Record whether this step actually moves the content
                    // (see [`last_horizontal_scroll_claim`]). The current offset
                    // is clamped for comparison only: the next offset must use
                    // the raw value, which may sit past the edge until prepaint.
                    if !delta.x.is_zero() {
                        let axis_max = (scroll_handle.content_size().width
                            - scroll_handle.viewport_bounds().size.width)
                            .max(px(0.));
                        let current = offset.x.clamp(-axis_max, px(0.));
                        // The generic mask still lets the div clamp its raw
                        // offset at prepaint. Document content clamps here.
                        let next = if horizontal_content {
                            current
                        } else {
                            offset.x
                        } + delta.x;
                        let clamped = next.clamp(-axis_max, px(0.));
                        record_horizontal_scroll(clamped - current);
                        offset.x = if horizontal_content { clamped } else { next };
                    }

                    if offset != scroll_handle.offset() {
                        scroll_handle.set_offset(offset);
                        cx.notify(view_id);
                        cx.stop_propagation();
                    }
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, IntoElement, ListAlignment, ListState, Render, ScrollDelta, ScrollEvent,
        TestAppContext, VisualTestContext, Window, div, list, point, px,
    };

    #[test]
    fn corner_notch_paths_stay_inside_their_corner_squares() {
        let width = px(200.);
        let height = px(100.);
        let radius = px(8.);
        let cases = [
            ("top left", point(px(0.), px(0.)), 1., 1.),
            ("top right", point(width, px(0.)), -1., 1.),
            ("bottom right", point(width, height), -1., -1.),
            ("bottom left", point(px(0.), height), 1., -1.),
        ];
        for (name, corner, x_dir, y_dir) in cases {
            let path =
                corner_notch_path(corner, x_dir, y_dir, radius).expect("the notch tessellates");
            let bounds = path.bounds;
            // Curve tessellation may inset fractionally; one pixel of slack.
            let tolerance = 1.0;
            assert!(
                (f32::from(bounds.size.width) - f32::from(radius)).abs() <= tolerance
                    && (f32::from(bounds.size.height) - f32::from(radius)).abs() <= tolerance,
                "{name}: notch size {:?} must stay a {radius:?} square",
                bounds.size,
            );
            let expected_origin = point(
                if x_dir > 0. {
                    corner.x
                } else {
                    corner.x - radius
                },
                if y_dir > 0. {
                    corner.y
                } else {
                    corner.y - radius
                },
            );
            assert!(
                (f32::from(bounds.origin.x) - f32::from(expected_origin.x)).abs() <= tolerance
                    && (f32::from(bounds.origin.y) - f32::from(expected_origin.y)).abs()
                        <= tolerance,
                "{name}: notch bounds {bounds:?} must anchor at the frame corner",
            );
        }
    }

    struct HorizontalScrollAreaTest {
        scroll_handle: ScrollHandle,
    }

    impl Render for HorizontalScrollAreaTest {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(100.)).h(px(40.)).child(horizontal_scroll_area(
                "horizontal-scroll-area",
                &self.scroll_handle,
                &Default::default(),
                div().w(px(300.)).h(px(40.)),
            ))
        }
    }

    #[gpui::test]
    fn horizontal_scroll_area_ignores_vertical_wheel(cx: &mut TestAppContext) {
        let scroll_handle = ScrollHandle::new();
        let (_, cx) = cx.add_window_view({
            let scroll_handle = scroll_handle.clone();
            move |_, _| HorizontalScrollAreaTest {
                scroll_handle: scroll_handle.clone(),
            }
        });
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });

        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-40.))),
            ..Default::default()
        });

        assert_eq!(scroll_handle.offset().x, px(0.));
    }

    /// Reproduces the markdown table case: the scroll area lives inside a
    /// `gpui::list` item. The list registers its wheel listener after its
    /// items paint, so in the bubble phase (reverse registration order) the
    /// list runs first and consumes `delta.y` of every trackpad swipe.
    struct ListWithHorizontalAreaTest {
        scroll_handle: ScrollHandle,
        list_state: ListState,
        occluded: bool,
    }

    impl Render for ListWithHorizontalAreaTest {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let scroll_handle = self.scroll_handle.clone();
            let mut root = div().w(px(100.)).h(px(100.)).child(
                list(self.list_state.clone(), move |ix, _, _| {
                    if ix == 0 {
                        horizontal_scroll_area(
                            "horizontal-scroll-area",
                            &scroll_handle,
                            &Default::default(),
                            div().w(px(300.)).h(px(40.)),
                        )
                        .into_any_element()
                    } else {
                        div().w(px(100.)).h(px(40.)).into_any_element()
                    }
                })
                .w_full()
                .h_full(),
            );
            if self.occluded {
                // An overlay above the list, like an open dialog or menu.
                root = root.child(div().absolute().top_0().left_0().size_full().occlude());
            }
            root
        }
    }

    fn setup_list_test<'a>(
        cx: &'a mut TestAppContext,
        scroll_handle: &ScrollHandle,
        list_state: &ListState,
        occluded: bool,
    ) -> &'a mut VisualTestContext {
        let (_, cx) = cx.add_window_view({
            let scroll_handle = scroll_handle.clone();
            let list_state = list_state.clone();
            move |_, _| ListWithHorizontalAreaTest {
                scroll_handle: scroll_handle.clone(),
                list_state: list_state.clone(),
                occluded,
            }
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
        cx
    }

    #[gpui::test]
    fn horizontal_scroll_area_in_list_keeps_horizontal_dominant_wheel(cx: &mut TestAppContext) {
        let scroll_handle = ScrollHandle::new();
        let list_state = ListState::new(10, ListAlignment::Top, px(0.));
        let cx = setup_list_test(cx, &scroll_handle, &list_state, false);

        // A trackpad swipe is rarely axis-pure: horizontal dominant with a
        // small vertical component.
        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(-40.), px(-10.))),
            ..Default::default()
        });

        // The area consumes the horizontal delta...
        assert_eq!(scroll_handle.offset().x, px(-40.));
        // ...and the outer list must not scroll vertically.
        let scroll_top = list_state.logical_scroll_top();
        assert_eq!((scroll_top.item_ix, scroll_top.offset_in_item), (0, px(0.)));
    }

    #[gpui::test]
    fn horizontal_scroll_area_in_list_bubbles_vertical_dominant_wheel(cx: &mut TestAppContext) {
        let scroll_handle = ScrollHandle::new();
        let list_state = ListState::new(10, ListAlignment::Top, px(0.));
        let cx = setup_list_test(cx, &scroll_handle, &list_state, false);

        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(-10.), px(-40.))),
            ..Default::default()
        });

        // Vertical dominant: the list scrolls, the area does not.
        assert_eq!(scroll_handle.offset().x, px(0.));
        let scroll_top = list_state.logical_scroll_top();
        assert_ne!((scroll_top.item_ix, scroll_top.offset_in_item), (0, px(0.)));
    }

    /// A list whose first row is a horizontal scroll area with content that
    /// fits, like a narrow Markdown table.
    struct ListWithFittingAreaTest(ListState);

    impl Render for ListWithFittingAreaTest {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(100.)).h(px(100.)).child(
                list(self.0.clone(), |ix, _, _| {
                    if ix == 0 {
                        horizontal_scroll_area(
                            "fitting-area",
                            &ScrollHandle::new(),
                            &Default::default(),
                            div().w(px(50.)).h(px(40.)),
                        )
                        .into_any_element()
                    } else {
                        div().w(px(100.)).h(px(40.)).into_any_element()
                    }
                })
                .w_full()
                .h_full(),
            )
        }
    }

    #[gpui::test]
    fn horizontal_scroll_area_that_fits_bubbles_horizontal_dominant_wheel(cx: &mut TestAppContext) {
        let list_state = ListState::new(10, ListAlignment::Top, px(0.));
        let (_, cx) = cx.add_window_view({
            let list_state = list_state.clone();
            move |_, _| ListWithFittingAreaTest(list_state.clone())
        });
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });

        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(-40.), px(-10.))),
            ..Default::default()
        });
        // Nothing to scroll sideways: the list takes the swipe.
        let scroll_top = list_state.logical_scroll_top();
        assert_ne!((scroll_top.item_ix, scroll_top.offset_in_item), (0, px(0.)));
    }

    /// A `MessageScroller`-shaped nesting: a `gpui::list` with a vertical
    /// mask bound to its `ListState`, inside an ancestor vertical scroller.
    struct ListWithVerticalMaskTest {
        outer_handle: ScrollHandle,
        list_state: ListState,
    }

    impl Render for ListWithVerticalMaskTest {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("outer")
                .w(px(100.))
                .h(px(100.))
                .overflow_y_scroll()
                .track_scroll(&self.outer_handle)
                .child(
                    div().w_full().h(px(400.)).child(
                        div()
                            .relative()
                            .w_full()
                            .h(px(80.))
                            .overflow_hidden()
                            .child(
                                list(self.list_state.clone(), |_, _, _| {
                                    div().w_full().h(px(40.)).into_any_element()
                                })
                                .w_full()
                                .h_full(),
                            )
                            .child(ScrollableMask::new(Axis::Vertical, &self.list_state)),
                    ),
                )
        }
    }

    fn setup_vertical_mask_test<'a>(
        cx: &'a mut TestAppContext,
        outer_handle: &ScrollHandle,
        list_state: &ListState,
    ) -> &'a mut VisualTestContext {
        let (_, cx) = cx.add_window_view({
            let outer_handle = outer_handle.clone();
            let list_state = list_state.clone();
            move |_, _| ListWithVerticalMaskTest {
                outer_handle: outer_handle.clone(),
                list_state: list_state.clone(),
            }
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
        cx
    }

    #[gpui::test]
    fn vertical_mask_contains_wheel_inside_the_list(cx: &mut TestAppContext) {
        let outer_handle = ScrollHandle::new();
        // A large overdraw measures every row up front, so the mask sees the
        // full content height.
        let list_state = ListState::new(10, ListAlignment::Top, px(1000.));
        let cx = setup_vertical_mask_test(cx, &outer_handle, &list_state);

        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-40.))),
            ..Default::default()
        });

        // The inner list consumes the wheel; the ancestor must not move.
        assert_eq!(list_state.scroll_px_offset_for_scrollbar().y, px(-40.));
        assert_eq!(outer_handle.offset().y, px(0.));
    }

    #[gpui::test]
    fn vertical_mask_chains_to_the_ancestor_at_the_edge(cx: &mut TestAppContext) {
        let outer_handle = ScrollHandle::new();
        let list_state = ListState::new(10, ListAlignment::Top, px(1000.));
        // Start the inner list at its bottom edge (400 - 80 = 320).
        list_state.scroll_to(gpui::ListOffset {
            item_ix: 8,
            offset_in_item: px(0.),
        });
        let cx = setup_vertical_mask_test(cx, &outer_handle, &list_state);

        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-40.))),
            ..Default::default()
        });

        // At the edge the mask lets the event bubble to the ancestor.
        assert_eq!(list_state.scroll_px_offset_for_scrollbar().y, px(-320.));
        assert_eq!(outer_handle.offset().y, px(-40.));
    }

    #[gpui::test]
    fn horizontal_scroll_area_covers_viewport_after_scrolled(cx: &mut TestAppContext) {
        let scroll_handle = ScrollHandle::new();
        let list_state = ListState::new(10, ListAlignment::Top, px(0.));
        let cx = setup_list_test(cx, &scroll_handle, &list_state, false);

        // Scroll the area to the middle, then repaint.
        scroll_handle.set_offset(point(px(-150.), px(0.)));
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });

        // Swipe over the right side of the viewport. The mask must still
        // cover it — it must not slide away with the scrolled content.
        cx.simulate_event(ScrollEvent {
            position: point(px(90.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(-40.), px(-10.))),
            ..Default::default()
        });

        assert_eq!(scroll_handle.offset().x, px(-190.));
        let scroll_top = list_state.logical_scroll_top();
        assert_eq!((scroll_top.item_ix, scroll_top.offset_in_item), (0, px(0.)));
    }

    #[gpui::test]
    fn horizontal_scroll_area_traps_wheel_at_edge(cx: &mut TestAppContext) {
        let scroll_handle = ScrollHandle::new();
        let list_state = ListState::new(10, ListAlignment::Top, px(0.));
        let cx = setup_list_test(cx, &scroll_handle, &list_state, false);

        // Scroll the area to its right edge (300 - 100 = 200).
        scroll_handle.set_offset(point(px(-200.), px(0.)));
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });

        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(-40.), px(-10.))),
            ..Default::default()
        });

        // A horizontal mask consumes even at the edge: a bubbled horizontal
        // delta would be axis-mapped onto the vertical list (#2468).
        let scroll_top = list_state.logical_scroll_top();
        assert_eq!((scroll_top.item_ix, scroll_top.offset_in_item), (0, px(0.)));
    }

    #[gpui::test]
    fn horizontal_scroll_area_ignores_wheel_when_occluded(cx: &mut TestAppContext) {
        let scroll_handle = ScrollHandle::new();
        let list_state = ListState::new(10, ListAlignment::Top, px(0.));
        let cx = setup_list_test(cx, &scroll_handle, &list_state, true);

        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(-40.), px(-10.))),
            ..Default::default()
        });

        // An overlay (dialog, context menu) occludes the area: the area
        // must not scroll underneath it.
        assert_eq!(scroll_handle.offset().x, px(0.));
    }

    #[gpui::test]
    fn horizontal_scroll_area_uses_horizontal_wheel(cx: &mut TestAppContext) {
        let scroll_handle = ScrollHandle::new();
        let (_, cx) = cx.add_window_view({
            let scroll_handle = scroll_handle.clone();
            move |_, _| HorizontalScrollAreaTest {
                scroll_handle: scroll_handle.clone(),
            }
        });
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });

        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(-40.), px(0.))),
            ..Default::default()
        });

        assert_eq!(scroll_handle.offset().x, px(-40.));
    }

    #[gpui::test]
    fn horizontal_scroll_area_claims_only_steps_that_move(cx: &mut TestAppContext) {
        let scroll_handle = ScrollHandle::new();
        let (_, cx) = cx.add_window_view({
            let scroll_handle = scroll_handle.clone();
            move |_, _| HorizontalScrollAreaTest {
                scroll_handle: scroll_handle.clone(),
            }
        });
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });

        // At the left edge, a swipe toward the start has no room: no claim.
        LAST_HORIZONTAL_SCROLL_CLAIM.set(None);
        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(40.), px(0.))),
            ..Default::default()
        });
        assert!(last_horizontal_scroll_claim().is_none());

        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(-250.), px(0.))),
            ..Default::default()
        });
        let claim = last_horizontal_scroll_claim().expect("a moving step claims");
        assert_eq!(claim.delta_x(), px(-200.));
        assert_eq!(scroll_handle.offset().x, px(-200.));

        // Ignored input keeps the last meaningful claim intact.
        for delta in [point(px(0.), px(-40.)), point(px(0.25), px(0.))] {
            cx.simulate_event(ScrollEvent {
                position: point(px(10.), px(10.)),
                delta: ScrollDelta::Pixels(delta),
                ..Default::default()
            });
            assert_eq!(last_horizontal_scroll_claim(), Some(claim));
        }
    }

    fn setup_horizontal_area_test<'a>(
        cx: &'a mut TestAppContext,
        scroll_handle: &ScrollHandle,
    ) -> &'a mut VisualTestContext {
        let (_, cx) = cx.add_window_view({
            let scroll_handle = scroll_handle.clone();
            move |_, _| HorizontalScrollAreaTest {
                scroll_handle: scroll_handle.clone(),
            }
        });
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
        cx
    }

    #[gpui::test]
    fn horizontal_mask_keeps_axis_lock_within_a_gesture(cx: &mut TestAppContext) {
        let scroll_handle = ScrollHandle::new();
        let cx = setup_horizontal_area_test(cx, &scroll_handle);

        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(-40.), px(-10.))),
            ..Default::default()
        });
        assert_eq!(scroll_handle.offset().x, px(-40.));

        // The same gesture can lean vertical without switching targets.
        // Comparing only this packet would stall the scroller at -40.
        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(-10.), px(-15.))),
            ..Default::default()
        });
        assert_eq!(scroll_handle.offset().x, px(-50.));
    }

    #[gpui::test]
    fn horizontal_content_keeps_its_axis_during_a_strong_turn(cx: &mut TestAppContext) {
        let scroll_handle = ScrollHandle::new();
        let cx = setup_horizontal_area_test(cx, &scroll_handle);

        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(-40.), px(-10.))),
            ..Default::default()
        });
        assert_eq!(scroll_handle.offset().x, px(-40.));

        // A strong turn is still part of the same gesture.
        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(-10.), px(-25.))),
            ..Default::default()
        });
        assert_eq!(scroll_handle.offset().x, px(-50.));
    }

    /// Reproduces the DataTable case: a vertically scrollable element
    /// nested inside an outer vertical scroller.
    struct NestedVerticalScrollTest {
        outer_handle: ScrollHandle,
        inner_handle: ScrollHandle,
        inner_content_height: Pixels,
    }

    impl Render for NestedVerticalScrollTest {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("outer")
                .w(px(100.))
                .h(px(100.))
                .overflow_y_scroll()
                .track_scroll(&self.outer_handle)
                .child(
                    div()
                        .relative()
                        .w_full()
                        .h(px(60.))
                        .child(
                            div()
                                .id("inner")
                                .size_full()
                                .overflow_y_scroll()
                                .track_scroll(&self.inner_handle)
                                .child(div().w_full().h(self.inner_content_height)),
                        )
                        .child(ScrollableMask::new(Axis::Vertical, &self.inner_handle)),
                )
                .child(div().w_full().h(px(400.)))
        }
    }

    fn setup_nested_vertical_test<'a>(
        cx: &'a mut TestAppContext,
        outer_handle: &ScrollHandle,
        inner_handle: &ScrollHandle,
        inner_content_height: Pixels,
    ) -> &'a mut VisualTestContext {
        let (_, cx) = cx.add_window_view({
            let outer_handle = outer_handle.clone();
            let inner_handle = inner_handle.clone();
            move |_, _| NestedVerticalScrollTest {
                outer_handle: outer_handle.clone(),
                inner_handle: inner_handle.clone(),
                inner_content_height,
            }
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
        cx
    }

    #[gpui::test]
    fn vertical_mask_consumes_wheel_when_scrollable(cx: &mut TestAppContext) {
        let outer_handle = ScrollHandle::new();
        let inner_handle = ScrollHandle::new();
        let cx = setup_nested_vertical_test(cx, &outer_handle, &inner_handle, px(300.));

        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-40.))),
            ..Default::default()
        });

        // The inner scroller consumes the event; the outer one must not move.
        assert_eq!(inner_handle.offset().y, px(-40.));
        assert_eq!(outer_handle.offset().y, px(0.));
    }

    #[gpui::test]
    fn vertical_mask_hands_off_to_parent_at_edge(cx: &mut TestAppContext) {
        let outer_handle = ScrollHandle::new();
        let inner_handle = ScrollHandle::new();
        let cx = setup_nested_vertical_test(cx, &outer_handle, &inner_handle, px(300.));

        // Scroll the inner element to its bottom edge (300 - 60 = 240).
        inner_handle.set_offset(point(px(0.), px(-240.)));
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });

        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-40.))),
            ..Default::default()
        });

        // At the edge the event bubbles: the outer scroller takes over.
        assert_eq!(outer_handle.offset().y, px(-40.));
        // The inner offset is clamped back to the edge on the next prepaint.
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
        assert_eq!(inner_handle.offset().y, px(-240.));
    }

    #[gpui::test]
    fn vertical_mask_bubbles_when_no_overflow(cx: &mut TestAppContext) {
        let outer_handle = ScrollHandle::new();
        let inner_handle = ScrollHandle::new();
        // Inner content (40) fits its 60px viewport: nothing to scroll.
        let cx = setup_nested_vertical_test(cx, &outer_handle, &inner_handle, px(40.));

        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-40.))),
            ..Default::default()
        });

        assert_eq!(outer_handle.offset().y, px(-40.));
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
        assert_eq!(inner_handle.offset().y, px(0.));
    }

    #[gpui::test]
    fn vertical_mask_ignores_transient_overscroll(cx: &mut TestAppContext) {
        let outer_handle = ScrollHandle::new();
        let inner_handle = ScrollHandle::new();
        let cx = setup_nested_vertical_test(cx, &outer_handle, &inner_handle, px(300.));

        inner_handle.set_offset(point(px(0.), px(-240.)));
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });

        // Two wheel events at the edge with no redraw in between: the first
        // one leaves the inner offset beyond the edge unclamped, which must
        // not read as "room to scroll" and swallow the second event.
        for _ in 0..2 {
            cx.simulate_event(ScrollEvent {
                position: point(px(10.), px(10.)),
                delta: ScrollDelta::Pixels(point(px(0.), px(-40.))),
                ..Default::default()
            });
        }

        assert_eq!(outer_handle.offset().y, px(-80.));
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
        assert_eq!(inner_handle.offset().y, px(-240.));
    }

    /// The vertical mask nested in a `gpui::list` ancestor: the list
    /// registers its wheel listener after its items paint, so only a
    /// capture-phase mask can stop it from scrolling on the same event.
    struct ListWithVerticalAreaTest {
        scroll_handle: ScrollHandle,
        list_state: ListState,
    }

    impl Render for ListWithVerticalAreaTest {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let scroll_handle = self.scroll_handle.clone();
            div().w(px(100.)).h(px(100.)).child(
                list(self.list_state.clone(), move |ix, _, _| {
                    if ix == 0 {
                        div()
                            .relative()
                            .w_full()
                            .h(px(60.))
                            .child(
                                div()
                                    .id("inner")
                                    .size_full()
                                    .overflow_y_scroll()
                                    .track_scroll(&scroll_handle)
                                    .child(div().w_full().h(px(300.))),
                            )
                            .child(ScrollableMask::new(Axis::Vertical, &scroll_handle))
                            .into_any_element()
                    } else {
                        div().w(px(100.)).h(px(40.)).into_any_element()
                    }
                })
                .w_full()
                .h_full(),
            )
        }
    }

    #[gpui::test]
    fn vertical_mask_in_list_consumes_wheel_when_scrollable(cx: &mut TestAppContext) {
        let scroll_handle = ScrollHandle::new();
        let list_state = ListState::new(10, ListAlignment::Top, px(0.));
        let (_, cx) = cx.add_window_view({
            let scroll_handle = scroll_handle.clone();
            let list_state = list_state.clone();
            move |_, _| ListWithVerticalAreaTest {
                scroll_handle: scroll_handle.clone(),
                list_state: list_state.clone(),
            }
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });

        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-40.))),
            ..Default::default()
        });

        // The inner scroller consumes the event; the list must not scroll.
        assert_eq!(scroll_handle.offset().y, px(-40.));
        let scroll_top = list_state.logical_scroll_top();
        assert_eq!((scroll_top.item_ix, scroll_top.offset_in_item), (0, px(0.)));
    }

    /// A 100px strip of 300px content, locked to its axis like a tab strip,
    /// with a vertical-wheel mask, inside a vertical scroller.
    struct VerticalWheelTest {
        strip: ScrollHandle,
        page: ScrollHandle,
    }

    impl Render for VerticalWheelTest {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("page")
                .w(px(100.))
                .h(px(60.))
                .overflow_y_scroll()
                .track_scroll(&self.page)
                .child(
                    div()
                        .relative()
                        .w(px(100.))
                        .h(px(40.))
                        .child(
                            div()
                                .id("strip")
                                .size_full()
                                .flex()
                                .overflow_x_scroll()
                                .restrict_scroll_to_axis()
                                .track_scroll(&self.strip)
                                .child(div().flex_none().w(px(300.)).h(px(40.))),
                        )
                        .child(ScrollableMask::vertical_wheel(&self.strip)),
                )
                .child(div().h(px(200.)))
        }
    }

    fn vertical_wheel_test(
        cx: &mut TestAppContext,
    ) -> (&mut VisualTestContext, ScrollHandle, ScrollHandle) {
        let (strip, page) = (ScrollHandle::new(), ScrollHandle::new());
        let (_, cx) = cx.add_window_view({
            let (strip, page) = (strip.clone(), page.clone());
            move |_, _| VerticalWheelTest { strip, page }
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, strip, page)
    }

    fn wheel(cx: &mut VisualTestContext, delta: ScrollDelta) {
        cx.simulate_event(ScrollEvent {
            position: point(px(10.), px(10.)),
            delta,
            ..Default::default()
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    #[gpui::test]
    fn vertical_wheel_scrolls_the_strip_and_then_the_parent(cx: &mut TestAppContext) {
        let (cx, strip, page) = vertical_wheel_test(cx);
        let line_height = ScrollDelta::Lines(gpui::point(0., 1.)).content_delta().y;

        wheel(cx, ScrollDelta::Lines(point(0., -1.)));
        assert_eq!(strip.offset().x, -line_height);
        assert_eq!(page.offset().y, px(0.));

        // Past the strip's end, the wheel scrolls the page.
        wheel(cx, ScrollDelta::Lines(point(0., -100.)));
        assert_eq!(strip.offset().x, px(-200.));
        assert_eq!(page.offset().y, px(0.));
        wheel(cx, ScrollDelta::Lines(point(0., -1.)));
        assert_eq!(strip.offset().x, px(-200.));
        assert!(page.offset().y < px(0.));

        // A delta that is no number does not move the strip.
        wheel(cx, ScrollDelta::Lines(point(0., f32::NAN)));
        assert_eq!(strip.offset().x, px(-200.));
    }

    #[gpui::test]
    fn vertical_wheel_leaves_trackpads_and_horizontal_wheels_to_the_scroller(
        cx: &mut TestAppContext,
    ) {
        let (cx, strip, page) = vertical_wheel_test(cx);
        let line_height = ScrollDelta::Lines(gpui::point(1., 0.)).content_delta().x;

        // A vertical trackpad swipe keeps the strip's axis lock.
        wheel(cx, ScrollDelta::Pixels(point(px(0.), px(-20.))));
        assert_eq!(strip.offset().x, px(0.));
        assert_eq!(page.offset().y, px(-20.));

        // A horizontal wheel reaches the strip's own listener.
        wheel(cx, ScrollDelta::Lines(point(-1., 0.)));
        assert_eq!(strip.offset().x, -line_height);
    }
}
