use std::time::Duration;

use gpui::{
    AppContext as _, Context, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    Styled as _, TestAppContext, VisualTestContext, Window, div, point, px, rgb,
};
use gpui_component::{
    Placement, Root,
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
    let platform = gpui_platform::current_platform(cfg!(target_os = "linux"));
    let mut app = TestAppContext::build_with_text_system(
        gpui::TestDispatcher::new(0),
        None,
        platform.text_system(),
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
