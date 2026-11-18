use super::*;
use crate::workspace::tree_motion::ROW_HEIGHT;

fn draw_frame(visual: &mut VisualTestContext) {
    visual.executor().advance_clock(Duration::from_millis(32));
    visual.update(|window, cx| {
        window.simulate_next_frame(cx);
        window.draw(cx).clear(cx);
    });
}

fn settle(visual: &mut VisualTestContext) {
    for _ in 0..8 {
        visual.run_until_parked();
        draw_frame(visual);
    }
}

#[::core::prelude::v1::test]
fn single_file_mode_switches_the_diff_and_keeps_wrap_in_the_reading_menu() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-qb", "main"]);
    fs::write(root.join("first.txt"), "first change\n").unwrap();
    fs::write(root.join("second.txt"), "second change\n").unwrap();

    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    app.update(gpui_component::init);
    let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
    let config = crate::config::ConfigFile::load(Some(fixture.path().join("config.toml")));
    let mut workspace = None;
    let (_, visual) = app.add_window_view(|window, cx| {
        let view =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Git, window, cx));
        workspace = Some(view.clone());
        gpui_component::Root::new(view, window, cx)
    });
    let workspace = workspace.unwrap();
    visual.simulate_resize(size(px(1280.), px(850.)));
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
    settle(visual);
    let panel = visual.update(|window, cx| {
        let panel = workspace.read(cx).git.clone().unwrap();
        panel.update(cx, |panel, cx| {
            panel.set_history_visible(false, cx);
            panel.set_tree_visible(true, cx);
            panel.select_worktree(window, cx);
        });
        panel
    });
    settle(visual);

    let click = |visual: &mut VisualTestContext, selector| {
        let center = visual.debug_bounds(selector).unwrap().center();
        visual.simulate_click(center, Modifiers::default());
        settle(visual);
    };
    let paths = |visual: &mut VisualTestContext| {
        visual.update(|_, cx| {
            panel
                .read(cx)
                .diff
                .read(cx)
                .files()
                .iter()
                .map(|file| file.path().to_string())
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(paths(visual), ["first.txt", "second.txt"]);
    click(visual, "git-diff-single-file");
    assert_eq!(paths(visual), ["first.txt"]);
    click(visual, "git-tree-row:file:second.txt");
    assert_eq!(paths(visual), ["second.txt"]);
    click(visual, "git-tree-row:file:first.txt");
    assert_eq!(paths(visual), ["first.txt"]);

    let wrap = visual.update(|_, cx| panel.read(cx).wrap);
    click(visual, "git-reading-options");
    assert!(visual.debug_bounds("popup-menu-surface").is_some());
    visual.simulate_keystrokes("down down down enter");
    settle(visual);
    assert_eq!(visual.update(|_, cx| panel.read(cx).wrap), !wrap);
    assert_eq!(paths(visual), ["first.txt"]);

    click(visual, "git-diff-single-file");
    assert_eq!(paths(visual), ["first.txt", "second.txt"]);
}

#[::core::prelude::v1::test]
fn git_menus_confirm_restore_and_discard_for_files_and_filtered_directories() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    fs::create_dir_all(root.join("folder/nested")).unwrap();
    let visible = "folder/visible.txt";
    let hidden = "folder/nested/hidden.txt";
    let untracked = "folder/untracked.txt";
    let hidden_untracked = "folder/nested/new.txt";
    let single_untracked = "new.txt";
    let ignored = "folder/ignored.txt";
    let deleted = "deleted.txt";
    for path in [visible, hidden, deleted] {
        fs::write(root.join(path), "original\n").unwrap();
    }
    git(&root, &["init", "-qb", "main"]);
    git(&root, &["add", "."]);
    git(
        &root,
        &[
            "-c",
            "user.name=Reviewer",
            "-c",
            "user.email=reviewer@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "initial",
        ],
    );
    fs::write(root.join(visible), "staged\n").unwrap();
    git(&root, &["add", visible]);
    fs::write(root.join(visible), "unstaged\n").unwrap();
    fs::write(root.join(hidden), "changed\n").unwrap();
    for path in [untracked, hidden_untracked, single_untracked, ignored] {
        fs::write(root.join(path), "new\n").unwrap();
    }
    fs::write(root.join(".git/info/exclude"), format!("{ignored}\n")).unwrap();
    fs::remove_file(root.join(deleted)).unwrap();

    let platform = gpui_platform::current_platform(true);
    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        platform.text_system(),
    );
    cx.update(gpui_component::init);
    let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
    let config = crate::config::ConfigFile::load(Some(fixture.path().join("config.toml")));
    let mut workspace = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        let view =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Git, window, cx));
        workspace = Some(view.clone());
        gpui_component::Root::new(view, window, cx)
    });
    let workspace = workspace.unwrap();
    visual.simulate_resize(size(px(1280.), px(850.)));
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
    settle(visual);
    let panel = visual.update(|window, cx| {
        let panel = workspace.read(cx).git.clone().unwrap();
        panel.update(cx, |panel, cx| {
            panel.show_history = false;
            panel.show_tree = true;
            panel.load_review(Selection::Worktree(Mode::All), false, window, cx);
        });
        panel
    });
    settle(visual);

    let open_action = |visual: &mut VisualTestContext, row, keys| {
        let pos = visual.debug_bounds(row).unwrap().center();
        visual.simulate_mouse_down(pos, MouseButton::Right, Modifiers::default());
        visual.simulate_mouse_up(pos, MouseButton::Right, Modifiers::default());
        settle(visual);
        visual.simulate_keystrokes(keys);
        settle(visual);
        assert!(visual.debug_bounds("git-confirm-file-restore").is_some());
    };
    let click_dialog = |visual: &mut VisualTestContext, button| {
        let pos = visual.debug_bounds(button).unwrap().center();
        visual.simulate_mouse_move(pos, None, Modifiers::default());
        visual.simulate_click(pos, Modifiers::default());
        settle(visual);
    };
    open_action(visual, "git-tree-row:file:deleted.txt", "up enter");
    click_dialog(visual, "git-cancel-file-restore");
    assert!(!root.join(deleted).exists());
    open_action(visual, "git-tree-row:file:deleted.txt", "up enter");
    click_dialog(visual, "git-confirm-file-restore");
    assert_eq!(
        fs::read_to_string(root.join(deleted)).unwrap(),
        "original\n"
    );

    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.filter_input.update(cx, |input, cx| {
                input.set_value("visible", window, cx);
            });
            panel.filter = "visible".into();
            cx.notify();
        });
    });
    settle(visual);
    assert!(
        visual
            .debug_bounds("git-tree-row:file:folder/nested/hidden.txt")
            .is_none()
    );
    open_action(visual, "git-tree-row:folder:folder", "up enter");
    assert!(visual.debug_bounds("git-restore-count:2").is_some());
    assert!(visual.debug_bounds("git-restore-hidden:1").is_some());

    fs::write(root.join(visible), "changed after confirmation opened\n").unwrap();
    click_dialog(visual, "git-confirm-file-restore");
    assert_eq!(
        fs::read_to_string(root.join(visible)).unwrap(),
        "changed after confirmation opened\n"
    );
    assert_eq!(fs::read_to_string(root.join(hidden)).unwrap(), "changed\n");
    visual.update(|_, cx| {
        assert!(
            panel
                .read(cx)
                .stage_error
                .as_deref()
                .unwrap()
                .contains(visible)
        );
    });

    open_action(visual, "git-tree-row:folder:folder", "up enter");
    click_dialog(visual, "git-confirm-file-restore");
    assert_eq!(fs::read_to_string(root.join(visible)).unwrap(), "staged\n");
    assert_eq!(fs::read_to_string(root.join(hidden)).unwrap(), "original\n");
    assert!(root.join(untracked).is_file());
    assert!(root.join(hidden_untracked).is_file());
    visual.update(|_, cx| {
        let status = panel
            .read(cx)
            .backend
            .file_version(visible)
            .unwrap()
            .status
            .unwrap();
        assert!(status.staged);
        assert!(!status.unstaged);
    });
    let undo = if cfg!(target_os = "macos") {
        "cmd-z"
    } else {
        "ctrl-z"
    };
    for (path, before) in [
        (visible, "changed after confirmation opened\n"),
        (hidden, "changed\n"),
    ] {
        visual.simulate_keystrokes(undo);
        settle(visual);
        visual.update(|_, cx| {
            let file = &workspace.read(cx).files[Path::new(path)];
            assert_eq!(file.editor.read(cx).text().to_string(), before);
            assert!(file.is_dirty());
        });
    }

    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.filter_input.update(cx, |input, cx| {
                input.set_value("untracked", window, cx);
            });
            panel.filter = "untracked".into();
            panel.load_review(Selection::Worktree(Mode::Unstaged), false, window, cx);
        });
    });
    settle(visual);

    open_action(visual, "git-tree-row:file:folder/untracked.txt", "up enter");
    assert!(visual.debug_bounds("git-restore-count:1").is_some());
    assert!(
        visual
            .debug_bounds("git-discard-untracked:folder/untracked.txt")
            .is_some()
    );
    click_dialog(visual, "git-cancel-file-restore");
    assert!(root.join(untracked).is_file());

    open_action(visual, "git-tree-row:folder:folder", "up enter");
    assert!(visual.debug_bounds("git-restore-count:2").is_some());
    assert!(visual.debug_bounds("git-restore-hidden:1").is_some());
    for selector in [
        "git-discard-untracked:folder/untracked.txt",
        "git-discard-untracked:folder/nested/new.txt",
    ] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector}");
    }
    click_dialog(visual, "git-confirm-file-restore");
    for path in [untracked, hidden_untracked] {
        assert!(!root.join(path).exists(), "{path}");
    }
    for path in [ignored, single_untracked, visible, hidden] {
        assert!(root.join(path).is_file(), "{path}");
    }

    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.filter_input.update(cx, |input, cx| {
                input.set_value("", window, cx);
            });
            panel.filter.clear();
            cx.notify();
        });
    });
    settle(visual);
    let row = visual
        .debug_bounds("git-tree-row:file:new.txt")
        .unwrap()
        .center();
    visual.simulate_click(row, Modifiers::default());
    settle(visual);
    let menu = visual
        .debug_bounds("git-file-menu:new.txt")
        .unwrap()
        .center();
    visual.simulate_mouse_move(menu, None, Modifiers::default());
    visual.simulate_click(menu, Modifiers::default());
    settle(visual);
    visual.simulate_keystrokes("up enter");
    settle(visual);
    assert!(
        visual
            .debug_bounds("git-discard-untracked:new.txt")
            .is_some()
    );
    click_dialog(visual, "git-confirm-file-restore");
    assert!(!root.join(single_untracked).exists());
}

#[::core::prelude::v1::test]
fn worktree_panel_preferences_survive_compact_layout_switching_and_reopen() {
    use crate::{config::ConfigFile, repository::Repository, session::PanelPreferences};
    use gpui_component::Root;

    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    let other = fixture.path().join("other");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-qb", "main"]);
    fs::write(root.join("code.txt"), "original\n").unwrap();
    fs::write(root.join("guide.md"), "# Guide\n").unwrap();
    git(&root, &["add", "."]);
    git(
        &root,
        &[
            "-c",
            "user.name=Reviewer",
            "-c",
            "user.email=reviewer@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "initial",
        ],
    );
    git(
        &root,
        &["worktree", "add", "-qb", "other", other.to_str().unwrap()],
    );
    for path in [&root, &other] {
        fs::write(path.join("code.txt"), "changed\n").unwrap();
    }
    let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
    let config = fixture.path().join("config.toml");
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    app.update(gpui_component::init);

    fn make_window<'a>(
        app: &'a mut TestAppContext,
        store: &Arc<Store>,
        config: &Path,
    ) -> (Entity<Workspace>, &'a mut VisualTestContext) {
        let mut workspace = None;
        let (_, visual) = app.add_window_view(|window, cx| {
            let view = cx.new(|cx| {
                Workspace::with_startup_view(
                    store.clone(),
                    ConfigFile::load(Some(config.to_owned())),
                    Some(WorkspaceView::Git),
                    window,
                    cx,
                )
            });
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        visual.simulate_resize(size(px(1280.), px(760.)));
        (workspace.unwrap(), visual)
    }
    let open = |workspace: &Entity<Workspace>, visual: &mut VisualTestContext, path: &Path| {
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root: path.to_owned(),
                        file: None,
                    },
                    window,
                    cx,
                );
            });
        });
        settle(visual);
        visual.update(|_, cx| {
            let workspace = workspace.read(cx);
            assert_eq!(workspace.repository.as_ref().unwrap().root, path);
            assert!(workspace.git.as_ref().unwrap().read(cx).review.is_some());
        });
    };
    let click = |visual: &mut VisualTestContext, name: &'static str| {
        let bounds = visual
            .debug_bounds(name)
            .unwrap_or_else(|| panic!("Missing control {name}"));
        visual.simulate_click(bounds.center(), Modifiers::default());
        settle(visual);
    };
    let panes = |visual: &mut VisualTestContext| {
        (
            visual.debug_bounds("git-history-toolbar").is_some(),
            visual.debug_bounds("git-tree-toolbar").is_some(),
        )
    };
    let switch = |workspace: &Entity<Workspace>, visual: &mut VisualTestContext, view| {
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| workspace.switch_view(view, window, cx));
        });
        settle(visual);
    };

    let (workspace, visual) = make_window(&mut app, &store, &config);
    open(&workspace, visual, &root);
    assert_eq!(panes(visual), (true, true));
    visual.simulate_resize(size(px(760.), px(760.)));
    settle(visual);
    assert_eq!(panes(visual), (true, false));
    click(visual, "git-toggle-files");
    assert_eq!(panes(visual), (false, true));
    visual.simulate_resize(size(px(1280.), px(760.)));
    settle(visual);
    assert_eq!(panes(visual), (true, true));
    click(visual, "git-hide-history");
    click(visual, "git-close-files");
    assert_eq!(panes(visual), (false, false));
    switch(&workspace, visual, WorkspaceView::Editor);
    click(visual, "hide-file-tree");
    assert!(visual.debug_bounds("show-file-tree").is_some());
    switch(&workspace, visual, WorkspaceView::Docs);
    click(visual, "hide-file-tree");
    assert!(visual.debug_bounds("show-file-tree").is_some());
    visual.update(|_, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.editor_tree_width = px(310.);
            workspace.docs_tree_width = px(295.);
            workspace.outline_width = px(250.);
            workspace.outline_visibility = Some(false);
            workspace.git.as_ref().unwrap().update(cx, |git, _| {
                git.history_width = px(340.);
                git.tree_width = px(330.);
            });
        });
    });
    switch(&workspace, visual, WorkspaceView::Git);

    open(&workspace, visual, &other);
    assert_eq!(panes(visual), (true, true));
    switch(&workspace, visual, WorkspaceView::Editor);
    assert!(visual.debug_bounds("hide-file-tree").is_some());
    switch(&workspace, visual, WorkspaceView::Docs);
    assert!(visual.debug_bounds("hide-file-tree").is_some());
    switch(&workspace, visual, WorkspaceView::Git);
    open(&workspace, visual, &root);
    assert_eq!(panes(visual), (false, false));
    assert_eq!(
        store
            .session(&Repository::open(&root).unwrap())
            .unwrap()
            .panels,
        PanelPreferences {
            editor_tree: false,
            docs_tree: false,
            git_history: false,
            git_tree: false,
            editor_tree_width: 310.,
            docs_tree_width: 295.,
            doc_outline_width: 250.,
            doc_outline_visible: Some(false),
            git_history_width: 340.,
            git_tree_width: 330.,
        },
    );

    let (reopened, visual) = make_window(&mut app, &store, &config);
    open(&reopened, visual, &root);
    assert_eq!(panes(visual), (false, false));
    visual.update(|_, cx| {
        let workspace = reopened.read(cx);
        assert_eq!(workspace.editor_tree_width, px(310.));
        assert_eq!(workspace.docs_tree_width, px(295.));
        assert_eq!(workspace.outline_width, px(250.));
        assert_eq!(workspace.outline_visibility, Some(false));
        let git = workspace.git.as_ref().unwrap().read(cx);
        assert_eq!(git.history_width, px(340.));
        assert_eq!(git.tree_width, px(330.));
    });
    for view in [WorkspaceView::Editor, WorkspaceView::Docs] {
        switch(&reopened, visual, view);
        assert!(visual.debug_bounds("show-file-tree").is_some());
    }
}

#[::core::prelude::v1::test]
fn diff_file_and_row_menus_reveal_worktree_lines() {
    use gpui_component::{Root, WindowExt as _, input::RopeExt as _};

    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-qb", "main"]);
    git(&root, &["config", "user.name", "Reviewer"]);
    git(&root, &["config", "user.email", "reviewer@example.invalid"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    let original = (0..82)
        .map(|row| match row {
            31 => "😀 shared marker\n".to_owned(),
            32 => "historical line\n".to_owned(),
            36 => "removed from the worktree\n".to_owned(),
            _ => format!("row {row}\n"),
        })
        .collect::<String>();
    let path = "new.txt";
    fs::write(root.join("old.txt"), &original).unwrap();
    let guide = "z-guide.md";
    let guide_original = original.replace("historical line", "## Initial section");
    fs::write(root.join(guide), &guide_original).unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "Initial"]);
    git(&root, &["mv", "old.txt", path]);
    let renamed = original.replace("historical line", "committed line");
    fs::write(root.join(path), &renamed).unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "Rename"]);
    let current = renamed
        .replace("row 29\n", "row 29\ninserted working line\n")
        .replace(
            "committed line",
            &format!("😀 working marker {}", "wrapped ".repeat(30)),
        )
        .replace("row 34\n", "\n")
        .replace("removed from the worktree\n", "");
    fs::write(root.join(path), &current).unwrap();
    let changed_guide = guide_original.replace("## Initial section", "## Staged section");
    fs::write(root.join(guide), &changed_guide).unwrap();
    git(&root, &["add", guide]);
    fs::write(
        root.join(guide),
        changed_guide.replace("row 5", "early draft row"),
    )
    .unwrap();

    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    app.update(gpui_component::init);
    let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
    let config = crate::config::ConfigFile::load(Some(fixture.path().join("config.toml")));
    let mut workspace = None;
    let (_, visual) = app.add_window_view(|window, cx| {
        let view =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Git, window, cx));
        workspace = Some(view.clone());
        Root::new(view, window, cx)
    });
    let workspace = workspace.unwrap();
    visual.simulate_resize(size(px(1200.), px(780.)));
    visual.update(|window, cx| {
        window.activate_window();
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(Location { root, file: None }, window, cx);
        });
    });
    settle(visual);
    let (panel, commit) = visual.update(|_, cx| {
        let panel = workspace.read(cx).git.clone().unwrap();
        let oid = panel.read(cx).commits[0].oid.to_string();
        (panel, Selection::Commit(oid, None))
    });
    let show_diff = |visual: &mut VisualTestContext, split, history| {
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.switch_view(WorkspaceView::Git, window, cx);
            });
            panel.update(cx, |panel, cx| {
                panel.show_history = false;
                panel.show_tree = true;
                panel.split = split;
                panel.wrap = true;
                let selection = if history {
                    commit.clone()
                } else {
                    Selection::Worktree(Mode::All)
                };
                panel.load_review(selection, false, window, cx);
                panel.reveal_tree_file(path, window, cx);
                cx.notify();
            });
        });
        settle(visual);
    };
    let assert_editor = |visual: &mut VisualTestContext, file: &str, expected| {
        visual.update(|window, cx| {
            let owner = workspace.read(cx);
            assert_eq!(owner.view, WorkspaceView::Editor);
            assert_eq!(owner.tabs.active(), Some(Path::new(file)));
            let editor = owner.files[Path::new(file)].editor.read(cx);
            if file == path {
                assert_eq!(editor.text(), current.as_str());
            }
            let offset = editor.cursor();
            let position = editor.text().offset_to_position(offset);
            assert_eq!((position.line, position.character), (expected, 0));
            let caret = editor.range_to_bounds(&(offset..offset)).unwrap();
            assert!(editor.input_bounds().contains(&caret.center()));
            assert!(window.notifications(cx).is_empty());
        });
    };
    let menu_click = |visual: &mut VisualTestContext, target, keys| {
        visual.simulate_mouse_down(target, MouseButton::Right, Modifiers::default());
        visual.simulate_mouse_up(target, MouseButton::Right, Modifiers::default());
        settle(visual);
        visual.simulate_keystrokes(keys);
        settle(visual);
    };

    for split in [false, true] {
        show_diff(visual, split, false);
        visual.update(|window, cx| {
            let panel = panel.read(cx);
            assert_eq!(
                panel.diff.read(cx).mode(),
                if split {
                    DiffMode::Split
                } else {
                    DiffMode::Unified
                },
            );
            let editor = panel.editor.focused_column(window, cx).clone();
            editor.update(cx, |editor, cx| {
                let offset = editor.text().to_string().find("😀 working marker").unwrap();
                editor.set_selected_range(offset..offset, cx);
                editor.focus(window, cx);
            });
        });
        settle(visual);
        let button = visual
            .debug_bounds("git-open-file-in-editor:new.txt")
            .unwrap();
        visual.simulate_click(button.center(), Modifiers::default());
        settle(visual);
        assert_editor(visual, path, 27);

        show_diff(visual, split, false);
        let tree_file = visual.debug_bounds("git-tree-row:file:new.txt").unwrap();
        menu_click(visual, tree_file.center(), "down enter");
        assert_editor(visual, path, 27);
    }

    for (split, history, side, line, opposite_padding, wrapped, expected) in [
        (false, false, DiffSide::Modified, 34, false, true, 33),
        (true, false, DiffSide::Modified, 36, false, false, 35),
        (false, false, DiffSide::Original, 37, false, false, 37),
        (true, false, DiffSide::Original, 37, true, false, 37),
        (true, true, DiffSide::Original, 32, false, false, 32),
        (true, true, DiffSide::Original, 33, false, false, 33),
    ] {
        show_diff(visual, split, history);
        let position = DiffLinePosition::new(path, side, line);
        let column = visual.update(|window, cx| {
            let panel = panel.read(cx);
            let column = if split && (side == DiffSide::Original) != opposite_padding {
                panel.editor.original_state()
            } else {
                panel.editor.focused_column(window, cx)
            }
            .clone();
            let clicked = panel.review_source_bounds(&position, cx).unwrap();
            column.update(cx, |editor, cx| {
                editor.focus(window, cx);
                let row = editor
                    .visible_row_range()
                    .unwrap()
                    .find(|&row| editor.projected_line_bounds(row).unwrap().top() == clicked.top())
                    .unwrap();
                let text = editor.text();
                // A right click inside an existing selection preserves its caret.
                editor.set_selected_range(
                    text.line_start_offset(row)..text.line_start_offset(row + 2),
                    cx,
                );
            });
            column
        });
        settle(visual);
        let target = visual.update(|_, cx| {
            let panel = panel.read(cx);
            let bounds = panel.review_source_bounds(&position, cx).unwrap();
            let column_bounds = column.read(cx).input_bounds();
            let y = if wrapped {
                assert!(bounds.size.height > column.read(cx).line_height().unwrap());
                bounds.bottom() - px(4.)
            } else {
                bounds.center().y
            };
            let target = point(column_bounds.center().x, y);
            assert!(column_bounds.contains(&target));
            target
        });
        menu_click(visual, target, "up enter");
        assert_editor(visual, path, expected);
    }

    show_diff(visual, false, false);
    let guide_index = visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.set_files_collapsed(true, cx);
            let review = panel.review.as_mut().unwrap();
            let ix = review.by_path[guide];
            review.patches[ix] = PatchState::Pending;
            review.documents[ix] = git::placeholder(&review.detail.files[ix], "").unwrap();
            review.patch_display[ix].content = None;
            panel.patch_loading[1] = true;
            ix
        })
    });
    settle(visual);
    let tree_guide = visual.debug_bounds("git-tree-row:file:z-guide.md").unwrap();
    menu_click(visual, tree_guide.center(), "down enter");
    visual.update(|window, cx| {
        assert_eq!(workspace.read(cx).view, WorkspaceView::Git);
        panel.update(cx, |panel, cx| {
            assert_eq!(panel.pending_editor_open.as_deref(), Some(guide));
            let review = panel.review.as_ref().unwrap();
            assert!(review.collapsed.contains(guide));
            assert!(
                review.documents[guide_index]
                    .hunk_locations()
                    .next()
                    .is_none()
            );
            panel.patch_loading[1] = false;
            panel.prefetch_patches(window, cx);
        });
    });
    settle(visual);
    assert_editor(visual, guide, 2);

    show_diff(visual, false, false);
    let tree_guide = visual.debug_bounds("git-tree-row:file:z-guide.md").unwrap();
    menu_click(visual, tree_guide.center(), "down down enter");
    visual.update(|_, cx| {
        let owner = workspace.read(cx);
        assert_eq!(owner.view, WorkspaceView::Docs);
        assert_eq!(owner.doc.as_ref().unwrap().read(cx).path, Path::new(guide));
    });
}

#[::core::prelude::v1::test]
fn commit_drafts_survive_worktree_switch_and_restart_until_committed() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    let linked = fixture.path().join("linked");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-qb", "main"]);
    git(&root, &["config", "user.name", "Reviewer"]);
    git(&root, &["config", "user.email", "reviewer@example.invalid"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    git(&root, &["commit", "--allow-empty", "-qm", "Initial"]);
    git(
        &root,
        &["worktree", "add", "-qb", "linked", linked.to_str().unwrap()],
    );
    fs::write(root.join("new.txt"), "staged content\n").unwrap();
    git(&root, &["add", "new.txt"]);

    let state = fixture.path().join("state");
    let new_app = || {
        let platform = gpui_platform::current_platform(true);
        let cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        cx
    };
    let build_workspace = |window: &mut Window, cx: &mut Context<Workspace>| {
        let store = Arc::new(Store::new(state.clone()).unwrap());
        let config = crate::config::ConfigFile::load(Some(fixture.path().join("config.toml")));
        Workspace::with_startup_view(store, config, Some(WorkspaceView::Git), window, cx)
    };
    let open = |visual: &mut VisualTestContext, workspace: &Entity<Workspace>, root: &Path| {
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root: root.to_owned(),
                        file: None,
                    },
                    window,
                    cx,
                );
            });
        });
        visual.run_until_parked();
    };
    let message_input = |workspace: &Entity<Workspace>, cx: &App| {
        workspace
            .read(cx)
            .git
            .as_ref()
            .unwrap()
            .read(cx)
            .commit_message
            .clone()
    };
    let edit = |visual: &mut VisualTestContext, workspace: &Entity<Workspace>, draft: &str| {
        visual.update(|window, cx| {
            message_input(workspace, cx).update(cx, |input, cx| {
                input.set_value(draft, window, cx);
            });
        });
    };
    let message = |visual: &mut VisualTestContext, workspace: &Entity<Workspace>| {
        visual.update(|_, cx| message_input(workspace, cx).read(cx).value())
    };
    let main_draft = "Add staged file\n\nKeep this body.";
    let final_draft = "Add staged file\n\nKeep this revised body.";
    let linked_draft = "Linked worktree draft";

    {
        let mut cx = new_app();
        let (workspace, visual) = cx.add_window_view(build_workspace);
        open(visual, &workspace, &root);
        // Switch and quit without advancing the periodic session writer's clock.
        edit(visual, &workspace, main_draft);
        open(visual, &workspace, &linked);
        assert!(message(visual, &workspace).is_empty());
        edit(visual, &workspace, linked_draft);
        open(visual, &workspace, &root);
        assert_eq!(message(visual, &workspace).as_str(), main_draft);
        edit(visual, &workspace, final_draft);
        cx.quit();
    }

    {
        let mut cx = new_app();
        let (workspace, visual) = cx.add_window_view(build_workspace);
        visual.simulate_resize(size(px(1280.), px(850.)));
        open(visual, &workspace, &root);
        assert_eq!(message(visual, &workspace).as_str(), final_draft);
        settle(visual);
        let button = visual.debug_bounds("git-create-commit").unwrap();
        visual.simulate_click(button.center(), Modifiers::default());
        visual.run_until_parked();
        assert!(message(visual, &workspace).is_empty());
        cx.quit();
    }

    let mut cx = new_app();
    let (workspace, visual) = cx.add_window_view(build_workspace);
    open(visual, &workspace, &root);
    assert!(message(visual, &workspace).is_empty());
    open(visual, &workspace, &linked);
    assert_eq!(message(visual, &workspace).as_str(), linked_draft);
    cx.quit();
}

#[::core::prelude::v1::test]
fn uncommitted_tree_folds_survive_modes_history_worktree_switch_and_restart() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    let linked = fixture.path().join("linked");
    let path = "folder/file.txt";
    fs::create_dir_all(root.join("folder")).unwrap();
    git(&root, &["init", "-qb", "main"]);
    git(&root, &["config", "user.name", "Reviewer"]);
    git(&root, &["config", "user.email", "reviewer@example.invalid"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    fs::write(root.join(path), "initial\n").unwrap();
    git(&root, &["add", path]);
    git(&root, &["commit", "-qm", "Initial"]);
    git(
        &root,
        &["worktree", "add", "-qb", "linked", linked.to_str().unwrap()],
    );
    fs::write(root.join(path), "staged\n").unwrap();
    git(&root, &["add", path]);
    fs::write(root.join(path), "unstaged\n").unwrap();
    fs::write(linked.join(path), "linked change\n").unwrap();

    let new_app = || {
        let cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            gpui_platform::current_platform(true).text_system(),
        );
        cx.update(gpui_component::init);
        cx
    };
    let build_workspace = |window: &mut Window, cx: &mut Context<Workspace>| {
        let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
        let config = crate::config::ConfigFile::load(Some(fixture.path().join("config.toml")));
        Workspace::with_startup_view(store, config, Some(WorkspaceView::Git), window, cx)
    };
    let open = |visual: &mut VisualTestContext, workspace: &Entity<Workspace>, root: &Path| {
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root: root.to_owned(),
                        file: None,
                    },
                    window,
                    cx,
                );
            });
        });
        settle(visual);
    };
    let select = |visual: &mut VisualTestContext, workspace: &Entity<Workspace>, selection| {
        visual.update(|window, cx| {
            workspace
                .read(cx)
                .git
                .clone()
                .unwrap()
                .update(cx, |panel, cx| {
                    panel.load_review(selection, false, window, cx);
                });
        });
        settle(visual);
    };
    let assert_fold = |visual: &mut VisualTestContext, workspace: &Entity<Workspace>, folded| {
        visual.update(|_, cx| {
            let panel = workspace.read(cx).git.as_ref().unwrap().read(cx);
            let review = panel.review.as_ref().unwrap();
            let rows = review.tree_rows.borrow();
            let file_row = rows.as_ref().unwrap().file_row(review.by_path[path]);
            assert_eq!(file_row.is_none(), folded, "{:?}", panel.selection);
            assert!(review.collapsed.is_empty(), "the diff body stays open");
        });
    };

    {
        let mut cx = new_app();
        let (workspace, visual) = cx.add_window_view(build_workspace);
        visual.simulate_resize(size(px(1280.), px(720.)));
        open(visual, &workspace, &root);
        assert_fold(visual, &workspace, false);
        let (folder, history) = visual.update(|_, cx| {
            let panel = workspace.read(cx).git.as_ref().unwrap().read(cx);
            let viewport = panel.tree_scroll.0.borrow().base_handle.bounds();
            let folder = point(viewport.center().x, viewport.top() + ROW_HEIGHT.half());
            let head = panel.history.as_ref().unwrap().head.clone().unwrap();
            (folder, Selection::Commit(head, None))
        });
        visual.simulate_click(folder, Modifiers::default());
        settle(visual);
        assert_fold(visual, &workspace, true);
        for mode in [Mode::Staged, Mode::Unstaged] {
            select(visual, &workspace, Selection::Worktree(mode));
            assert_fold(visual, &workspace, true);
        }
        select(visual, &workspace, history.clone());
        assert_fold(visual, &workspace, false);

        open(visual, &workspace, &linked);
        assert_fold(visual, &workspace, false);
        open(visual, &workspace, &root);
        select(visual, &workspace, Selection::Worktree(Mode::All));
        assert_fold(visual, &workspace, true);
        select(visual, &workspace, history);
        cx.quit();
    }

    let mut cx = new_app();
    let (workspace, visual) = cx.add_window_view(build_workspace);
    visual.simulate_resize(size(px(1280.), px(720.)));
    open(visual, &workspace, &root);
    assert_fold(visual, &workspace, false);
    select(visual, &workspace, Selection::Worktree(Mode::All));
    assert_fold(visual, &workspace, true);
    cx.quit();
}

fn check_file_tree_navigation(split: bool) {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    fs::create_dir_all(root.join("folder")).unwrap();
    git(&root, &["init", "-qb", "main"]);
    git(&root, &["config", "user.name", "Reviewer"]);
    git(&root, &["config", "user.email", "reviewer@example.invalid"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    let paths = (0..52)
        .map(|ix| format!("folder/{ix:02}.txt"))
        .chain(["last.txt".into()])
        .collect::<Vec<_>>();
    for path in &paths {
        fs::write(root.join(path), "before\n").unwrap();
    }
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "Initial"]);
    for path in &paths {
        fs::write(root.join(path), "after\n").unwrap();
    }
    fs::write(
        root.join("folder/31.txt"),
        format!("{}\n", "long replacement line ".repeat(80)),
    )
    .unwrap();
    git(&root, &["add", "last.txt"]);
    fs::write(root.join("last.txt"), "working copy\n").unwrap();

    let platform = gpui_platform::current_platform(true);
    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        platform.text_system(),
    );
    cx.update(gpui_component::init);
    let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
    let config = crate::config::ConfigFile::load(Some(fixture.path().join("config.toml")));
    let (workspace, visual) = cx.add_window_view(|window, cx| {
        Workspace::with_config(store, config, WorkspaceView::Git, window, cx)
    });
    visual.simulate_resize(size(px(1280.), px(900.)));
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(Location { root, file: None }, window, cx);
        });
    });
    settle(visual);
    let panel = visual.update(|_, cx| workspace.read(cx).git.clone().unwrap());
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.show_history = false;
            panel.split = split;
            panel.wrap = split;
            panel.load_review(Selection::Worktree(Mode::All), false, window, cx);
            cx.notify();
        });
    });
    settle(visual);
    visual.update(|_, cx| {
        assert_eq!(
            panel.read(cx).diff.read(cx).mode(),
            if split {
                DiffMode::Split
            } else {
                DiffMode::Unified
            }
        );
    });

    let tree_viewport = |visual: &mut VisualTestContext| {
        visual.update(|_, cx| {
            let scroll = panel.read(cx).tree_scroll.0.borrow();
            (scroll.base_handle.bounds(), scroll.base_handle.offset().y)
        })
    };
    let check_target = |visual: &mut VisualTestContext, path, selector| {
        let viewport = visual.debug_bounds("git-diff-editor").unwrap();
        let header = visual
            .debug_bounds(selector)
            .expect("target header is visible");
        assert!(
            (header.top() - viewport.top()).abs() < px(0.5),
            "{path} header at {}, viewport at {}",
            header.top(),
            viewport.top(),
        );
        visual.update(|_, cx| {
            let panel = panel.read(cx);
            let review = panel.review.as_ref().unwrap();
            assert_eq!(panel.active_file(cx), Some(review.by_path[path]));
            assert_eq!(
                panel.diff.read(cx).top_file().map(SharedString::as_str),
                Some(path)
            );
            assert!(panel.pending_file_reveal.is_none());
            if split {
                let row = panel.diff.read(cx).editor_navigation_row();
                let original = panel.editor.original_state().read(cx);
                let header = original.projected_line_bounds(row).unwrap();
                assert!((header.top() - viewport.top()).abs() < px(0.5));
            }
        });
    };

    for (path, selector) in [
        ("last.txt", "git-editor-file-header:last.txt"),
        ("folder/31.txt", "git-editor-file-header:folder/31.txt"),
    ] {
        if path != "last.txt" {
            visual.update(|_, cx| {
                panel.update(cx, |panel, cx| {
                    panel.show_hunks = true;
                    App::notify(cx, panel.tree_pane.entity_id());
                });
            });
            settle(visual);
        }
        let row = visual.update(|_, cx| {
            panel.update(cx, |panel, cx| {
                let review = panel.review.as_ref().unwrap();
                let ix = review.by_path[path];
                assert!(matches!(review.patches[ix], PatchState::Pending), "{path}");
                let row = review
                    .tree_rows
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .file_row(ix)
                    .unwrap();
                // Hold both patch lanes to check layout before and after publication.
                panel.patch_loading = [true; 2];
                panel
                    .tree_scroll
                    .scroll_to_item(row, ScrollStrategy::Nearest);
                App::notify(cx, panel.tree_pane.entity_id());
                row
            })
        });
        settle(visual);
        let (viewport, offset) = tree_viewport(visual);
        let position = point(
            viewport.center().x,
            viewport.top() + offset + ROW_HEIGHT * row + ROW_HEIGHT.half(),
        );
        assert!(viewport.contains(&position));
        visual.simulate_click(position, Modifiers::default());
        for _ in 0..3 {
            draw_frame(visual);
            check_target(visual, path, selector);
            assert_eq!(
                tree_viewport(visual),
                (viewport, offset),
                "{path} while loading"
            );
        }

        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.patch_loading = [false; 2];
                panel.prefetch_patches(window, cx);
            });
        });
        for _ in 0..8 {
            visual.run_until_parked();
            draw_frame(visual);
            check_target(visual, path, selector);
            assert_eq!(
                tree_viewport(visual),
                (viewport, offset),
                "{path} after publication"
            );
        }
        visual.update(|_, cx| {
            let review = panel.read(cx).review.as_ref().unwrap();
            let ix = review.by_path[path];
            assert!(matches!(review.patches[ix], PatchState::Ready), "{path}");
            assert!(review.documents[ix].hunk_locations().next().is_some());
            if path == "last.txt" {
                let lines = review.documents[ix]
                    .patch_source_lines()
                    .map(|(_, _, text)| text)
                    .collect::<Vec<_>>();
                assert_eq!(lines, ["before", "working copy"]);
            }
        });
    }

    let viewport = visual.debug_bounds("git-diff-editor").unwrap();
    let position = point(viewport.right() - px(100.), viewport.center().y);
    visual.simulate_event(ScrollEvent {
        position,
        delta: ScrollDelta::Pixels(point(px(0.), px(600.))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
        source: ScrollSource::Pointer,
    });
    settle(visual);
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        let review = panel.review.as_ref().unwrap();
        let top = panel.diff.read(cx).top_file().unwrap();
        assert_ne!(top.as_str(), "folder/31.txt");
        assert_eq!(panel.active_file(cx), Some(review.by_path[top.as_str()]));
    });
}

#[::core::prelude::v1::test]
fn unified_tree_click_keeps_file_header_and_tree_position() {
    check_file_tree_navigation(false);
}

#[::core::prelude::v1::test]
fn split_tree_click_keeps_file_header_and_tree_position() {
    check_file_tree_navigation(true);
}

#[::core::prelude::v1::test]
fn hunk_arrows_reveal_the_adjacent_label_in_both_layouts() {
    use gpui_component::diff::DiffEditorRow;

    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-qb", "main"]);
    git(&root, &["config", "user.name", "Reviewer"]);
    git(&root, &["config", "user.email", "reviewer@example.invalid"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    let original = (0..120)
        .map(|ix| format!("line {ix}\n"))
        .collect::<String>();
    let path = "long.txt";
    fs::write(root.join(path), &original).unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "Initial"]);
    let staged = original.replace("line 2\n", "staged\n");
    fs::write(root.join(path), &staged).unwrap();
    git(&root, &["add", "."]);
    fs::write(root.join(path), staged.replace("line 110\n", "changed\n")).unwrap();

    let platform = gpui_platform::current_platform(true);
    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        platform.text_system(),
    );
    cx.update(gpui_component::init);
    let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
    let config = crate::config::ConfigFile::load(Some(fixture.path().join("config.toml")));
    let (workspace, visual) = cx.add_window_view(|window, cx| {
        Workspace::with_config(store, config, WorkspaceView::Git, window, cx)
    });
    visual.simulate_resize(size(px(640.), px(360.)));
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(Location { root, file: None }, window, cx);
        });
    });
    visual.run_until_parked();
    let panel = visual.update(|_, cx| workspace.read(cx).git.clone().unwrap());
    for split in [false, true] {
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.show_history = false;
                panel.show_tree = false;
                panel.split = split;
                panel.load_review(Selection::Worktree(Mode::All), false, window, cx);
                cx.notify();
            });
        });
        settle(visual);
        let (first, second) = visual.update(|_, cx| {
            let diff = panel.read(cx).diff.read(cx);
            let rows = diff
                .editor_rows()
                .enumerate()
                .filter_map(|(row, item)| matches!(item, DiffEditorRow::Hunk(..)).then_some(row))
                .collect::<Vec<_>>();
            assert_eq!(rows.len(), 2);
            (rows[0], rows[1])
        });
        let next: &'static str = Box::leak(format!("git-next-hunk:{first}").into_boxed_str());
        let previous: &'static str =
            Box::leak(format!("git-previous-hunk:{second}").into_boxed_str());
        for (click, target) in [(next, previous), (previous, next)] {
            let button = visual
                .debug_bounds(click)
                .expect("the arrow is in the viewport");
            assert!(button.size.width >= px(24.));
            visual.simulate_click(button.center(), Modifiers::default());
            settle(visual);
            let viewport = visual.debug_bounds("git-diff-editor").unwrap();
            let destination = visual
                .debug_bounds(target)
                .expect("the destination label is visible");
            assert!(viewport.contains(&destination.center()));
            assert!(
                destination.top() >= viewport.top() + px(27.),
                "split={split}, {click} -> {target}: {destination:?} in {viewport:?}"
            );
        }
    }
}

#[::core::prelude::v1::test]
fn narrow_worktree_summary_commits_and_expands_the_full_message_inline() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-qb", "main"]);
    git(&root, &["config", "user.name", "Reviewer"]);
    git(&root, &["config", "user.email", "reviewer@example.invalid"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    let file = "new.txt";
    fs::write(root.join(file), "staged content\n".repeat(40)).unwrap();
    git(&root, &["add", "."]);
    fs::write(root.join(file), "later worktree edit\n").unwrap();

    let platform = gpui_platform::current_platform(true);
    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        platform.text_system(),
    );
    cx.update(gpui_component::init);
    let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
    let config = crate::config::ConfigFile::load(Some(fixture.path().join("config.toml")));
    let (workspace, visual) = cx.add_window_view(|window, cx| {
        Workspace::with_config(store, config, WorkspaceView::Git, window, cx)
    });
    visual.simulate_resize(size(px(520.), px(600.)));
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
    visual.run_until_parked();
    let panel = visual.update(|_, cx| workspace.read(cx).git.clone().unwrap());
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.show_history = false;
            panel.show_tree = false;
            cx.notify();
        });
    });
    settle(visual);

    let toolbar = visual.debug_bounds("git-toolbar").unwrap();
    let field = visual.debug_bounds("git-commit-message-entry").unwrap();
    assert!(field.size.width >= px(140.));
    assert!(field.top() >= toolbar.bottom());
    let summary = visual.debug_bounds("git-review-summary").unwrap();
    assert!(summary.contains(&field.center()));
    let file_header = visual
        .debug_bounds("git-editor-file-header:new.txt")
        .unwrap();
    assert!((summary.bottom() - file_header.top()).abs() <= px(1.));

    let body = (0..42)
        .map(|ix| format!("Explanation {ix}\n"))
        .collect::<String>();
    visual.update(|window, cx| {
        panel
            .read(cx)
            .commit_message
            .clone()
            .update(cx, |input, cx| {
                input.set_value(format!("Initial commit\n\n{body}"), window, cx);
            });
        panel.update(cx, |_, cx| cx.notify());
    });
    settle(visual);
    let summary = visual.debug_bounds("git-review-summary").unwrap();
    assert_eq!(visual.debug_bounds("git-toolbar"), Some(toolbar));
    let field = visual.debug_bounds("git-commit-message-entry").unwrap();
    let viewport = visual.debug_bounds("git-diff-editor").unwrap();
    assert!(
        field.size.height > viewport.size.height,
        "{field:?} in {viewport:?}"
    );
    let button = visual.debug_bounds("git-create-commit").unwrap();
    assert!(summary.contains(&button.center()));
    let message = visual.update(|_, cx| panel.read(cx).commit_message.read(cx).input_bounds());
    assert!(button.top() >= message.bottom());
    assert!(field.bottom() - button.bottom() <= px(16.));
    visual.simulate_event(ScrollEvent {
        position: point(viewport.right() - px(8.), viewport.center().y),
        delta: ScrollDelta::Pixels(point(px(0.), viewport.bottom() - button.bottom() - px(24.))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
        source: ScrollSource::Pointer,
    });
    settle(visual);
    let button = visual.debug_bounds("git-create-commit").unwrap();
    assert!(
        viewport.contains(&button.center()),
        "{button:?} in {viewport:?}"
    );
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.detail_loading = true;
            cx.notify();
        });
        window.draw(cx).clear(cx);
    });
    visual.simulate_click(button.center(), Modifiers::default());
    visual.update(|_, cx| panel.update(cx, |panel, _| panel.detail_loading = false));
    settle(visual);
    let oid = visual.update(|_, cx| {
        let panel = panel.read(cx);
        assert!(panel.stage_error.is_none(), "{:?}", panel.stage_error);
        assert!(panel.commit_message.read(cx).value().is_empty());
        let commit = &panel.commits[0];
        assert_eq!(commit.subject.as_str(), "Initial commit");
        commit.oid.to_string()
    });
    assert_eq!(
        fs::read_to_string(root.join(file)).unwrap(),
        "later worktree edit\n"
    );
    let show = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["show", "HEAD:new.txt"])
        .output()
        .unwrap();
    assert_eq!(show.stdout, "staged content\n".repeat(40).into_bytes());

    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.load_review(Selection::Commit(oid, None), false, window, cx);
        });
    });
    settle(visual);
    let preview = visual
        .debug_bounds("git-commit-message")
        .expect("body is in the diff");
    let text_size = visual.update(|_, cx| crate::workspace::text_base_size(cx));
    let line_height = (text_size * 1.4).round();
    assert!(
        preview.size.height <= px(line_height * 8. + 1.),
        "{preview:?}"
    );
    let details = visual.debug_bounds("git-commit-details").unwrap();
    let compact_summary = visual.debug_bounds("git-review-summary").unwrap();
    assert!(compact_summary.contains(&details.center()));
    assert!(
        viewport.contains(&details.center()),
        "{details:?} in {viewport:?}"
    );
    visual.simulate_click(details.center(), Modifiers::default());
    settle(visual);
    let full = visual
        .debug_bounds("git-full-commit-message")
        .expect("details show the full body");
    assert!(full.size.height > preview.size.height);
    assert_eq!(full.top(), preview.top());
    assert!(
        visual
            .debug_bounds("git-review-summary")
            .unwrap()
            .size
            .height
            > compact_summary.size.height
    );
    assert!(visual.debug_bounds("git-commit-message").is_none());
    visual.simulate_click(details.center(), Modifiers::default());
    settle(visual);
    assert_eq!(visual.debug_bounds("git-commit-message"), Some(preview));
}
