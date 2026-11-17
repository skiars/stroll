#![cfg(target_os = "linux")]

use std::{sync::Arc, time::Duration};

use gpui::{
    AppContext as _, Context, Entity, ImageSource, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, RenderImage, ScrollDelta, ScrollEvent, ScrollHandle,
    ScrollSource, StatefulInteractiveElement as _, Styled as _, TestAppContext, TestDispatcher,
    TouchPhase, VisualTestContext, Window, div, point, px, size,
};
use gpui_component::{
    Root,
    input::{Editor, EditorState},
    text::{TextView, TextViewState},
};

struct ScrollViewport {
    handle: ScrollHandle,
    content: Pixels,
}

impl Render for ScrollViewport {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(
            div()
                .id("touchpad-viewport")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.handle)
                .child(div().w_full().h(self.content)),
        )
    }
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
    });
}

fn advance_frame(cx: &mut VisualTestContext, milliseconds: u64) {
    cx.executor()
        .advance_clock(Duration::from_millis(milliseconds));
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
        window.draw(cx).clear(cx);
    });
}

fn scroll_event(cx: &mut VisualTestContext, delta: f32, phase: TouchPhase) {
    scroll_event_at(cx, point(px(100.), px(100.)), delta, phase);
}

fn scroll_event_at(
    cx: &mut VisualTestContext,
    position: gpui::Point<Pixels>,
    delta: f32,
    phase: TouchPhase,
) {
    send_scroll(cx, position, delta, phase, ScrollSource::Touchpad);
}

fn send_scroll(
    cx: &mut VisualTestContext,
    position: gpui::Point<Pixels>,
    delta: f32,
    phase: TouchPhase,
    source: ScrollSource,
) {
    cx.simulate_event(ScrollEvent {
        position,
        delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
        touch_phase: phase,
        source,
        ..ScrollEvent::default()
    });
}

#[test]
fn fixed_width_markdown_keeps_release_speed_when_packets_arrive_in_a_burst() {
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    let mut document = None;
    let (_, cx) = app.add_window_view(|window, cx| {
        gpui_component::init(cx);
        cx.set_smooth_scrolling(true);
        let text = cx.new(|cx| {
            TextViewState::markdown(
                &concat!(
                    "## 小节\n\n",
                    "固定宽度下的连续滚动，包含需要换行的中文段落和 `inline code`。\n\n",
                    "<img src=\"sample.png\" width=\"200\" height=\"100\">\n\n",
                    "| 项目 | 内容 |\n| --- | --- |\n| 图表 | 带有多列的表格 |\n\n",
                )
                .repeat(20),
                cx,
            )
            .scrollable(true)
        });
        text.read(cx).list_state().set_pixel_aligned_scroll(true);
        document = Some(text.clone());
        let image = Arc::new(RenderImage::new([image::Frame::new(
            image::RgbaImage::new(200, 100),
        )]));
        let preview = cx.new(|_| ScrollDocument { text, image });
        Root::new(preview, window, cx)
    });
    cx.simulate_resize(size(px(420.), px(300.)));
    cx.run_until_parked();
    draw(cx);
    let document = document.unwrap();
    let position = point(px(210.), px(120.));
    let offset = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            let list = document.read(cx).list_state();
            let logical = list.scroll_px_offset_for_scrollbar().y;
            assert!((list.painted_scroll_offset() - window.pixel_snap(logical)).abs() < px(0.01));
            logical
        })
    };

    // Packets in the same batch were 8 ms apart on the device, across a u32 timestamp wrap.
    let start = u32::MAX - 12;
    for packet in 0..5 {
        send_scroll(
            cx,
            position,
            -8.,
            if packet == 0 {
                TouchPhase::Started
            } else {
                TouchPhase::Moved
            },
            ScrollSource::TimedTouchpad(start.wrapping_add(packet * 8)),
        );
    }
    advance_frame(cx, 16);
    let before_release = offset(cx);
    assert!(before_release < px(-25.) && before_release > px(-40.));
    send_scroll(
        cx,
        position,
        0.,
        TouchPhase::Ended,
        ScrollSource::TimedTouchpad(start.wrapping_add(34)),
    );

    // The device moves at 1 px/ms. Residual motion and momentum must total at most 16 px on the first frame after release.
    advance_frame(cx, 16);
    let first = offset(cx);
    assert!((before_release - first).as_f32() > 14.);
    assert!((before_release - first).as_f32() <= 16.1);
    let mut previous = first;
    let mut last_step = before_release - first;
    for _ in 0..20 {
        advance_frame(cx, 16);
        let current = offset(cx);
        let step = previous - current;
        assert!(
            step > px(0.) && step <= last_step + px(0.05),
            "{step:?} after {last_step:?}"
        );
        previous = current;
        last_step = step;
    }

    // After a brief render thread stall, momentum advances by at most 25 ms of motion.
    advance_frame(cx, 90);
    let delayed_step = previous - offset(cx);
    assert!(delayed_step > px(0.) && delayed_step <= last_step * 1.57);
}

struct ScrollDocument {
    text: Entity<TextViewState>,
    image: Arc<RenderImage>,
}

impl Render for ScrollDocument {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let image = self.image.clone();
        TextView::new(&self.text)
            .scrollable(true)
            .image_source(move |_| ImageSource::Render(image.clone()))
            .size_full()
    }
}

fn open<'a>(
    app: &'a mut TestAppContext,
    content: Pixels,
) -> (ScrollHandle, &'a mut VisualTestContext) {
    let handle = ScrollHandle::new();
    let (_, mut cx) = app.add_window_view(|_, cx| {
        cx.set_smooth_scrolling(true);
        ScrollViewport {
            handle: handle.clone(),
            content,
        }
    });
    cx.simulate_resize(size(px(300.), px(200.)));
    draw(&mut cx);
    (handle, cx)
}

#[test]
fn touchpad_input_is_frame_paced_and_flings_after_release() {
    let mut app = TestAppContext::build(TestDispatcher::new(0), None);
    let (handle, mut cx) = open(&mut app, px(10_000.));

    for frame in 0..10 {
        let phase = if frame == 0 {
            TouchPhase::Started
        } else {
            TouchPhase::Moved
        };
        scroll_event(&mut cx, -10., phase);
        cx.executor().advance_clock(Duration::from_millis(8));
    }

    // Raw packets are queued, not applied on receipt.
    assert_eq!(handle.offset().y, px(0.));

    advance_frame(&mut cx, 16);
    let first = handle.offset().y;
    assert!(
        first < px(0.) && first > px(-100.),
        "the first frame must trail the packets, got {first:?}"
    );

    scroll_event(&mut cx, 0., TouchPhase::Ended);
    for _ in 0..8 {
        advance_frame(&mut cx, 16);
    }
    let after_release = handle.offset().y;
    assert!(
        after_release < px(-100.),
        "the gesture must keep moving after the finger lifts, got {after_release:?}"
    );

    for _ in 0..120 {
        advance_frame(&mut cx, 16);
    }
    let stopped = handle.offset().y;
    for _ in 0..8 {
        advance_frame(&mut cx, 16);
    }
    assert_eq!(handle.offset().y, stopped);
}

#[test]
fn touchpad_momentum_stops_at_the_end() {
    let mut app = TestAppContext::build(TestDispatcher::new(0), None);
    let (handle, mut cx) = open(&mut app, px(300.));

    for frame in 0..6 {
        let phase = if frame == 0 {
            TouchPhase::Started
        } else {
            TouchPhase::Moved
        };
        scroll_event(&mut cx, -20., phase);
        cx.executor().advance_clock(Duration::from_millis(8));
    }
    scroll_event(&mut cx, 0., TouchPhase::Ended);

    for _ in 0..80 {
        advance_frame(&mut cx, 16);
    }

    assert_eq!(handle.offset().y, px(-100.));
    let a = handle.offset().y;
    for _ in 0..8 {
        advance_frame(&mut cx, 16);
    }
    assert_eq!(handle.offset().y, a);
}

#[test]
fn a_new_gesture_interrupts_momentum() {
    let mut app = TestAppContext::build(TestDispatcher::new(0), None);
    let (handle, mut cx) = open(&mut app, px(10_000.));

    for frame in 0..6 {
        let phase = if frame == 0 {
            TouchPhase::Started
        } else {
            TouchPhase::Moved
        };
        scroll_event(&mut cx, -20., phase);
        cx.executor().advance_clock(Duration::from_millis(8));
    }
    scroll_event(&mut cx, 0., TouchPhase::Ended);
    for _ in 0..4 {
        advance_frame(&mut cx, 16);
    }
    let before_reverse = handle.offset().y;

    scroll_event(&mut cx, 20., TouchPhase::Started);
    cx.executor().advance_clock(Duration::from_millis(8));
    scroll_event(&mut cx, 20., TouchPhase::Moved);
    advance_frame(&mut cx, 16);
    assert!(handle.offset().y > before_reverse);
}

#[test]
fn reduced_motion_keeps_touchpad_input_direct() {
    let handle = ScrollHandle::new();
    let mut app = TestAppContext::build(TestDispatcher::new(0), None);
    let (_, mut cx) = app.add_window_view(|_, cx| {
        cx.set_smooth_scrolling(true);
        cx.set_reduce_motion(true);
        ScrollViewport {
            handle: handle.clone(),
            content: px(10_000.),
        }
    });
    cx.simulate_resize(size(px(300.), px(200.)));
    draw(&mut cx);

    scroll_event(&mut cx, -24., TouchPhase::Moved);
    assert_eq!(handle.offset().y, px(-24.));
    advance_frame(&mut cx, 32);
    assert_eq!(handle.offset().y, px(-24.));
}

#[test]
fn pointer_precise_scrolling_is_not_resampled() {
    let handle = ScrollHandle::new();
    let mut app = TestAppContext::build(TestDispatcher::new(0), None);
    let (_, mut cx) = app.add_window_view(|_, cx| {
        cx.set_smooth_scrolling(true);
        ScrollViewport {
            handle: handle.clone(),
            content: px(10_000.),
        }
    });
    cx.simulate_resize(size(px(300.), px(200.)));
    draw(&mut cx);

    cx.simulate_event(ScrollEvent {
        position: point(px(100.), px(100.)),
        delta: ScrollDelta::Pixels(point(px(0.), px(-24.))),
        touch_phase: TouchPhase::Moved,
        source: ScrollSource::Pointer,
        ..ScrollEvent::default()
    });
    assert_eq!(handle.offset().y, px(-24.));
    advance_frame(&mut cx, 32);
    assert_eq!(handle.offset().y, px(-24.));
}

struct EditorAndDocument {
    editor: Entity<EditorState>,
    document: Entity<TextViewState>,
}

impl Render for EditorAndDocument {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(
                div()
                    .h(px(180.))
                    .w_full()
                    .child(Editor::new(&self.editor).size_full()),
            )
            .child(
                div()
                    .h(px(180.))
                    .w_full()
                    .child(TextView::new(&self.document).scrollable(true).size_full()),
            )
    }
}

#[test]
fn touchpad_scrolling_reaches_the_editor_and_document_viewports() {
    let mut app = TestAppContext::build(TestDispatcher::new(0), None);
    let mut entities = None;
    let (_, mut cx) = app.add_window_view(|window, cx| {
        gpui_component::init(cx);
        cx.set_smooth_scrolling(true);
        let editor =
            cx.new(|cx| EditorState::new(window, cx).default_value("editor line\n".repeat(300)));
        let document = cx.new(|cx| {
            TextViewState::markdown(&"document paragraph.\n\n".repeat(200), cx).scrollable(true)
        });
        entities = Some((editor.clone(), document.clone()));
        Root::new(
            cx.new(|_| EditorAndDocument { editor, document }),
            window,
            cx,
        )
    });
    cx.simulate_resize(size(px(300.), px(400.)));
    draw(&mut cx);
    draw(&mut cx);
    let (editor, document) = entities.unwrap();

    for frame in 0..4 {
        let phase = if frame == 0 {
            TouchPhase::Started
        } else {
            TouchPhase::Moved
        };
        scroll_event_at(&mut cx, point(px(100.), px(90.)), -20., phase);
        cx.executor().advance_clock(Duration::from_millis(8));
    }
    advance_frame(&mut cx, 16);
    let editor_offset = cx.update(|_, cx| editor.read(cx).scroll_offset().y);
    assert!(
        editor_offset < px(0.),
        "editor did not scroll: {editor_offset:?}"
    );

    for frame in 0..4 {
        let phase = if frame == 0 {
            TouchPhase::Started
        } else {
            TouchPhase::Moved
        };
        scroll_event_at(&mut cx, point(px(100.), px(270.)), -20., phase);
        cx.executor().advance_clock(Duration::from_millis(8));
    }
    advance_frame(&mut cx, 16);
    let document_offset = cx.update(|_, cx| {
        document
            .read(cx)
            .list_state()
            .scroll_px_offset_for_scrollbar()
            .y
    });
    assert!(
        document_offset < px(0.),
        "document did not scroll: {document_offset:?}"
    );
}

#[test]
fn disabling_smooth_scrolling_keeps_touchpad_input_direct() {
    let handle = ScrollHandle::new();
    let mut app = TestAppContext::build(TestDispatcher::new(0), None);
    let (_, mut cx) = app.add_window_view(|_, cx| {
        cx.set_smooth_scrolling(false);
        ScrollViewport {
            handle: handle.clone(),
            content: px(10_000.),
        }
    });
    cx.simulate_resize(size(px(300.), px(200.)));
    draw(&mut cx);

    scroll_event(&mut cx, -24., TouchPhase::Moved);
    assert_eq!(handle.offset().y, px(-24.));
    advance_frame(&mut cx, 32);
    assert_eq!(handle.offset().y, px(-24.));
}

#[test]
fn a_single_packet_release_has_no_momentum() {
    let mut app = TestAppContext::build(TestDispatcher::new(0), None);
    let (handle, mut cx) = open(&mut app, px(10_000.));

    scroll_event(&mut cx, -10., TouchPhase::Started);
    cx.executor().advance_clock(Duration::from_millis(8));
    scroll_event(&mut cx, 0., TouchPhase::Ended);
    for _ in 0..40 {
        advance_frame(&mut cx, 16);
    }

    assert_eq!(handle.offset().y, px(-10.));
}
