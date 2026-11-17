use std::{fs, process::Command, time::Instant};

use anyhow::Context as _;
use gpui_component::Root;

use super::*;
use crate::{config::ConfigFile, repository::Store, workspace::FindReferences};

fn wait_for(
    visual: &mut VisualTestContext,
    step: &str,
    mut ready: impl FnMut(&mut VisualTestContext) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        visual.executor().advance_clock(Duration::from_millis(20));
        visual.run_until_parked();
        visual.update(|window, cx| {
            window.simulate_next_frame(cx);
            window.draw(cx).clear(cx);
        });
        if ready(visual) {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "Timed out waiting for {step}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn workflow(authority: Option<String>) -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().canonicalize()?.join("repository");
    let main = PathBuf::from("main.cpp");
    let usage = PathBuf::from("使用 #.cpp");
    let declaration = PathBuf::from("symbol.cpp");
    fs::create_dir(&root)?;
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()?
            .success()
    );
    fs::write(root.join(&main), "word();\n")?;
    fs::write(root.join(&usage), "😀 word();\n")?;
    fs::write(root.join(&declaration), "int word();\n")?;
    let script = fixture.path().join("server.py");
    fs::write(
        &script,
        include_str!("../../../tests/fixtures/lsp_references.py"),
    )?;
    let config_path = fixture.path().join("config.toml");
    fs::write(
        &config_path,
        format!(
            "[lsp.clangd]\ncommand = 'python3 -u {}'\n",
            script.display()
        ),
    )?;
    let config = ConfigFile::load(Some(config_path));
    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let platform = gpui_platform::current_platform(cfg!(target_os = "linux"));
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        platform.text_system(),
    );
    app.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
        cx.bind_keys([KeyBinding::new(
            "shift-f12",
            FindReferences,
            Some("WorkspaceEditor > (Input && mode == editor && !Locator)"),
        )]);
    });
    let mut workspace = None;
    let (_, visual) = app.add_window_view(|window, cx| {
        let view =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
        workspace = Some(view.clone());
        Root::new(view, window, cx)
    });
    let workspace = workspace.unwrap();
    visual.simulate_resize(size(px(1050.), px(650.)));
    let location = authority.map_or_else(
        || root.clone(),
        |authority| PathBuf::from(format!("ssh://{authority}{}", root.display())),
    );
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(
                crate::repository::Location {
                    root: location,
                    file: Some(main.clone()),
                },
                window,
                cx,
            );
        });
    });
    wait_for(visual, "reference capability", |visual| {
        visual.update(|_, cx| {
            workspace
                .read(cx)
                .active_file()
                .and_then(|file| file.language_server.as_ref())
                .is_some_and(|server| server.document.supports_references())
        })
    })?;
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.request_file(
                usage.clone(),
                FileOpen::Select {
                    preview: false,
                    position: None,
                },
                window,
                cx,
            );
        });
    });
    wait_for(visual, "target file", |visual| {
        visual.update(|_, cx| workspace.read(cx).tabs.active() == Some(&usage))
    })?;
    let draft = "// draft\n😀 word();\n";
    let symbol_offset = "😀 ".len();
    let source = visual.update(|window, cx| {
        let target = workspace.read(cx).files[&usage].editor.clone();
        target.update(cx, |editor, cx| editor.replace_all(draft, window, cx));
        workspace.update(cx, |workspace, cx| {
            workspace.request_file(
                main.clone(),
                FileOpen::Select {
                    preview: false,
                    position: None,
                },
                window,
                cx,
            );
        });
        let source = workspace.read(cx).files[&main].editor.clone();
        source.update(cx, |editor, cx| {
            editor.replace_all("😀 word();\n", window, cx);
            editor.set_selected_range(symbol_offset..symbol_offset, cx);
            editor.focus(window, cx);
        });
        source
    });
    visual.simulate_keystrokes("shift-f12");
    let wait_for_results = |visual: &mut VisualTestContext, count, step| {
        wait_for(visual, step, |visual| {
            visual.update(|_, cx| {
                let locator = &workspace.read(cx).locator;
                locator.open
                    && !locator.busy
                    && !locator.preview_busy
                    && locator.items.len() == count
            })
        })
        .with_context(|| {
            visual.update(|_, cx| {
                let locator = &workspace.read(cx).locator;
                format!(
                    "Expected {count} results, got {} (open: {}, search: {:?}, preview: {:?})",
                    locator.items.len(),
                    locator.open,
                    locator.error,
                    locator.preview_error
                )
            })
        })
    };
    wait_for_results(visual, 1, "shortcut results")?;
    visual.update(|_, cx| {
        let locator = &workspace.read(cx).locator;
        assert!(!locator.references.as_ref().unwrap().include_declaration);
        assert_eq!(locator.skipped, 1);
        let preview = locator.preview.as_ref().unwrap();
        assert_eq!(preview.entry.path, usage);
        assert_eq!(preview.text, draft);
        let offset = "// draft\n😀 ".len();
        assert_eq!(preview.range, Some(offset..offset + 4));
    });

    let checkbox = visual.debug_bounds("checkbox-indicator").unwrap();
    visual.simulate_click(checkbox.center(), Modifiers::default());
    wait_for_results(visual, 2, "results including the declaration")?;
    visual.update(|_, cx| {
        let locator = &workspace.read(cx).locator;
        assert!(locator.references.as_ref().unwrap().include_declaration);
        assert_eq!(locator.items[0].path, declaration);
        assert_eq!(locator.preview.as_ref().unwrap().entry.path, usage);
    });
    visual.simulate_keystrokes("escape");
    visual.update(|window, cx| {
        assert!(source.read(cx).focus_handle(cx).is_focused(window));
        source.update(cx, |editor, cx| editor.set_selected_range(0..0, cx));
    });
    visual.simulate_keystrokes("shift-f12");
    wait_for_results(visual, 0, "empty results")?;
    visual.update(|_, cx| assert!(workspace.read(cx).locator.error.is_none()));
    visual.simulate_keystrokes("escape");
    wait_for(visual, "locator dismissal", |visual| {
        visual.update(|_, cx| workspace.read(cx).locator.motion.is_none())
    })?;

    let hit = visual.update(|_, cx| {
        source
            .read(cx)
            .range_to_bounds(&(symbol_offset..symbol_offset + 4))
            .unwrap()
    });
    let open_menu = |visual: &mut VisualTestContext| {
        visual.simulate_mouse_down(hit.center(), MouseButton::Right, Modifiers::default());
        visual.simulate_mouse_up(hit.center(), MouseButton::Right, Modifiers::default());
        wait_for(visual, "editor context menu focus", |visual| {
            visual.update(|window, cx| !source.read(cx).focus_handle(cx).is_focused(window))
        })
    };
    open_menu(visual)?;
    // Whole-line clipboard support enables Cut and Copy ahead of Paste.
    visual.simulate_keystrokes("down down down down enter");
    wait_for(visual, "built-in Select All menu action", |visual| {
        visual.update(|_, cx| source.read(cx).selected_range() == (0.."😀 word();\n".len()))
    })?;
    visual.update(|_, cx| source.update(cx, |editor, cx| editor.set_selected_range(0..0, cx)));
    open_menu(visual)?;
    visual.simulate_keystrokes("up enter");
    wait_for_results(visual, 1, "context menu results")?;
    visual.simulate_keystrokes("enter");
    wait_for(visual, "reference jump", |visual| {
        visual.update(|_, cx| {
            let workspace = workspace.read(cx);
            workspace.tabs.active() == Some(&usage) && !workspace.locator.open
        })
    })?;
    visual.update(|_, cx| {
        let editor = workspace.read(cx).files[&usage].editor.read(cx);
        assert_eq!(editor.text(), draft);
        let offset = "// draft\n😀 ".len();
        assert_eq!(editor.selected_range(), offset..offset + 4);
    });
    assert_eq!(fs::read_to_string(root.join(main))?, "word();\n");
    assert_eq!(fs::read_to_string(root.join(usage))?, "😀 word();\n");
    Ok(())
}

#[::core::prelude::v1::test]
fn find_references_previews_drafts_and_jumps() -> Result<()> {
    workflow(None)
}

#[::core::prelude::v1::test]
#[ignore = "requires a disposable SSH host with a fixture clangd command"]
fn find_references_over_ssh() -> Result<()> {
    workflow(Some(std::env::var("STROLL_TEST_LSP_SSH_AUTHORITY")?))
}
