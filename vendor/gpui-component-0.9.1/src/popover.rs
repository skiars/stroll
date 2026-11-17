use gpui::{
    Anchor, AnyElement, App, Background, Bounds, Context, Div, ElementId, FocusHandle, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement, PathBuilder, Pixels, Point,
    Refineable as _, RenderOnce, Stateful, Style, StyleRefinement, Styled, Window, canvas, point,
    prelude::FluentBuilder as _, px, transparent_black,
};
use std::{cell::Cell, rc::Rc};

use crate::ThemeStyled as _;
use crate::{
    Selectable, StyledExt as _, scroll::ScrollableElement as _, styled::popover_shadow, v_flex,
};
use gpui_base::Popover as BasePopover;
pub use gpui_base::PopoverState;
use gpui_base::{Placement, Positioner};

pub(crate) fn init(_: &mut App) {}

const DROPDOWN_GAP: Pixels = px(6.);
const DROPDOWN_MARGIN: Pixels = px(8.);

pub(crate) fn dropdown_positioner(bounds: Bounds<Pixels>) -> Positioner {
    Positioner::side(bounds)
        .placement(Placement::Bottom)
        .align(gpui_base::Align::Start)
        .offset(DROPDOWN_GAP)
        .margin(DROPDOWN_MARGIN)
}

/// Choose a side from the space needed for the fixed controls and one row.
/// The full height of the filtered list must not influence this decision.
pub(crate) fn dropdown_list_positioner(
    bounds: Bounds<Pixels>,
    minimum_height: Pixels,
    window: &Window,
) -> (Positioner, Pixels) {
    let viewport = Positioner::viewport_bounds(window, DROPDOWN_MARGIN);
    let below = bounds.bottom() + DROPDOWN_GAP;
    let above = bounds.top() - DROPDOWN_GAP;
    let below_height = (viewport.bottom() - below).max(px(0.));
    let above_height = (above - viewport.top()).max(px(0.));
    let (anchor, y, height) = if below_height < minimum_height && above_height > below_height {
        (Anchor::BottomLeft, above, above_height)
    } else {
        (Anchor::TopLeft, below, below_height)
    };

    (
        Positioner::corner(anchor, point(bounds.left(), y)).margin(DROPDOWN_MARGIN),
        height,
    )
}

/// Places a searchable dropdown and keeps its surface paintable during exit.
pub(crate) fn dropdown_popup(
    id: impl Into<ElementId>,
    positioner: Positioner,
    open: bool,
    surface: impl IntoElement + Styled + 'static,
    window: &mut Window,
    cx: &mut App,
) -> Positioner {
    use gpui_base::{DROPDOWN_TRAVEL, PopupFrame, PopupMotion, PopupMotionKind};
    use std::time::Instant;

    let motion = window.use_keyed_state(id, cx, |_, _| None::<PopupMotion>);
    let opacity = motion.update(cx, |motion, cx| {
        if open {
            motion
                .get_or_insert_with(|| PopupMotion::new(PopupMotionKind::Dropdown, Instant::now()));
        }
        let Some(active) = motion else {
            return 0.;
        };
        active.retarget(open, Instant::now());
        let opacity = active.opacity(window, cx);
        if !open && opacity == 0. {
            *motion = None;
        }
        opacity
    });

    positioner
        .travel(DROPDOWN_TRAVEL * (1. - opacity))
        .when(open || opacity > 0., |positioner| {
            positioner.child(PopupFrame::new(
                surface
                    .opacity(opacity)
                    .shadow(popover_shadow(opacity.powi(3))),
                !open,
            ))
        })
}

/// A popover element that can be triggered by a button or any other element.
#[derive(IntoElement)]
pub struct Popover {
    id: ElementId,
    style: StyleRefinement,
    anchor: Anchor,
    offset: Option<Pixels>,
    arrow: bool,
    default_open: bool,
    motion_kind: gpui_base::PopupMotionKind,
    open: Option<bool>,
    tracked_focus_handle: Option<FocusHandle>,
    trigger: Option<Box<dyn FnOnce(bool, &Window, &App) -> AnyElement + 'static>>,
    content: Option<
        Rc<
            dyn Fn(&mut PopoverState, &mut Window, &mut Context<PopoverState>) -> AnyElement
                + 'static,
        >,
    >,
    children: Vec<AnyElement>,
    /// Style for the trigger container, the element laid out in the parent
    /// and measured to anchor the popup.
    trigger_style: Option<StyleRefinement>,
    mouse_button: MouseButton,
    appearance: bool,
    scrollable: bool,
    overlay_closable: bool,
    on_open_change: Option<Rc<dyn Fn(&bool, &mut Window, &mut App)>>,
}

impl Popover {
    /// Create a new Popover with `view` mode.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            style: StyleRefinement::default(),
            anchor: Anchor::TopLeft,
            offset: None,
            arrow: false,
            trigger: None,
            trigger_style: None,
            content: None,
            tracked_focus_handle: None,
            children: vec![],
            mouse_button: MouseButton::Left,
            appearance: true,
            scrollable: true,
            overlay_closable: true,
            default_open: false,
            motion_kind: gpui_base::PopupMotionKind::Popover,
            open: None,
            on_open_change: None,
        }
    }

    /// The standard Popover surface with an arrow, for passive surfaces such as tooltips.
    pub fn surface_with_arrow(
        side: Placement,
        target: Point<Pixels>,
        window: &Window,
        cx: &App,
    ) -> Div {
        let depth = Self::arrow_depth(window);
        Self::with_surface_arrow(
            Self::style_surface(v_flex(), cx),
            move || Some((side, target, depth)),
            window,
            cx,
        )
    }

    fn arrow_depth(window: &Window) -> Pixels {
        window.rem_size() * 0.375
    }

    fn style_surface<E: Styled>(surface: E, cx: &App) -> E {
        surface.popover_style(cx).p_3()
    }

    /// Give a Popover surface a continuous outline pointing at its resolved anchor.
    fn with_surface_arrow<E: Styled + ParentElement>(
        mut surface: E,
        arrow: impl Fn() -> Option<(Placement, Point<Pixels>, Pixels)> + 'static,
        window: &Window,
        cx: &App,
    ) -> E {
        let mut style = Style::default();
        style.refine(surface.style());
        let rem = window.rem_size();
        let background = style
            .background
            .and_then(|fill| fill.color())
            .unwrap_or_else(|| crate::styled::popup_color(cx).into());
        let radius = style.corner_radii.top_left.to_pixels(rem);
        let widths = style.border_widths.to_pixels(rem);
        let border = style.border_color.unwrap_or_else(transparent_black);
        // Absolute children start inside the border. Extend the decoration back
        // to the same outer box as the backdrop and fill.
        surface.border_color(transparent_black()).child(
            canvas(
                |_, _, _| (),
                move |bounds, (), window, _| {
                    paint_arrow_outline(
                        bounds,
                        arrow(),
                        radius,
                        background.into(),
                        border,
                        widths.top,
                        window,
                    );
                },
            )
            .absolute()
            .inset_0()
            .top(-widths.top)
            .right(-widths.right)
            .bottom(-widths.bottom)
            .left(-widths.left),
        )
    }

    pub fn dropdown_motion(mut self) -> Self {
        self.motion_kind = gpui_base::PopupMotionKind::Dropdown;
        self
    }

    /// Set the anchor corner of the popover, default is [`Anchor::TopLeft`].
    ///
    /// This names the popover's own anchor, not a corner of the trigger.
    /// `TopLeft` opens below the trigger, left-aligned; `BottomRight` opens
    /// above it, right-aligned. Legacy anchoring clamps without flipping.
    pub fn anchor(mut self, anchor: impl Into<Anchor>) -> Self {
        self.anchor = anchor.into();
        self
    }

    /// Gap from the trigger to the surface (or arrow tip), default 0.25rem.
    /// Preserves the anchor and does not enable automatic flipping.
    pub fn offset(mut self, offset: impl Into<Pixels>) -> Self {
        self.offset = Some(offset.into());
        self
    }

    /// Show an arrow pointing toward the trigger. Default is `false`.
    /// Follows the anchor, with its base inset to avoid rounded corners.
    /// Uses the surface background, falling back to the theme's popover color.
    pub fn arrow(mut self, arrow: bool) -> Self {
        self.arrow = arrow;
        self
    }

    /// Set the mouse button to trigger the popover, default is `MouseButton::Left`.
    pub fn mouse_button(mut self, mouse_button: MouseButton) -> Self {
        self.mouse_button = mouse_button;
        self
    }

    /// Set the trigger element of the popover.
    pub fn trigger<T>(mut self, trigger: T) -> Self
    where
        T: Selectable + IntoElement + 'static,
    {
        self.trigger = Some(Box::new(|is_open, _, _| {
            let open = trigger.is_open();
            trigger.open(open || is_open).into_any_element()
        }));
        self
    }

    /// Set the default open state of the popover, default is `false`.
    ///
    /// This is only used to initialize the open state of the popover.
    ///
    /// And please note that if you use the `open` method, this value will be ignored.
    pub fn default_open(mut self, open: bool) -> Self {
        self.default_open = open;
        self
    }

    /// Force set the open state of the popover.
    ///
    /// If this is set, the popover will be controlled by this value.
    ///
    /// NOTE: You must be used in conjunction with `on_open_change` to handle state changes.
    pub fn open(mut self, open: bool) -> Self {
        self.open = Some(open);
        self
    }

    /// Add a callback to be called when the open state changes.
    ///
    /// The first `&bool` parameter is the **new open state**.
    ///
    /// This is useful when using the `open` method to control the popover state.
    pub fn on_open_change<F>(mut self, callback: F) -> Self
    where
        F: Fn(&bool, &mut Window, &mut App) + 'static,
    {
        self.on_open_change = Some(Rc::new(callback));
        self
    }

    /// Set the style for the trigger container.
    ///
    /// The container is the element laid out in the parent and measured to
    /// anchor the popup, so this is where `w_full` or `flex_1` must go for the
    /// trigger to fill its slot.
    pub fn trigger_style(mut self, style: StyleRefinement) -> Self {
        self.trigger_style = Some(style);
        self
    }

    /// Let the content use its own scrolling, for example a virtualized list.
    pub fn scrollable(mut self, scrollable: bool) -> Self {
        self.scrollable = scrollable;
        self
    }

    /// Set whether clicking outside the popover will dismiss it, default is `true`.
    pub fn overlay_closable(mut self, closable: bool) -> Self {
        self.overlay_closable = closable;
        self
    }

    /// Set the content builder for content of the Popover.
    ///
    /// This callback will called every time on render the popover.
    /// So, you should avoid creating new elements or entities in the content closure.
    pub fn content<F, E>(mut self, content: F) -> Self
    where
        E: IntoElement,
        F: Fn(&mut PopoverState, &mut Window, &mut Context<PopoverState>) -> E + 'static,
    {
        self.content = Some(Rc::new(move |state, window, cx| {
            content(state, window, cx).into_any_element()
        }));
        self
    }

    /// Set whether the popover no style, default is `false`.
    ///
    /// If no style:
    ///
    /// - The popover will not have a bg, border, shadow, or padding.
    /// - The click out of the popover will not dismiss it.
    pub fn appearance(mut self, appearance: bool) -> Self {
        self.appearance = appearance;
        self
    }

    /// Bind the focus handle to receive focus when the popover is opened.
    /// If you not set this, a new focus handle will be created for the popover to
    ///
    /// If popover is opened, the focus will be moved to the focus handle.
    pub fn track_focus(mut self, handle: &FocusHandle) -> Self {
        self.tracked_focus_handle = Some(handle.clone());
        self
    }
}

impl ParentElement for Popover {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl Styled for Popover {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl Popover {
    pub(crate) fn render_popover_content(
        anchor: Anchor,
        appearance: bool,
        _: &mut Window,
        cx: &mut App,
    ) -> Stateful<Div> {
        v_flex()
            .id("content")
            .occlude()
            .tab_group()
            .when(appearance, |this| Self::style_surface(this, cx))
            .map(|this| match anchor {
                Anchor::TopLeft | Anchor::TopCenter | Anchor::TopRight => this.top_1(),
                Anchor::BottomLeft | Anchor::BottomCenter | Anchor::BottomRight => this.bottom_1(),
                Anchor::LeftCenter | Anchor::RightCenter => this.top_1(), // Fallback for centered
            })
    }
}

impl RenderOnce for Popover {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let anchor = self.anchor;
        let arrow_size = if self.arrow {
            Self::arrow_depth(window)
        } else {
            px(0.)
        };
        let offset = self.offset.unwrap_or(window.rem_size() * 0.25) + arrow_size;
        let resolved = Rc::new(Cell::new(None));
        let arrow_position = resolved.clone();
        let arrow = self.arrow;
        let appearance = self.appearance;
        let scrollable = appearance && self.scrollable;
        let style = self.style;
        let children = self.children;
        let content = self.content;
        let trigger_style = self.trigger_style;

        BasePopover::new(self.id)
            .motion_kind(self.motion_kind)
            .anchor(self.anchor)
            .offset(offset)
            .on_position(move |position, trigger| resolved.set(Some((position, trigger))))
            .mouse_button(self.mouse_button)
            .default_open(self.default_open)
            .overlay_closable(self.overlay_closable)
            .content(move |state, window, cx| {
                let body = v_flex()
                    .when_some(content, |this, content| {
                        this.child((content)(state, window, cx))
                    })
                    .children(children);
                let mut surface = v_flex()
                    .id("content")
                    .occlude()
                    .tab_group()
                    .when(appearance, |this| Self::style_surface(this, cx))
                    .refine_style(&style)
                    .child(if scrollable {
                        body.max_h((window.viewport_size().height - px(64.)).max(px(0.)))
                            .overflow_y_popup_scrollbar()
                            .into_any_element()
                    } else {
                        body.into_any_element()
                    });
                if arrow {
                    surface = Self::with_surface_arrow(
                        surface,
                        move || {
                            let (position, trigger) = arrow_position.get()?;
                            let (legacy_side, target) = arrow_anchor(anchor, trigger);
                            Some((
                                position.placement.unwrap_or(legacy_side),
                                target,
                                arrow_size,
                            ))
                        },
                        window,
                        cx,
                    );
                }
                surface
            })
            .when_some(self.trigger, |this, trigger| this.trigger_with(trigger))
            .when_some(trigger_style, |this, style| this.refine_style(&style))
            .when_some(self.open, |this, open| this.open(open))
            .when_some(self.tracked_focus_handle, |this, handle| {
                this.track_focus(&handle)
            })
            .when_some(self.on_open_change, |this, callback| {
                this.on_open_change(move |open, window, cx| callback(open, window, cx))
            })
            .into_any_element()
    }
}

/// The arrow follows the named anchor instead of always aiming at trigger center.
fn arrow_anchor(anchor: Anchor, trigger: Bounds<Pixels>) -> (Placement, Point<Pixels>) {
    match anchor {
        Anchor::TopLeft => (Placement::Bottom, trigger.bottom_left()),
        Anchor::TopCenter => (Placement::Bottom, trigger.bottom_center()),
        Anchor::TopRight => (Placement::Bottom, trigger.bottom_right()),
        Anchor::BottomLeft => (Placement::Top, trigger.origin),
        Anchor::BottomCenter => (Placement::Top, trigger.top_center()),
        Anchor::BottomRight => (Placement::Top, trigger.top_right()),
        Anchor::LeftCenter => (Placement::Right, trigger.right_center()),
        Anchor::RightCenter => (Placement::Left, trigger.left_center()),
    }
}

fn paint_arrow_outline(
    bounds: Bounds<Pixels>,
    arrow: Option<(Placement, Point<Pixels>, Pixels)>,
    radius: Pixels,
    background: Background,
    border_color: Hsla,
    border_width: Pixels,
    window: &mut Window,
) {
    let bounds = Bounds::from_corners(
        window.pixel_snap_point(bounds.origin),
        window.pixel_snap_point(bounds.bottom_right()),
    );
    if bounds.is_empty() {
        return;
    }

    let radius = radius.min(bounds.size.width.min(bounds.size.height) / 2.);
    let arrow = arrow.and_then(|(side, target, depth)| {
        let points = arrow_points(bounds, side, target, depth, radius, border_width)?;
        Some((side, points))
    });
    if let Some((_, [start, tip, end])) = arrow {
        // Meet the popup material exactly at its snapped edge. Overlapping it
        // would apply the translucent tint twice, leaving a seam at the join.
        let mut fill = PathBuilder::fill();
        fill.move_to(start);
        fill.line_to(tip);
        fill.line_to(end);
        fill.close();
        paint_path(window, fill, background);
    }
    if border_width > px(0.) && border_color.a > 0. {
        let mut border = PathBuilder::stroke(border_width);
        outline(&mut border, bounds, radius, arrow);
        paint_path(window, border, border_color);
    }
}

fn paint_path(window: &mut Window, builder: PathBuilder, color: impl Into<Background>) {
    if let Ok(mut path) = builder.build() {
        // Retain the partially covered pixels at the tip and perimeter.
        path.bounds = path.bounds.dilate(px(1. / window.scale_factor()));
        window.paint_path(path, color);
    }
}

fn arrow_points(
    bounds: Bounds<Pixels>,
    side: Placement,
    target: Point<Pixels>,
    depth: Pixels,
    radius: Pixels,
    stroke: Pixels,
) -> Option<[Point<Pixels>; 3]> {
    let (start, end, target) = if side.is_horizontal() {
        (bounds.top(), bounds.bottom(), target.y)
    } else {
        (bounds.left(), bounds.right(), target.x)
    };
    let half = depth.min((end - start) / 2. - radius - stroke);
    if half <= px(0.) {
        return None;
    }
    let inset = radius + half + stroke;
    let center = target.clamp(start + inset, end - inset);
    let a = center - half;
    let b = center + half;
    Some(match side {
        Placement::Bottom => [
            point(a, bounds.top()),
            point(center, bounds.top() - depth),
            point(b, bounds.top()),
        ],
        Placement::Left => [
            point(bounds.right(), a),
            point(bounds.right() + depth, center),
            point(bounds.right(), b),
        ],
        Placement::Top => [
            point(b, bounds.bottom()),
            point(center, bounds.bottom() + depth),
            point(a, bounds.bottom()),
        ],
        Placement::Right => [
            point(bounds.left(), b),
            point(bounds.left() - depth, center),
            point(bounds.left(), a),
        ],
    })
}

/// Trace clockwise. On the arrow edge, the two slopes replace the straight base.
fn outline(
    path: &mut PathBuilder,
    bounds: Bounds<Pixels>,
    radius: Pixels,
    arrow: Option<(Placement, [Point<Pixels>; 3])>,
) {
    let (left, top, right, bottom) = (bounds.left(), bounds.top(), bounds.right(), bounds.bottom());
    let control = radius * 0.552_284_8;
    let edge = |path: &mut PathBuilder, side, end| {
        if let Some((placement, points)) = arrow
            && placement == side
        {
            for point in points {
                path.line_to(point);
            }
        }
        path.line_to(end);
    };
    path.move_to(point(left + radius, top));
    edge(path, Placement::Bottom, point(right - radius, top));
    path.cubic_bezier_to(
        point(right, top + radius),
        point(right - radius + control, top),
        point(right, top + radius - control),
    );
    edge(path, Placement::Left, point(right, bottom - radius));
    path.cubic_bezier_to(
        point(right - radius, bottom),
        point(right, bottom - radius + control),
        point(right - radius + control, bottom),
    );
    edge(path, Placement::Top, point(left + radius, bottom));
    path.cubic_bezier_to(
        point(left, bottom - radius),
        point(left + radius - control, bottom),
        point(left, bottom - radius + control),
    );
    edge(path, Placement::Right, point(left, top + radius));
    path.cubic_bezier_to(
        point(left + radius, top),
        point(left, top + radius - control),
        point(left + radius - control, top),
    );
    path.close();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{button::Button, h_flex, theme::Theme};
    use gpui::{Bounds, Context, MouseButton, Point, Render, div, point, px, size};
    use gpui_base::Popup as BasePopup;
    use std::{cell::RefCell, rc::Rc};

    #[test]
    fn arrows_stay_on_the_straight_edges() {
        let bounds = Bounds::new(point(px(100.), px(100.)), size(px(80.), px(60.)));
        let target = point(px(0.), px(0.));
        for (side, expected) in [
            (Placement::Right, point(px(94.), px(111.))),
            (Placement::Left, point(px(186.), px(111.))),
            (Placement::Bottom, point(px(111.), px(94.))),
            (Placement::Top, point(px(111.), px(166.))),
        ] {
            let points = arrow_points(bounds, side, target, px(6.), px(4.), px(1.)).unwrap();
            assert_eq!(points[1], expected);
        }
    }

    #[test]
    fn test_popover_builder_chaining() {
        let popover = Popover::new("test")
            .anchor(Anchor::BottomCenter)
            .mouse_button(MouseButton::Right)
            .default_open(true)
            .appearance(false)
            .overlay_closable(false);

        assert_eq!(popover.anchor, Anchor::BottomCenter);
        assert_eq!(popover.mouse_button, MouseButton::Right);
        assert!(popover.default_open);
        assert!(!popover.appearance);
        assert!(!popover.overlay_closable);
    }

    #[test]
    fn test_resolved_corner_top_positions() {
        use gpui::px;

        let bounds = Bounds {
            origin: Point {
                x: px(100.),
                y: px(100.),
            },
            size: gpui::Size {
                width: px(200.),
                height: px(50.),
            },
        };

        let pos = BasePopup::resolved_corner(Anchor::TopLeft, bounds);
        assert_eq!(pos.x, px(100.));
        assert_eq!(pos.y, px(100.));

        let pos = BasePopup::resolved_corner(Anchor::TopCenter, bounds);
        assert_eq!(pos.x, px(200.));
        assert_eq!(pos.y, px(100.));

        let pos = BasePopup::resolved_corner(Anchor::TopRight, bounds);
        assert_eq!(pos.x, px(300.));
        assert_eq!(pos.y, px(100.));

        let pos = BasePopup::resolved_corner(Anchor::BottomLeft, bounds);
        assert_eq!(pos.x, px(100.));
        assert_eq!(pos.y, px(50.));

        let pos = BasePopup::resolved_corner(Anchor::BottomCenter, bounds);
        assert_eq!(pos.x, px(200.));
        assert_eq!(pos.y, px(50.));

        let pos = BasePopup::resolved_corner(Anchor::BottomRight, bounds);
        assert_eq!(pos.x, px(300.));
        assert_eq!(pos.y, px(50.));
    }

    struct PopoverHarness {
        changes: Rc<RefCell<Vec<bool>>>,
    }

    struct AnchorHarness {
        anchor: Anchor,
        offset: Option<Pixels>,
        origin: Point<Pixels>,
        arrow: bool,
    }

    impl Render for AnchorHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                div()
                    .absolute()
                    .left(self.origin.x)
                    .top(self.origin.y)
                    .child(
                        Popover::new("positioned-popover")
                            .default_open(true)
                            .appearance(false)
                            .arrow(self.arrow)
                            .anchor(self.anchor)
                            .when_some(self.offset, |this, gap| this.offset(gap))
                            .trigger(Button::new("positioned-trigger").size(px(40.)))
                            .child(
                                div()
                                    .debug_selector(|| "positioned-content".into())
                                    .size(px(60.)),
                            ),
                    ),
            )
        }
    }

    #[gpui::test]
    fn anchor_and_offset_position_the_surface_on_each_trigger_edge(cx: &mut gpui::TestAppContext) {
        cx.update(crate::init);
        let (view, window) = cx.add_window_view(|_, _| AnchorHarness {
            anchor: Anchor::TopLeft,
            offset: None,
            origin: point(px(200.), px(200.)),
            arrow: false,
        });
        window.update(|window, cx| crate::test_redraw(window, cx));
        window.update(|window, cx| crate::test_redraw(window, cx));
        // Legacy TopLeft means below the trigger, including the default 0.25rem gap.
        let legacy = window.debug_bounds("positioned-content").unwrap();
        assert_eq!(legacy.left(), px(200.));
        assert_eq!(legacy.top(), px(244.));

        for (side, x, y) in [
            (Anchor::BottomLeft, 200., 128.),
            (Anchor::BottomCenter, 190., 128.),
            (Anchor::BottomRight, 180., 128.),
            (Anchor::TopLeft, 200., 252.),
            (Anchor::TopCenter, 190., 252.),
            (Anchor::TopRight, 180., 252.),
            (Anchor::RightCenter, 128., 190.),
            (Anchor::LeftCenter, 252., 190.),
        ] {
            window.update(|window, cx| {
                view.update(cx, |view, cx| {
                    view.anchor = side;
                    view.offset = Some(px(12.));
                    cx.notify();
                });
                crate::test_redraw(window, cx);
            });
            assert_eq!(
                window.debug_bounds("positioned-content").unwrap().origin,
                point(px(x), px(y)),
                "{side:?}"
            );
        }
        // Current-frame trigger bounds must be used after the owner moves.
        window.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.origin = point(px(260.), px(240.));
                cx.notify();
            });
            crate::test_redraw(window, cx);
        });
        assert_eq!(
            window.debug_bounds("positioned-content").unwrap().origin,
            point(px(312.), px(230.))
        );
    }

    struct TriggerStyleHarness {
        styled: bool,
    }

    impl Render for TriggerStyleHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            // Keep the trigger clear of the popup's window margin so the
            // content origin reflects only the trigger container's bounds.
            div().size_full().child(
                h_flex()
                    .absolute()
                    .left(px(100.))
                    .top(px(100.))
                    .w(px(200.))
                    .child(
                        Popover::new("trigger-style-popover")
                            .default_open(true)
                            .appearance(false)
                            .offset(px(0.))
                            .anchor(Anchor::TopRight)
                            .when(self.styled, |this| {
                                this.trigger_style(StyleRefinement::default().w_full())
                            })
                            .trigger(Button::new("styled-trigger").size(px(40.)))
                            .child(
                                div()
                                    .debug_selector(|| "styled-content".into())
                                    .size(px(20.)),
                            ),
                    ),
            )
        }
    }

    /// `trigger_style` styles the container the parent lays out and the popup
    /// is anchored to. Applied to anything nested inside it, `w_full` would
    /// resolve against a container that only wraps its content.
    #[gpui::test]
    fn trigger_style_is_applied_to_the_trigger_container(cx: &mut gpui::TestAppContext) {
        cx.update(crate::init);
        let (view, window) = cx.add_window_view(|_, _| TriggerStyleHarness { styled: false });
        window.update(|window, cx| crate::test_redraw(window, cx));
        window.update(|window, cx| crate::test_redraw(window, cx));
        // Unstyled: the container wraps the 40px trigger, so the content's right
        // edge meets the trigger's right edge at 140px.
        assert_eq!(
            window.debug_bounds("styled-content").unwrap().origin,
            point(px(120.), px(140.))
        );

        window.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.styled = true;
                cx.notify();
            });
            crate::test_redraw(window, cx);
        });
        window.update(|window, cx| crate::test_redraw(window, cx));
        // `w_full` stretches the container across the 200px row, and the popup
        // follows the container's right edge at 300px.
        assert_eq!(
            window.debug_bounds("styled-content").unwrap().origin,
            point(px(280.), px(140.))
        );
    }

    #[gpui::test]
    fn arrow_reserves_space_without_changing_anchor_alignment(cx: &mut gpui::TestAppContext) {
        cx.update(crate::init);
        let (view, window) = cx.add_window_view(|_, _| AnchorHarness {
            anchor: Anchor::TopLeft,
            offset: Some(px(12.)),
            origin: point(px(200.), px(8.)),
            arrow: true,
        });
        window.update(|window, cx| crate::test_redraw(window, cx));
        window.update(|window, cx| crate::test_redraw(window, cx));
        // Bottom edge 48 + tip gap 12 + arrow depth 6.
        assert_eq!(
            window.debug_bounds("positioned-content").unwrap().origin,
            point(px(200.), px(66.))
        );
        window.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.anchor = Anchor::TopRight;
                view.arrow = false;
                cx.notify();
            });
            crate::test_redraw(window, cx);
        });
        assert_eq!(
            window.debug_bounds("positioned-content").unwrap().origin,
            point(px(180.), px(60.))
        );
    }

    #[gpui::test]
    fn anchor_does_not_flip_when_offset_or_arrow_is_enabled(cx: &mut gpui::TestAppContext) {
        cx.update(crate::init);
        let (_, window) = cx.add_window_view(|_, _| AnchorHarness {
            anchor: Anchor::BottomCenter,
            offset: Some(px(12.)),
            origin: point(px(200.), px(8.)),
            arrow: true,
        });
        window.update(|window, cx| crate::test_redraw(window, cx));
        window.update(|window, cx| crate::test_redraw(window, cx));
        // Clamp to the window margin instead of flipping below the trigger.
        assert_eq!(
            window.debug_bounds("positioned-content").unwrap().top(),
            px(8.)
        );
    }

    #[test]
    fn arrow_alignment_uses_the_anchor_instead_of_trigger_center() {
        let trigger = Bounds::new(point(px(120.), px(120.)), size(px(40.), px(20.)));
        for (anchor, side, target) in [
            (
                Anchor::TopLeft,
                Placement::Bottom,
                point(px(120.), px(140.)),
            ),
            (
                Anchor::TopCenter,
                Placement::Bottom,
                point(px(140.), px(140.)),
            ),
            (
                Anchor::TopRight,
                Placement::Bottom,
                point(px(160.), px(140.)),
            ),
            (
                Anchor::BottomLeft,
                Placement::Top,
                point(px(120.), px(120.)),
            ),
            (
                Anchor::BottomCenter,
                Placement::Top,
                point(px(140.), px(120.)),
            ),
            (
                Anchor::BottomRight,
                Placement::Top,
                point(px(160.), px(120.)),
            ),
            (
                Anchor::LeftCenter,
                Placement::Right,
                point(px(160.), px(130.)),
            ),
            (
                Anchor::RightCenter,
                Placement::Left,
                point(px(120.), px(130.)),
            ),
        ] {
            assert_eq!(arrow_anchor(anchor, trigger), (side, target));
        }
    }

    impl Render for PopoverHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let changes = self.changes.clone();
            Popover::new("runtime-popover")
                .trigger(Button::new("runtime-trigger").label("Open").size(px(100.)))
                .content(|_, _, _| {
                    div()
                        .debug_selector(|| "runtime-popover-content".into())
                        .size(px(40.))
                })
                .on_open_change(move |open, _, _| changes.borrow_mut().push(*open))
        }
    }

    #[gpui::test]
    fn pointer_open_and_outside_dismiss_use_the_base_popup_host(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_base::GlobalState::init(cx);
            cx.set_global(Theme::default());
            init(cx);
        });

        let changes = Rc::new(RefCell::new(Vec::new()));
        let (_, cx) = cx.add_window_view({
            let changes = changes.clone();
            move |_, _| PopoverHarness { changes }
        });
        cx.update(|window, cx| crate::test_redraw(window, cx));

        cx.simulate_click(point(px(20.), px(20.)), Default::default());
        cx.update(|window, cx| crate::test_redraw(window, cx));
        assert!(cx.debug_bounds("runtime-popover-content").is_some());

        cx.simulate_click(point(px(300.), px(300.)), Default::default());
        cx.update(|window, cx| crate::test_redraw(window, cx));
        assert!(cx.debug_bounds("runtime-popover-content").is_none());
        // A change callback reports state transitions, not redundant dismissal
        // requests. The base host may see both paths, but only the first closes.
        assert_eq!(&*changes.borrow(), &[true, false]);
    }

    struct DefaultOpenHarness;

    impl Render for DefaultOpenHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            Popover::new("default-open-popover")
                .default_open(true)
                .trigger(Button::new("default-open-trigger").label("Open"))
                .child(
                    div()
                        .debug_selector(|| "default-open-content".into())
                        .size(px(40.)),
                )
        }
    }

    #[gpui::test]
    fn default_open_is_forwarded_to_the_base_popover(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_base::GlobalState::init(cx);
            cx.set_global(Theme::default());
            init(cx);
        });
        let (_, cx) = cx.add_window_view(|_, _| DefaultOpenHarness);
        cx.update(|window, cx| crate::test_redraw(window, cx));
        cx.update(|window, cx| crate::test_redraw(window, cx));
        assert!(cx.debug_bounds("default-open-content").is_some());
    }

    struct Harness {
        open: bool,
    }

    impl Render for Harness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(dropdown_popup(
                "dropdown",
                dropdown_positioner(Bounds::new(
                    point(px(0.), px(100.)),
                    size(px(120.), px(30.)),
                )),
                self.open,
                div().debug_selector(|| "surface".into()).size(px(50.)),
                window,
                cx,
            ))
        }
    }

    #[gpui::test]
    fn dropdown_keeps_its_surface_during_exit_and_restarts_after_it(cx: &mut gpui::TestAppContext) {
        cx.update(crate::init);
        let (view, window) = cx.add_window_view(|_, _| Harness { open: true });

        window.update(|window, cx| crate::test_redraw(window, cx));
        let opening = window.debug_bounds("surface").unwrap().origin;

        std::thread::sleep(std::time::Duration::from_millis(90));
        window.update(|window, cx| crate::test_redraw(window, cx));
        let settled = window.debug_bounds("surface").unwrap().origin;

        assert!(
            opening.y < settled.y,
            "the surface should slide down into place, from {opening:?} to {settled:?}",
        );

        let set_open = |open| {
            window.update(|window, cx| {
                view.update(cx, |this, cx| {
                    this.open = open;
                    cx.notify();
                });
                crate::test_redraw(window, cx);
            });
        };
        set_open(false);
        assert!(window.debug_bounds("surface").is_some());
        std::thread::sleep(std::time::Duration::from_millis(90));
        window.update(|window, cx| crate::test_redraw(window, cx));
        assert!(window.debug_bounds("surface").is_none());
        set_open(true);

        let reopening = window.debug_bounds("surface").unwrap().origin;
        assert!(
            reopening.y < settled.y,
            "reopening should start the motion over at {opening:?} rather than showing a \
             settled surface, but the first frame was already at {reopening:?}",
        );
    }
}
