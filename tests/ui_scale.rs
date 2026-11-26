use gpui::{
    AppContext as _, Context, Modifiers, Render, Styled, TestAppContext, TestDispatcher, Window,
    div, point, prelude::*, px, rgb, size,
};

struct ZoomProbe {
    clicks: usize,
}

impl Render for ZoomProbe {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().bg(rgb(0x000000)).child(
            div()
                .id("zoom-target")
                .absolute()
                .left(px(80.))
                .top(px(60.))
                .size(px(40.))
                .bg(rgb(0xff0000))
                .on_click(cx.listener(|probe, _, _, _| probe.clicks += 1)),
        )
    }
}

#[test]
fn ui_scale_composes_with_display_dpi_and_converts_pointer_events() {
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    let (probe, visual) = app.add_window_view(|_, _| ZoomProbe { clicks: 0 });
    visual.simulate_resize(size(px(640.), px(480.)));
    visual.simulate_scale_factor_change(1.5);
    let native_bounds = visual.update(|window, cx| {
        window.set_ui_scale(1.25, cx);
        window.draw(cx).clear(cx);
        assert_eq!(window.scale_factor(), 1.875);
        assert_eq!(window.viewport_size(), size(px(512.), px(384.)));
        window.window_bounds().get_bounds()
    });
    // At 125%, the visible target is x=100..150 and y=75..125 in
    // native logical pixels. This click would miss without conversion.
    visual.simulate_click(point(px(140.), px(100.)), Modifiers::default());
    visual.update(|window, cx| {
        assert_eq!(probe.read(cx).clicks, 1);
        assert_eq!(window.mouse_position(), point(px(112.), px(80.)));
    });

    visual.simulate_scale_factor_change(2.);
    visual.update(|window, cx| {
        assert_eq!(window.scale_factor(), 2.5);
        assert_eq!(window.window_bounds().get_bounds(), native_bounds);
        assert_eq!(window.viewport_size(), size(px(512.), px(384.)));
        window.set_ui_scale(1., cx);
        assert_eq!(window.scale_factor(), 2.);
        assert_eq!(window.viewport_size(), size(px(640.), px(480.)));
    });
}

#[cfg(target_os = "linux")]
#[test]
fn ui_scale_offscreen_render_resizes_content_within_the_same_surface() -> anyhow::Result<()> {
    use gpui::{HeadlessAppContext, PlatformHeadlessRenderer};
    use gpui_wgpu::WgpuHeadlessRenderer;
    use std::sync::Arc;

    let mut app = HeadlessAppContext::with_platform(
        gpui_platform::current_platform(true).text_system(),
        Arc::new(()),
        || {
            Ok(Some(
                Box::new(WgpuHeadlessRenderer::new()?) as Box<dyn PlatformHeadlessRenderer>
            ))
        },
    );
    let window = app.open_window(size(px(320.), px(240.)), |_, cx| {
        cx.new(|_| ZoomProbe { clicks: 0 })
    })?;
    app.simulate_scale_factor_change(window.into(), 1.5)?;
    for (scale, pixel) in [(1., [0, 0, 0, 255]), (1.25, [255, 0, 0, 255])] {
        app.update_window(window.into(), |_, window, cx| {
            window.set_ui_scale(scale, cx);
            window.draw(cx).clear(cx);
        })?;
        let image = app.capture_screenshot(window.into())?;
        assert_eq!(image.dimensions(), (480, 360));
        assert_eq!(image.get_pixel(210, 150).0, pixel);
    }
    Ok(())
}
