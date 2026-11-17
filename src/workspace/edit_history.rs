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
        if document.conflict {
            return Ok(());
        }
        if let Some(saved) = self
            .store
            .read_editor_history(self.repository.as_deref(), path)?
            && saved.baseline == document.draft_baseline()
            && saved.buffer == <[u8; 32]>::from(Sha256::digest(document.text.as_bytes()))
        {
            editor.restore_edit_history(&saved.history)?;
        }
        Ok(())
    }

    fn history_drafts(&self, cx: &App) -> Result<Option<Vec<DraftUpdate>>> {
        let mut files = self
            .files
            .values()
            .filter(|file| file.pending_draft && !file.document.conflict && !file.missing)
            .collect::<Vec<_>>();
        files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        let mut updates = Vec::new();
        for file in files {
            let editor = file.editor.read(cx);
            let Some(history) = editor.edit_history_snapshot()? else {
                // A composition still owns its pending transaction. Publish the text and its history together.
                return Ok(None);
            };
            let text = editor.text().to_string();
            updates.push(DraftUpdate {
                path: file.path.clone(),
                revision: file.revision,
                write: file.draft_write.clone(),
                snapshot: SavedEditHistory {
                    key: self
                        .store
                        .edit_history_key(self.repository.as_deref(), &file.path),
                    baseline: file.document.draft_baseline(),
                    buffer: Sha256::digest(text.as_bytes()).into(),
                    draft: (editor.text() != &file.saved).then_some(text),
                    history,
                    undo_group: editor.history_group(true),
                    redo_group: editor.history_group(false),
                },
            });
        }
        Ok(Some(updates))
    }

    pub(super) fn queue_drafts(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.files.values().any(|file| {
            file.pending_draft
                && !file.document.conflict
                && !file.missing
                && file.queued_revision != Some(file.revision)
        }) {
            return;
        }
        let updates = match self.history_drafts(cx) {
            Ok(Some(updates)) => updates,
            Ok(None) => return,
            Err(error) => {
                eprintln!("Could not capture editor history: {error:#}");
                return;
            }
        };
        let revisions = updates
            .iter()
            .map(|entry| (entry.path.clone(), entry.revision))
            .collect::<Vec<_>>();
        for (path, revision) in &revisions {
            let file = self.files.get_mut(path).unwrap();
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
        let Some(updates) = self.history_drafts(cx)? else {
            anyhow::bail!("Finish composing text before saving edit history.");
        };
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
    use gpui::{TestAppContext, TestDispatcher, VisualTestContext};
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
        let platform = gpui_platform::current_platform(true);
        let app = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
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
        let (workspace, visual) = app.add_window_view(|window, cx| {
            Workspace::with_config(store, config, WorkspaceView::Editor, window, cx)
        });
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
