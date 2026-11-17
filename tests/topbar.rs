#![cfg(target_os = "linux")]

use std::{fs, path::PathBuf, process::Command, sync::Arc};

use gpui::{
    AppContext as _, Background, Bounds, HeadlessAppContext, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PlatformInput, ScaledPixels, WindowBounds, WindowDecorations,
    point, px, rgb, size,
};
use gpui_component::{ActiveTheme as _, Root, ThemeMode, TitleBar};
use stroll::{
    config::ConfigFile,
    repository::{Location, Store},
    workspace::{Workspace, WorkspaceView, apply_theme},
};

#[test]
fn compact_topbar_highlights_and_clicks_from_the_outer_hit_area() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("example");
    fs::create_dir(&repo)?;
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()?
            .success()
    );
    let store = Arc::new(Store::new(temp.path().join("state"))?);
    let config = ConfigFile::load(Some(temp.path().join("config.toml")));
    let screenshots = std::env::var_os("STROLL_TOPBAR_SCREENSHOTS").map(PathBuf::from);
    let capture = screenshots.is_some();
    let mut cx = HeadlessAppContext::with_platform(
        gpui_platform::current_platform(true).text_system(),
        Arc::new(gpui_kit_assets::AllAssets),
        move || {
            if capture {
                Ok(Some(Box::new(gpui_wgpu::WgpuHeadlessRenderer::new()?)))
            } else {
                Ok(None)
            }
        },
    );
    let handle = cx.update(|cx| {
        gpui_component::init(cx);
        cx.open_window(
            gpui::WindowOptions {
                window_decorations: Some(WindowDecorations::Client),
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(940.), px(480.)),
                ))),
                focus: false,
                show: false,
                ..TitleBar::window_options()
            },
            |window, cx| {
                let workspace = cx.new(|cx| {
                    Workspace::with_config(store, config, WorkspaceView::Editor, window, cx)
                });
                workspace.update(cx, |workspace, cx| {
                    workspace.open_repository(
                        Location {
                            root: repo,
                            file: None,
                        },
                        window,
                        cx,
                    );
                });
                cx.new(|cx| Root::new(workspace, window, cx))
            },
        )
    })?;
    cx.run_until_parked();

    let mut close_position = None;
    for (mode, theme, bar_color) in [
        (ThemeMode::Light, "light", 0xf5f6f9),
        (ThemeMode::Dark, "dark", 0x1c1d22),
    ] {
        let bar = cx.update_window(handle.into(), |_, window, cx| {
            apply_theme(mode, cx);
            window.refresh();
            window.draw(cx).clear(cx);
            window
                .painted_quads()
                .into_iter()
                .find(|quad| {
                    quad.bounds.size.width.0 > 1600.
                        && quad.background == Background::from(rgb(bar_color))
                })
                .expect("workspace topbar")
                .bounds
        })?;
        assert_eq!(bar.size.height, ScaledPixels(72.));

        for (index, name) in ["minimize", "maximize", "close"].into_iter().enumerate() {
            let center = point(
                px(bar.right().0 / 2. - 85. + index as f32 * 34.),
                px(bar.center().y.0 / 2.),
            );
            // The point is inside the original hit area but above the colored circle.
            let position = point(center.x, px(bar.top().0 / 2. + 2.));
            cx.update_window(handle.into(), |_, window, cx| {
                window.dispatch_event(
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position,
                        ..Default::default()
                    }),
                    cx,
                );
                window.draw(cx).clear(cx);
                let quad = window
                    .painted_quads()
                    .into_iter()
                    .find(|quad| {
                        quad.bounds.center() == point(center.x.scale(2.), center.y.scale(2.))
                    })
                    .unwrap_or_else(|| panic!("{theme} {name} hover is missing"));
                assert_eq!(quad.bounds.size, size(ScaledPixels(44.), ScaledPixels(44.)));
                assert!(quad.corner_radii.top_left >= ScaledPixels(22.));
                let color = quad.background.as_solid().expect("solid highlight");
                if name == "close" {
                    assert_eq!(color, cx.theme().danger);
                } else {
                    assert!(color.l < cx.theme().secondary_hover.l);
                    assert!(color.l < gpui::Hsla::from(rgb(bar_color)).l);
                }
            })?;
            if let Some(output) = &screenshots {
                fs::create_dir_all(output)?;
                let screenshot = cx.capture_screenshot(handle.into())?;
                image::imageops::crop_imm(
                    &screenshot,
                    bar.left().0 as u32,
                    bar.top().0 as u32,
                    bar.size.width.0 as u32,
                    88,
                )
                .to_image()
                .save(output.join(format!("topbar-{theme}-{name}.png")))?;
            }
            if name == "close" {
                close_position = Some(position);
            }
        }
    }

    let position = close_position.unwrap();
    cx.update_window(handle.into(), |_, window, cx| {
        window.dispatch_event(
            PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }),
            cx,
        );
        window.draw(cx).clear(cx);
        assert!(window.painted_quads().iter().any(|quad| {
            quad.bounds.size == size(ScaledPixels(44.), ScaledPixels(44.))
                && quad.background == Background::from(cx.theme().danger_active)
        }));
        window.dispatch_event(
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }),
            cx,
        );
    })?;
    cx.run_until_parked();
    assert!(cx.update(|cx| cx.windows().is_empty()));
    Ok(())
}
