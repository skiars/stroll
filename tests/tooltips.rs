#[path = "../src/test_support.rs"]
mod test_support;

use std::time::Duration;

use gpui::{
    AppContext as _, Context, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    Styled as _, TestAppContext, VisualTestContext, Window, div, point, px, rgb,
};
use gpui_component::{
    Placement, Root, WindowExt as _,
    tooltip::{ManagedTooltipExt as _, Tooltip},
};

// Distinct fill colors let assertions observe each tooltip in the painted
// scene without running its renderer on every frame.
const TOOLTIP_COLORS: [u32; 2] = [0x7a135e, 0x136b7a];

struct RestartingTriggers;

impl Render for RestartingTriggers {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().children((0..2).map(|index| {
            div()
                .id(("trigger", index))
                .absolute()
                .left(px(10. + index as f32 * 100.))
                .top(px(10.))
                .w(px(100.))
                .h(px(30.))
                .managed_tooltip_restarting_at(Placement::Bottom, move |_, cx| {
                    cx.new(|_| Tooltip::new("tip").bg(rgb(TOOLTIP_COLORS[index])))
                        .into()
                })
        }))
    }
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.draw(cx).clear(cx));
}

fn wait(cx: &mut VisualTestContext) {
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    draw(cx);
}

fn painted_bounds(
    cx: &mut VisualTestContext,
    index: usize,
) -> Option<gpui::Bounds<gpui::ScaledPixels>> {
    draw(cx);
    cx.update(|window, _| {
        let marker: gpui::Background = rgb(TOOLTIP_COLORS[index]).into();
        window
            .painted_quads()
            .iter()
            .find(|quad| quad.background == marker)
            .map(|quad| quad.bounds)
    })
}

fn assert_shown(cx: &mut VisualTestContext, index: usize) -> gpui::Bounds<gpui::ScaledPixels> {
    painted_bounds(cx, index).unwrap_or_else(|| panic!("tooltip {index} must be painted"))
}

fn assert_hidden(cx: &mut VisualTestContext, index: usize) {
    assert!(
        painted_bounds(cx, index).is_none(),
        "tooltip {index} must not be painted"
    );
}

fn open(cx: &mut TestAppContext) -> VisualTestContext {
    cx.update(|cx| {
        gpui_component::init(cx);
        // These tests cover the delay, not intermediate fade frames.
        cx.set_reduce_motion(true);
    });
    let probe = cx.new(|_| RestartingTriggers);
    let window = cx
        .update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                cx.new(|cx| Root::new(probe.clone(), window, cx))
            })
        })
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    draw(&mut cx);
    cx
}

#[test]
fn restarting_tooltips_wait_on_every_entry() {
    let mut app = TestAppContext::build_with_text_system(
        gpui::TestDispatcher::new(0),
        None,
        test_support::text_system(cfg!(target_os = "linux")),
    );
    let mut cx = open(&mut app);

    cx.simulate_mouse_move(point(px(50.), px(25.)), None, Default::default());
    wait(&mut cx);
    let tooltip = assert_shown(&mut cx, 0);
    assert!(
        tooltip.top().0 >= 40.,
        "the tooltip must sit below the trigger: {tooltip:?}"
    );

    // Entering the adjacent trigger dismisses the visible tooltip and starts
    // its own delay instead of switching immediately.
    cx.simulate_mouse_move(point(px(150.), px(25.)), None, Default::default());
    draw(&mut cx);
    assert_hidden(&mut cx, 0);
    assert_hidden(&mut cx, 1);

    cx.executor().advance_clock(Duration::from_millis(100));
    cx.run_until_parked();
    assert_hidden(&mut cx, 1);

    wait(&mut cx);
    let tooltip = assert_shown(&mut cx, 1);
    assert!(
        tooltip.top().0 >= 40.,
        "the swapped tooltip must sit below the trigger: {tooltip:?}"
    );
    assert_hidden(&mut cx, 0);
}

#[test]
fn modal_tooltip_paints_above_the_dialog_and_clears_between_surfaces() {
    const MODAL_COLOR: u32 = 0x193457;
    let mut app = TestAppContext::build_with_text_system(
        gpui::TestDispatcher::new(0),
        None,
        test_support::text_system(cfg!(target_os = "linux")),
    );
    let mut cx = open(&mut app);
    cx.simulate_mouse_move(point(px(50.), px(25.)), None, Default::default());
    wait(&mut cx);
    assert_shown(&mut cx, 0);

    cx.update(|window, cx| {
        window.open_dialog(cx, |dialog, _, _| {
            dialog
                .margin_top(px(60.))
                .width(px(320.))
                .bg(rgb(MODAL_COLOR))
                .child(
                    div().h(px(150.)).child(
                        div()
                            .id("modal-tooltip-trigger")
                            .debug_selector(|| "modal-tooltip-trigger".into())
                            .w(px(60.))
                            .h(px(25.))
                            .managed_tooltip_at(Placement::Bottom, |_, cx| {
                                cx.new(|_| Tooltip::new("modal-tip").bg(rgb(TOOLTIP_COLORS[1])))
                                    .into()
                            }),
                    ),
                )
        });
    });
    draw(&mut cx);
    assert_hidden(&mut cx, 0);
    let trigger = cx.debug_bounds("modal-tooltip-trigger").unwrap().center();
    cx.simulate_mouse_move(trigger, None, Default::default());
    wait(&mut cx);
    cx.update(|window, _| {
        let quads = window.painted_quads();
        let dialog_fill: gpui::Background = rgb(MODAL_COLOR).into();
        let tip_fill: gpui::Background = rgb(TOOLTIP_COLORS[1]).into();
        let dialog = quads
            .iter()
            .position(|quad| quad.background == dialog_fill)
            .unwrap();
        let tip = quads
            .iter()
            .position(|quad| quad.background == tip_fill)
            .unwrap();
        assert!(quads[dialog].bounds.intersects(&quads[tip].bounds));
        assert!(dialog < tip, "the modal must paint before its tooltip");
    });

    cx.update(|window, cx| window.close_dialog(cx));
    assert_hidden(&mut cx, 1);
}
