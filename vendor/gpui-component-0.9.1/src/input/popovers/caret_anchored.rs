//! Caret-anchored popups positioned from the input's current-frame geometry.
//!
//! The input stashes fresh caret geometry in its prepaint
//! ([`PrepaintCaretGeometry`]). Popups live inside `deferred()`, so their
//! prepaint runs after the input's prepaint but before its paint; resolving
//! the anchor there puts the popup at the current frame's caret instead of
//! the previous frame's.

use std::ops::Range;

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Point, Size, Style, WeakEntity, Window, px,
};

use crate::input::{EditorState, PrepaintCaretGeometry};

pub(super) const CARET_POPUP_MARGIN: Pixels = px(8.);

/// Vertical space available to editor popups, excluding surrounding chrome.
pub(super) fn editor_popup_vertical_bounds(editor: &EditorState, window: &Window) -> Range<Pixels> {
    let visible = editor.visible_text_bounds();
    visible.top().max(CARET_POPUP_MARGIN)
        ..visible
            .bottom()
            .min(window.viewport_size().height - CARET_POPUP_MARGIN)
}

/// Computes the caret-anchored popup origin from fresh prepaint geometry.
///
/// Shared by the completion and code-action menus so both anchor to the
/// same point: just below the caret, nudged left. The result is in window
/// coordinates, matching where the caret itself is painted.
pub(crate) fn caret_popup_anchor(geometry: PrepaintCaretGeometry) -> Point<Pixels> {
    Point::new(
        geometry.cursor_bounds().origin.x - px(4.),
        geometry.cursor_bounds().origin.y
            + geometry.scroll_offset().y
            + geometry.line_height()
            + px(4.),
    )
}

/// A popup positioned at the caret from the input's current-frame prepaint.
///
/// The content is built in prepaint (not render) from the resolved anchor,
/// so position, hitboxes, accessibility bounds, and width/layout decisions
/// are all consistent with the current frame's caret. Always use inside
/// `deferred()` so the prepaint runs after the input's prepaint.
pub struct CaretAnchoredPopup {
    editor: WeakEntity<EditorState>,
    build: Box<dyn Fn(Point<Pixels>, Pixels, Pixels, &mut Window, &mut App) -> AnyElement>,
    content: Option<AnyElement>,
    prefer_above: bool,
}

impl CaretAnchoredPopup {
    pub fn new(
        editor: WeakEntity<EditorState>,
        build: impl Fn(Point<Pixels>, Pixels, Pixels, &mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        Self {
            editor,
            build: Box::new(build),
            content: None,
            prefer_above: false,
        }
    }

    /// Prefer the space above the caret when the content fits there.
    pub fn prefer_above(mut self) -> Self {
        self.prefer_above = true;
        self
    }
}

impl Element for CaretAnchoredPopup {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // The real layout happens in prepaint, once the anchor is resolved.
        let layout_id = window.request_layout(Style::default(), [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        let editor = editor.read(cx);
        let Some(geometry) = editor.prepaint_caret_geometry() else {
            return;
        };
        let mut caret = geometry.cursor_bounds();
        caret.origin.y += geometry.scroll_offset().y;
        if !caret.intersects(&editor.visible_text_bounds()) {
            return;
        }
        let vertical = editor_popup_vertical_bounds(editor, window);
        let anchor = caret_popup_anchor(geometry);
        let viewport = window.viewport_size();
        let margin = CARET_POPUP_MARGIN;
        let above_anchor = (anchor.y - geometry.line_height() - px(8.)).min(vertical.end);
        let below_anchor = anchor.y.max(vertical.start);
        let above = above_anchor - vertical.start;
        let below = vertical.end - below_anchor;
        let max_height = above.max(below);
        if max_height <= px(0.) {
            return;
        }
        let mut content = (self.build)(anchor, max_height, geometry.text_size(), window, cx);
        let size = content.layout_as_root(
            Size {
                width: AvailableSpace::MaxContent,
                height: AvailableSpace::MinContent,
            },
            window,
            cx,
        );
        if size.height > max_height {
            return;
        }
        let right = (viewport.width - size.width - margin).max(margin);
        let y = if self.prefer_above && size.height <= above {
            above_anchor - size.height
        } else if size.height <= below {
            below_anchor
        } else {
            above_anchor - size.height
        };
        content.prepaint_at(
            Point::new(
                anchor.x.clamp(margin, right),
                y.clamp(vertical.start, vertical.end - size.height),
            ),
            window,
            cx,
        );
        self.content = Some(content);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(content) = self.content.as_mut() {
            content.paint(window, cx);
        }
    }
}

impl IntoElement for CaretAnchoredPopup {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
