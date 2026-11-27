use std::time::{Duration, Instant};

use gpui::{
    BackdropBlur, Bounds, ContentMask, Corners, PrimitiveBatch, Quad, ScaledPixels, Scene,
    SceneBatch, point, rgb, size,
};
use gpui_component::{PopupMotion, PopupMotionKind};

#[test]
fn fade_reverses_without_jumping_and_respects_reduced_motion() {
    let start = Instant::now();
    let mut motion = PopupMotion::new(PopupMotionKind::Hover, start);
    let interrupted = start + Duration::from_millis(55);
    let before = motion.sample(interrupted, false).0;
    assert!((0. ..1.).contains(&before));
    motion.retarget(false, interrupted);
    assert_eq!(motion.sample(interrupted, false), (before, true));
    let later = interrupted + Duration::from_millis(20);
    let closing = motion.sample(later, false).0;
    assert!(closing < before && closing > 0.);
    assert_eq!(motion.sample(later, true), (0., false));
    motion.retarget(true, later);
    assert_eq!(motion.sample(later, false), (closing, true));
    assert_eq!(
        motion.sample(later + Duration::from_millis(200), false),
        (1., false)
    );
}

#[test]
fn dropdown_motion_matches_the_preview() {
    let kind = PopupMotionKind::Dropdown;
    let duration = Duration::from_millis(80);
    assert_eq!(kind.duration(true), duration);
    assert_eq!(kind.duration(false), duration);

    let start = Instant::now();
    let mut motion = PopupMotion::new(kind, start);
    let halfway = start + duration / 2;
    let (opening, running) = motion.sample(halfway, false);
    assert!(running && opening > 0.5 && opening < 1.);
    motion.retarget(false, halfway);
    assert_eq!(motion.sample(halfway, false), (opening, true));
    assert_eq!(motion.sample(halfway, true), (0., false));

    let opened = start + duration;
    let mut motion = PopupMotion::new(kind, start);
    assert_eq!(motion.sample(opened, false), (1., false));
    motion.retarget(false, opened);
    let halfway = opened + duration / 2;
    let (closing, running) = motion.sample(halfway, false);
    assert!(running && closing > 0.5 && closing < 1.);
    motion.retarget(true, halfway);
    assert_eq!(motion.sample(halfway, false), (closing, true));
    assert_eq!(motion.sample(halfway, true), (1., false));
}

#[gpui::test]
fn default_dialog_fades_with_four_pixels_of_travel_over_180_ms(cx: &mut gpui::TestAppContext) {
    use gpui::{
        AppContext as _, Context, IntoElement, ParentElement as _, Render, Styled, Window, div, px,
    };
    use gpui_component::{Root, Theme, WindowExt as _};

    struct Probe;

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    cx.update(gpui_component::init);
    let (_, cx) = cx.add_window_view(|window, cx| {
        let probe = cx.new(|_| Probe);
        Root::new(probe, window, cx)
    });
    cx.simulate_resize(size(px(1000.), px(800.)));

    let draw = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| window.draw(cx).clear(cx));
    };
    let open = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| {
            window.open_dialog(cx, |dialog, _, _| {
                dialog.title("Push").child("Outgoing commit")
            });
        });
        cx.run_until_parked();
        draw(cx);
        draw(cx);
    };
    let advance = |cx: &mut gpui::VisualTestContext, millis| {
        cx.executor().advance_clock(Duration::from_millis(millis));
        cx.run_until_parked();
        draw(cx);
    };

    cx.update(|_, cx| {
        Theme::update(cx, |theme| theme.font_size = px(24.));
        cx.set_reduce_motion(false);
    });
    open(cx);
    cx.update(|window, _| assert_eq!(window.rem_size(), px(24.)));
    assert_eq!(cx.debug_bounds("dialog-0").unwrap().top(), px(76.));

    advance(cx, 30);
    let entering = cx.debug_bounds("dialog-0").unwrap().top();
    assert!(entering > px(76.) && entering < px(80.));
    advance(cx, 150);
    let resting = cx.debug_bounds("dialog-0").unwrap();
    assert_eq!(resting.top(), px(80.));

    cx.simulate_keystrokes("escape");
    draw(cx);
    cx.update(|window, cx| assert!(!window.has_active_dialog(cx)));
    assert_eq!(cx.debug_bounds("dialog-0"), Some(resting));
    advance(cx, 30);
    let leaving = cx.debug_bounds("dialog-0").unwrap().top();
    assert!(
        leaving > px(76.) && leaving < resting.top(),
        "unexpected dialog top during exit: {leaving:?}"
    );
    advance(cx, 149);
    assert!(cx.debug_bounds("dialog-0").is_some());
    advance(cx, 1);
    assert!(cx.debug_bounds("dialog-0").is_none());

    cx.update(|_, cx| cx.set_reduce_motion(true));
    open(cx);
    assert_eq!(cx.debug_bounds("dialog-0"), Some(resting));
    cx.simulate_keystrokes("escape");
    draw(cx);
    assert!(cx.debug_bounds("dialog-0").is_none());
}

#[test]
fn scene_splits_quad_batches_at_backdrops_and_replays_the_order() {
    let area = bounds(0., 0., 64., 48.);
    let mut scene = Scene::default();
    scene.push_layer(area);
    scene.insert_primitive(solid(area, 0x000000));
    scene.insert_backdrop_blur(blur(area));
    scene.insert_primitive(solid(area, 0xffffff));
    scene.insert_backdrop_blur(blur(area));
    scene.insert_primitive(solid(area, 0xff0000));
    scene.pop_layer();
    scene.finish();

    let mut replay = Scene::default();
    replay.replay(0..scene.len(), &scene);
    replay.finish();
    let order = |scene: &Scene| {
        scene
            .batches_with_backdrop()
            .map(|batch| match batch {
                SceneBatch::Draw(PrimitiveBatch::Quads(range)) => range.len(),
                SceneBatch::Backdrop(_) => 0,
                _ => panic!("unexpected paint command"),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(order(&scene), [1, 0, 1, 0, 1]);
    assert_eq!(order(&replay), order(&scene));
    replay.clear();
    assert!(replay.batches_with_backdrop().next().is_none());
}

#[cfg(windows)]
#[test]
fn directx_blurs_the_scene_under_the_popup() -> anyhow::Result<()> {
    use gpui::{
        AppContext as _, IntoElement, Render, Styled, Window, WindowBounds, WindowOptions, canvas,
        fill, px,
    };

    struct BackdropProbe;

    impl Render for BackdropProbe {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            canvas(
                |_, _, _| (),
                |_, (), window, _| {
                    let scale = window.scale_factor();
                    let rect = |x: f32, y: f32, width: f32, height: f32| {
                        Bounds::new(
                            point(px(x / scale), px(y / scale)),
                            size(px(width / scale), px(height / scale)),
                        )
                    };
                    window.paint_quad(fill(rect(0., 0., 80., 64.), rgb(0)));
                    window.paint_quad(fill(rect(40., 0., 40., 64.), rgb(0xffffff)));
                    window.with_content_mask(
                        Some(ContentMask {
                            bounds: rect(0., 12., 80., 52.),
                        }),
                        |window| {
                            window.paint_backdrop_blur(
                                rect(12., 8., 56., 48.),
                                Corners::all(px(12. / scale)),
                                px(6. / scale),
                                1.,
                            );
                        },
                    );
                    window.paint_quad(fill(rect(39., 25., 2., 14.), rgb(0xff0000)));
                },
            )
            .size_full()
        }
    }

    let (sender, receiver) = std::sync::mpsc::channel();
    gpui_platform::application().run(move |cx| {
        let window_size = size(px(320.), px(240.));
        let handle = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    point(px(0.), px(0.)),
                    window_size,
                ))),
                show: false,
                focus: false,
                ..Default::default()
            },
            |window, cx| {
                // A hidden Win32 window starts with a 1×1 swap chain.
                window.resize(window_size);
                cx.new(|_| BackdropProbe)
            },
        );
        cx.spawn(async move |cx| {
            let image = handle.and_then(|handle| {
                cx.update_window(handle.into(), |_, window, cx| {
                    window.draw(cx).clear(cx);
                    window.render_to_image()
                })?
            });
            sender.send(image).unwrap();
            cx.update(|cx| cx.quit());
        })
        .detach();
    });

    let image = receiver.recv()??;
    assert_eq!(image.get_pixel(38, 10).0, [0, 0, 0, 255], "content clip");
    assert_eq!(image.get_pixel(12, 12).0, [0, 0, 0, 255], "rounded corner");
    assert_eq!(image.get_pixel(39, 30).0, [255, 0, 0, 255], "foreground");
    assert_eq!(image.get_pixel(73, 21).0, [255; 4], "outside");
    let left = image.get_pixel(37, 21).0[0];
    let right = image.get_pixel(42, 21).0[0];
    assert!(
        left > 15 && right < 240 && left < right,
        "blur samples: {left}, {right}"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn gpu_blurs_underlying_pixels_inside_clip_and_leaves_foreground_sharp() -> anyhow::Result<()> {
    use gpui::{DevicePixels, PlatformHeadlessRenderer};
    use gpui_wgpu::WgpuHeadlessRenderer;

    let mut renderer = WgpuHeadlessRenderer::new()?;
    let mut scene = Scene::default();
    let area = bounds(0., 0., 80., 64.);
    scene.insert_primitive(solid(area, 0x000000));
    scene.insert_primitive(solid(bounds(40., 0., 40., 64.), 0xffffff));
    let mut effect = blur(bounds(12., 8., 56., 48.));
    effect.content_mask.bounds = bounds(12., 12., 56., 44.);
    scene.insert_backdrop_blur(effect);
    scene.insert_primitive(solid(bounds(39., 25., 2., 14.), 0xff0000));
    scene.finish();

    let image = renderer.render_scene_to_image(&scene, size(DevicePixels(80), DevicePixels(64)))?;
    assert_eq!(image.get_pixel(38, 10).0, [0, 0, 0, 255], "content clip");
    assert_eq!(image.get_pixel(12, 12).0, [0, 0, 0, 255], "rounded corner");
    assert_eq!(image.get_pixel(39, 30).0, [255, 0, 0, 255], "foreground");
    let left = image.get_pixel(37, 21).0[0];
    let right = image.get_pixel(42, 21).0[0];
    assert!(
        left > 15 && right < 240 && left < right,
        "blur samples: {left}, {right}"
    );
    assert_eq!(image.get_pixel(73, 21).0, [255, 255, 255, 255], "outside");
    // A second popup must sample the first popup's foreground in order.
    scene.insert_backdrop_blur(blur(bounds(33., 20., 16., 24.)));
    scene.finish();
    let stacked =
        renderer.render_scene_to_image(&scene, size(DevicePixels(80), DevicePixels(64)))?;
    let softened = stacked.get_pixel(39, 30).0;
    assert!(
        softened[0] > softened[1] && softened[1] > 30,
        "stacked blur: {softened:?}"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn gpu_matches_css_blur_spread_and_keeps_the_outer_shadow_outside() -> anyhow::Result<()> {
    use gpui::{DevicePixels, PlatformHeadlessRenderer, Shadow};
    use gpui_wgpu::WgpuHeadlessRenderer;

    let mut renderer = WgpuHeadlessRenderer::new()?;
    let mut scene = Scene::default();
    let canvas = bounds(0., 0., 160., 96.);
    let popup = bounds(12., 8., 136., 80.);
    let mask = ContentMask { bounds: canvas };
    scene.insert_primitive(solid(canvas, 0x000000));
    scene.insert_primitive(solid(bounds(80., 0., 80., 96.), 0xffffff));
    let mut effect = blur(popup);
    effect.radius = ScaledPixels(20.);
    scene.insert_backdrop_blur(effect);
    scene.insert_primitive(Shadow {
        order: 0,
        blur_radius: ScaledPixels(14.),
        bounds: popup,
        corner_radii: effect.corner_radii,
        content_mask: mask,
        color: gpui::Hsla::from(rgb(0)),
        element_bounds: popup,
        element_corner_radii: effect.corner_radii,
        inset: 0,
        clip_interior: 1,
    });
    scene.finish();

    let image =
        renderer.render_scene_to_image(&scene, size(DevicePixels(160), DevicePixels(96)))?;
    // At one standard deviation from a sharp edge, a Gaussian has mixed in
    // roughly 16% of the opposite side. Keep a tolerance for GPU sampling.
    let left = image.get_pixel(60, 48).0[0];
    let right = image.get_pixel(100, 48).0[0];
    assert!(
        (30..=55).contains(&left) && (200..=225).contains(&right),
        "{left}, {right}"
    );
    let [r, g, b, a] = image.get_pixel(135, 48).0;
    assert!(r >= 254 && r == g && g == b && a == 255, "white interior");
    assert!(image.get_pixel(150, 48).0[0] < 250, "outer shadow");
    Ok(())
}

fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
    Bounds::new(
        point(ScaledPixels(x), ScaledPixels(y)),
        size(ScaledPixels(width), ScaledPixels(height)),
    )
}

fn solid(bounds: Bounds<ScaledPixels>, color: u32) -> Quad {
    Quad {
        bounds,
        content_mask: ContentMask { bounds },
        background: rgb(color).into(),
        ..Default::default()
    }
}

fn blur(bounds: Bounds<ScaledPixels>) -> BackdropBlur {
    BackdropBlur {
        order: 0,
        bounds,
        content_mask: ContentMask { bounds },
        corner_radii: Corners::all(ScaledPixels(12.)),
        radius: ScaledPixels(6.),
        saturation: 1.,
        opacity: 1.,
    }
}

#[cfg(target_os = "linux")]
#[gpui::test]
fn dismissed_menu_restores_focus_and_does_not_capture_clicks_while_fading(
    cx: &mut gpui::TestAppContext,
) {
    use std::{cell::Cell, rc::Rc};

    use gpui::{
        AppContext as _, Context, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
        Render, StatefulInteractiveElement as _, Styled, VisualTestContext, Window, div, px,
    };
    use gpui_component::{
        Root,
        menu::{PopupMenu, PopupMenuItem},
    };

    struct Underlay {
        focus: FocusHandle,
        clicks: Rc<Cell<usize>>,
    }

    impl Render for Underlay {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let clicks = self.clicks.clone();
            div()
                .id("underlay")
                .size_full()
                .track_focus(&self.focus)
                .on_click(move |_, _, _| clicks.set(clicks.get() + 1))
        }
    }

    cx.update(gpui_component::init);
    let clicks = Rc::new(Cell::new(0));
    let underlay = cx.new(|cx| Underlay {
        focus: cx.focus_handle(),
        clicks: clicks.clone(),
    });
    let root_content = underlay.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| Root::new(root_content, window, cx));
    let activated = Rc::new(Cell::new(false));
    let clicked = activated.clone();
    let position = point(px(28.), px(28.));
    let menu = VisualTestContext::update(cx, |window, cx| {
        let focus = underlay.read(cx).focus.clone();
        focus.focus(window, cx);
        let menu = PopupMenu::build(window, cx, |menu, _, _| {
            menu.item(PopupMenuItem::new("Test command").on_click(move |_, _, _| clicked.set(true)))
        });
        PopupMenu::show_at(menu.clone(), position, window, cx);
        window.draw(cx).clear(cx);
        assert!(menu.focus_handle(cx).is_focused(window));
        menu
    });
    // Wait for the entrance to finish so the exit has a visible frame.
    std::thread::sleep(Duration::from_millis(110));
    cx.simulate_keystrokes("escape");
    VisualTestContext::update(cx, |window, cx| {
        window.draw(cx).clear(cx);
        assert!(underlay.read(cx).focus.is_focused(window));
        assert!(!menu.focus_handle(cx).is_focused(window));
    });
    cx.simulate_click(position + point(px(18.), px(14.)), Default::default());
    assert_eq!(clicks.get(), 1);
    assert!(!activated.get());
}

#[gpui::test]
fn context_menu_reserves_two_extra_pixels_for_leading_icons(cx: &mut gpui::TestAppContext) {
    use gpui::{
        AppContext as _, Context, InteractiveElement as _, IntoElement, MouseButton,
        ParentElement as _, Render, Styled, VisualTestContext, Window, div, px,
    };
    use gpui_component::{
        Icon, IconName, Root, Sizable as _,
        menu::{ContextMenuExt as _, PopupMenuItem},
    };

    struct Probe;

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let label = |id: &'static str| {
                PopupMenuItem::element(move |_, _| {
                    div().id(id).debug_selector(move || id.into()).child(id)
                })
            };
            div()
                .id("icon-size-probe")
                .size_full()
                .context_menu(move |menu, window, _| {
                    let original_size = window.rem_size() * 0.75;
                    menu.item(label("default-icon").icon(IconName::Copy))
                        .item(label("empty-icon"))
                        .item(
                            label("explicit-icon")
                                .icon(Icon::new(IconName::Copy).with_size(original_size)),
                        )
                })
                .shared_layer()
        }
    }

    cx.update(gpui_component::init);
    let (_, cx) = cx.add_window_view(|window, cx| {
        let probe = cx.new(|_| Probe);
        Root::new(probe, window, cx)
    });
    cx.simulate_mouse_down(
        point(px(28.), px(28.)),
        MouseButton::Right,
        Default::default(),
    );
    VisualTestContext::update(cx, |window, cx| {
        window.draw(cx).clear(cx);
    });
    let mut left = |id| cx.debug_bounds(id).expect("context menu label").left();
    assert_eq!(left("default-icon"), left("empty-icon"));
    assert_eq!(left("default-icon") - left("explicit-icon"), px(2.));
}

#[cfg(target_os = "linux")]
#[gpui::test]
fn popup_material_uses_the_web_theme_colors_and_filter(cx: &mut gpui::TestAppContext) {
    use gpui::{StyleRefinement, px};
    use gpui_component::{ThemeMode, popup_material};
    use stroll::workspace::apply_theme;

    cx.update(|cx| {
        gpui_component::init(cx);
        for (mode, color) in [(ThemeMode::Light, 0xffffff), (ThemeMode::Dark, 0x1c1d22)] {
            apply_theme(mode, cx);
            let material = popup_material(StyleRefinement::default(), cx);
            let fill = material
                .background
                .unwrap()
                .color()
                .unwrap()
                .as_solid()
                .unwrap();
            assert_eq!(fill, gpui::Hsla::from(rgb(color)).alpha(0.5));
            assert_eq!(material.backdrop_blur, Some(px(20.)));
            assert_eq!(material.backdrop_saturation, Some(1.6));
        }
    });
}

#[cfg(target_os = "linux")]
#[gpui::test]
fn popup_scrollbar_overlays_the_content_and_can_be_dragged(cx: &mut gpui::TestAppContext) {
    use std::{cell::Cell, rc::Rc};

    use gpui::{
        Context, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Render,
        ScrollDelta, ScrollEvent, ScrollHandle, StatefulInteractiveElement as _, Styled,
        VisualTestContext, Window, div, px,
    };
    use gpui_component::{
        Theme,
        scroll::{ScrollbarMode, ScrollbarViewport as _},
        v_flex,
    };

    struct ScrollProbe {
        scroll: ScrollHandle,
        tall: Rc<Cell<bool>>,
    }

    impl Render for ScrollProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                v_flex()
                    .id("popup-scroll-viewport")
                    .w(px(260.))
                    .max_h(px(120.))
                    .p_2()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .w_full()
                            .h(px(if self.tall.get() { 420. } else { 70. }))
                            .flex_shrink_0(),
                    )
                    .vertical_popup_scrollbar(&self.scroll),
            )
        }
    }

    cx.update(|cx| {
        gpui_component::init(cx);
        Theme::set_scrollbar_mode(ScrollbarMode::Always, cx);
        cx.set_reduce_motion(true);
    });
    let scroll = ScrollHandle::new();
    let tall = Rc::new(Cell::new(false));
    let (probe, cx) = cx.add_window_view({
        let scroll = scroll.clone();
        let tall = tall.clone();
        move |_, _| ScrollProbe { scroll, tall }
    });
    let draw = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| window.draw(cx).clear(cx));
    };
    cx.simulate_scale_factor_change(1.);
    draw(cx);
    let short = scroll.bounds();
    assert_eq!(scroll.max_offset().y, px(0.));

    tall.set(true);
    probe.update(cx, |_, cx| cx.notify());
    draw(cx);
    let long = scroll.bounds();
    assert_eq!(long.size.width, short.size.width);
    assert_eq!(
        scroll.bounds_for_item(0).unwrap().size.width,
        short.size.width - px(16.)
    );
    assert!(scroll.bounds_for_item(1).is_none());
    assert_eq!(long.size.height, px(120.));
    assert!(scroll.max_offset().y >= px(300.));

    cx.simulate_event(ScrollEvent {
        position: long.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-65.))),
        ..Default::default()
    });
    draw(cx);
    assert_eq!(scroll.offset().y, px(-65.));
    cx.update(|window, _| {
        let track_right = (long.right() - px(3.)).scale(1.);
        assert!(window.painted_quads().iter().any(|quad| {
            quad.bounds.right() == track_right
                && quad.bounds.top() > long.top().scale(1.)
                && quad.bounds.bottom() < long.bottom().scale(1.)
                && quad.bounds.size.width == ScaledPixels(4.)
                && quad.background.as_solid().is_some_and(|color| color.a > 0.)
        }));
    });

    // Drag the overlaid thumb from the right edge without changing the layout.
    scroll.set_offset(point(px(0.), px(0.)));
    probe.update(cx, |_, cx| cx.notify());
    draw(cx);
    let assert_thumb_width = |cx: &mut VisualTestContext| {
        cx.update(|window, _| {
            let thumb = window
                .painted_quads()
                .into_iter()
                .find(|quad| {
                    quad.bounds.right() == (long.right() - px(3.)).scale(1.)
                        && quad.bounds.size.height.0 >= 20.
                        && quad.background.as_solid().is_some_and(|color| color.a > 0.)
                })
                .expect("popup thumb");
            assert_eq!(thumb.bounds.size.width, ScaledPixels(4.));
        });
    };
    assert_thumb_width(cx);

    let outside_track = point(long.right() - px(11.), long.bottom() - px(5.));
    cx.simulate_mouse_move(outside_track, None, Default::default());
    cx.simulate_mouse_down(outside_track, MouseButton::Left, Default::default());
    cx.simulate_mouse_up(outside_track, MouseButton::Left, Default::default());
    draw(cx);
    assert_eq!(scroll.offset().y, px(0.));

    let inside_track = point(long.right() - px(9.), outside_track.y);
    cx.simulate_mouse_move(inside_track, None, Default::default());
    cx.simulate_mouse_down(inside_track, MouseButton::Left, Default::default());
    cx.simulate_mouse_up(inside_track, MouseButton::Left, Default::default());
    draw(cx);
    assert!(scroll.offset().y < px(0.));
    scroll.set_offset(point(px(0.), px(0.)));
    probe.update(cx, |_, cx| cx.notify());
    draw(cx);

    let thumb = point(long.right() - px(5.), long.top() + px(20.));
    let end = point(thumb.x, long.bottom() - px(5.));
    cx.simulate_mouse_move(thumb, None, Default::default());
    draw(cx);
    assert_thumb_width(cx);
    cx.simulate_mouse_down(thumb, MouseButton::Left, Default::default());
    cx.simulate_mouse_move(end, Some(MouseButton::Left), Default::default());
    draw(cx);
    assert_thumb_width(cx);
    cx.simulate_mouse_up(end, MouseButton::Left, Default::default());
    draw(cx);
    assert!(
        scroll.offset().y < px(-230.),
        "thumb offset: {:?}",
        scroll.offset()
    );
    assert_eq!(scroll.bounds().size.width, short.size.width);

    scroll.set_offset(point(px(0.), px(0.)));
    probe.update(cx, |_, cx| cx.notify());
    draw(cx);
    cx.simulate_mouse_down(thumb, MouseButton::Left, Default::default());
    let middle = point(thumb.x, (thumb.y + end.y) / 2.);
    cx.simulate_mouse_move(middle, MouseButton::Left, Default::default());
    let dragged_offset = scroll.offset();
    assert!(dragged_offset.y < px(0.));
    cx.simulate_mouse_move(end, None, Default::default());
    cx.simulate_mouse_move(end, MouseButton::Left, Default::default());
    assert_eq!(scroll.offset(), dragged_offset);
}

#[cfg(target_os = "linux")]
#[gpui::test]
fn resize_handle_cancels_pending_and_active_drags_when_the_left_button_is_up(
    cx: &mut gpui::TestAppContext,
) {
    use gpui::{
        AppContext as _, Axis, DragMoveEvent, InteractiveElement as _, MouseButton,
        ParentElement as _, Styled as _, div, px,
    };
    use gpui_component::{
        Root,
        resizable::{HandleEdge, resize_handle},
    };

    struct Resize;
    impl gpui::Render for Resize {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            div()
        }
    }

    struct Split(std::rc::Rc<std::cell::Cell<usize>>);
    impl gpui::Render for Split {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            let observed = self.0.clone();
            div()
                .id("resize-probe")
                .relative()
                .size_full()
                .on_drag_move(move |_: &DragMoveEvent<Resize>, _, _| {
                    observed.set(observed.get() + 1)
                })
                .child(
                    resize_handle::<Resize, Resize>("split", Axis::Horizontal)
                        .inside(HandleEdge::Leading)
                        .on_drag(Resize, |_, _, _, cx| cx.new(|_| Resize)),
                )
        }
    }

    let moves = std::rc::Rc::new(std::cell::Cell::new(0));
    let (_, cx) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        let content = cx.new(|_| Split(moves.clone()));
        Root::new(content, window, cx)
    });
    let draw = |cx: &mut gpui::VisualTestContext| cx.update(|window, cx| window.draw(cx).clear(cx));
    draw(cx);
    let start = point(px(2.), px(40.));
    let move_to = |x| point(start.x + px(x), start.y);
    let active = |cx: &mut gpui::VisualTestContext| cx.update(|_, cx| cx.has_active_drag());

    cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
    cx.simulate_mouse_move(move_to(40.), None, Default::default());
    cx.simulate_mouse_move(move_to(45.), MouseButton::Left, Default::default());
    assert!(!active(cx));

    cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
    cx.simulate_mouse_move(move_to(20.), MouseButton::Left, Default::default());
    assert!(active(cx));
    cx.simulate_mouse_move(move_to(40.), MouseButton::Left, Default::default());
    let dragged = moves.get();
    assert!(dragged > 0);
    cx.simulate_mouse_move(move_to(80.), None, Default::default());
    assert!(!active(cx));
    cx.simulate_mouse_move(move_to(90.), MouseButton::Left, Default::default());
    assert_eq!(moves.get(), dragged);
}

#[gpui::test]
fn open_diagnostic_updates_in_place_after_an_edit(cx: &mut gpui::TestAppContext) {
    use gpui::{AppContext as _, Context, Entity, Render, Styled as _, Window, px};
    use gpui_component::{
        Root,
        input::{Editor, EditorState},
    };

    use lsp_types::{Diagnostic, Position, Range};

    let diagnostic = |row, end, message: &str| {
        Diagnostic::new_simple(
            Range::new(Position::new(row, 0), Position::new(row, end)),
            message.into(),
        )
    };
    struct Probe(Entity<EditorState>);
    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            Editor::new(&self.0).size_full()
        }
    }

    cx.update(gpui_component::init);
    let mut editor = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| {
            let mut state = EditorState::new(window, cx).default_value("error; ok\nother\n");
            let set = state.diagnostics_mut().unwrap();
            set.push(diagnostic(0, 5, "Previous error"));
            set.push(diagnostic(1, 5, "Other error"));
            state
        });
        editor = Some(state.clone());
        let probe = cx.new(|_| Probe(state));
        Root::new(probe, window, cx)
    });
    let editor = editor.unwrap();
    let pointer = cx.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            editor.focus(window, cx);
            editor.set_selected_range(9..9, cx);
        });
        window.draw(cx).clear(cx);
        editor.read(cx).range_to_bounds(&(0..5)).unwrap().center()
    });
    cx.simulate_mouse_move(pointer, None, Default::default());

    let visible_message = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            editor
                .read(cx)
                .diagnostic_popover()
                .map(|entry| entry.message.clone())
        })
    };
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(300));
    cx.run_until_parked();
    assert_eq!(visible_message(cx).as_deref(), Some("Previous error"));
    cx.simulate_input(" ");
    assert_eq!(visible_message(cx).as_deref(), Some("Previous error"));
    cx.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            editor.edit_ranges(&[(4..5, String::new())], window, cx);
        });
    });
    assert_eq!(visible_message(cx).as_deref(), Some("Previous error"));
    cx.update(|_, cx| {
        editor.update(cx, |editor, cx| {
            let text = editor.text().clone();
            let set = editor.diagnostics_mut().unwrap();
            assert_eq!(set.iter().nth(1).unwrap().range, 10..15);
            set.reset(&text);
            set.push(diagnostic(0, 4, "Current error"));
            cx.notify();
        });
    });
    assert_eq!(visible_message(cx).as_deref(), Some("Current error"));
    cx.update(|_, cx| {
        editor.update(cx, |editor, cx| {
            editor.diagnostics_mut().unwrap().clear();
            cx.notify();
        });
    });
    assert_eq!(visible_message(cx), None);

    let (pointer, explicit) = cx.update(|window, cx| {
        let explicit = editor.update(cx, |editor, cx| {
            let set = editor.diagnostics_mut().unwrap();
            set.push(diagnostic(0, 4, "Pending hover"));
            set.push(diagnostic(1, 5, "Explicit diagnostic"));
            let explicit = set.iter().nth(1).unwrap().clone();
            cx.notify();
            explicit
        });
        window.draw(cx).clear(cx);
        let bounds = editor.read(cx).range_to_bounds(&(0..4)).unwrap();
        (bounds.center() - point(px(1.), px(0.)), explicit)
    });
    cx.simulate_mouse_move(pointer, None, Default::default());
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(299));
    cx.run_until_parked();
    assert_eq!(visible_message(cx), None);
    cx.update(|_, cx| {
        editor.update(cx, |editor, cx| {
            editor.present_diagnostic(explicit.clone(), cx);
            assert_eq!(editor.hover_popover().unwrap().symbol_range, explicit.range);
        });
    });
    assert_eq!(visible_message(cx).as_deref(), Some("Explicit diagnostic"));
    cx.executor().advance_clock(Duration::from_millis(1));
    cx.run_until_parked();
    assert_eq!(visible_message(cx).as_deref(), Some("Explicit diagnostic"));
}

#[gpui::test]
fn editor_hover_waits_for_pointer_motion_after_scrolling(cx: &mut gpui::TestAppContext) {
    use std::rc::Rc;

    use gpui::{
        App, AppContext as _, Context, Entity, Render, ScrollDelta, ScrollEvent, Styled as _, Task,
        Window, px,
    };
    use gpui_component::{
        Root,
        input::{Editor, EditorState, HoverProvider, Rope},
    };
    use lsp_types::{Hover, HoverContents, MarkedString};

    struct Documentation;
    impl HoverProvider for Documentation {
        fn hover(
            &self,
            _: &Rope,
            offset: usize,
            _: &mut Window,
            _: &mut App,
        ) -> Task<anyhow::Result<Option<Hover>>> {
            Task::ready(Ok(Some(Hover {
                contents: HoverContents::Scalar(MarkedString::String(offset.to_string())),
                range: None,
            })))
        }
    }

    struct Probe(Entity<EditorState>);
    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            Editor::new(&self.0).size_full()
        }
    }

    cx.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
    });
    let mut editor = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| {
            let mut state = EditorState::new(window, cx).default_value("symbol\n".repeat(100));
            state.lsp_mut().hover_provider = Some(Rc::new(Documentation));
            state
        });
        editor = Some(state.clone());
        let probe = cx.new(|_| Probe(state));
        Root::new(probe, window, cx)
    });
    let editor = editor.unwrap();
    let (pointer, line_height) = cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        let first = editor.read(cx).range_to_bounds(&(14..20)).unwrap();
        let next = editor.read(cx).range_to_bounds(&(21..27)).unwrap();
        (first.center(), next.top() - first.top())
    });
    let scroll = |cx: &mut gpui::VisualTestContext, dy| {
        cx.simulate_event(ScrollEvent {
            position: pointer,
            delta: ScrollDelta::Pixels(point(px(0.), dy)),
            ..Default::default()
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
    };
    let advance = |cx: &mut gpui::VisualTestContext, millis| {
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(millis));
        cx.run_until_parked();
    };
    let active = |cx: &mut gpui::VisualTestContext| {
        cx.update(|_, cx| {
            editor
                .read(cx)
                .hover_popover()
                .map(|tip| tip.symbol_range.clone())
        })
    };

    let next_target = pointer + point(px(0.), line_height);
    let blank = pointer + point(px(150.), px(0.));
    cx.simulate_mouse_move(pointer, None, Default::default());
    advance(cx, 200);
    assert_eq!(active(cx), None);
    cx.simulate_mouse_move(blank, None, Default::default());
    advance(cx, 300);
    assert_eq!(active(cx), None);

    cx.simulate_mouse_move(pointer, None, Default::default());
    advance(cx, 200);
    cx.simulate_mouse_move(next_target, None, Default::default());
    advance(cx, 299);
    assert_eq!(active(cx), None);

    // Motion inside the same symbol keeps the original timer.
    cx.simulate_mouse_move(
        next_target + point(px(1.), px(0.)),
        None,
        Default::default(),
    );
    advance(cx, 1);
    assert_eq!(active(cx), Some(21..27));

    cx.update(|_, cx| editor.update(cx, |editor, cx| editor.clear_hover_state(cx)));
    cx.simulate_mouse_move(pointer, None, Default::default());
    advance(cx, 300);
    assert_eq!(active(cx), Some(14..20));
    scroll(cx, px(40.));
    assert_eq!(active(cx), Some(14..20));
    scroll(cx, px(-2.));
    assert_eq!(active(cx), Some(14..20));

    scroll(cx, px(2.) - line_height);
    assert_eq!(active(cx), None);
    cx.simulate_mouse_move(pointer, None, Default::default());
    advance(cx, 300);
    assert_eq!(active(cx), None);
    cx.simulate_mouse_move(pointer + point(px(1.), px(0.)), None, Default::default());
    advance(cx, 300);
    assert_eq!(active(cx), Some(21..27));

    // A scroll that invalidates a pending hover must also cancel its task.
    cx.simulate_mouse_move(
        pointer + point(px(0.), line_height),
        None,
        Default::default(),
    );
    scroll(cx, -line_height * 2.);
    advance(cx, 300);
    assert_eq!(active(cx), None);
}

#[gpui::test]
fn lsp_hover_stays_open_during_pointer_handoff_and_copies_documentation(
    cx: &mut gpui::TestAppContext,
) {
    use std::rc::Rc;

    use gpui::{
        App, AppContext as _, ClipboardItem, Context, Entity, MouseButton, ParentElement as _,
        Render, Styled as _, Task, Window, div, px,
    };
    use gpui_component::{
        Root,
        input::{Copy, Editor, EditorState, HoverProvider, Rope},
    };
    use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind};

    const MARKDOWN: &str = "Documentation for **symbol**.";
    const PLAIN_TEXT: &str = "Literal **text** with ``` backticks.";

    struct Documentation;
    impl HoverProvider for Documentation {
        fn hover(
            &self,
            _: &Rope,
            offset: usize,
            _: &mut Window,
            _: &mut App,
        ) -> Task<anyhow::Result<Option<Hover>>> {
            let (kind, value) = if offset < "literal".len() {
                (MarkupKind::PlainText, PLAIN_TEXT)
            } else {
                (MarkupKind::Markdown, MARKDOWN)
            };
            Task::ready(Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind,
                    value: value.into(),
                }),
                range: None,
            })))
        }
    }

    struct Probe(Entity<EditorState>, Entity<EditorState>);
    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            div()
                .flex()
                .size_full()
                .child(
                    div()
                        .w(px(160.))
                        .h_full()
                        .flex_none()
                        .child(Editor::new(&self.0).size_full()),
                )
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .child(Editor::new(&self.1).size_full()),
                )
        }
    }

    cx.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
    });
    let mut editors = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| {
            let mut state = EditorState::new(window, cx).default_value("literal\nsymbol\nsymbol\n");
            state.lsp_mut().hover_provider = Some(Rc::new(Documentation));
            state
        });
        let peer = cx.new(|cx| {
            let mut state = EditorState::new(window, cx).default_value("peers\npeers\npeers\n");
            state.lsp_mut().hover_provider = Some(Rc::new(Documentation));
            state
        });
        editors = Some((state.clone(), peer.clone()));
        let probe = cx.new(|_| Probe(state, peer));
        Root::new(probe, window, cx)
    });
    let (editor, peer) = editors.unwrap();
    let draw = |cx: &mut gpui::VisualTestContext| cx.update(|window, cx| window.draw(cx).clear(cx));
    draw(cx);

    let clipboard_text = |cx: &mut gpui::VisualTestContext| {
        cx.update(|_, cx| {
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default()
        })
    };
    for (range, expected) in [(15..21, MARKDOWN), (0..7, PLAIN_TEXT)] {
        let symbol = cx.update(|_, cx| editor.read(cx).range_to_bounds(&range).unwrap());
        cx.simulate_mouse_move(symbol.center(), None, Default::default());
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(300));
        cx.run_until_parked();
        draw(cx);

        let docs = cx.debug_bounds("hover-markdown").unwrap();
        let step = point(symbol.right() + px(20.), symbol.center().y);
        assert!(docs.union(&symbol).contains(&step));
        assert!(!docs.contains(&step));
        cx.simulate_mouse_move(step, None, Default::default());
        draw(cx);
        cx.update(|_, cx| {
            let hover = editor
                .read(cx)
                .hover_popover()
                .expect("hover during handoff");
            assert_eq!(hover.symbol_range, range);
        });

        if expected == MARKDOWN {
            let start = docs.origin + point(px(19.), px(15.));
            let end = start + point(px(50.), px(0.));
            cx.simulate_mouse_move(start, None, Default::default());
            draw(cx);
            cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
            cx.simulate_mouse_move(end, MouseButton::Left, Default::default());
            cx.simulate_mouse_up(end, MouseButton::Left, Default::default());
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                cx.write_to_clipboard(ClipboardItem::new_string(String::new()));
                window.dispatch_action(Box::new(Copy), cx);
            });
            let selection = clipboard_text(cx);
            assert!(!selection.is_empty() && "Documentation".contains(&selection));
        }

        let copy = cx.debug_bounds("hover-copy-documentation").unwrap();
        assert_eq!(copy.top() - docs.top(), px(10.));
        assert_eq!(docs.right() - copy.right(), px(10.));
        assert_eq!(copy.size, size(px(20.), px(20.)));
        cx.simulate_mouse_move(copy.center(), None, Default::default());
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let scale = window.scale_factor();
            assert!(window.painted_quads().iter().any(|quad| {
                quad.bounds == copy.scale(scale)
                    && quad.corner_radii == Corners::all(px(3.).scale(scale))
                    && quad.background.as_solid().is_some_and(|color| color.a > 0.)
            }));
        });
        cx.simulate_click(copy.center(), Default::default());
        assert_eq!(clipboard_text(cx), expected);

        if expected == MARKDOWN {
            let peer_symbol = cx.update(|_, cx| peer.read(cx).range_to_bounds(&(12..17)).unwrap());
            let peer_pointer = peer_symbol.center();
            assert!(docs.union(&symbol).contains(&peer_pointer));
            assert!(!docs.contains(&peer_pointer));
            cx.simulate_mouse_move(peer_pointer, None, Default::default());
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(300));
            cx.run_until_parked();
            cx.update(|_, cx| {
                assert!(editor.read(cx).hover_popover().is_some());
                assert_eq!(peer.read(cx).hover_popover().unwrap().symbol_range, 12..17);
            });
        }
        cx.simulate_mouse_move(point(px(2.), px(2.)), None, Default::default());
        draw(cx);
        cx.update(|_, cx| assert!(editor.read(cx).hover_popover().is_none()));
    }
}

#[gpui::test]
fn escape_dismisses_editor_hover(cx: &mut gpui::TestAppContext) {
    use std::{cell::Cell, rc::Rc};

    use gpui::{App, AppContext as _, Context, Entity, Render, Styled as _, Task, Window, px};
    use gpui_component::{
        Root,
        input::{Editor, EditorState, HoverProvider, Rope},
    };
    use lsp_types::{CompletionItem, Hover, HoverContents, MarkedString};

    struct Documentation(Rc<Cell<usize>>);
    impl HoverProvider for Documentation {
        fn hover(
            &self,
            _: &Rope,
            _: usize,
            _: &mut Window,
            cx: &mut App,
        ) -> Task<anyhow::Result<Option<Hover>>> {
            self.0.set(self.0.get() + 1);
            let delay = cx.background_executor().timer(Duration::from_millis(200));
            cx.background_spawn(async move {
                delay.await;
                Ok(Some(Hover {
                    contents: HoverContents::Scalar(MarkedString::String("Hover docs".into())),
                    range: None,
                }))
            })
        }
    }

    struct Probe(Entity<EditorState>);
    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            Editor::new(&self.0).size_full()
        }
    }

    cx.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
    });
    let requests = Rc::new(Cell::new(0));
    let mut editor = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| {
            let mut state = EditorState::new(window, cx).default_value("symbol next\n");
            state.lsp_mut().hover_provider = Some(Rc::new(Documentation(requests.clone())));
            state.focus(window, cx);
            state
        });
        editor = Some(state.clone());
        let view = cx.new(|_| Probe(state));
        Root::new(view, window, cx)
    });
    let editor = editor.unwrap();
    let draw = |visual: &mut gpui::VisualTestContext| {
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear(cx));
    };
    let advance = |visual: &mut gpui::VisualTestContext, millis| {
        visual
            .executor()
            .advance_clock(Duration::from_millis(millis));
        draw(visual);
    };
    draw(visual);
    let source = visual.update(|_, cx| editor.read(cx).range_to_bounds(&(0..6)).unwrap());
    let pointer = source.center();
    let hover_visible = |visual: &mut gpui::VisualTestContext| {
        visual.update(|_, cx| editor.read(cx).hover_popover().is_some())
    };
    visual.simulate_mouse_move(pointer, None, Default::default());
    draw(visual);
    advance(visual, 150);
    advance(visual, 200);
    assert!(hover_visible(visual));
    assert_eq!(requests.get(), 1);

    visual.update(|_, cx| {
        editor.update(cx, |editor, cx| {
            editor.present_completion_items(
                0,
                "",
                vec![CompletionItem::new_simple("symbol".into(), "Item".into())],
                cx,
            );
        });
    });
    draw(visual);
    visual.simulate_keystrokes("escape");
    draw(visual);
    assert!(
        hover_visible(visual),
        "completion receives the first Escape"
    );
    visual.update(|_, cx| {
        assert!(!editor.read(cx).completion_menu_state().open);
        editor.update(cx, |editor, cx| editor.add_selection(8..8, cx));
    });
    visual.simulate_keystrokes("escape");
    draw(visual);
    assert!(!hover_visible(visual));
    assert_eq!(
        visual.update(|_, cx| editor.read(cx).selection_endpoints().count()),
        2
    );
    visual.simulate_keystrokes("escape");
    assert_eq!(
        visual.update(|_, cx| editor.read(cx).selection_endpoints().count()),
        1
    );

    visual.simulate_mouse_move(pointer + point(px(1.), px(0.)), None, Default::default());
    draw(visual);
    advance(visual, 400);
    assert_eq!(
        requests.get(),
        1,
        "jitter on the dismissed symbol must not reopen it"
    );

    visual.simulate_mouse_move(point(px(2.), px(2.)), None, Default::default());
    visual.simulate_mouse_move(pointer, None, Default::default());
    draw(visual);
    advance(visual, 150);
    assert_eq!(requests.get(), 2);
    assert!(!hover_visible(visual));
    visual.simulate_keystrokes("escape");
    advance(visual, 350);
    assert!(
        !hover_visible(visual),
        "a pending hover must stay dismissed"
    );
}

#[cfg(target_os = "linux")]
#[gpui::test]
fn long_lsp_hover_can_scroll_without_resizing_or_moving_the_editor(cx: &mut gpui::TestAppContext) {
    use std::{cell::Cell, rc::Rc};

    use gpui::{
        AppContext as _, InteractiveElement as _, MouseButton, ParentElement as _, ScrollDelta,
        ScrollEvent, Styled, div, px,
    };
    use gpui_component::{
        Root,
        input::{Editor, EditorState},
    };
    use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind};

    cx.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
    });
    struct EditorProbe(gpui::Entity<EditorState>);
    impl gpui::Render for EditorProbe {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            div()
                .id("editor-probe")
                .size_full()
                .child(Editor::new(&self.0).size_full())
        }
    }
    let mut editor = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let line = "fn symbol() { let value = \"a long line with several words for wrapping the source editor\"; }";
        let text = format!("{}\n{}", line.repeat(8), "fn below() {}\n".repeat(100));
        let state = cx.new(|cx| {
            EditorState::new(window, cx)
                .progressive_wrap()
                .soft_wrap(true)
                .default_value(text)
        });
        editor = Some(state.clone());
        let probe = cx.new(|_| EditorProbe(state));
        Root::new(probe, window, cx)
    });
    let editor = editor.unwrap();
    cx.simulate_scale_factor_change(1.);
    cx.simulate_resize(gpui::size(px(926.), px(700.)));
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let editor_layout = |cx: &mut gpui::VisualTestContext| {
        cx.update(|_, cx| {
            let state = editor.read(cx);
            (
                state.input_bounds(),
                state.projected_line_bounds(0).unwrap(),
                state.projected_line_bounds(1).unwrap(),
                state.visible_row_range(),
                state.scroll_offset(),
                state.display_map().wrap_row_count(),
            )
        })
    };
    let initial_layout = editor_layout(cx);
    assert!(initial_layout.1.size.height > initial_layout.2.size.height);
    let layout_notifications = Rc::new(Cell::new(0));
    let _observe_layout = cx.update(|_, cx| {
        let notifications = layout_notifications.clone();
        cx.observe(&editor, move |_, _| {
            notifications.set(notifications.get() + 1)
        })
    });
    let symbol = cx.update(|_, cx| editor.read(cx).range_to_bounds(&(3..9)).unwrap());
    cx.simulate_mouse_move(symbol.center(), None, Default::default());
    let show_hover = |cx: &mut gpui::VisualTestContext, body: String| {
        cx.update(|_, cx| {
            editor.update(cx, |editor, cx| {
                editor.present_hover(
                    3..9,
                    Hover {
                        range: None,
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: body,
                        }),
                    },
                    cx,
                );
            });
        });
        layout_notifications.set(0);
    };
    let popup = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| {
            // Hover anchors are derived from the last editor layout.
            window.draw(cx).clear(cx);
            window
                .painted_quads()
                .iter()
                .rev()
                .find(|quad| {
                    quad.bounds.size.width.0 >= 440.
                        && quad.bounds.size.width.0 <= 560.
                        && quad.bounds.size.height.0 > 20.
                })
                .expect("hover surface")
                .bounds
        })
    };
    show_hover(cx, "Documentation for symbol.".into());
    let short = popup(cx);
    assert_eq!(editor_layout(cx), initial_layout);
    assert_eq!(layout_notifications.get(), 0);

    let copy_hover_selection = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(String::new()));
            window.dispatch_action(Box::new(gpui_component::input::Copy), cx);
        });
        cx.update(|_, cx| {
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default()
        })
    };
    let start = point(px(short.left().0 + 20.), px(short.top().0 + 16.));
    let drag_to = |x| point(px(short.left().0 + x), start.y);
    cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
    cx.simulate_mouse_move(drag_to(70.), MouseButton::Left, Default::default());
    let selected = copy_hover_selection(cx);
    assert!(!selected.is_empty());
    cx.simulate_mouse_move(drag_to(180.), None, Default::default());
    assert_eq!(copy_hover_selection(cx), selected);
    cx.simulate_mouse_move(drag_to(160.), MouseButton::Left, Default::default());
    assert_eq!(copy_hover_selection(cx), selected);

    show_hover(
        cx,
        (0..40)
            .map(|i| format!("Paragraph {i:02}. Documentation for symbol.\n\n"))
            .collect(),
    );
    let long = popup(cx);
    assert_eq!(editor_layout(cx), initial_layout);
    assert_eq!(layout_notifications.get(), 0);
    assert_eq!(long.size.width, short.size.width);
    assert!(long.size.height.0 > short.size.height.0 && long.size.height.0 <= 362.);
    let thumb = |cx: &mut gpui::VisualTestContext| {
        cx.update(|window, _| {
            window
                .painted_quads()
                .iter()
                .rev()
                .find(|quad| {
                    quad.bounds.right().0 > long.right().0 - 15.
                        && quad.bounds.right().0 <= long.right().0
                        && quad.bounds.size.width.0 >= 3.
                        && quad.bounds.size.width.0 <= 10.
                        && quad.bounds.size.height.0 >= 20.
                        && quad.bounds.size.height.0 < long.size.height.0
                        && quad
                            .background
                            .as_solid()
                            .is_some_and(|color| color.a > 0.1)
                })
                .expect("overlay thumb")
                .bounds
        })
    };
    let initial_thumb = thumb(cx);
    assert_eq!(initial_thumb.size.width, ScaledPixels(4.));
    let target = point(px(long.left().0 + 24.), px(long.top().0 + 40.));
    cx.simulate_mouse_move(target, None, Default::default());
    cx.simulate_event(ScrollEvent {
        position: target,
        delta: ScrollDelta::Pixels(point(px(0.), px(-150.))),
        ..Default::default()
    });
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        assert!(editor.read(cx).hover_popover().is_some());
        assert_eq!(editor.read(cx).range_to_bounds(&(3..9)).unwrap(), symbol);
    });
    assert_eq!(popup(cx), long);
    assert!(thumb(cx).top() > initial_thumb.top());
    assert_eq!(editor_layout(cx), initial_layout);
    cx.update(|_, cx| {
        editor.update(cx, |editor, cx| editor.clear_hover_state(cx));
    });
    layout_notifications.set(0);
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(editor_layout(cx), initial_layout);
    assert_eq!(layout_notifications.get(), 0);
}

#[cfg(target_os = "linux")]
#[gpui::test]
fn hover_code_blocks_keep_the_popover_surface(cx: &mut gpui::TestAppContext) {
    use gpui::{
        AppContext as _, InteractiveElement as _, IntoElement, ParentElement as _, Render,
        Styled as _, Window, div, px, size,
    };
    use gpui_component::{
        ActiveTheme as _, Root, ThemeMode,
        input::{Editor, EditorState},
    };
    use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind};
    use stroll::workspace::apply_theme;

    cx.update(|cx| {
        gpui_component::init(cx);
        apply_theme(ThemeMode::Dark, cx);
        cx.set_reduce_motion(true);
    });

    struct EditorProbe(gpui::Entity<EditorState>);

    impl Render for EditorProbe {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            div()
                .id("editor-probe")
                .size_full()
                .child(Editor::new(&self.0).size_full())
        }
    }

    let mut editor = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| EditorState::new(window, cx).default_value("fn symbol() {}\n"));
        editor = Some(state.clone());
        let probe = cx.new(|_| EditorProbe(state));
        Root::new(probe, window, cx)
    });
    let editor = editor.unwrap();
    cx.simulate_scale_factor_change(1.);
    cx.simulate_resize(size(px(900.), px(600.)));
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.update(|_, cx| {
        editor.update(cx, |editor, cx| {
            editor.present_hover(
                3..9,
                Hover {
                    range: None,
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: "```rust\nfn symbol() {}\n```\n".to_owned(),
                    }),
                },
                cx,
            );
        });
    });

    let code_block_uses_editor_background = cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        let surface = window
            .painted_quads()
            .iter()
            .rev()
            .find(|quad| {
                quad.bounds.size.width.0 >= 440.
                    && quad.bounds.size.width.0 <= 560.
                    && quad.bounds.size.height.0 > 20.
            })
            .expect("hover surface")
            .bounds;
        let editor_background = cx
            .theme()
            .highlight_theme
            .style
            .editor_background
            .expect("dark code theme editor background");
        window.painted_quads().iter().any(|quad| {
            surface.contains(&quad.bounds.origin)
                && surface.contains(&quad.bounds.bottom_right())
                && quad
                    .background
                    .as_solid()
                    .is_some_and(|color| color.a > 0.5 && color == editor_background)
        })
    });
    assert!(
        !code_block_uses_editor_background,
        "hover code blocks must not paint the editor background"
    );
}

#[cfg(target_os = "linux")]
#[gpui::test]
fn dropdown_does_not_rebuild_while_closing(cx: &mut gpui::TestAppContext) {
    use std::{cell::Cell, rc::Rc};

    use gpui::{Context, IntoElement, Render, Styled, VisualTestContext, Window, px};
    use gpui_component::{
        button::Button,
        menu::{DropdownMenu, PopupMenuItem},
    };

    struct Dropdown(Rc<Cell<usize>>);

    impl Render for Dropdown {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let builds = self.0.clone();
            Button::new("dropdown")
                .label("Theme")
                .w(px(100.))
                .h(px(30.))
                .dropdown_menu(move |menu, _, _| {
                    builds.set(builds.get() + 1);
                    menu.item(PopupMenuItem::new("System"))
                })
        }
    }

    cx.update(gpui_component::init);
    let builds = Rc::new(Cell::new(0));
    let (_, cx) = cx.add_window_view({
        let builds = builds.clone();
        move |_, _| Dropdown(builds)
    });
    let draw = |cx: &mut VisualTestContext| {
        VisualTestContext::update(cx, |window, cx| window.draw(cx).clear(cx));
    };
    let trigger = point(px(12.), px(12.));
    draw(cx);
    cx.simulate_click(trigger, Default::default());
    draw(cx);
    assert_eq!(builds.get(), 1);

    std::thread::sleep(Duration::from_millis(160));
    cx.simulate_keystrokes("escape");
    draw(cx);
    assert_eq!(builds.get(), 1, "the exit frame must keep the old menu");

    cx.simulate_click(trigger, Default::default());
    draw(cx);
    assert_eq!(builds.get(), 2, "reopening must rebuild the menu");
}

#[cfg(target_os = "linux")]
#[test]
fn gpu_refreshes_cached_backdrops_only_when_their_inputs_change() -> anyhow::Result<()> {
    use gpui::{BackdropInput, DevicePixels, PlatformHeadlessRenderer};
    use gpui_wgpu::WgpuHeadlessRenderer;

    let mut renderer = WgpuHeadlessRenderer::new()?;
    let viewport = size(DevicePixels(160), DevicePixels(96));
    let region = bounds(0., 0., 160., 96.);
    let popup = bounds(40., 12., 80., 72.);
    let make_scene = |back_color, front_color, radius| {
        let mut scene = Scene::default();
        scene.insert_primitive(solid(region, back_color));
        scene.insert_primitive(solid(bounds(80., 0., 80., 96.), 0xffffff));
        let mut effect = blur(popup);
        effect.radius = ScaledPixels(radius);
        scene.insert_backdrop_blur(effect);
        scene.insert_primitive(solid(bounds(50., 26., 8., 8.), front_color));
        scene.finish();
        scene
    };
    let first = make_scene(0, 0xff0000, 20.);
    let second = make_scene(0, 0x0000ff, 20.);
    let third = make_scene(0xff0000, 0x0000ff, 20.);
    let fourth = make_scene(0xff0000, 0x0000ff, 4.);
    let mut first_input = BackdropInput::default();
    let mut input = BackdropInput::default();
    assert!(first.backdrop_input(first.backdrop_blurs[0], region, &mut first_input));
    assert!(second.backdrop_input(second.backdrop_blurs[0], region, &mut input));
    assert!(
        first_input == input,
        "changing foreground preserves the backdrop"
    );
    assert!(third.backdrop_input(third.backdrop_blurs[0], region, &mut input));
    assert!(
        first_input != input,
        "changing the background invalidates the backdrop"
    );

    let first = renderer.render_scene_to_image(&first, viewport)?;
    let second = renderer.render_scene_to_image(&second, viewport)?;
    let third = renderer.render_scene_to_image(&third, viewport)?;
    let fourth = renderer.render_scene_to_image(&fourth, viewport)?;
    assert_eq!(first.get_pixel(70, 50), second.get_pixel(70, 50));
    assert_eq!(second.get_pixel(54, 30).0, [0, 0, 255, 255]);
    assert!(third.get_pixel(70, 50).0[0] > second.get_pixel(70, 50).0[0]);
    assert!(third.get_pixel(70, 50).0[1] > fourth.get_pixel(70, 50).0[1]);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn cropped_path_scratch_keeps_screen_coordinates_and_paint_order() -> anyhow::Result<()> {
    use gpui::{DevicePixels, PathBuilder, PlatformHeadlessRenderer, px};
    use gpui_wgpu::WgpuHeadlessRenderer;

    let rectangle = |x, y, w, h, clip, color| {
        let mut builder = PathBuilder::fill();
        builder.add_polygon(
            &[
                point(px(x), px(y)),
                point(px(x + w), px(y)),
                point(px(x + w), px(y + h)),
                point(px(x), px(y + h)),
            ],
            true,
        );
        let mut path = builder.build().unwrap().scale(1.);
        path.content_mask = ContentMask { bounds: clip };
        path.color = rgb(color).into();
        path
    };

    let screen = bounds(0., 0., 256., 224.);
    let mut scene = Scene::default();
    scene.insert_primitive(solid(screen, 0));
    scene.insert_primitive(rectangle(
        95.,
        75.,
        135.,
        35.,
        bounds(108., 81., 100., 30.),
        0xff0000,
    ));
    scene.insert_primitive(solid(bounds(122., 83., 60., 12.), 0xffffff));
    scene.insert_primitive(rectangle(140., 88., 26., 16., screen, 0x0000ff));
    scene.finish();

    let mut renderer = WgpuHeadlessRenderer::new()?;
    let image =
        renderer.render_scene_to_image(&scene, size(DevicePixels(256), DevicePixels(224)))?;
    assert_eq!(image.get_pixel(107, 83).0, [0, 0, 0, 255]);
    assert_eq!(image.get_pixel(110, 83).0, [255, 0, 0, 255]);
    assert_eq!(image.get_pixel(130, 90).0, [255, 255, 255, 255]);
    assert_eq!(image.get_pixel(150, 90).0, [0, 0, 255, 255]);
    assert_eq!(image.get_pixel(150, 106).0, [255, 0, 0, 255]);
    assert_eq!(image.get_pixel(209, 106).0, [0, 0, 0, 255]);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn partial_popup_repaint_matches_full_frames_when_rows_and_backdrop_change() -> anyhow::Result<()> {
    use gpui::{DevicePixels, PlatformHeadlessRenderer};
    use gpui_wgpu::WgpuHeadlessRenderer;

    let viewport = size(DevicePixels(200), DevicePixels(120));
    let screen = bounds(0., 0., 200., 120.);
    let results = bounds(41., 31., 62., 55.);
    let popup = bounds(29., 18., 132., 80.);
    let make_scene = |partial, row_y, background, border| {
        let mut scene = Scene::default();
        scene.insert_primitive(solid(screen, background));
        scene.insert_primitive(solid(bounds(80., 0., 120., 120.), 0x808080));
        scene.insert_backdrop_blur(blur(popup));
        let mut tint = solid(popup, 0xffffcc);
        tint.background = gpui::Hsla::from(gpui::rgba(0xffffcc80)).into();
        scene.insert_primitive(tint);
        if partial {
            scene.begin_partial_region(results);
        }
        let mut row = solid(bounds(36., row_y, 73., 12.), 0x1050ff);
        row.content_mask = ContentMask { bounds: results };
        row.background = gpui::Hsla::from(gpui::rgba(0x1050ff73)).into();
        scene.insert_primitive(row);
        if partial {
            scene.end_partial_region();
        }
        let mut line = solid(bounds(99., 28., 2., 61.), border);
        line.background = gpui::Hsla::from(gpui::rgba((border << 8) | 0xcc)).into();
        scene.insert_primitive(line);
        scene.finish();
        scene
    };
    let mut renderer = WgpuHeadlessRenderer::new()?;
    let mut full = WgpuHeadlessRenderer::new()?;
    for (row, background, border) in [
        (34., 0xffffff, 0x008000),
        (58., 0xffffff, 0x008000),
        (70., 0x551177, 0x008000),
        (72., 0x551177, 0x992200),
        (46., 0x551177, 0x992200),
    ] {
        let actual =
            renderer.render_scene_to_image(&make_scene(true, row, background, border), viewport)?;
        let expected =
            full.render_scene_to_image(&make_scene(false, row, background, border), viewport)?;
        assert_eq!(
            actual.as_raw(),
            expected.as_raw(),
            "popup frame at row {row}"
        );
    }
    let closed = {
        let mut scene = Scene::default();
        scene.insert_primitive(solid(screen, 0x123456));
        scene.finish();
        renderer.render_scene_to_image(&scene, viewport)?
    };
    assert_eq!(closed.get_pixel(55, 50).0, [0x12, 0x34, 0x56, 255]);
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn scroll_repaint_matches_full_render_with_fixed_overlays_and_changing_rows() -> anyhow::Result<()>
{
    use gpui::{DevicePixels, Hsla, PlatformHeadlessRenderer};
    use gpui_wgpu::WgpuHeadlessRenderer;

    let viewport = size(DevicePixels(448), DevicePixels(432));
    let screen = bounds(0., 0., 448., 432.);
    let area = bounds(32., 24., 384., 384.);
    let make_scene = |partial, offset, changed, opaque: bool| {
        let mut scene = Scene::default();
        scene.insert_primitive(solid(screen, 0xccccee));
        if partial {
            scene.begin_scroll_region(area, Some(ScaledPixels(offset)));
        }
        let mut base = solid(area, 0xffffff);
        if !opaque {
            base.background = Hsla::from(gpui::rgba(0xffffff99)).into();
        }
        scene.insert_primitive(base);
        for row in 0..22 {
            let mut bar = solid(
                bounds(40., 25. + row as f32 * 20. + offset, 260., 12.),
                if changed && row == 9 {
                    0xdd4010
                } else {
                    0x00bb55
                },
            );
            bar.content_mask = ContentMask { bounds: area };
            bar.background = bar.background.opacity(0.45);
            scene.insert_primitive(bar);
        }
        // Simulate rasterization of rounded table corners and disjoint changes at opposite ends of the content.
        for (x, y) in [(55., 71.), (329., 162.), (63., 291.)] {
            let point = |x, y| gpui::point(gpui::px(x), gpui::px(y));
            let mut builder = gpui::PathBuilder::fill();
            builder.move_to(point(x, y + offset));
            builder.line_to(point(x + 17., y + offset));
            builder.arc_to(
                point(17., 17.),
                gpui::px(0.),
                false,
                false,
                point(x, y + 17. + offset),
            );
            builder.close();
            let mut path = builder.build().unwrap().scale(1.);
            path.color = gpui::rgb(0x7321bb).into();
            path.content_mask = ContentMask { bounds: area };
            scene.insert_primitive(path);
        }
        if partial {
            scene.end_partial_region();
        }
        let mut overlay = solid(
            bounds(35., 90. + if changed { 11. } else { 0. }, 45., 20.),
            0x440044,
        );
        overlay.background = overlay.background.opacity(0.5);
        scene.insert_primitive(overlay);
        scene.finish();
        scene
    };
    let mut retained = WgpuHeadlessRenderer::new()?;
    let mut full = WgpuHeadlessRenderer::new()?;
    for (offset, changed, opaque) in [
        (0., false, true),
        (-24., false, true),
        (-45., true, true),
        (-45.7, true, true),
        (-21., false, true),
        (-21., false, false),
        (-44., false, false),
        (-47., false, true),
    ] {
        let actual =
            retained.render_scene_to_image(&make_scene(true, offset, changed, opaque), viewport)?;
        let expected =
            full.render_scene_to_image(&make_scene(false, offset, changed, opaque), viewport)?;
        for (position, (a, b)) in actual.pixels().zip(expected.pixels()).enumerate() {
            assert_eq!(a, b, "pixel {position} at scroll offset {offset}");
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn retained_document_and_live_backdrop_match_full_repaint() -> anyhow::Result<()> {
    use gpui::{DevicePixels, PartialFrame, PlatformHeadlessRenderer};
    use gpui_wgpu::WgpuHeadlessRenderer;

    let viewport = size(DevicePixels(520), DevicePixels(480));
    let screen = bounds(0., 0., 520., 480.);
    let document = bounds(60., 64., 430., 380.);
    let results = bounds(30., 84., 175., 245.);
    let make_scene = |partial, offset, result_y, popup_opacity, overlaps| {
        let mut scene = Scene::default();
        scene.insert_primitive(solid(screen, 0xddddff));
        if partial {
            scene.begin_scroll_region(document, Some(ScaledPixels(offset)));
        }
        scene.insert_primitive(solid(document, 0xffffff));
        for row in 0..21 {
            let mut line = solid(
                bounds(88., 72. + row as f32 * 24. + offset, 310., 14.),
                if row % 2 == 0 { 0x213360 } else { 0xc84040 },
            );
            line.content_mask = ContentMask { bounds: document };
            scene.insert_primitive(line);
        }
        if partial {
            scene.end_partial_region();
        }
        if popup_opacity > 0. {
            let fixed = if overlaps {
                bounds(91., 121., 98., 13.)
            } else {
                bounds(91., 49., 98., 11.)
            };
            scene.insert_primitive(solid(fixed, 0x226622));
            let popup = bounds(21., 40., 220., 310.);
            let mut effect = blur(popup);
            effect.radius = ScaledPixels(40.);
            effect.opacity = popup_opacity;
            scene.insert_backdrop_blur(effect);
            let mut tint = solid(popup, 0xfefede);
            tint.background = tint.background.opacity(popup_opacity * 0.5);
            scene.insert_primitive(tint);
            if partial {
                scene.begin_partial_region(results);
            }
            let mut row = solid(bounds(36., result_y, 128., 16.), 0x1155ff);
            row.content_mask = ContentMask { bounds: results };
            scene.insert_primitive(row);
            if partial {
                scene.end_partial_region();
            }
        }
        scene.finish();
        scene
    };
    let mut retained = WgpuHeadlessRenderer::new()?;
    let mut full = WgpuHeadlessRenderer::new()?;
    for (offset, row, opacity, overlaps) in [
        (0., 102., 1., false),
        (-13., 102., 1., false),
        (-13., 134., 1., false),
        (-26., 150., 0.4, true),
        (-26., 150., 0.4, false),
        (-26., 150., 0., false),
        (-42., 150., 1., false),
    ] {
        let scene = make_scene(true, offset, row, opacity, overlaps);
        let (_, _, overlay) = scene.partial_frame(&mut PartialFrame::default()).unwrap();
        assert_eq!(overlay.is_some(), opacity > 0.);
        let actual = retained.render_scene_to_image(&scene, viewport)?;
        if opacity > 0. && !overlaps {
            assert_eq!(overlay, Some(scene.backdrop_blurs[0].order));
        }
        let expected = full
            .render_scene_to_image(&make_scene(false, offset, row, opacity, overlaps), viewport)?;
        for (position, (a, b)) in actual.pixels().zip(expected.pixels()).enumerate() {
            assert_eq!(
                a, b,
                "pixel {position} at scroll offset {offset}, row {row}"
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn popover_arrow_joins_the_material_at_fractional_scales() -> anyhow::Result<()> {
    use std::{cell::Cell, rc::Rc, sync::Arc};

    use gpui::{
        Anchor, AppContext as _, Context, DevicePixels, HeadlessAppContext, IntoElement,
        ParentElement as _, Pixels, PlatformAtlas, PlatformHeadlessRenderer, Render, Size,
        Styled as _, Window, div, px,
    };
    use gpui_component::{ActiveTheme as _, Root, ThemeMode, button::Button, popover::Popover};
    use gpui_wgpu::WgpuHeadlessRenderer;
    use image::RgbaImage;

    const SIZE: Size<Pixels> = Size {
        width: px(420.),
        height: px(260.),
    };

    type ArrowGeometry = (gpui::Point<ScaledPixels>, ScaledPixels);

    struct Renderer {
        wgpu: WgpuHeadlessRenderer,
        scale: Rc<Cell<f32>>,
        arrow: Rc<Cell<Option<ArrowGeometry>>>,
    }

    impl PlatformHeadlessRenderer for Renderer {
        fn render_scene_to_image(
            &mut self,
            scene: &Scene,
            _: Size<DevicePixels>,
        ) -> anyhow::Result<RgbaImage> {
            let scale = self.scale.get();
            let arrow = scene
                .paths
                .iter()
                .find(|path| {
                    // The transparent material uses a separate triangle fill and a single closed outline.
                    let width = path.bounds.size.width.0 / scale;
                    path.color.as_solid().is_some_and(|color| color.a == 0.5)
                        && (4.5..9.).contains(&width)
                })
                .and_then(|path| {
                    let vertices = || path.vertices.iter().map(|vertex| vertex.xy_position);
                    let tip = vertices().min_by(|a, b| a.x.0.total_cmp(&b.x.0))?;
                    let base = vertices().max_by(|a, b| a.x.0.total_cmp(&b.x.0))?.x;
                    Some((tip, base))
                });
            if let Some((_, base)) = arrow {
                assert!(
                    scene.backdrop_blurs.iter().any(|blur| {
                        (blur.bounds.left().0 - base.0).abs() <= 1.
                            && (blur.radius.0 / scale - 20.).abs() < 0.01
                            && (blur.bounds.bottom().0 - (SIZE.height.as_f32() - 8.) * scale).abs()
                                <= 2.
                    }),
                    "the arrow meets the default backdrop at the clamped edge: {scale}x, base {base:?}, blurs {:?}",
                    scene
                        .backdrop_blurs
                        .iter()
                        .map(|blur| (blur.bounds, blur.radius))
                        .collect::<Vec<_>>()
                );
            }
            self.arrow.set(arrow);
            self.wgpu
                .render_scene_to_image(scene, SIZE.to_device_pixels(scale))
        }

        fn render_scene(&mut self, scene: &Scene, _: Size<DevicePixels>) -> anyhow::Result<()> {
            self.wgpu
                .render_scene(scene, SIZE.to_device_pixels(self.scale.get()))
        }

        fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
            self.wgpu.sprite_atlas()
        }
    }

    struct ArrowPopover;

    impl Render for ArrowPopover {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let background = if cx.theme().mode.is_dark() {
                0x444444
            } else {
                0xb0b0b0
            };
            div().size_full().bg(rgb(background)).child(
                div().absolute().left(px(24.)).top(px(206.)).child(
                    Popover::new("arrow-probe")
                        .default_open(true)
                        .anchor(Anchor::LeftCenter)
                        .offset(px(10.))
                        .arrow(true)
                        .w(px(190.))
                        .scrollable(false)
                        .trigger(Button::new("arrow-trigger").label("Open"))
                        .content(|_, _, _| div().h(px(76.)).child("Popover with an arrow")),
                ),
            )
        }
    }

    let scale = Rc::new(Cell::new(1.));
    let arrow = Rc::new(Cell::new(None));
    let capture_scale = scale.clone();
    let capture_arrow = arrow.clone();
    let platform = gpui_platform::current_platform(true);
    let mut cx =
        HeadlessAppContext::with_platform(platform.text_system(), Arc::new(()), move || {
            Ok(Some(Box::new(Renderer {
                wgpu: WgpuHeadlessRenderer::new()?,
                scale: capture_scale.clone(),
                arrow: capture_arrow.clone(),
            })))
        });
    cx.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
    });
    let handle = cx.open_window(SIZE, |window, cx| {
        let view = cx.new(|_| ArrowPopover);
        cx.new(|cx| Root::new(view, window, cx))
    })?;
    let output = std::env::var_os("STROLL_HISTORY_SCREENSHOTS").map(std::path::PathBuf::from);

    for (mode, theme) in [(ThemeMode::Light, "light"), (ThemeMode::Dark, "dark")] {
        for factor in [1., 1.25, 1.5, 2.] {
            scale.set(factor);
            cx.update_window(handle.into(), |_, window, cx| {
                stroll::workspace::apply_theme(mode, cx);
                window.set_scale_factor(factor);
                window.draw(cx).clear(cx);
                window.refresh();
                window.draw(cx).clear(cx);
            })?;
            let shot = cx.capture_screenshot(handle.into())?;
            if let Some(output) = &output {
                std::fs::create_dir_all(output)?;
                shot.save(output.join(format!(
                    "popover-arrow-{theme}-{}.png",
                    (factor * 100.) as u32
                )))?;
            }
            let (tip, base) = arrow
                .get()
                .expect("the translucent popover paints its arrow");
            let x = base.0.round() as u32;
            let y = tip.y.0.round() as u32;
            let outer = shot.get_pixel(x - 3, y).0;
            let inner = shot.get_pixel(x + 3, y).0;
            for column in x - 2..=x + 2 {
                let pixel = shot.get_pixel(column, y).0;
                for channel in 0..3 {
                    let range = (outer[channel].min(inner[channel]).saturating_sub(4))
                        ..=(outer[channel].max(inner[channel]).saturating_add(4));
                    assert!(
                        range.contains(&pixel[channel]),
                        "a seam at the arrow join at {theme} {factor}x ({column}, {y}), base {x}: {pixel:?}, {outer:?}, {inner:?}"
                    );
                }
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn completion_popup_stays_in_view_and_tab_accepts_the_selected_row() -> anyhow::Result<()> {
    use std::{path::PathBuf, sync::Arc};

    use gpui::{
        AppContext as _, Context, Entity, HeadlessAppContext, Keystroke, Render, Styled as _,
        Window, px,
    };
    use gpui_component::{
        ActiveTheme as _, Root, ThemeMode,
        input::{Editor, EditorState},
    };
    use gpui_wgpu::WgpuHeadlessRenderer;
    use lsp_types::{CompletionItem, CompletionItemKind, Documentation, MarkupContent, MarkupKind};

    struct Probe(Entity<EditorState>, f32);
    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            Editor::new(&self.0).text_size(px(self.1)).size_full()
        }
    }

    const SOURCE: &str = concat!(
        "#include <optional>\n",
        "#include \"records.h\"\n",
        "\n",
        "namespace app {\n",
        "\n",
        "void load(RecordLibrary &library) {\n",
        "    const std::size_t id = 42;\n",
        "    if (!library.isOpen())\n",
        "        return;\n",
        "\n",
        "    const auto record = library.ge;\n",
        "}\n",
        "\n",
        "} // namespace app\n",
    );
    const LONG_NAME: &str = "getRecordWithAnExplicitScopeAndAccessPolicy";
    let documentation = Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value: concat!(
            "**Find a record by its identifier.**\n\n",
            "Returns an empty result if the record is missing or the library is closed.\n\n",
            "```cpp\n",
            "library.getRecord(id);\n",
            "```\n\n",
            "This lookup leaves the caller's current selection intact.",
        )
        .to_owned(),
    });
    let items = vec![
        CompletionItem {
            label: "getRecord".into(),
            kind: Some(CompletionItemKind::METHOD),
            detail: Some("std::optional<Record>".into()),
            documentation: Some(documentation),
            ..Default::default()
        },
        CompletionItem {
            label: LONG_NAME.into(),
            kind: Some(CompletionItemKind::FUNCTION),
            detail: Some("std::optional<ScopedRecordWithAccessPolicy>".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "generation".into(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some("std::size_t".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "genericLookup".into(),
            kind: Some(CompletionItemKind::METHOD),
            deprecated: Some(true),
            ..Default::default()
        },
    ];
    let output = std::env::var_os("STROLL_COMPLETION_SCREENSHOTS").map(PathBuf::from);
    if let Some(output) = &output {
        std::fs::create_dir_all(output)?;
    }
    let mut cx = HeadlessAppContext::with_platform(
        gpui_platform::current_platform(true).text_system(),
        Arc::new(gpui_kit_assets::AllAssets),
        || Ok(Some(Box::new(WgpuHeadlessRenderer::new()?))),
    );
    cx.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
    });

    let mut previous_list_height: Option<f32> = None;
    for (mode, width, height, font_size, stacked, name) in [
        (ThemeMode::Light, 950., 420., 15., false, "light"),
        (ThemeMode::Dark, 950., 420., 17., false, "dark"),
        (ThemeMode::Dark, 344., 300., 15., true, "narrow-dark"),
    ] {
        cx.update(|cx| stroll::workspace::apply_theme(mode, cx));
        let mut editor = None;
        let window = cx.open_window(size(px(width), px(height)), |window, cx| {
            let state = cx.new(|cx| {
                EditorState::new(window, cx)
                    .language("cpp")
                    .default_value(SOURCE)
            });
            editor = Some(state.clone());
            let view = cx.new(|_| Probe(state, font_size));
            cx.new(|cx| Root::new(view, window, cx))
        })?;
        let window = window.into();
        let editor = editor.unwrap();
        let caret = SOURCE.find("ge;").unwrap() + 2;
        cx.update_window(window, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.focus(window, cx);
                editor.set_selected_range(caret..caret, cx);
                editor.present_completion_items(caret - 2, "ge", items.clone(), cx);
            });
            window.draw(cx).clear(cx);
        })?;
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.draw(cx).clear(cx);
            let scale = window.scale_factor();
            let material = cx.theme().popover.alpha(0.5).into();
            let mut shells = window
                .painted_quads()
                .into_iter()
                .filter(|quad| quad.background == material && quad.corner_radii.top_left.0 > 0.)
                .map(|quad| quad.bounds)
                .collect::<Vec<_>>();
            let start = |area: &Bounds<ScaledPixels>| {
                if stacked { area.top().0 } else { area.left().0 }
            };
            shells.sort_by(|a, b| start(a).total_cmp(&start(b)));
            shells.dedup();
            assert_eq!(
                shells.len(),
                2,
                "completion list and documentation in {name}: {shells:?}"
            );
            for shell in &shells {
                assert!(shell.left().0 >= 7. * scale && shell.top().0 >= 7. * scale);
                assert!(shell.right().0 <= (width - 7.) * scale);
                assert!(shell.bottom().0 <= (height - 7.) * scale);
            }
            let caret = editor.read(cx).prepaint_caret_geometry().unwrap();
            let caret_y = f32::from(caret.cursor_bounds().top() + caret.scroll_offset().y) * scale;
            let stacked_above = stacked && shells[1].bottom().0 <= caret_y;
            let (list, docs) = if stacked_above {
                (shells[1], shells[0])
            } else {
                (shells[0], shells[1])
            };
            if !stacked {
                let height = list.size.height.0 / scale;
                if let Some(previous) = previous_list_height {
                    assert!(
                        (height - previous - 8.0_f32).abs() < 1.,
                        "four rows should grow by 2px each"
                    );
                }
                previous_list_height = Some(height);
            }
            if stacked {
                assert_eq!(list.left(), docs.left());
                if stacked_above {
                    assert!(docs.bottom() < list.top());
                } else {
                    assert!(list.bottom() < docs.top());
                }
            } else {
                if list.bottom().0 <= caret_y {
                    assert_eq!(list.bottom(), docs.bottom());
                } else {
                    assert_eq!(list.top(), docs.top());
                }
                assert!(list.right() < docs.left());
            }
            assert!(docs.size.height.0 > 48. * scale);
        })?;
        if let Some(output) = &output {
            cx.capture_screenshot(window)?
                .save(output.join(format!("completion-{name}.png")))?;
        }
        cx.update_window(window, |_, window, cx| {
            window.dispatch_keystroke(Keystroke::parse("down").unwrap(), cx);
            window.dispatch_keystroke(Keystroke::parse("tab").unwrap(), cx);
        })?;
        cx.run_until_parked();
        cx.update(|cx| {
            let editor = editor.read(cx);
            assert!(!editor.completion_menu_state().open);
            assert_eq!(
                editor.text().to_string(),
                SOURCE.replace(".ge;", &format!(".{LONG_NAME};"))
            );
        });
    }
    Ok(())
}

#[gpui::test]
fn completion_popup_keeps_list_geometry_through_documentation_changes(
    cx: &mut gpui::TestAppContext,
) {
    use gpui::{
        AppContext as _, Context, Entity, Render, ScrollDelta, ScrollEvent, Styled as _,
        VisualTestContext, Window, px,
    };
    use gpui_component::{
        Root,
        input::{Editor, EditorState},
    };
    use lsp_types::{CompletionItem, Documentation};

    struct Probe(Entity<EditorState>);
    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            Editor::new(&self.0).text_size(px(15.)).size_full()
        }
    }

    cx.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
    });
    let prefix = "other();\n".repeat(32);
    let start = prefix.len() + "batch.ops.".len();
    let mut editor = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        let source = format!("{prefix}batch.ops.foldl();\n{prefix}");
        let state = cx.new(|cx| EditorState::new(window, cx).default_value(source));
        editor = Some(state.clone());
        let view = cx.new(|_| Probe(state));
        Root::new(view, window, cx)
    });
    let editor = editor.unwrap();
    let candidates = (0..15)
        .map(|index| CompletionItem {
            label: format!("candidate{index:02}"),
            documentation: match index {
                0 => Some(Documentation::String("Short documentation.".into())),
                1 => Some(Documentation::String(
                    "A paragraph of documentation.\n\n".repeat(30),
                )),
                _ => None,
            },
            ..Default::default()
        })
        .collect::<Vec<_>>();
    let draw = |visual: &mut VisualTestContext| {
        for _ in 0..3 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
    };
    let present = |visual: &mut VisualTestContext, query: &str, items| {
        visual.update(|window, cx| {
            editor.update(cx, |state, cx| {
                state.focus(window, cx);
                state.present_completion_items(start, query, items, cx);
            });
        });
        draw(visual);
    };
    let scroll_row_to = |visual: &mut VisualTestContext, y| {
        visual.update(|_, cx| {
            editor.update(cx, |state, cx| {
                let row = state.projected_line_bounds(32).unwrap();
                let offset = state.scroll_offset();
                state.set_scroll_offset(point(offset.x, offset.y + px(y) - row.top()), cx);
            });
        });
        draw(visual);
    };
    let caret_y = |visual: &mut VisualTestContext| {
        visual.update(|_, cx| {
            let caret = editor.read(cx).prepaint_caret_geometry().unwrap();
            caret.cursor_bounds().top() + caret.scroll_offset().y
        })
    };
    let dismiss = |visual: &mut VisualTestContext| {
        visual.update(|_, cx| {
            editor.update(cx, |state, cx| state.dismiss_completion_overlay(cx));
        });
        draw(visual);
    };

    for (width, stacked) in [(950., false), (344., true)] {
        for (height, row, above) in [(420., 350., true), (650., 100., false)] {
            dismiss(visual);
            visual.simulate_resize(size(px(width), px(height)));
            draw(visual);
            visual.update(|_, cx| {
                editor.update(cx, |state, cx| {
                    state.set_selected_range(start..start, cx);
                })
            });
            draw(visual);
            scroll_row_to(visual, row);
            present(visual, "", candidates.clone());

            let list = visual.debug_bounds("completion-menu").unwrap();
            let y = caret_y(visual);
            if above {
                assert!(list.bottom() < y);
            } else {
                assert!(list.top() > y);
            }
            let assert_documentation = |visual: &mut VisualTestContext| {
                let docs = visual.debug_bounds("completion-documentation").unwrap();
                assert!(docs.top() >= px(7.) && docs.bottom() <= px(height - 7.));
                if stacked {
                    assert_eq!(docs.left(), list.left());
                    if above {
                        assert!(docs.bottom() < list.top());
                    } else {
                        assert!(list.bottom() < docs.top());
                    }
                } else {
                    assert!(list.right() < docs.left());
                    if above {
                        assert_eq!(docs.bottom(), list.bottom());
                    } else {
                        assert_eq!(docs.top(), list.top());
                    }
                }
                docs.size.height
            };
            let short = assert_documentation(visual);
            visual.simulate_keystrokes("down");
            draw(visual);
            let long = assert_documentation(visual);
            assert!(long > short);
            if !stacked {
                assert!(long > list.size.height);
            }
            assert_eq!(visual.debug_bounds("completion-menu"), Some(list));
            visual.simulate_event(ScrollEvent {
                position: list.center(),
                delta: ScrollDelta::Pixels(point(px(0.), px(-80.))),
                ..Default::default()
            });
            draw(visual);
            assert_eq!(visual.debug_bounds("completion-menu"), Some(list));
            assert_eq!(assert_documentation(visual), long);

            visual.simulate_keystrokes("down");
            draw(visual);
            assert!(visual.debug_bounds("completion-documentation").is_none());
            assert_eq!(visual.debug_bounds("completion-menu"), Some(list));
            visual.simulate_keystrokes("up");
            draw(visual);
            assert_eq!(assert_documentation(visual), long);
            assert_eq!(visual.debug_bounds("completion-menu"), Some(list));

            present(visual, "foldl", vec![candidates[2].clone()]);
            let narrowed = visual.debug_bounds("completion-menu").unwrap();
            assert!(narrowed.size.height < list.size.height);
            assert_eq!(narrowed.left(), list.left());
            if above {
                assert_eq!(narrowed.bottom(), list.bottom());
                // Below now has room for the single row. Neither resize nor scroll can flip it.
                visual.simulate_resize(size(px(width), px(650.)));
                draw(visual);
                assert!(visual.debug_bounds("completion-menu").unwrap().bottom() < caret_y(visual));
                scroll_row_to(visual, 100.);
                assert!(visual.debug_bounds("completion-menu").unwrap().bottom() < caret_y(visual));
                for scale in [1.25, 2.] {
                    visual.simulate_scale_factor_change(scale);
                    draw(visual);
                    assert!(
                        visual.debug_bounds("completion-menu").unwrap().bottom() < caret_y(visual)
                    );
                }
                // End the session when its original side can no longer hold a row.
                scroll_row_to(visual, 15.);
                assert!(visual.debug_bounds("completion-menu").is_none());
                assert!(!visual.update(|_, cx| editor.read(cx).completion_menu_state().open));
                present(visual, "foldl", vec![candidates[2].clone()]);
                assert!(visual.debug_bounds("completion-menu").unwrap().top() > caret_y(visual));
            } else {
                assert_eq!(narrowed.top(), list.top());
                scroll_row_to(visual, -40.);
                assert!(visual.debug_bounds("completion-menu").is_none());
                assert!(!visual.update(|_, cx| editor.read(cx).completion_menu_state().open));
            }
        }

        // Two rows fit below, even though the selected documentation is tall.
        // The narrow layout preserves both rows and omits documentation here.
        dismiss(visual);
        visual.simulate_resize(size(px(width), px(420.)));
        draw(visual);
        visual.update(|_, cx| {
            editor.update(cx, |state, cx| state.set_selected_range(start..start, cx));
        });
        draw(visual);
        scroll_row_to(visual, 318.);
        present(
            visual,
            "",
            vec![candidates[1].clone(), candidates[0].clone()],
        );
        let list = visual.debug_bounds("completion-menu").unwrap();
        assert!(list.top() > caret_y(visual));
        assert_eq!(
            visual.debug_bounds("completion-documentation").is_none(),
            stacked
        );
        visual.simulate_keystrokes("down");
        draw(visual);
        assert_eq!(visual.debug_bounds("completion-menu"), Some(list));

        // The reopen can arrive before GPUI draws a frame with the menu hidden.
        visual.update(|_, cx| {
            editor.update(cx, |state, cx| {
                state.dismiss_completion_overlay(cx);
                state.present_completion_items(start, "", candidates.clone(), cx);
            });
        });
        draw(visual);
        assert!(visual.debug_bounds("completion-menu").unwrap().bottom() < caret_y(visual));
        visual.simulate_keystrokes("escape");
        draw(visual);
        assert!(visual.debug_bounds("completion-menu").is_none());
        present(visual, "", vec![candidates[0].clone()]);
        let single = visual.debug_bounds("completion-menu").unwrap();
        assert!(single.top() > caret_y(visual));
        let last_fitting_row = visual.update(|window, cx| {
            let editor = editor.read(cx);
            let row = editor.projected_line_bounds(32).unwrap();
            let bottom = editor
                .visible_text_bounds()
                .bottom()
                .min(window.viewport_size().height - px(8.));
            f32::from(row.top() + bottom - single.bottom() - px(0.25))
        });
        scroll_row_to(visual, last_fitting_row);
        assert!(visual.debug_bounds("completion-menu").unwrap().top() > caret_y(visual));
        scroll_row_to(visual, last_fitting_row + 2.);
        assert!(visual.debug_bounds("completion-menu").is_none());
        assert!(!visual.update(|_, cx| editor.read(cx).completion_menu_state().open));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn editor_hover_appends_documentation_to_web_diagnostics() -> anyhow::Result<()> {
    use std::{cell::Cell, rc::Rc, sync::Arc};

    use gpui::{
        App, AppContext as _, Context, Entity, HeadlessAppContext, InputEvent as _, MouseButton,
        MouseDownEvent, MouseMoveEvent, MouseUpEvent, Render, Styled as _, Task, Window,
        linear_color_stop, linear_gradient, px,
    };
    use gpui_component::{
        ActiveTheme as _, Root, ThemeMode,
        input::{CodeActionProvider, Editor, EditorState, HoverProvider, Rope},
    };
    use gpui_wgpu::WgpuHeadlessRenderer;
    use lsp_types::{
        CodeAction, Diagnostic, DiagnosticSeverity as Severity, Hover, HoverContents, MarkedString,
    };

    const SOURCE: &str = concat!(
        "fn render_report() {\n",
        "    let report = render(report_id);\n",
        "    println!(\"{report}\");\n",
        "}\n",
    );
    const SYMBOL: std::ops::Range<usize> = 45..54;

    struct Documentation(Rc<Cell<Duration>>);
    impl HoverProvider for Documentation {
        fn hover(
            &self,
            _: &Rope,
            _: usize,
            _: &mut Window,
            cx: &mut App,
        ) -> Task<anyhow::Result<Option<Hover>>> {
            let timer = cx.background_executor().timer(self.0.get());
            cx.background_spawn(async move {
                timer.await;
                Ok(Some(Hover {
                    contents: HoverContents::Scalar(MarkedString::String(
                        "**Report identifier**\n\nThe identifier is used to look up a report."
                            .into(),
                    )),
                    range: None,
                }))
            })
        }
    }

    struct QuickFix;
    impl CodeActionProvider for QuickFix {
        fn id(&self) -> gpui::SharedString {
            "fixture".into()
        }

        fn code_actions(
            &self,
            _: Entity<EditorState>,
            range: std::ops::Range<usize>,
            _: &mut Window,
            _: &mut App,
        ) -> Task<anyhow::Result<Vec<CodeAction>>> {
            assert_eq!(range, SYMBOL);
            Task::ready(Ok(vec![CodeAction {
                title: "Use the report key".into(),
                ..Default::default()
            }]))
        }

        fn probe_current_line(&self) -> bool {
            true
        }

        fn probe_code_actions(
            &self,
            _: Entity<EditorState>,
            range: std::ops::Range<usize>,
            _: &mut Window,
            _: &mut App,
        ) -> Task<anyhow::Result<Vec<CodeAction>>> {
            Task::ready(Ok(if range.contains(&SYMBOL.start) {
                vec![CodeAction {
                    title: "Use the report key".into(),
                    ..Default::default()
                }]
            } else {
                Vec::new()
            }))
        }

        fn supports_preview(&self) -> bool {
            true
        }

        fn preview_code_action(
            &self,
            _: Entity<EditorState>,
            _: CodeAction,
            _: &mut Window,
            _: &mut App,
        ) -> Task<anyhow::Result<String>> {
            Task::ready(Ok("- report_id\n+ report_key".into()))
        }

        fn perform_code_action(
            &self,
            _: Entity<EditorState>,
            _: CodeAction,
            _: bool,
            _: &mut Window,
            _: &mut App,
        ) -> Task<anyhow::Result<()>> {
            Task::ready(Ok(()))
        }
    }

    struct Probe(Entity<EditorState>);
    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            Editor::new(&self.0).size_full()
        }
    }

    let mut cx = HeadlessAppContext::with_platform(
        gpui_platform::current_platform(true).text_system(),
        Arc::new(gpui_kit_assets::AllAssets),
        || Ok(Some(Box::new(WgpuHeadlessRenderer::new()?))),
    );
    cx.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
        stroll::workspace::apply_theme(ThemeMode::Light, cx);
    });
    let mut editor = None;
    let delay = Rc::new(Cell::new(Duration::from_millis(50)));
    let window = cx.open_window(size(px(760.), px(640.)), |window, cx| {
        let state = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language("rust")
                .default_value(SOURCE);
            state.lsp_mut().hover_provider = Some(Rc::new(Documentation(delay.clone())));
            // Deliberately mix severities and overlap unevenly. A short diagnostic
            // ending before the hit must not hide the enclosing ones.
            for (start, end, severity, message, source) in [
                (
                    4,
                    34,
                    Severity::HINT,
                    "Give the identifier a descriptive name.",
                    None,
                ),
                (
                    8,
                    11,
                    Severity::ERROR,
                    "Do not show this outside its range.",
                    None,
                ),
                (
                    11,
                    34,
                    Severity::ERROR,
                    "Could not compile this expression.",
                    None,
                ),
                (
                    17,
                    34,
                    Severity::WARNING,
                    "The fallback lookup is deprecated.",
                    Some("compiler"),
                ),
                (
                    24,
                    33,
                    Severity::INFORMATION,
                    "This lookup runs synchronously.",
                    None,
                ),
                (
                    24,
                    33,
                    Severity::ERROR,
                    "Cannot find `report_id` in this scope.",
                    Some("rustc"),
                ),
            ] {
                state.diagnostics_mut().unwrap().push(Diagnostic {
                    range: lsp_types::Range::new(
                        lsp_types::Position::new(1, start),
                        lsp_types::Position::new(1, end),
                    ),
                    severity: Some(severity),
                    source: source.map(str::to_owned),
                    message: message.into(),
                    ..Default::default()
                });
            }
            state
        });
        editor = Some(state.clone());
        let probe = cx.new(|_| Probe(state));
        cx.new(|cx| Root::new(probe, window, cx))
    })?;
    let window = window.into();
    let editor = editor.unwrap();
    let symbol = cx.update_window(window, |_, window, cx| {
        window.draw(cx).clear(cx);
        editor.read(cx).range_to_bounds(&SYMBOL).unwrap()
    })?;
    let move_to = |cx: &mut HeadlessAppContext, position| -> anyhow::Result<()> {
        cx.update_window(window, |_, window, cx| {
            window.dispatch_event(
                MouseMoveEvent {
                    position,
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
        })?;
        cx.run_until_parked();
        Ok(())
    };
    let assert_rows = |cx: &mut HeadlessAppContext, has_docs| {
        cx.update(|cx| {
            let hover = editor
                .read(cx)
                .hover_popover()
                .expect("diagnostics are visible");
            assert_eq!(
                hover
                    .diagnostics
                    .iter()
                    .map(|row| row.message.as_str())
                    .collect::<Vec<_>>(),
                [
                    "Cannot find `report_id` in this scope.",
                    "Could not compile this expression.",
                    "The fallback lookup is deprecated.",
                    "This lookup runs synchronously.",
                    "Give the identifier a descriptive name.",
                ]
            );
            assert_eq!(hover.hover.is_some(), has_docs);
        });
    };
    let popup = |cx: &mut HeadlessAppContext| {
        cx.update_window(window, |_, window, cx| {
            window.draw(cx).clear(cx);
            let theme = &cx.theme().highlight_theme.style.status;
            let colors = [
                theme.error(cx),
                theme.error(cx),
                theme.warning(cx),
                theme.info(cx),
                theme.hint(cx),
            ];
            let gradients = colors.map(|tone| {
                linear_gradient(
                    90.,
                    linear_color_stop(tone.alpha(0.12), 0.),
                    linear_color_stop(tone.alpha(0.), 0.75),
                )
            });
            let scale = window.scale_factor();
            let quads = window.painted_quads();
            let mut rows = quads
                .iter()
                .filter(|quad| gradients.contains(&quad.background))
                .collect::<Vec<_>>();
            rows.sort_by(|a, b| a.bounds.top().0.total_cmp(&b.bounds.top().0));
            assert_eq!(
                rows.iter().map(|quad| quad.background).collect::<Vec<_>>(),
                gradients
            );
            let shells = quads
                .iter()
                .filter(|quad| {
                    quad.bounds.size.width.0 >= 440. * scale
                        && quad.bounds.size.width.0 <= 560. * scale
                        && quad.border_widths.top == ScaledPixels(0.)
                        && quad.bounds.size.width > rows[0].bounds.size.width
                        && quad.bounds.contains(&rows[0].bounds.center())
                })
                .collect::<Vec<_>>();
            assert_eq!(
                shells.len(),
                1,
                "one popup shell contains the diagnostic rows"
            );
            (shells[0].bounds, rows[0].bounds)
        })
    };

    let advance = |cx: &HeadlessAppContext, milliseconds| {
        cx.advance_clock(Duration::from_millis(milliseconds));
        cx.run_until_parked();
    };
    let assert_hidden = |cx: &mut HeadlessAppContext| {
        cx.update(|cx| assert!(editor.read(cx).hover_popover().is_none()));
    };
    let move_into_diagnostic =
        |cx: &mut HeadlessAppContext, shell: Bounds<ScaledPixels>, row: Bounds<ScaledPixels>| {
            let (step, target) = cx.update_window(window, |_, window, cx| {
                let scale = window.scale_factor();
                let target = point(
                    px(row.right().0 / scale - 23.),
                    px(row.top().0 / scale + 21.),
                );
                let step = point(target.x, symbol.center().y);
                let state = editor.read(cx);
                let hover = state.hover_popover().unwrap();
                let anchor = state
                    .visible_text_range_bounds(&hover.symbol_range)
                    .unwrap()
                    .scale(scale);
                let scaled_step = step.scale(scale);
                assert!(shell.union(&anchor).contains(&scaled_step));
                assert!(!anchor.contains(&scaled_step) && !shell.contains(&scaled_step));
                assert!(row.contains(&target.scale(scale)));
                (step, target)
            })?;
            move_to(cx, step)?;
            assert_rows(cx, false);
            move_to(cx, target)?;
            assert_rows(cx, false);
            Ok::<_, anyhow::Error>(target)
        };
    move_to(&mut cx, symbol.center())?;
    advance(&cx, 150);
    advance(&cx, 149);
    assert_hidden(&mut cx);
    advance(&cx, 1);
    assert_rows(&mut cx, true);
    let (combined_shell, combined_row) = popup(&mut cx)?;

    // Leaving while the provider is running must discard its response.
    move_to(&mut cx, point(px(750.), px(550.)))?;
    delay.set(Duration::from_millis(300));
    move_to(&mut cx, symbol.center())?;
    advance(&cx, 150);
    move_to(&mut cx, point(px(750.), px(550.)))?;
    advance(&cx, 300);
    assert_hidden(&mut cx);

    let output = std::env::var_os("STROLL_DIAGNOSTIC_SCREENSHOTS").map(std::path::PathBuf::from);
    move_to(&mut cx, symbol.center())?;
    advance(&cx, 150);
    advance(&cx, 149);
    assert_hidden(&mut cx);
    advance(&cx, 1);
    assert_rows(&mut cx, false);
    let (initial_shell, initial_row) = popup(&mut cx)?;
    assert_eq!(combined_shell.origin, initial_shell.origin);
    assert_eq!(combined_row, initial_row);
    assert!(combined_shell.size.height > initial_shell.size.height);
    move_into_diagnostic(&mut cx, initial_shell, initial_row)?;
    advance(&cx, 150);
    assert_rows(&mut cx, true);
    for (mode, label) in [(ThemeMode::Light, "light"), (ThemeMode::Dark, "dark")] {
        cx.update(|cx| stroll::workspace::apply_theme(mode, cx));
        popup(&mut cx)?;
        let image = cx.capture_screenshot(window)?;
        if let Some(output) = &output {
            std::fs::create_dir_all(output)?;
            image.save(output.join(format!("diagnostic-hover-{label}.png")))?;
        }
    }
    cx.update_window(window, |_, window, cx| {
        editor.update(cx, |editor, cx| {
            editor.focus(window, cx);
            editor.set_selected_range(SYMBOL.start..SYMBOL.start, cx);
            editor
                .lsp_mut()
                .code_action_providers
                .push(Rc::new(QuickFix));
        });
        window.draw(cx).clear(cx);
    })?;
    cx.run_until_parked();
    cx.advance_clock(Duration::from_millis(250));
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(
            editor
                .read(cx)
                .code_action_indicator_bounds(px(22.))
                .is_some()
        );
    })?;
    move_to(&mut cx, point(px(750.), px(550.)))?;
    move_to(&mut cx, symbol.center())?;
    advance(&cx, 150);
    advance(&cx, 150);
    assert_rows(&mut cx, false);
    let (shell, row) = popup(&mut cx)?;
    let quick_fix = move_into_diagnostic(&mut cx, shell, row)?;
    cx.update_window(window, |_, window, cx| {
        window.dispatch_event(
            MouseDownEvent {
                button: MouseButton::Left,
                position: quick_fix,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            MouseUpEvent {
                button: MouseButton::Left,
                position: quick_fix,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
    })?;
    cx.run_until_parked();
    let (action_shell, _) = popup(&mut cx)?;
    assert_eq!(action_shell.origin, combined_shell.origin);
    cx.update(|cx| {
        let menu = editor.read(cx).code_action_menu_state();
        assert!(menu.open && menu.items.len() == 1);
        assert!(menu.hover.as_ref().unwrap().hover.is_none());
    });
    advance(&cx, 150);
    cx.update(|cx| {
        let menu = editor.read(cx).code_action_menu_state();
        assert!(menu.open && menu.hover.as_ref().unwrap().hover.is_some());
    });
    assert_eq!(popup(&mut cx)?.0.origin, action_shell.origin);
    let preview_fix = |cx: &mut HeadlessAppContext| -> anyhow::Result<()> {
        cx.update_window(window, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                let item = editor.code_action_menu_state().items[0].clone();
                editor.preview_code_action(&item, window, cx);
            });
        })?;
        cx.run_until_parked();
        Ok(())
    };
    preview_fix(&mut cx)?;
    let (preview_shell, _) = popup(&mut cx)?;
    assert_eq!(preview_shell.origin, action_shell.origin);
    cx.update(|cx| {
        let editor = editor.read(cx);
        assert_eq!(editor.text(), SOURCE);
        assert!(editor.code_action_menu_state().preview.is_some());
    });
    if let Some(output) = std::env::var_os("STROLL_ACTION_SCREENSHOTS") {
        let output = std::path::PathBuf::from(output);
        std::fs::create_dir_all(&output)?;
        // Show the complete surface in the screenshot. The assertions above
        // exercise the same surface with enough diagnostics to require scroll.
        cx.update_window(window, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                let hover = editor.code_action_menu_state().hover.clone().unwrap();
                editor.dismiss_lsp_overlays(cx);
                editor.present_hover(SYMBOL, hover.hover.unwrap(), cx);
                editor.present_diagnostic(hover.diagnostics[0].clone(), cx);
                editor.open_code_actions(SYMBOL, window, cx);
            });
        })?;
        cx.run_until_parked();
        preview_fix(&mut cx)?;
        for (mode, label) in [(ThemeMode::Light, "light"), (ThemeMode::Dark, "dark")] {
            cx.update(|cx| stroll::workspace::apply_theme(mode, cx));
            cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))?;
            cx.capture_screenshot(window)?
                .save(output.join(format!("code-actions-{label}.png")))?;
        }
    }
    cx.update(|cx| {
        editor.update(cx, |editor, cx| {
            editor.dismiss_lsp_overlays(cx);
            editor.lsp_mut().hover_provider = None;
            editor.lsp_mut().code_action_providers.clear();
        });
    });
    cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))?;
    move_to(&mut cx, point(px(740.), symbol.center().y))?;
    cx.update(|cx| assert!(editor.read(cx).hover_popover().is_none()));
    move_to(&mut cx, symbol.center())?;
    advance(&cx, 299);
    assert_hidden(&mut cx);
    advance(&cx, 1);
    assert_rows(&mut cx, false);
    let (shell, row) = popup(&mut cx)?;
    move_into_diagnostic(&mut cx, shell, row)?;
    move_to(&mut cx, point(px(750.), px(550.)))?;
    assert_hidden(&mut cx);
    Ok(())
}

#[gpui::test]
fn editor_hover_enrichment_preserves_anchor_wraps_and_discards_late_results(
    cx: &mut gpui::TestAppContext,
) {
    use std::rc::Rc;

    use gpui::{App, AppContext as _, Context, Entity, Render, Styled, Task, Window, px};
    use gpui_component::{
        Root,
        input::{Editor, EditorState, HoverDocumentation, HoverParagraph, HoverProvider, Rope},
    };
    use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind};

    const PROSE: &str = "Read the original.";
    const TRANSLATION: &str = "Read the translation.";

    struct Documentation;
    impl HoverProvider for Documentation {
        fn hover(
            &self,
            _: &Rope,
            _: usize,
            _: &mut Window,
            _: &mut App,
        ) -> Task<anyhow::Result<Option<Hover>>> {
            Task::ready(Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: PROSE.into(),
                }),
                range: None,
            })))
        }

        fn enrich_hover(
            &self,
            _: &Hover,
            _: usize,
            _: &mut Window,
            cx: &mut App,
        ) -> Task<Option<HoverDocumentation>> {
            let timer = cx.background_executor().timer(Duration::from_millis(450));
            cx.background_spawn(async move {
                timer.await;
                Some(HoverDocumentation {
                    paragraphs: std::collections::BTreeMap::from([(
                        0,
                        HoverParagraph {
                            original: PROSE.into(),
                            translation: TRANSLATION.into(),
                        },
                    )])
                    .into(),
                    plaintext: None,
                })
            })
        }
    }

    struct Probe(Entity<EditorState>);
    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            Editor::new(&self.0).size_full()
        }
    }

    cx.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
    });
    let mut editor = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        let state = cx.new(|cx| {
            let mut state = EditorState::new(window, cx).default_value("source");
            state.lsp_mut().hover_provider = Some(Rc::new(Documentation));
            state
        });
        editor = Some(state.clone());
        let probe = cx.new(|_| Probe(state));
        Root::new(probe, window, cx)
    });
    let editor = editor.unwrap();
    let frame = |cx: &mut gpui::VisualTestContext, duration| {
        cx.run_until_parked();
        cx.executor().advance_clock(duration);
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    };
    let ready = |visual: &mut gpui::VisualTestContext| {
        visual.update(|_, cx| {
            editor
                .read(cx)
                .hover_popover()
                .map(|hover| (hover.symbol_range.clone(), hover.documentation.is_some()))
        })
    };

    for width in [760., 205.] {
        visual.simulate_resize(size(px(width), px(530.)));
        frame(visual, Duration::ZERO);
        let anchor =
            visual.update(|_, cx| editor.read(cx).range_to_bounds(&(0..6)).unwrap().center());
        visual.simulate_mouse_move(anchor, None, Default::default());
        frame(visual, Duration::from_millis(500));
        assert_eq!(ready(visual), Some((0..6, false)));
        frame(visual, Duration::from_millis(500));
        assert_eq!(ready(visual), Some((0..6, true)));
        let original = visual.debug_bounds("hover-original-paragraph").unwrap();
        let translation = visual.debug_bounds("hover-translated-paragraph").unwrap();
        if width > 500. {
            assert_eq!(
                original.top(),
                translation.top(),
                "{width}: original {original:?}, translation {translation:?}"
            );
            assert!(original.right() <= translation.left());
        } else {
            assert!(original.bottom() <= translation.top());
        }
        visual.update(|_, cx| editor.update(cx, |editor, cx| editor.clear_hover_state(cx)));
        visual.simulate_mouse_move(anchor + point(px(1.), px(0.)), None, Default::default());
        frame(visual, Duration::from_millis(500));
        assert_eq!(ready(visual), Some((0..6, false)));
        visual.update(|_, cx| editor.update(cx, |editor, cx| editor.clear_hover_state(cx)));
        frame(visual, Duration::from_millis(500));
        assert_eq!(ready(visual), None);
        visual.simulate_mouse_move(point(px(width - 10.), px(500.)), None, Default::default());
    }
}

#[gpui::test]
fn shared_input_and_dropdown_outlines_match_at_each_density(cx: &mut gpui::TestAppContext) {
    use gpui::{
        AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
        Render, Styled as _, Window, div, px,
    };
    use gpui_component::{
        Root, Sizable as _, Size,
        button::Button,
        input::{Input, InputState},
        select::{SearchableVec, Select, SelectState},
        v_flex,
    };

    struct Controls {
        field: Entity<InputState>,
        picker: Entity<SelectState<SearchableVec<&'static str>>>,
        size: Size,
    }

    impl Render for Controls {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            v_flex()
                .w(px(240.))
                .gap_2()
                .child(
                    div()
                        .debug_selector(|| "outline-input".into())
                        .child(Input::new(&self.field).with_size(self.size).w_full()),
                )
                .child(
                    div()
                        .debug_selector(|| "outline-select".into())
                        .child(Select::new(&self.picker).with_size(self.size).w_full()),
                )
                .child(
                    Button::new("outline-button")
                        .debug_selector(|| "outline-button".into())
                        .with_size(self.size)
                        .label("Change mode")
                        .outline()
                        .dropdown_caret(true)
                        .w_full(),
                )
        }
    }

    cx.update(gpui_component::init);
    let mut controls = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        let panel = cx.new(|cx| Controls {
            field: cx.new(|cx| InputState::new(window, cx).placeholder("Search files")),
            picker: cx.new(|cx| {
                SelectState::new(SearchableVec::from(vec!["Main"]), None, window, cx)
                    .searchable(true)
            }),
            size: Size::Medium,
        });
        controls = Some(panel.clone());
        Root::new(panel, window, cx)
    });
    let controls = controls.unwrap();
    for scale in [1., 1.25] {
        visual.simulate_scale_factor_change(scale);
        for (size, height) in [
            (Size::XSmall, 24.),
            (Size::Small, 28.),
            (Size::Medium, 32.),
            (Size::Large, 40.),
            (Size::Size(px(34.)), 34.),
        ] {
            visual.update(|window, cx| {
                controls.update(cx, |panel, cx| {
                    panel.size = size;
                    cx.notify();
                });
                window.draw(cx).clear(cx);
            });
            for selector in ["outline-input", "outline-select", "outline-button"] {
                let bounds = visual.debug_bounds(selector).unwrap();
                assert!(
                    (bounds.size.height - px(height)).abs() <= px(0.51 / scale),
                    "{selector} at {scale}x: expected {height}px, got {}",
                    bounds.size.height,
                );
                visual.update(|window, _| {
                    let bounds = bounds.scale(scale);
                    assert!(
                        window.painted_quads().iter().any(|quad| {
                            quad.border_widths.top.0 > 0.
                                && (quad.bounds.size.height.0 - bounds.size.height.0).abs() <= 1.
                                && (quad.bounds.origin.y.0 - bounds.origin.y.0).abs() <= 1.
                        }),
                        "{selector} painted border at {scale}x"
                    );
                });
            }
        }
    }
}

#[gpui::test]
fn shared_select_scrolls_below_when_one_row_fits_and_falls_back_above(
    cx: &mut gpui::TestAppContext,
) {
    use gpui::{
        App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
        ParentElement as _, Render, ScrollDelta, ScrollEvent, SharedString, Styled as _,
        VisualTestContext, Window, div, px,
    };
    use gpui_component::{
        Root,
        select::{SearchableVec, Select, SelectItem, SelectState},
    };

    #[derive(Clone)]
    struct Choice(usize);

    impl SelectItem for Choice {
        type Value = usize;

        fn title(&self) -> SharedString {
            format!("Revision {:02}", self.0).into()
        }

        fn value(&self) -> &usize {
            &self.0
        }

        fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
            let id = self.0;
            div()
                .debug_selector(move || format!("revision-{id}"))
                .child(self.title())
        }
    }

    struct Picker(Entity<SelectState<SearchableVec<Choice>>>);

    impl Render for Picker {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().pt(px(150.)).pl(px(12.)).child(
                div()
                    .w(px(180.))
                    .debug_selector(|| "select-trigger".into())
                    .child(Select::new(&self.0)),
            )
        }
    }

    cx.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
    });
    let (_, visual) = cx.add_window_view(|window, cx| {
        let picker = cx.new(|cx| {
            let choices = SearchableVec::from((0..40).map(Choice).collect::<Vec<_>>());
            SelectState::new(choices, None, window, cx).searchable(true)
        });
        let view = cx.new(|_| Picker(picker));
        Root::new(view, window, cx)
    });
    let draw = |visual: &mut VisualTestContext| {
        for _ in 0..2 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
    };
    let open = |visual: &mut VisualTestContext, height| {
        visual.simulate_resize(size(px(280.), px(height)));
        draw(visual);
        let trigger = visual.debug_bounds("select-trigger").unwrap();
        visual.simulate_click(trigger.center(), Default::default());
        draw(visual);
        trigger
    };

    let trigger = open(visual, 284.);
    let popup = visual.debug_bounds("select-popup-surface").unwrap();
    let search = visual.debug_bounds("popup-list-search").unwrap();
    assert!(popup.top() > trigger.bottom());
    assert!(popup.bottom() <= px(276.));
    let first = visual.debug_bounds("revision-0").unwrap();
    assert!(first.top() >= search.bottom());
    assert!(visual.debug_bounds("revision-10").is_none());

    visual.simulate_event(ScrollEvent {
        position: first.center(),
        delta: ScrollDelta::Pixels(point(px(0.), px(-280.))),
        ..Default::default()
    });
    draw(visual);
    assert!(visual.debug_bounds("revision-0").is_none());
    assert!(visual.debug_bounds("revision-10").is_some());
    assert_eq!(visual.debug_bounds("popup-list-search"), Some(search));

    visual.simulate_keystrokes("escape");
    draw(visual);
    let trigger = open(visual, 241.);
    let popup = visual.debug_bounds("select-popup-surface").unwrap();
    assert!(popup.bottom() < trigger.top());
    visual.simulate_input("Revision 39");
    visual.executor().advance_clock(Duration::from_millis(300));
    draw(visual);
    let filtered = visual.debug_bounds("select-popup-surface").unwrap();
    assert_eq!(filtered.bottom(), popup.bottom());
    assert!(filtered.size.height < popup.size.height);
    assert!(visual.debug_bounds("revision-39").is_some());
}
