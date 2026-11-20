#[path = "../src/test_support.rs"]
mod test_support;

use std::{cell::RefCell, rc::Rc, time::Duration};

use gpui::{
    AppContext as _, Bounds, Context, Entity, IntoElement, Modifiers, MouseButton, MouseExitEvent,
    ParentElement as _, Render, ScrollHandle, Styled, TestAppContext, TestDispatcher,
    VisualTestContext, Window, div, point, px, rgb, size,
};
use gpui_component::input::{
    Editor, EditorState, GutterDecoration, InputEvent, OverviewRulerDecoration, OverviewRulerLane,
};
use gpui_component::scroll::{Scrollbar, ScrollbarMode, content_scrollbar, thin_scrollbar};
use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

struct ScrollbarProbe {
    scroll: ScrollHandle,
    mode: ScrollbarMode,
    preset: Preset,
}

#[derive(Clone, Copy)]
enum Preset {
    Thin,
    Content,
}

impl Render for ScrollbarProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let viewport = Bounds::new(point(px(0.), px(0.)), size(px(200.), px(200.)));
        let scrollbar: Scrollbar = match self.preset {
            Preset::Thin => thin_scrollbar(&self.scroll),
            Preset::Content => content_scrollbar(&self.scroll),
        }
        .mode(self.mode)
        .viewport_bounds(viewport)
        .scroll_size(size(px(200.), px(1000.)));

        div().relative().size(px(200.)).child(scrollbar)
    }
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
        window.draw(cx).clear(cx);
    });
    cx.run_until_parked();
}

fn advance(cx: &mut VisualTestContext, milliseconds: u64) {
    cx.executor()
        .advance_clock(Duration::from_millis(milliseconds));
    draw(cx);
}

fn thumb_width(cx: &mut VisualTestContext) -> Option<f32> {
    cx.update(|window, _| {
        let scale = window.scale_factor();
        let viewport_right = 200.;
        window.painted_quads().iter().find_map(|quad| {
            let width = quad.bounds.size.width.as_f32() / scale;
            let height = quad.bounds.size.height.as_f32() / scale;
            let right = quad.bounds.right().as_f32() / scale;
            let alpha = quad.background.as_solid().map_or(0., |color| color.a);
            (width > 2.
                && width <= 20.
                && height > 20.
                && (right - viewport_right).abs() < 20.
                && alpha > 0.02)
                .then_some(width)
        })
    })
}

fn probe<'a>(
    cx: &'a mut TestAppContext,
    mode: ScrollbarMode,
    preset: Preset,
) -> &'a mut VisualTestContext {
    cx.update(gpui_component::init);
    let scroll = ScrollHandle::new();
    scroll.set_offset(point(px(0.), px(-100.)));
    let (_, visual) = cx.add_window_view({
        move |_, _| ScrollbarProbe {
            scroll,
            mode,
            preset,
        }
    });
    visual.simulate_resize(size(px(200.), px(200.)));
    draw(visual);
    visual
}

#[gpui::test]
fn content_scrollbars_stay_visible_after_the_idle_hold(cx: &mut TestAppContext) {
    let visual = probe(cx, ScrollbarMode::Always, Preset::Content);
    advance(visual, 1_100);
    let width = thumb_width(visual).expect("content scrollbars do not fade out");
    assert_eq!(width, 6., "content thumb keeps the standard width");
}

#[gpui::test]
fn non_content_scrollbars_use_the_thin_overlay_geometry(cx: &mut TestAppContext) {
    let visual = probe(cx, ScrollbarMode::Always, Preset::Thin);
    let width = thumb_width(visual).expect("the thin scrollbar paints a thumb");
    assert_eq!(width, 4., "non-content scrollbars use the thin thumb");
}

struct OverviewProbe(Entity<EditorState>);

impl Render for OverviewProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Editor::new(&self.0)
            .appearance(false)
            .bordered(false)
            .size_full()
    }
}

#[test]
fn interactive_editor_gutter_animates_paint_with_fixed_full_width_targets() {
    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        test_support::text_system(cfg!(target_os = "linux")),
    );
    cx.update(gpui_component::init);
    let outlined = rgb(0xe07746).into();
    let filled = rgb(0x249965).into();
    let fallback = rgb(0x7654a8).into();
    let (view, visual) = cx.add_window_view(|window, cx| {
        OverviewProbe(cx.new(|cx| {
            let mut editor = EditorState::new(window, cx).default_value("one\ntwo\nthree\n");
            editor.set_interactive_gutter_width(px(16.), cx);
            editor.create_gutter_decorations_collection(
                vec![
                    GutterDecoration::new(0..3)
                        .with_color(outlined)
                        .with_action(1)
                        .outlined(true),
                    GutterDecoration::new(8..13)
                        .with_color(filled)
                        .with_action(2),
                    GutterDecoration::new(0..3)
                        .with_color(fallback)
                        .with_action(3)
                        .fallback(true),
                ],
                cx,
            );
            editor
        }))
    });
    visual.simulate_resize(size(px(360.), px(160.)));
    let clicks = Rc::new(RefCell::new(Vec::new()));
    let (editor, _subscription) = visual.update(|_, cx| {
        let editor = view.read(cx).0.clone();
        let clicks = clicks.clone();
        let subscription = cx.subscribe(&editor, move |_, event: &InputEvent, _| {
            if let InputEvent::GutterClick(action) = event {
                clicks.borrow_mut().push(*action);
            }
        });
        (editor, subscription)
    });
    let markers = |visual: &mut VisualTestContext| {
        visual.update(|window, _| {
            let scale = window.scale_factor();
            let quads = window.painted_quads();
            assert!(
                quads
                    .iter()
                    .all(|quad| quad.background.as_solid() != Some(fallback))
            );
            let outline = quads
                .iter()
                .find(|quad| quad.border_color == outlined)
                .unwrap();
            for width in [
                outline.border_widths.top,
                outline.border_widths.right,
                outline.border_widths.bottom,
                outline.border_widths.left,
            ] {
                assert_eq!(
                    width.as_f32() / scale,
                    1.,
                    "the outline keeps a 1 px stroke"
                );
            }
            let fill = quads
                .iter()
                .find(|quad| quad.background.as_solid() == Some(filled))
                .unwrap();
            [outline, fill].map(|quad| quad.bounds.map(|value| px(value.as_f32() / scale)))
        })
    };
    let text_geometry = |visual: &mut VisualTestContext| {
        visual.update(|_, cx| editor.read(cx).cursor_layout().unwrap())
    };

    draw(visual);
    let [outline, fill] = markers(visual);
    assert_eq!(outline.size.width, px(5.));
    assert_eq!(fill.size.width, px(5.));
    let geometry = text_geometry(visual);
    let at_right_edge =
        |bounds: Bounds<gpui::Pixels>| point(bounds.left() + px(15.5), bounds.center().y);
    let hover_outline = at_right_edge(outline);
    let hover_fill = at_right_edge(fill);
    let modifiers = Modifiers::default();
    visual.simulate_mouse_move(hover_outline, None, modifiers);
    visual.simulate_click(hover_outline, modifiers);
    assert_eq!(
        *clicks.borrow(),
        [1],
        "the right edge is clickable before expansion"
    );
    draw(visual);
    advance(visual, 20);
    let intermediate = markers(visual)[0];
    assert!(intermediate.size.width > px(5.) && intermediate.size.width < px(12.));
    assert_eq!(intermediate.origin, outline.origin);
    assert_eq!(text_geometry(visual), geometry);
    advance(visual, 150);
    assert_eq!(markers(visual)[0].size.width, px(12.));
    assert_eq!(text_geometry(visual), geometry);

    visual.simulate_mouse_move(hover_fill, None, modifiers);
    draw(visual);
    advance(visual, 20);
    let [closing, opening] = markers(visual);
    for width in [closing.size.width, opening.size.width] {
        assert!(width > px(5.) && width < px(12.));
    }
    visual.simulate_click(hover_fill, modifiers);
    assert_eq!(*clicks.borrow(), [1, 2]);
    advance(visual, 150);
    let [closed, open] = markers(visual);
    assert_eq!(closed, outline);
    assert_eq!(open.size.width, px(12.));
    assert_eq!(open.origin, fill.origin);
    assert_eq!(text_geometry(visual), geometry);

    visual.simulate_event(MouseExitEvent {
        position: hover_fill,
        pressed_button: None,
        modifiers,
    });
    draw(visual);
    advance(visual, 150);
    assert_eq!(markers(visual), [outline, fill]);

    visual.simulate_mouse_move(hover_fill, None, modifiers);
    draw(visual);
    advance(visual, 150);
    assert_eq!(markers(visual)[1], open);
    let outside_strip = hover_fill + point(px(1.), px(0.));
    visual.simulate_mouse_move(outside_strip, None, modifiers);
    draw(visual);
    advance(visual, 150);
    assert_eq!(markers(visual), [outline, fill]);
    visual.simulate_click(outside_strip, modifiers);
    let unmarked_row = point(hover_outline.x, (outline.bottom() + fill.top()) / 2.);
    visual.simulate_click(unmarked_row, modifiers);
    assert_eq!(*clicks.borrow(), [1, 2]);
}

#[gpui::test]
fn editor_overview_uses_the_full_track_and_distinguishes_click_from_drag(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let git_color = rgb(0x249965);
    let center_color = rgb(0x963bc7);
    let fallback_color = rgb(0x825eca);
    let (view, visual) = cx.add_window_view(|window, cx| {
        OverviewProbe(cx.new(|cx| {
            let mut editor = EditorState::new(window, cx)
                .navigable_overview_ruler()
                .default_value("let a = 0;\n".repeat(240));
            let git = |line: usize| {
                let offset = line * "let a = 0;\n".len();
                GutterDecoration::new(offset..offset + 1).with_color(git_color.into())
            };
            editor.create_gutter_decorations_collection(
                vec![
                    git(52).with_action(1).with_tooltip("First marker"),
                    git(121),
                    git(119).with_color(fallback_color.into()).fallback(true),
                    git(149).without_overview_marker(),
                ],
                cx,
            );
            let center_offset = 122 * "let a = 0;\n".len();
            editor.create_overview_ruler_decorations_collection(
                vec![
                    OverviewRulerDecoration::new(center_offset..center_offset + 1)
                        .with_lane(OverviewRulerLane::Center)
                        .with_color(center_color.into()),
                ],
                cx,
            );
            editor
                .diagnostics_mut()
                .unwrap()
                .extend([100, 123].map(|line| Diagnostic {
                    range: Range::new(Position::new(line, 4), Position::new(line, 5)),
                    severity: Some(DiagnosticSeverity::ERROR),
                    message: "Example".into(),
                    ..Default::default()
                }));
            editor
        }))
    });
    visual.simulate_resize(size(px(360.), px(300.)));
    draw(visual);
    let editor = visual.update(|_, cx| view.read(cx).0.clone());
    let (git, fallback, center, diagnostics) = visual.update(|window, _| {
        let mut git = Vec::new();
        let mut fallback = None;
        let mut center = None;
        let mut diagnostics = Vec::new();
        for quad in window.painted_quads() {
            let bounds = quad
                .bounds
                .map(|value| px(value.as_f32() / window.scale_factor()));
            if bounds.left() < px(340.) {
                continue;
            }
            if quad.background.as_solid() == Some(center_color.into()) {
                assert!(center.replace(bounds).is_none());
                continue;
            }
            if bounds.size.width != px(3.) {
                continue;
            }
            if quad.background.as_solid() == Some(git_color.into()) {
                git.push((bounds, quad.order));
            } else if quad.background.as_solid() == Some(fallback_color.into()) {
                fallback = Some((bounds, quad.order));
            } else if quad.background.as_solid().is_some_and(|color| color.a > 0.) {
                diagnostics.push(bounds);
            }
        }
        let (fallback, fallback_order) = fallback.expect("the fallback is mirrored");
        assert!(
            git.iter().any(|(bounds, order)| {
                !bounds.intersect(&fallback).is_empty() && *order > fallback_order
            }),
            "fallback marks paint below overlapping ordinary marks"
        );
        git.sort_by_key(|(bounds, _)| bounds.top());
        (
            git.into_iter()
                .map(|(bounds, _)| bounds)
                .collect::<Vec<_>>(),
            fallback,
            center.expect("the center-lane decoration is painted"),
            diagnostics,
        )
    });
    assert_eq!(git.len(), 2);
    assert_eq!(diagnostics.len(), 2);
    assert!(git[0].right() < center.left() && center.right() < diagnostics[0].left());

    let scroll = |cx: &mut VisualTestContext| cx.update(|_, cx| editor.read(cx).scroll_offset().y);
    let left = git[0].center().x;
    let right = diagnostics[0].center().x;
    let click = |cx: &mut VisualTestContext, x, y| {
        cx.simulate_click(point(x, y), Default::default());
        draw(cx);
        scroll(cx)
    };
    let git_offset = click(visual, right, git[0].center().y);
    assert!(git_offset < px(0.));
    assert_eq!(click(visual, left, git[0].center().y), git_offset);
    let diagnostic_offset = click(visual, left, diagnostics[0].center().y);
    assert!(diagnostic_offset < git_offset);
    assert_eq!(
        click(visual, right, diagnostics[0].center().y),
        diagnostic_offset
    );
    assert_eq!(fallback.left(), git[1].left());
    let fallback_y = fallback.center().y;
    assert!(git[1].contains(&point(left, fallback_y)));
    let overlap_scroll = click(visual, left, fallback_y);
    let exposed_scroll = click(visual, left, git[1].top() + px(3.));
    assert!(
        (overlap_scroll - exposed_scroll).abs() < px(12.),
        "clicking either part of the ordinary mark reveals its line"
    );

    let overlap = (git[1].bottom().min(diagnostics[1].bottom())
        + git[1].top().max(diagnostics[1].top()))
        / 2.;
    assert!(overlap < git[1].bottom() && overlap > diagnostics[1].top());
    let git_offset = click(visual, left, overlap);
    let diagnostic_offset = click(visual, right, overlap);
    let center_offset = click(visual, center.center().x, overlap);
    assert!(
        diagnostic_offset < center_offset && center_offset < git_offset,
        "clicking each painted lane reveals its own marker"
    );
    assert!(
        diagnostic_offset < git_offset,
        "right half reveals the diagnostic"
    );
    assert_eq!(
        click(visual, left, overlap),
        git_offset,
        "a click over the thumb reveals the mark"
    );

    let thumb_point = point(center.center().x, overlap);
    visual.simulate_mouse_down(thumb_point, MouseButton::Left, Default::default());
    draw(visual);
    assert_eq!(
        scroll(visual),
        git_offset,
        "a press waits for a click or drag"
    );
    let drag_end = point(thumb_point.x, overlap + px(35.));
    visual.simulate_mouse_move(drag_end, Some(MouseButton::Left), Default::default());
    draw(visual);
    visual.simulate_mouse_up(drag_end, MouseButton::Left, Default::default());
    draw(visual);
    assert!(
        scroll(visual) < diagnostic_offset - px(100.),
        "a drag moves the viewport rather than activating the mark"
    );

    visual.update(|_, cx| editor.update(cx, |editor, _| editor.diagnostics_mut().unwrap().clear()));
    draw(visual);
    assert_eq!(
        click(visual, left, overlap),
        git_offset,
        "cleared diagnostics leave no stale targets"
    );
}
