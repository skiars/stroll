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

#[track_caller]
fn wait_for(visual: &mut VisualTestContext, ready: impl Fn(&App) -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        frame(visual);
        if visual.update(|_, cx| ready(cx)) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "Timed out waiting for source navigation at {}",
            std::panic::Location::caller()
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
    let readme = Path::new("README.md");
    let guide = Path::new("guide.md");
    fs::write(
        root.join(readme),
        format!(
            "# Readme\n\n{}\n## End\n",
            "Reading position.\n\n".repeat(25)
        ),
    )?;
    fs::write(
        root.join(guide),
        "# Guide\n\n## Target\n\nSee [source](alpha.txt#L42).\n",
    )?;
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
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| workspace.find(window, cx));
    });
    frame(visual);
    visual.update(|window, cx| {
        let editor = workspace.read(cx).active_file().unwrap().editor.clone();
        assert!(Editor::search_has_focus(&editor, window, cx));
        editor.update(cx, |state, cx| state.focus(window, cx));
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
    visual.update(|window, cx| {
        let editor = &workspace.read(cx).active_file().unwrap().editor;
        let state = editor.read(cx);
        assert!(state.search_session().open);
        assert!(state.focus_handle(cx).is_focused(window));
    });
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
    crate::workspace::finish_scroll_navigation(visual);
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
    crate::workspace::finish_scroll_navigation(visual);
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
                    path: Some("missing.txt".into()),
                    position: None,
                    document: None,
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
            .all(|location| location.path.as_deref() != Some(Path::new("missing.txt")))
    })?;
    assert!(visual.update(|_, cx| !restored(cx, first)));

    let editor = visual.update(|window, cx| {
        let editor = reopened.read(cx).active_file().unwrap().editor.clone();
        editor.update(cx, |state, cx| {
            state.open_search(false, cx);
            state.set_search_query("line", true, cx);
            state.set_selected_range(0..0, cx);
            state.focus(window, cx);
        });
        editor
    });
    frame(visual);
    visual.update(|window, cx| editor.update(cx, |state, cx| state.focus(window, cx)));
    for (key, selection) in [
        ("f3", 0..4),
        ("f3", 15..19),
        ("shift-f3", 0..4),
        ("escape f3", 15..19),
    ] {
        visual.simulate_keystrokes(key);
        frame(visual);
        visual.update(|window, cx| {
            let state = editor.read(cx);
            assert!(state.focus_handle(cx).is_focused(window));
            assert!(state.search_session().open);
            assert_eq!(state.selected_range(), selection);
        });
    }
    let navigate =
        |visual: &mut VisualTestContext,
         action: fn(&mut Workspace, &mut Window, &mut Context<Workspace>)| {
            visual.update(|window, cx| {
                reopened.update(cx, |workspace, cx| action(workspace, window, cx))
            });
        };
    let reading = |cx: &App, path: &Path, fragment: Option<&str>| {
        let workspace = reopened.read(cx);
        workspace.view == WorkspaceView::Docs
            && workspace
                .document_history_location(cx)
                .is_some_and(|location| {
                    location.path.as_deref() == Some(path)
                        && location.document.is_some_and(|document| {
                            document.fragment.as_deref() == fragment && document.position.is_some()
                        })
                })
    };
    navigate(visual, |workspace, window, cx| {
        workspace.open_file("README.md".into(), false, window, cx);
    });
    wait_for(visual, |cx| {
        reopened
            .read(cx)
            .active_file()
            .is_some_and(|file| file.path == readme)
    })?;
    navigate(visual, |workspace, window, cx| {
        workspace.open_markdown_in_docs("README.md".into(), window, cx);
    });
    wait_for(visual, |cx| reading(cx, readme, None))?;
    let end = visual.debug_bounds("document-heading-1").unwrap();
    visual.simulate_click(end.center(), Modifiers::none());
    wait_for(visual, |cx| reading(cx, readme, Some("end")))?;
    navigate(visual, |workspace, window, cx| {
        workspace.follow_document_link(Path::new("README.md"), "guide.md#target", window, cx);
    });
    wait_for(visual, |cx| reading(cx, guide, Some("target")))?;
    navigate(visual, |workspace, window, cx| {
        workspace.follow_document_link(Path::new("guide.md"), "beta.txt#L42", window, cx);
    });
    wait_for(visual, |cx| {
        let workspace = reopened.read(cx);
        workspace.view == WorkspaceView::Editor
            && workspace.source_location(cx).is_some_and(|location| {
                location.path.as_deref() == Some(beta)
                    && location
                        .position
                        .is_some_and(|position| position.selection_start == (41, 0))
            })
    })?;
    for (path, fragment) in [
        (guide, Some("target")),
        (readme, Some("end")),
        (readme, None),
    ] {
        visual.simulate_keystrokes("alt-left");
        wait_for(visual, |cx| reading(cx, path, fragment))?;
    }
    let pointer = point(px(620.), px(240.));
    visual.simulate_mouse_down(
        pointer,
        MouseButton::Navigate(NavigationDirection::Back),
        Modifiers::none(),
    );
    visual.simulate_mouse_up(
        pointer,
        MouseButton::Navigate(NavigationDirection::Back),
        Modifiers::none(),
    );
    wait_for(visual, |cx| {
        let workspace = reopened.read(cx);
        workspace.view == WorkspaceView::Editor
            && workspace
                .active_file()
                .is_some_and(|file| file.path == readme)
    })?;
    visual.simulate_keystrokes("alt-right");
    wait_for(visual, |cx| reading(cx, readme, None))?;
    let click_nav = |visual: &mut VisualTestContext, id: &'static str| {
        let button = visual.debug_bounds(id).unwrap();
        visual.simulate_click(button.center(), Modifiers::none());
    };
    let editor_readme = |cx: &App| {
        let workspace = reopened.read(cx);
        workspace.view == WorkspaceView::Editor
            && workspace
                .active_file()
                .is_some_and(|file| file.path == readme)
    };

    // Go back before a cold topbar switch completes in either direction.
    visual.update(|window, cx| {
        reopened.update(cx, |workspace, cx| {
            let index = workspace.session.source_history.index;
            assert!(workspace.files.remove(readme).is_some());
            workspace.switch_view_from_topbar(WorkspaceView::Editor, window, cx);
            assert!(matches!(
                workspace.loading_file,
                Some((_, FileOpen::Restore { from: Some(_) }))
            ));
            workspace.source_back(&SourceBack, window, cx);
            assert_eq!(workspace.view, WorkspaceView::Docs);
            assert_eq!(workspace.doc.as_ref().unwrap().read(cx).path, readme);
            assert_eq!(workspace.session.source_history.index, index);
        });
    });
    wait_for(visual, |cx| reading(cx, readme, None))?;

    click_nav(visual, "nav-workspace");
    wait_for(visual, editor_readme)?;
    visual.update(|window, cx| {
        reopened.update(cx, |workspace, cx| {
            let index = workspace.session.source_history.index;
            workspace.doc = None;
            workspace.switch_view_from_topbar(WorkspaceView::Docs, window, cx);
            assert!(workspace.doc_loading.is_some());
            workspace.source_back(&SourceBack, window, cx);
            assert_eq!(workspace.view, WorkspaceView::Editor);
            assert_eq!(workspace.tabs.active(), Some(readme));
            assert_eq!(workspace.session.source_history.index, index);
        });
    });
    visual.simulate_keystrokes("alt-left");
    wait_for(visual, |cx| reading(cx, readme, None))?;
    visual.simulate_keystrokes("alt-right");
    wait_for(visual, editor_readme)?;
    click_nav(visual, "nav-docs");
    wait_for(visual, |cx| reading(cx, readme, None))?;
    visual.simulate_keystrokes("alt-left");
    wait_for(visual, editor_readme)?;

    // An empty editor is a history location too. Replaying it preserves tabs opened since.
    visual.update(|window, cx| {
        reopened.update(cx, |workspace, cx| {
            let previous = workspace.tabs.deselect();
            workspace.finish_tab_selection(previous.as_deref(), window, cx);
        })
    });
    frame(visual);
    click_nav(visual, "nav-docs");
    wait_for(visual, |cx| reading(cx, readme, None))?;
    visual.simulate_keystrokes("alt-left");
    wait_for(visual, |cx| {
        let workspace = reopened.read(cx);
        workspace.view == WorkspaceView::Editor
            && workspace.tabs.active().is_none()
            && workspace.tabs.paths().iter().any(|path| path == readme)
    })?;
    visual.simulate_keystrokes("alt-right");
    wait_for(visual, |cx| reading(cx, readme, None))?;
    Ok(())
}
