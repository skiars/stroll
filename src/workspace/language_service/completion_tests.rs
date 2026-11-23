use std::{
    cell::RefCell,
    fs,
    path::Path,
    process::Command,
    rc::Rc,
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
    with_open_workspace(
        fixture,
        config,
        Location {
            root,
            file: Some(file.into()),
        },
        |visual, workspace| {
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
            body(visual, workspace, &editor)
        },
    )
}

fn with_open_workspace(
    fixture: &Path,
    config: ConfigFile,
    location: Location,
    body: impl FnOnce(&mut VisualTestContext, &Entity<Workspace>) -> Result<()>,
) -> Result<()> {
    let store = Arc::new(Store::new(fixture.join("state"))?);
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        crate::test_support::text_system(true),
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
            workspace.open_repository(location, window, cx);
        })
    });
    body(visual, &workspace)
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
fn workspace_completion_requeries_deleted_members_in_language_packs() -> Result<()> {
    use crate::{repository::Repository, session::WorktreeSession};
    use serde_json::json;

    let fixture = tempfile::tempdir()?;
    let script = fixture.path().join("server.py");
    fs::write(
        &script,
        include_str!("../../../tests/fixtures/lsp_completion.py"),
    )?;
    let root = fixture.path().join("repository");
    fs::create_dir(&root)?;
    ensure!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()?
            .success()
    );
    let extension = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/textmate");
    fs::write(
        root.join(".stroll.json"),
        serde_json::to_vec(&json!({
            "languageExtensionPaths": [extension],
            "lsp": [{"id": "example", "languages": {".example": "example"},
                "server": {"command": "python3", "args": ["-u", script]}}],
        }))?,
    )?;
    let store = Store::new(fixture.path().join("state"))?;
    store.save_session(
        &Repository::open(&root)?,
        &WorktreeSession {
            worktree_lsp_trusted: true,
            ..Default::default()
        },
    )?;

    let source = "let value = sub.fragments.items()\n";
    let caret = source.find(".items").unwrap();
    with_source_editor(
        fixture.path(),
        "main.example",
        "",
        source,
        caret,
        |visual, editor| {
            visual
                .simulate_keystrokes("backspace backspace backspace backspace backspace backspace");
            wait(visual, "shortened member", |visual| {
                labels(visual, editor) == ["fragments"]
            })?;
            visual.simulate_keystrokes("escape g");
            wait(visual, "language-pack identifier", |visual| {
                labels(visual, editor) == ["fragments"]
            })?;
            visual.simulate_keystrokes("backspace backspace backspace backspace");
            wait(visual, "empty member", |visual| {
                labels(visual, editor) == ["fragments"]
            })?;
            let requests = fs::read_to_string(fixture.path().join("requests"))?;
            assert_eq!(
                requests.lines().collect::<Vec<_>>(),
                [
                    "1 let value = sub.fra",
                    "1 let value = sub.frag",
                    "1 let value = sub.",
                ]
            );
            visual.simulate_keystrokes("tab");
            wait(visual, "restored member", |visual| {
                visual.update(|_, cx| editor.read(cx).text().to_string() == source)
            })?;
            Ok(())
        },
    )
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
#[ignore = "requires pylsp with Pyflakes on PATH"]
fn workspace_python_with_pylsp() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let package = fixture.path().join("repository/packages/app");
    fs::create_dir_all(&package)?;
    fs::write(package.join("pyproject.toml"), "[project]\nname = 'app'\n")?;
    fs::write(
        package.join("helper.py"),
        "def greeting():\n    \"\"\"A greeting from the project.\"\"\"\n    return 'hello'\n",
    )?;
    let source = "from helper import greet\nprint(greeting())\nunknown_name\nfrom pyflakes.api import checkP\n";
    let caret = source.find('\n').unwrap();
    with_workspace_editor(
        fixture.path(),
        "packages/app/main.py",
        "",
        source,
        caret,
        |visual, workspace, editor| {
            visual.update(|_, cx| {
                let file = workspace.read(cx).active_file().unwrap();
                assert_eq!(file.server_project.as_ref().unwrap().key.root, package);
            });
            wait(visual, "Pyflakes diagnostic", |visual| {
                visual.update(|_, cx| {
                    editor.read(cx).diagnostics().is_some_and(|diagnostics| {
                        diagnostics
                            .iter()
                            .any(|item| item.message.contains("unknown_name"))
                    })
                })
            })?;

            visual.simulate_keystrokes("ctrl-space");
            wait(visual, "Python import completion", |visual| {
                labels(visual, editor)
                    .first()
                    .is_some_and(|label| label.starts_with("greeting"))
            })?;
            visual.simulate_keystrokes("enter");
            wait(visual, "Python completion insertion", |visual| {
                visual.update(|_, cx| {
                    editor
                        .read(cx)
                        .text()
                        .to_string()
                        .starts_with("from helper import greeting\n")
                })
            })?;

            visual.update(|_, cx| {
                editor.update(cx, |state, cx| {
                    let end = state.text().to_string().rfind('\n').unwrap();
                    state.set_selected_range(end..end, cx);
                })
            });
            visual.simulate_keystrokes("ctrl-space");
            wait(visual, "completion from the pylsp environment", |visual| {
                labels(visual, editor)
                    .iter()
                    .any(|label| label.starts_with("checkPath"))
            })?;
            visual.simulate_keystrokes("escape");

            let hover_result = Rc::new(RefCell::new(None));
            visual.update(|window, cx| {
                let state = editor.read(cx);
                let text = state.text().clone();
                let offset = text.to_string().rfind("greeting").unwrap() + 1;
                let provider = state.lsp().hover_provider.clone().unwrap();
                let hover = provider.hover(&text, offset, window, cx);
                let result = hover_result.clone();
                cx.spawn(async move |_| {
                    *result.borrow_mut() = Some(hover.await);
                })
                .detach();
                editor.update(cx, |state, cx| state.set_selected_range(offset..offset, cx));
            });
            wait(visual, "Python hover", |_| hover_result.borrow().is_some())?;
            let hover = hover_result.borrow_mut().take().unwrap()?.unwrap();
            let lsp_types::HoverContents::Markup(contents) = hover.contents else {
                anyhow::bail!("Expected Python hover documentation");
            };
            assert!(contents.value.contains("A greeting from the project."));

            visual.simulate_keystrokes("f12");
            wait(visual, "Python cross-file definition", |visual| {
                visual.update(|_, cx| {
                    workspace
                        .read(cx)
                        .active_file()
                        .is_some_and(|file| file.path == Path::new("packages/app/helper.py"))
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
fn hover_source_links_use_workspace_navigation() -> Result<()> {
    use gpui::point;

    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("repository");
    fs::create_dir(&root)?;
    let script = fixture.path().join("server.py");
    fs::write(
        &script,
        include_str!("../../../tests/fixtures/lsp_hover_links.py"),
    )?;
    let header = fixture.path().join("external # header.hpp");
    let declaration = "namespace sample {\n/* 😀 */ struct External {};\n}\n";
    fs::write(&header, declaration)?;
    let header = header.canonicalize()?;
    let source = "struct Local {};\n\nvoid inspect() {}\n";
    let caret = source.find("inspect").unwrap();
    let configuration = format!(
        "[lsp.clangd]\ncommand = 'python3 -u {}'\n",
        script.display()
    );
    with_workspace_editor(
        fixture.path(),
        "main.cpp",
        &configuration,
        source,
        caret,
        |visual, workspace, editor| {
            let hover_link = |visual: &mut VisualTestContext, url: &str| -> Result<_> {
                fs::write(
                    script.with_extension("md"),
                    format!("[Definition](<{url}>)"),
                )?;
                let pointer = visual.update(|_, cx| {
                    editor
                        .read(cx)
                        .range_to_bounds(&(caret..caret + "inspect".len()))
                        .unwrap()
                        .center()
                });
                visual.simulate_mouse_move(point(px(2.), px(2.)), None, Default::default());
                visual.simulate_mouse_move(pointer, None, Default::default());
                wait(visual, "hover link", |visual| {
                    visual.debug_bounds("hover-markdown").is_some()
                        && visual.update(|_, cx| {
                            matches!(
                                editor.read(cx).hover_popover().and_then(|popover| popover.hover.as_ref()),
                                Some(lsp_types::Hover { contents: lsp_types::HoverContents::Markup(markup), .. })
                                    if markup.value.contains(url)
                            )
                        })
                })?;
                let link =
                    visual.debug_bounds("hover-markdown").unwrap().origin + point(px(17.), px(17.));
                visual.simulate_mouse_move(link, None, Default::default());
                frame(visual);
                visual.simulate_click(link, Default::default());
                frame(visual);
                Ok(())
            };
            let main = Path::new("main.cpp");
            let local_uri = url::Url::from_file_path(root.join(main)).unwrap();
            let header_uri = url::Url::from_file_path(&header).unwrap();
            for (url, path, offset) in [
                (
                    format!("{local_uri}#L1,8"),
                    main,
                    source.find("Local").unwrap(),
                ),
                (
                    format!("{header_uri}#L2,17"),
                    header.as_path(),
                    declaration.find("External").unwrap(),
                ),
                (
                    format!("{header_uri}#L3"),
                    header.as_path(),
                    declaration.rfind('}').unwrap(),
                ),
            ] {
                hover_link(visual, &url)?;
                wait(visual, "linked source position", |visual| {
                    visual.update(|_, cx| {
                        workspace.read(cx).active_file().is_some_and(|file| {
                            file.path == path && file.editor.read(cx).cursor() == offset
                        })
                    })
                })?;
                assert!(visual.opened_url().is_none());
                visual.update(|window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        workspace.source_back(
                            &super::super::source_history::SourceBack,
                            window,
                            cx,
                        );
                    });
                });
                wait(visual, "return to hover source", |visual| {
                    visual.update(|_, cx| {
                        workspace.read(cx).active_file().is_some_and(|file| {
                            file.path == main && file.editor.read(cx).cursor() == caret
                        })
                    })
                })?;
            }

            hover_link(visual, "https://example.com/reference")?;
            assert_eq!(
                visual.opened_url().as_deref(),
                Some("https://example.com/reference")
            );
            Ok(())
        },
    )
}

#[test]
#[ignore = "requires clangd on PATH"]
fn workspace_definition_opens_header_outside_worktree_with_clangd() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let include = fixture.path().join("include");
    fs::create_dir(&include)?;
    let header = include.join("shared header.hpp");
    let declaration = "namespace sample {\ninline int external_value() { return 42; }\n}\n";
    fs::write(&header, declaration)?;
    let header = header.canonicalize()?;
    let source = "#include \"../include/shared header.hpp\"\nint main() { return sample::external_value(); }\n";
    let caret = source.find("external_value").unwrap() + 1;
    with_workspace_editor(
        fixture.path(),
        "main.cpp",
        "[lsp.clangd]\ncommand = 'clangd'\n",
        source,
        caret,
        |visual, workspace, _| {
            visual.simulate_keystrokes("f12");
            let definition = declaration.find("external_value").unwrap();
            wait(visual, "definition in an external header", |visual| {
                visual.update(|_, cx| {
                    workspace.read(cx).active_file().is_some_and(|file| {
                        file.path == header && file.editor.read(cx).cursor() == definition
                    })
                })
            })?;

            visual.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.source_back(&super::super::source_history::SourceBack, window, cx);
                });
            });
            wait(visual, "source caret after navigating back", |visual| {
                visual.update(|_, cx| {
                    workspace.read(cx).active_file().is_some_and(|file| {
                        file.path == Path::new("main.cpp") && file.editor.read(cx).cursor() == caret
                    })
                })
            })
        },
    )
}

#[test]
fn editor_annotations_handle_delayed_out_of_order_and_empty_results() -> Result<()> {
    use serde_json::{Value, json};

    let fixture = tempfile::tempdir()?;
    let root = fixture.path();
    let script = root.join("server.py");
    fs::write(
        &script,
        include_str!("../../../tests/fixtures/lsp_annotation_updates.py"),
    )?;
    let source = "int broken = unknown;\nint apple = apple;\nint pear = pear;\n";
    let error_start = source.find("unknown").unwrap();
    let configuration = format!(
        "[lsp.clangd]\ncommand = 'python3 -u {}'\n",
        script.display()
    );
    let mut response_number = 0;
    let mut send = |message: Value| -> Result<_> {
        response_number += 1;
        let dest = root.join(format!("reply-{response_number:02}.json"));
        let staging = dest.with_extension("tmp");
        fs::write(&staging, serde_json::to_vec(&message)?)?;
        fs::rename(staging, &dest)?;
        Ok(dest)
    };
    with_workspace_editor(
        root,
        "main.cpp",
        &configuration,
        source,
        source.find("apple").unwrap() + 1,
        |visual, workspace, editor| {
            let receive = |visual: &mut VisualTestContext,
                           name: &str,
                           check: &dyn Fn(&mut VisualTestContext)|
             -> Result<Value> {
                let incoming = root.join(format!("{name}.json"));
                wait(visual, name, |visual| {
                    check(visual);
                    incoming.exists()
                })?;
                Ok(serde_json::from_slice(&fs::read(incoming)?)?)
            };
            let open = receive(visual, "open-1", &|_| {})?;
            let uri = &open["params"]["textDocument"]["uri"];
            let publish = |version: &Value, message: Option<(&str, usize)>| {
                let diagnostics = message
                    .map(|(message, end)| {
                        json!({
                            "range": {
                                "start": {"line": 0, "character": error_start},
                                "end": {"line": 0, "character": end},
                            },
                            "severity": 1,
                            "message": message,
                        })
                    })
                    .into_iter()
                    .collect::<Vec<_>>();
                json!({
                    "method": "textDocument/publishDiagnostics",
                    "params": {"uri": uri, "version": version, "diagnostics": diagnostics},
                })
            };
            let highlight = |request: &Value, word: &str| {
                let (line, text) = source
                    .lines()
                    .enumerate()
                    .find(|(_, line)| line.contains(word))
                    .unwrap();
                assert_eq!(request["params"]["position"]["line"], line);
                let result = text
                    .match_indices(word)
                    .map(|(start, _)| {
                        json!({
                            "range": {
                                "start": {"line": line, "character": start},
                                "end": {"line": line, "character": start + word.len()},
                            }
                        })
                    })
                    .collect::<Vec<_>>();
                json!({"id": request["id"], "result": result})
            };
            let shown_word = |visual: &mut VisualTestContext, word: &str| {
                visual.update(|_, cx| {
                    let file = workspace.read(cx).active_file().unwrap();
                    let ranges = file.caret_hints.highlight_ranges(cx);
                    ranges.len() == 2
                        && ranges
                            .into_iter()
                            .all(|range| editor.read(cx).text().slice(range) == word)
                })
            };
            let shown_diagnostic = |visual: &mut VisualTestContext, message: &str| {
                visual.update(|window, cx| {
                    let editor = editor.read(cx);
                    editor
                        .diagnostic_popover()
                        .is_some_and(|entry| entry.message == message)
                        && editor
                            .diagnostics()
                            .unwrap()
                            .iter()
                            .any(|entry| entry.message == message)
                        && !window.painted_underlines().is_empty()
                }) && visual.debug_bounds("diagnostic-quick-fix-button").is_some()
            };
            let edit_before_caret = |visual: &mut VisualTestContext,
                                     range: std::ops::Range<usize>,
                                     replacement: &str| {
                visual.update(|window, cx| {
                    editor.update(cx, |editor, cx| {
                        let caret = editor.cursor() - range.len() + replacement.len();
                        editor.edit_ranges(&[(range, replacement.into())], window, cx);
                        editor.set_selected_range(caret..caret, cx);
                    });
                });
            };

            send(publish(
                &open["params"]["textDocument"]["version"],
                Some(("initial", error_start + 7)),
            ))?;
            let apple = receive(visual, "highlight-1", &|_| {})?;
            send(highlight(&apple, "apple"))?;
            wait(
                visual,
                "initial diagnostic and apple highlights",
                |visual| {
                    shown_word(visual, "apple")
                        && visual.update(|_, cx| !editor.read(cx).diagnostics().unwrap().is_empty())
                },
            )?;
            let pointer = visual.update(|_, cx| {
                editor
                    .read(cx)
                    .range_to_bounds(&(error_start..error_start + 7))
                    .unwrap()
                    .center()
            });
            visual.simulate_mouse_move(pointer, None, Default::default());
            wait(visual, "initial diagnostic popup", |visual| {
                shown_diagnostic(visual, "initial")
            })?;

            let apple_and_initial = |visual: &mut VisualTestContext| {
                assert!(shown_word(visual, "apple"));
                assert!(shown_diagnostic(visual, "initial"));
            };
            edit_before_caret(visual, error_start + 6..error_start + 7, "");
            let change = receive(visual, "change-1", &apple_and_initial)?;
            let older = &change["params"]["textDocument"]["version"];
            let first = receive(visual, "highlight-2", &apple_and_initial)?;
            send(json!({"id": first["id"], "result": null}))?;
            let confirmation = receive(visual, "highlight-3", &apple_and_initial)?;
            send(highlight(&confirmation, "apple"))?;
            for _ in 0..4 {
                frame(visual);
                apple_and_initial(visual);
            }

            edit_before_caret(visual, error_start + 6..error_start + 6, "n");
            let change = receive(visual, "change-2", &apple_and_initial)?;
            let latest = &change["params"]["textDocument"]["version"];
            let late = receive(visual, "highlight-4", &apple_and_initial)?;
            send(publish(latest, Some(("current", error_start + 7))))?;
            wait(visual, "updated diagnostic in the same popup", |visual| {
                assert!(shown_word(visual, "apple"));
                assert!(shown_diagnostic(visual, "initial") || shown_diagnostic(visual, "current"));
                shown_diagnostic(visual, "current")
            })?;

            visual.update(|_, cx| {
                editor.update(cx, |editor, cx| {
                    let caret = source.find("pear").unwrap() + 1;
                    editor.set_selected_range(caret..caret, cx);
                });
            });
            let assert_no_highlights = |visual: &mut VisualTestContext| {
                visual.update(|_, cx| {
                    let file = workspace.read(cx).active_file().unwrap();
                    assert!(file.caret_hints.highlight_ranges(cx).is_empty());
                });
            };
            frame(visual);
            assert_no_highlights(visual);
            let pear = receive(visual, "highlight-5", &|visual| {
                assert_no_highlights(visual);
                assert!(shown_diagnostic(visual, "current"));
            })?;
            send(highlight(&pear, "pear"))?;
            wait(visual, "latest symbol", |visual| shown_word(visual, "pear"))?;
            send(highlight(&late, "apple"))?;
            let stale = send(publish(older, None))?;
            let pear_and_current = |visual: &mut VisualTestContext| {
                assert!(shown_word(visual, "pear"));
                assert!(shown_diagnostic(visual, "current"));
            };
            wait(
                visual,
                "older responses sent after the latest results",
                |visual| {
                    pear_and_current(visual);
                    !stale.exists()
                },
            )?;
            for _ in 0..4 {
                frame(visual);
                pear_and_current(visual);
            }

            edit_before_caret(visual, error_start..error_start + 1, "");
            let change = receive(visual, "change-3", &pear_and_current)?;
            let empty = receive(visual, "highlight-6", &pear_and_current)?;
            send(json!({"id": empty["id"], "result": []}))?;
            let confirmation = receive(visual, "highlight-7", &pear_and_current)?;
            send(json!({"id": confirmation["id"], "result": null}))?;
            send(publish(&change["params"]["textDocument"]["version"], None))?;
            wait(
                visual,
                "confirmed empty results clear the decorations and popup",
                |visual| {
                    visual.update(|_, cx| {
                        let file = workspace.read(cx).active_file().unwrap();
                        let editor = editor.read(cx);
                        file.caret_hints.highlight_ranges(cx).is_empty()
                            && editor.diagnostics().unwrap().is_empty()
                            && editor.diagnostic_popover().is_none()
                    })
                },
            )?;
            Ok(())
        },
    )
}

#[test]
#[ignore = "requires clangd on PATH"]
fn local_cpp_diagnostics_and_highlights_stay_visible_during_edits() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().canonicalize()?;
    let source = concat!(
        "int broken = unknown;\n",
        "int malformed = ;\n",
        "int apple = 1;\n",
        "int pear = 2;\n",
        "int run() { return apple + apple + pear + pear; }\n",
    );
    let local_file = root.join("standalone.cpp");
    fs::write(&local_file, source)?;
    let configuration = "[lsp.clangd]\ncommand = 'clangd'\n";

    with_workspace_editor(
        &root,
        "repository.cpp",
        configuration,
        "int repository_value;\n",
        0,
        |visual, workspace, _| {
            let position = visual.debug_bounds("file-drop-content").unwrap().center();
            visual.simulate_event(gpui::FileDropEvent::Entered {
                position,
                paths: gpui::ExternalPaths([local_file.clone()].into_iter().collect()),
            });
            visual.simulate_event(gpui::FileDropEvent::Submit { position });
            exercise_local_cpp(visual, workspace, &local_file, source)
        },
    )?;

    let store = Arc::new(Store::new(root.join("standalone-state"))?);
    let config = ConfigFile::load(Some(root.join("config.toml")));
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        crate::test_support::text_system(true),
    );
    app.update(gpui_component::init);
    app.update(|cx| Workspace::open_standalone(store, config, local_file.clone(), None, cx))?;
    let handle = app.windows()[0];
    let mut visual = VisualTestContext::from_window(handle, &app);
    visual.simulate_resize(size(px(940.), px(580.)));
    let workspace = app.update_window(handle, |root, _, cx| {
        root.downcast::<Root>()
            .unwrap()
            .read(cx)
            .view()
            .clone()
            .downcast::<Workspace>()
            .unwrap()
    })?;
    visual.update(|_, cx| assert!(workspace.read(cx).repository.is_none()));
    exercise_local_cpp(&mut visual, &workspace, &local_file, source)
}

fn exercise_local_cpp(
    visual: &mut VisualTestContext,
    workspace: &Entity<Workspace>,
    path: &Path,
    source: &str,
) -> Result<()> {
    let initial_diagnostics = wait(visual, "diagnostics for the local C++ file", |visual| {
        visual.update(|_, cx| {
            workspace.read(cx).active_file().is_some_and(|file| {
                file.path == path
                    && file.server_project.as_ref().is_some_and(|project| {
                        !project.key.is_remote()
                            && project.key.project_path() == path.parent().unwrap()
                    })
                    && file.editor.read(cx).diagnostics().is_some_and(|set| {
                        set.iter().any(|entry| entry.message.contains("unknown"))
                            && set.iter().any(|entry| {
                                entry
                                    .message
                                    .to_ascii_lowercase()
                                    .contains("expected expression")
                            })
                    })
            })
        })
    });
    if initial_diagnostics.is_err() {
        visual.update(|_, cx| {
            let state = workspace.read(cx);
            let active = state.active_file();
            eprintln!(
                "Expected {path:?}, active file: {:?}",
                active.map(|file| &file.path)
            );
            if let Some(file) = active {
                eprintln!("Has a language server: {}", file.language_server.is_some());
                eprintln!(
                    "Actual diagnostics: {:?}",
                    file.editor
                        .read(cx)
                        .diagnostics()
                        .into_iter()
                        .flat_map(|set| set.iter())
                        .map(|entry| entry.message.as_str())
                        .collect::<Vec<_>>()
                );
            }
        });
        initial_diagnostics?;
    }
    let editor = visual.update(|window, cx| {
        let file = workspace.read(cx).active_file().unwrap();
        let editor = file.editor.clone();
        editor.update(cx, |editor, cx| {
            editor.focus(window, cx);
        });
        editor
    });
    let highlights = |visual: &mut VisualTestContext, word: &str| {
        visual.update(|_, cx| {
            let file = workspace.read(cx).active_file().unwrap();
            let ranges = file.caret_hints.highlight_ranges(cx);
            ranges.len() == 3
                && ranges
                    .into_iter()
                    .all(|range| editor.read(cx).text().slice(range) == word)
        })
    };
    let error_offset = source.find("unknown").unwrap();
    let operator = source.find("apple + apple").unwrap() + "apple ".len();
    let apple_caret = operator + "+ a".len();
    let pear_caret = source.rfind("pear").unwrap() + 1;
    let pointer = visual.update(|_, cx| {
        editor
            .read(cx)
            .range_to_bounds(&(error_offset..error_offset + "unknown".len()))
            .unwrap()
            .center()
    });
    let has_error_popup = |visual: &mut VisualTestContext| {
        visual.update(|_, cx| {
            let editor = editor.read(cx);
            let is_unresolved = |entry: &gpui_component::highlighter::DiagnosticEntry| {
                editor
                    .text()
                    .slice(entry.range.clone())
                    .to_string()
                    .starts_with("unknow")
                    && entry.message.contains("undeclared identifier")
            };
            editor
                .diagnostic_popover()
                .as_deref()
                .is_some_and(is_unresolved)
                && editor.diagnostics().unwrap().iter().any(is_unresolved)
        }) && visual.debug_bounds("diagnostic-quick-fix-button").is_some()
    };
    let select = |visual: &mut VisualTestContext, caret| {
        visual.update(|_, cx| {
            editor.update(cx, |editor, cx| editor.set_selected_range(caret..caret, cx));
        });
    };
    let edit_with_caret = |visual: &mut VisualTestContext,
                           range: std::ops::Range<usize>,
                           replacement: &str,
                           caret: usize| {
        visual.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.edit_ranges(&[(range, replacement.into())], window, cx);
                editor.set_selected_range(caret..caret, cx);
            });
        });
    };

    for interval in [30, 70, 130, 220].map(Duration::from_millis) {
        if interval != Duration::from_millis(30) {
            let len = visual.update(|_, cx| editor.read(cx).text().len());
            edit_with_caret(visual, 0..len, source, error_offset + "unknown".len());
            wait(
                visual,
                "restored diagnostics before a new input cadence",
                |visual| visual.update(|_, cx| editor.read(cx).diagnostics().unwrap().len() >= 2),
            )?;
        }
        select(visual, error_offset + "unknown".len());
        // Move away first so the same pointer position can open the next round's popup.
        visual.simulate_mouse_move(gpui::point(px(2.), px(2.)), None, Default::default());
        visual.simulate_mouse_move(pointer, None, Default::default());
        wait(
            visual,
            "open unresolved-identifier diagnostic popup",
            has_error_popup,
        )?;

        for _ in 0..3 {
            visual.simulate_keystrokes("backspace");
            visual.update(|_, cx| {
                assert_eq!(
                    editor.read(cx).text().slice(error_offset..error_offset + 7),
                    "unknow;"
                );
            });
            draw_through_interval(visual, interval, |visual| {
                assert!(
                    has_error_popup(visual),
                    "diagnostic or popup blinked at {interval:?} after Backspace"
                );
            });
            visual.simulate_input("n");
            visual.update(|_, cx| assert_eq!(editor.read(cx).text().to_string(), source));
            draw_through_interval(visual, interval, |visual| {
                assert!(
                    has_error_popup(visual),
                    "diagnostic or popup blinked at {interval:?} after restore"
                );
            });
        }
        select(visual, apple_caret);
        wait(
            visual,
            "three apple highlights before operator edits",
            |visual| highlights(visual, "apple"),
        )?;
        for _ in 0..3 {
            edit_with_caret(visual, operator..operator + 1, "", apple_caret - 1);
            draw_through_interval(visual, interval, |visual| {
                assert!(
                    highlights(visual, "apple"),
                    "apple highlights blinked at {interval:?} after deleting +"
                );
                assert!(
                    has_error_popup(visual),
                    "the unrelated popup blinked at {interval:?}"
                );
            });
            edit_with_caret(visual, operator..operator, "+", apple_caret);
            draw_through_interval(visual, interval, |visual| {
                assert!(
                    highlights(visual, "apple"),
                    "apple highlights blinked at {interval:?} after restoring +"
                );
                assert!(
                    has_error_popup(visual),
                    "the unrelated popup blinked at {interval:?}"
                );
            });
        }

        let repaired_caret = apple_caret - "unknown".len() + 2;
        visual.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                let semicolon = source.find("= ;").unwrap() + 2;
                editor.edit_ranges(
                    &[
                        (error_offset..error_offset + "unknown".len(), "0".into()),
                        (semicolon..semicolon, "0".into()),
                    ],
                    window,
                    cx,
                );
                editor.set_selected_range(repaired_caret..repaired_caret, cx);
            });
        });
        wait(visual, "clangd clears the repaired errors", |visual| {
            visual.update(|_, cx| editor.read(cx).diagnostics().unwrap().is_empty())
        })?;
        assert!(!has_error_popup(visual));

        select(visual, pear_caret - "unknown".len() + 2);
        frame(visual);
        assert!(
            !highlights(visual, "apple"),
            "old highlights remained after moving to pear"
        );
        wait(visual, "three pear highlights", |visual| {
            highlights(visual, "pear")
        })?;
        draw_through_interval(visual, interval, |visual| {
            assert!(highlights(visual, "pear") && !has_error_popup(visual));
        });
        eprintln!("Validated {interval:?} in {path:?}");
    }

    visual.dispatch_action(crate::workspace::FindReferences);
    frame(visual);
    assert!(visual.update(|_, cx| workspace.read(cx).locator.open));
    wait(visual, "jump from a local C++ reference", |visual| {
        if visual.update(|_, cx| workspace.read(cx).locator.open) {
            visual.simulate_keystrokes("enter");
        }
        visual.update(|_, cx| !workspace.read(cx).locator.open)
    })
}

fn draw_through_interval(
    visual: &mut VisualTestContext,
    interval: Duration,
    mut check: impl FnMut(&mut VisualTestContext),
) {
    let start = Instant::now();
    let mut elapsed = Duration::ZERO;
    loop {
        visual.run_until_parked();
        visual.update(|window, cx| {
            window.simulate_next_frame(cx);
            window.draw(cx).clear(cx);
        });
        check(visual);
        if elapsed == interval {
            break;
        }
        let next = (elapsed + Duration::from_millis(10)).min(interval);
        std::thread::sleep((start + next).saturating_duration_since(Instant::now()));
        visual.executor().advance_clock(next - elapsed);
        elapsed = next;
    }
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
            assert!(bounds.size.width < px(320.));
            assert!(bounds.right() <= px(932.));
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
            visual.simulate_keystrokes(if cfg!(target_os = "macos") {
                "cmd-shift-space"
            } else {
                "ctrl-shift-space"
            });
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

#[test]
#[ignore = "requires an installed Vue project and the npm tools described in docs/language-packs.md"]
fn workspace_routes_vue_and_typescript_to_the_same_session() -> Result<()> {
    use anyhow::Context as _;
    use futures_lite::future::block_on;
    use gpui_component::Rope;

    use crate::{language::LanguageServer, remote::SshTarget};

    let root = std::path::PathBuf::from(
        std::env::var_os("STROLL_VUE_TEST_PROJECT")
            .context("Set STROLL_VUE_TEST_PROJECT to an installed Vue test fixture")?,
    )
    .canonicalize()?;
    let mut locations = vec![root.clone()];
    if let Ok(host) = std::env::var("STROLL_VUE_TEST_SSH_HOST") {
        locations.push(format!("ssh://{host}{}", root.display()).into());
    }
    for location in locations {
        let remote = SshTarget::parse(&location)?.is_some();
        let fixture = tempfile::tempdir()?;
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        with_open_workspace(
            fixture.path(),
            config,
            Location {
                root: location.clone(),
                file: Some("App.vue".into()),
            },
            |visual, workspace| {
                for file in ["App.vue", "types.ts"] {
                    if file != "App.vue" {
                        visual.update(|window, cx| {
                            workspace.update(cx, |workspace, cx| {
                                workspace.open_file(file.into(), false, window, cx);
                            });
                        });
                    }
                    wait(visual, file, |visual| {
                        visual.update(|_, cx| {
                            let workspace = workspace.read(cx);
                            workspace.files.get(Path::new(file)).is_some_and(|file| {
                                file.language_server.is_some()
                                    && file.server_project.as_ref().is_some_and(|project| {
                                        matches!(workspace.language_servers.get(&project.key),
                                            Some(LanguageService::Session { client, .. })
                                            if client.state() == SessionState::Ready)
                                    })
                            })
                        })
                    })?;
                }
                let (vue, ts, vue_source, ts_source) = visual.update(|_, cx| {
                    let workspace = workspace.read(cx);
                    let vue = &workspace.files[Path::new("App.vue")];
                    let ts = &workspace.files[Path::new("types.ts")];
                    let vue_project = vue.server_project.as_ref().unwrap();
                    let ts_project = ts.server_project.as_ref().unwrap();
                    assert!(vue_project.key == ts_project.key);
                    assert_eq!(vue_project.key.server_id(), LanguageServer::Vue.name());
                    assert_eq!(vue_project.key.root, root);
                    assert_eq!(vue_project.key.remote, remote);
                    assert_eq!(vue_project.language_id, "vue");
                    assert_eq!(ts_project.language_id, "typescript");
                    (
                        vue.language_server.as_ref().unwrap().document.clone(),
                        ts.language_server.as_ref().unwrap().document.clone(),
                        vue.editor.read(cx).text().clone(),
                        ts.editor.read(cx).text().clone(),
                    )
                });
                let launcher = location.display();
                let hover_at = vue_source
                    .to_string()
                    .find("{{ formatted")
                    .context("template expression")?
                    + 4;
                let hover = block_on(vue.hover(vue_source.clone(), hover_at))?
                    .with_context(|| launcher.to_string())?;
                assert!(
                    serde_json::to_string(&hover)?.contains("formatted: string"),
                    "{launcher}: {hover:?}"
                );
                let to_ts = vue_source
                    .to_string()
                    .find("count.toFixed")
                    .context("TypeScript use")?
                    + 1;
                let links = block_on(vue.definitions(vue_source.clone(), to_ts))?;
                assert!(
                    links
                        .iter()
                        .any(|link| link.target_uri.as_str().ends_with("/types.ts")),
                    "{launcher}: {links:?}"
                );
                let to_vue = ts_source.to_string().find("= App").context("Vue use")? + 3;
                let links = block_on(ts.definitions(ts_source.clone(), to_vue))?;
                assert!(
                    links
                        .iter()
                        .any(|link| link.target_uri.as_str().ends_with("/App.vue")),
                    "{launcher}: {links:?}"
                );
                let edited =
                    Rope::from(vue_source.to_string().replace("count.toFixed(2)", "count"));
                let offset = edited.to_string().find("{{ formatted").unwrap() + 4;
                let hover =
                    block_on(vue.hover(edited, offset))?.context("hover after unsaved change")?;
                assert!(
                    serde_json::to_string(&hover)?.contains("formatted: 42"),
                    "{launcher}: {hover:?}"
                );
                Ok(())
            },
        )?;
    }
    Ok(())
}
