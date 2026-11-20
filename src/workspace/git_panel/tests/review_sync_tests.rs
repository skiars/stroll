use super::*;
use crate::repository::{Location, Store};
use crate::workspace::WorkspaceView;
use gpui::{
    PlatformInput, ScrollDelta, ScrollEvent, ScrollSource, TestAppContext, TestDispatcher,
    TouchPhase, VisualTestContext,
};
use std::{fs, path::Path, process::Command, time::Instant};

#[path = "edit_model_tests.rs"]
mod edit_model_tests;

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

fn wait_for_review(
    visual: &mut VisualTestContext,
    expected: &str,
    mut ready: impl FnMut(&mut VisualTestContext) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        visual.executor().advance_clock(Duration::from_millis(100));
        settle(visual);
        if ready(visual) {
            return;
        }
        assert!(Instant::now() < deadline, "Waiting for {expected}");
        std::thread::sleep(Duration::from_millis(10));
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

#[::core::prelude::v1::test]
fn file_association_highlights_ux_in_source_and_git_snapshots() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    setup(&root);
    let path = "main.ux";
    fs::write(
        root.join(path),
        "<template>\n  <text>old</text>\n</template>\n",
    )
    .unwrap();
    command(&root, &["add", path]);
    command(&root, &["commit", "-qm", "Initial"]);
    fs::write(
        root.join(path),
        "<template>\n  <text>new</text>\n</template>\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("config.toml"),
        "[files.associations]\n\"*.ux\" = \"html\"\n\"*.rs\" = \"missing-language\"\n",
    )
    .unwrap();

    let mut app = test_context();
    let (panel, visual) = open(&mut app, temp.path(), &root);
    let owner = visual.update(|_, cx| panel.read(cx).workspace.upgrade().unwrap());
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.load_review(Selection::Worktree(Mode::All), false, window, cx);
        });
    });
    wait_for_review(visual, "the UX patch", |visual| {
        visual.update(|_, cx| {
            panel.read(cx).review.as_ref().is_some_and(|review| {
                review.by_path.get(path).is_some_and(|&ix| {
                    matches!(review.patches[ix], PatchState::Ready)
                        && review.sources.get(&ix).is_some_and(|sources| {
                            sources.contains_key(&(path.into(), DiffSide::Original))
                        })
                })
            })
        })
    });
    let diff_source = visual.update(|_, cx| {
        let review = panel.read(cx).review.as_ref().unwrap();
        let editor = review.sources[&review.by_path[path]][&(path.into(), DiffSide::Original)]
            .editor()
            .clone();
        assert_eq!(editor.read(cx).language_name().as_str(), "html");
        editor
    });
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            let ix = panel.review.as_ref().unwrap().by_path[path];
            panel.open_layer_snapshot(ix, None, Side::Old, 1, window, cx);
        });
    });
    wait_for_review(visual, "the original UX snapshot", |visual| {
        visual.update(|_, cx| {
            panel
                .read(cx)
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.editor.as_ref())
                .is_some_and(|editor| {
                    assert_eq!(editor.read(cx).language_name().as_str(), "html");
                    true
                })
        })
    });
    visual.update(|window, cx| {
        owner.update(cx, |owner, cx| {
            assert!(
                owner
                    .associated_highlight(Path::new("example.rs"))
                    .is_none()
            );
            assert_eq!(
                owner
                    .source_language(Path::new("example.rs"), &gpui_component::input::Rope::new())
                    .as_str(),
                "rust",
            );
            owner.switch_view(WorkspaceView::Editor, window, cx);
            owner.open_file(path.into(), false, window, cx);
        });
    });
    wait_for_review(visual, "the UX source editor", |visual| {
        visual.update(|_, cx| {
            owner
                .read(cx)
                .files
                .get(Path::new(path))
                .is_some_and(|file| {
                    let editor = file.editor.read(cx);
                    assert_eq!(editor.language_name().as_str(), "text");
                    editor.display_map().is_fold_candidate(0)
                })
        })
    });
    let (editor, snapshot) = visual.update(|window, cx| {
        let editor = owner.read(cx).files[Path::new(path)].editor.clone();
        let snapshot = panel
            .read(cx)
            .snapshot
            .as_ref()
            .unwrap()
            .editor
            .clone()
            .unwrap();
        editor.update(cx, |editor, cx| {
            editor.edit_ranges(&[(19..22, "draft".into())], window, cx);
            editor.set_selected_range(19..24, cx);
        });
        (editor, snapshot)
    });
    let draft = visual.update(|_, cx| editor.read(cx).text().clone());
    crate::repository::atomic_write(&temp.path().join("config.toml"), b"[files.associations]\n")
        .unwrap();
    wait_for_review(
        visual,
        "the removed file association to refresh both editors",
        |visual| {
            visual.update(|_, cx| {
                let editor = editor.read(cx);
                snapshot.read(cx).language_name().as_str() == "text"
                    && diff_source.read(cx).language_name().as_str() == "text"
                    && !editor.display_map().is_fold_candidate(0)
            })
        },
    );
    visual.update(|_, cx| {
        assert_eq!(owner.read(cx).files[Path::new(path)].editor, editor);
        assert_eq!(editor.read(cx).text(), &draft);
        assert_eq!(editor.read(cx).selected_range(), 19..24);
        assert_eq!(
            panel.read(cx).snapshot.as_ref().unwrap().editor.as_ref(),
            Some(&snapshot)
        );
    });
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
fn hunk_toolbar_keeps_mixed_diff_in_place_across_repeated_toggles() {
    for split in [false, true] {
        check_hunk_toggles(split);
    }
}

fn check_hunk_toggles(split: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    setup(&root);
    let path = "b.txt";
    let original = (1..=130)
        .map(|line| format!("original line {line}\n"))
        .collect::<Vec<_>>();
    for file in ["a.txt", path, "c.txt"] {
        fs::write(root.join(file), original.concat()).unwrap();
    }
    command(&root, &["add", "."]);
    command(&root, &["commit", "-qm", "Initial"]);

    let mut disk = original.clone();
    disk.remove(99);
    disk[57] = "modified middle line\n".into();
    disk.insert(11, "additional early line\n".into());
    let disk = disk.concat();
    fs::write(root.join(path), &disk).unwrap();
    for file in ["a.txt", "c.txt"] {
        fs::write(root.join(file), format!("changed\n{}", original.concat())).unwrap();
    }

    let mut cx = test_context();
    let (panel, visual) = open(&mut cx, temp.path(), &root);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.split = split;
            let ix = panel.review.as_ref().unwrap().by_path[path];
            panel.reveal_file(ix, true, window, cx);
        })
    });
    settle(visual);
    let source_bounds = |visual: &mut VisualTestContext, position: &DiffLinePosition| {
        visual.update(|_, cx| panel.read(cx).review_source_bounds(position, cx).unwrap())
    };

    for (side, line) in [(DiffSide::Modified, 59), (DiffSide::Original, 100)] {
        let position = DiffLinePosition::new(path, side, line);
        visual.update(|_, cx| {
            panel
                .read(cx)
                .diff
                .clone()
                .update(cx, |diff, cx| diff.scroll_to_line(position.clone(), cx));
        });
        crate::workspace::finish_scroll_navigation(visual);
        let bounds = source_bounds(visual, &position);
        let revision = visual.update(|_, cx| panel.read(cx).diff.read(cx).editor_row_revision());
        let hover = point(bounds.left() + px(56.), bounds.center().y);
        for staged in [true, false, true, false] {
            let selector = if staged {
                "git-hunk-action:0"
            } else {
                "git-hunk-action:1"
            };
            visual.simulate_mouse_move(hover, None, Modifiers::default());
            visual.update(|window, cx| window.draw(cx).clear(cx));
            let button = visual.debug_bounds(selector).unwrap_or_else(|| {
                panic!("missing {selector}, split={split}, {side:?} line {line}")
            });
            visual.simulate_mouse_move(button.center(), None, Modifiers::default());
            visual.simulate_click(button.center(), Modifiers::default());
            visual.update(|window, cx| window.draw(cx).clear(cx));
            assert_eq!(source_bounds(visual, &position), bounds);
            settle(visual);
            assert_eq!(
                source_bounds(visual, &position),
                bounds,
                "split={split}, {side:?} {line}, stage={staged}"
            );
            visual.update(|_, cx| {
                let panel = panel.read(cx);
                assert!(panel.stage_error.is_none(), "{:?}", panel.stage_error);
                assert_eq!(panel.diff.read(cx).editor_row_revision(), revision);
                let review = panel.review.as_ref().unwrap();
                let status = &review.detail.status.as_ref().unwrap()[path];
                assert_eq!(status.staged, staged);
                assert!(status.unstaged);
            });
            let mut indexed = original.clone();
            if staged {
                if side == DiffSide::Modified {
                    indexed[57] = "modified middle line\n".into();
                } else {
                    indexed.remove(99);
                }
            }
            let output = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["show", ":b.txt"])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, indexed.concat().as_bytes());
            assert_eq!(fs::read_to_string(root.join(path)).unwrap(), disk);
        }
    }

    visual.update(|_, cx| {
        let workspace = panel.read(cx).workspace.upgrade().unwrap();
        workspace.update(cx, |workspace, _| workspace.repository_watch_active = false);
    });
    let position = DiffLinePosition::new(path, DiffSide::Original, 100);
    let before = source_bounds(visual, &position);
    visual.deactivate_window();
    visual.update(|window, _| window.activate_window());
    settle(visual);
    assert_eq!(source_bounds(visual, &position), before);
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
    let (patch, assets) = review_patch(&backend, &detail, &stale_file, 3, false, None).unwrap();
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
fn changed_worktree_status_keeps_net_diff_readable_without_edit_targets() {
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
    let (patch, assets) = review_patch(&backend, &detail, &file, 3, false, None).unwrap();
    let lines = patch
        .document
        .patch_source_lines()
        .map(|(_, _, text)| text)
        .collect::<Vec<_>>();
    assert_eq!(lines, ["original", "worktree change"]);
    assert!(assets.version.is_none() && assets.operations.is_none());
}

#[::core::prelude::v1::test]
fn inserting_a_blank_line_in_a_staged_new_file_refreshes_the_diff() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    setup(&root);
    command(&root, &["commit", "--allow-empty", "-qm", "Initial"]);
    let name = "package.sh";
    let path = Path::new(name);
    let shebang = "#!/usr/bin/env bash";
    let source = format!("{shebang}\nset -euo pipefail\n");
    fs::write(root.join(path), &source).unwrap();
    command(&root, &["add", name]);

    let mut cx = test_context();
    let (panel, visual) = open(&mut cx, temp.path(), &root);
    wait_for_review(visual, "the editable staged diff", |visual| {
        visual.update(|window, cx| {
            let editor = panel.read(cx).editor.focused_column(window, cx).clone();
            let Some(offset) = editor.read(cx).text().to_string().find(shebang) else {
                return false;
            };
            let caret = offset + shebang.len();
            editor.update(cx, |editor, cx| {
                editor.set_selected_range(caret..caret, cx);
                editor.focus(window, cx);
            });
            panel.read(cx).editing_path(window, cx).as_deref() == Some(path)
        })
    });

    visual.simulate_keystrokes("enter");
    wait_for_review(visual, "the refreshed live diff", |visual| {
        visual.update(|_, cx| {
            let review = panel.read(cx).review.as_ref().unwrap();
            review.live.contains_key(&review.by_path[name])
        })
    });
    let expected = format!("{shebang}\n\nset -euo pipefail\n");
    visual.update(|window, cx| {
        let panel = panel.read(cx);
        let editor = panel.editor.focused_column(window, cx).read(cx);
        assert!(editor.text().to_string().contains(&expected));
        let workspace = panel.workspace.upgrade().unwrap();
        let file = &workspace.read(cx).files[path];
        assert_eq!(file.committed.to_string(), expected);
    });
}

#[::core::prelude::v1::test]
fn local_diff_edits_stay_visible_and_editable_across_layout_and_filter_changes() {
    for split in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        setup(&root);
        command(&root, &["commit", "--allow-empty", "-qm", "Initial"]);
        let name = "a.sh";
        let path = Path::new(name);
        let shebang = "#!/usr/bin/env bash";
        let last = "echo done";
        fs::write(root.join(name), format!("{shebang}\n{last}\n")).unwrap();
        fs::write(root.join("b.sh"), "echo next file\n").unwrap();
        command(&root, &["add", "."]);

        let mut cx = test_context();
        let (panel, visual) = open(&mut cx, temp.path(), &root);
        let set_split = |visual: &mut VisualTestContext, split| {
            visual.update(|_, cx| {
                panel.update(cx, |panel, cx| {
                    panel.split = split;
                    cx.notify();
                });
            });
        };
        set_split(visual, split);
        let editor =
            visual.update(|window, cx| panel.read(cx).editor.focused_column(window, cx).clone());
        let focus_line_end = |visual: &mut VisualTestContext, line: &str| {
            visual.update(|window, cx| {
                let Some(start) = editor.read(cx).text().to_string().find(line) else {
                    return false;
                };
                let caret = start + line.len();
                editor.update(cx, |editor, cx| {
                    editor.set_selected_range(caret..caret, cx);
                    editor.focus(window, cx);
                });
                panel.read(cx).editing_path(window, cx).as_deref() == Some(path)
            })
        };
        let is_split = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| panel.read(cx).diff.read(cx).mode() == DiffMode::Split)
        };
        let header = "git-editor-file-header:b.sh";
        wait_for_review(visual, "both staged files", |visual| {
            visual.debug_bounds(header).is_some()
                && is_split(visual) == split
                && focus_line_end(visual, shebang)
        });
        let revision = visual.update(|_, cx| panel.read(cx).diff.read(cx).editor_row_revision());
        visual.simulate_keystrokes("enter enter");
        settle(visual);

        // Inspect the frame before the debounced patch has been published.
        let position = DiffLinePosition::new(name, DiffSide::Modified, 4);
        let last_line = visual.update(|_, cx| {
            let panel = panel.read(cx);
            assert_eq!(panel.diff.read(cx).editor_row_revision(), revision);
            panel.review_source_bounds(&position, cx).unwrap()
        });
        assert_eq!(
            visual.debug_bounds(header).unwrap().top(),
            last_line.bottom()
        );

        set_split(visual, !split);
        settle(visual);
        visual.simulate_input("echo draft");
        let expected = format!("{shebang}\n\necho draft\n{last}\n");
        visual.update(|_, cx| {
            assert!(editor.read(cx).text().to_string().contains(&expected));
        });
        wait_for_review(visual, "the requested layout and live patch", |visual| {
            is_split(visual) != split
                && visual.update(|_, cx| {
                    let review = panel.read(cx).review.as_ref().unwrap();
                    review.live.contains_key(&review.by_path[name])
                })
        });

        for mode in [Mode::Staged, Mode::All] {
            visual.update(|window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.load_review(Selection::Worktree(mode), false, window, cx);
                });
            });
            wait_for_review(visual, "the selected worktree filter", |visual| {
                visual.update(|_, cx| {
                    let panel = panel.read(cx);
                    panel.review.as_ref().is_some_and(|review| {
                        review.detail.comparison.worktree_mode() == Some(mode)
                            && matches!(review.patches[review.by_path[name]], PatchState::Ready)
                    })
                })
            });
        }
        wait_for_review(
            visual,
            "the editable draft after switching filters",
            |visual| focus_line_end(visual, "echo draft"),
        );
        visual.simulate_input(" kept");
        visual.update(|_, cx| {
            let workspace = panel.read(cx).workspace.upgrade().unwrap();
            assert_eq!(
                workspace.read(cx).files[path].committed.to_string(),
                expected.replace("echo draft", "echo draft kept")
            );
        });
        assert_eq!(
            fs::read_to_string(root.join(name)).unwrap(),
            format!("{shebang}\n{last}\n")
        );
    }
}

#[::core::prelude::v1::test]
fn typing_after_pending_diff_undo_or_redo_uses_the_visible_caret() {
    for (split, redo) in [(false, false), (false, true), (true, false), (true, true)] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        setup(&root);
        command(&root, &["commit", "--allow-empty", "-qm", "Initial"]);
        let name = "a.txt";
        let path = Path::new(name);
        let source = "first\nsecond line with words\n";
        fs::write(root.join(name), source).unwrap();
        command(&root, &["add", name]);
        let mut cx = test_context();
        let (panel, visual) = open(&mut cx, temp.path(), &root);
        visual.update(|_, cx| {
            panel.update(cx, |panel, cx| {
                panel.split = split;
                cx.notify();
            })
        });
        let editor =
            visual.update(|window, cx| panel.read(cx).editor.focused_column(window, cx).clone());
        let focus_line = |visual: &mut VisualTestContext, prefix: &str| {
            visual.update(|window, cx| {
                let Some(start) = editor.read(cx).text().to_string().find(prefix) else {
                    return false;
                };
                let caret = start + prefix.len();
                editor.update(cx, |editor, cx| {
                    editor.focus(window, cx);
                    editor.set_selected_range(caret..caret, cx);
                });
                panel.read(cx).editing_path(window, cx).as_deref() == Some(path)
            })
        };
        let assert_projection = |visual: &mut VisualTestContext, text: &str, prefix: &str| {
            visual.update(|_, cx| {
                let editor = editor.read(cx);
                let projected = editor.text().to_string();
                assert!(
                    projected.contains(text),
                    "split={split}, redo={redo}: {projected:?}"
                );
                assert!(
                    projected[..editor.cursor()].ends_with(prefix),
                    "split={split}, redo={redo}"
                );
            });
        };

        wait_for_review(visual, "editable diff", |visual| {
            focus_line(visual, "first")
        });
        visual.simulate_input(" draft");
        wait_for_review(visual, "published initial draft", |visual| {
            visual.update(|_, cx| {
                let panel = panel.read(cx);
                let review = panel.review.as_ref().unwrap();
                review.live.contains_key(&review.by_path[name])
            })
        });
        let revision = visual.update(|_, cx| panel.read(cx).diff.read(cx).editor_row_revision());

        assert!(focus_line(visual, "first draft"));
        visual.simulate_keystrokes("enter");
        assert!(focus_line(visual, "second line"));
        visual
            .update(|window, cx| window.dispatch_action(Box::new(gpui_component::input::Undo), cx));
        assert_projection(
            visual,
            "first draft\nsecond line with words\n",
            "second line",
        );
        if redo {
            visual.update(|window, cx| {
                window.dispatch_action(Box::new(gpui_component::input::Redo), cx)
            });
            assert_projection(
                visual,
                "first draft\n\nsecond line with words\n",
                "second line",
            );
        }

        visual.simulate_input("MARK");
        let gap = if redo { "\n\n" } else { "\n" };
        let expected = format!("first draft{gap}second lineMARK with words\n");
        assert_projection(visual, &expected, "second lineMARK");
        visual.update(|_, cx| {
            let panel = panel.read(cx);
            assert_eq!(panel.diff.read(cx).editor_row_revision(), revision);
            let workspace = panel.workspace.upgrade().unwrap();
            assert_eq!(
                workspace.read(cx).files[path].committed.to_string(),
                expected
            );
        });
        wait_for_review(visual, "rebuilt patch with the new input", |visual| {
            visual.update(|_, cx| panel.read(cx).diff.read(cx).editor_row_revision() != revision)
        });
        assert_projection(visual, &expected, "second lineMARK");
        assert_eq!(fs::read_to_string(root.join(name)).unwrap(), source);
    }
}

#[::core::prelude::v1::test]
fn pending_diff_replays_history_across_collapsed_context() {
    for split in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        setup(&root);
        let name = "text.txt";
        let path = Path::new(name);
        let context = (0..24)
            .map(|i| format!("context {i:02}\n"))
            .collect::<String>();
        fs::write(root.join(path), format!("old first\n{context}old last\n")).unwrap();
        command(&root, &["add", name]);
        command(&root, &["commit", "-qm", "Initial"]);
        let disk = format!("new first with words\n{context}new last with words\n");
        fs::write(root.join(path), &disk).unwrap();
        let mut cx = test_context();
        let (panel, visual) = open(&mut cx, temp.path(), &root);
        let editor = visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.split = split;
                cx.notify();
            });
            panel.read(cx).editor.focused_column(window, cx).clone()
        });
        let focus = |visual: &mut VisualTestContext, marker: &str| {
            focus_review_line(visual, &panel, &editor, path, marker, None)
        };
        wait_for_review(visual, "editable hunks", |visual| {
            focus(visual, "new first")
        });
        // One source-editor transaction changes both hunks and preserves the
        // omitted context between them. The first hunk also changes height.
        let draft = format!("draft first with words\nextra line\n{context}draft last with words\n");
        visual.update(|window, cx| {
            let workspace = panel.read(cx).workspace.upgrade().unwrap();
            let source = workspace.read(cx).files[path].editor.clone();
            source.update(cx, |source, cx| {
                source.edit_ranges(&[(0..disk.len(), draft.clone())], window, cx);
            });
        });
        wait_for_review(visual, "visible draft hunks", |visual| {
            focus(visual, "draft last")
        });
        let revision = visual.update(|_, cx| panel.read(cx).diff.read(cx).editor_row_revision());
        let check = |visual: &mut VisualTestContext, text: &str, caret: &str, pending: bool| {
            visual.update(|window, cx| {
                assert_review_history(window, cx, &panel, &editor, path, text, caret);
                let projected = editor.read(cx).text().to_string();
                let (first, last) = (text.lines().next().unwrap(), text.lines().last().unwrap());
                assert!(
                    projected.contains(first) && projected.contains(last),
                    "{projected:?}"
                );
                assert_eq!(
                    projected.contains("extra line"),
                    text.contains("extra line")
                );
                assert!(!projected.contains("context 12"));
                assert_eq!(
                    panel.read(cx).diff.read(cx).editor_row_revision() == revision,
                    pending,
                    "split={split}, caret after {caret:?}"
                );
            });
        };

        check(visual, &draft, "draft last", true);
        visual
            .update(|window, cx| window.dispatch_action(Box::new(gpui_component::input::Undo), cx));
        check(visual, &disk, "new last", true);
        visual
            .update(|window, cx| window.dispatch_action(Box::new(gpui_component::input::Redo), cx));
        check(visual, &draft, "draft last", true);
        visual.update(|window, cx| {
            window.dispatch_keystroke(gpui::Keystroke::parse("?").unwrap(), cx);
        });
        let expected = draft.replace("last with", "last? with");
        check(visual, &expected, "draft last?", true);
        wait_for_review(visual, "rebuilt draft patch", |visual| {
            visual.update(|_, cx| panel.read(cx).diff.read(cx).editor_row_revision() != revision)
        });
        check(visual, &expected, "draft last?", false);
        assert_eq!(fs::read_to_string(root.join(path)).unwrap(), disk);
    }
}

#[::core::prelude::v1::test]
fn pending_diff_undo_preserves_multiple_carets_for_typing() {
    for split in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        setup(&root);
        command(&root, &["commit", "--allow-empty", "-qm", "Initial"]);
        let name = "text.txt";
        let path = Path::new(name);
        let disk = "first with words\nsecond with words\nthird with words\n";
        fs::write(root.join(path), disk).unwrap();
        command(&root, &["add", name]);
        let mut cx = test_context();
        let (panel, visual) = open(&mut cx, temp.path(), &root);
        let editor = visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.split = split;
                cx.notify();
            });
            panel.read(cx).editor.focused_column(window, cx).clone()
        });
        wait_for_review(visual, "editable diff", |visual| {
            focus_review_line(visual, &panel, &editor, path, "third", None)
        });
        visual.simulate_input(" draft");
        wait_for_review(visual, "initial draft", |visual| {
            visual.update(|_, cx| {
                let review = panel.read(cx).review.as_ref().unwrap();
                review.live.contains_key(&review.by_path[name])
            })
        });
        let revision = visual.update(|_, cx| panel.read(cx).diff.read(cx).editor_row_revision());
        assert!(focus_review_line(
            visual,
            &panel,
            &editor,
            path,
            "third draft",
            None
        ));
        visual.simulate_keystrokes("enter");
        assert!(focus_review_line(
            visual,
            &panel,
            &editor,
            path,
            "first",
            Some("second")
        ));
        visual
            .update(|window, cx| window.dispatch_action(Box::new(gpui_component::input::Undo), cx));
        visual.simulate_input("?");

        let expected = "first? with words\nsecond? with words\nthird draft with words\n";
        let check = |visual: &mut VisualTestContext| {
            visual.update(|window, cx| {
                assert_review_history(window, cx, &panel, &editor, path, expected, "first?");
                assert!(editor.read(cx).text().to_string().contains(expected));
                panel.read(cx).diff.read(cx).editor_row_revision()
            })
        };
        assert_eq!(check(visual), revision);
        wait_for_review(visual, "rebuilt draft patch", |visual| {
            check(visual) != revision
        });
        assert_eq!(fs::read_to_string(root.join(path)).unwrap(), disk);
    }
}

fn focus_review_line(
    visual: &mut VisualTestContext,
    panel: &Entity<GitPanel>,
    editor: &Entity<EditorState>,
    path: &Path,
    marker: &str,
    second: Option<&str>,
) -> bool {
    visual.update(|window, cx| {
        let projected = editor.read(cx).text().to_string();
        let Some(caret) = projected.find(marker).map(|pos| pos + marker.len()) else {
            return false;
        };
        editor.update(cx, |editor, cx| {
            editor.focus(window, cx);
            editor.set_selected_range(caret..caret, cx);
            if let Some(marker) = second {
                let caret = projected.find(marker).unwrap() + marker.len();
                editor.add_selection(caret..caret, cx);
            }
        });
        panel.read(cx).editing_path(window, cx).as_deref() == Some(path)
    })
}

fn assert_review_history(
    window: &Window,
    cx: &App,
    panel: &Entity<GitPanel>,
    editor: &Entity<EditorState>,
    path: &Path,
    expected: &str,
    caret: &str,
) {
    let editor = editor.read(cx);
    let panel = panel.read(cx);
    let workspace = panel.workspace.upgrade().unwrap();
    assert_eq!(
        workspace.read(cx).files[path].committed.to_string(),
        expected
    );
    let projected = editor.text().to_string();
    assert!(
        projected[..editor.cursor()].ends_with(caret),
        "{projected:?}"
    );
    assert_eq!(panel.editing_path(window, cx).as_deref(), Some(path));
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

#[::core::prelude::v1::test]
fn historical_binary_thumbnails_skip_automatic_patches_and_keep_text_diffs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    setup(&root);
    let png = image::RgbImage::from_pixel(8, 8, image::Rgb([32, 160, 144]));
    png.save(root.join("b-deleted.jpg")).unwrap();
    fs::write(root.join("d-text.svg"), "<svg>old</svg>\n").unwrap();
    command(&root, &["add", "."]);
    command(&root, &["commit", "-qm", "Initial"]);
    png.save(root.join("a-added.png")).unwrap();
    fs::remove_file(root.join("b-deleted.jpg")).unwrap();
    fs::write(root.join("c-text.png"), "Text despite the suffix\n").unwrap();
    fs::write(root.join("d-text.svg"), "<svg>new</svg>\n").unwrap();
    command(&root, &["add", "-A"]);
    command(&root, &["commit", "-qm", "Image changes"]);
    let mut app = test_context();
    let (panel, visual) = open(&mut app, temp.path(), &root);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            let oid = panel.history.as_ref().unwrap().head.clone().unwrap();
            panel.select_history_target(Some(&oid), window, cx);
        });
    });
    wait_for_review(
        visual,
        "binary metadata, image previews, and text patches",
        |visual| {
            visual.update(|_, cx| {
                let panel = panel.read(cx);
                let Some(review) = panel.review.as_ref() else {
                    return false;
                };
                if panel.stats_loading || review.detail.comparison.commit_target().is_none() {
                    return false;
                }
                let ix = |path| review.by_path[path];
                let added = ix("a-added.png");
                let deleted = ix("b-deleted.jpg");
                if !review.images.contains_key(&added) || !review.images.contains_key(&deleted) {
                    return false;
                }
                for index in [added, deleted] {
                    assert!(matches!(review.patches[index], PatchState::Pending));
                    assert!(change_stat(&review.stats, &review.detail.files[index]).binary);
                }
                assert!(matches!(
                    review.images[&added],
                    [DiffImage::Missing, DiffImage::Ready(_)]
                ));
                assert!(matches!(
                    review.images[&deleted],
                    [DiffImage::Ready(_), DiffImage::Missing]
                ));
                ["c-text.png", "d-text.svg"].into_iter().all(|path| {
                    let index = ix(path);
                    matches!(review.patches[index], PatchState::Ready)
                        && review.documents[index].hunk_locations().next().is_some()
                })
            })
        },
    );

    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            let ix = panel.review.as_ref().unwrap().by_path["a-added.png"];
            panel.reveal_file(ix, true, window, cx);
        });
    });
    settle(visual);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            let review = panel.review.as_ref().unwrap();
            let ix = review.by_path["a-added.png"];
            assert!(matches!(review.patches[ix], PatchState::Pending));
            panel.request_patch(ix, window, cx);
        });
    });
    wait_for_review(visual, "explicit binary patch", |visual| {
        visual.update(|_, cx| {
            let review = panel.read(cx).review.as_ref().unwrap();
            let ix = review.by_path["a-added.png"];
            matches!(review.patches[ix], PatchState::Ready) && review.documents[ix].is_binary()
        })
    });
}
