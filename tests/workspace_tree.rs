#![cfg(target_os = "linux")]

use std::{fs, process::Command, sync::Arc, time::Duration};

use gpui::{
    AppContext as _, Bounds, HeadlessAppContext, MouseMoveEvent, PlatformInput, ScaledPixels,
    point, px, size,
};
use gpui_component::Root;
use stroll::{
    config::ConfigFile,
    repository::{Location, Store},
    workspace::{Workspace, WorkspaceView},
};

#[test]
fn modified_file_row_does_not_show_a_status_tooltip() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("example");
    fs::create_dir(&repo)?;
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "test@example.invalid"]);
    fs::write(repo.join("alpha.txt"), "one\n")?;
    git(&["add", "."]);
    git(&["commit", "-qm", "initial"]);
    fs::write(repo.join("alpha.txt"), "one\ntwo\n")?;

    let store = Arc::new(Store::new(temp.path().join("state"))?);
    let config = ConfigFile::load(Some(temp.path().join("config.toml")));
    let mut cx = HeadlessAppContext::with_platform(
        gpui_platform::current_platform(true).text_system(),
        Arc::new(gpui_kit_assets::AllAssets),
        || Ok(None),
    );
    let handle = cx.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
        cx.open_window(
            gpui::WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(800.), px(600.)),
                ))),
                focus: false,
                show: false,
                ..Default::default()
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
    for _ in 0..3 {
        cx.run_until_parked();
        let _ = cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear(cx))?;
    }

    let before = cx.update_window(handle.into(), |_, window, _| window.painted_quads())?;
    // The first tree row sits below the 36px title bar and the 40px tree header.
    let position = point(px(80.), px(90.));
    cx.update_window(handle.into(), |_, window, cx| {
        window.dispatch_event(
            PlatformInput::MouseMove(MouseMoveEvent {
                position,
                ..Default::default()
            }),
            cx,
        );
    })?;
    cx.advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    let _ = cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear(cx))?;
    let after = cx.update_window(handle.into(), |_, window, _| window.painted_quads())?;
    let popup = after.iter().find(|quad| {
        // The tooltip surface is translucent; the hovered row background is opaque.
        !before.iter().any(|old| old.bounds == quad.bounds)
            && quad.bounds.size.width > ScaledPixels(20.)
            && quad.bounds.size.height > ScaledPixels(20.)
            && quad
                .background
                .as_solid()
                .is_some_and(|color| color.a > 0. && color.a < 1.)
    });
    assert!(popup.is_none(), "unexpected tooltip surface");
    Ok(())
}
