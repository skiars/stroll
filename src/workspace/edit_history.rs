use super::*;
use crate::repository::SavedEditHistory;
use sha2::{Digest as _, Sha256};

struct DraftUpdate {
    path: PathBuf,
    revision: u64,
    write: Arc<Mutex<DraftWrite>>,
    snapshot: SavedEditHistory,
}

fn write_draft_batch(
    updates: Vec<DraftUpdate>,
    store: &Store,
    restore: Option<(&Repository, u64, &Path)>,
) {
    let (updates, snapshots): (Vec<_>, Vec<_>) = updates
        .into_iter()
        .map(|entry| ((entry.revision, entry.write), entry.snapshot))
        .unzip();
    // Acquire the file writers in path order for both async persistence and an explicit flush.
    let mut writes = updates
        .iter()
        .map(|(revision, write)| (*revision, write.lock().unwrap()))
        .collect::<Vec<_>>();
    let mut dirty = Vec::new();
    let snapshots = writes
        .iter()
        .zip(snapshots)
        .filter_map(|((revision, state), snapshot)| {
            (state.revision < *revision).then(|| {
                dirty.push(snapshot.draft.is_some());
                snapshot
            })
        })
        .collect::<Vec<_>>();
    if snapshots.is_empty() && restore.is_none() {
        return;
    }
    let result = store
        .write_editor_histories(snapshots, restore)
        .map_err(|error| format!("{error:#}"));
    for ((revision, state), dirty) in writes
        .iter_mut()
        .filter(|(revision, state)| state.revision < *revision)
        .zip(dirty)
    {
        state.revision = *revision;
        state.result = Some(match &result {
            Ok(()) => Ok(dirty),
            Err(error) => Err(anyhow::anyhow!("{error}")),
        });
    }
}

impl Workspace {
    pub(super) fn restore_editor_history(
        &self,
        path: &Path,
        document: &Document,
        editor: &mut EditorState,
    ) -> Result<()> {
        if let Some(saved) = self
            .store
            .read_editor_history(self.repository.as_deref(), path)?
            && saved.baseline == document.edit_baseline()
            && saved.buffer == <[u8; 32]>::from(Sha256::digest(document.text.as_bytes()))
        {
            editor.restore_edit_history(&saved.history)?;
        }
        Ok(())
    }

    fn history_drafts(
        &self,
        include_queued: bool,
        cx: &App,
    ) -> Result<(Vec<DraftUpdate>, Vec<PathBuf>)> {
        let mut files = self
            .files
            .values()
            .filter(|file| {
                file.pending_draft
                    && (include_queued || file.queued_revision != Some(file.revision))
            })
            .collect::<Vec<_>>();
        files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        let mut updates = Vec::new();
        let mut deferred = Vec::new();
        for file in files {
            let editor = file.editor.read(cx);
            let history = if editor.text() == &file.committed {
                editor.edit_history_snapshot()?
            } else {
                None
            };
            let Some(history) = history else {
                // Publish matching text and history once the editor can supply both.
                deferred.push(file.path.clone());
                continue;
            };
            let text = file.committed.to_string();
            updates.push(DraftUpdate {
                path: file.path.clone(),
                revision: file.revision,
                write: file.draft_write.clone(),
                snapshot: SavedEditHistory {
                    key: self
                        .store
                        .edit_history_key(self.repository.as_deref(), &file.path),
                    baseline: file.document.edit_baseline(),
                    buffer: Sha256::digest(text.as_bytes()).into(),
                    draft: (file.committed != file.saved).then_some(text),
                    format: file.document.draft_format(),
                    history,
                    undo_group: editor.history_group(true),
                    redo_group: editor.history_group(false),
                },
            });
        }
        Ok((updates, deferred))
    }

    pub(super) fn queue_drafts(&mut self, window: &Window, cx: &mut Context<Self>) {
        let (updates, deferred) = match self.history_drafts(false, cx) {
            Ok(batch) => batch,
            Err(error) => {
                eprintln!("Could not capture editor history: {error:#}");
                return;
            }
        };
        if !deferred.is_empty() {
            // Wait for a coherent batch so a multi-file undo group recovers together.
            for path in deferred {
                self.files.get_mut(&path).unwrap().draft_deferred = true;
            }
            return;
        }
        if updates.is_empty() {
            return;
        }
        let revisions = updates
            .iter()
            .map(|entry| (entry.path.clone(), entry.revision))
            .collect::<Vec<_>>();
        for (path, revision) in &revisions {
            let file = self.files.get_mut(path).unwrap();
            file.draft_deferred = false;
            file.queued_revision = Some(*revision);
            if let Some(server) = &file.language_server {
                server.document.changed(file.committed.clone());
            }
        }
        let store = self.store.clone();
        let task = cx.background_spawn(async move { write_draft_batch(updates, &store, None) });
        cx.spawn_in(window, async move |this, cx| {
            task.await;
            _ = this.update_in(cx, |this, window, cx| {
                if let Err(error) = this.finish_draft_batch(&revisions) {
                    this.show_error(error, window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn finish_draft_batch(&mut self, revisions: &[(PathBuf, u64)]) -> Result<()> {
        let mut error = None;
        for (path, revision) in revisions {
            let Some(file) = self
                .files
                .get_mut(path)
                .filter(|file| file.revision == *revision)
            else {
                continue;
            };
            file.queued_revision = None;
            let mut write = file.draft_write.lock().unwrap();
            if write.revision == *revision {
                match write.result.take() {
                    Some(Ok(dirty)) => {
                        file.dirty = dirty;
                        file.pending_draft = false;
                        file.draft_deferred = false;
                    }
                    Some(Err(failure)) => {
                        write.revision = 0;
                        error.get_or_insert(failure);
                    }
                    None => {}
                }
            }
        }
        error.map_or(Ok(()), Err)
    }

    pub(super) fn flush_drafts(&mut self, cx: &App) -> Result<()> {
        self.flush_edit_history(cx, None)
    }

    pub(super) fn flush_edit_history(
        &mut self,
        cx: &App,
        restore: Option<(&Repository, u64, &Path)>,
    ) -> Result<()> {
        let (updates, deferred) = self.history_drafts(true, cx)?;
        anyhow::ensure!(
            deferred.is_empty(),
            "Finish the current edit before saving edit history."
        );
        if updates.is_empty() && restore.is_none() {
            return Ok(());
        }
        let revisions = updates
            .iter()
            .map(|entry| (entry.path.clone(), entry.revision))
            .collect::<Vec<_>>();
        write_draft_batch(updates, &self.store, restore);
        self.finish_draft_batch(&revisions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{FileAction, FileDiffOptions, FileEdit, Mode};
    use gpui::{
        EntityInputHandler as _, Keystroke, TestAppContext, TestDispatcher, VisualTestContext,
    };
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let fixture = tempfile::tempdir().unwrap();
        let state = fixture.path().join("state");
        let root = fixture.path().join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Editor"]);
        git(&root, &["config", "user.email", "editor@example.invalid"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        for path in ["a.txt", "b.txt"] {
            fs::write(root.join(path), "original\n").unwrap();
        }
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "initial"]);
        (fixture, root, state)
    }

    fn test_app() -> TestAppContext {
        let app = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(true),
        );
        app.update(gpui_component::init);
        app
    }

    fn open<'a>(
        app: &'a mut TestAppContext,
        root: &Path,
        state: &Path,
    ) -> (Entity<Workspace>, &'a mut VisualTestContext) {
        let store = Arc::new(Store::new(state.to_owned()).unwrap());
        let config = crate::config::ConfigFile::load(Some(state.join("config.toml")));
        let mut workspace = None;
        let (_, visual) = app.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root: root.to_owned(),
                        file: Some("a.txt".into()),
                    },
                    window,
                    cx,
                );
            });
        });
        visual.run_until_parked();
        (workspace, visual)
    }

    fn read(workspace: &Entity<Workspace>, visual: &mut VisualTestContext, path: &str) -> String {
        visual.update(|_, cx| {
            workspace.read(cx).files[Path::new(path)]
                .editor
                .read(cx)
                .text()
                .to_string()
        })
    }

    fn open_for_external_change(
        app: &mut TestAppContext,
        root: &Path,
        state: &Path,
        standalone: bool,
    ) -> (Entity<Workspace>, VisualTestContext, PathBuf) {
        let absolute = root.join("a.txt");
        let (workspace, path) = if standalone {
            let store = Arc::new(Store::new(state.to_owned()).unwrap());
            let config = crate::config::ConfigFile::load(Some(state.join("config.toml")));
            app.update(|cx| Workspace::open_standalone(store, config, absolute.clone(), None, cx))
                .unwrap();
            let handle = app.windows()[0];
            let workspace = app
                .update_window(handle, |root, _, cx| {
                    root.downcast::<gpui_component::Root>()
                        .unwrap()
                        .read(cx)
                        .view()
                        .clone()
                        .downcast::<Workspace>()
                        .unwrap()
                })
                .unwrap();
            (workspace, absolute)
        } else {
            (open(app, root, state).0, "a.txt".into())
        };
        let visual = VisualTestContext::from_window(app.windows()[0], app);
        visual.run_until_parked();
        (workspace, visual, path)
    }

    #[::core::prelude::v1::test]
    fn external_rename_keeps_the_buffer_and_saves_its_draft_at_the_new_path() {
        for standalone in [false, true] {
            let (_fixture, root, state) = fixture();
            let root = root.canonicalize().unwrap();
            let mut app = test_app();
            let (workspace, mut visual, path) =
                open_for_external_change(&mut app, &root, &state, standalone);
            let editor = visual.update(|window, cx| {
                let editor = workspace.read(cx).files[&path].editor.clone();
                editor.update(cx, |editor, cx| {
                    editor.replace_text_in_range(None, "draft ", window, cx);
                });
                editor
            });
            let renamed = root.join("renamed.txt");
            fs::rename(root.join(&path), &renamed).unwrap();
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    if standalone {
                        this.apply_standalone_changes(
                            vec![(path.clone(), renamed.clone())],
                            window,
                            cx,
                        );
                    } else {
                        let repo = this.repository.clone().unwrap();
                        this.apply_repository_changes(
                            &repo,
                            crate::git::watch::RepoChanges {
                                paths: ["a.txt".to_owned(), "renamed.txt".to_owned()].into(),
                                directories: [PathBuf::new()].into(),
                                renames: vec![("a.txt".into(), "renamed.txt".into())],
                                structure: true,
                                ..Default::default()
                            },
                            window,
                            cx,
                        );
                    }
                });
            });
            visual.run_until_parked();
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    let file = this.active_file().unwrap();
                    assert_eq!(
                        file.path,
                        if standalone {
                            renamed.clone()
                        } else {
                            "renamed.txt".into()
                        }
                    );
                    assert_eq!(file.editor, editor);
                    assert!(!file.missing && !this.file_editor_locked(file));
                    assert_eq!(file.committed, "draft original\n");
                    assert_eq!(
                        stored_history(this, &file.path).unwrap().draft.as_deref(),
                        Some("draft original\n")
                    );
                    assert!(stored_history(this, &path).is_none_or(|saved| saved.draft.is_none()));
                    this.save(&SaveFile, window, cx);
                });
            });
            visual.run_until_parked();
            assert_eq!(fs::read_to_string(&renamed).unwrap(), "draft original\n");
            assert!(!root.join("a.txt").exists());
        }
    }

    #[cfg(unix)]
    #[::core::prelude::v1::test]
    fn save_before_a_rename_notification_follows_the_file_identity() {
        for standalone in [false, true] {
            let (_fixture, root, state) = fixture();
            let root = root.canonicalize().unwrap();
            let mut app = test_app();
            let (workspace, mut visual, path) =
                open_for_external_change(&mut app, &root, &state, standalone);
            visual.update(|window, cx| {
                workspace.read(cx).files[&path]
                    .editor
                    .clone()
                    .update(cx, |editor, cx| {
                        editor.replace_text_in_range(None, "draft ", window, cx);
                    });
            });
            let renamed = root.join("renamed.txt");
            fs::rename(root.join(&path), &renamed).unwrap();
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| this.save(&SaveFile, window, cx));
            });
            visual.run_until_parked();
            assert_eq!(fs::read_to_string(&renamed).unwrap(), "draft original\n");
            assert!(!root.join(&path).exists());
        }
    }

    #[::core::prelude::v1::test]
    fn deleted_buffers_can_save_with_or_without_a_watcher_update() {
        for standalone in [false, true] {
            for watcher_updated in [false, true] {
                let (_fixture, root, state) = fixture();
                let root = root.canonicalize().unwrap();
                let mut app = test_app();
                let (workspace, mut visual, path) =
                    open_for_external_change(&mut app, &root, &state, standalone);
                let absolute = root.join(&path);
                fs::remove_file(&absolute).unwrap();
                if watcher_updated {
                    visual.update(|window, cx| {
                        workspace.update(cx, |this, cx| {
                            this.sync_document_paths(vec![path.clone()], window, cx);
                        });
                    });
                    visual.run_until_parked();
                }
                visual.update(|_, cx| {
                    let this = workspace.read(cx);
                    let file = &this.files[&path];
                    assert_eq!(file.missing, watcher_updated);
                    assert!(!this.file_editor_locked(file));
                    assert!(!file.is_dirty());
                });
                if !watcher_updated {
                    visual.update(|window, cx| {
                        workspace.read(cx).files[&path]
                            .editor
                            .clone()
                            .update(cx, |editor, cx| {
                                editor.replace_text_in_range(None, "draft ", window, cx);
                            });
                    });
                }
                visual.update(|window, cx| {
                    workspace.update(cx, |this, cx| this.save(&SaveFile, window, cx));
                });
                visual.run_until_parked();
                assert_eq!(
                    fs::read_to_string(&absolute).unwrap(),
                    if watcher_updated {
                        "original\n"
                    } else {
                        "draft original\n"
                    }
                );
                visual.update(|_, cx| {
                    let file = &workspace.read(cx).files[&path];
                    assert!(!file.missing && !file.is_dirty());
                });
            }
        }
    }

    #[::core::prelude::v1::test]
    fn a_file_deleted_after_overwrite_confirmation_can_be_recreated() {
        for standalone in [false, true] {
            let (_fixture, root, state) = fixture();
            let root = root.canonicalize().unwrap();
            let mut app = test_app();
            let (workspace, mut visual, path) =
                open_for_external_change(&mut app, &root, &state, standalone);
            let absolute = root.join(&path);
            visual.update(|window, cx| {
                workspace.read(cx).files[&path]
                    .editor
                    .clone()
                    .update(cx, |editor, cx| {
                        editor.replace_text_in_range(None, "draft ", window, cx);
                    });
            });
            fs::write(&absolute, "external\n").unwrap();
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    this.sync_document_paths(vec![path.clone()], window, cx)
                });
            });
            visual.run_until_parked();
            let expected =
                visual.update(|_, cx| workspace.read(cx).files[&path].external.as_ref().unwrap().0);
            fs::remove_file(&absolute).unwrap();
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    this.save_buffer(path.clone(), false, Some(expected), window, cx);
                });
            });
            visual.run_until_parked();
            assert_eq!(fs::read_to_string(&absolute).unwrap(), "draft original\n");
        }
    }

    #[::core::prelude::v1::test]
    fn edits_after_deletion_restore_with_the_original_encoding_and_history() {
        for standalone in [false, true] {
            let (_fixture, root, state) = fixture();
            let root = root.canonicalize().unwrap();
            let absolute = root.join("a.txt");
            fs::write(&absolute, "\u{feff}original\r\n").unwrap();
            {
                let mut app = test_app();
                let (workspace, mut visual, path) =
                    open_for_external_change(&mut app, &root, &state, standalone);
                fs::remove_file(&absolute).unwrap();
                visual.update(|window, cx| {
                    workspace.update(cx, |this, cx| {
                        this.sync_document_paths(vec![path.clone()], window, cx)
                    });
                });
                visual.run_until_parked();
                visual.update(|window, cx| {
                    let this = workspace.read(cx);
                    let file = &this.files[&path];
                    assert!(file.missing && !this.file_editor_locked(file));
                    file.editor.clone().update(cx, |editor, cx| {
                        editor.replace_text_in_range(None, "draft ", window, cx);
                        assert_eq!(editor.text(), "draft original\n");
                    });
                });
                flush(&workspace, &mut visual);
            }
            let mut app = test_app();
            let (workspace, mut visual, path) =
                open_for_external_change(&mut app, &root, &state, standalone);
            visual.update(|window, cx| {
                let file = &workspace.read(cx).files[&path];
                assert!(file.missing);
                assert_eq!(file.committed, "draft original\n");
                file.editor.clone().update(cx, |editor, cx| {
                    editor.replay_text_history(true, window, cx);
                    assert_eq!(editor.text(), "original\n");
                    editor.replay_text_history(false, window, cx);
                });
            });
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| this.save(&SaveFile, window, cx));
            });
            visual.run_until_parked();
            assert_eq!(
                fs::read(&absolute).unwrap(),
                b"\xef\xbb\xbfdraft original\r\n"
            );
        }
    }

    #[::core::prelude::v1::test]
    fn mixed_newlines_reload_as_editable_and_normalize_on_save() {
        for standalone in [false, true] {
            let (_fixture, root, state) = fixture();
            let root = root.canonicalize().unwrap();
            let mut app = test_app();
            let (workspace, mut visual, path) =
                open_for_external_change(&mut app, &root, &state, standalone);
            let absolute = root.join(&path);
            let mixed = b"\xef\xbb\xbfexternal\r\nsecond\n";
            fs::write(&absolute, mixed).unwrap();
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    this.sync_document_paths(vec![path.clone()], window, cx);
                });
            });
            visual.run_until_parked();
            assert_eq!(fs::read(&absolute).unwrap(), mixed);
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                let this = workspace.read(cx);
                let file = &this.files[&path];
                assert!(!this.file_editor_locked(file));
                assert!(!file.is_dirty() && !file.document.conflict);
                assert_eq!(file.committed, "external\nsecond\n");
                assert_eq!(file.document.line_ending_label(), "CRLF");
                file.editor.clone().update(cx, |editor, cx| {
                    assert!(editor.is_editable());
                    editor.replace_text_in_range(Some(0..0), "edited ", window, cx);
                });
            });
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| this.save(&SaveFile, window, cx));
            });
            visual.run_until_parked();
            assert_eq!(
                fs::read(&absolute).unwrap(),
                b"\xef\xbb\xbfedited external\r\nsecond\r\n"
            );
            visual.update(|_, cx| {
                let file = &workspace.read(cx).files[&path];
                assert!(!file.document.mixed_newlines);
                assert!(!file.is_dirty());
            });

            // An explicit save also normalizes line endings without a text edit.
            // The disk read in the save can see this before the watcher does.
            fs::write(&absolute, b"edited external\nsecond\r\n").unwrap();
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| this.save(&SaveFile, window, cx));
            });
            visual.run_until_parked();
            assert_eq!(fs::read(&absolute).unwrap(), b"edited external\nsecond\n");
            visual.update(|_, cx| {
                let file = &workspace.read(cx).files[&path];
                assert_eq!(file.document.line_ending_label(), "LF");
                assert!(!file.document.mixed_newlines && !file.is_dirty());
            });
        }
    }

    #[::core::prelude::v1::test]
    fn mixed_newlines_allow_overwriting_live_and_restored_drafts() {
        for standalone in [false, true] {
            for reopen in [false, true] {
                let (_fixture, root, state) = fixture();
                let root = root.canonicalize().unwrap();
                let absolute = root.join("a.txt");
                let mixed = b"external\r\nsecond\nthird\n";
                let newer = b"external\r\nsecond\r\nthird\n";
                let mut app = test_app();
                let (workspace, mut visual, path) =
                    open_for_external_change(&mut app, &root, &state, standalone);
                visual.update(|window, cx| {
                    workspace.read(cx).files[&path]
                        .editor
                        .clone()
                        .update(cx, |editor, cx| {
                            editor.replace_text_in_range(None, "draft ", window, cx);
                        });
                });
                flush(&workspace, &mut visual);
                fs::write(&absolute, mixed).unwrap();
                let (workspace, mut visual, path) = if reopen {
                    drop(visual);
                    drop(workspace);
                    drop(app);
                    app = test_app();
                    open_for_external_change(&mut app, &root, &state, standalone)
                } else {
                    (workspace, visual, path)
                };
                visual.update(|window, cx| {
                    workspace.update(cx, |this, cx| this.save(&SaveFile, window, cx));
                });
                visual.run_until_parked();
                assert_eq!(fs::read(&absolute).unwrap(), mixed);
                let expected = visual.update(|window, cx| {
                    window.draw(cx).clear(cx);
                    assert!(window.has_active_dialog(cx));
                    let this = workspace.read(cx);
                    let file = &this.files[&path];
                    assert!(file.document.conflict && file.is_dirty());
                    assert_eq!(file.document.restored, reopen);
                    assert!(!this.file_editor_locked(file));
                    assert!(file.editor.read(cx).is_editable());
                    assert_eq!(file.committed, "draft original\n");
                    assert_eq!(file.document.line_ending_label(), "LF");
                    if reopen {
                        file.document.draft_baseline()
                    } else {
                        file.external.as_ref().unwrap().0
                    }
                });

                // The text is identical after normalization, but the disk bytes changed.
                fs::write(&absolute, newer).unwrap();
                visual.update(|window, cx| {
                    window.close_dialog(cx);
                    workspace.update(cx, |this, cx| {
                        this.save_buffer(path.clone(), false, Some(expected), window, cx);
                    });
                });
                visual.run_until_parked();
                assert_eq!(fs::read(&absolute).unwrap(), newer);
                let latest = visual.update(|window, cx| {
                    assert!(window.has_active_dialog(cx));
                    let file = &workspace.read(cx).files[&path];
                    assert_eq!(file.committed, "draft original\n");
                    let latest = file.external.as_ref().unwrap().0;
                    assert_ne!(latest, expected);
                    latest
                });

                visual.update(|window, cx| {
                    window.close_dialog(cx);
                    workspace.update(cx, |this, cx| {
                        this.save_buffer(path.clone(), false, Some(latest), window, cx);
                    });
                });
                visual.run_until_parked();
                assert_eq!(fs::read(&absolute).unwrap(), b"draft original\n");
                visual.update(|_, cx| {
                    let file = &workspace.read(cx).files[&path];
                    assert!(!file.document.conflict && !file.document.mixed_newlines);
                    assert!(!file.is_dirty());
                });
            }
        }
    }

    fn replay(workspace: &Entity<Workspace>, visual: &mut VisualTestContext, undo: bool) {
        visual.update(|window, cx| {
            workspace.read(cx).files[Path::new("a.txt")]
                .editor
                .clone()
                .update(cx, |editor, cx| {
                    editor.replay_text_history(undo, window, cx);
                });
        });
        visual.run_until_parked();
    }

    fn flush(workspace: &Entity<Workspace>, visual: &mut VisualTestContext) {
        visual.update(|_, cx| {
            workspace.update(cx, |workspace, cx| workspace.flush_drafts(cx).unwrap())
        });
    }

    fn vim_keys(
        workspace: &mut Workspace,
        keys: &[&'static str],
        window: &mut Window,
        cx: &mut App,
    ) {
        let path = workspace.tabs.active().unwrap();
        let file = workspace.files.get_mut(path).unwrap();
        for key in keys {
            if *key == "escape" {
                assert!(file.vim.escape(&file.editor, window, cx));
            } else {
                assert!(
                    file.vim
                        .key_down(
                            &file.editor,
                            false,
                            &Keystroke::parse(key).unwrap(),
                            file.wrap,
                            window,
                            cx,
                        )
                        .is_some()
                );
            }
        }
    }

    fn stored_history(workspace: &Workspace, path: &Path) -> Option<SavedEditHistory> {
        workspace
            .store
            .read_editor_history(workspace.repository.as_deref(), path)
            .unwrap()
    }

    #[::core::prelude::v1::test]
    fn vim_insert_saves_the_requested_text_and_keeps_one_undo_entry() {
        for standalone in [false, true] {
            let (_fixture, root, state) = fixture();
            let disk = root.join("a.txt").canonicalize().unwrap();
            let mut app = test_app();
            let (workspace, visual) = if standalone {
                let store = Arc::new(Store::new(state.clone()).unwrap());
                let config = crate::config::ConfigFile::load(Some(state.join("config.toml")));
                let (workspace, visual) = app.add_window_view(|window, cx| {
                    Workspace::with_config(store, config, WorkspaceView::Editor, window, cx)
                });
                visual.update(|window, cx| {
                    workspace.update(cx, |this, cx| {
                        this.standalone = Some(disk.clone());
                        this.request_file(disk.clone(), FileOpen::Startup, window, cx);
                    });
                });
                visual.run_until_parked();
                (workspace, visual)
            } else {
                open(&mut app, &root, &state)
            };
            let editor = visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    vim_keys(this, &["2", "i"], window, cx);
                    this.active_file().unwrap().editor.clone()
                })
            });
            visual.update(|window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.replace_text_in_range(None, "a", window, cx);
                });
            });
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| this.save(&SaveFile, window, cx));
                editor.update(cx, |editor, cx| {
                    editor.replace_text_in_range(None, "b", window, cx);
                });
            });
            visual.run_until_parked();
            assert_eq!(fs::read_to_string(&disk).unwrap(), "aoriginal\n");
            visual.update(|_, cx| {
                let this = workspace.read(cx);
                let file = this.active_file().unwrap();
                assert!(file.vim.mode() == vim::source::Mode::Insert);
                assert!(file.is_dirty());
                assert_eq!(file.committed, "aboriginal\n");
                let saved = stored_history(this, &file.path).unwrap();
                assert_eq!(saved.baseline, file.document.edit_baseline());
                assert_eq!(saved.draft.as_deref(), Some("aboriginal\n"));
            });
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| vim_keys(this, &["escape"], window, cx));
            });
            visual.executor().advance_clock(Duration::from_millis(250));
            visual.run_until_parked();
            visual.update(|window, cx| {
                let this = workspace.read(cx);
                let file = this.active_file().unwrap();
                assert_eq!(
                    stored_history(this, &file.path).unwrap().draft.as_deref(),
                    Some("ababoriginal\n")
                );
                editor.update(cx, |editor, cx| {
                    editor.replay_text_history(true, window, cx);
                    assert_eq!(editor.text(), "original\n");
                    editor.replay_text_history(false, window, cx);
                    assert_eq!(editor.text(), "ababoriginal\n");
                });
            });
        }
    }

    #[::core::prelude::v1::test]
    fn vim_insert_draft_checkpoint_restores_undo_without_saving_the_file() {
        let (_fixture, root, state) = fixture();
        {
            let mut app = test_app();
            let (workspace, visual) = open(&mut app, &root, &state);
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    vim_keys(this, &["c", "i", "w"], window, cx);
                    this.active_file().unwrap().editor.update(cx, |editor, cx| {
                        editor.replace_text_in_range(None, "draft", window, cx);
                    });
                });
            });
            visual.run_until_parked();
            visual.executor().advance_clock(Duration::from_millis(250));
            visual.run_until_parked();
            visual.update(|_, cx| {
                let this = workspace.read(cx);
                let file = this.active_file().unwrap();
                assert!(file.vim.mode() == vim::source::Mode::Insert);
                let saved = stored_history(this, &file.path).unwrap();
                assert_eq!(saved.draft.as_deref(), Some("draft\n"));
            });
        }
        let mut app = test_app();
        let (workspace, visual) = open(&mut app, &root, &state);
        assert_eq!(read(&workspace, visual, "a.txt"), "draft\n");
        replay(&workspace, visual, true);
        assert_eq!(read(&workspace, visual, "a.txt"), "original\n");
        replay(&workspace, visual, false);
        assert_eq!(read(&workspace, visual, "a.txt"), "draft\n");
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "original\n"
        );
    }

    #[::core::prelude::v1::test]
    fn vim_escape_retries_a_deferred_draft_without_another_text_change() {
        let (_fixture, root, state) = fixture();
        let mut app = test_app();
        let (workspace, visual) = open(&mut app, &root, &state);
        let path = Path::new("a.txt");
        let group = visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                let group = this.store.register_edit_group(&[path.to_owned()]).unwrap();
                this.files[path].editor.update(cx, |editor, cx| {
                    editor.edit_ranges_in_group(&[(0..8, "draft".into())], group, window, cx);
                });
                vim_keys(this, &["i"], window, cx);
                group
            })
        });
        visual.run_until_parked();
        visual.executor().advance_clock(Duration::from_millis(250));
        visual.run_until_parked();
        let revision = visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert!(this.files[path].draft_deferred);
            assert!(stored_history(this, path).is_none());
            this.files[path].revision
        });
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| vim_keys(this, &["escape"], window, cx));
        });
        visual.run_until_parked();
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.files[path].revision, revision);
            let saved = stored_history(this, path).unwrap();
            assert_eq!(saved.undo_group, Some(group));
            assert_eq!(saved.draft.as_deref(), Some("draft\n"));
        });
    }

    #[::core::prelude::v1::test]
    fn vim_insert_save_excludes_ime_preedit_and_drafts_resume_after_cancellation() {
        let (_fixture, root, state) = fixture();
        let mut app = test_app();
        let (workspace, visual) = open(&mut app, &root, &state);
        let path = Path::new("a.txt");
        let editor = visual.update(|window, cx| {
            workspace.update(cx, |this, cx| vim_keys(this, &["i"], window, cx));
            let editor = workspace.read(cx).files[path].editor.clone();
            editor.update(cx, |editor, cx| {
                editor.replace_text_in_range(None, "saved", window, cx);
            });
            editor
        });
        visual.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.replace_and_mark_text_in_range(None, "仮", None, window, cx);
                assert!(editor.is_composing());
                assert!(editor.edit_history_snapshot().unwrap().is_none());
            });
            workspace.update(cx, |this, cx| this.save(&SaveFile, window, cx));
        });
        visual.run_until_parked();
        assert_eq!(
            fs::read_to_string(root.join(path)).unwrap(),
            "savedoriginal\n"
        );
        visual.update(|window, cx| {
            assert!(workspace.read(cx).files[path].draft_deferred);
            editor.update(cx, |editor, cx| {
                editor.replace_and_mark_text_in_range(None, "", None, window, cx);
            });
            workspace.update(cx, |this, cx| vim_keys(this, &["escape"], window, cx));
        });
        visual.run_until_parked();
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            let saved = stored_history(this, path).unwrap();
            assert!(saved.draft.is_none());
            assert!(!this.files[path].draft_deferred);
        });
    }

    #[::core::prelude::v1::test]
    fn persistent_edit_history_keeps_edits_and_redo_across_restarts() {
        let (_fixture, root, state) = fixture();
        {
            let mut app = test_app();
            let (workspace, visual) = open(&mut app, &root, &state);
            visual.update(|window, cx| {
                workspace.read(cx).files[Path::new("a.txt")]
                    .editor
                    .clone()
                    .update(cx, |editor, cx| {
                        editor.edit_ranges(&[(0..8, "saved".into())], window, cx);
                    });
            });
            visual.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.start_save("a.txt".into(), false, window, cx)
                });
            });
            visual.run_until_parked();
            visual.update(|window, cx| {
                workspace.read(cx).files[Path::new("a.txt")]
                    .editor
                    .clone()
                    .update(cx, |editor, cx| {
                        editor.edit_ranges(&[(5..5, "!".into())], window, cx);
                    });
            });
            flush(&workspace, visual);
            visual.update(|_, cx| {
                let workspace = workspace.read(cx);
                let persisted = workspace
                    .store
                    .read_editor_history(workspace.repository.as_deref(), Path::new("a.txt"))
                    .unwrap()
                    .unwrap();
                let snapshot: serde_json::Value =
                    serde_json::from_slice(&persisted.history).unwrap();
                assert_eq!(
                    snapshot[0][1]["changes"][0],
                    serde_json::json!({
                        "old_start": 5, "new_start": 5, "before": "", "after": "!"
                    })
                );
                assert_eq!(snapshot[0][1].as_object().unwrap().len(), 2);
            });
        }
        {
            let mut app = test_app();
            let (workspace, visual) = open(&mut app, &root, &state);
            assert_eq!(read(&workspace, visual, "a.txt"), "saved!\n");
            replay(&workspace, visual, true);
            assert_eq!(read(&workspace, visual, "a.txt"), "saved\n");
            replay(&workspace, visual, true);
            assert_eq!(read(&workspace, visual, "a.txt"), "original\n");
            flush(&workspace, visual);
        }
        {
            let mut app = test_app();
            let (workspace, visual) = open(&mut app, &root, &state);
            assert_eq!(read(&workspace, visual, "a.txt"), "original\n");
            replay(&workspace, visual, false);
            assert_eq!(read(&workspace, visual, "a.txt"), "saved\n");
            replay(&workspace, visual, false);
            assert_eq!(read(&workspace, visual, "a.txt"), "saved!\n");
        }
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "saved\n");
    }

    #[::core::prelude::v1::test]
    fn persistent_edit_history_replays_a_multi_file_group_after_restart() {
        let (_fixture, root, state) = fixture();
        {
            let mut app = test_app();
            let (workspace, visual) = open(&mut app, &root, &state);
            visual.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.ensure_review_buffer(
                        "b.txt".into(),
                        window,
                        cx,
                        |_, result, _, _| result.unwrap(),
                    );
                });
            });
            visual.run_until_parked();
            visual.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let paths = ["a.txt".into(), "b.txt".into()];
                    let group = workspace.store.register_edit_group(&paths).unwrap();
                    for path in &paths {
                        workspace.files[path].editor.update(cx, |editor, cx| {
                            editor.edit_ranges_in_group(
                                &[(0..8, "renamed".into())],
                                group,
                                window,
                                cx,
                            );
                        });
                    }
                });
            });
            flush(&workspace, visual);
        }
        let mut app = test_app();
        let (workspace, visual) = open(&mut app, &root, &state);
        assert_eq!(read(&workspace, visual, "a.txt"), "renamed\n");
        replay(&workspace, visual, true);
        assert_eq!(read(&workspace, visual, "a.txt"), "original\n");
        assert_eq!(read(&workspace, visual, "b.txt"), "original\n");
        replay(&workspace, visual, false);
        assert_eq!(read(&workspace, visual, "a.txt"), "renamed\n");
        assert_eq!(read(&workspace, visual, "b.txt"), "renamed\n");
    }

    #[::core::prelude::v1::test]
    fn persistent_edit_history_undoes_restore_and_deletion_after_restart() {
        let (_fixture, root, state) = fixture();
        fs::write(root.join("a.txt"), "edited\n").unwrap();
        fs::write(root.join("untracked.txt"), "keep these bytes\n").unwrap();
        let previous_journal = root.join(".git/stroll/operations.json");
        fs::create_dir_all(previous_journal.parent().unwrap()).unwrap();
        fs::write(&previous_journal, "old journal").unwrap();
        {
            let mut app = test_app();
            let (workspace, visual) = open(&mut app, &root, &state);
            visual.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let repo = workspace.repository.clone().unwrap();
                    let backend = repo.git_backend();
                    for name in ["a.txt", "untracked.txt"] {
                        let path = Path::new(name);
                        let version = backend
                            .file_changes(
                                name,
                                FileDiffOptions {
                                    mode: Mode::Unstaged,
                                    context: 3,
                                    whitespace: false,
                                },
                            )
                            .unwrap()
                            .version;
                        let edit = FileEdit {
                            path: name.into(),
                            version,
                            action: FileAction::Restore,
                            block: None,
                        };
                        workspace
                            .begin_git_edit([path.to_owned()], FileAction::Restore, cx)
                            .unwrap();
                        let outcome = Workspace::run_git_edit(&repo, path, edit).unwrap();
                        workspace.finish_recorded_git_edit(&repo, path, outcome, window, cx);
                    }
                });
            });
            assert_eq!(
                fs::read_to_string(root.join("a.txt")).unwrap(),
                "original\n"
            );
            assert!(!root.join("untracked.txt").exists());
        }
        {
            let mut app = test_app();
            let (workspace, visual) = open(&mut app, &root, &state);
            visual.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.replay_git_history(None, None, true, window, cx)
                });
            });
            visual.run_until_parked();
            assert_eq!(
                fs::read_to_string(root.join("untracked.txt")).unwrap(),
                "keep these bytes\n"
            );
            replay(&workspace, visual, true);
            assert_eq!(read(&workspace, visual, "a.txt"), "edited\n");
            assert_eq!(
                fs::read_to_string(root.join("a.txt")).unwrap(),
                "original\n"
            );
            flush(&workspace, visual);
        }
        {
            let mut app = test_app();
            let (workspace, visual) = open(&mut app, &root, &state);
            assert_eq!(read(&workspace, visual, "a.txt"), "edited\n");
            replay(&workspace, visual, false);
            assert_eq!(read(&workspace, visual, "a.txt"), "original\n");
            assert_eq!(fs::read_to_string(previous_journal).unwrap(), "old journal");
        }
    }
}
