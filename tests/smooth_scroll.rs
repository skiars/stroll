#[path = "../src/test_support.rs"]
mod test_support;

use std::{cell::Cell, rc::Rc, time::Duration};

use gpui::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ListAlignment,
    ListOffset, ListState, ParentElement as _, Pixels, Render, ScrollDelta, ScrollEvent,
    ScrollHandle, ScrollSource, StatefulInteractiveElement as _, Styled as _, TestAppContext,
    TestDispatcher, UniformListScrollHandle, VisualTestContext, Window, div, list, point, px,
    uniform_list,
};
use gpui_component::{
    Root,
    diff::{Diff, DiffEvent, DiffFile, DiffLinePosition, DiffSide, DiffState},
    input::{Editor, EditorState},
    scroll::ScrollableMask,
    text::{TextView, TextViewState},
};

struct ScrollViews {
    outer: ScrollHandle,
    uniform: UniformListScrollHandle,
    rows: ListState,
    horizontal: ScrollHandle,
    editor: Entity<EditorState>,
    diff: Entity<DiffState>,
    document: Entity<TextViewState>,
}

impl Render for ScrollViews {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("outer")
            .w(px(300.))
            .h(px(500.))
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .track_scroll(&self.outer)
            .child(
                uniform_list("uniform", 100, |range, _, _| {
                    range.map(|_| div().h(px(20.))).collect::<Vec<_>>()
                })
                .h(px(80.))
                .flex_none()
                .track_scroll(&self.uniform),
            )
            .child(
                list(self.rows.clone(), |_, _, _| {
                    div().h(px(20.)).into_any_element()
                })
                .h(px(80.))
                .flex_none(),
            )
            .child(
                div()
                    .relative()
                    .h(px(80.))
                    .flex_none()
                    .child(
                        div()
                            .id("horizontal")
                            .size_full()
                            .overflow_x_scroll()
                            .restrict_scroll_to_axis()
                            .track_scroll(&self.horizontal)
                            .child(div().w(px(900.)).h_full().flex_none()),
                    )
                    .child(ScrollableMask::vertical_wheel(&self.horizontal)),
            )
            .child(
                div()
                    .h(px(80.))
                    .flex_none()
                    .child(Editor::new(&self.editor).size_full()),
            )
            .child(
                div()
                    .h(px(80.))
                    .flex_none()
                    .child(Diff::new(&self.diff).render_preamble(|_, _| div().h(px(90.)))),
            )
            .child(
                div().h(px(80.)).flex_none().child(
                    TextView::new(&self.document)
                        .scrollable(true)
                        .wheel_line_height(px(30.))
                        .size_full(),
                ),
            )
            .child(div().h(px(700.)).flex_none())
    }
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.draw(cx).clear(cx));
}

fn advance_frame(cx: &mut VisualTestContext, milliseconds: u64) {
    cx.executor()
        .advance_clock(Duration::from_millis(milliseconds));
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
        window.draw(cx).clear(cx);
    });
}

#[gpui::test]
fn editor_selection_autoscroll_stops_after_a_missing_mouse_up(cx: &mut TestAppContext) {
    use gpui::MouseButton;

    struct EditorView(Entity<EditorState>);
    impl Render for EditorView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            Editor::new(&self.0).size_full()
        }
    }

    let mut editor = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        let state = cx.new(|cx| {
            EditorState::new(window, cx).default_value("some text on each line\n".repeat(200))
        });
        editor = Some(state.clone());
        let view = cx.new(|_| EditorView(state));
        Root::new(view, window, cx)
    });
    let editor = editor.unwrap();
    cx.simulate_resize(gpui::size(px(400.), px(200.)));
    draw(cx);
    let bounds = cx.update(|_, cx| editor.read(cx).input_bounds());
    let start = point(bounds.left() + px(12.), bounds.top() + px(9.));
    let edge = point(start.x, bounds.bottom() + px(20.));
    cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
    cx.simulate_mouse_move(edge, MouseButton::Left, Default::default());
    let snapshot = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            let editor = editor.read(cx);
            (editor.selected_range(), editor.scroll_offset())
        })
    };
    let before_scroll = snapshot(cx);
    advance_frame(cx, 32);
    let during_drag = snapshot(cx);
    assert!(!during_drag.0.is_empty());
    assert!(during_drag.1.y < before_scroll.1.y);

    cx.simulate_mouse_move(start, None, Default::default());
    advance_frame(cx, 64);
    assert_eq!(snapshot(cx), during_drag);
    cx.simulate_mouse_move(edge, MouseButton::Left, Default::default());
    advance_frame(cx, 64);
    assert_eq!(snapshot(cx), during_drag);
}

fn finish_scroll(cx: &mut VisualTestContext) {
    for _ in 0..16 {
        advance_frame(cx, 16);
    }
}

fn scroll(cx: &mut VisualTestContext, y: Pixels, delta: ScrollDelta) {
    cx.simulate_event(ScrollEvent {
        position: point(px(100.), y),
        delta,
        ..ScrollEvent::default()
    });
}

#[gpui::test]
fn editor_navigation_retargets_and_yields_to_direct_input(cx: &mut TestAppContext) {
    struct EditorView(Entity<EditorState>);
    impl Render for EditorView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            Editor::new(&self.0).size_full()
        }
    }

    let mut editor = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        cx.set_smooth_scrolling(true);
        cx.set_reduce_motion(false);
        let state = cx.new(|cx| {
            EditorState::new(window, cx)
                .soft_wrap(true)
                .default_value(format!("{}\nshort\n", "wrapped words ".repeat(32)).repeat(45))
        });
        state.update(cx, |state, cx| state.focus(window, cx));
        editor = Some(state.clone());
        Root::new(cx.new(|_| EditorView(state)), window, cx)
    });
    let editor = editor.unwrap();
    visual.simulate_resize(gpui::size(px(380.), px(220.)));
    draw(visual);
    let position = |visual: &mut VisualTestContext| {
        visual.update(|_, cx| {
            let editor = editor.read(cx);
            (editor.selected_range().end, editor.scroll_offset().y)
        })
    };
    let start = position(visual);
    visual.simulate_keystrokes("pagedown");
    draw(visual);
    let first = position(visual);
    assert!(first.0 > start.0);
    assert_eq!(first.1, start.1);
    advance_frame(visual, 16);
    let intermediate = position(visual);
    assert!(intermediate.1 < start.1);

    visual.simulate_keystrokes("pagedown pagedown pageup");
    draw(visual);
    let latest = position(visual);
    assert!(
        latest.0 > first.0,
        "{start:?} {first:?} {intermediate:?} {latest:?}"
    );
    assert_eq!(latest.1, intermediate.1);
    finish_scroll(visual);
    let end = position(visual);
    assert_eq!(end.0, latest.0);
    assert!(end.1 < latest.1);
    visual.update(|_, cx| {
        let state = editor.read(cx);
        let row = state.cursor_position().line as usize;
        assert!(state.visible_row_range().unwrap().contains(&row));
    });

    visual.simulate_keystrokes("pagedown");
    draw(visual);
    advance_frame(visual, 16);
    let before_wheel = position(visual).1;
    scroll(visual, px(100.), ScrollDelta::Pixels(point(px(0.), px(8.))));
    let after_wheel = position(visual).1;
    assert!(after_wheel > before_wheel);
    finish_scroll(visual);
    assert_eq!(position(visual).1, after_wheel);

    visual.update(|_, cx| cx.set_reduce_motion(true));
    visual.simulate_keystrokes("pagedown");
    draw(visual);
    let immediate = position(visual);
    assert!(immediate.1 < after_wheel);
    finish_scroll(visual);
    assert_eq!(position(visual), immediate);

    visual.update(|_, cx| {
        cx.set_reduce_motion(false);
        cx.set_smooth_scrolling(false);
    });
    visual.simulate_keystrokes("pagedown");
    draw(visual);
    let disabled = position(visual);
    assert!(disabled.1 < immediate.1);
    finish_scroll(visual);
    assert_eq!(position(visual), disabled);
}

#[gpui::test]
fn markdown_navigation_measures_the_target_before_painting_the_arrival(cx: &mut TestAppContext) {
    let mut text = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        cx.set_smooth_scrolling(true);
        cx.set_reduce_motion(false);
        let paragraph = format!("{}\n\n", "a paragraph that wraps ".repeat(25));
        let source = format!(
            "# Start\n\n{}## Target\n\n{}",
            paragraph.repeat(18),
            paragraph
        );
        let document = cx.new(|cx| TextViewState::markdown(&source, cx).scrollable(true));
        document.read(cx).list_state().set_scroll_past_end(true);
        text = Some(document.clone());
        Root::new(cx.new(|_| ScrollDocument(document)), window, cx)
    });
    struct ScrollDocument(Entity<TextViewState>);
    impl Render for ScrollDocument {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            TextView::new(&self.0).scrollable(true).size_full()
        }
    }

    let text = text.unwrap();
    visual.simulate_resize(gpui::size(px(380.), px(220.)));
    visual.run_until_parked();
    draw(visual);
    let (list, block, range) = visual.update(|_, cx| {
        let text = text.read(cx);
        let rendered = text.rendered_text();
        let start = rendered.as_str().find("Target").unwrap();
        let source = rendered.source().find("Target").unwrap();
        (
            text.list_state().clone(),
            text.block_for_source_offset(source).unwrap(),
            start..start,
        )
    });
    let inset = px(12.);
    visual.update(|_, cx| {
        text.update(cx, |text, cx| {
            text.scroll_to_range(range.clone(), Some(inset), true, cx)
                .unwrap()
        })
    });
    draw(visual);
    let first = list.logical_scroll_top();
    assert!(first.item_ix > 0, "{first:?}, destination block {block}");
    assert!(list.is_navigating());
    advance_frame(visual, 16);
    assert_ne!(list.logical_scroll_top(), first);
    finish_scroll(visual);
    assert!(!list.is_navigating());
    let target = list
        .bounds_for_item(block)
        .expect("the target block is laid out");
    assert!(target.intersects(&list.viewport_bounds()));
    let animated = list.scroll_px_offset_for_scrollbar().y;
    list.scroll_to(Default::default());
    visual.update(|_, cx| {
        text.update(cx, |text, cx| {
            text.reveal_range_at_top(range, inset, cx).unwrap()
        })
    });
    draw(visual);
    assert!((animated - list.scroll_px_offset_for_scrollbar().y).abs() <= px(1.));
}

#[gpui::test]
fn sibling_scroll_only_preserves_an_active_list_reveal(cx: &mut TestAppContext) {
    struct Panes(ListState);
    impl Render for Panes {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .child(div().w(px(160.)).h_full().flex_none())
                .child(
                    list(self.0.clone(), |_, _, _| {
                        div().h(px(20.)).into_any_element()
                    })
                    .flex_1()
                    .h_full(),
                )
        }
    }

    let rows = ListState::new(100, ListAlignment::Top, px(300.));
    let (_, visual) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        cx.set_smooth_scrolling(true);
        cx.set_reduce_motion(false);
        Root::new(cx.new(|_| Panes(rows.clone())), window, cx)
    });
    visual.simulate_resize(gpui::size(px(400.), px(200.)));
    draw(visual);
    rows.navigate_to_reveal_item(16, true, true);
    draw(visual);
    advance_frame(visual, 16);
    assert!(rows.is_navigating());
    scroll(visual, px(90.), ScrollDelta::Lines(point(0., -1.)));
    assert!(
        rows.is_navigating(),
        "a sibling's wheel should leave the reveal running"
    );

    rows.navigate_to(ListOffset {
        item_ix: 26,
        offset_in_item: px(0.),
    });
    draw(visual);
    assert!(rows.is_navigating());
    scroll(visual, px(90.), ScrollDelta::Lines(point(0., -1.)));
    assert!(
        !rows.is_navigating(),
        "a new position navigation should keep the usual unscoped cancellation"
    );
}

#[gpui::test]
fn diff_file_navigation_scrolls_through_intermediate_rows(cx: &mut TestAppContext) {
    struct DiffView(Entity<DiffState>);
    impl Render for DiffView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            Diff::new(&self.0)
        }
    }

    let mut state = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        cx.set_smooth_scrolling(true);
        cx.set_reduce_motion(false);
        let files = (1..=4).map(|index| {
            DiffFile::parse(&format!(
                "--- /dev/null\n+++ b/file-{index}.txt\n@@ -0,0 +1,8 @@\n{}",
                "+line\n".repeat(8),
            ))
            .unwrap()
            .remove(0)
        });
        let diff = cx.new(|cx| DiffState::new(files, cx));
        state = Some(diff.clone());
        Root::new(cx.new(|_| DiffView(diff)), window, cx)
    });
    let state = state.unwrap();
    visual.simulate_resize(gpui::size(px(430.), px(260.)));
    draw(visual);
    let anchor = |visual: &mut VisualTestContext| {
        visual.update(|_, cx| {
            let anchor = state.read(cx).viewport_anchor().unwrap();
            (
                anchor.path().to_owned(),
                anchor.line().cloned(),
                anchor.offset(),
            )
        })
    };
    let start = anchor(visual);
    visual.update(|_, cx| state.update(cx, |diff, cx| diff.scroll_to_file("file-2.txt", cx)));
    draw(visual);
    assert_eq!(anchor(visual), start);
    advance_frame(visual, 16);
    let middle = anchor(visual);
    assert_eq!(middle.0, "file-1.txt");
    assert_ne!(middle, start);
    finish_scroll(visual);
    assert_eq!(anchor(visual), ("file-2.txt".to_owned(), None, px(0.)));
}

#[test]
fn wheel_scrolling_animates_through_the_desktop_viewports() {
    check_wheel_viewports(false);
}

#[test]
fn precise_wheel_scrolling_animates_through_the_desktop_viewports() {
    check_wheel_viewports(true);
}

fn check_wheel_viewports(precise: bool) {
    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        test_support::text_system(cfg!(target_os = "linux")),
    );
    let outer = ScrollHandle::new();
    let uniform = UniformListScrollHandle::new();
    let rows = ListState::new(100, ListAlignment::Top, px(300.));
    let horizontal = ScrollHandle::new();
    let mut views = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
        cx.set_smooth_scrolling(true);
        let editor = cx.new(|cx| {
            EditorState::new(window, cx).default_value("some text\n".repeat(100))
        });
        let patch = format!(
            "diff --git a/test.txt b/test.txt\n--- a/test.txt\n+++ b/test.txt\n@@ -1 +1,100 @@\n-old\n{}",
            "+new\n".repeat(100)
        );
        let diff = cx.new(|cx| DiffState::new(DiffFile::parse(&patch).unwrap(), cx));
        let document = cx.new(|cx| {
            TextViewState::markdown(&"A document paragraph.\n\n".repeat(80), cx).scrollable(true)
        });
        let view = cx.new(|_| ScrollViews {
            outer: outer.clone(),
            uniform: uniform.clone(),
            rows: rows.clone(),
            horizontal: horizontal.clone(),
            editor,
            diff,
            document,
        });
        views = Some(view.clone());
        Root::new(view, window, cx)
    });
    draw(cx);
    let views = views.unwrap();
    let (editor, diff, document_rows) = cx.update(|_, cx| {
        let views = views.read(cx);
        (
            views.editor.clone(),
            views.diff.clone(),
            views.document.read(cx).list_state().clone(),
        )
    });
    let wheel = |cx: &mut VisualTestContext, y, distance| {
        cx.simulate_event(ScrollEvent {
            position: point(px(100.), y),
            delta: if precise {
                ScrollDelta::Pixels(point(px(0.), px(distance * 20.)))
            } else {
                ScrollDelta::Lines(point(0., distance))
            },
            source: if precise {
                ScrollSource::PreciseWheel
            } else {
                ScrollSource::Pointer
            },
            ..Default::default()
        });
    };
    let document_notch = if precise { 20. } else { 30. };
    let editor_y = px(280.);
    let uniform_offset = || uniform.0.borrow().base_handle.offset().y;
    let list_offset = || rows.scroll_px_offset_for_scrollbar().y;
    let document_y = px(440.);
    let document_offset = || document_rows.scroll_px_offset_for_scrollbar().y;

    let viewports = [
        (px(40.), &uniform_offset as &dyn Fn() -> Pixels, px(-60.)),
        (px(120.), &list_offset, px(-60.)),
        (px(200.), &|| horizontal.offset().x, px(-60.)),
        (document_y, &document_offset, px(-3. * document_notch)),
    ];
    for (y, offset, _) in viewports {
        wheel(cx, y, -3.);
        assert_eq!(offset(), px(0.));
    }
    advance_frame(cx, 40);
    let offsets = viewports.map(|(_, offset, _)| offset());
    finish_scroll(cx);
    for ((y, offset, target), intermediate) in viewports.into_iter().zip(offsets) {
        assert!(intermediate < px(0.), "scroll area at {y:?}");
        assert_eq!(offset(), target, "scroll area at {y:?}");
        assert_eq!(outer.offset().y, px(0.));
    }

    wheel(cx, editor_y, -3.);
    let editor_offset =
        |cx: &mut VisualTestContext| cx.update(|_, cx| editor.read(cx).scroll_offset().y);
    assert_eq!(editor_offset(cx), px(0.));
    advance_frame(cx, 40);
    let intermediate = editor_offset(cx);
    assert!(intermediate < px(0.));
    finish_scroll(cx);
    assert_eq!(editor_offset(cx), px(-60.));
    assert_eq!(outer.offset().y, px(0.));

    let diff_y = px(360.);
    let diff_top_file =
        |cx: &mut VisualTestContext| cx.update(|_, cx| diff.read(cx).top_file().cloned());
    assert_eq!(diff_top_file(cx), None);
    let diff_steps = Rc::new(Cell::new(0));
    let _subscription = cx.update(|_, cx| {
        let steps = diff_steps.clone();
        cx.subscribe(&diff, move |_, event: &DiffEvent, _| {
            if matches!(event, DiffEvent::Scrolled) {
                steps.set(steps.get() + 1);
            }
        })
    });
    wheel(cx, diff_y, if precise { -4.5 } else { -3. });
    assert_eq!(diff_steps.get(), 0);
    advance_frame(cx, 40);
    assert_eq!(outer.offset().y, px(0.));
    assert_eq!(diff_steps.get(), 1);
    advance_frame(cx, 16);
    assert_eq!(diff_steps.get(), 2);
    finish_scroll(cx);
    assert!(diff_top_file(cx).is_some());
    // Move just above the 90 px preamble boundary with a native gesture.
    scroll(cx, diff_y, ScrollDelta::Pixels(point(px(0.), px(1.))));
    assert_eq!(diff_top_file(cx), None);

    let outer_y = px(490.);
    wheel(cx, outer_y, -2.);
    wheel(cx, outer_y, -2.);
    assert_eq!(outer.offset().y, px(0.));
    finish_scroll(cx);
    let combined = outer.offset().y;
    assert_eq!(combined, px(-80.));
    outer.set_offset(point(px(0.), px(0.)));
    draw(cx);
    wheel(cx, outer_y, -4.);
    finish_scroll(cx);
    assert_eq!(outer.offset().y, combined);

    wheel(cx, outer_y, -6.);
    advance_frame(cx, 30);
    let before_reverse = outer.offset().y;
    wheel(cx, outer_y, 2.);
    advance_frame(cx, 30);
    assert!(outer.offset().y > before_reverse);

    let before_pixels = outer.offset().y;
    scroll(cx, outer_y, ScrollDelta::Pixels(point(px(0.), px(-13.))));
    assert_eq!(outer.offset().y, before_pixels - px(13.));
    advance_frame(cx, 140);
    assert_eq!(outer.offset().y, before_pixels - px(13.));

    cx.update(|_, cx| cx.set_smooth_scrolling(false));
    outer.set_offset(point(px(0.), px(0.)));
    draw(cx);
    let before_document = document_offset();
    wheel(cx, document_y, -3.);
    assert_eq!(document_offset(), before_document - px(3. * document_notch));
    draw(cx);
    scroll(cx, document_y, ScrollDelta::Pixels(point(px(0.), px(-13.))));
    assert_eq!(
        document_offset(),
        before_document - px(3. * document_notch + 13.)
    );

    let before = outer.offset().y;
    wheel(cx, outer_y, -2.);
    let after = outer.offset().y;
    assert_eq!(after, before - px(40.));
    advance_frame(cx, 140);
    assert_eq!(outer.offset().y, after);
}

#[test]
fn wheel_bursts_keep_advancing_and_a_delayed_frame_does_not_jump_to_the_target() {
    struct ScrollViewport(ScrollHandle);

    impl Render for ScrollViewport {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("scroll")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.0)
                .child(div().h(px(4000.)))
        }
    }

    let mut cx = TestAppContext::build(TestDispatcher::new(0), None);
    let handle = ScrollHandle::new();
    let (_, cx) = cx.add_window_view(|_, cx| {
        cx.set_smooth_scrolling(true);
        ScrollViewport(handle.clone())
    });
    draw(cx);
    let wheel = ScrollDelta::Lines(point(0., -3.));
    scroll(cx, px(20.), wheel);
    advance_frame(cx, 160);
    let first = handle.offset().y;
    assert!(first < px(0.) && first > px(-60.));
    for _ in 0..4 {
        let before = handle.offset().y;
        cx.executor().advance_clock(Duration::from_millis(15));
        scroll(cx, px(20.), wheel);
        advance_frame(cx, 1);
        assert!(before - handle.offset().y > px(10.));
    }
    finish_scroll(cx);
    assert_eq!(handle.offset().y, px(-300.));
}

#[test]
fn replacing_an_earlier_patch_keeps_the_current_source_in_view() {
    let patch = |name: &str, lines: usize| {
        DiffFile::parse(&format!(
            "--- /dev/null\n+++ b/{name}\n@@ -0,0 +1,{lines} @@\n{}",
            "+line\n".repeat(lines),
        ))
        .unwrap()
        .remove(0)
    };
    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        test_support::text_system(cfg!(target_os = "linux")),
    );
    let state = cx.new(|cx| DiffState::new([patch("first", 2), patch("second", 100)], cx));
    state.update(&mut cx, |state, cx| {
        state.scroll_to_line(DiffLinePosition::new("second", DiffSide::Modified, 8), cx);
        assert_eq!(state.top_file().map(|path| path.as_str()), Some("second"));
        state.replace_files([patch("first", 40)], cx);
        assert_eq!(state.top_file().map(|path| path.as_str()), Some("second"));
        state.next_change(cx);
        assert_eq!(state.top_file().map(|path| path.as_str()), Some("first"));
    });
}

#[test]
fn split_diff_padding_keeps_the_source_line_at_the_same_viewport_position() {
    struct Columns([Entity<EditorState>; 2]);

    impl Render for Columns {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .size_full()
                .children(self.0.iter().map(|editor| {
                    div()
                        .w(px(300.))
                        .h_full()
                        .child(Editor::new(editor).size_full())
                }))
        }
    }

    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        test_support::text_system(cfg!(target_os = "linux")),
    );
    cx.update(gpui_component::init);
    let mut columns = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        let tail = "source line\n".repeat(99);
        let short = format!("short line\n{tail}");
        let long = format!("{}\n{tail}", "wrapped words ".repeat(20));
        let rows = Rc::new((1..=100).map(|line| (Some(line), None)).collect::<Vec<_>>());
        let columns = columns.insert([short, long].map(|text| {
            cx.new(|cx| {
                let mut editor = EditorState::new(window, cx)
                    .soft_wrap(true)
                    .default_value(text);
                editor.set_projected_rows(rows.clone(), cx);
                editor
            })
        }));
        Root::new(cx.new(|_| Columns(columns.clone())), window, cx)
    });
    let columns = columns.unwrap();
    visual.simulate_scale_factor_change(1.);
    visual.simulate_resize(gpui::size(px(600.), px(360.)));
    draw(visual);

    let anchor = 35;
    visual.update(|_, cx| {
        for column in &columns {
            column.update(cx, |editor, cx| {
                editor.scroll_to_projected_line(anchor, px(-3.), cx);
            });
        }
    });
    draw(visual);
    let viewport = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            columns.each_ref().map(|column| {
                let editor = column.read(cx);
                (
                    editor.projected_top_line(),
                    editor.projected_line_bounds(anchor).unwrap().top(),
                )
            })
        })
    };
    let before = viewport(visual);
    assert_eq!(before[0], before[1]);
    let padding = visual.update(|_, cx| {
        let [left, right] = columns.each_ref().map(|column| column.read(cx));
        assert!(right.projected_wrap_rows(0) > left.projected_wrap_rows(0));
        assert_ne!(left.scroll_offset().y, right.scroll_offset().y);
        Rc::new(
            left.projected_wrap_rows_snapshot()
                .zip(right.projected_wrap_rows_snapshot())
                .map(|(left, right)| left.max(right))
                .collect::<Vec<_>>(),
        )
    });
    let set_padding = |visual: &mut VisualTestContext, rows: Rc<Vec<usize>>| {
        visual.update(|_, cx| {
            for column in &columns {
                column.update(cx, |editor, cx| {
                    editor.set_projected_min_rows(rows.clone(), cx);
                });
            }
        });
        draw(visual);
        assert_eq!(viewport(visual), before);
    };
    set_padding(visual, padding);
    visual.update(|_, cx| {
        assert_eq!(
            columns[0].read(cx).scroll_offset().y,
            columns[1].read(cx).scroll_offset().y
        );
    });
    set_padding(visual, Rc::default());
}

#[test]
fn precise_wheel_replay_preserves_distance_and_yields_to_native_gestures() {
    struct EditorView(Entity<EditorState>);
    impl Render for EditorView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            Editor::new(&self.0).size_full()
        }
    }

    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        test_support::text_system(cfg!(target_os = "linux")),
    );
    let mut editor = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        cx.set_smooth_scrolling(true);
        cx.set_reduce_motion(false);
        let state = cx.new(|cx| {
            EditorState::new(window, cx).default_value("scroll replay line\n".repeat(1000))
        });
        editor = Some(state.clone());
        Root::new(cx.new(|_| EditorView(state)), window, cx)
    });
    let editor = editor.unwrap();
    cx.simulate_resize(gpui::size(px(600.), px(360.)));
    draw(cx);
    let position = cx.update(|_, cx| editor.read(cx).input_bounds().center());
    let offset = |cx: &mut VisualTestContext| cx.update(|_, cx| editor.read(cx).scroll_offset().y);
    let packet = |cx: &mut VisualTestContext, delta, source| {
        cx.simulate_event(ScrollEvent {
            position,
            delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
            source,
            ..Default::default()
        });
    };

    // The first two physical wheel bursts captured by scroll_probe on macOS.
    // Microsecond gaps include packets arriving more slowly than a 60 Hz frame.
    let samples = [
        (0, -13.),
        (17041, -28.),
        (25753, -88.),
        (23000, -103.),
        (17452, -102.),
        (29026, -103.),
        (50358, -103.),
        (203328, -13.),
        (17932, -103.),
        (15993, -103.),
        (16640, -102.),
        (24439, -103.),
        (37325, -103.),
        (204522, -129.),
        (23694, -308.),
        (18011, -206.),
        (15197, -205.),
        (16658, -206.),
    ];
    let mut now = 0;
    let mut next_frame = 16667;
    let mut total = 0.;
    for (gap, delta) in samples {
        let at = now + gap;
        while next_frame <= at {
            cx.executor()
                .advance_clock(Duration::from_micros(next_frame - now));
            now = next_frame;
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
            });
            next_frame += 16667;
        }
        cx.executor().advance_clock(Duration::from_micros(at - now));
        now = at;
        let before = offset(cx);
        packet(cx, delta, ScrollSource::PreciseWheel);
        assert_eq!(offset(cx), before, "wheel input waits for the frame");
        total += delta;
    }
    packet(cx, 0., ScrollSource::PreciseWheel);
    finish_scroll(cx);
    assert_eq!(
        offset(cx),
        px(total),
        "pixel distances must not become line units"
    );

    // Retarget before a frame: direction reversal drops the old pending tail.
    packet(cx, -308., ScrollSource::PreciseWheel);
    advance_frame(cx, 17);
    let reversed_at = offset(cx);
    packet(cx, 103., ScrollSource::PreciseWheel);
    advance_frame(cx, 17);
    assert!(offset(cx) > reversed_at);
    finish_scroll(cx);
    assert_eq!(offset(cx), reversed_at + px(103.));

    // A native trackpad packet cancels a wheel tail and remains immediate.
    packet(cx, -308., ScrollSource::PreciseWheel);
    advance_frame(cx, 17);
    let before_native = offset(cx);
    packet(cx, 13., ScrollSource::Pointer);
    assert_eq!(offset(cx), before_native + px(13.));
    finish_scroll(cx);
    assert_eq!(offset(cx), before_native + px(13.));

    // Changing the switch while a wheel animation is active leaves no stale tail.
    packet(cx, -205., ScrollSource::PreciseWheel);
    advance_frame(cx, 17);
    let before_disabled = offset(cx);
    cx.update(|_, cx| cx.set_smooth_scrolling(false));
    packet(cx, -28., ScrollSource::PreciseWheel);
    assert_eq!(offset(cx), before_disabled - px(28.));
    finish_scroll(cx);
    assert_eq!(offset(cx), before_disabled - px(28.));

    cx.update(|_, cx| cx.set_smooth_scrolling(true));
    packet(cx, 100000., ScrollSource::PreciseWheel);
    for _ in 0..40 {
        advance_frame(cx, 17);
    }
    assert_eq!(offset(cx), px(0.));
    packet(cx, 103., ScrollSource::PreciseWheel);
    finish_scroll(cx);
    assert_eq!(
        offset(cx),
        px(0.),
        "the upper boundary must not bounce for a wheel"
    );
    packet(cx, -103., ScrollSource::PreciseWheel);
    advance_frame(cx, 17);
    assert!(offset(cx) < px(0.) && offset(cx) > px(-103.));
    finish_scroll(cx);
    assert_eq!(offset(cx), px(-103.));
}
