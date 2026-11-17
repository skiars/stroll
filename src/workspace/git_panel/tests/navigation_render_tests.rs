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
                assert!(review.secondary.contains_key(&ix));
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
