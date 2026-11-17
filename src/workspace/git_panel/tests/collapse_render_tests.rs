use gpui::InputEvent as _;
use gpui_component::{Root, ThemeMode};
use gpui_wgpu::WgpuHeadlessRenderer;

use super::*;

const VIEWPORT: Size<Pixels> = Size {
    width: px(1100.),
    height: px(720.),
};

fn draw(cx: &mut HeadlessAppContext, window: AnyWindowHandle) -> Result<()> {
    for _ in 0..3 {
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.simulate_next_frame(cx);
            window.draw(cx).clear(cx);
        })?;
    }
    Ok(())
}

fn wait_for_tree_rows(
    cx: &mut HeadlessAppContext,
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    ready: impl Fn(&Workspace) -> bool,
) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        draw(cx, window)?;
        if cx.update_window(window, |_, _, cx| ready(workspace.read(cx)))? {
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "Workspace tree did not load"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn click(
    cx: &mut HeadlessAppContext,
    handle: AnyWindowHandle,
    position: Point<Pixels>,
) -> Result<()> {
    cx.update_window(handle, |_, window, cx| {
        window.simulate_mouse_move(position, cx);
        window.dispatch_event(
            MouseDownEvent {
                position,
                button: MouseButton::Left,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            MouseUpEvent {
                position,
                button: MouseButton::Left,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
    })?;
    draw(cx, handle)
}

fn drag(
    cx: &mut HeadlessAppContext,
    handle: AnyWindowHandle,
    from: Point<Pixels>,
    to: Point<Pixels>,
) -> Result<()> {
    cx.update_window(handle, |_, window, cx| {
        window.simulate_mouse_move(from, cx);
        window.dispatch_event(
            MouseDownEvent {
                position: from,
                button: MouseButton::Left,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            MouseMoveEvent {
                position: to,
                pressed_button: Some(MouseButton::Left),
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            MouseUpEvent {
                position: to,
                button: MouseButton::Left,
                click_count: 1,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
    })?;
    draw(cx, handle)
}

fn record(
    cx: &mut HeadlessAppContext,
    window: AnyWindowHandle,
    name: &str,
    phase: &str,
) -> Result<image::RgbaImage> {
    let image = cx.capture_screenshot(window)?;
    if let Some(output) = std::env::var_os("STROLL_COLLAPSE_SCREENSHOTS") {
        let output = PathBuf::from(output);
        fs::create_dir_all(&output)?;
        image.save(output.join(format!("{name}-{phase}.png")))?;
    }
    Ok(image)
}

fn row_point(bounds: Bounds<Pixels>) -> Point<Pixels> {
    point(bounds.left() + px(85.), bounds.top() + px(14.))
}

#[::core::prelude::v1::test]
fn workspace_disclosures_animate_offscreen() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let repo = fixture.path().join("repo");
    fs::create_dir_all(repo.join("guide"))?;
    git(&repo, &["init", "-qb", "main"]);
    git(&repo, &["config", "user.name", "Reviewer"]);
    git(&repo, &["config", "user.email", "reviewer@example.invalid"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    let files = [
        ("README.md", "Stroll"),
        ("guide/first-steps.md", "First steps"),
        ("guide/settings.md", "Settings"),
        ("guide/troubleshooting.md", "Troubleshooting"),
    ];
    for (path, title) in files {
        fs::write(repo.join(path), format!("# {title}\n\nPrevious content.\n"))?;
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "Add documentation"]);
    for (path, title) in files {
        fs::write(repo.join(path), format!("# {title}\n\nUpdated content.\n"))?;
    }

    let platform = gpui_platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(gpui_kit_assets::AllAssets),
        || Ok(Some(Box::new(WgpuHeadlessRenderer::new()?))),
    );
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::workspace::apply_theme(ThemeMode::Light, cx);
        cx.set_reduce_motion(false);
    });
    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let config = crate::config::ConfigFile::load(Some(fixture.path().join("config.toml")));
    let mut workspace = None;
    let handle = cx.open_window(VIEWPORT, |window, cx| {
        let view =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
        workspace = Some(view.clone());
        cx.new(|cx| Root::new(view, window, cx))
    })?;
    let workspace = workspace.unwrap();
    let handle = handle.into();
    cx.update_window(handle, |_, window, cx| {
        window.activate_window();
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(
                Location {
                    root: repo,
                    file: None,
                },
                window,
                cx,
            )
        });
    })?;
    draw(&mut cx, handle)?;

    for (view, name) in [
        (WorkspaceView::Editor, "workspace-tree"),
        (WorkspaceView::Docs, "document-tree"),
    ] {
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |workspace, cx| workspace.switch_view(view, window, cx));
        })?;
        wait_for_tree_rows(&mut cx, handle, &workspace, |workspace| {
            !workspace.rows.is_empty()
        })?;
        let folder = cx.update_window(handle, |_, _, cx| {
            let workspace = workspace.read(cx);
            assert_eq!(workspace.rows[0].entry.path, Path::new("guide"));
            assert!(!workspace.rows[0].expanded);
            row_point(workspace.tree_scroll.0.borrow().base_handle.bounds())
        })?;
        let closed = record(&mut cx, handle, name, "closed")?;
        click(&mut cx, handle, folder)?;
        wait_for_tree_rows(&mut cx, handle, &workspace, |workspace| {
            workspace.rows.len() > 1 && workspace.rows[1].depth == 1
        })?;
        cx.advance_clock(Duration::from_millis(90));
        draw(&mut cx, handle)?;
        let opening = record(&mut cx, handle, name, "opening")?;
        cx.advance_clock(Duration::from_millis(200));
        draw(&mut cx, handle)?;
        let open = record(&mut cx, handle, name, "open")?;
        assert!(
            closed != opening,
            "the opening frame matches the closed frame"
        );
        assert!(opening != open, "the opening frame matches the open frame");
        // Check that a visible row retained for the closing animation cannot
        // open a file or document.
        let current = cx.update_window(handle, |_, _, cx| {
            workspace.read(cx).session.document.clone()
        })?;
        click(&mut cx, handle, folder)?;
        click(&mut cx, handle, folder + point(px(0.), px(28.)))?;
        cx.update_window(handle, |_, _, cx| {
            let workspace = workspace.read(cx);
            assert_eq!(workspace.session.document, current);
            assert!(workspace.active_file().is_none());
        })?;
        cx.advance_clock(Duration::from_millis(200));
        draw(&mut cx, handle)?;
    }

    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.switch_view(WorkspaceView::Editor, window, cx)
        });
    })?;
    draw(&mut cx, handle)?;
    let locator_closed = record(&mut cx, handle, "locator", "closed")?;
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |workspace, cx| workspace.open_locator(window, cx));
    })?;
    cx.advance_clock(Duration::from_millis(60));
    draw(&mut cx, handle)?;
    let locator_opening = record(&mut cx, handle, "locator", "opening")?;
    cx.advance_clock(Duration::from_millis(90));
    draw(&mut cx, handle)?;
    let locator_open = record(&mut cx, handle, "locator", "open")?;
    assert!(locator_opening != locator_closed && locator_opening != locator_open);

    let popup_left = (VIEWPORT.width - px(960.)) / 2.;
    let popup_top = px(92.) + VIEWPORT.height * 0.07;
    click(
        &mut cx,
        handle,
        point(popup_left + px(85.), popup_top + px(18.)),
    )?;
    let scope = point(popup_left + px(50.), popup_top + px(122.));
    let closed = record(&mut cx, handle, "search-scope", "closed")?;
    click(&mut cx, handle, scope)?;
    cx.advance_clock(Duration::from_millis(70));
    draw(&mut cx, handle)?;
    let opening = record(&mut cx, handle, "search-scope", "opening")?;
    cx.advance_clock(Duration::from_millis(600));
    draw(&mut cx, handle)?;
    let open = record(&mut cx, handle, "search-scope", "open")?;
    assert!(
        closed != opening,
        "the opening frame matches the closed frame"
    );
    assert!(opening != open, "the opening frame matches the open frame");

    let locator_origin = |cx: &mut HeadlessAppContext| {
        cx.update_window(handle, |_, window, _| {
            let scale = window.scale_factor();
            let quad = window
                .painted_quads()
                .into_iter()
                .find(|quad| {
                    quad.bounds.size.width.0 == 960. * scale
                        && quad.bounds.size.height.0 == 504. * scale
                })
                .expect("locator surface");
            point(
                px(quad.bounds.origin.x.0 / scale),
                px(quad.bounds.origin.y.0 / scale),
            )
        })
    };
    let origin = locator_origin(&mut cx)?;
    let drag_start = point(popup_left + px(660.), popup_top + px(18.));
    let offset = point(px(40.), px(-36.));
    drag(&mut cx, handle, drag_start, drag_start + offset)?;
    assert_eq!(locator_origin(&mut cx)?, origin + offset);
    let moved = record(&mut cx, handle, "locator", "dragged")?;
    cx.update_window(handle, |_, window, cx| {
        window.dispatch_action(Box::new(crate::workspace::OpenLocator), cx);
    })?;
    draw(&mut cx, handle)?;
    assert_eq!(cx.capture_screenshot(handle)?, moved);
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.close_locator(true, window, cx);
            workspace.open_locator(window, cx);
        });
    })?;
    cx.advance_clock(Duration::from_millis(150));
    draw(&mut cx, handle)?;
    assert_eq!(locator_origin(&mut cx)?, origin);

    let files_tab = point(popup_left + px(30.), popup_top + px(18.));
    click(&mut cx, handle, files_tab)?;
    cx.advance_clock(Duration::from_millis(200));
    draw(&mut cx, handle)?;
    let locator_open = record(&mut cx, handle, "locator", "results")?;
    let locator_focus = cx.update_window(handle, |_, window, cx| window.focused(cx))?;
    click(
        &mut cx,
        handle,
        point(popup_left + px(940.), popup_top + px(18.)),
    )?;
    cx.advance_clock(Duration::from_millis(75));
    draw(&mut cx, handle)?;
    let locator_closing = record(&mut cx, handle, "locator", "closing")?;
    assert!(locator_closing != locator_open && locator_closing != locator_closed);
    click(
        &mut cx,
        handle,
        point(popup_left + px(85.), popup_top + px(18.)),
    )?;
    cx.update_window(handle, |_, window, cx| {
        assert!(!workspace.read(cx).locator.open);
        assert_ne!(window.focused(cx), locator_focus);
    })?;
    cx.advance_clock(Duration::from_millis(35));
    draw(&mut cx, handle)?;
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            assert!(workspace.locator_overlay(window, cx).is_none());
        });
    })?;

    let panel = cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.switch_view(WorkspaceView::Git, window, cx);
            workspace.git.clone().unwrap()
        })
    })?;
    panel.update(&mut cx, |panel, _| panel.show_history = false);
    cx.update_window(handle, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.load_review(Selection::Worktree(Mode::All), false, window, cx);
        });
    })?;
    draw(&mut cx, handle)?;
    let folder = cx.update_window(handle, |_, _, cx| {
        row_point(panel.read(cx).tree_scroll.0.borrow().base_handle.bounds())
    })?;
    let open = record(&mut cx, handle, "git-tree", "open")?;
    click(&mut cx, handle, folder)?;
    cx.advance_clock(Duration::from_millis(90));
    draw(&mut cx, handle)?;
    let closing = record(&mut cx, handle, "git-tree", "closing")?;
    cx.advance_clock(Duration::from_millis(200));
    draw(&mut cx, handle)?;
    let closed = record(&mut cx, handle, "git-tree", "closed")?;
    assert!(
        open != closing,
        "the Git tree closing frame matches the open frame"
    );
    assert!(
        closing != closed,
        "the Git tree closing frame matches the closed frame"
    );
    cx.update_window(handle, |_, _, cx| {
        assert!(
            panel
                .read(cx)
                .review
                .as_ref()
                .unwrap()
                .directories
                .contains("guide")
        );
    })?;

    cx.update_window(handle, |_, _, cx| {
        panel.read(cx).diff.clone().update(cx, |diff, cx| {
            diff.scroll_to_file("README.md", cx);
        });
    })?;
    draw(&mut cx, handle)?;
    let (tree_bottom, hunk_toggle) = cx.update_window(handle, |_, _, cx| {
        let bounds = panel.read(cx).tree_scroll.0.borrow().base_handle.bounds();
        (
            bounds.bottom(),
            point(bounds.center().x, bounds.bottom() + px(17.)),
        )
    })?;
    let closed = record(&mut cx, handle, "git-hunks", "closed")?;
    click(&mut cx, handle, hunk_toggle)?;
    cx.advance_clock(Duration::from_millis(70));
    draw(&mut cx, handle)?;
    let opening = record(&mut cx, handle, "git-hunks", "opening")?;
    cx.advance_clock(Duration::from_millis(600));
    draw(&mut cx, handle)?;
    let open = record(&mut cx, handle, "git-hunks", "open")?;
    assert!(
        closed != opening,
        "the opening frame matches the closed frame"
    );
    assert!(opening != open, "the opening frame matches the open frame");
    cx.update_window(handle, |_, _, cx| {
        let panel = panel.read(cx);
        assert!(panel.show_hunks);
        assert!(panel.tree_scroll.0.borrow().base_handle.bounds().bottom() < tree_bottom);
    })?;

    let other = fixture.path().join("other");
    fs::create_dir(&other)?;
    git(&other, &["init", "-qb", "main"]);
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |workspace, cx| workspace.open_locator(window, cx));
    })?;
    cx.advance_clock(Duration::from_millis(150));
    draw(&mut cx, handle)?;
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.close_locator(true, window, cx);
            workspace.open_repository(
                Location {
                    root: other.clone(),
                    file: None,
                },
                window,
                cx,
            );
        });
    })?;
    draw(&mut cx, handle)?;
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            assert_eq!(workspace.repository.as_ref().unwrap().root, other);
            assert!(workspace.locator_overlay(window, cx).is_none());
        });
    })?;
    Ok(())
}
