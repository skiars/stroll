use super::*;
use crate::repository::{Location, Store};
use crate::workspace::WorkspaceView;
use gpui::{
    PlatformInput, ScrollDelta, ScrollEvent, ScrollSource, TestAppContext, TestDispatcher,
    TouchPhase, VisualTestContext,
};
use std::{fs, path::Path, process::Command, time::Instant};

fn command(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn setup(root: &Path) {
    command(root, &["init", "-qb", "main"]);
    command(root, &["config", "user.name", "Reviewer"]);
    command(root, &["config", "user.email", "reviewer@example.invalid"]);
    command(root, &["config", "commit.gpgsign", "false"]);
}

fn test_context() -> TestAppContext {
    let platform = gpui_platform::current_platform(true);
    let cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        platform.text_system(),
    );
    cx.update(gpui_component::init);
    cx
}

fn settle(visual: &mut VisualTestContext) {
    for _ in 0..5 {
        visual.run_until_parked();
        visual.update(|window, cx| {
            window.simulate_next_frame(cx);
            window.draw(cx).clear(cx);
        });
    }
}

fn open<'a>(
    cx: &'a mut TestAppContext,
    temp: &Path,
    root: &Path,
) -> (Entity<GitPanel>, &'a mut VisualTestContext) {
    let store = Arc::new(Store::new(temp.join("state")).unwrap());
    let config = crate::config::ConfigFile::load(Some(temp.join("config.toml")));
    let (workspace, visual) = cx.add_window_view(|window, cx| {
        Workspace::with_config(store, config, WorkspaceView::Git, window, cx)
    });
    visual.simulate_resize(size(px(1280.), px(720.)));
    visual.update(|window, cx| {
        window.activate_window();
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
    let panel = visual.update(|_, cx| workspace.read(cx).git.clone().unwrap());
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.show_history = false;
            panel.split = false;
            panel.wrap = false;
            cx.notify();
        })
    });
    settle(visual);
    (panel, visual)
}

fn report(
    panel: &Entity<GitPanel>,
    visual: &mut VisualTestContext,
    incoming: &DiffLinePosition,
    label: &str,
) {
    visual.update(|window, cx| {
        let panel = panel.read(cx);
        let diff = panel.diff.read(cx);
        let anchor = diff.viewport_anchor();
        let position = anchor.as_ref().map(|a| (a.path(), a.line().map(|line| line.line())));
        let editor = panel.editor.focused_column(window, cx).read(cx);
        let bounds = panel.review_source_bounds(incoming, cx).map(|b| b.top());
        let ready = panel.review.as_ref().map(|r| r.patches.iter().filter(|p| matches!(p, PatchState::Ready)).count());
        eprintln!("{label} focus={} anchor={position:?} y={bounds:?} offset={:?} visible={:?} ready={ready:?}", window.is_window_active(), editor.scroll_offset().y, editor.visible_row_range());
    });
}

#[::core::prelude::v1::test]
fn partial_stage_and_refocus_keep_reading_position() {
    for split in [false, true] {
        check_partial_stage_and_refocus(split);
    }
}

fn check_partial_stage_and_refocus(split: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir(&root).unwrap();
    setup(&root);
    for path in ["a.txt", "b.txt", "c.txt"] {
        let body = (1..=320)
            .map(|line| format!("original line {line}\n"))
            .collect::<String>();
        fs::write(root.join(path), body).unwrap();
    }
    command(&root, &["add", "."]);
    command(&root, &["commit", "-qm", "Initial"]);
    for path in ["a.txt", "b.txt", "c.txt"] {
        let body = (1..=320)
            .map(|line| {
                if (32..36).contains(&line) || (58..63).contains(&line) || (98..104).contains(&line)
                {
                    format!("modified line {line}\n")
                } else {
                    format!("original line {line}\n")
                }
            })
            .collect::<String>();
        fs::write(root.join(path), body).unwrap();
    }
    let mut cx = test_context();
    let (panel, visual) = open(&mut cx, tmp.path(), &root);
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.split = split;
            cx.notify();
        })
    });
    settle(visual);
    let source = DiffLinePosition::new("b.txt", DiffSide::Modified, 34);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            let ix = panel.review.as_ref().unwrap().by_path["b.txt"];
            panel.reveal_file(ix, true, window, cx);
        })
    });
    settle(visual);
    visual.update(|_, cx| {
        panel
            .read(cx)
            .diff
            .clone()
            .update(cx, |diff, cx| diff.scroll_to_line(source.clone(), cx))
    });
    settle(visual);
    let source_y = |position: &DiffLinePosition, visual: &mut VisualTestContext| {
        visual.update(|_, cx| {
            panel
                .read(cx)
                .review_source_bounds(position, cx)
                .unwrap()
                .top()
        })
    };
    let before = source_y(&source, visual);
    let middle = DiffLinePosition::new("b.txt", DiffSide::Modified, 58);
    visual.update(|window, cx| {
        let panel = panel.read(cx);
        let editor = panel.editor.focused_column(window, cx).read(cx);
        assert!(
            editor
                .input_bounds()
                .intersects(&panel.review_source_bounds(&middle, cx).unwrap())
        );
    });
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            let review = panel.review.as_ref().unwrap();
            assert_eq!(review.active, review.by_path.get("b.txt").copied());
            let operations = &review.operations[&review.by_path["b.txt"]];
            let blocks = &operations.primary.as_ref().unwrap().blocks;
            assert_eq!(blocks.len(), 3, "{blocks:?}");
            let edit = git::FileEdit {
                path: "b.txt".into(),
                version: operations.version.clone(),
                action: git::FileAction::Stage,
                block: blocks[1].target.clone(),
            };
            assert!(edit.block.is_some());
            panel.edit_worktree_file(edit, window, cx);
        })
    });
    settle(visual);
    let migrated = DiffLinePosition::new(secondary_path("b.txt"), DiffSide::Modified, 34);
    let stage_delta = (source_y(&migrated, visual) - before).abs();
    assert!(
        stage_delta <= px(1.),
        "stage, split={split}: {stage_delta:?}"
    );
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        let review = panel.review.as_ref().unwrap();
        assert!(panel.stage_error.is_none(), "{:?}", panel.stage_error);
        assert!(review.secondary.contains_key(&review.by_path["b.txt"]));
    });
    let source = DiffLinePosition::new(secondary_path("b.txt"), DiffSide::Modified, 100);
    visual.update(|_, cx| {
        panel
            .read(cx)
            .diff
            .clone()
            .update(cx, |diff, cx| diff.scroll_to_line(source.clone(), cx))
    });
    settle(visual);
    let before = source_y(&source, visual);
    let revision = visual.update(|_, cx| panel.read(cx).diff.read(cx).editor_row_revision());
    visual.update(|_, cx| {
        let workspace = panel.read(cx).workspace.upgrade().unwrap();
        workspace.update(cx, |workspace, _| workspace.repository_watch_active = false);
    });
    visual.deactivate_window();
    visual.update(|window, _| window.activate_window());
    settle(visual);
    let focus_delta = (source_y(&source, visual) - before).abs();
    assert!(
        focus_delta <= px(1.),
        "focus, split={split}: {focus_delta:?}"
    );
    visual.update(|_, cx| assert_eq!(panel.read(cx).diff.read(cx).editor_row_revision(), revision));

    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            let review = panel.review.as_ref().unwrap();
            let operations = &review.operations[&review.by_path["b.txt"]];
            let block = operations.primary.as_ref().unwrap().blocks[0]
                .target
                .clone();
            panel.edit_worktree_file(
                git::FileEdit {
                    path: "b.txt".into(),
                    action: git::FileAction::Unstage,
                    version: operations.version.clone(),
                    block,
                },
                window,
                cx,
            );
        })
    });
    settle(visual);
    let merged = DiffLinePosition::new("b.txt", DiffSide::Modified, 100);
    let unstage_delta = (source_y(&merged, visual) - before).abs();
    assert!(
        unstage_delta <= px(1.),
        "unstage, split={split}: {unstage_delta:?}"
    );
    eprintln!(
        "split={split}: stage={stage_delta:?}, refocus={focus_delta:?}, unstage={unstage_delta:?}"
    );
}

#[::core::prelude::v1::test]
fn worktree_watch_skips_ignored_paths_and_refreshes_tracked_edits_with_the_same_status() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    setup(&root);
    let path = "watched.txt";
    let ignored = Path::new("ignored.log");
    fs::write(root.join(".gitignore"), "*.log\nwatched.txt\n").unwrap();
    fs::write(root.join(path), "initial version\n").unwrap();
    command(&root, &["add", "."]);
    command(&root, &["add", "-f", path]);
    command(&root, &["commit", "-qm", "Initial"]);
    fs::write(root.join(path), "first external edit\n").unwrap();

    let mut cx = test_context();
    let (panel, visual) = open(&mut cx, temp.path(), &root);
    let has_text = |visual: &mut VisualTestContext, text: &str| {
        visual.update(|window, cx| {
            let panel = panel.read(cx);
            panel
                .editor
                .focused_column(window, cx)
                .read(cx)
                .text()
                .to_string()
                .contains(text)
        })
    };
    assert!(has_text(visual, "first external edit"));

    let detail = visual.update(|_, cx| panel.read(cx).review.as_ref().unwrap().detail.clone());
    fs::write(root.join(ignored), "ignored build output\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        visual.executor().advance_clock(crate::git::watch::DEBOUNCE);
        settle(visual);
        let workspace_saw_file = visual.update(|_, cx| {
            let panel = panel.read(cx);
            assert!(Arc::ptr_eq(&panel.review.as_ref().unwrap().detail, &detail));
            let workspace = panel.workspace.upgrade().unwrap();
            let workspace = workspace.read(cx);
            workspace
                .repository_snapshot
                .as_ref()
                .and_then(|repo| repo.children.get(Path::new("")))
                .is_some_and(|entries| entries.iter().any(|entry| entry.path == ignored))
        });
        if workspace_saw_file {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Workspace missed the ignored creation"
        );
        std::thread::sleep(Duration::from_millis(15));
    }

    fs::write(root.join(path), "second external edit\nanother line\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        visual.executor().advance_clock(crate::git::watch::DEBOUNCE);
        settle(visual);
        if has_text(visual, "second external edit") {
            break;
        }
        if Instant::now() >= deadline {
            visual.update(|_, cx| {
                let panel = panel.read(cx);
                let active = panel
                    .workspace
                    .upgrade()
                    .unwrap()
                    .read(cx)
                    .repository_watch_active;
                let review = panel.review.as_ref().unwrap();
                let text = review.documents[0].patch_source_lines().collect::<Vec<_>>();
                panic!(
                    "watch active={active}, git error={:?}, detail error={:?}, patch={text:?}",
                    panel.history_error, panel.detail_error
                );
            });
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    visual.update(|_, cx| assert_eq!(panel.read(cx).review.as_ref().unwrap().totals, (2, 1)));
}

#[::core::prelude::v1::test]
fn worktree_refresh_retains_stats_for_files_without_loaded_patches() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    setup(&root);
    let path = "tracked.txt";
    fs::write(root.join(path), "before\n").unwrap();
    command(&root, &["add", "."]);
    command(&root, &["commit", "-qm", "Initial"]);
    fs::write(root.join(path), "after\n").unwrap();

    let mut cx = test_context();
    let (panel, visual) = open(&mut cx, temp.path(), &root);
    let mut previous = visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            let previous = panel.review.take().unwrap();
            panel.refresh_review_paths(HashSet::from([path.into()]), window, cx);
            previous
        })
    });
    let ix = previous.by_path[path];
    previous.patches[ix] = PatchState::Pending;
    assert_eq!(previous.documents[ix].additions(), 1);
    assert_eq!(
        change_stat(&previous.stats, &previous.detail.files[ix]).counts,
        Some((1, 1))
    );

    settle(visual);
    visual.update(|_, cx| {
        panel.update(cx, |panel, _| {
            let review = panel.review.as_mut().unwrap();
            let ix = review.by_path[path];
            review.stats = Rc::default();
            review.counts = Rc::default();
            review.patches[ix] = PatchState::Pending;
            review.patch_display[ix] = PatchDisplay::default();
            review.documents[ix] = translated_placeholder(&review.detail.files[ix]).unwrap();
            review.reconcile_worktree(&mut previous, &HashSet::from([path.into()]));
            assert_eq!(
                change_stat(&review.stats, &review.detail.files[ix]).counts,
                Some((1, 1))
            );
            assert_eq!(review.totals, (1, 1));
            assert_eq!(review.documents[ix].additions(), 1);
            assert_eq!(review.patch_display[ix].truncated, Some(false));
        });
    });
}

#[::core::prelude::v1::test]
fn slow_worktree_reads_publish_during_continuous_writes_and_keep_open_snapshots() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    setup(&root);
    let path = "z-watched.txt";
    fs::write(root.join(path), "original\n").unwrap();
    command(&root, &["add", "."]);
    command(&root, &["commit", "-qm", "Initial"]);
    fs::write(root.join(path), "revision 1\n").unwrap();

    let mut cx = test_context();
    let (panel, visual) = open(&mut cx, temp.path(), &root);
    let delay = Duration::from_millis(350);
    let (backend, detail) = visual.update(|_, cx| {
        let panel = panel.read(cx);
        (
            panel.backend.clone(),
            panel.review.as_ref().unwrap().detail.clone(),
        )
    });
    let mut stale_file = detail.files[0].clone();
    stale_file.worktree = None;
    let (patch, _, assets) = review_patch(&backend, &detail, &stale_file, 3, false, None).unwrap();
    assert_eq!(patch.document.additions(), 1);
    assert!(assets.version.is_none() && assets.operations.is_none());
    visual.update(|window, cx| {
        let workspace = panel.read(cx).workspace.upgrade().unwrap();
        // Deliver filesystem events directly so the test controls their timing.
        workspace.update(cx, |workspace, _| {
            workspace.repository_watch = Task::ready(())
        });
        panel.update(cx, |panel, cx| {
            panel.review_read_delay = delay;
            panel.reveal_tree_file(path, window, cx);
        });
    });
    settle(visual);

    let notify = |visual: &mut VisualTestContext, path: &str| {
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.repository_changed(
                    git::watch::RepoChanges {
                        paths: [path.to_owned()].into(),
                        ..Default::default()
                    },
                    window,
                    cx,
                );
            })
        });
        settle(visual);
    };
    let progress = |visual: &mut VisualTestContext| {
        visual.update(|_, cx| {
            let panel = panel.read(cx);
            let review = panel.review.as_ref().unwrap();
            let ix = review.by_path[path];
            assert_eq!(review.patch_display[ix].truncated, Some(false));
            assert!(!panel.writes_busy(cx));
            (
                review.documents[ix].additions(),
                review.totals.0,
                !panel.repo_changes.is_empty(),
            )
        })
    };
    assert_eq!(progress(visual), (1, 1, false));

    let mut published = HashSet::new();
    let mut queued = false;
    for revision in 2..=24 {
        fs::write(
            root.join(path),
            format!("revision {revision}\n").repeat(revision),
        )
        .unwrap();
        notify(visual, path);
        let (patch, stat, pending) = progress(visual);
        assert!(patch > 0 && stat > 0);
        assert!(patch <= revision && stat <= revision);
        if patch > 1 && stat > 1 {
            published.insert(patch);
        }
        queued |= pending;
        visual.executor().advance_clock(Duration::from_millis(65));
        settle(visual);
    }
    assert!(queued, "Writes should accumulate while a read is pending");
    assert!(
        published.len() >= 2,
        "Updates should publish while writes are ongoing: {published:?}"
    );
    for _ in 0..4 {
        visual.executor().advance_clock(delay);
        settle(visual);
    }
    assert_eq!(progress(visual), (24, 24, false));

    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            let ix = panel.review.as_ref().unwrap().by_path[path];
            panel.open_layer_snapshot(ix, None, Side::New, 1, window, cx);
        })
    });
    settle(visual);
    let editor = visual.update(|_, cx| {
        panel
            .read(cx)
            .snapshot
            .as_ref()
            .unwrap()
            .editor
            .clone()
            .unwrap()
    });
    fs::write(root.join("a-added.txt"), "another changed file\n").unwrap();
    notify(visual, "a-added.txt");
    visual.executor().advance_clock(delay);
    settle(visual);
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        assert_eq!(
            panel.active_file(cx),
            Some(1),
            "The snapshot file moved in the tree"
        );
        assert!(panel.snapshot.as_ref().unwrap().editor.as_ref() == Some(&editor));
    });

    fs::write(root.join(path), "original\n").unwrap();
    notify(visual, path);
    visual.executor().advance_clock(delay);
    settle(visual);
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        assert_eq!(
            panel.active_file(cx),
            None,
            "The snapshot path left the worktree review"
        );
        let snapshot = panel.snapshot.as_ref().unwrap();
        assert_eq!(snapshot.detail.files[snapshot.file].path, path);
        assert!(snapshot.editor.as_ref() == Some(&editor));
        assert_eq!(
            editor.read(cx).text().to_string(),
            "revision 24\n".repeat(24)
        );
    });
}

#[::core::prelude::v1::test]
fn changed_worktree_status_keeps_both_diff_layers_readable_without_edit_targets() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    setup(root);
    let path = "layered.txt";
    fs::write(root.join(path), "original\n").unwrap();
    command(root, &["add", path]);
    command(root, &["commit", "-qm", "Initial"]);
    fs::write(root.join(path), "staged change\n").unwrap();
    command(root, &["add", path]);
    fs::write(root.join(path), "worktree change\n").unwrap();
    let backend = crate::repository::Repository::open_root(root.to_owned())
        .unwrap()
        .git_backend();
    let (detail, _) = backend
        .review_files(Comparison::Worktree {
            mode: Mode::All,
            base: None,
        })
        .unwrap();
    let mut file = detail.files[0].clone();
    file.worktree.as_mut().unwrap().code = '?';
    let (primary, secondary, assets) =
        review_patch(&backend, &detail, &file, 3, false, None).unwrap();
    let secondary = secondary.unwrap();
    assert!(
        primary
            .document
            .patch_source_lines()
            .any(|(_, _, text)| text == "staged change")
    );
    assert!(
        secondary
            .document
            .patch_source_lines()
            .any(|(_, _, text)| text == "worktree change")
    );
    assert!(assets.version.is_none() && assets.operations.is_none());
}

#[::core::prelude::v1::test]
fn custom_workspace_diff_edits_undoes_and_saves_against_the_selected_base() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    setup(&root);
    let name = "text.txt";
    let path = Path::new(name);
    fs::write(root.join(path), "comparison base\nshared\n").unwrap();
    command(&root, &["add", name]);
    command(&root, &["commit", "-qm", "Comparison base"]);
    command(&root, &["branch", "comparison-base"]);
    fs::write(root.join(path), "ordinary index\nshared\n").unwrap();
    command(&root, &["commit", "-qam", "Ordinary index"]);
    fs::write(root.join(path), "working content\nshared\n").unwrap();

    let mut cx = test_context();
    let (panel, visual) = open(&mut cx, temp.path(), &root);
    let branch = "refs/heads/comparison-base";
    let focus_line = |visual: &mut VisualTestContext, expected: &str| {
        visual.update(|window, cx| {
            let editor = panel.read(cx).editor.focused_column(window, cx).clone();
            editor.update(cx, |editor, cx| {
                let offset = editor.text().to_string().find(expected).unwrap();
                editor.set_selected_range(offset..offset, cx);
                editor.focus(window, cx);
            });
            window.draw(cx).clear(cx);
            assert_eq!(
                panel.read(cx).editing_path(window, cx).as_deref(),
                Some(path)
            );
        });
    };
    let wait_for_patch = |visual: &mut VisualTestContext, expected: &str| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            visual.executor().advance_clock(Duration::from_millis(100));
            settle(visual);
            let ready = visual.update(|window, cx| {
                let panel = panel.read(cx);
                let Some(review) = &panel.review else {
                    return false;
                };
                let Some(&ix) = review.by_path.get(name) else {
                    return false;
                };
                let workspace = panel.workspace.upgrade().unwrap();
                let editable = !panel.detail_loading
                    && review.openable[ix]
                    && review.versions.contains_key(&ix)
                    && matches!(review.patches[ix], PatchState::Ready)
                    && workspace.read(cx).files.contains_key(path);
                if !editable {
                    return false;
                }
                let mut source = review.documents[ix].patch_source_lines();
                let has_base = source.any(|(_, _, text)| text == "comparison base");
                let has_edited_row = source.any(|(_, _, text)| text == expected);
                assert!(review.operations.is_empty());
                has_base
                    && has_edited_row
                    && panel
                        .editor
                        .focused_column(window, cx)
                        .read(cx)
                        .text()
                        .to_string()
                        .contains(expected)
            });
            if ready {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Waiting for custom diff text {expected}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    for (arbitrary, start, prefix) in [
        (true, "working content", "arbitrary "),
        (false, "arbitrary working content", "branch "),
    ] {
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                if arbitrary {
                    let base = panel
                        .history
                        .as_ref()
                        .unwrap()
                        .branches
                        .iter()
                        .find(|item| item.reference == branch)
                        .unwrap()
                        .oid
                        .clone();
                    panel.load_review(
                        Selection::Compare {
                            base: Some(base),
                            target: None,
                            previous: Box::new(Selection::Worktree(Mode::All)),
                        },
                        false,
                        window,
                        cx,
                    );
                } else {
                    panel.select_branch(branch.to_owned(), window, cx);
                }
                panel.editor.focus(window, cx);
            });
        });
        wait_for_patch(visual, start);
        focus_line(visual, start);
        visual.simulate_input(prefix);
        let edited = format!("{prefix}{start}");
        wait_for_patch(visual, &edited);
        assert_eq!(
            fs::read_to_string(root.join(path)).unwrap(),
            format!("{start}\nshared\n")
        );
        focus_line(visual, &edited);
        visual
            .update(|window, cx| window.dispatch_action(Box::new(gpui_component::input::Undo), cx));
        wait_for_patch(visual, start);
        focus_line(visual, start);
        visual
            .update(|window, cx| window.dispatch_action(Box::new(gpui_component::input::Redo), cx));
        wait_for_patch(visual, &edited);
        focus_line(visual, &edited);
        visual.update(|window, cx| {
            window.dispatch_action(Box::new(crate::workspace::SaveFile), cx);
        });
        settle(visual);
        assert_eq!(
            fs::read_to_string(root.join(path)).unwrap(),
            format!("{edited}\nshared\n")
        );
        wait_for_patch(visual, &edited);
    }
}

#[::core::prelude::v1::test]
#[ignore = "manual release-mode timing probe for cold diffs"]
fn cold_bottom_probe() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir(&root).unwrap();
    setup(&root);
    command(&root, &["commit", "--allow-empty", "-qm", "Initial"]);
    let file_count = 42;
    for ix in 0..file_count {
        let body = (0..1050).map(|line| format!("file {ix} content at line {line} with enough words to shape twice over in a long enough editor line plus some extra content to wrap many of the lines\n")).collect::<String>();
        fs::write(root.join(format!("{ix:02}.txt")), body).unwrap();
    }
    let mut cx = test_context();
    let (panel, visual) = open(&mut cx, tmp.path(), &root);
    for ix in 0..file_count - 4 {
        visual
            .update(|window, cx| panel.update(cx, |panel, cx| panel.request_patch(ix, window, cx)));
        visual.run_until_parked();
    }
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.split = true;
            panel.wrap = false;
            cx.notify();
        })
    });
    settle(visual);
    let near = DiffLinePosition::new("37.txt", DiffSide::Modified, 1000);
    visual.update(|_, cx| {
        panel
            .read(cx)
            .diff
            .clone()
            .update(cx, |diff, cx| diff.scroll_to_line(near.clone(), cx))
    });
    settle(visual);
    let position = visual.update(|window, cx| {
        panel
            .read(cx)
            .editor
            .focused_column(window, cx)
            .read(cx)
            .input_bounds()
            .center()
    });
    report(&panel, visual, &near, "cold end before");
    let mut last_offset = None;
    let mut bottom_frames = 0;
    for frame in 0..28 {
        let event = ScrollEvent {
            position,
            delta: ScrollDelta::Pixels(point(px(0.), px(-5000.))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
            source: ScrollSource::Pointer,
        };
        visual.update(|window, cx| {
            window.dispatch_event(PlatformInput::Scroll(event), cx);
        });
        visual.executor().advance_clock(Duration::from_millis(17));
        let cost = visual.update(|window, cx| {
            let start = Instant::now();
            window.simulate_next_frame(cx);
            window.draw(cx).clear(cx);
            start.elapsed()
        });
        eprintln!("cold end frame {frame}: draw {cost:?}");
        report(&panel, visual, &near, "cold end sample");
        let offset = visual.update(|window, cx| {
            let panel = panel.read(cx);
            panel
                .editor
                .focused_column(window, cx)
                .read(cx)
                .scroll_offset()
                .y
        });
        if let Some(old) = last_offset {
            assert!(offset <= old, "scroll reversed at frame {frame}");
            bottom_frames = if offset == old { bottom_frames + 1 } else { 0 };
        }
        last_offset = Some(offset);
        let start = Instant::now();
        visual.run_until_parked();
        eprintln!("cold end frame {frame}: settle {:?}", start.elapsed());
    }
    assert!(
        bottom_frames >= 2,
        "scroll never reached the final boundary"
    );
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        let diff = panel.diff.read(cx);
        assert_eq!(diff.top_file().map(SharedString::as_str), Some("41.txt"));
        assert!(
            panel
                .review
                .as_ref()
                .unwrap()
                .patches
                .iter()
                .all(|patch| { matches!(patch, PatchState::Ready) })
        );
    });
}
