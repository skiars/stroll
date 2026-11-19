use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Element, ElementId, GlobalElementId,
    InspectorElementId, InteractiveElement as _, IntoElement, LayoutId, MouseDownEvent,
    ParentElement as _, Pixels, Size, Style, Styled as _, WeakEntity, Window, div, point, px,
};
use gpui_base::TestSupportExt as _;
use lsp_types::Documentation;

use crate::{list::List, scroll::ScrollableElement as _};

use super::{
    super::{
        caret_anchored::{CARET_POPUP_MARGIN, caret_popup_anchor},
        editor_popover, render_markdown,
    },
    CompletionMenu, ROW_HEIGHT_PADDING,
};

const MAX_LIST_HEIGHT: Pixels = px(240.);
const MIN_DOCUMENTATION_WIDTH: Pixels = px(200.);
const PANEL_GAP: Pixels = px(6.);
const POPUP_SPACE: Size<AvailableSpace> = Size {
    width: AvailableSpace::MaxContent,
    height: AvailableSpace::MinContent,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Above,
    Below,
}

impl Side {
    fn choose(above: Pixels, below: Pixels, preferred: Pixels) -> Self {
        if below >= preferred || (above < preferred && below >= above) {
            Self::Below
        } else {
            Self::Above
        }
    }

    fn top(self, near_edge: Pixels, height: Pixels) -> Pixels {
        match self {
            Self::Above => near_edge - height,
            Self::Below => near_edge,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Placement {
    side: Side,
    // Keep the trigger column fixed in the scrolling editor's coordinate space.
    x_in_editor: Pixels,
}

pub(super) struct CompletionPopup {
    menu: WeakEntity<CompletionMenu>,
    list: Option<AnyElement>,
    documentation: Option<AnyElement>,
}

impl CompletionPopup {
    pub(super) fn new(menu: WeakEntity<CompletionMenu>) -> Self {
        Self {
            menu,
            list: None,
            documentation: None,
        }
    }
}

impl Element for CompletionPopup {
    type RequestLayoutState = ();
    type PrepaintState = Option<Bounds<Pixels>>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (window.request_layout(Style::default(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.documentation = None;
        let menu = self.menu.upgrade()?;
        let state = menu.read(cx);
        let editor = state.editor.upgrade()?.read(cx);
        let viewport = window.viewport_size();
        let margin = CARET_POPUP_MARGIN;
        let revision = state.revision;
        let dismiss = |window: &Window, cx: &mut App| {
            let menu = self.menu.clone();
            window.defer(cx, move |_, cx| {
                let _ = menu.update(cx, |menu, cx| {
                    if menu.open && menu.revision == revision {
                        menu.dismiss(cx);
                    }
                });
            });
        };

        let Some(geometry) = editor.prepaint_caret_geometry() else {
            dismiss(window, cx);
            return None;
        };
        let input_bounds = geometry.input_bounds();
        let mut caret = geometry.cursor_bounds();
        caret.origin.y += geometry.scroll_offset().y;
        if !caret.intersects(&input_bounds) {
            dismiss(window, cx);
            return None;
        }

        // The list owns the direction and the rectangle next to the caret.
        let anchor = caret_popup_anchor(geometry);
        let above_edge = anchor.y - geometry.line_height() - px(8.);
        let above = (above_edge - margin).max(px(0.));
        let below = (viewport.height - margin - anchor.y).max(px(0.));
        let text_size = (geometry.text_size() - px(0.5)).max(px(1.));
        let row_height = text_size + ROW_HEIGHT_PADDING;
        // editor_popover uses .p_1() and a 1px border on both sides.
        let insets = window.rem_size() * 0.5 + px(2.);
        let list = state.list.clone();
        let count = list.read(cx).delegate().items.len();
        let max_list_height = MAX_LIST_HEIGHT.max(row_height);
        let preferred = max_list_height.min(row_height * count as f32) + insets;
        let editor_x = input_bounds.left() + geometry.scroll_offset().x;
        let placement = state.placement.get().unwrap_or_else(|| Placement {
            side: Side::choose(above, below, preferred),
            x_in_editor: anchor.x - editor_x,
        });
        let (near_edge, available) = match placement.side {
            Side::Above => (above_edge, above),
            Side::Below => (anchor.y, below),
        };
        let safe_right = viewport.width - margin;
        let list_width = editor
            .lsp()
            .completion_menu
            .max_width
            .min(safe_right - margin);
        let list_space = available - insets;
        if list_space < row_height || list_width <= insets {
            dismiss(window, cx);
            return None;
        }
        let list_height = max_list_height.min(list_space.floor().max(row_height));
        state.placement.set(Some(placement));

        let x = (editor_x + placement.x_in_editor).clamp(margin, safe_right - list_width);
        let mut list_panel = editor_popover("completion-menu", cx)
            .debug_selector(|| "completion-menu".into())
            .test_support()
            .font_family(window.text_style().font().family)
            .text_size(text_size)
            .w(list_width)
            .child(List::new(&list).popup_scrollbar().max_h(list_height))
            .into_any_element();
        let list_size = list_panel.layout_as_root(POPUP_SPACE, window, cx);
        let list_bounds = Bounds::new(
            point(x, placement.side.top(near_edge, list_size.height)),
            list_size,
        );
        list_panel.prepaint_at(list_bounds.origin, window, cx);
        self.list = Some(list_panel);

        // Lay out the documentation with the space left by the list. Its size
        // and presence have no effect on the list's position or scroll viewport.
        let documentation = list
            .read(cx)
            .delegate()
            .selected_item()
            .and_then(|item| item.documentation.as_ref())
            .map(|doc| match doc {
                Documentation::String(text) => text,
                Documentation::MarkupContent(markup) => &markup.value,
            })
            .filter(|text| !text.trim().is_empty())
            .cloned();
        let Some(documentation) = documentation else {
            return Some(list_bounds);
        };
        let right = list_bounds.right() + PANEL_GAP;
        let width_to_right = safe_right - right;
        let (doc_x, doc_width, doc_edge, doc_space) = if width_to_right >= MIN_DOCUMENTATION_WIDTH {
            (right, width_to_right.min(list_width), near_edge, available)
        } else {
            let edge = match placement.side {
                Side::Above => list_bounds.top() - PANEL_GAP,
                Side::Below => list_bounds.bottom() + PANEL_GAP,
            };
            (
                x,
                list_width,
                edge,
                available - list_size.height - PANEL_GAP,
            )
        };
        let doc_height = (doc_space - insets).floor();
        if doc_height < row_height {
            return Some(list_bounds);
        }

        let mut doc_panel = editor_popover("completion-documentation", cx)
            .debug_selector(|| "completion-documentation".into())
            .test_support()
            .w(doc_width)
            .child(
                div()
                    .max_h(doc_height)
                    .px_2()
                    .py_0p5()
                    .child(render_markdown("doc", &documentation, window, cx))
                    .overflow_y_popup_scrollbar(),
            )
            .into_any_element();
        let doc_size = doc_panel.layout_as_root(POPUP_SPACE, window, cx);
        let doc_bounds = Bounds::new(
            point(doc_x, placement.side.top(doc_edge, doc_size.height)),
            doc_size,
        );
        doc_panel.prepaint_at(doc_bounds.origin, window, cx);
        self.documentation = Some(doc_panel);

        Some(list_bounds.union(&doc_bounds))
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        bounds: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(bounds) = *bounds else { return };
        let menu = self.menu.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase.capture() && !window.has_active_prompt() && !bounds.contains(&event.position) {
                let _ = menu.update(cx, |menu, cx| menu.dismiss(cx));
            }
        });
        if let Some(list) = &mut self.list {
            list.paint(window, cx);
        }
        if let Some(documentation) = &mut self.documentation {
            documentation.paint(window, cx);
        }
    }
}

impl IntoElement for CompletionPopup {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}
