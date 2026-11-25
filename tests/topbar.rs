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
    workspace::{SwitchWorkspaceView, ToggleTerminal, Workspace, WorkspaceView, apply_theme},
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

#[test]
fn project_gradient_joins_the_file_tree_at_the_editor_corner() -> anyhow::Result<()> {
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
    fs::write(repo.join("main.rs"), "fn main() {}\n")?;
    fs::write(repo.join("README.md"), "# Example\n")?;
    let git = |args: &[&str]| -> anyhow::Result<()> {
        assert!(
            Command::new("git")
                .current_dir(&repo)
                .args(args)
                .status()?
                .success()
        );
        Ok(())
    };
    git(&["add", "."])?;
    git(&[
        "-c",
        "user.name=Example",
        "-c",
        "user.email=example@example.com",
        "commit",
        "-qm",
        "Add example files",
    ])?;
    let config_path = temp.path().join("config.toml");
    fs::write(
        &config_path,
        "[appearance]\ngradient_title_bar = true\n\n[terminal]\nshell = '/bin/sh'\n",
    )?;
    let config = ConfigFile::load(Some(config_path));
    let store = Arc::new(Store::new(temp.path().join("state"))?);
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
                            file: Some(PathBuf::from("main.rs")),
                            root: repo,
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

    for (mode, theme, base) in [
        (ThemeMode::Light, "light", [255, 255, 255, 255]),
        (ThemeMode::Dark, "dark", [20, 21, 24, 255]),
    ] {
        let (edge, top, nav, bar_x) = cx.update_window(handle.into(), |_, window, cx| {
            apply_theme(mode, cx);
            window.refresh();
            window.draw(cx).clear(cx);
            let quads = window.painted_quads();
            let bar = quads
                .iter()
                .find(|quad| {
                    quad.bounds.size.height == ScaledPixels(72.)
                        && quad.bounds.size.width > ScaledPixels(500.)
                        && quad.background.as_solid().is_none()
                })
                .expect("title bar gradient");
            let bar_end = quads
                .iter()
                .find(|quad| {
                    quad.bounds.left() == bar.bounds.right()
                        && quad.bounds.top() == bar.bounds.top()
                        && quad.bounds.size.height == bar.bounds.size.height
                        && quad.background.as_solid().is_none()
                })
                .expect("title bar gradient at the right edge");
            let navigation = quads
                .iter()
                .find(|quad| {
                    quad.bounds.center().y == bar.bounds.center().y
                        && quad.bounds.size.height == ScaledPixels(60.)
                        && quad.corner_radii.top_left == ScaledPixels(16.)
                })
                .expect("view switcher track");
            let white = gpui::Hsla::white();
            let ink = gpui::Hsla::from(rgb(0x303540));
            let (track, selected_color) = if mode.is_dark() {
                (rgb(0x15161a).into(), rgb(0x383838).into())
            } else {
                (ink.opacity(0.06), white)
            };
            assert_eq!(navigation.background.as_solid(), Some(track));
            let selected = quads
                .iter()
                .find(|quad| {
                    quad.bounds.center().y == navigation.bounds.center().y
                        && quad.bounds.left() > navigation.bounds.left()
                        && quad.bounds.right() < navigation.bounds.right()
                        && quad.bounds.size.height == ScaledPixels(48.)
                        && quad.background.as_solid() == Some(selected_color)
                })
                .expect("selected view switcher button");
            assert!(!quads.iter().any(|quad| {
                (quad.bounds == navigation.bounds || quad.bounds == selected.bounds)
                    && quad.border_widths != Default::default()
            }));
            let fade = quads
                .iter()
                .find(|quad| {
                    quad.bounds.top() == bar.bounds.bottom()
                        && quad.bounds.size.height == ScaledPixels(240.)
                        && quad.content_mask.bounds.size.width == ScaledPixels(480.)
                        && quad.background.as_solid().is_none()
                })
                .expect("120 px gradient clipped to the file tree");
            assert_eq!(bar.background, fade.background);
            let edge = fade.content_mask.bounds.right();
            assert!(!quads.iter().any(|quad| {
                quad.bounds.bottom() == bar.bounds.bottom()
                    && quad.bounds.size.height == ScaledPixels(2.)
                    && quad.bounds.size.width > ScaledPixels(500.)
                    && quad.background.as_solid().is_none()
            }));
            let corner = quads
                .iter()
                .find(|quad| {
                    quad.corner_radii.top_left == ScaledPixels(24.)
                        && quad.bounds.left() == edge
                        && quad.bounds.top() == bar.bounds.bottom()
                })
                .expect("rounded editor surface");
            assert_eq!(corner.corner_radii.top_right, ScaledPixels(0.));
            assert_eq!(corner.bounds.size.height, ScaledPixels(72.));
            let base_color = gpui::rgba(u32::from_be_bytes(base));
            assert_eq!(corner.background, base_color.into());
            assert!(quads.iter().any(|quad| {
                quad.bounds.right() == edge
                    && quad.bounds.top() == corner.bounds.top() + ScaledPixels(24.)
                    && quad.bounds.size.width == ScaledPixels(2.)
                    && quad.background.as_solid().is_none()
            }));
            let (width, inset, background) = if mode.is_dark() {
                (2., 0., Background::from(rgb(0x3d4048)))
            } else {
                let black = gpui::Hsla::black();
                (
                    8.,
                    2.,
                    gpui::linear_gradient(
                        90.,
                        gpui::linear_color_stop(black.opacity(0.), 0.),
                        gpui::linear_color_stop(black.opacity(0.09), 1.),
                    ),
                )
            };
            assert!(quads.iter().any(|quad| {
                quad.bounds.right() == edge - ScaledPixels(inset)
                    && quad.bounds.top() == fade.bounds.bottom()
                    && quad.bounds.size.width == ScaledPixels(width)
                    && quad.background == background
            }));
            (
                edge.0 as u32,
                corner.bounds.top().0 as u32,
                navigation.bounds,
                bar_end.bounds.right().0 as u32 - 24,
            )
        })?;
        if let Some(output) = &screenshots {
            fs::create_dir_all(output)?;
            let screenshot = cx.capture_screenshot(handle.into())?;
            assert_ne!(screenshot.get_pixel(edge + 1, top + 1).0, base);
            assert_eq!(screenshot.get_pixel(edge + 9, top + 20).0, base);
            assert_eq!(screenshot.get_pixel(edge - 12, top + 246).0, base);
            if mode == ThemeMode::Light {
                let tint = screenshot.get_pixel(bar_x, top - 1).0;
                let content = screenshot.get_pixel(bar_x, top + 1).0;
                assert_eq!(screenshot.get_pixel(bar_x, top - 8).0, tint);
                assert_eq!(content, base);
                assert!(tint[0] + 14 < content[0]);
                assert_eq!(
                    screenshot.get_pixel(edge - 24, top - 1),
                    screenshot.get_pixel(edge - 24, top - 3),
                );
            }
            screenshot.save(output.join(format!("project-gradient-{theme}.png")))?;
        }
        if mode == ThemeMode::Light {
            cx.update_window(handle.into(), |_, window, cx| {
                let move_to = |window: &mut gpui::Window, cx: &mut gpui::App, position| {
                    window.dispatch_event(
                        PlatformInput::MouseMove(MouseMoveEvent {
                            position,
                            ..Default::default()
                        }),
                        cx,
                    );
                    window.draw(cx).clear(cx);
                };
                move_to(
                    window,
                    cx,
                    point(px(nav.left().0 / 2. + 8.), px(nav.center().y.0 / 2.)),
                );
                assert!(window.painted_quads().iter().any(|quad| {
                    quad.bounds.left() == nav.left() + ScaledPixels(6.)
                        && quad.bounds.center().y == nav.center().y
                        && quad.background.as_solid() == Some(gpui::Hsla::white().opacity(0.30))
                }));
                move_to(window, cx, point(px(0.), px(0.)));
            })?;
        }
    }

    let click = |window: &mut gpui::Window, cx: &mut gpui::App, position| {
        window.dispatch_event(
            PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }),
            cx,
        );
        window.dispatch_event(
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position,
                click_count: 1,
                ..Default::default()
            }),
            cx,
        );
    };
    cx.update_window(handle.into(), |_, window, cx| {
        window.dispatch_action(Box::new(ToggleTerminal), cx);
    })?;
    cx.run_until_parked();
    let terminal_background = Background::from(rgb(0x17191f));
    let maximize = cx.update_window(handle.into(), |_, window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
        let panel = window
            .painted_quads()
            .into_iter()
            .find(|quad| {
                quad.background == terminal_background
                    && quad.bounds.size.width > ScaledPixels(800.)
                    && quad.bounds.size.height > ScaledPixels(200.)
            })
            .expect("docked terminal panel");
        point(
            px(panel.bounds.right().0 / 2. - 39.),
            px(panel.bounds.top().0 / 2. + 14.),
        )
    })?;
    cx.update_window(handle.into(), |_, window, cx| click(window, cx, maximize))?;
    cx.run_until_parked();
    let terminal = cx.update_window(handle.into(), |_, window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
        let quads = window.painted_quads();
        let panel = quads
            .iter()
            .find(|quad| {
                quad.background == terminal_background
                    && quad.corner_radii.top_left == ScaledPixels(24.)
            })
            .expect("rounded maximized terminal");
        assert!(quads.iter().any(|quad| {
            quad.background.as_solid().is_none()
                && quad.content_mask.bounds.origin == panel.bounds.origin
                && quad.content_mask.bounds.size == size(ScaledPixels(24.), ScaledPixels(24.))
        }));
        panel.bounds
    })?;
    if let Some(output) = &screenshots {
        let screenshot = cx.capture_screenshot(handle.into())?;
        let (x, y) = (terminal.left().0 as u32, terminal.top().0 as u32);
        assert_ne!(screenshot.get_pixel(x + 1, y + 1).0, [23, 25, 31, 255]);
        assert_eq!(screenshot.get_pixel(x + 26, y + 4).0, [23, 25, 31, 255]);
        screenshot.save(output.join("project-gradient-maximized-terminal.png"))?;
    }

    let mut history_toggle = None;
    for (view, name, width, background) in [
        (WorkspaceView::Docs, "docs", 480., 0x141518),
        (WorkspaceView::Git, "git", 600., 0x1c1d22),
    ] {
        cx.update_window(handle.into(), |_, window, cx| {
            window.dispatch_action(Box::new(SwitchWorkspaceView(view)), cx);
        })?;
        cx.run_until_parked();
        let corner = cx.update_window(handle.into(), |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
            let quads = window.painted_quads();
            let bar = quads
                .iter()
                .find(|quad| {
                    quad.bounds.size.height == ScaledPixels(72.)
                        && quad.corner_radii.top_left > ScaledPixels(0.)
                        && quad.background.as_solid().is_none()
                })
                .expect("title bar gradient");
            let fade = quads
                .iter()
                .find(|quad| {
                    quad.bounds.size.height == ScaledPixels(240.)
                        && quad.content_mask.bounds.size.width == ScaledPixels(width)
                        && quad.background == bar.background
                })
                .unwrap_or_else(|| panic!("project gradient in the {name} sidebar"));
            quads
                .iter()
                .find(|quad| {
                    quad.bounds.left() == fade.content_mask.bounds.right()
                        && quad.bounds.top() == fade.bounds.top()
                        && quad.corner_radii.top_left == ScaledPixels(24.)
                        && quad.background == Background::from(rgb(background))
                })
                .unwrap_or_else(|| panic!("rounded {name} content"))
                .bounds
        })?;
        if view == WorkspaceView::Git {
            history_toggle = Some(point(
                px(corner.left().0 / 2. + 22.),
                px(corner.center().y.0 / 2.),
            ));
        }
        if let Some(output) = &screenshots {
            cx.run_until_parked();
            cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear(cx))?;
            cx.capture_screenshot(handle.into())?
                .save(output.join(format!("project-gradient-{name}.png")))?;
        }
    }

    let position = history_toggle.unwrap();
    cx.update_window(handle.into(), |_, window, cx| {
        click(window, cx, position);
    })?;
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
        let quads = window.painted_quads();
        assert!(quads.iter().any(|quad| {
            quad.bounds.size.height == ScaledPixels(72.)
                && quad.bounds.size.width > ScaledPixels(500.)
                && quad.background.as_solid().is_none()
        }));
        assert!(!quads.iter().any(|quad| {
            quad.bounds.size.height == ScaledPixels(240.)
                && quad.content_mask.bounds.size.width == ScaledPixels(600.)
                && quad.background.as_solid().is_none()
        }));
    })?;
    Ok(())
}
