#![cfg(any(target_os = "linux", target_os = "windows"))]

use gpui::{
    Context, Hsla, IntoElement, ParentElement as _, Render, Rgba, Styled as _, TestAppContext,
    TestDispatcher, Window, div, px, rgb,
};
use gpui_component::checkbox::Checkbox;

fn color_bytes(color: Hsla) -> [u8; 4] {
    let rgba: Rgba = color.into();
    [
        (rgba.r * 255.).round() as u8,
        (rgba.g * 255.).round() as u8,
        (rgba.b * 255.).round() as u8,
        (rgba.a * 255.).round() as u8,
    ]
}

struct IndeterminateHarness;

impl Render for IndeterminateHarness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().child(Checkbox::new("mixed").indeterminate(true))
    }
}

#[test]
fn unchecked_indicator_border_can_be_set_per_instance() {
    // Unchecked checkboxes have a transparent background, so the input border can blend into the panel.
    // Callers can supply a brighter control border color.
    struct BorderHarness;

    impl Render for BorderHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .bg(rgb(0x1c1d22))
                .child(Checkbox::new("unchecked").indicator_border_color(rgb(0x757983).into()))
        }
    }

    let platform = gpui_platform::current_platform(cfg!(target_os = "linux"));
    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        platform.text_system(),
    );
    cx.update(gpui_component::init);
    let (_, visual) = cx.add_window_view(|_, _| BorderHarness);
    visual.update(|window, cx| window.draw(cx).clear(cx));
    visual.run_until_parked();

    let quads = visual.update(|window, _| window.painted_quads());
    assert!(
        quads.iter().any(|quad| {
            color_bytes(quad.border_color) == color_bytes(rgb(0x757983).into())
                && quad.border_widths.left.0 > 0.
        }),
        "the unchecked indicator must paint the requested border color"
    );
}

#[test]
fn indeterminate_mark_is_centered_in_the_indicator() {
    let platform = gpui_platform::current_platform(cfg!(target_os = "linux"));
    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        platform.text_system(),
    );
    cx.update(gpui_component::init);
    let (_, visual) = cx.add_window_view(|_, _| IndeterminateHarness);
    visual.update(|window, cx| window.draw(cx).clear(cx));
    visual.run_until_parked();

    let indicator = visual
        .debug_bounds("checkbox-indicator")
        .expect("the checkbox indicator must be laid out");
    let mark = visual
        .debug_bounds("checkbox-indeterminate-mark")
        .expect("the indeterminate mark must be laid out");
    assert!(
        (mark.center().x - indicator.center().x).abs() <= px(0.5),
        "the indeterminate mark must be horizontally centered: indicator {indicator:?}, mark {mark:?}"
    );
    assert!(
        (mark.center().y - indicator.center().y).abs() <= px(0.5),
        "the indeterminate mark must be vertically centered: indicator {indicator:?}, mark {mark:?}"
    );
}
