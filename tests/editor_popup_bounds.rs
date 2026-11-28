use std::{cell::Cell, rc::Rc, time::Duration};

use gpui::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Styled as _, Task, TestAppContext, VisualTestContext, Window,
    deferred, div, point, px, size,
};
use gpui_component::{
    Root,
    input::{CaretAnchoredPopup, Editor, EditorState, HoverProvider, Rope},
};
use lsp_types::{CompletionItem, Documentation, Hover, HoverContents, MarkedString};

struct HoverDocumentation {
    requests: Rc<Cell<usize>>,
    latency: Duration,
}

impl HoverProvider for HoverDocumentation {
    fn hover(
        &self,
        _: &Rope,
        _: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Option<Hover>>> {
        self.requests.set(self.requests.get() + 1);
        let timer = cx.background_executor().timer(self.latency);
        cx.background_executor().spawn(async move {
            timer.await;
            Ok(Some(Hover {
                contents: HoverContents::Scalar(MarkedString::String(
                    "Example documentation".to_owned(),
                )),
                range: None,
            }))
        })
    }
}

struct EditorPane {
    editor: Entity<EditorState>,
    signature: bool,
}

impl Render for EditorPane {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(div().h(px(220.)).flex_none())
            .child(
                div()
                    .id("editor-pane")
                    .debug_selector(|| "editor-pane".into())
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(Editor::new(&self.editor).size_full())
                    .children(self.signature.then(|| {
                        deferred(
                            CaretAnchoredPopup::new(
                                self.editor.downgrade(),
                                |_, height, _, _, _| {
                                    div()
                                        .id("test-signature-help")
                                        .debug_selector(|| "test-signature-help".into())
                                        .h(px(42.))
                                        .max_h(height)
                                        .child("first_symbol(argument)")
                                        .into_any_element()
                                },
                            )
                            .prefer_above(),
                        )
                        .with_priority(gpui_base::POPUP_PRIORITY)
                    })),
            )
            .child(div().h(px(60.)).flex_none())
    }
}

fn setup(
    cx: &mut TestAppContext,
    signature: bool,
    latency: Duration,
) -> (Entity<EditorState>, Rc<Cell<usize>>, &mut VisualTestContext) {
    let requests = Rc::new(Cell::new(0));
    let mut editor = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
        let state = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .default_value("first_symbol(second_symbol);\n".repeat(60));
            state.lsp_mut().hover_provider = Some(Rc::new(HoverDocumentation {
                requests: requests.clone(),
                latency,
            }));
            state.focus(window, cx);
            state
        });
        editor = Some(state.clone());
        let pane = cx.new(|_| EditorPane {
            editor: state,
            signature,
        });
        Root::new(pane, window, cx)
    });
    visual.simulate_resize(size(px(900.), px(470.)));
    draw(visual);
    (editor.unwrap(), requests, visual)
}

fn draw(visual: &mut VisualTestContext) {
    for _ in 0..3 {
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear(cx));
    }
}

fn advance(visual: &mut VisualTestContext, millis: u64) {
    visual.run_until_parked();
    visual
        .executor()
        .advance_clock(Duration::from_millis(millis));
    draw(visual);
}

#[gpui::test]
fn hover_stays_in_the_editor_pane_and_preserves_pointer_handoff(cx: &mut TestAppContext) {
    let (editor, _, visual) = setup(cx, false, Duration::ZERO);
    let symbol = visual.update(|_, cx| editor.read(cx).range_to_bounds(&(0..12)).unwrap());
    visual.simulate_mouse_move(symbol.center(), None, Default::default());
    advance(visual, 150);
    advance(visual, 151);
    let pane = visual.debug_bounds("editor-pane").unwrap();
    let docs = visual.debug_bounds("hover-markdown").unwrap();
    assert!(docs.top() >= symbol.bottom());
    assert!(docs.top() >= pane.top() && docs.bottom() <= pane.bottom());

    let copy = visual.debug_bounds("hover-copy-documentation").unwrap();
    visual.simulate_mouse_move(copy.center(), None, Default::default());
    visual.simulate_click(copy.center(), Default::default());
    assert_eq!(
        visual.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
        Some("Example documentation".to_owned())
    );
}

#[cfg(target_os = "linux")]
#[gpui::test]
fn lsp_hover_fits_inside_the_visible_frame_at_the_right_edge(cx: &mut TestAppContext) {
    let (editor, _, visual) = setup(cx, false, Duration::ZERO);
    visual.update(|window, _| window.request_decorations(gpui::WindowDecorations::Client));
    visual.simulate_resize(size(px(460.), px(470.)));
    draw(visual);

    let symbol = visual.update(|_, cx| editor.read(cx).range_to_bounds(&(13..26)).unwrap());
    visual.simulate_mouse_move(symbol.center(), None, Default::default());
    advance(visual, 150);
    advance(visual, 151);

    let pane = visual.debug_bounds("editor-pane").unwrap();
    let popup = visual.debug_bounds("hover-popover-content").unwrap();
    assert_eq!(popup.right(), pane.right() - px(8.));
    assert_eq!(popup.left(), pane.left() + px(8.));
}

#[gpui::test]
fn long_hover_uses_the_available_space_in_the_editor_pane(cx: &mut TestAppContext) {
    let (editor, _, visual) = setup(cx, false, Duration::ZERO);
    visual.update(|_, cx| {
        editor.update(cx, |editor, cx| {
            editor.present_hover(
                0..12,
                Hover {
                    contents: HoverContents::Scalar(MarkedString::String(
                        "Example documentation.\n\n".repeat(30),
                    )),
                    range: None,
                },
                cx,
            );
        });
    });
    draw(visual);

    let pane = visual.debug_bounds("editor-pane").unwrap();
    let docs = visual.debug_bounds("hover-documentation").unwrap();
    assert!(docs.size.height > pane.size.height * 0.5);
    assert!(docs.top() >= pane.top() && docs.bottom() <= pane.bottom());
}

#[gpui::test]
fn hover_leaving_for_the_top_bar_discards_pending_requests_and_late_results(
    cx: &mut TestAppContext,
) {
    let (editor, requests, visual) = setup(cx, false, Duration::from_millis(250));
    let symbol = visual.update(|_, cx| editor.read(cx).range_to_bounds(&(0..12)).unwrap());
    let pane = visual.debug_bounds("editor-pane").unwrap();
    let bar = point(symbol.center().x, pane.top() - px(2.));

    for (dwell, expected_requests) in [(80, 0), (160, 1)] {
        visual.simulate_mouse_move(symbol.center(), None, Default::default());
        advance(visual, dwell);
        visual.simulate_mouse_move(bar, None, Default::default());
        advance(visual, 500);
        assert_eq!(requests.get(), expected_requests);
        assert!(visual.debug_bounds("hover-markdown").is_none());
    }

    // Move as soon as the response is published, before the popup receives hitboxes.
    let moved_on_response = Rc::new(Cell::new(false));
    let _observation = visual.update(|window, cx| {
        let moved = moved_on_response.clone();
        window.observe(&editor, cx, move |editor, window, cx| {
            if editor.read(cx).hover_popover().is_some() && !moved.replace(true) {
                window.dispatch_event(
                    gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                        position: bar,
                        pressed_button: None,
                        modifiers: Default::default(),
                    }),
                    cx,
                );
            }
        })
    });
    visual.simulate_mouse_move(symbol.center(), None, Default::default());
    advance(visual, 150);
    advance(visual, 251);
    assert!(moved_on_response.get());
    assert!(visual.debug_bounds("hover-markdown").is_none());
}

#[gpui::test]
fn signature_help_flips_below_the_caret_at_the_editor_top(cx: &mut TestAppContext) {
    let (editor, _, visual) = setup(cx, true, Duration::ZERO);
    let pane = visual.debug_bounds("editor-pane").unwrap();
    let first_row = visual.update(|_, cx| editor.read(cx).range_to_bounds(&(0..12)).unwrap());
    let signature = visual.debug_bounds("test-signature-help").unwrap();
    assert!(signature.top() >= first_row.bottom());
    assert!(signature.top() >= pane.top() && signature.bottom() <= pane.bottom());
}

#[gpui::test]
fn completion_list_and_documentation_fit_the_editor_pane(cx: &mut TestAppContext) {
    let (editor, _, visual) = setup(cx, false, Duration::ZERO);
    visual.update(|_, cx| {
        editor.update(cx, |state, cx| {
            let items = (0..24)
                .map(|index| CompletionItem {
                    label: format!("candidate{index:02}"),
                    documentation: Some(Documentation::String("Candidate documentation".into())),
                    ..Default::default()
                })
                .collect();
            state.present_completion_items(0, "", items, cx);
        });
    });
    draw(visual);

    let pane = visual.debug_bounds("editor-pane").unwrap();
    let first_row = visual.update(|_, cx| editor.read(cx).range_to_bounds(&(0..12)).unwrap());
    for selector in ["completion-menu", "completion-documentation"] {
        let popup = visual.debug_bounds(selector).unwrap();
        assert!(
            popup.top() >= first_row.bottom() && popup.bottom() <= pane.bottom(),
            "{selector} at {popup:?} exceeds {pane:?}"
        );
    }
}
