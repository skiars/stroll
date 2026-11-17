use super::*;
use crate::git::{FileAction, Operation, UndoAvailability};
use gpui_component::input::{Redo, Undo};
use std::path::Path;

const GIT_GROUP: u64 = 1 << 63;

pub(super) fn restore_group(id: u64) -> u64 {
    id | GIT_GROUP
}

enum RestoreHistory {
    Text(u64, PathBuf),
    Disk(Operation),
    UnrecordedText(Operation, Document, [u8; 32]),
}

struct GitHistoryKeys;
impl Global for GitHistoryKeys {}

#[derive(Default)]
pub(super) struct GitHistory {
    editors: BTreeMap<u64, Vec<WeakEntity<EditorState>>>,
    pub(super) busy: bool,
}

impl Workspace {
    pub(super) fn bind_git_history_keys(cx: &mut App) {
        if cx.has_global::<GitHistoryKeys>() {
            return;
        }
        cx.set_global(GitHistoryKeys);
        #[cfg(target_os = "macos")]
        let (undo, redo) = ("cmd-z", "cmd-shift-z");
        #[cfg(not(target_os = "macos"))]
        let (undo, redo) = ("ctrl-z", "ctrl-y");
        cx.bind_keys([
            KeyBinding::new(undo, Undo, Some("GitReview")),
            KeyBinding::new(redo, Redo, Some("GitReview")),
        ]);
        #[cfg(not(target_os = "macos"))]
        cx.bind_keys([
            KeyBinding::new("ctrl-shift-z", Redo, Some("Input")),
            KeyBinding::new("ctrl-shift-z", Redo, Some("GitReview")),
        ]);
    }

    pub(super) fn history_handler(
        &self,
        path: &Path,
        cx: &Context<Self>,
    ) -> impl Fn(u64, bool, &mut Window, &mut App) + use<> {
        let workspace = cx.weak_entity();
        let path = path.to_owned();
        move |group, undo, window, cx| {
            let workspace = workspace.clone();
            let path = path.clone();
            window.defer(cx, move |window, cx| {
                _ = workspace.update(cx, |workspace, cx| match group {
                    0 => workspace.replay_git_history(None, Some(path), undo, window, cx),
                    id if id & GIT_GROUP != 0 => {
                        if workspace.files.get(&path).is_some_and(|file| {
                            !file.editor.read(cx).has_external_edit_history(undo)
                        }) {
                            workspace.replay_text_restore(id & !GIT_GROUP, path, undo, window, cx);
                        } else {
                            workspace.replay_git_history(
                                Some(id & !GIT_GROUP),
                                Some(path),
                                undo,
                                window,
                                cx,
                            );
                        }
                    }
                    id => workspace.replay_language_edits(id, undo, window, cx),
                });
            });
        }
    }

    pub(super) fn record_git_history(
        &mut self,
        operation: Option<Operation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(operation) = operation else { return };
        let group = operation.id | GIT_GROUP;
        let sources = operation
            .paths
            .iter()
            .filter_map(|path| {
                let file = self.files.get(Path::new(path))?;
                file.editor.update(cx, |editor, cx| {
                    editor.edit_ranges_in_group(&[], group, window, cx)
                });
                Some(file.editor.downgrade())
            })
            .collect::<Vec<_>>();
        self.git_history.editors.insert(operation.id, sources);
        if self.git_history.editors.len() > 1000 {
            self.git_history.editors.pop_first();
        }
    }

    pub(super) fn replay_git_history(
        &mut self,
        id: Option<u64>,
        source: Option<PathBuf>,
        undo: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.git_history.busy || !self.git_edit.is_empty() {
            return;
        }
        let Some(repo) = self.repository.clone() else {
            return;
        };
        let backend = repo.git_backend();
        if !backend.supports_git_operations() {
            return;
        }
        if let Err(error) = self.flush_drafts(cx) {
            self.show_error(error, window, cx);
            return;
        }
        self.git_history.busy = true;
        let store = self.store.clone();
        let history_repo = repo.clone();
        let work = cx.background_spawn(async move {
            let (recorded, tips) = store.text_restore_tips(&history_repo, undo)?;
            let text = tips.into_iter().find(|(entry, path)| {
                id.is_none_or(|id| id == *entry)
                    && source.as_ref().is_none_or(|source| source == path)
            });
            let operations = backend.operation_history()?;
            let matches = |operation: &&Operation| {
                operation.action == FileAction::Restore
                    && !recorded.contains(&operation.id)
                    && id.is_none_or(|id| id == operation.id)
                    && source_for_history(&source, operation)
            };
            let disk = if undo {
                operations
                    .iter()
                    .filter(matches)
                    .find(|operation| operation.availability == UndoAvailability::Available)
            } else {
                operations
                    .iter()
                    .filter(matches)
                    .filter(|operation| operation.redoable)
                    .max_by_key(|operation| (operation.undo_order, std::cmp::Reverse(operation.id)))
            };
            let selected = match (text, disk) {
                (Some((id, path)), disk) if disk.is_none_or(|disk| id > disk.id) => {
                    Some(RestoreHistory::Text(id, path))
                }
                (_, Some(disk)) => {
                    if let Some((bytes, baseline)) = backend.restore_history_text(disk.id, undo)?
                        && let Ok(document) = Document::decode(bytes.0)
                        && !document.mixed_newlines
                    {
                        Some(RestoreHistory::UnrecordedText(
                            disk.clone(),
                            document,
                            baseline,
                        ))
                    } else {
                        Some(RestoreHistory::Disk(disk.clone()))
                    }
                }
                _ => None,
            };
            Ok::<_, anyhow::Error>(selected)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            _ = this.update_in(cx, |workspace, window, cx| {
                if !workspace
                    .repository
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &repo))
                {
                    return;
                }
                workspace.git_history.busy = false;
                match result {
                    Ok(Some(RestoreHistory::Text(id, path))) => {
                        workspace.replay_text_restore(id, path, undo, window, cx);
                    }
                    Ok(Some(RestoreHistory::UnrecordedText(operation, document, baseline))) => {
                        workspace.replay_unrecorded_text_restore(
                            operation, document, baseline, undo, window, cx,
                        );
                    }
                    Ok(Some(RestoreHistory::Disk(operation))) => {
                        workspace.apply_git_history(operation, undo, window, cx);
                    }
                    Ok(None) => {}
                    Err(error) => workspace.show_error(error, window, cx),
                }
            });
        })
        .detach();
    }

    fn replay_text_restore(
        &mut self,
        id: u64,
        path: PathBuf,
        undo: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_review_buffer(path.clone(), window, cx, move |this, result, window, cx| {
            if let Err(error) = result {
                this.show_error(error, window, cx);
                return;
            }
            let editor = this
                .files
                .get(&path)
                .filter(|file| {
                    !file.document.conflict
                        && !file.missing
                        && !this.saving.contains(&path)
                        && !this.git_edit.contains(&path)
                        && file.editor.read(cx).history_group(undo) == Some(restore_group(id))
                })
                .map(|file| file.editor.clone());
            let Some(editor) = editor else {
                toast::info(window, cx, t!("git.editor_history_conflict"));
                return;
            };
            editor.update(cx, |editor, cx| {
                editor.replay_history_group(restore_group(id), undo, window, cx)
            });
            if this.view == WorkspaceView::Git && !this.tabs.paths().iter().any(|tab| tab == &path)
            {
                this.tabs.open_in_background(path);
                this.capture_session(cx);
            }
        });
    }

    fn replay_unrecorded_text_restore(
        &mut self,
        operation: Operation,
        target: Document,
        baseline: [u8; 32],
        undo: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = operation.paths.first().map(PathBuf::from) else {
            return;
        };
        let Some(repo) = self.repository.clone() else {
            return;
        };
        self.ensure_review_buffer(path.clone(), window, cx, move |this, result, window, cx| {
            if let Err(error) = result {
                this.show_error(error, window, cx);
                return;
            }
            let Some(file) = this.files.get(&path).filter(|file| {
                !file.document.conflict
                    && !file.missing
                    && !file.document.mixed_newlines
                    && !this.saving.contains(&path)
                    && !this.git_edit.contains(&path)
                    && file.committed == file.saved
                    && file.document.draft_baseline() == baseline
            }) else {
                toast::info(window, cx, t!("git.editor_history_conflict"));
                return;
            };
            let current = file.saved.to_string();
            if current == target.text {
                this.apply_git_history(operation, undo, window, cx);
                return;
            }
            let editor = file.editor.clone();
            editor.update(cx, |editor, cx| {
                let group = restore_group(operation.id);
                if undo {
                    editor.replace_projected_text(
                        0..current.len(),
                        target.text.clone(),
                        window,
                        cx,
                    );
                    let edit = super::editor_git::restore_text_edit(&target.text, &current);
                    editor.edit_ranges_in_group(&[edit], group, window, cx);
                    editor.replay_history_group(group, true, window, cx);
                } else {
                    let edit = super::editor_git::restore_text_edit(&current, &target.text);
                    editor.edit_ranges_in_group(&[edit], group, window, cx);
                }
            });
            this.editor_changed(editor, window, cx);
            if let Err(error) = this.flush_edit_history(cx, Some((&repo, operation.id, &path))) {
                this.show_error(error, window, cx);
            }
            if this.view == WorkspaceView::Git && !this.tabs.paths().iter().any(|tab| tab == &path)
            {
                this.tabs.open_in_background(path);
                this.capture_session(cx);
            }
        });
    }

    fn move_git_history_marker(&self, id: u64, undo: bool, window: &mut Window, cx: &mut App) {
        for source in self.git_history.editors.get(&id).into_iter().flatten() {
            _ = source.update(cx, |editor, cx| {
                editor.replay_history_group(id | GIT_GROUP, undo, window, cx)
            });
        }
    }

    fn apply_git_history(
        &mut self,
        operation: Operation,
        undo: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let blocked = self
            .git_history
            .editors
            .get(&operation.id)
            .into_iter()
            .flatten()
            .filter_map(WeakEntity::upgrade)
            .any(|source| source.read(cx).history_group(undo) != Some(operation.id | GIT_GROUP));
        if blocked {
            toast::info(window, cx, t!("git.editor_history_conflict"));
            return;
        }
        let disk = operation.action == FileAction::Restore;
        let path = if disk {
            let Some(path) = operation.paths.first() else {
                return;
            };
            Some(PathBuf::from(path))
        } else {
            None
        };
        let check = if disk {
            FileAction::Restore
        } else {
            FileAction::Unstage
        };
        if let Err(error) =
            self.begin_git_edit(operation.paths.iter().map(PathBuf::from), check, cx)
        {
            self.show_error(error, window, cx);
            return;
        }
        let Some(repo) = self.repository.clone() else {
            return;
        };
        let working_repo = repo.clone();
        let history_path = path.clone();
        let id = operation.id;
        let work = cx.background_spawn(async move {
            working_repo.git_backend().replay_operation(id, undo)?;
            Ok::<_, anyhow::Error>(
                history_path
                    .as_deref()
                    .map(|path| Self::restored_document(&working_repo, path)),
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            _ = this.update_in(cx, |workspace, window, cx| {
                if !workspace
                    .repository
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &repo))
                {
                    return;
                }
                let (success, document) = match result {
                    Ok(document) => {
                        workspace.move_git_history_marker(id, undo, window, cx);
                        (true, document)
                    }
                    Err(error) => {
                        workspace.show_error(error, window, cx);
                        (false, None)
                    }
                };
                if let Some(path) = path {
                    workspace.finish_git_edit(&path, document, false, window, cx);
                } else {
                    workspace.finish_stage_files(&operation.paths, success, window, cx);
                }
                if let Some(panel) = &workspace.git {
                    panel.update(cx, |panel, cx| {
                        panel.refresh_changed_paths(operation.paths, window, cx)
                    });
                }
                cx.notify();
            });
        })
        .detach();
    }
}

fn source_for_history(source: &Option<PathBuf>, operation: &Operation) -> bool {
    source
        .as_deref()
        .is_none_or(|source| operation.paths.iter().any(|path| Path::new(path) == source))
}
