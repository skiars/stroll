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

fn workflow(authority: Option<String>, vim: bool) -> Result<()> {
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
    if vim {
        let readme = (0..40)
            .map(|row| {
                if row == 28 {
                    "unique_reader_term\n\n".to_owned()
                } else {
                    format!("Paragraph {row} in the reader.\n\n")
                }
            })
            .collect::<String>();
        fs::write(root.join("README.md"), &readme)?;
        fs::write(root.join(".gitignore"), "ignored/\n")?;
        fs::create_dir(root.join("ignored"))?;
        fs::write(
            root.join("ignored/guide.MDX"),
            readme.replace("unique_reader_term", "ignored_reader_term"),
        )?;
        fs::write(
            root.join("guide.MDX.cpp"),
            "unique_reader_term ignored_reader_term\n",
        )?;
    }
    let script = fixture.path().join("server.py");
    fs::write(
        &script,
        include_str!("../../../tests/fixtures/lsp_references.py"),
    )?;
    let config_path = fixture.path().join("config.toml");
    fs::write(
        &config_path,
        format!(
            "[keyboard]\nvim_mode = {vim}\n[lsp.clangd]\ncommand = 'python3 -u {}'\n",
            script.display()
        ),
    )?;
    let config = ConfigFile::load(Some(config_path));
    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        crate::test_support::text_system(cfg!(target_os = "linux")),
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
    if vim {
        let accept = |visual: &mut VisualTestContext, keys, mode, query| -> Result<()> {
            visual.simulate_keystrokes(keys);
            wait_for(visual, "Locator mode", |visual| {
                visual.update(|_, cx| {
                    let locator = &workspace.read(cx).locator;
                    locator.open && locator.mode == mode
                })
            })?;
            if let Some(query) = query {
                visual.simulate_input(query);
            }
            wait_for(visual, "Locator match preview", |visual| {
                visual.update(|_, cx| {
                    let locator = &workspace.read(cx).locator;
                    let ready = !locator.busy && !locator.preview_busy && locator.preview.is_some();
                    if ready && workspace.read(cx).view == WorkspaceView::Docs {
                        assert!(
                            locator
                                .items
                                .iter()
                                .all(|item| docs::is_markdown(&item.path))
                        );
                    }
                    ready
                })
            })?;
            visual.simulate_keystrokes("enter");
            Ok(())
        };
        let check_editor = |visual: &mut VisualTestContext, step, path: &Path, offset| {
            wait_for(visual, step, |visual| {
                visual.update(|window, cx| {
                    let workspace = workspace.read(cx);
                    if workspace.view != WorkspaceView::Editor
                        || workspace.locator.open
                        || workspace.tabs.active() != Some(path)
                    {
                        return false;
                    }
                    let file = workspace.active_file().unwrap();
                    let editor = file.editor.read(cx);
                    file.vim.mode() == crate::workspace::vim::source::Mode::Normal
                        && editor.focus_handle(cx).is_focused(window)
                        && editor.selected_range() == (offset..offset)
                        && editor.cursor() == offset
                })
            })
        };
        let round_trip =
            |visual: &mut VisualTestContext, keys, mode, query, path: &Path, offset, origin| {
                accept(visual, keys, mode, query)?;
                check_editor(visual, "Normal-mode jump", path, offset)?;
                visual.simulate_keystrokes("ctrl-o");
                check_editor(visual, "jump back to the originating caret", &main, origin)?;
                visual.simulate_keystrokes("ctrl-i");
                check_editor(visual, "jump forward to the accepted result", path, offset)?;
                visual.simulate_keystrokes("ctrl-o");
                check_editor(visual, "jump back before the next query", &main, origin)
            };
        visual.simulate_keystrokes("l l");
        round_trip(
            visual,
            "space s s",
            Mode::DocumentSymbols,
            None,
            &main,
            0,
            2,
        )?;
        round_trip(
            visual,
            "space s shift-s",
            Mode::WorkspaceSymbols,
            Some("word"),
            &declaration,
            4,
            2,
        )?;
        round_trip(
            visual,
            "space s g",
            Mode::Text,
            Some("int word"),
            &declaration,
            0,
            2,
        )?;
        round_trip(
            visual,
            "g r",
            Mode::References,
            None,
            &usage,
            "😀 ".len(),
            2,
        )?;

        visual.simulate_keystrokes("space 1");
        let check_reader = |visual: &mut VisualTestContext, step, at_top| {
            wait_for(visual, step, |visual| {
                visual.update(|window, cx| {
                    let this = workspace.read(cx);
                    this.view == WorkspaceView::Docs
                        && this.doc.as_ref().is_some_and(|reader| {
                            let reader = reader.read(cx);
                            reader.content_focused(window, cx)
                                && reader.session_position(cx).is_some_and(|position| {
                                    if at_top {
                                        position.block == 0
                                    } else {
                                        position.block > 0
                                    }
                                })
                        })
                })
            })
            .with_context(|| {
                visual.update(|window, cx| {
                    let this = workspace.read(cx);
                    let reader = this.doc.as_ref().map(|reader| reader.read(cx));
                    format!(
                        "View: {:?}, reader focus: {}, position: {:?}",
                        this.view,
                        reader.is_some_and(|reader| reader.content_focused(window, cx)),
                        reader.and_then(|reader| reader.session_position(cx)),
                    )
                })
            })
        };
        check_reader(visual, "reading position before Locator", true)?;
        accept(visual, "space s g", Mode::Text, Some("unique_reader_term"))?;
        check_reader(visual, "text result in the reader", false)?;
        let reader_position = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| {
                workspace
                    .read(cx)
                    .doc
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .session_position(cx)
            })
        };
        let destination = reader_position(visual);
        visual.simulate_keystrokes("ctrl-o");
        check_reader(visual, "return to the original reading position", true)?;
        visual.simulate_keystrokes("ctrl-i");
        check_reader(visual, "return to the text result in the reader", false)?;
        assert_eq!(reader_position(visual), destination);
        visual.simulate_keystrokes("ctrl-o");
        check_reader(
            visual,
            "restore reader before opening the ignored document",
            true,
        )?;
        let check_ignored = |visual: &mut VisualTestContext| {
            wait_for(visual, "ignored document in the reader", |visual| {
                visual.update(|_, cx| {
                    let this = workspace.read(cx);
                    this.view == WorkspaceView::Docs
                        && this
                            .doc
                            .as_ref()
                            .is_some_and(|doc| doc.read(cx).path == Path::new("ignored/guide.MDX"))
                })
            })
        };
        accept(visual, "space f f", Mode::Files, Some("guide.MDX"))?;
        check_ignored(visual)?;
        check_reader(visual, "ignored document opened at the top", true)?;
        visual.simulate_keystrokes("ctrl-o");
        check_reader(visual, "restore reader before ignored text jump", true)?;
        accept(visual, "space s g", Mode::Text, Some("ignored_reader_term"))?;
        check_ignored(visual)?;
        check_reader(visual, "ignored text result in the reader", false)?;

        visual.simulate_keystrokes("space 3");
        let check_git = |visual: &mut VisualTestContext| {
            wait_for(visual, "focused Git view", |visual| {
                visual.update(|window, cx| {
                    let this = workspace.read(cx);
                    this.view == WorkspaceView::Git
                        && this
                            .git
                            .as_ref()
                            .is_some_and(|panel| panel.read(cx).vim_focused(window, cx))
                })
            })
        };
        check_git(visual)?;
        accept(visual, "space s g", Mode::Text, None)?;
        check_editor(visual, "source result from Git", &declaration, 0)?;
        visual.simulate_keystrokes("ctrl-o");
        check_git(visual)?;
        visual.simulate_keystrokes("ctrl-i");
        check_editor(visual, "redo the jump from Git", &declaration, 0)?;
        return Ok(());
    }
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
    workflow(None, false)
}

#[::core::prelude::v1::test]
fn vim_locator_symbols_and_text_jump_in_normal_mode() -> Result<()> {
    workflow(None, true)
}

#[::core::prelude::v1::test]
#[ignore = "requires a disposable SSH host with a fixture clangd command"]
fn find_references_over_ssh() -> Result<()> {
    workflow(Some(std::env::var("STROLL_TEST_LSP_SSH_AUTHORITY")?), false)
}
