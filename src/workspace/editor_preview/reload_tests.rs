use super::{
    tests::{open_markdown_preview_fixture, settle},
    *,
};
use crate::session::DocumentPosition;

fn test_app() -> TestAppContext {
    TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        crate::test_support::text_system(true),
    )
}
fn draw_frame(visual: &mut VisualTestContext) {
    visual.executor().advance_clock(Duration::from_millis(20));
    for _ in 0..8 {
        if !visual.executor().tick() {
            break;
        }
    }
    visual.update(|window, cx| {
        window.simulate_next_frame(cx);
        window.draw(cx).clear(cx);
    });
}
fn document_source(count: usize) -> String {
    (0..count)
        .map(|index| format!("## Section {index:04}\n\nParagraph for section {index:04}.\n\n"))
        .collect()
}

fn restore_to(
    visual: &mut VisualTestContext,
    reader: &Entity<docs::DocReader>,
    source_offset: usize,
) {
    visual.update(|_, cx| {
        reader.update(cx, |reader, cx| {
            reader.restore_position(Some(DocumentPosition {
                source_offset: Some(source_offset),
                block: 0,
                inset: 0.,
                line: None,
            }));
            cx.notify();
        })
    });
}

#[::core::prelude::v1::test]
fn markdown_edit_and_external_save_keep_the_visible_heading() -> Result<()> {
    let source = document_source(90);
    let anchor = source.find("## Section 0040").unwrap();
    let prefix = "# Inserted title\n\nNew introductory paragraph.\n\n".repeat(16);
    let changed = format!(
        "{prefix}{}",
        source.replace("Paragraph for section 0070.", "Changed later paragraph.")
    );
    let expected = changed.find("## Section 0040").unwrap();
    for external in [false, true] {
        let fixture = tempfile::tempdir()?;
        let mut cx = test_app();
        let (workspace, visual) = open_markdown_preview_fixture(&mut cx, fixture.path(), &source)?;
        settle(visual, &workspace, |this, _| this.repository_watch_active);
        let reader = visual.update(|_, cx| {
            workspace
                .read(cx)
                .active_markdown_preview()
                .unwrap()
                .clone()
        });
        restore_to(visual, &reader, anchor);
        settle(visual, &workspace, |this, cx| {
            this.active_markdown_preview()
                .and_then(|reader| reader.read(cx).session_position(cx))
                .and_then(|position| position.source_offset)
                == Some(anchor)
        });
        if external {
            let staged = fixture.path().join("replacement.tmp");
            fs::write(&staged, &changed)?;
            fs::rename(staged, fixture.path().join("complex-layout.md"))?;
        } else {
            visual.update(|window, cx| {
                let editor = workspace.read(cx).active_file().unwrap().editor.clone();
                editor.update(cx, |editor, cx| {
                    editor.replace_all(changed.clone(), window, cx)
                });
                window.draw(cx).clear(cx);
            });
        }
        settle(visual, &workspace, |this, cx| {
            this.active_markdown_preview().is_some_and(|reader| {
                reader.read(cx).source_text().as_ref() == changed
                    && reader.read(cx).session_position(cx).is_some()
            })
        });
        let after = visual.update(|_, cx| {
            workspace
                .read(cx)
                .active_markdown_preview()
                .unwrap()
                .read(cx)
                .session_position(cx)
                .unwrap()
                .source_offset
                .unwrap()
        });
        assert_eq!(after, expected);
    }
    Ok(())
}

#[::core::prelude::v1::test]
fn markdown_docs_reload_keeps_the_body_and_visible_heading() -> Result<()> {
    use crate::repository::Location;
    use std::{cell::Cell, process::Command, rc::Rc};

    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("repo");
    fs::create_dir(&root)?;
    let git = |args: &[&str]| -> Result<()> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()?;
        anyhow::ensure!(
            out.status.success(),
            "git error: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(())
    };
    git(&["init", "-qb", "main"])?;
    let path = PathBuf::from("README.md");
    let source = document_source(1400);
    fs::write(root.join(&path), &source)?;
    git(&["add", "README.md"])?;
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-qm",
        "initial",
    ])?;
    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
    let mut cx = test_app();
    cx.update(gpui_component::init);
    let mut workspace = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        let view =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Docs, window, cx));
        workspace = Some(view.clone());
        gpui_component::Root::new(view, window, cx)
    });
    let workspace = workspace.unwrap();
    visual.simulate_resize(size(px(960.), px(520.)));
    visual.update(|window, cx| {
        workspace.update(cx, |this, cx| {
            this.open_repository(
                Location {
                    root: root.clone(),
                    file: Some(path.clone()),
                },
                window,
                cx,
            );
        })
    });
    settle(visual, &workspace, |this, cx| {
        this.repository_watch_active
            && this
                .doc
                .as_ref()
                .is_some_and(|doc| doc.read(cx).session_position(cx).is_some())
    });
    let reader = visual.update(|_, cx| workspace.read(cx).doc.as_ref().unwrap().clone());
    let anchor = source.find("## Section 0040").unwrap();
    restore_to(visual, &reader, anchor);
    settle(visual, &workspace, |this, cx| {
        this.doc
            .as_ref()
            .and_then(|doc| doc.read(cx).session_position(cx))
            .and_then(|position| position.source_offset)
            == Some(anchor)
    });
    let saw_loading = Rc::new(Cell::new(false));
    visual.update(|_, cx| {
        let saw_loading = saw_loading.clone();
        cx.observe(&workspace, move |workspace, cx| {
            let workspace = workspace.read(cx);
            if workspace.doc_loading.is_some() && workspace.doc.is_none() {
                saw_loading.set(true);
            }
        })
        .detach();
    });
    let prefix = "# Inserted title\n\nNew introductory paragraph.\n\n".repeat(16);
    let changed = format!("{prefix}{source}");
    let expected = changed.find("## Section 0040").unwrap();
    let staged = root.join("replacement.tmp");
    fs::write(&staged, &changed)?;
    fs::rename(staged, root.join(path))?;
    let mut blank_frames = 0;
    for _ in 0..300 {
        draw_frame(visual);
        let (blank, after) = visual.update(|_, cx| {
            let workspace = workspace.read(cx);
            let after = workspace.doc.as_ref().and_then(|doc| {
                let doc = doc.read(cx);
                (doc.source_text().as_ref() == changed)
                    .then(|| doc.session_position(cx))
                    .flatten()
            });
            (
                workspace.doc_loading.is_some() && workspace.doc.is_none(),
                after,
            )
        });
        blank_frames += usize::from(blank);
        if let Some(after) = after {
            let after = after.source_offset.unwrap();
            assert!(!saw_loading.get());
            assert_eq!(blank_frames, 0);
            assert_eq!(after, expected);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    anyhow::bail!("external reader reload did not settle");
}

#[::core::prelude::v1::test]
fn markdown_standalone_docs_reloads_in_place_and_atomic_writes() -> Result<()> {
    use gpui_component::Root;
    let fixture = tempfile::tempdir()?;
    let path = fixture.path().canonicalize()?.join("standalone.md");
    let original = "# Initial standalone\n\nInitial text.\n";
    fs::write(&path, original)?;
    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
    let mut app = test_app();
    app.update(gpui_component::init);
    app.update(|cx| Workspace::open_standalone(store, config, path.clone(), None, cx))?;
    let handle = app.windows()[0];
    let workspace = app.update_window(handle, |root, _, cx| {
        root.downcast::<Root>()
            .unwrap()
            .read(cx)
            .view()
            .clone()
            .downcast::<Workspace>()
            .unwrap()
    })?;
    let mut visual = VisualTestContext::from_window(handle, &app);
    settle(&mut visual, &workspace, |this, cx| {
        this.repository_watch_active
            && this
                .doc
                .as_ref()
                .is_some_and(|doc| doc.read(cx).source_text().as_ref() == original)
    });
    for atomic in [false, true] {
        let updated = if atomic {
            "# Updated by rename\n"
        } else {
            "# Updated in place\n"
        };
        if atomic {
            let staging = path.with_extension("tmp");
            fs::write(&staging, updated)?;
            fs::rename(staging, &path)?;
        } else {
            fs::write(&path, updated)?;
        }
        settle(&mut visual, &workspace, |this, cx| {
            this.doc
                .as_ref()
                .is_some_and(|doc| doc.read(cx).source_text().as_ref() == updated)
        });
    }
    Ok(())
}

#[::core::prelude::v1::test]
fn markdown_repository_docs_reloads_on_return_and_publishes_latest_write() -> Result<()> {
    use crate::repository::Location;
    use std::process::Command;

    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("repo");
    fs::create_dir(&root)?;
    let git = |args: &[&str]| -> Result<()> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()?;
        anyhow::ensure!(
            out.status.success(),
            "git error: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(())
    };
    git(&["init", "-qb", "main"])?;
    let path = PathBuf::from("README.md");
    let initial = "# Initial document\n\nInitial content.\n";
    fs::write(root.join(&path), initial)?;
    git(&["add", "README.md"])?;
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-qm",
        "initial",
    ])?;
    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
    let mut cx = test_app();
    cx.update(gpui_component::init);
    let mut workspace = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        let view =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Docs, window, cx));
        workspace = Some(view.clone());
        gpui_component::Root::new(view, window, cx)
    });
    let workspace = workspace.unwrap();
    visual.update(|window, cx| {
        workspace.update(cx, |this, cx| {
            this.open_repository(
                Location {
                    root: root.clone(),
                    file: Some(path.clone()),
                },
                window,
                cx,
            );
        })
    });
    settle(visual, &workspace, |this, cx| {
        this.repository_watch_active
            && this
                .doc
                .as_ref()
                .is_some_and(|doc| doc.read(cx).source_text().as_ref() == initial)
    });
    visual.update(|window, cx| {
        workspace.update(cx, |this, cx| {
            this.switch_view(WorkspaceView::Editor, window, cx);
            this.request_file(path.clone(), FileOpen::Startup, window, cx);
        })
    });
    settle(visual, &workspace, |this, cx| {
        this.view == WorkspaceView::Editor
            && this
                .files
                .get(&path)
                .is_some_and(|file| file.editor.read(cx).text() == initial)
    });
    let changed = "# Written in background\n\nChanged while Editor was active.\n";
    let staged = root.join("staged.tmp");
    fs::write(&staged, changed)?;
    fs::rename(staged, root.join(&path))?;
    settle(visual, &workspace, |this, cx| {
        this.files
            .get(&path)
            .is_some_and(|file| file.editor.read(cx).text() == changed)
    });
    visual.update(|window, cx| {
        workspace.update(cx, |this, cx| {
            this.switch_view(WorkspaceView::Docs, window, cx);
        })
    });
    settle(visual, &workspace, |this, cx| {
        this.doc
            .as_ref()
            .is_some_and(|doc| doc.read(cx).source_text().as_ref() == changed)
    });
    let control = "# New write while Docs is active\n";
    fs::write(root.join(&path), control)?;
    settle(visual, &workspace, |this, cx| {
        this.doc
            .as_ref()
            .is_some_and(|doc| doc.read(cx).source_text().as_ref() == control)
    });
    let first = "# First racing write\n";
    let second = "# Second racing write\n";
    fs::write(root.join(&path), first)?;
    visual.update(|window, cx| {
        workspace.update(cx, |this, cx| {
            this.refresh_selected_document(window, cx);
        })
    });
    fs::write(root.join(&path), second)?;
    visual.update(|window, cx| {
        workspace.update(cx, |this, cx| {
            this.refresh_selected_document(window, cx);
        })
    });
    settle(visual, &workspace, |this, cx| {
        this.doc
            .as_ref()
            .is_some_and(|doc| doc.read(cx).source_text().as_ref() == second)
    });
    for _ in 0..12 {
        draw_frame(visual);
    }
    visual.update(|_, cx| {
        assert_eq!(
            workspace
                .read(cx)
                .doc
                .as_ref()
                .unwrap()
                .read(cx)
                .source_text()
                .as_ref(),
            second
        );
    });
    Ok(())
}

#[::core::prelude::v1::test]
fn markdown_narrow_table_wraps_two_long_text_columns() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let mut cx = test_app();
    let words = "small words ".repeat(18);
    let source = format!("| First | Second |\n| --- | --- |\n| {words} | {words} |\n");
    let (_, visual) = open_markdown_preview_fixture(&mut cx, fixture.path(), &source)?;
    let pane = visual.debug_bounds("editor-preview-pane").unwrap();
    let has_thumb = visual.update(|window, _| {
        let scale = window.scale_factor();
        window.painted_quads().into_iter().any(|quad| {
            let center = quad.bounds.center().map(|value| px(value.as_f32() / scale));
            let height = quad.bounds.size.height.as_f32() / scale;
            let width = quad.bounds.size.width.as_f32() / scale;
            pane.contains(&center) && (5.9..=8.1).contains(&height) && width >= 18.
        })
    });
    assert!(
        !has_thumb,
        "a two-column text table should wrap in the split preview"
    );
    Ok(())
}
