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
fn diff_toolbar_navigates_in_unified_split_and_single_file_modes() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-qb", "main"]);
    fs::write(root.join("first.txt"), "first change\n".repeat(120)).unwrap();
    fs::write(root.join("second.txt"), "second change\n".repeat(120)).unwrap();

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
            panel.split = false;
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
    let scroll_down = |visual: &mut VisualTestContext| {
        let viewport = visual.debug_bounds("git-diff-editor").unwrap();
        visual.simulate_event(ScrollEvent {
            position: point(viewport.left() + px(90.), viewport.center().y),
            delta: ScrollDelta::Pixels(point(px(0.), px(-950.))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
            source: ScrollSource::Pointer,
        });
        settle(visual);
    };
    let back_to_top = |visual: &mut VisualTestContext, split: bool, smooth, reduce_motion| {
        visual.update(|_, cx| {
            cx.set_smooth_scrolling(smooth);
            cx.set_reduce_motion(reduce_motion);
        });
        let offsets = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| {
                let editor = &panel.read(cx).editor;
                (
                    editor.original_state().read(cx).scroll_offset().y,
                    editor.modified_state().read(cx).scroll_offset().y,
                )
            })
        };
        scroll_down(visual);
        let (left, right) = offsets(visual);
        assert!(right < px(0.));
        if split {
            assert_eq!(left, right);
        }
        let button = visual.debug_bounds("git-diff-back-to-top").unwrap();
        visual.simulate_click(button.center(), Modifiers::default());
        let (before_left, before_right) = (left, right);
        if smooth && !reduce_motion {
            let mut saw_split_motion = false;
            for _ in 0..3 {
                draw_frame(visual);
                let (left, right) = offsets(visual);
                saw_split_motion |=
                    before_left < left && left < px(0.) && before_right < right && right < px(0.);
            }
            let (_, right) = offsets(visual);
            assert!(before_right < right && right < px(0.));
            if split {
                assert!(saw_split_motion);
            }
        } else {
            draw_frame(visual);
            let (left, right) = offsets(visual);
            assert_eq!(right, px(0.));
            if split {
                assert_eq!(left, px(0.));
            }
        }
        settle(visual);
        let (left, right) = offsets(visual);
        assert_eq!(right, px(0.));
        if split {
            assert_eq!(left, px(0.));
        }
        let viewport = visual.debug_bounds("git-diff-editor").unwrap();
        let summary = visual.debug_bounds("git-review-summary").unwrap();
        assert!((summary.top() - viewport.top()).abs() < px(1.));
    };

    let back_button = visual.debug_bounds("git-diff-back-to-top").unwrap();
    let single_file_button = visual.debug_bounds("git-diff-single-file").unwrap();
    assert!(back_button.right() <= single_file_button.left());

    assert_eq!(paths(visual), ["first.txt", "second.txt"]);
    back_to_top(visual, false, true, false);
    back_to_top(visual, false, false, false);
    back_to_top(visual, false, true, true);
    click(visual, "git-diff-single-file");
    assert_eq!(paths(visual), ["first.txt"]);
    click(visual, "git-tree-row:file:second.txt");
    assert_eq!(paths(visual), ["second.txt"]);
    back_to_top(visual, false, true, false);
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

    click(visual, "git-reading-options");
    visual.simulate_keystrokes("down down enter");
    settle(visual);
    assert!(visual.update(|_, cx| panel.read(cx).split));
    back_to_top(visual, true, true, false);

    let left = visual.update(|window, cx| {
        let left = panel.read(cx).editor.original_state().clone();
        left.update(cx, |editor, cx| editor.focus(window, cx));
        left
    });
    settle(visual);
    back_to_top(visual, true, true, false);
    click(visual, "git-reading-options");
    visual.simulate_keystrokes("down down down down enter");
    settle(visual);
    assert!(visual.update(|window, cx| {
        gpui_component::input::Editor::search_has_focus(&left, window, cx)
    }));

    visual.simulate_keystrokes("escape");
    visual.simulate_resize(size(px(760.), px(850.)));
    settle(visual);
    assert!(visual.debug_bounds("git-diff-split").is_none());
    assert!(visual.debug_bounds("git-diff-back-to-top").is_some());
    click(visual, "git-diff-single-file");
    assert_eq!(paths(visual).len(), 1);
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

    let click = |visual: &mut VisualTestContext, selector| {
        let center = visual.debug_bounds(selector).unwrap().center();
        visual.simulate_click(center, Modifiers::default());
        settle(visual);
    };
    click(visual, "git-tree-row:folder:folder");
    click(visual, "git-reading-options");
    visual.simulate_keystrokes("down down down down down down down enter");
    settle(visual);
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        let diff = panel.diff.read(cx);
        let saved = panel.session(cx).unwrap();
        for path in [visible, hidden, untracked, hidden_untracked] {
            assert!(diff.is_file_collapsed(path), "{path}");
            assert!(saved.worktree_collapsed_files.contains(path), "{path}");
        }
        assert!(!diff.is_file_collapsed(deleted));
    });
    click(visual, "git-tree-row:folder:folder");
    visual.update(|_, cx| {
        assert!(panel.read(cx).diff.read(cx).is_file_collapsed(hidden));
        panel.update(cx, |panel, cx| panel.set_files_collapsed(false, cx));
    });
    settle(visual);

    let open_action = |visual: &mut VisualTestContext, row, keys| {
        let pos = visual.debug_bounds(row).unwrap().center();
        visual.simulate_mouse_down(pos, MouseButton::Right, Modifiers::default());
        visual.simulate_mouse_up(pos, MouseButton::Right, Modifiers::default());
        settle(visual);
        assert!(visual.debug_bounds("popup-menu-separator").is_some());
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

    let check_toolbar = |visual: &mut VisualTestContext, history_visible, tree_visible| {
        let toolbar = visual.debug_bounds("git-toolbar").unwrap();
        let history = visual.debug_bounds("git-history-toolbar");
        let tree = visual.debug_bounds("git-tree-toolbar");
        assert_eq!(history.is_some(), history_visible);
        assert_eq!(tree.is_some(), tree_visible);
        if let Some(history) = history {
            assert!(history.right() <= toolbar.left());
            assert_eq!(history.top(), toolbar.top());
            assert_eq!(history.bottom(), toolbar.bottom());
        }
        if let Some(tree) = tree {
            assert!(toolbar.right() <= tree.left());
            assert_eq!(tree.top(), toolbar.top());
            assert_eq!(tree.bottom(), toolbar.bottom());
        }
        let history_toggle = visual.debug_bounds("git-toggle-history").unwrap();
        let files_toggle = visual.debug_bounds("git-toggle-files").unwrap();
        assert!((px(0.)..=px(12.)).contains(&(history_toggle.left() - toolbar.left())));
        let files_band = tree.unwrap_or(toolbar);
        assert!((px(0.)..=px(12.)).contains(&(files_band.right() - files_toggle.right())));
        assert!(files_toggle.left() >= files_band.left());
        assert_eq!(files_toggle.center().y, files_band.center().y);
        assert!(visual.debug_bounds("git-hide-history").is_none());
        assert!(visual.debug_bounds("git-close-files").is_none());

        let history_remote = visual.debug_bounds("git-remote-menu");
        let center_remote = visual.debug_bounds("git-remote-menu-center");
        let (band, remote) = if let Some(history) = history {
            assert!(center_remote.is_none());
            (history, history_remote.unwrap())
        } else {
            assert!(history_remote.is_none());
            (toolbar, center_remote.unwrap())
        };
        assert!(remote.left() >= band.left() && remote.right() <= band.right());
    };

    let (workspace, visual) = make_window(&mut app, &store, &config);
    open(&workspace, visual, &root);
    check_toolbar(visual, true, true);
    visual.simulate_resize(size(px(760.), px(760.)));
    settle(visual);
    check_toolbar(visual, true, false);
    click(visual, "git-toggle-files");
    check_toolbar(visual, false, true);
    visual.simulate_resize(size(px(1280.), px(760.)));
    settle(visual);
    check_toolbar(visual, true, true);
    click(visual, "git-toggle-history");
    click(visual, "git-toggle-files");
    check_toolbar(visual, false, false);
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

    let row_cases = [
        (false, false, DiffSide::Modified, 34, false, true, 33),
        (true, false, DiffSide::Modified, 36, false, false, 35),
        (false, false, DiffSide::Original, 37, false, false, 37),
        (true, false, DiffSide::Original, 37, true, false, 37),
        (true, true, DiffSide::Original, 32, false, false, 32),
        (true, true, DiffSide::Original, 33, false, false, 33),
    ];
    for (split, history, side, line, opposite_padding, wrapped, expected) in row_cases {
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
        menu_click(visual, target, "down enter");
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
            assert_eq!(
                panel
                    .pending_editor_open
                    .as_ref()
                    .map(|request| request.path.as_str()),
                Some(guide)
            );
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

    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| workspace.toggle_vim_mode(window, cx));
    });
    for (split, history, side, line, opposite_padding, _, expected) in row_cases {
        show_diff(visual, split, history);
        let position = DiffLinePosition::new(path, side, line);
        let (column, caret) = visual.update(|window, cx| {
            let panel = panel.read(cx);
            let column = if split && (side == DiffSide::Original) != opposite_padding {
                panel.editor.original_state()
            } else {
                panel.editor.modified_state()
            }
            .clone();
            let bounds = panel.review_source_bounds(&position, cx).unwrap();
            let caret = column.update(cx, |editor, cx| {
                let row = editor
                    .visible_row_range()
                    .unwrap()
                    .find(|&row| editor.projected_line_bounds(row).unwrap().top() == bounds.top())
                    .unwrap();
                let caret = editor.text().line_start_offset(row);
                editor.set_selected_range(caret..caret, cx);
                editor.focus(window, cx);
                caret
            });
            (column, caret)
        });
        visual.simulate_keystrokes("g");
        assert!(visual.debug_bounds("vim-go").is_none());
        visual.executor().advance_clock(Duration::from_millis(300));
        settle(visual);
        assert!(visual.debug_bounds("vim-go-key-f").is_some());
        assert!(visual.debug_bounds("vim-go-key-D").is_none());
        visual.simulate_keystrokes("f");
        settle(visual);
        assert_editor(visual, path, expected);
        assert!(visual.debug_bounds("status-vim-mode: NORMAL").is_some());
        assert!(visual.debug_bounds("vim-go").is_none());

        visual.simulate_keystrokes("ctrl-o");
        settle(visual);
        visual.update(|window, cx| {
            assert_eq!(workspace.read(cx).view, WorkspaceView::Git);
            let editor = column.read(cx);
            assert!(editor.focus_handle(cx).is_focused(window));
            assert_eq!(editor.cursor(), caret);
        });
        visual.simulate_keystrokes("ctrl-i");
        settle(visual);
        assert_editor(visual, path, expected);
        assert!(visual.debug_bounds("status-vim-mode: NORMAL").is_some());
    }

    show_diff(visual, false, false);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.focus_vim_file_tree(window, cx));
    });
    visual.simulate_keystrokes("j g");
    visual.executor().advance_clock(Duration::from_millis(300));
    settle(visual);
    assert!(visual.debug_bounds("vim-go-key-f").is_some());
    assert!(visual.debug_bounds("vim-go-key-D").is_some());
    visual.simulate_keystrokes("f");
    settle(visual);
    assert_editor(visual, guide, 2);
    visual.simulate_keystrokes("ctrl-o");
    settle(visual);
    let assert_guide_in_tree = |visual: &mut VisualTestContext| {
        visual.update(|window, cx| {
            assert_eq!(workspace.read(cx).view, WorkspaceView::Git);
            let panel = panel.read(cx);
            assert!(panel.vim_tree_focus.is_focused(window));
            assert_eq!(panel.vim_tree_path.as_deref(), Some(guide));
        });
    };
    assert_guide_in_tree(visual);
    visual.simulate_keystrokes("ctrl-i");
    settle(visual);
    assert_editor(visual, guide, 2);

    visual.simulate_keystrokes("ctrl-o g shift-d");
    settle(visual);
    visual.update(|window, cx| {
        let owner = workspace.read(cx);
        assert_eq!(owner.view, WorkspaceView::Docs);
        let doc = owner.doc.as_ref().unwrap().read(cx);
        assert_eq!(doc.path, Path::new(guide));
        assert!(doc.content_focused(window, cx));
    });
    visual.simulate_keystrokes("ctrl-o");
    settle(visual);
    assert_guide_in_tree(visual);
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
    let paths = (0..100)
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
        cx.set_smooth_scrolling(true);
        cx.set_reduce_motion(false);
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
        draw_frame(visual);
        check_target(visual, path, selector);
        for _ in 0..8 {
            draw_frame(visual);
            assert_eq!(
                tree_viewport(visual),
                (viewport, offset),
                "{path} while loading"
            );
        }
        check_target(visual, path, selector);

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

    let reveal = |visual: &mut VisualTestContext, path| {
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.reveal_tree_file(path, window, cx));
        });
        draw_frame(visual);
    };
    let patch_state = |visual: &mut VisualTestContext, path| {
        visual.update(|_, cx| {
            let review = panel.read(cx).review.as_ref().unwrap();
            review.patches[review.by_path[path]].clone()
        })
    };
    let patch_delay = Duration::from_secs(1);
    let finish_patch = |visual: &mut VisualTestContext| {
        visual.executor().advance_clock(patch_delay);
        visual.run_until_parked();
    };
    visual.update(|_, cx| {
        panel.update(cx, |panel, _| {
            panel.patch_loading[0] = true;
            panel.review_read_delay = patch_delay;
        });
    });

    assert!(matches!(
        patch_state(visual, "folder/55.txt"),
        PatchState::Pending
    ));
    reveal(visual, "folder/55.txt");
    check_target(
        visual,
        "folder/55.txt",
        "git-editor-file-header:folder/55.txt",
    );
    assert!(matches!(
        patch_state(visual, "folder/55.txt"),
        PatchState::Loading
    ));
    reveal(visual, "folder/67.txt");
    check_target(
        visual,
        "folder/67.txt",
        "git-editor-file-header:folder/67.txt",
    );
    assert!(matches!(
        patch_state(visual, "folder/55.txt"),
        PatchState::Loading
    ));
    assert!(matches!(
        patch_state(visual, "folder/67.txt"),
        PatchState::Pending
    ));
    finish_patch(visual);
    draw_frame(visual);
    assert!(matches!(
        patch_state(visual, "folder/55.txt"),
        PatchState::Ready
    ));
    check_target(
        visual,
        "folder/67.txt",
        "git-editor-file-header:folder/67.txt",
    );
    finish_patch(visual);
    settle(visual);
    assert!(matches!(
        patch_state(visual, "folder/67.txt"),
        PatchState::Ready
    ));
    check_target(
        visual,
        "folder/67.txt",
        "git-editor-file-header:folder/67.txt",
    );

    assert!(matches!(
        patch_state(visual, "folder/80.txt"),
        PatchState::Pending
    ));
    reveal(visual, "folder/80.txt");
    check_target(
        visual,
        "folder/80.txt",
        "git-editor-file-header:folder/80.txt",
    );
    reveal(visual, "folder/31.txt");
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        assert!(matches!(
            panel
                .pending_file_reveal
                .as_ref()
                .map(|reveal| reveal.scroll),
            Some(FileRevealScroll::Animating),
        ));
    });
    assert!(matches!(
        patch_state(visual, "folder/80.txt"),
        PatchState::Loading
    ));
    finish_patch(visual);
    settle(visual);
    assert!(matches!(
        patch_state(visual, "folder/80.txt"),
        PatchState::Ready
    ));
    check_target(
        visual,
        "folder/31.txt",
        "git-editor-file-header:folder/31.txt",
    );

    assert!(matches!(
        patch_state(visual, "folder/85.txt"),
        PatchState::Pending
    ));
    reveal(visual, "folder/85.txt");
    check_target(
        visual,
        "folder/85.txt",
        "git-editor-file-header:folder/85.txt",
    );
    let viewport = visual.debug_bounds("git-diff-editor").unwrap();
    let anchor = |visual: &mut VisualTestContext| {
        visual.update(|_, cx| {
            let panel = panel.read(cx);
            let anchor = panel.diff.read(cx).viewport_anchor().unwrap();
            (
                anchor.path().to_owned(),
                anchor.line().cloned(),
                anchor.offset(),
            )
        })
    };
    visual.update(|window, cx| {
        use gpui::InputEvent as _;
        window.dispatch_event(
            ScrollEvent {
                position: point(viewport.right() - px(100.), viewport.center().y),
                delta: ScrollDelta::Pixels(point(px(0.), px(-460.))),
                modifiers: Modifiers::default(),
                touch_phase: TouchPhase::Moved,
                source: ScrollSource::Pointer,
            }
            .to_platform_input(),
            cx,
        );
    });
    for _ in 0..8 {
        draw_frame(visual);
    }
    let scrolled = anchor(visual);
    assert_ne!(scrolled.0, "folder/85.txt");
    assert!(matches!(
        patch_state(visual, "folder/85.txt"),
        PatchState::Loading
    ));
    finish_patch(visual);
    settle(visual);
    assert!(matches!(
        patch_state(visual, "folder/85.txt"),
        PatchState::Ready
    ));
    assert_eq!(anchor(visual), scrolled);
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        let review = panel.review.as_ref().unwrap();
        assert_eq!(panel.active_file(cx), Some(review.by_path[&scrolled.0]));
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
fn hunk_arrows_align_under_the_pointer_across_files_in_both_layouts() {
    use gpui_component::diff::{DiffEditorRow, DiffSide};

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
    let path = "a.txt";
    let binary = "b.bin";
    let last = "c.txt";
    let added = "new line\n".repeat(120);
    fs::write(root.join(path), &original).unwrap();
    fs::write(root.join(binary), b"base\0data").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "Initial"]);
    let staged = original.replace("line 2\n", "staged\n");
    let mixed = staged.replace("line 110\n", "changed\n");
    fs::write(root.join(path), &mixed).unwrap();
    fs::write(root.join(binary), b"changed\0data").unwrap();
    fs::write(root.join(last), &added).unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "Review"]);
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&root)
        .output()
        .unwrap();
    let commit = String::from_utf8(commit.stdout).unwrap().trim().to_owned();
    git(&root, &["reset", "--hard", "HEAD^"]);
    fs::write(root.join(path), &staged).unwrap();
    git(&root, &["add", path]);
    fs::write(root.join(path), &mixed).unwrap();
    fs::write(root.join(binary), b"changed\0data").unwrap();
    fs::write(root.join(last), &added).unwrap();

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
    for (split, selection) in [
        (false, Selection::Worktree(Mode::All)),
        (true, Selection::Commit(commit, None)),
    ] {
        let read_only = split;
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.show_history = false;
                panel.show_tree = false;
                panel.split = split;
                panel.only_matching = true;
                panel.filter = path.to_owned();
                panel.load_review(selection, true, window, cx);
                cx.notify();
            });
        });
        settle(visual);
        visual.update(|_, cx| {
            panel.update(cx, |panel, cx| {
                let review = panel.review.as_mut().unwrap();
                assert!(matches!(
                    review.patches[review.by_path[last]],
                    PatchState::Pending
                ));
                Arc::make_mut(&mut review.collapsed).insert(last.to_owned());
                panel.only_matching = false;
                panel.sync_documents(cx);
                cx.notify();
            });
        });
        settle(visual);
        let arrow = |visual: &mut VisualTestContext, file, hunk, direction| {
            let header = visual.update(|_, cx| {
                panel.read(cx).diff.read(cx).editor_rows().position(|row| {
                    matches!(row, DiffEditorRow::Hunk(path, index, _) if path.as_str() == file && index == hunk)
                }).unwrap()
            });
            let selector =
                Box::leak(format!("editor-git-{direction}-hunk:diff-{header}").into_boxed_str());
            visual.debug_bounds(selector)
        };
        let hover = visual.update(|_, cx| {
            let line = DiffLinePosition::new(path, DiffSide::Original, 3);
            let bounds = panel.read(cx).review_source_bounds(&line, cx).unwrap();
            point(bounds.left() + px(100.), bounds.center().y)
        });
        assert!(
            arrow(visual, path, 0, "next").is_none(),
            "the header has no arrows"
        );
        visual.simulate_mouse_move(hover, None, Modifiers::default());
        settle(visual);
        let next = arrow(visual, path, 0, "next").unwrap();
        assert_eq!(next.size.width, px(32.));
        if read_only {
            assert!(visual.debug_bounds("git-hunk-action:0").is_none());
            assert!(visual.debug_bounds("git-hunk-action:1").is_none());
        }
        let disabled = arrow(visual, path, 0, "previous").unwrap();
        visual.simulate_mouse_move(disabled.center(), None, Modifiers::default());
        visual.simulate_click(disabled.center(), Modifiers::default());
        settle(visual);
        assert!(
            arrow(visual, path, 0, "next")
                .unwrap()
                .contains(&next.center())
        );

        let next_click = point(next.center().x, next.top() + px(6.));
        visual.simulate_mouse_move(next_click, None, Modifiers::default());
        for (file, index) in [(path, 1), (last, 0)] {
            visual.simulate_click(next_click, Modifiers::default());
            settle(visual);
            let destination = arrow(visual, file, index, "next")
                .unwrap_or_else(|| panic!("missing {file} hunk {index} in split={split}"));
            assert!(destination.contains(&next_click));
            assert!(
                (destination.top() - next.top()).abs() < px(1.),
                "split={split}, {file} hunk {index}: {next:?} -> {destination:?}"
            );
        }
        visual.update(|_, cx| {
            let panel = panel.read(cx);
            assert!(panel.pending_hunk_navigation.is_none());
            assert!(!panel.diff.read(cx).is_file_collapsed(last));
            assert!(matches!(
                panel.review.as_ref().unwrap().patches
                    [panel.review.as_ref().unwrap().by_path[last]],
                PatchState::Ready
            ));
        });

        let previous = arrow(visual, last, 0, "previous").unwrap();
        let back_click = point(previous.center().x, next_click.y);
        visual.simulate_mouse_move(back_click, None, Modifiers::default());
        for (file, index) in [(path, 1), (path, 0)] {
            visual.simulate_click(back_click, Modifiers::default());
            settle(visual);
            let destination = arrow(visual, file, index, "previous").unwrap();
            assert!(destination.contains(&back_click));
            assert!((destination.top() - next.top()).abs() < px(1.));
        }
    }
}

#[::core::prelude::v1::test]
fn path_filter_keeps_the_tree_position_and_restores_it_when_cleared() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-qb", "main"]);
    for index in 0..64 {
        let tag = if index % 2 == 0 { "even" } else { "odd" };
        fs::write(root.join(format!("file-{index:02}-{tag}.txt")), "content\n").unwrap();
    }

    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    app.update(gpui_component::init);
    let store = Arc::new(Store::new(fixture.path().join("state")).unwrap());
    let config = crate::config::ConfigFile::load(Some(fixture.path().join("config.toml")));
    let (workspace, visual) = app.add_window_view(|window, cx| {
        Workspace::with_config(store, config, WorkspaceView::Git, window, cx)
    });
    visual.simulate_resize(size(px(1280.), px(720.)));
    visual.simulate_scale_factor_change(1.);
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(Location { root, file: None }, window, cx);
        });
    });
    settle(visual);
    let panel = visual.update(|_, cx| workspace.read(cx).git.clone().unwrap());
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| panel.set_tree_visible(true, cx));
    });
    settle(visual);

    let inset = px(4.);
    let expected = -21. * ROW_HEIGHT - inset;
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            let mut tree = panel.tree_scroll.0.borrow_mut();
            tree.deferred_scroll_to_item = None;
            tree.base_handle.set_offset(point(px(0.), expected));
            assert!(tree.base_handle.max_offset().y >= -expected);
            App::notify(cx, panel.tree_pane.entity_id());
        });
        assert_eq!(window.scale_factor(), 1.);
    });
    settle(visual);
    let filter = |visual: &mut VisualTestContext, query: &'static str, expected: Pixels| {
        visual.update(|window, cx| {
            let input = panel.read(cx).filter_input.clone();
            input.update(cx, |input, cx| input.replace_all(query, window, cx));
        });
        settle(visual);
        visual.update(|_, cx| {
            let panel = panel.read(cx);
            assert_eq!(panel.filter, query);
            assert_eq!(
                panel.tree_scroll.0.borrow().base_handle.offset().y,
                expected
            );
        });
    };
    filter(visual, "txt", expected);
    // The old top row was odd. Its nearest surviving successor is file 22.
    filter(visual, "even", -11. * ROW_HEIGHT - inset);
    filter(visual, "no-matches", px(0.));
    filter(visual, "", expected);
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
    visual.update(|window, cx| {
        let input = panel.read(cx).commit_message.clone();
        input.update(cx, |input, cx| {
            input.set_cursor_position(lsp_types::Position::new(44, 0), window, cx);
        });
    });
    settle(visual);
    visual.simulate_keystrokes("enter");
    settle(visual);
    visual.update(|_, cx| {
        let input = panel.read(cx).commit_message.read(cx);
        assert_eq!(input.cursor_position().line, 45);
        let caret = input.prepaint_caret_geometry().unwrap();
        let bottom = caret.cursor_bounds().bottom() + caret.scroll_offset().y;
        assert!(
            bottom <= viewport.bottom() + px(0.5),
            "{bottom:?} in {viewport:?}"
        );
        assert!(bottom >= viewport.bottom() - caret.line_height());
    });
    let button = visual.debug_bounds("git-create-commit").unwrap();
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

#[::core::prelude::v1::test]
fn git_ref_picker_keeps_checkout_first_and_browses_remote_refs_and_annotated_tags() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-qb", "current"]);
    git(&root, &["commit", "--allow-empty", "-qm", "Tagged commit"]);
    git(&root, &["tag", "-am", "Release", "release/v1"]);
    git(&root, &["branch", "a-first"]);
    git(&root, &["branch", "z-recent"]);
    let linked = fixture.path().join("linked");
    git(
        &root,
        &["worktree", "add", "-q", linked.to_str().unwrap(), "a-first"],
    );
    git(&root, &["remote", "add", "origin", "."]);
    git(&root, &["update-ref", "refs/remotes/origin/other", "HEAD"]);
    git(&root, &["commit", "--allow-empty", "-qm", "New work"]);

    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    app.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
    });
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
    visual.simulate_resize(size(px(960.), px(600.)));
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

    let click = |visual: &mut VisualTestContext, selector| {
        let position = visual
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing {selector}"))
            .center();
        visual.simulate_click(position, Modifiers::default());
        settle(visual);
    };
    let assert_branch = |visual: &mut VisualTestContext, expected| {
        visual.update(|_, cx| {
            assert_eq!(
                workspace.read(cx).git.as_ref().unwrap().read(cx).branch,
                expected
            );
        });
    };
    click(visual, "repository-menu-trigger");
    let linked_row: &str =
        Box::leak(format!("worktree-info:{}", crate::paths::display(&linked)).into_boxed_str());
    let row = visual.debug_bounds(linked_row).unwrap();
    visual.simulate_mouse_move(row.center(), None, Modifiers::default());
    visual.executor().advance_clock(Duration::from_millis(350));
    visual.simulate_mouse_move(
        row.center() + point(px(4.), px(0.)),
        None,
        Modifiers::default(),
    );
    visual.executor().advance_clock(Duration::from_millis(180));
    draw_frame(visual);
    assert!(visual.debug_bounds("hover-copy-branch").is_some());
    settle(visual);
    let copy = visual.debug_bounds("hover-copy-branch").unwrap();
    assert_eq!(copy.size, size(px(24.), px(24.)));
    visual.simulate_mouse_move(copy.center(), None, Modifiers::default());
    visual.executor().advance_clock(Duration::from_millis(650));
    settle(visual);
    click(visual, "hover-copy-branch");
    assert_eq!(
        visual
            .read_from_clipboard()
            .and_then(|item| item.text())
            .as_deref(),
        Some("a-first")
    );
    assert!(visual.debug_bounds(linked_row).is_some());
    visual.simulate_keystrokes("escape");
    settle(visual);

    click(visual, "git-branch-picker");
    assert!(visual.debug_bounds("git-ref-picker-popup").is_some());
    let current = visual.debug_bounds("git-ref:refs/heads/current").unwrap();
    let first = visual.debug_bounds("git-ref:refs/heads/a-first").unwrap();
    assert_eq!(current.size.height, first.size.height);
    assert!(current.bottom() <= first.top());
    visual.simulate_mouse_move(first.center(), None, Modifiers::default());
    visual.executor().advance_clock(Duration::from_millis(550));
    settle(visual);
    click(visual, "hover-copy-branch");
    assert_eq!(
        visual
            .read_from_clipboard()
            .and_then(|item| item.text())
            .as_deref(),
        Some("a-first")
    );
    assert!(visual.debug_bounds("git-ref-picker-popup").is_some());
    assert_branch(visual, "refs/heads/current");
    assert!(
        visual
            .debug_bounds("git-ref-worktree:refs/heads/a-first")
            .is_some()
    );
    assert!(
        visual
            .debug_bounds("git-ref:refs/remotes/origin/other")
            .is_none()
    );
    assert!(
        visual
            .debug_bounds("git-ref:refs/tags/release/v1")
            .is_none()
    );
    click(visual, "git-ref-tags");
    assert!(
        visual
            .debug_bounds("git-ref:refs/tags/release/v1")
            .is_some()
    );
    click(visual, "git-ref-remote");
    assert!(
        visual
            .debug_bounds("git-ref:refs/remotes/origin/other")
            .is_some()
    );

    click(visual, "git-ref:refs/heads/z-recent");
    assert_branch(visual, "refs/heads/z-recent");
    click(visual, "git-branch-picker");
    let current = visual.debug_bounds("git-ref:refs/heads/current").unwrap();
    let recent = visual.debug_bounds("git-ref:refs/heads/z-recent").unwrap();
    let first = visual.debug_bounds("git-ref:refs/heads/a-first").unwrap();
    assert!(current.bottom() <= recent.top() && recent.bottom() <= first.top());

    visual.simulate_input("origin/other");
    settle(visual);
    assert!(
        visual
            .debug_bounds("git-ref:refs/remotes/origin/other")
            .is_some()
    );
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_branch(visual, "refs/remotes/origin/other");

    click(visual, "git-branch-picker");
    visual.simulate_input("no-such-reference");
    settle(visual);
    visual.simulate_keystrokes("ctrl-a");
    visual.simulate_input("release/v1");
    settle(visual);
    assert!(
        visual
            .debug_bounds("git-ref:refs/tags/release/v1")
            .is_some()
    );
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_branch(visual, "refs/tags/release/v1");
    visual.update(|_, cx| {
        let panel = workspace.read(cx).git.as_ref().unwrap().read(cx);
        assert_eq!(panel.commits[0].subject.as_ref(), "Tagged commit");
        assert_eq!(
            panel.session(cx).unwrap().recent_branches,
            ["refs/heads/z-recent"]
        );
    });
    let head = Command::new("git")
        .current_dir(&root)
        .args(["branch", "--show-current"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "current");
}

#[::core::prelude::v1::test]
fn git_management_removes_a_worktree_and_keeps_its_branch_in_the_branch_tab() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    let linked = fixture.path().join("linked");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-qb", "main"]);
    git(&root, &["commit", "--allow-empty", "-qm", "Initial"]);
    git(
        &root,
        &["worktree", "add", "-qb", "topic", linked.to_str().unwrap()],
    );

    let untracked = linked.join("local.txt");

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
    visual.simulate_resize(size(px(960.), px(480.)));
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
    let click = |visual: &mut VisualTestContext, selector| {
        let center = visual.debug_bounds(selector).unwrap().center();
        visual.simulate_click(center, Modifiers::default());
        settle(visual);
    };
    for mode in [
        gpui_component::ThemeMode::Light,
        gpui_component::ThemeMode::Dark,
    ] {
        visual.update(|_, cx| crate::workspace::apply_theme(mode, cx));
        settle(visual);
        let comparison = visual.debug_bounds("git-worktree-mode").unwrap();
        let management = visual.debug_bounds("git-management").unwrap();
        assert_eq!(comparison.top(), management.top());
        assert_eq!(comparison.bottom(), management.bottom());
        assert!(comparison.right() < management.left());

        click(visual, "git-management");
        assert!(visual.debug_bounds("git-management-trees").is_some());
        assert!(visual.debug_bounds("git-management-branches").is_some());
        let remove = visual.debug_bounds("git-worktree-remove:1").unwrap();
        visual.simulate_mouse_move(remove.center(), None, Modifiers::default());
        settle(visual);
        visual.update(|window, _| {
            let bounds = remove.scale(window.scale_factor());
            let danger = Palette::for_mode(mode).danger;
            assert!(window.painted_quads().iter().any(|quad| {
                quad.bounds == bounds && quad.background.as_solid() == Some(danger.alpha(0.12))
            }));
        });
        click(visual, "git-management-close");
    }

    click(visual, "git-management");
    let dialog_bounds = |visual: &mut VisualTestContext| visual.debug_bounds("dialog-0").unwrap();
    let origin = dialog_bounds(visual).origin;
    let assert_position = |bounds: Bounds<Pixels>| {
        assert_eq!(bounds.left(), origin.x);
        assert!((bounds.top() - origin.y).abs() < px(1.), "{bounds:?}");
    };
    click(visual, "git-worktree-remove:0");
    assert!(visual.debug_bounds("git-management-apply").is_none());
    click(visual, "git-worktree-remove:1");
    let clean_confirmation = dialog_bounds(visual);
    assert_position(clean_confirmation);
    assert!(
        visual
            .debug_bounds("git-worktree-removal-details")
            .is_some()
    );
    assert!(linked.is_dir());
    fs::write(&untracked, "untracked\n").unwrap();
    fs::write(root.join(".git/info/exclude"), "ignored.tmp\n").unwrap();
    let ignored = linked.join("ignored.tmp");
    fs::write(&ignored, "ignored\n").unwrap();
    for index in 0..12 {
        fs::write(linked.join(format!("extra-{index}.txt")), "untracked\n").unwrap();
    }
    click(visual, "git-management-apply");
    let updated_confirmation = dialog_bounds(visual);
    assert_position(updated_confirmation);
    assert!(updated_confirmation.size.height > clean_confirmation.size.height);
    assert!(untracked.is_file());
    assert!(
        visual
            .debug_bounds("git-worktree-removal-entries")
            .is_some()
    );
    click(visual, "git-management-cancel");
    assert_position(dialog_bounds(visual));
    assert!(untracked.is_file());
    assert!(ignored.is_file());
    click(visual, "git-worktree-remove:1");
    click(visual, "git-management-apply");
    assert_position(dialog_bounds(visual));
    assert!(!linked.exists());
    git(&root, &["show-ref", "--verify", "refs/heads/topic"]);

    click(visual, "git-management-branches");
    assert_position(dialog_bounds(visual));
    click(visual, "git-branch-delete:1");
    assert!(visual.debug_bounds("git-management-apply").is_none());
    assert!(
        !Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["show-ref", "--verify", "--quiet", "refs/heads/topic"])
            .status()
            .unwrap()
            .success()
    );

    let stale = fixture.path().join("stale");
    let saved = fixture.path().join("stale.stroll-saved");
    git(
        &root,
        &["worktree", "add", "--detach", stale.to_str().unwrap()],
    );
    git(&stale, &["commit", "--allow-empty", "-qm", "Detached work"]);
    fs::remove_file(stale.join(".git")).unwrap();
    fs::write(stale.join("local.txt"), "keep").unwrap();

    click(visual, "git-management-close");
    click(visual, "git-management");
    click(visual, "git-worktree-remove:1");
    assert!(
        visual
            .debug_bounds("git-worktree-removal-entries")
            .is_some()
    );
    assert!(stale.join("local.txt").is_file());
    click(visual, "git-management-cancel");
    assert_eq!(fs::read_to_string(stale.join("local.txt")).unwrap(), "keep");
    assert!(!saved.exists());
    click(visual, "git-worktree-remove:1");
    click(visual, "git-management-apply");
    assert!(!stale.exists());
    assert!(!saved.exists());
    assert!(visual.debug_bounds("git-worktree-remove:1").is_none());
}

#[::core::prelude::v1::test]
fn git_push_reviews_edits_to_the_destination_and_pull_runs_directly() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("repo");
    let origin = fixture.path().join("origin");
    let backup = fixture.path().join("backup");
    let peer = fixture.path().join("peer");
    for path in [&root, &origin, &backup] {
        fs::create_dir(path).unwrap();
    }
    git(&root, &["init", "-qb", "main"]);
    git(&origin, &["init", "--bare", "-qb", "main"]);
    git(&backup, &["init", "--bare", "-qb", "main"]);
    git(&root, &["commit", "--allow-empty", "-qm", "Initial"]);
    git(
        &root,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(
        &root,
        &["remote", "add", "backup", backup.to_str().unwrap()],
    );
    git(&root, &["push", "--set-upstream", "origin", "main"]);
    git(
        &root,
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            peer.to_str().unwrap(),
        ],
    );
    fs::write(peer.join("pulled.txt"), "Fast-forward change\n").unwrap();
    git(&peer, &["add", "pulled.txt"]);
    git(&peer, &["commit", "-qm", "Pull this commit"]);
    git(&peer, &["push", "origin", "main"]);

    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    app.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
    });
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
    visual.simulate_resize(size(px(1000.), px(480.)));
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
    let click = |visual: &mut VisualTestContext, selector| {
        let center = visual.debug_bounds(selector).unwrap().center();
        visual.simulate_click(center, Modifiers::default());
        settle(visual);
    };
    let (remote_menu, push_button) = if visual.debug_bounds("git-remote-menu").is_some() {
        ("git-remote-menu", "git-remote-menu-push")
    } else {
        ("git-remote-menu-center", "git-remote-menu-center-push")
    };
    let click_remote_menu = |visual: &mut VisualTestContext| {
        let split = visual.debug_bounds(remote_menu).unwrap();
        let push = visual.debug_bounds(push_button).unwrap();
        assert_eq!(split.size.height, px(24.));
        assert_eq!(push.top(), split.top());
        assert_eq!(push.bottom(), split.bottom());
        assert!(push.right() < split.right());
        visual.simulate_click(
            point(split.right() - px(8.), split.center().y),
            Modifiers::default(),
        );
        settle(visual);
        assert!(visual.debug_bounds("popup-menu-surface").is_some());
    };
    let menu_icons = Rc::new(std::cell::RefCell::new(BTreeSet::new()));
    visual.update(|_, cx| {
        let menu_icons = menu_icons.clone();
        gpui_component::set_icon_resolver(cx, move |path, _| {
            menu_icons.borrow_mut().insert(path.to_owned());
            None
        });
    });
    click_remote_menu(visual);
    for icon in [
        "cloud-download",
        "download",
        "git-compare-arrows",
        "link-2",
        "server",
    ] {
        assert!(
            menu_icons.borrow().contains(&format!("icons/{icon}.svg")),
            "{icon}"
        );
    }
    visual.update(|_, cx| gpui_component::set_icon_resolver(cx, |_, _| None));
    visual.simulate_keystrokes("down down enter");
    settle(visual);
    assert!(root.join("pulled.txt").is_file());
    assert!(visual.debug_bounds("git-management-apply").is_none());

    fs::write(root.join("local.txt"), "Local change\n").unwrap();
    git(&root, &["add", "local.txt"]);
    git(&root, &["commit", "-qm", "Push this commit"]);
    git(
        &peer,
        &[
            "push",
            backup.to_str().unwrap(),
            "HEAD:refs/heads/main",
            "HEAD:refs/heads/rel-archive",
        ],
    );
    git(
        &peer,
        &["commit", "--allow-empty", "-qm", "Alternate branch change"],
    );
    git(
        &peer,
        &["push", backup.to_str().unwrap(), "HEAD:refs/heads/release"],
    );
    visual.update(|window, cx| {
        let panel = workspace.read(cx).git.clone().unwrap();
        panel.update(cx, |panel, cx| panel.refresh(window, cx));
    });
    settle(visual);

    let select_branch_text = |visual: &mut VisualTestContext| {
        let branch = visual.debug_bounds("git-push-branch").unwrap();
        visual.simulate_click(
            point(branch.center().x, branch.bottom() - px(8.)),
            Modifiers::default(),
        );
        #[cfg(target_os = "macos")]
        visual.simulate_keystrokes("cmd-a");
        #[cfg(not(target_os = "macos"))]
        visual.simulate_keystrokes("ctrl-a");
    };
    let update_preview = |visual: &mut VisualTestContext, name| {
        select_branch_text(visual);
        visual.simulate_input(name);
        visual.run_until_parked();
        visual.executor().advance_clock(Duration::from_millis(500));
        settle(visual);
    };

    let push = visual.debug_bounds(push_button).unwrap().center();
    // Render before the test executor runs the first preview request.
    visual.update(|window, cx| {
        window.dispatch_event(
            PlatformInput::MouseDown(MouseDownEvent {
                position: push,
                button: MouseButton::Left,
                click_count: 1,
                ..Default::default()
            }),
            cx,
        );
        window.dispatch_event(
            PlatformInput::MouseUp(MouseUpEvent {
                position: push,
                button: MouseButton::Left,
                click_count: 1,
                ..Default::default()
            }),
            cx,
        );
    });
    visual.update(|window, cx| window.draw(cx).clear(cx));
    assert!(visual.debug_bounds("git-push-preview").is_none());
    let pending_footer = visual.debug_bounds("git-push-footer").unwrap();
    settle(visual);
    assert_eq!(visual.debug_bounds("git-push-footer"), Some(pending_footer));
    let push_origin = visual.debug_bounds("dialog-0").unwrap().origin;
    update_preview(visual, "preview-only");
    let hint = visual.debug_bounds("git-push-new-target").unwrap();
    let field = visual.debug_bounds("git-push-branch").unwrap();
    assert!(hint.left() >= field.right());
    assert!((hint.center().y - field.center().y).abs() < px(1.));
    update_preview(visual, "invalid branch");
    assert!(visual.debug_bounds("git-push-error").is_some());
    click(visual, "git-push-cancel");
    click(visual, "git-management");
    let management_origin = visual.debug_bounds("dialog-0").unwrap().origin;
    assert_eq!(management_origin.x, push_origin.x);
    assert!((management_origin.y - push_origin.y).abs() < px(1.));
    click(visual, "git-management-close");
    click(visual, push_button);
    click(visual, "git-push-cancel");
    // Reopening advances the modal layer id. Both mouse and keyboard completion must still work.
    click(visual, push_button);
    let dialog = visual.debug_bounds("dialog-0").unwrap();
    assert_eq!(dialog.size.width, px(620.));
    assert!(dialog.top() >= px(79.));
    let commit_panel = visual.debug_bounds("git-push-commits-panel").unwrap();
    let file_panel = visual.debug_bounds("git-push-files-panel").unwrap();
    assert!(commit_panel.size.width < file_panel.size.width);
    assert_eq!(commit_panel.top(), file_panel.top());
    assert_eq!(commit_panel.bottom(), file_panel.bottom());
    assert!(commit_panel.size.height >= px(120.));
    assert!(
        (visual.debug_bounds("git-push-force").unwrap().center().y
            - visual.debug_bounds("git-push-apply").unwrap().center().y)
            .abs()
            < px(1.)
    );
    assert!(
        (visual
            .debug_bounds("git-push-upstream-field")
            .unwrap()
            .center()
            .y
            - visual.debug_bounds("git-push-cancel").unwrap().center().y)
            .abs()
            < px(1.)
    );
    let remote = visual.debug_bounds("git-push-remote").unwrap();
    let arrow = visual.debug_bounds("git-push-target-arrow").unwrap();
    let field = visual.debug_bounds("git-push-branch").unwrap();
    assert!(remote.right() <= arrow.left() && arrow.right() <= field.left());
    assert!((remote.center().y - field.center().y).abs() < px(1.));
    let commit = visual.debug_bounds("git-push-commit:0").unwrap();
    visual.simulate_mouse_move(commit.center(), None, Modifiers::default());
    visual.run_until_parked();
    visual.executor().advance_clock(Duration::from_secs(1));
    settle(visual);
    visual.update(|window, cx| {
        let row = commit.scale(window.scale_factor());
        let fill: Background = cx.theme().popover.alpha(0.5).into();
        let tooltip = window
            .painted_quads()
            .into_iter()
            .rev()
            .find(|quad| quad.background == fill)
            .expect("the outgoing commit tooltip must be painted above the dialog");
        let scale = window.scale_factor();
        let left_offset = (tooltip.bounds.left().0 - row.left().0) / scale;
        let gap = (tooltip.bounds.top().0 - row.bottom().0) / scale;
        assert!(
            left_offset.abs() <= 1.,
            "tooltip left offset: {left_offset}"
        );
        assert!(
            (3. ..=5.).contains(&gap),
            "tooltip gap below the row: {gap}"
        );
    });
    assert!(visual.debug_bounds("git-push-file:0").is_some());
    assert!(visual.debug_bounds("git-push-overwrite-warning").is_none());

    click(visual, "git-push-remote");
    visual.simulate_keystrokes("down enter");
    settle(visual);
    let previous_preview = visual.debug_bounds("git-push-preview").unwrap();
    select_branch_text(visual);
    visual.simulate_input("rel");
    visual.run_until_parked();
    draw_frame(visual);
    assert_eq!(
        visual.debug_bounds("git-push-preview"),
        Some(previous_preview)
    );
    assert_eq!(visual.debug_bounds("dialog-0"), Some(dialog));
    assert!(
        visual
            .debug_bounds("git-push-branch-option:release")
            .is_some()
    );
    assert!(
        visual
            .debug_bounds("git-push-branch-option:rel-archive")
            .is_some()
    );
    click(visual, "git-push-branch-option:rel-archive");
    assert!(
        visual
            .debug_bounds("git-push-branch-option:release")
            .is_none()
    );
    assert!(visual.debug_bounds("git-push-overwrite-warning").is_none());
    select_branch_text(visual);
    visual.simulate_input("rel");
    visual.simulate_keystrokes("down down enter");
    settle(visual);
    let updated = visual.debug_bounds("dialog-0").unwrap();
    assert!((updated.top() - dialog.top()).abs() <= px(1.));
    assert!(updated.size.height > dialog.size.height);
    assert!(visual.debug_bounds("git-push-overwrite-warning").is_some());
    click(visual, "git-push-apply");
    assert!(visual.debug_bounds("git-push-dialog").is_some());
    click(visual, "git-push-force");
    click(visual, "git-push-apply");
    assert!(visual.debug_bounds("git-push-dialog").is_none());

    let tip = |repo: &Path, name| {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", name])
            .output()
            .unwrap();
        assert!(output.status.success());
        output.stdout
    };
    assert_eq!(tip(&backup, "release"), tip(&root, "main"));
    assert_ne!(tip(&backup, "main"), tip(&root, "main"));
    assert_ne!(tip(&origin, "main"), tip(&root, "main"));
}

#[::core::prelude::v1::test]
fn editor_git_submenu_opens_scoped_history_and_reveals_the_historical_path() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("repo");
    fs::create_dir(&root)?;
    git(&root, &["init", "-qb", "main"]);
    git(&root, &["config", "user.name", "History Test"]);
    git(&root, &["config", "user.email", "history@example.invalid"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    let path = PathBuf::from("current file.txt");
    fs::write(root.join("old file.txt"), "before\noriginal\nafter\n")?;
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "Add file"]);
    git(&root, &["mv", "old file.txt", "current file.txt"]);
    git(&root, &["commit", "-qm", "Rename file"]);
    fs::write(root.join(&path), "before\nchanged\nafter\n")?;
    git(&root, &["commit", "-qam", "Change selected line"]);

    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    app.update(gpui_component::init);
    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let config = crate::config::ConfigFile::load(Some(fixture.path().join("config.toml")));
    let mut workspace = None;
    let (_, visual) = app.add_window_view(|window, cx| {
        let view =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
        workspace = Some(view.clone());
        gpui_component::Root::new(view, window, cx)
    });
    let workspace = workspace.unwrap();
    visual.simulate_resize(size(px(1280.), px(760.)));
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(
                Location {
                    root,
                    file: Some(path.clone()),
                },
                window,
                cx,
            );
        });
    });
    settle(visual);
    let editor = visual.update(|_, cx| workspace.read(cx).files[&path].editor.clone());
    let text = "local\nbefore\nchanged\nafter\n";
    let menu = |visual: &mut VisualTestContext, selected: bool| {
        visual.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.focus(window, cx);
                if selected {
                    editor.set_value(text, window, cx);
                    editor.set_selected_range(0.."local\nbefore\nchanged\n".len(), cx);
                } else {
                    editor.set_selected_range(0..0, cx);
                }
            });
        });
        settle(visual);
        let hit = visual.update(|_, cx| editor.read(cx).range_to_bounds(&(0..3)).unwrap().center());
        visual.simulate_mouse_down(hit, MouseButton::Right, Modifiers::default());
        visual.simulate_mouse_up(hit, MouseButton::Right, Modifiers::default());
        settle(visual);
        visual.simulate_keystrokes("up enter");
        settle(visual);
        visual.simulate_keystrokes(if selected { "down enter" } else { "enter" });
        settle(visual);
    };

    menu(visual, true);
    let panel = visual.update(|_, cx| {
        assert_eq!(workspace.read(cx).view, WorkspaceView::Git);
        workspace.read(cx).git.clone().unwrap()
    });
    assert!(visual.debug_bounds("git-source-history-scope").is_some());
    assert!(visual.debug_bounds("git-source-history-skipped").is_some());
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        assert!(panel.history_error.is_none(), "{:?}", panel.history_error);
        assert_eq!(
            panel
                .commits
                .iter()
                .map(|row| row.subject.as_str())
                .collect::<Vec<_>>(),
            ["Change selected line", "Add file"],
        );
    });
    visual.simulate_keystrokes("down down");
    settle(visual);
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        assert_eq!(
            panel.diff.read(cx).top_file().map(SharedString::as_str),
            Some("old file.txt")
        );
    });
    let clear = visual
        .debug_bounds("git-clear-source-history")
        .unwrap()
        .center();
    visual.simulate_click(clear, Modifiers::default());
    settle(visual);
    assert!(visual.debug_bounds("git-source-history-scope").is_none());
    assert!(visual.debug_bounds("git-commit-search").is_some());

    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.switch_view(WorkspaceView::Editor, window, cx)
        });
    });
    settle(visual);
    menu(visual, false);
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        assert!(panel.history_error.is_none(), "{:?}", panel.history_error);
        assert_eq!(
            panel
                .commits
                .iter()
                .map(|row| row.subject.as_str())
                .collect::<Vec<_>>(),
            ["Change selected line", "Rename file", "Add file"],
        );
        assert_eq!(editor.read(cx).text(), text);
    });
    Ok(())
}
