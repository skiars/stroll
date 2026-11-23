use std::{cell::Cell, rc::Rc, time::Duration};

use gpui::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, ParentElement as _, Render,
    Styled as _, Task, TestAppContext, VisualTestContext, Window, div, point, px, size,
};
use gpui_component::{
    Root,
    input::{Editor, EditorState, HoverProvider, Rope},
};
use lsp_types::{Hover, HoverContents, MarkedString};

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
                    .child(Editor::new(&self.editor).size_full()),
            )
            .child(div().h(px(60.)).flex_none())
    }
}

fn setup(
    cx: &mut TestAppContext,
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
        let pane = cx.new(|_| EditorPane { editor: state });
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
fn hover_leaving_for_the_top_bar_discards_pending_requests_and_late_results(
    cx: &mut TestAppContext,
) {
    let (editor, requests, visual) = setup(cx, Duration::from_millis(250));
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
