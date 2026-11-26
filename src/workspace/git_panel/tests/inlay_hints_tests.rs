use super::*;
use gpui_component::input::RopeExt as _;
use std::{cell::RefCell, time::Instant};

fn wait_for(
    visual: &mut VisualTestContext,
    description: &str,
    mut check: impl FnMut(&mut Window, &mut App) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        visual.executor().advance_clock(Duration::from_millis(24));
        visual.run_until_parked();
        if visual.update(|window, cx| {
            window.simulate_next_frame(cx);
            window.draw(cx).clear(cx);
            check(window, cx)
        }) {
            return;
        }
        assert!(Instant::now() < deadline, "Waiting for {description}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[::core::prelude::v1::test]
fn renders_inlay_hints_in_the_workspace_and_worktree_diff_layouts() {
    check_worktree_diff_inlay_hints(false);
}

#[::core::prelude::v1::test]
fn arbitrary_comparison_with_workspace_target_reuses_live_lsp() {
    check_worktree_diff_inlay_hints(true);
}

fn check_worktree_diff_inlay_hints(arbitrary: bool) {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-qb", "main"]);
    git(&root, &["config", "user.name", "Reviewer"]);
    git(&root, &["config", "user.email", "reviewer@example.invalid"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    let source = PathBuf::from("main.cpp");
    fs::write(root.join(&source), "// baseline anchor\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "Initial"]);
    fs::write(root.join(&source), "// staged anchor\n").unwrap();
    git(&root, &["add", "."]);
    let working_text = "// 😺 worktree anchor\n// stable anchor\n";
    fs::write(root.join(&source), working_text).unwrap();
    let config_path = fixture.path().join("config.toml");
    let server = fixture.path().join("server.py");
    fs::write(
        &server,
        include_str!("../../../../tests/fixtures/lsp_inlay_hints.py"),
    )
    .unwrap();
    let python = if cfg!(windows) { "python" } else { "python3" };
    fs::write(
        &config_path,
        format!(
            "[editor]\ninlay_hints = true\n[lsp.clangd]\ncommand = '{python} -u {}'\n",
            server.display()
        ),
    )
    .unwrap();
    let config = crate::config::ConfigFile::load(Some(config_path));
    let platform = gpui_platform::current_platform(true);
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        platform.text_system(),
    );
    app.update(gpui_component::init);
    let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
    let (workspace, visual) = app.add_window_view(|window, cx| {
        Workspace::with_config(store, config, WorkspaceView::Git, window, cx)
    });
    visual.simulate_resize(size(px(1050.), px(670.)));
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(
                Location {
                    root: root.clone(),
                    file: None,
                },
                window,
                cx,
            );
        });
    });
    wait_for(visual, "git history", |_, cx| {
        workspace
            .read(cx)
            .git
            .as_ref()
            .is_some_and(|panel| panel.read(cx).history.is_some())
    });
    let panel = visual.update(|window, cx| {
        let panel = workspace.read(cx).git.clone().unwrap();
        panel.update(cx, |panel, cx| {
            panel.show_history = false;
            let selection = Selection::Worktree(Mode::All);
            let selection = if arbitrary {
                Selection::Compare {
                    base: panel.history.as_ref().unwrap().head.clone(),
                    target: None,
                    previous: Box::new(selection),
                }
            } else {
                selection
            };
            panel.load_review(selection, false, window, cx);
            cx.notify();
        });
        panel
    });
    let stable_hint_width = |editor: &EditorState| {
        let source = editor.text().to_string();
        let start = source.find("// stable ")?;
        let anchor = start + "// stable ".len();
        let width = editor.range_to_bounds(&(start..anchor))?.size.width;
        let before = editor.range_to_bounds(&(start..anchor - 1))?.size.width;
        Some(width - before)
    };
    let check_diff = |split, window: &mut Window, cx: &mut App| {
        let panel = panel.read(cx);
        let mode = if split {
            DiffMode::Split
        } else {
            DiffMode::Unified
        };
        if panel.diff.read(cx).mode() != mode {
            return false;
        }
        let modified = panel.editor.focused_column(window, cx).read(cx);
        let Some(row) = modified
            .text()
            .iter_lines()
            .position(|line| line.to_string().contains("worktree anchor"))
        else {
            return false;
        };
        if modified.projected_wrap_rows(row) <= 1 {
            return false;
        }
        for (row, line) in modified.text().iter_lines().enumerate() {
            if !line.to_string().contains("worktree") {
                assert!(modified.projected_wrap_rows(row) <= 1);
            }
        }
        assert!(!modified.text().to_string().contains("long_type"));
        if split {
            let original = panel.editor.original_state().read(cx);
            assert!(original.projected_wrap_rows(row) <= 1);
            let Some((left, right)) = original
                .projected_line_bounds(row)
                .zip(modified.projected_line_bounds(row))
            else {
                return false;
            };
            return (left.top() - right.top()).abs() < px(0.5)
                && left.size.height == right.size.height;
        }
        true
    };
    wait_for(
        visual,
        "working tree hints in unified diff",
        |window, cx| check_diff(false, window, cx) && root.join("refresh-acknowledged").exists(),
    );
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.split = true;
            // Keep the projected modified editor as the focused column.
            panel.editor.focus(window, cx);
            cx.notify();
        });
    });
    wait_for(
        visual,
        "working tree hints and row alignment in split diff",
        |window, cx| check_diff(true, window, cx),
    );
    let projection = visual.update(|window, cx| {
        let editor = panel.read(cx).editor.focused_column(window, cx).clone();
        editor.update(cx, |editor, cx| {
            let text = editor.text().to_string();
            let offset = text.find("worktree anchor").unwrap();
            editor.apply_editor_edits(&[(offset..offset, "!".into())], false, window, cx);
        });
        editor
    });
    wait_for(visual, "unchanged hint after a projected edit", |_, cx| {
        let state = projection.read(cx);
        assert!(stable_hint_width(state).is_some_and(|width| width > px(18.)));
        state.text().to_string().contains("!worktree anchor")
    });
    visual.simulate_keystrokes(if cfg!(target_os = "macos") {
        "cmd-z"
    } else {
        "ctrl-z"
    });
    wait_for(visual, "projected undo", |_, cx| {
        workspace.read(cx).files[&source].committed.to_string() == working_text
    });
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.switch_view(WorkspaceView::Editor, window, cx);
            workspace.open_file(source.clone(), false, window, cx);
        });
    });
    wait_for(visual, "workspace hints", |_, cx| {
        workspace.read(cx).active_file().is_some_and(|file| {
            let editor = file.editor.read(cx);
            editor.projected_wrap_rows(0) > 1 && editor.text().to_string() == working_text
        })
    });
    let anchor = working_text.find("anchor").unwrap();
    let editor = visual.update(|window, cx| {
        let editor = workspace.read(cx).files[&source].editor.clone();
        editor.update(cx, |editor, cx| {
            editor.focus(window, cx);
            editor.set_selected_range(anchor..anchor, cx);
        });
        editor
    });
    wait_for(visual, "caret after the wrapped hint", |_, cx| {
        let state = editor.read(cx);
        state
            .range_to_bounds(&(anchor..anchor))
            .zip(state.range_to_bounds(&(0..0)))
            .is_some_and(|(after, before)| after.top() > before.top())
    });
    visual.simulate_keystrokes("up");
    visual.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            assert!(editor.cursor() < anchor);
            editor.set_selected_range(0..working_text.len(), cx);
            assert_eq!(editor.selected_text().to_string(), working_text);
            editor.apply_editor_edits(
                &[(
                    0..working_text.find('\n').unwrap(),
                    "// waiting anchor".into(),
                )],
                false,
                window,
                cx,
            );
        });
    });
    wait_for(
        visual,
        "pending response for the intermediate edit",
        |_, cx| {
            let state = editor.read(cx);
            root.join("held-inlay").exists()
                && state.projected_wrap_rows(0) == 1
                && stable_hint_width(state).is_some_and(|width| width > px(18.))
        },
    );
    visual.update(|window, cx| {
        editor.update(cx, |editor, cx| {
            editor.apply_editor_edits(
                &[(
                    0..editor.text().line_end_offset(0),
                    "// edited anchor".into(),
                )],
                false,
                window,
                cx,
            );
        });
    });
    let anchor_advance = |cx: &App| {
        let editor = editor.read(cx);
        let offset = "// edited ".len();
        let wide = editor.range_to_bounds(&(0..offset))?.size.width;
        let before = editor.range_to_bounds(&(0..offset - 1))?.size.width;
        Some(wide - before)
    };
    wait_for(
        visual,
        "edited line with source-backed hint geometry",
        |_, cx| {
            editor.read(cx).projected_wrap_rows(0) == 1
                && anchor_advance(cx).is_some_and(|width| width > px(18.))
                && stable_hint_width(editor.read(cx)).is_some_and(|width| width > px(18.))
                && root.join("late-inlay-sent").exists()
        },
    );
    visual.update(|_, cx| {
        let hints = workspace.read(cx).files[&source].inlay_hints.clone();
        hints.set_enabled(false, cx);
    });
    wait_for(visual, "hidden workspace hints", |_, cx| {
        anchor_advance(cx).is_some_and(|width| width < px(18.))
    });
}

#[::core::prelude::v1::test]
fn commit_diff_reuses_lsp_for_unambiguous_worktree_lines() {
    check_historical_diff_lsp(false);
}

#[::core::prelude::v1::test]
fn arbitrary_historical_comparison_reuses_verified_lsp_and_remains_read_only() {
    check_historical_diff_lsp(true);
}

fn check_historical_diff_lsp(arbitrary: bool) {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-qb", "main"]);
    git(&root, &["config", "user.name", "Reviewer"]);
    git(&root, &["config", "user.email", "reviewer@example.invalid"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    let cases = [
        (
            "left/main.cpp",
            "// 😺 worktree left old anchor",
            "// 😺 worktree left new anchor",
        ),
        (
            "right/main.cpp",
            "// 😺 worktree right old anchor",
            "// 😺 worktree right new anchor",
        ),
        (
            "ambiguous/main.cpp",
            "// 😺 worktree ambiguous old anchor",
            "// 😺 worktree repeated anchor",
        ),
    ];
    for (path, before, _) in cases {
        let path = root.join(path);
        fs::create_dir(path.parent().unwrap()).unwrap();
        fs::write(path, format!("{before}\n")).unwrap();
    }
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "Before"]);
    for (path, _, after) in cases {
        fs::write(root.join(path), format!("{after}\n")).unwrap();
    }
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "Reviewed"]);
    let oid = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(oid.status.success());
    let oid = String::from_utf8(oid.stdout).unwrap().trim().to_owned();
    fs::write(
        root.join(cases[0].0),
        format!("// inserted above the old line\n{}\n", cases[0].1),
    )
    .unwrap();
    fs::write(
        root.join(cases[1].0),
        format!(
            "// first inserted line\n// second inserted line\n{}\n// added later\n",
            cases[1].2
        ),
    )
    .unwrap();
    fs::write(
        root.join(cases[2].0),
        format!("{}\n{}\n", cases[2].2, cases[2].2),
    )
    .unwrap();
    git(&root, &["commit", "-qam", "Shift and duplicate lines"]);

    let config_path = fixture.path().join("config.toml");
    let server = fixture.path().join("server.py");
    fs::write(
        &server,
        include_str!("../../../../tests/fixtures/lsp_inlay_hints.py"),
    )
    .unwrap();
    let python = if cfg!(windows) { "python" } else { "python3" };
    fs::write(
        &config_path,
        format!(
            "[editor]\ninlay_hints = true\n[lsp.clangd]\ncommand = '{python} -u {}'\n",
            server.display()
        ),
    )
    .unwrap();
    let config = crate::config::ConfigFile::load(Some(config_path));
    let platform = gpui_platform::current_platform(true);
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        platform.text_system(),
    );
    app.update(gpui_component::init);
    let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
    let mut workspace = None;
    let (_, visual) = app.add_window_view(|window, cx| {
        let view =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Git, window, cx));
        workspace = Some(view.clone());
        gpui_component::Root::new(view, window, cx)
    });
    let workspace = workspace.unwrap();
    visual.simulate_resize(size(px(1250.), px(1000.)));
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(
                Location {
                    root: root.clone(),
                    file: None,
                },
                window,
                cx,
            );
        });
    });
    wait_for(visual, "git panel", |_, cx| {
        workspace.read(cx).git.is_some()
    });
    let panel = visual.update(|window, cx| {
        let panel = workspace.read(cx).git.clone().unwrap();
        panel.update(cx, |panel, cx| {
            panel.show_history = false;
            panel.split = false;
            let selection = if arbitrary {
                let commit = panel.backend.commit(&oid).unwrap();
                Selection::Compare {
                    base: commit.parents.first().cloned(),
                    target: Some(oid),
                    previous: Box::new(Selection::Worktree(Mode::All)),
                }
            } else {
                Selection::Commit(oid, None)
            };
            panel.load_review(selection, false, window, cx);
            panel.editor.focus(window, cx);
            cx.notify();
        });
        panel
    });

    let mut navigation = None;
    for split in [false, true] {
        let mode = if split {
            DiffMode::Split
        } else {
            DiffMode::Unified
        };
        visual.update(|_, cx| {
            panel.update(cx, |panel, cx| {
                panel.split = split;
                cx.notify();
            });
        });
        wait_for(
            visual,
            "only unambiguously mapped commit lines show hints",
            |window, cx| {
                let panel = panel.read(cx);
                if panel.diff.read(cx).mode() != mode {
                    return false;
                }
                let modified = panel.editor.focused_column(window, cx).read(cx);
                let original = if split {
                    panel.editor.original_state().read(cx)
                } else {
                    modified
                };
                for (ix, (_, before, after)) in cases.into_iter().enumerate() {
                    let Some(old_row) =
                        original.text().iter_lines().position(|line| line == before)
                    else {
                        return false;
                    };
                    let Some(new_row) = modified.text().iter_lines().position(|line| line == after)
                    else {
                        return false;
                    };
                    let old_hints = original.projected_wrap_rows(old_row) > 1;
                    let new_hints = modified.projected_wrap_rows(new_row) > 1;
                    if (old_hints, new_hints) != (ix == 0, ix == 1) {
                        return false;
                    }
                }
                true
            },
        );

        let responses = Rc::new(RefCell::new(Vec::new()));
        visual.update(|window, cx| {
            let panel = panel.read(cx);
            let modified = panel.editor.focused_column(window, cx).clone();
            let original = if split {
                panel.editor.original_state().clone()
            } else {
                modified.clone()
            };
            for (ix, (path, before, after)) in cases.into_iter().enumerate() {
                for (editor, line, matches) in
                    [(&original, before, ix == 0), (&modified, after, ix == 1)]
                {
                    let editor = editor.read(cx);
                    let text = editor.text().clone();
                    let offset =
                        text.to_string().find(line).unwrap() + line.find("anchor").unwrap();
                    let projected_range = lsp_types::Range::new(
                        text.offset_to_position(offset),
                        text.offset_to_position(offset + "anchor".len()),
                    );
                    let hover = editor.lsp().hover_provider.clone().unwrap();
                    let definition = editor.lsp().definition_provider.clone().unwrap();
                    let hover = hover.hover(&text, offset, window, cx);
                    let definition = definition.definitions(&text, offset, window, cx);
                    let responses = responses.clone();
                    let target = root.join(path);
                    cx.spawn(async move |_| {
                        let hover = hover.await.unwrap();
                        let definition = definition.await.unwrap();
                        responses.borrow_mut().push((
                            line,
                            matches.then_some(ix + 1),
                            target,
                            projected_range,
                            hover,
                            definition,
                        ));
                    })
                    .detach();
                }
            }
        });
        wait_for(visual, "hover and definition responses", |_, _| {
            responses.borrow().len() == cases.len() * 2
        });
        for (line, row, target, projected_range, hover, links) in responses.take() {
            if let Some(row) = row {
                let hover = hover.unwrap();
                let lsp_types::HoverContents::Markup(contents) = hover.contents else {
                    panic!("Expected LSP hover documentation");
                };
                assert_eq!(contents.value, line);
                assert_eq!(links.len(), 1);
                let link = &links[0];
                assert_eq!(hover.range, Some(projected_range));
                assert_eq!(link.origin_selection_range, Some(projected_range));
                assert_eq!(link.target_selection_range.start.line as usize, row);
                assert_eq!(
                    link.target_uri.as_str(),
                    url::Url::from_file_path(&target).unwrap().as_str()
                );
                if target.ends_with(cases[0].0) {
                    navigation = Some(lsp_types::ShowDocumentParams {
                        uri: link.target_uri.clone(),
                        external: None,
                        take_focus: Some(true),
                        selection: Some(link.target_selection_range),
                    });
                }
            } else {
                assert!(hover.is_none());
                assert!(links.is_empty());
            }
        }
    }
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| workspace.toggle_vim_mode(window, cx));
    });
    for (editor, line) in visual.update(|_, cx| {
        let panel = panel.read(cx);
        [
            (panel.editor.original_state().clone(), cases[0].1),
            (panel.editor.modified_state().clone(), cases[1].2),
        ]
    }) {
        let caret = visual.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                let caret =
                    editor.text().to_string().find(line).unwrap() + line.find("anchor").unwrap();
                editor.set_selected_range(caret..caret, cx);
                editor.focus(window, cx);
                caret
            })
        });
        visual.simulate_keystrokes("shift-k");
        wait_for(visual, "documentation at the diff caret", |_, cx| {
            editor.read(cx).hover_popover().is_some_and(|popover| {
                matches!(popover.hover.as_ref().map(|hover| &hover.contents),
                    Some(lsp_types::HoverContents::Markup(markup)) if markup.value == line)
            })
        });
        visual.simulate_keystrokes("escape");
        visual.update(|window, cx| {
            assert!(editor.read(cx).hover_popover().is_none());
            assert!(editor.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(editor.read(cx).cursor(), caret);
        });
    }
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| workspace.toggle_vim_mode(window, cx));
    });
    if arbitrary {
        let (editor, before) = visual.update(|window, cx| {
            let editor = panel.read(cx).editor.focused_column(window, cx).clone();
            let text = editor.read(cx).text().clone();
            let caret = text.to_string().find(cases[1].2).unwrap();
            editor.update(cx, |editor, cx| {
                editor.set_selected_range(caret..caret, cx);
                editor.focus(window, cx);
            });
            (editor, text)
        });
        visual.simulate_input("blocked");
        visual.update(|_, cx| {
            assert_eq!(editor.read(cx).text(), &before);
            let file = &workspace.read(cx).files[Path::new(cases[1].0)];
            assert_eq!(file.committed, file.saved);
        });
    }
    // A draft invalidates the mapping until the LSP document matches the worktree again.
    visual.update(|window, cx| {
        let current = workspace.read(cx).files[Path::new(cases[1].0)]
            .editor
            .clone();
        current.update(cx, |current, cx| {
            let end = current.text().len();
            current.apply_editor_edits(
                &[(end..end, "// unsaved line outside the hunk\n".into())],
                false,
                window,
                cx,
            );
        });
    });
    wait_for(
        visual,
        "the prior mapping is invalidated by a draft",
        |window, cx| {
            let editor = panel.read(cx).editor.focused_column(window, cx).read(cx);
            let row = editor
                .text()
                .iter_lines()
                .position(|line| line == cases[1].2)
                .unwrap();
            editor.projected_wrap_rows(row) == 1
        },
    );

    let navigation = navigation.unwrap();
    let expected = visual.update(|window, cx| {
        let expected = workspace
            .read(cx)
            .repository
            .as_ref()
            .unwrap()
            .source_path(navigation.uri.as_str())
            .unwrap();
        let editor = panel.read(cx).editor.focused_column(window, cx).read(cx);
        let show = editor.lsp().show_document.clone().unwrap();
        assert!(show(&navigation, window, cx));
        expected
    });
    wait_for(
        visual,
        "definition opens the current worktree file",
        |_, cx| {
            let workspace = workspace.read(cx);
            workspace.view == WorkspaceView::Editor
                && workspace.tabs.active() == Some(expected.as_path())
                && workspace.active_file().is_some_and(|file| {
                    let editor = file.editor.read(cx);
                    editor.text().offset_to_point(editor.cursor()).row == 1
                })
        },
    );
}
