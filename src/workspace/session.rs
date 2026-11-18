use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;
use gpui::*;
use gpui_component::input::{EditorState, Position, RopeExt as _};

use crate::{
    repository::Store,
    session::{FilePosition, StandaloneSession, WorktreeSession},
};

use super::{Workspace, WorkspaceView};

enum SavedSession {
    Worktree(WorktreeSession),
    Standalone(StandaloneSession),
}

impl SavedSession {
    fn write(&self, path: &Path) -> Result<()> {
        match self {
            Self::Worktree(session) => Store::write_session_file(path, session),
            Self::Standalone(session) => Store::write_session_file(path, session),
        }
    }
}

impl Workspace {
    // The viewport handles also change during scrollbar drags and smooth scrolling.
    // Sample them together at a bounded rate, with a synchronous flush at lifecycle boundaries.
    pub(super) fn start_session_writer(&self, cx: &mut Context<Self>) {
        cx.spawn(async |workspace, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(750))
                    .await;
                let Ok(write) = workspace.update(cx, |this, cx| {
                    this.capture_session(cx);
                    if this.session_revision == *this.session_write.lock().unwrap() {
                        return None;
                    }
                    let (path, session) = this.saved_session()?;
                    let state = this.session_write.clone();
                    let revision = this.session_revision;
                    let write = cx.background_spawn(async move {
                        let mut written = state.lock().unwrap();
                        if revision > *written {
                            session.write(&path)?;
                            *written = revision;
                        }
                        Ok::<_, anyhow::Error>(())
                    });
                    Some((revision, write))
                }) else {
                    break;
                };
                if let Some((revision, write)) = write
                    && let Err(error) = write.await
                {
                    let _ = workspace.update(cx, |this, cx| {
                        if revision == this.session_revision {
                            // The session writer runs without a window to own a toast.
                            eprintln!("session write failed: {error:#}");
                            cx.notify();
                        }
                    });
                }
            }
        })
        .detach();
    }

    pub(super) fn capture_session(&mut self, cx: &App) {
        if self.repository.is_none() && self.standalone.is_none() {
            return;
        }
        let disconnected = self
            .repository
            .as_ref()
            .and_then(|repo| repo.ssh.as_ref())
            .is_some_and(|ssh| ssh.failure().is_some());
        let mut panels = self.session.panels;
        panels.editor_tree_width = self.editor_tree_width.into();
        panels.docs_tree_width = self.docs_tree_width.into();
        panels.doc_outline_width = self.outline_width.into();
        panels.doc_outline_visible = self.outline_visibility;
        if let Some(panel) = &self.git {
            panel.read(cx).capture_panel_preferences(&mut panels);
        }
        if panels != self.session.panels {
            self.session.panels = panels;
            self.session_revision += 1;
        }
        if self.session.view != Some(self.view)
            && self
                .standalone
                .as_deref()
                .is_none_or(|anchor| self.standalone_active_path() == Some(anchor))
        {
            self.session.view = Some(self.view);
            self.session_revision += 1;
        }
        if self.session.tabs != self.tabs {
            self.session.tabs = self.tabs.clone();
            self.session_revision += 1;
        }
        if let Some(panel) = &self.git {
            let message = panel.read(cx).commit_draft(cx);
            let draft = (!message.is_empty()).then_some(message.as_str());
            if self.session.commit_draft.as_deref() != draft {
                self.session.commit_draft = draft.map(str::to_owned);
                self.session_revision += 1;
            }
        }
        match self.view {
            WorkspaceView::Editor => {
                self.capture_active_file_position(cx);
                self.capture_source_viewport(cx);
            }
            WorkspaceView::Docs => self.capture_document_position(cx),
            WorkspaceView::Git if !disconnected => {
                if let Some(panel) = &self.git
                    && let Some(state) = panel.read(cx).session(cx)
                    && self.session.git.as_ref() != Some(&state)
                {
                    self.session.git = Some(state);
                    self.session_revision += 1;
                }
            }
            WorkspaceView::Git => {}
        }
    }

    pub(super) fn capture_active_file_position(&mut self, cx: &App) {
        if let Some(path) = self.tabs.active().map(Path::to_owned)
            && let Some(file) = self.files.get(&path)
            && file.restore_position.is_none()
            && let Some(position) = file_position(file.editor.read(cx))
            && self.session.files.get(&path) != Some(&position)
        {
            self.session.files.insert(path, position);
            self.session_revision += 1;
        }
    }

    pub(super) fn capture_document_position(&mut self, cx: &App) {
        let Some(doc) = self.doc.as_ref().map(|doc| doc.read(cx)) else {
            return;
        };
        if let Some(position) = doc.session_position(cx)
            && self.session.documents.get(&doc.path) != Some(&position)
        {
            self.session.documents.insert(doc.path.clone(), position);
            self.session_revision += 1;
        }
    }

    fn saved_session(&self) -> Option<(PathBuf, SavedSession)> {
        if let Some(path) = &self.standalone {
            return Some((
                self.store.standalone_session_path(path),
                SavedSession::Standalone(StandaloneSession {
                    view: self.session.view,
                    panels: self.session.panels,
                    file_position: self.session.files.get(path).copied(),
                    document_position: self.session.documents.get(path).copied(),
                }),
            ));
        }
        let repo = self.repository.as_ref()?;
        Some((
            self.store.session_path(repo),
            SavedSession::Worktree(self.session.clone()),
        ))
    }

    pub(super) fn restore_panel_preferences(&mut self) {
        let panels = self.session.panels;
        self.editor_tree_width = px(panels.editor_tree_width.max(160.));
        self.docs_tree_width = px(panels.docs_tree_width.max(160.));
        self.outline_width = px(panels.doc_outline_width.max(120.));
        self.outline_visibility = panels.doc_outline_visible;
    }

    pub(super) fn flush_session(&mut self, cx: &App) -> Result<()> {
        self.capture_session(cx);
        let mut written = self.session_write.lock().unwrap();
        if *written < self.session_revision
            && let Some((path, session)) = self.saved_session()
        {
            session.write(&path)?;
            *written = self.session_revision;
        }
        Ok(())
    }

    pub(super) fn restore_file_viewport(&self, window: &mut Window, cx: &Context<Self>) {
        if self.view != WorkspaceView::Editor || self.terminal_maximized() {
            return;
        }
        if let Some(path) = self.tabs.active().map(Path::to_owned)
            && self
                .files
                .get(&path)
                .is_some_and(|file| file.restore_position.is_some())
        {
            cx.on_next_frame(window, move |this, _, cx| {
                if this.view != WorkspaceView::Editor || this.tabs.active() != Some(path.as_path())
                {
                    return;
                }
                if let Some(file) = this.files.get_mut(&path) {
                    file.editor.update(cx, |editor, cx| {
                        if editor.line_height().is_some()
                            && let Some(position) = file.restore_position.take()
                        {
                            editor.restore_viewport_anchor(
                                Position::new(position.top.0, position.top.1),
                                px(position.inset),
                                px(position.horizontal),
                                cx,
                            );
                        }
                    });
                }
            });
        }
    }
}

fn file_position(editor: &EditorState) -> Option<FilePosition> {
    Some(file_position_at(editor, editor.viewport_anchor()?))
}

pub(super) fn source_file_position(
    editor: &EditorState,
    pending: Option<FilePosition>,
) -> FilePosition {
    let mut position = file_position_at(editor, editor.viewport_anchor().unwrap_or_default());
    if let Some(pending) = pending {
        position.top = pending.top;
        position.inset = pending.inset;
        position.horizontal = pending.horizontal;
    }
    position
}

fn file_position_at(editor: &EditorState, (top, inset): (Position, Pixels)) -> FilePosition {
    let selection = editor.selected_range();
    let start = editor.text().offset_to_position(selection.start);
    let end = editor.text().offset_to_position(selection.end);
    FilePosition {
        selection_start: (start.line, start.character),
        selection_end: (end.line, end.character),
        reversed: editor.is_selection_reversed(),
        top: (top.line, top.character),
        inset: inset.into(),
        horizontal: editor.scroll_offset().x.into(),
    }
}

pub(super) fn restore_file_selection(
    editor: &mut EditorState,
    position: FilePosition,
    cx: &mut Context<EditorState>,
) {
    let start = editor.text().position_to_offset(&Position::new(
        position.selection_start.0,
        position.selection_start.1,
    ));
    let end = editor.text().position_to_offset(&Position::new(
        position.selection_end.0,
        position.selection_end.1,
    ));
    editor.set_selected_range_with_direction(start.min(end)..end, position.reversed, cx);
}
