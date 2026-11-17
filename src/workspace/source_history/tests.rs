use std::{
    fs,
    process::Command,
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use gpui_component::{
    Root,
    input::{Editor, RopeExt as _},
};

use super::*;
use crate::{
    config::ConfigFile,
    repository::{Location, Store},
};
use std::sync::Arc;

fn frame(visual: &mut VisualTestContext) {
    visual.executor().advance_clock(Duration::from_millis(25));
    visual.run_until_parked();
    visual.update(|window, cx| {
        window.simulate_next_frame(cx);
        window.draw(cx).clear(cx);
    });
}

fn wait_for(visual: &mut VisualTestContext, ready: impl Fn(&App) -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        frame(visual);
        if visual.update(|_, cx| ready(cx)) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "Timed out waiting for source navigation"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[::core::prelude::v1::test]
fn source_history_replays_files_jumps_and_worktree_positions() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("main");
    let other = fixture.path().join("other");
    fs::create_dir(&root)?;
    let git = |args: &[&str]| -> Result<()> {
        let result = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()?;
        ensure!(
            result.status.success(),
            "{:?}: {}",
            args,
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(())
    };
    git(&["init", "-qb", "main"])?;
    let alpha = Path::new("alpha.txt");
    let beta = Path::new("beta.txt");
    let gamma = Path::new("gamma.txt");
    for file in [alpha, beta, gamma] {
        fs::write(root.join(file), "line with text\n".repeat(160))?;
    }
    git(&["add", "."])?;
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-qm",
        "initial",
    ])?;
    git(&["worktree", "add", "-qb", "other", other.to_str().unwrap()])?;
    let state = Arc::new(Store::new(fixture.path().join("state"))?);
    let reopened_state = state.clone();
    let config_file = fixture.path().join("config.toml");
    let reopened_config = config_file.clone();
    fs::write(
        &config_file,
        "[lsp]\nenabled = false\n[terminal]\nshell = '/bin/sh'\n",
    )?;
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    app.update(gpui_component::init);
    let mut workspace = None;
    let (_, visual) = app.add_window_view(|window, cx| {
        let view = cx.new(|cx| {
            Workspace::with_config(
                state,
                ConfigFile::load(Some(config_file)),
                WorkspaceView::Editor,
                window,
                cx,
            )
        });
        workspace = Some(view.clone());
        Root::new(view, window, cx)
    });
    let workspace = workspace.unwrap();
    visual.simulate_resize(size(px(1100.), px(520.)));
    let open = |visual: &mut VisualTestContext, repository: &Path, file: Option<&Path>| {
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root: repository.to_owned(),
                        file: file.map(Path::to_owned),
                    },
                    window,
                    cx,
                );
            });
        });
    };
    let select = |visual: &mut VisualTestContext, path: &Path| {
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_file(path.to_owned(), false, window, cx);
            });
        });
    };
    let active = |cx: &App, repo: &Path, path: &Path| {
        let workspace = workspace.read(cx);
        workspace
            .repository
            .as_ref()
            .is_some_and(|current| current.root == repo)
            && workspace
                .active_file()
                .is_some_and(|file| file.path == path && file.restore_position.is_none())
    };
    let position = |cx: &App| {
        workspace
            .read(cx)
            .source_location(cx)
            .unwrap()
            .position
            .unwrap()
    };
    let press_side_button = |visual: &mut VisualTestContext, direction| {
        let pointer = visual.debug_bounds("workspace-editor").unwrap().center();
        visual.simulate_mouse_down(pointer, MouseButton::Navigate(direction), Modifiers::none());
        pointer
    };

    open(visual, &root, Some(alpha));
    wait_for(visual, |cx| active(cx, &root, alpha))?;
    visual.update(|_, cx| {
        let editor = workspace.read(cx).active_file().unwrap().editor.clone();
        editor.update(cx, |editor, cx| {
            let offset = editor.text().position_to_offset(&Position::new(70, 2));
            editor.set_selected_range_with_direction(offset..offset + 4, true, cx);
            editor.restore_viewport_anchor(Position::new(60, 0), px(3.), px(0.), cx);
        });
    });
    frame(visual);
    let first = visual.update(|_, cx| position(cx));
    assert_eq!(first.selection_start, (70, 2));
    assert!(first.reversed);
    assert_eq!(first.top, (60, 0));

    select(visual, beta);
    wait_for(visual, |cx| active(cx, &root, beta))?;
    visual.simulate_keystrokes("alt-left");
    wait_for(visual, |cx| {
        active(cx, &root, alpha) && position(cx) == first
    })?;
    visual.simulate_keystrokes("alt-right");
    wait_for(visual, |cx| active(cx, &root, beta))?;
    let before_jump = visual.update(|_, cx| position(cx));
    let uri: lsp_types::Uri = url::Url::from_file_path(root.join(beta))
        .unwrap()
        .as_str()
        .parse()?;
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            let at = lsp_types::Position::new(110, 4);
            workspace.open_definition(
                lsp_types::ShowDocumentParams {
                    uri,
                    selection: Some(lsp_types::Range::new(at, at)),
                    external: None,
                    take_focus: Some(true),
                },
                window,
                cx,
            );
        });
    });
    frame(visual);
    let after_jump = visual.update(|_, cx| position(cx));
    assert_eq!(after_jump.selection_start, (110, 4));
    assert!(after_jump.top.0 > 80);

    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| workspace.find(window, cx));
    });
    frame(visual);
    visual.update(|window, cx| {
        let file = workspace.read(cx).active_file().unwrap();
        assert!(Editor::search_has_focus(&file.editor, window, cx));
    });
    visual.simulate_keystrokes("alt-left");
    press_side_button(visual, NavigationDirection::Back);
    frame(visual);
    assert_eq!(visual.update(|_, cx| position(cx)), after_jump);

    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_terminal_menu(window, cx);
        });
    });
    frame(visual);
    assert!(visual.update(|window, cx| workspace.read(cx).terminal_focused(window, cx)));
    visual.simulate_keystrokes("alt-left");
    press_side_button(visual, NavigationDirection::Back);
    frame(visual);
    assert_eq!(
        visual.update(|_, cx| position(cx).selection_start),
        (110, 4)
    );
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_terminal_menu(window, cx);
            workspace
                .active_file()
                .unwrap()
                .editor
                .update(cx, |editor, cx| editor.focus(window, cx));
        });
    });
    frame(visual);

    let pointer = press_side_button(visual, NavigationDirection::Back);
    wait_for(visual, |cx| {
        active(cx, &root, beta) && position(cx) == before_jump
    })?;
    visual.simulate_mouse_up(
        pointer,
        MouseButton::Navigate(NavigationDirection::Back),
        Modifiers::none(),
    );
    frame(visual);
    assert_eq!(visual.update(|_, cx| position(cx)), before_jump);
    press_side_button(visual, NavigationDirection::Forward);
    wait_for(visual, |cx| {
        active(cx, &root, beta) && position(cx).selection_start == (110, 4)
    })?;
    visual.simulate_keystrokes("alt-left");
    wait_for(visual, |cx| {
        active(cx, &root, beta) && position(cx) == before_jump
    })?;
    visual.simulate_keystrokes("alt-left");
    wait_for(visual, |cx| active(cx, &root, alpha))?;
    select(visual, gamma);
    wait_for(visual, |cx| active(cx, &root, gamma))?;
    visual.simulate_keystrokes("alt-right");
    frame(visual);
    assert!(visual.update(|_, cx| active(cx, &root, gamma)));

    open(visual, &other, Some(beta));
    wait_for(visual, |cx| active(cx, &other, beta))?;
    visual.simulate_keystrokes("alt-left");
    frame(visual);
    assert!(visual.update(|_, cx| active(cx, &other, beta)));
    open(visual, &root, None);
    wait_for(visual, |cx| active(cx, &root, gamma))?;
    visual.simulate_keystrokes("alt-left");
    wait_for(visual, |cx| {
        active(cx, &root, alpha) && position(cx) == first
    })?;
    let click = visual.update(|_, cx| {
        let editor = workspace.read(cx).active_file().unwrap().editor.read(cx);
        let offset = editor.text().position_to_offset(&Position::new(73, 7));
        editor.range_to_bounds(&(offset..offset)).unwrap().center()
    });
    visual.simulate_click(click, Modifiers::none());
    frame(visual);
    let clicked = visual.update(|_, cx| position(cx));
    assert_eq!(clicked.selection_start, (73, 7));
    visual.simulate_keystrokes("alt-left");
    wait_for(visual, |cx| position(cx) == first)?;

    visual.update(|_, cx| workspace.update(cx, |this, cx| this.flush_session(cx)))?;
    let root_to_open = root.clone();
    let mut reopened = None;
    let (_, visual) = app.add_window_view(|window, cx| {
        let view = cx.new(|cx| {
            let mut workspace = Workspace::with_config(
                reopened_state,
                ConfigFile::load(Some(reopened_config)),
                WorkspaceView::Editor,
                window,
                cx,
            );
            workspace.open_repository(
                Location {
                    root: root_to_open,
                    file: None,
                },
                window,
                cx,
            );
            workspace
        });
        reopened = Some(view.clone());
        Root::new(view, window, cx)
    });
    let reopened = reopened.unwrap();
    visual.simulate_resize(size(px(1100.), px(520.)));
    let restored = |cx: &App, expected| {
        reopened
            .read(cx)
            .active_file()
            .is_some_and(|file| file.restore_position.is_none())
            && reopened
                .read(cx)
                .source_location(cx)
                .and_then(|location| location.position)
                == Some(expected)
    };
    wait_for(visual, |cx| restored(cx, first))?;
    visual.simulate_keystrokes("alt-right");
    wait_for(visual, |cx| restored(cx, clicked))?;
    visual.simulate_keystrokes("alt-left");
    wait_for(visual, |cx| restored(cx, first))?;
    visual.simulate_keystrokes("pagedown");
    wait_for(visual, |cx| {
        reopened
            .read(cx)
            .source_location(cx)
            .and_then(|location| location.position)
            .is_some_and(|position| position.selection_end.0 > first.selection_end.0 + 8)
    })?;
    visual.simulate_keystrokes("alt-left");
    wait_for(visual, |cx| restored(cx, first))?;

    // Leave an unavailable location between the current entry and the next one.
    visual.update(|_, cx| {
        reopened.update(cx, |workspace, _| {
            let history = workspace.source_history_mut().unwrap();
            history.entries.insert(
                history.index + 1,
                SourceLocation {
                    path: "missing.txt".into(),
                    position: None,
                },
            );
        });
    });
    visual.simulate_keystrokes("alt-right");
    wait_for(visual, |cx| {
        let history = &reopened.read(cx).session.source_history;
        history
            .entries
            .iter()
            .all(|location| location.path != Path::new("missing.txt"))
    })?;
    assert!(visual.update(|_, cx| !restored(cx, first)));
    Ok(())
}
