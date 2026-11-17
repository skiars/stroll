use std::{
    fs,
    path::Path,
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use gpui::{
    AppContext as _, Entity, KeyBinding, TestAppContext, TestDispatcher, VisualTestContext, px,
    size,
};
use gpui_component::{
    Root,
    input::{EditorState, GoToDefinition, ShowCompletions},
};

use super::{LanguageService, SessionState, Workspace};
use crate::{
    config::ConfigFile,
    repository::{Location, Store},
    workspace::{FormatDocument, WorkspaceView},
};

fn with_editor(
    fixture: &Path,
    command: &str,
    source: &str,
    caret: usize,
    body: impl FnOnce(&mut VisualTestContext, &Entity<EditorState>) -> Result<()>,
) -> Result<()> {
    with_source_editor(
        fixture,
        "main.cpp",
        &format!("[lsp.clangd]\ncommand = '{command}'\n"),
        source,
        caret,
        body,
    )
}

pub(super) fn with_source_editor(
    fixture: &Path,
    file: &str,
    configuration: &str,
    source: &str,
    caret: usize,
    body: impl FnOnce(&mut VisualTestContext, &Entity<EditorState>) -> Result<()>,
) -> Result<()> {
    with_workspace_editor(
        fixture,
        file,
        configuration,
        source,
        caret,
        |visual, _, editor| body(visual, editor),
    )
}

pub(super) fn with_workspace_editor(
    fixture: &Path,
    file: &str,
    configuration: &str,
    source: &str,
    caret: usize,
    body: impl FnOnce(&mut VisualTestContext, &Entity<Workspace>, &Entity<EditorState>) -> Result<()>,
) -> Result<()> {
    let root = fixture.join("repository");
    fs::create_dir_all(&root)?;
    ensure!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()?
            .success()
    );
    fs::write(root.join(file), source)?;
    let config_path = fixture.join("config.toml");
    fs::write(&config_path, configuration)?;
    let config = ConfigFile::load(Some(config_path));
    let expect_server = config.current().lsp.enabled;
    let store = Arc::new(Store::new(fixture.join("state"))?);
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    app.update(|cx| {
        gpui_component::init(cx);
        cx.bind_keys([
            KeyBinding::new(
                "ctrl-space",
                ShowCompletions,
                Some("WorkspaceEditor > (Input && mode == editor && !Locator)"),
            ),
            KeyBinding::new(
                "f12",
                GoToDefinition,
                Some("Input && mode == editor && !Locator"),
            ),
            KeyBinding::new(
                "alt-shift-f",
                FormatDocument,
                Some("WorkspaceEditor > (Input && mode == editor && !Locator)"),
            ),
        ]);
    });
    let mut workspace = None;
    let (_, visual) = app.add_window_view(|window, cx| {
        let view =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
        workspace = Some(view.clone());
        Root::new(view, window, cx)
    });
    let workspace = workspace.unwrap();
    visual.simulate_resize(size(px(940.), px(580.)));
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(
                Location {
                    root,
                    file: Some(file.into()),
                },
                window,
                cx,
            );
        })
    });
    wait(visual, "source editor", |visual| {
        visual.update(|_, cx| {
            let workspace = workspace.read(cx);
            workspace.active_file().is_some_and(|file| {
                !expect_server || (file.language_server.is_some()
                    && file.server_project.as_ref().is_some_and(|project| {
                        matches!(
                            workspace.language_servers.get(&project.key),
                            Some(LanguageService::Session { client, .. }) if client.state() == SessionState::Ready
                        )
                    }))
            })
        })
    })?;
    let editor = visual.update(|window, cx| {
        let editor = workspace.read(cx).active_file().unwrap().editor.clone();
        editor.update(cx, |state, cx| {
            state.set_selected_range(caret..caret, cx);
            state.focus(window, cx);
        });
        editor
    });
    frame(visual);
    body(visual, &workspace, &editor)
}

pub(super) fn frame(visual: &mut VisualTestContext) {
    visual.executor().advance_clock(Duration::from_millis(25));
    visual.run_until_parked();
    visual.update(|window, cx| {
        window.simulate_next_frame(cx);
        window.draw(cx).clear(cx);
    });
}

pub(super) fn wait(
    visual: &mut VisualTestContext,
    expected: &str,
    mut ready: impl FnMut(&mut VisualTestContext) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        frame(visual);
        if ready(visual) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "Timed out waiting for {expected}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub(super) fn labels(visual: &mut VisualTestContext, editor: &Entity<EditorState>) -> Vec<String> {
    visual.update(|_, cx| {
        let menu = editor.read(cx).completion_menu_state();
        if menu.open {
            menu.items.iter().map(|item| item.label.clone()).collect()
        } else {
            Vec::new()
        }
    })
}

#[test]
fn workspace_completion_triggers_filters_applies_and_drops_stale_replies() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let script = fixture.path().join("server.py");
    fs::write(
        &script,
        include_str!("../../../tests/fixtures/lsp_completion.py"),
    )?;
    let command = format!("python3 -u {}", script.display());
    let source = "void run() {\n  /* 😀 */ object;\n}\n";
    let caret = source.find(';').unwrap();
    with_editor(fixture.path(), &command, source, caret, |visual, editor| {
        visual.simulate_keystrokes("i d");
        wait(visual, "debounced identifier", |visual| {
            labels(visual, editor) == ["objectIdentifier"]
        })?;
        visual.simulate_keystrokes("escape");
        visual.update(|window, cx| {
            editor.update(cx, |state, cx| {
                state.set_value(source, window, cx);
                state.set_selected_range(caret..caret, cx);
            })
        });
        frame(visual);
        visual.simulate_keystrokes("ctrl-space");
        wait(visual, "manual invocation", |visual| {
            labels(visual, editor) == ["objectValue"]
                && visual.debug_bounds("completion-menu").is_some()
        })?;
        visual.simulate_keystrokes("enter");
        frame(visual);
        visual.update(|_, cx| {
            let state = editor.read(cx);
            assert_eq!(
                state.text().to_string(),
                source.replace("object", "objectValue"),
                "{:?}",
                state.completion_menu_state()
            );
        });
        visual.update(|window, cx| {
            editor.update(cx, |state, cx| {
                state.replay_text_history(true, window, cx);
                assert_eq!(state.selected_range(), caret..caret);
            })
        });

        visual.simulate_keystrokes(".");
        wait(visual, "trigger character result", |visual| {
            labels(visual, editor) == ["member", "method"]
        })?;
        visual.simulate_keystrokes("m e");
        frame(visual);
        assert!(visual.debug_bounds("completion-menu").is_some());
        visual.simulate_keystrokes("tab enter");
        visual.update(|_, cx| {
            assert_eq!(
                editor.read(cx).text().to_string(),
                source.replace("object", "object.me")
            );
        });
        wait(visual, "incomplete result refresh", |visual| {
            labels(visual, editor) == ["Field via LSP", "method"]
        })?;
        visual.simulate_keystrokes("m");
        wait(visual, "complete result narrowed", |visual| {
            labels(visual, editor) == ["Field via LSP"]
        })?;
        let requests = fs::read_to_string(fixture.path().join("requests"))?;
        assert_eq!(requests.lines().count(), 4, "{requests}");

        let typed = source.replace("object", "object.mem");
        let completed = format!(
            "#include <widget.h>\n{}",
            source.replace("object", "object.member")
        );
        visual.simulate_keystrokes("tab");
        wait(visual, "Tab completion and auto-import", |visual| {
            visual.update(|_, cx| editor.read(cx).text().to_string() == completed)
        })?;
        assert!(labels(visual, editor).is_empty());
        visual.update(|window, cx| {
            editor.update(cx, |state, cx| {
                let at = state.text().to_string().find(';').unwrap();
                assert_eq!(state.selected_range(), at..at);
                state.replay_text_history(true, window, cx);
                assert_eq!(state.text().to_string(), typed);
                assert_eq!(state.selected_range(), caret + 4..caret + 4);
                state.replay_text_history(false, window, cx);
                assert_eq!(state.text().to_string(), completed);
                assert_eq!(state.selected_range(), at..at);
            })
        });

        visual.simulate_keystrokes("ctrl-space");
        wait(visual, "in-flight request", |_| {
            fixture.path().join("awaiting-release").exists()
        })?;
        visual.simulate_keystrokes("escape");
        visual.update(|_, cx| editor.update(cx, |state, cx| state.set_selected_range(0..0, cx)));
        fs::write(fixture.path().join("release"), "")?;
        for _ in 0..12 {
            frame(visual);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(labels(visual, editor).is_empty());
        visual.simulate_keystrokes("tab");
        frame(visual);
        visual.update(|_, cx| {
            let editor = editor.read(cx);
            let inserted = editor.cursor();
            assert!(inserted > 0, "Tab with no menu should indent");
            let text = editor.text().to_string();
            assert!(text[..inserted].chars().all(|ch| matches!(ch, ' ' | '\t')));
            assert_eq!(&text[inserted..], completed);
        });
        Ok(())
    })
}

#[test]
fn workspace_completion_keeps_the_selected_candidate_across_a_refresh() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let script = fixture.path().join("server.py");
    fs::write(
        &script,
        include_str!("../../../tests/fixtures/lsp_completion.py"),
    )?;
    let command = format!("python3 -u {}", script.display());
    let source = "void run() {\n  /* 😀 */ object;\n}\n";
    with_editor(
        fixture.path(),
        &command,
        source,
        source.find(';').unwrap(),
        |visual, editor| {
            visual.simulate_keystrokes(".");
            wait(visual, "initial candidates", |visual| {
                labels(visual, editor) == ["member", "method"]
            })?;
            visual.simulate_keystrokes("down m e");
            wait(visual, "refreshed candidates", |visual| {
                labels(visual, editor) == ["Field via LSP", "method"]
            })?;
            visual.simulate_keystrokes("tab");
            wait(visual, "the selected method is accepted", |visual| {
                visual.update(|_, cx| {
                    editor.read(cx).text().to_string() == source.replace("object", "object.method")
                })
            })
        },
    )
}

#[test]
#[ignore = "requires clangd on PATH"]
fn workspace_completion_with_clangd() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let source = "struct Widget { int member; };\nvoid run(Widget w) { w; }\n";
    let caret = source.rfind(';').unwrap();
    with_editor(fixture.path(), "clangd", source, caret, |visual, editor| {
        visual.simulate_keystrokes(".");
        wait(visual, "clangd completion", |visual| {
            labels(visual, editor)
                .iter()
                .any(|label| label.trim() == "member")
        })?;
        for key in ["m", "e", "m"] {
            visual.simulate_keystrokes(key);
            frame(visual);
        }
        wait(visual, "filtered clangd completion", |visual| {
            labels(visual, editor)
                .first()
                .is_some_and(|label| label.trim() == "member")
        })?;
        visual.simulate_keystrokes("enter");
        wait(visual, "clangd completion insertion", |visual| {
            visual.update(|_, cx| {
                editor.read(cx).text().to_string() == source.replace("{ w;", "{ w.member;")
            })
        })?;
        Ok(())
    })
}

#[test]
fn workspace_caret_hints_use_the_latest_symbol_and_signature() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let script = fixture.path().join("server.py");
    fs::write(
        &script,
        include_str!("../../../tests/fixtures/lsp_caret_hints.py"),
    )?;
    let command = format!("python3 -u {}", script.display());
    let source = "/* 😀 */ value;\nvalue = 0;\ninvoke\n";
    let files = fixture.path();
    with_editor(
        files,
        &command,
        source,
        source.find("value").unwrap() + 1,
        |visual, editor| {
            wait(visual, "UTF-16 document highlight request", |_| {
                files.join("highlight-requested").exists()
            })?;
            visual.update(|_, cx| {
                editor.update(cx, |state, cx| {
                    let caret = source.rfind("invoke").unwrap() + "invoke".len();
                    state.set_selected_range(caret..caret, cx);
                });
            });
            visual.simulate_input("(");
            wait(visual, "signature popup from open parenthesis", |visual| {
                files.join("triggered").exists()
                    && visual.debug_bounds("editor-signature-help").is_some()
            })?;
            let bounds = visual.debug_bounds("editor-signature-help").unwrap();
            assert!(bounds.left() >= px(0.) && bounds.top() >= px(0.));
            visual.simulate_keystrokes("ctrl-space");
            wait(visual, "completion while signature is active", |visual| {
                labels(visual, editor) == ["candidate"]
                    && visual.debug_bounds("editor-signature-help").is_none()
            })?;
            visual.simulate_keystrokes("escape");
            wait(visual, "signature restored after completion", |visual| {
                visual.debug_bounds("editor-signature-help").is_some()
            })?;
            visual.update(|window, cx| {
                editor.update(cx, |state, cx| {
                    state.open_code_actions(state.selected_range(), window, cx);
                })
            });
            wait(visual, "code actions obscure the signature", |visual| {
                visual.update(|_, cx| {
                    let menu = editor.read(cx).code_action_menu_state();
                    menu.open && !menu.loading && menu.items.len() == 1
                }) && visual.debug_bounds("editor-signature-help").is_none()
            })?;
            visual.simulate_keystrokes("escape");
            wait(
                visual,
                "Escape dismisses the visible code action menu",
                |visual| {
                    visual.update(|_, cx| !editor.read(cx).code_action_menu_state().open)
                        && visual.debug_bounds("editor-signature-help").is_some()
                },
            )?;
            visual.simulate_keystrokes("escape");
            frame(visual);
            assert!(visual.debug_bounds("editor-signature-help").is_none());
            visual.simulate_keystrokes("ctrl-shift-space");
            wait(visual, "explicit signature request", |visual| {
                files.join("invoked").exists()
                    && visual.debug_bounds("editor-signature-help").is_some()
            })?;
            visual.simulate_input("1");
            visual.simulate_input(",");
            wait(visual, "delayed comma response", |_| {
                files.join("delayed").exists()
            })?;
            visual.update(|_, cx| {
                editor.update(cx, |state, cx| state.set_selected_range(0..0, cx));
            });
            fs::write(files.join("release"), "")?;
            wait(visual, "stale signature discarded", |visual| {
                files.join("replied").exists()
                    && visual.debug_bounds("editor-signature-help").is_none()
            })?;
            for _ in 0..8 {
                frame(visual);
                assert!(visual.debug_bounds("editor-signature-help").is_none());
            }
            Ok(())
        },
    )
}
