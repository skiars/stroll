use gpui_base::{PopupFrame, PopupMotion, PopupMotionKind};
use rust_i18n::t;
use std::{cell::Cell, ops::Range, rc::Rc, time::Instant};

use gpui::{
    AnyElement, App, AppContext as _, AvailableSpace, Bounds, ClickEvent, Element, ElementId,
    Entity, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, Pixels, Point, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, StyleRefinement, Styled, WeakEntity, Window, deferred, div,
    linear_color_stop, linear_gradient, point, prelude::FluentBuilder as _, px, relative, rems,
};

use crate::{
    ActiveTheme as _, Colorize as _, Icon, Sizable as _, StyledExt, ThemeStyled as _,
    clipboard::Clipboard,
    highlighter::{DiagnosticEntry, diagnostic_foreground},
    input::{
        EditorState, HoverPopoverState,
        popovers::{
            CodeActionMenu, DOCUMENTATION_FONT_SIZE, QUICK_FIX_BUTTON_SIZE,
            caret_anchored::{CARET_POPUP_MARGIN, editor_popup_vertical_bounds},
            quick_fix_button, render_markdown_with_code_style,
        },
    },
    scroll::{ScrollbarMode, popup_scrollbar},
    text::{MarkdownExtensions, MarkdownNode, TextView},
};

pub struct HoverPopover {
    editor: Entity<EditorState>,
    content: HoverPopoverState,
    motion: PopupMotion,
    scroll: ScrollHandle,
    pointer_down: Rc<Cell<bool>>,
    last_trigger_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    closing: bool,
    actions: Option<Entity<CodeActionMenu>>,
    position: Rc<Cell<Option<Point<Pixels>>>>,
    documentation: Option<(SharedString, MarkdownExtensions)>,
}

impl HoverPopover {
    pub(crate) fn new(
        editor: Entity<EditorState>,
        content: HoverPopoverState,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|_| Self {
            documentation: None,
            editor,
            content,
            motion: PopupMotion::new(PopupMotionKind::Hover, Instant::now()),
            scroll: ScrollHandle::new(),
            pointer_down: Rc::default(),
            last_trigger_bounds: Rc::default(),
            closing: false,
            actions: None,
            position: Rc::default(),
        })
    }

    pub(crate) fn scroll_handle(&self) -> &ScrollHandle {
        &self.scroll
    }

    pub(crate) fn set_code_actions(
        &mut self,
        actions: Option<Entity<CodeActionMenu>>,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.actions != actions {
            self.actions = actions;
            cx.notify();
        }
    }

    pub(crate) fn needs_update(&self, content: &HoverPopoverState) -> bool {
        self.closing
            || ((!self.content.diagnostics.is_empty() || !content.diagnostics.is_empty())
                && !Rc::ptr_eq(&self.content.diagnostics, &content.diagnostics))
            || self.content.symbol_range != content.symbol_range
            || self.content.hover != content.hover
            || self.content.documentation != content.documentation
    }

    pub(crate) fn update_hover(
        &mut self,
        content: HoverPopoverState,
        cx: &mut gpui::Context<Self>,
    ) {
        let appended_documentation = !content.diagnostics.is_empty()
            && Rc::ptr_eq(&self.content.diagnostics, &content.diagnostics)
            && self.content.symbol_range == content.symbol_range
            && self.content.hover.is_none();
        let enriched = self.content.hover.is_some()
            && self.content.hover == content.hover
            && self.content.symbol_range == content.symbol_range
            && self.content.documentation != content.documentation;
        if self.content.documentation != content.documentation {
            self.documentation = None;
        }
        if !appended_documentation && !enriched {
            self.scroll.set_offset(point(px(0.), px(0.)));
            self.position.set(None);
        }
        self.content = content;
        self.closing = false;
        self.motion.retarget(true, Instant::now());
        cx.notify();
    }

    pub(crate) fn close(&mut self, cx: &mut gpui::Context<Self>) {
        self.closing = true;
        self.motion.retarget(false, Instant::now());
        cx.notify();
    }

    pub(crate) fn closing(&self) -> bool {
        self.closing
    }

    pub(crate) fn visible(&self, window: &Window, cx: &App) -> bool {
        !self.closing || self.motion.opacity(window, cx) > 0.
    }
}

impl Render for HoverPopover {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let document_text = self
            .content
            .hover
            .as_ref()
            .map(|hover| match &hover.contents {
                lsp_types::HoverContents::Scalar(scalar) => match scalar {
                    lsp_types::MarkedString::String(s) => s.clone(),
                    lsp_types::MarkedString::LanguageString(ls) => ls.value.clone(),
                },
                lsp_types::HoverContents::Array(arr) => arr
                    .iter()
                    .map(|item| match item {
                        lsp_types::MarkedString::String(s) => s.as_str(),
                        lsp_types::MarkedString::LanguageString(ls) => ls.value.as_str(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n"),
                lsp_types::HoverContents::Markup(markup) => markup.value.clone(),
            })
            .map(SharedString::from);
        let literal = self.content.hover.as_ref().is_some_and(|hover| {
            matches!(&hover.contents, lsp_types::HoverContents::Markup(markup)
                if markup.kind == lsp_types::MarkupKind::PlainText)
        });
        let documentation = document_text.as_ref().map(|text| {
            if literal {
                let mut fence = "```".to_owned();
                while text.contains(&fence) {
                    fence.push('`');
                }
                format!("{fence}\n{text}\n{fence}").into()
            } else {
                text.clone()
            }
        });
        let copy_text = document_text.filter(|text| !text.trim().is_empty());
        let copy_button_id = ("copy-hover-documentation", self.content.symbol_range.start);
        let has_documentation = documentation.is_some();
        // Nested Markdown renderers run under the prose style, so capture the
        // editor font here and pass it through for all hover code.
        let code_font = window.text_style().font().family;
        if self
            .documentation
            .as_ref()
            .is_some_and(|(font, _)| *font != code_font)
        {
            self.documentation = None;
        }
        let editor = self.editor.downgrade();
        let (_, extensions) = self.documentation.get_or_insert_with(|| {
            (
                code_font.clone(),
                documentation_extensions(&self.content, code_font.clone(), editor.clone()),
            )
        });
        let extensions = extensions.clone();
        let plaintext = self
            .content
            .documentation
            .as_ref()
            .and_then(|doc| doc.plaintext.clone());
        let diagnostics = self.content.diagnostics.clone();
        let actions = self.actions.clone();
        let has_actions = actions.is_some();
        let code_languages = self
            .editor
            .read(cx)
            .lsp()
            .hover_provider
            .as_ref()
            .and_then(|provider| provider.code_languages());
        let can_fix = self.editor.read(cx).can_open_code_actions();

        let mut popover = Popover::new(
            "hover-popover",
            self.editor.clone(),
            self.content.symbol_range.clone(),
            move |_, cx| {
                let mut content = div().flex().flex_col();
                for (index, diagnostic) in diagnostics.iter().enumerate() {
                    let last =
                        index + 1 == diagnostics.len() && documentation.is_none() && !has_actions;
                    content = content.child(render_diagnostic(
                        index,
                        last,
                        diagnostic,
                        (can_fix && index == 0).then(|| editor.clone()),
                        cx,
                    ));
                }
                if let Some(actions) = &actions {
                    content = content.child(actions.clone());
                }
                if let Some(documentation) = &documentation {
                    if has_actions {
                        content = content.child(div().mx(px(12.)).h(px(1.)).bg(cx.theme().border));
                    }
                    let copy_inset = px(10.);
                    let copy_button_size = DOCUMENTATION_FONT_SIZE * 1.6;
                    content = content.child(
                        div()
                            .debug_selector(|| "hover-markdown".into())
                            .relative()
                            .px(px(12.))
                            .py(px(8.))
                            .when(copy_text.is_some(), |view| {
                                view.pr(copy_inset + copy_button_size + px(6.))
                            })
                            .child(
                                render_hover_markdown(
                                    "message",
                                    documentation.clone(),
                                    &code_font,
                                    &editor,
                                    cx,
                                )
                                .code_languages(code_languages.clone())
                                .markdown_extensions(extensions.clone()),
                            )
                            .when_some(plaintext.clone(), |view, text| {
                                view.child(
                                    div()
                                        .text_color(translation_foreground(cx))
                                        .whitespace_normal()
                                        .child(text),
                                )
                            })
                            .when_some(copy_text.clone(), |view, text| {
                                let label = t!("Input.Copy Documentation");
                                view.child(
                                    div()
                                        .absolute()
                                        .top(copy_inset)
                                        .right(copy_inset)
                                        .debug_selector(|| "hover-copy-documentation".into())
                                        .child(
                                            Clipboard::new(copy_button_id)
                                                .with_size(copy_button_size)
                                                .button_style(DOCUMENTATION_FONT_SIZE, px(3.))
                                                .value(text)
                                                .tooltip(label.clone())
                                                .accessibility_label(label),
                                        ),
                                )
                            }),
                    );
                }
                content
            },
        );
        popover = popover.track_scroll(&self.scroll);
        popover.pointer_down = self.pointer_down.clone();
        popover.last_trigger_bounds = self.last_trigger_bounds.clone();
        popover.closing = self.closing;
        popover.actions = has_actions;
        popover.has_documentation = has_documentation;
        popover.prefers_natural_width = self
            .content
            .documentation
            .as_ref()
            .is_some_and(|doc| !doc.paragraphs.is_empty());
        if has_actions || (can_fix && !self.content.diagnostics.is_empty()) {
            popover.width_limit = px(480.)..px(480.);
        }
        if self.content.hover.is_some() || !self.content.diagnostics.is_empty() {
            popover.position = Some(self.position.clone());
        }
        popover.style.opacity = Some(self.motion.opacity(window, cx));
        deferred(PopupFrame::new(popover, self.closing)).with_priority(gpui_base::POPUP_PRIORITY)
    }
}

fn render_hover_markdown(
    id: impl Into<ElementId>,
    markdown: impl Into<SharedString>,
    code_font: &SharedString,
    editor: &WeakEntity<EditorState>,
    cx: &mut App,
) -> TextView {
    let editor = editor.clone();
    render_markdown_with_code_style(
        id,
        markdown,
        code_font.clone(),
        cx.theme().hover_inline_code_size,
        cx,
    )
    .on_link_click(move |url, event, window, cx| {
        let activate = match event {
            ClickEvent::Mouse(click) => {
                matches!(click.up.button, MouseButton::Left | MouseButton::Middle)
            }
            ClickEvent::Keyboard(_) => true,
            ClickEvent::Touch(click) => !click.long_press,
        };
        if !activate {
            return;
        }
        if let Ok(uri) = url.parse::<lsp_types::Uri>()
            && uri
                .scheme()
                .is_some_and(|scheme| scheme.as_str().eq_ignore_ascii_case("file"))
            && let Some(handler) = editor
                .upgrade()
                .and_then(|editor| editor.read(cx).lsp().show_document.clone())
            && handler(
                &lsp_types::ShowDocumentParams {
                    uri,
                    external: Some(false),
                    take_focus: Some(true),
                    selection: None,
                },
                window,
                cx,
            )
        {
            return;
        }
        cx.open_url(url);
    })
}

fn documentation_extensions(
    content: &HoverPopoverState,
    code_font: SharedString,
    editor: WeakEntity<EditorState>,
) -> MarkdownExtensions {
    use crate::input::HoverParagraph;
    use crate::text::markdown_ast::Node;

    let Some(documentation) = content
        .documentation
        .as_ref()
        .filter(|doc| !doc.paragraphs.is_empty())
    else {
        return MarkdownExtensions::default();
    };
    const PAIRED: &str = "hover-enriched-paragraph";
    let paragraphs = documentation.paragraphs.clone();
    MarkdownExtensions::default()
        .block_parser(move |node, cx| {
            let Node::Paragraph(paragraph) = node else {
                return None;
            };
            let offset = cx.offset() + paragraph.position.as_ref()?.start.offset;
            let pair = paragraphs.get(&offset)?.clone();
            Some(
                MarkdownNode::new(PAIRED, (offset, pair))
                    .text(node.to_string())
                    .markdown(cx.node_source(node)?),
            )
        })
        .block_renderer(PAIRED, move |node, window, cx| {
            let (offset, pair) = node.data::<(usize, HoverParagraph)>().unwrap();
            div()
                .id(("hover-paragraph", *offset))
                .flex()
                .flex_wrap()
                .gap_x(rems(0.8))
                .gap_y(rems(0.35))
                .child(hover_prose(
                    "hover-original-paragraph",
                    *offset,
                    pair.original.clone(),
                    None,
                    &code_font,
                    &editor,
                    window,
                    cx,
                ))
                .child(hover_prose(
                    "hover-translated-paragraph",
                    *offset,
                    pair.translation.clone(),
                    Some(translation_foreground(cx)),
                    &code_font,
                    &editor,
                    window,
                    cx,
                ))
        })
}

fn translation_foreground(cx: &App) -> gpui::Hsla {
    let theme = cx.theme();
    theme.foreground.mix_oklab(theme.muted_foreground, 0.8)
}

/// Give the flex item its natural width. The regular TextView fills its
/// containing block, which would otherwise give both items the entire row.
fn hover_prose(
    id: &'static str,
    offset: usize,
    text: SharedString,
    color: Option<gpui::Hsla>,
    code_font: &SharedString,
    editor: &WeakEntity<EditorState>,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement {
    let width = render_hover_markdown(
        format!("{id}-measure-{offset}"),
        text.clone(),
        code_font,
        editor,
        cx,
    )
    .into_any_element()
    .layout_as_root(
        gpui::size(AvailableSpace::MaxContent, AvailableSpace::MaxContent),
        window,
        cx,
    )
    .width;
    div()
        .debug_selector(move || id.into())
        .w(width)
        .max_w_full()
        .flex_none()
        .child(
            render_hover_markdown((id, offset), text, code_font, editor, cx)
                .when_some(color, |view, color| view.text_color(color)),
        )
}

fn render_diagnostic(
    index: usize,
    last: bool,
    diagnostic: &DiagnosticEntry,
    editor: Option<gpui::WeakEntity<EditorState>>,
    cx: &App,
) -> impl IntoElement {
    let tone = diagnostic_foreground(diagnostic.severity, cx);
    let icon = Icon::new(gpui_kit_assets::IconName::CircleAlert)
        .size(px(16.))
        .text_color(tone);
    // LSP messages are plain text. Escape markup while keeping the TextView
    // used for selection elsewhere in this surface.
    let message = diagnostic
        .message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\n', "<br>");
    let mut row = div()
        .id(("hover-diagnostic", index))
        .flex()
        .flex_col()
        .flex_none()
        .px(px(12.))
        .py(px(10.))
        .bg(linear_gradient(
            90.,
            linear_color_stop(tone.alpha(0.12), 0.),
            linear_color_stop(tone.alpha(0.), 0.75),
        ))
        .child(
            div()
                .flex()
                .items_start()
                .gap(px(8.))
                .child(div().flex_none().pt(px(2.)).child(icon))
                .child(
                    TextView::html(("diagnostic-message", index), message)
                        .w_full()
                        .min_w(px(0.))
                        .flex_1()
                        .text_size(px(12.5))
                        .line_height(relative(1.6))
                        .selectable(true),
                )
                .children(editor.map(|editor| {
                    let range = diagnostic.range.clone();
                    div()
                        .flex_none()
                        .debug_selector(|| "diagnostic-quick-fix-button".into())
                        .child(
                            quick_fix_button(
                                ("diagnostic-quick-fix", index),
                                t!("Input.Quick Fix").into(),
                                tone,
                                cx,
                            )
                            .on_click(move |_, window, cx| {
                                let _ = editor.update(cx, |editor, cx| {
                                    editor.focus(window, cx);
                                    editor.open_code_actions(range.clone(), window, cx);
                                });
                            }),
                        )
                })),
        );
    // The scroll viewport clips to a rectangle. Keep the filled corners
    // within the popup's ten-pixel radius and one-pixel border.
    if index == 0 {
        row = row.rounded_tl(px(9.)).rounded_tr(px(9.));
    }
    if last {
        row = row.rounded_bl(px(9.)).rounded_br(px(9.));
    }
    if let Some(source) = &diagnostic.source {
        row.child(
            div()
                .pl(px(24.))
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child(source.clone()),
        )
    } else {
        row
    }
}

pub(crate) struct Popover {
    id: ElementId,
    style: StyleRefinement,
    editor: Entity<EditorState>,
    range: Range<usize>,
    scroll: ScrollHandle,
    pointer_down: Rc<Cell<bool>>,
    last_trigger_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    closing: bool,
    actions: bool,
    has_documentation: bool,
    prefers_natural_width: bool,
    position: Option<Rc<Cell<Option<Point<Pixels>>>>>,
    width_limit: Range<Pixels>,
    content_builder: Box<dyn Fn(&mut Window, &mut App) -> AnyElement>,
}

impl Styled for Popover {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl Popover {
    pub fn new<F, E>(
        id: impl Into<ElementId>,
        editor: Entity<EditorState>,
        range: Range<usize>,
        f: F,
    ) -> Self
    where
        F: Fn(&mut Window, &mut App) -> E + 'static,
        E: IntoElement,
    {
        Self {
            id: id.into(),
            editor,
            range,
            scroll: ScrollHandle::new(),
            pointer_down: Rc::default(),
            last_trigger_bounds: Rc::default(),
            closing: false,
            actions: false,
            has_documentation: false,
            prefers_natural_width: false,
            position: None,
            style: StyleRefinement::default(),
            width_limit: px(440.)..px(560.),
            content_builder: Box::new(move |window, cx| (f)(window, cx).into_any_element()),
        }
    }

    pub(crate) fn track_scroll(mut self, scroll: &ScrollHandle) -> Self {
        self.scroll = scroll.clone();
        self
    }

    /// Get the bounds of the range in the editor, if it is visible.
    fn trigger_bounds(&self, cx: &App) -> Option<Bounds<Pixels>> {
        let editor = self.editor.read(cx);
        if self.closing {
            return self.last_trigger_bounds.get();
        }
        let bounds = editor.visible_text_range_bounds(&self.range);
        if bounds.is_none() || self.actions || editor.hover_popover().is_some() {
            self.last_trigger_bounds.set(bounds);
            bounds
        } else {
            None
        }
    }
}

impl IntoElement for Popover {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

pub(crate) struct PopoverPlacement {
    bounds: Bounds<Pixels>,
    trigger_bounds: Bounds<Pixels>,
    element: AnyElement,
}

impl Element for Popover {
    type RequestLayoutState = ();
    type PrepaintState = Option<PopoverPlacement>;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        (window.request_layout(gpui::Style::default(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let trigger_bounds = self.trigger_bounds(cx)?;
        let viewport = window.viewport_size();
        let vertical = editor_popup_vertical_bounds(self.editor.read(cx), window);
        let margin = CARET_POPUP_MARGIN;
        let max_width = self.width_limit.end.min(viewport.width - margin * 2);
        let height = vertical.end - vertical.start;
        if max_width <= px(0.) || height <= px(2.) {
            return None;
        }

        let above = trigger_bounds.top().clamp(vertical.start, vertical.end) - vertical.start;
        let below = vertical.end - trigger_bounds.bottom().clamp(vertical.start, vertical.end);
        // Leave room for the two borders around the scroll viewport.
        let pinned = self
            .position
            .as_ref()
            .and_then(|position| self.actions.then(|| position.get()).flatten())
            .filter(|pos| pos.y >= vertical.start && pos.y < vertical.end - px(2.));
        let max_height = (viewport.height * 0.5)
            .min(px(360.))
            .min(pinned.map_or(above.max(below), |pos| vertical.end - pos.y) - px(2.));
        if max_height <= px(0.) {
            return None;
        }

        let mut popover = div()
            .id("hover-popover-content")
            .flex_none()
            .occlude()
            .popover_style(cx)
            .max_w(max_width)
            .min_w(self.width_limit.start.min(max_width))
            .refine_style(&self.style)
            .child(
                div()
                    .id("hover-documentation")
                    .debug_selector(|| "hover-documentation".into())
                    .max_h(max_height)
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .text_size(px(12.5))
                    .line_height(relative(1.6))
                    .child((self.content_builder)(window, cx)),
            )
            .relative()
            .child(popup_scrollbar(&self.scroll).mode(ScrollbarMode::Always))
            .into_any_element();

        let available = if self.prefers_natural_width {
            gpui::size(AvailableSpace::MaxContent, AvailableSpace::MinContent)
        } else {
            AvailableSpace::min_size()
        };
        let popover_size = popover.layout_as_root(available, window, cx);
        if popover_size.height > height {
            return None;
        }
        let origin = pinned.unwrap_or_else(|| {
            let y = if popover_size.height <= above {
                trigger_bounds.top() - popover_size.height
            } else {
                trigger_bounds.bottom()
            };
            point(trigger_bounds.left(), y)
        });
        let origin = point(
            origin.x.clamp(
                margin,
                (viewport.width - popover_size.width - margin).max(margin),
            ),
            origin
                .y
                .clamp(vertical.start, vertical.end - popover_size.height),
        );
        if let Some(position) = &self.position {
            position.set(Some(origin));
        }
        popover.prepaint_at(origin, window, cx);
        Some(PopoverPlacement {
            bounds: Bounds::new(origin, popover_size),
            trigger_bounds,
            element: popover,
        })
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        placement: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(placement) = placement else { return };
        let bounds = placement.bounds;
        placement.element.paint(window, cx);
        if !self.closing
            && let Some(hover) = self.editor.read(cx).hover_popover()
            && hover.symbol_range == self.range
        {
            hover.mark_presented();
        }

        let editor = self.editor.clone();
        let pointer_down = self.pointer_down.clone();
        let actions = self.actions;
        window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
            if !phase.capture() {
                return;
            }
            if !bounds.contains(&event.position) {
                if actions
                    && editor
                        .read(cx)
                        .code_action_indicator_bounds(QUICK_FIX_BUTTON_SIZE)
                        .is_some_and(|bulb| bulb.contains(&event.position))
                {
                    return;
                }
                editor.update(cx, |editor, cx| {
                    editor.dismiss_code_action_overlay(cx);
                    editor.clear_hover_state(cx);
                });
            } else if event.button == MouseButton::Left {
                pointer_down.set(true);
            }
        });

        if self.actions {
            let editor = self.editor.clone();
            window.on_mouse_event(move |event: &gpui::ScrollEvent, phase, _, cx| {
                if phase.capture()
                    && !bounds.contains(&event.position)
                    && editor.read(cx).input_bounds().contains(&event.position)
                {
                    editor.update(cx, |editor, cx| editor.dismiss_code_action_overlay(cx));
                }
            });
        }
        let trigger_bounds = placement.trigger_bounds;
        let keep_open_region = trigger_bounds.union(&bounds);
        let editor = self.editor.clone();
        let pointer_down = self.pointer_down.clone();
        let has_documentation = self.has_documentation;
        let symbol_range = self.range.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
            if !phase.capture() {
                return;
            }
            if !event.dragging() {
                pointer_down.set(false);
            }
            if !pointer_down.get() && !keep_open_region.contains(&event.position) {
                editor.update(cx, |editor, cx| editor.clear_hover_state(cx));
            } else if has_documentation {
                let handing_off = !event.dragging()
                    && !event.modifiers.alt
                    && !event.modifiers.secondary(cx)
                    && !trigger_bounds.contains(&event.position)
                    && !bounds.contains(&event.position);
                editor.read(cx).set_hover_pointer_handoff(
                    &symbol_range,
                    handing_off.then_some(event.position),
                );
            }
        });

        // Keep the scroll viewport mounted while dragging the thumb, even if
        // the pointer travels outside the popup before the button is released.
        let pointer_down = self.pointer_down.clone();
        let editor = self.editor.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
            if phase.capture()
                && event.button == MouseButton::Left
                && pointer_down.replace(false)
                && !keep_open_region.contains(&event.position)
            {
                editor.update(cx, |editor, cx| editor.clear_hover_state(cx));
            }
        });
    }
}
