use std::path::{Path, PathBuf};

use super::{
    FileOpen, Workspace, WorkspaceView,
    session::{restore_file_selection, source_file_position},
};
use crate::session::{FilePosition, SourceHistory, SourceLocation};
use gpui::*;
use gpui_component::{WindowExt as _, input::Position};

const HISTORY_LIMIT: usize = 80;
// Page and structural keyboard moves can create a stop. Ordinary line-by-line movement does not.
const CARET_JUMP_LINES: u32 = 8;

actions!(workspace, [SourceBack, SourceForward]);

impl SourceLocation {
    pub(super) fn empty(view: WorkspaceView) -> Self {
        Self {
            path: None,
            position: None,
            document: (view == WorkspaceView::Docs).then(Default::default),
        }
    }

    fn same_document(&self, other: &Self) -> bool {
        self.path == other.path && self.document.is_some() && other.document.is_some()
    }

    fn same_destination(&self, other: &Self) -> bool {
        if self.path != other.path {
            return false;
        }
        if self.document.is_some() || other.document.is_some() {
            return self.document == other.document;
        }
        match (self.position, other.position) {
            (Some(first), Some(second)) => {
                first.selection_start == second.selection_start
                    && first.selection_end == second.selection_end
                    && first.reversed == second.reversed
            }
            (None, None) => true,
            _ => false,
        }
    }
}

impl SourceHistory {
    pub(super) fn normalize(&mut self) {
        while self.entries.len() > HISTORY_LIMIT {
            self.entries.pop_front();
            self.index = self.index.saturating_sub(1);
        }
        self.index = self.index.min(self.entries.len().saturating_sub(1));
    }

    fn record(&mut self, source: Option<SourceLocation>, target: SourceLocation) {
        self.entries.truncate(self.index + 1);
        if let Some(source) = source {
            if let Some(current) = self
                .entries
                .back_mut()
                .filter(|entry| entry.same_destination(&source) || entry.same_document(&source))
            {
                *current = source;
            } else {
                self.entries.push_back(source);
            }
        }
        self.entries.push_back(target);
        while self.entries.len() > HISTORY_LIMIT {
            self.entries.pop_front();
        }
        self.index = self.entries.len() - 1;
    }

    fn replay(&mut self, index: usize, source: Option<SourceLocation>) {
        if let Some(source) = source
            && let Some(current) = self.entries.get_mut(self.index)
            && current.same_destination(&source)
        {
            *current = source;
        }
        self.index = index;
    }

    pub(super) fn remap(&mut self, map: impl Fn(&Path) -> PathBuf) {
        for entry in &mut self.entries {
            entry.path = entry.path.as_deref().map(&map);
        }
    }

    pub(super) fn remove(&mut self, path: &Path) {
        let removed = |entry: &SourceLocation| {
            entry
                .path
                .as_deref()
                .is_some_and(|file| file.starts_with(path))
        };
        let removed_before = self
            .entries
            .iter()
            .take(self.index)
            .filter(|entry| removed(entry))
            .count();
        self.entries.retain(|entry| !removed(entry));
        self.index = self
            .index
            .saturating_sub(removed_before)
            .min(self.entries.len().saturating_sub(1));
    }
}

impl Workspace {
    pub(super) fn bind_source_navigation_keys(cx: &mut App) {
        let (back, forward) = if cfg!(target_os = "macos") {
            ("ctrl--", "ctrl-shift--")
        } else {
            ("alt-left", "alt-right")
        };
        let context = Some("(WorkspaceEditor || WorkspaceDocs) && !Terminal && !Locator");
        cx.bind_keys([
            KeyBinding::new(back, SourceBack, context),
            KeyBinding::new(forward, SourceForward, context),
        ]);
    }

    pub(super) fn source_back(
        &mut self,
        _: &SourceBack,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate_source(false, window, cx);
    }

    pub(super) fn source_forward(
        &mut self,
        _: &SourceForward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate_source(true, window, cx);
    }

    pub(super) fn source_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let MouseButton::Navigate(direction) = event.button else {
            return;
        };
        // Preserve the focused input when GPUI handles the mouse-down event.
        if !matches!(self.view, WorkspaceView::Editor | WorkspaceView::Docs) {
            return;
        }
        window.prevent_default();
        self.navigate_source(direction == NavigationDirection::Forward, window, cx);
    }

    fn navigate_source(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (source_focused, composing) = self.active_file().map_or((false, false), |file| {
            let editor = file.editor.read(cx);
            (
                editor.focus_handle(cx).is_focused(window),
                editor.is_composing(),
            )
        });
        if !matches!(self.view, WorkspaceView::Editor | WorkspaceView::Docs)
            || self.confirming_close
            || self.repository_overlay_open()
            || self.repository_ui.opening.is_some()
            || self.locator.open
            || window.has_active_dialog(cx)
            || window.has_active_prompt()
            || self.terminal_focused(window, cx)
            || composing
            || (!source_focused
                && window
                    .context_stack()
                    .iter()
                    .any(|context| context.contains("Input")))
        {
            cx.propagate();
            return;
        }
        let history = &self.session.source_history;
        if !forward {
            let pending_origin = match self.view {
                WorkspaceView::Editor => match &self.loading_file {
                    Some((_, FileOpen::Restore { from })) => from.as_ref(),
                    _ => None,
                },
                WorkspaceView::Docs => {
                    self.doc_loading
                        .as_ref()
                        .and_then(|open| match &open.navigation {
                            Some(super::DocumentNavigation::Record(Some(from)))
                                if from.document.is_none() =>
                            {
                                Some(from)
                            }
                            _ => None,
                        })
                }
                WorkspaceView::Git => None,
            };
            if let Some(origin) = pending_origin.cloned() {
                // The view has switched, but loading has not committed its history entry.
                self.replay_source_location(history.index, origin, window, cx);
                return;
            }
        }
        let pending_document = self
            .doc_loading
            .as_ref()
            .and_then(|open| match &open.navigation {
                Some(super::DocumentNavigation::Replay { index, .. }) => Some(*index),
                _ => None,
            });
        let cursor = match &self.loading_file {
            Some((_, FileOpen::History { index, .. })) => *index,
            _ => pending_document.unwrap_or(history.index),
        };
        let index = if forward {
            cursor.checked_add(1)
        } else {
            cursor.checked_sub(1)
        };
        let Some((index, location)) =
            index.and_then(|index| Some((index, history.entries.get(index)?.clone())))
        else {
            return;
        };
        if index == history.index
            && ((self.view == WorkspaceView::Editor && location.document.is_none())
                || (self.view == WorkspaceView::Docs && location.document.is_some()))
        {
            self.cancel_file_open();
            self.cancel_document_navigation();
            cx.notify();
            return;
        }
        if self.view == WorkspaceView::Docs {
            self.capture_document_history_position(cx);
        } else {
            self.capture_source_viewport(cx);
        }
        self.replay_source_location(index, location, window, cx);
    }

    fn replay_source_location(
        &mut self,
        index: usize,
        location: SourceLocation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = location.path else {
            let view = if location.document.is_some() {
                WorkspaceView::Docs
            } else {
                WorkspaceView::Editor
            };
            self.restore_empty_view(view, window, cx);
            self.complete_navigation_replay(index);
            return;
        };
        if let Some(document) = location.document {
            self.open_history_document(path, index, document, window, cx);
        } else {
            if self.view == WorkspaceView::Docs {
                self.cancel_document_navigation();
                self.switch_view(WorkspaceView::Editor, window, cx);
            }
            if self.view == WorkspaceView::Editor {
                self.request_file(
                    path,
                    FileOpen::History {
                        index,
                        position: location.position,
                    },
                    window,
                    cx,
                );
            }
        }
    }

    fn restore_empty_view(
        &mut self,
        view: WorkspaceView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_file_open();
        match view {
            WorkspaceView::Docs => {
                self.capture_document_position(cx);
                self.doc_task = Task::ready(());
                self.doc_loading = None;
                self.doc = None;
                self.doc_error = None;
                self.session.document = None;
                self.session.open_documents.get_or_insert_default();
            }
            WorkspaceView::Editor => {
                self.capture_active_file_position(cx);
                self.cancel_document_navigation();
                let previous = self.tabs.deselect();
                if self.view == view {
                    self.finish_tab_selection(previous.as_deref(), window, cx);
                }
            }
            WorkspaceView::Git => return,
        }
        if self.view == view {
            self.rebuild_tree();
            cx.notify();
        } else {
            self.switch_view(view, window, cx);
        }
    }

    pub(super) fn topbar_location(&self, cx: &App) -> SourceLocation {
        match self.view {
            WorkspaceView::Docs => self.document_history_location(cx),
            WorkspaceView::Editor => self.source_location(cx),
            WorkspaceView::Git => None,
        }
        .unwrap_or_else(|| SourceLocation::empty(self.view))
    }

    pub(super) fn document_history_location(&self, cx: &App) -> Option<SourceLocation> {
        let reader = self.doc.as_ref()?.read(cx);
        Some(SourceLocation {
            path: Some(reader.path.clone()),
            position: None,
            document: Some(reader.history_location(cx)),
        })
    }

    pub(super) fn capture_document_history_position(&mut self, cx: &App) {
        let Some(location) = self.document_history_location(cx) else {
            return;
        };
        let history = &mut self.session.source_history;
        if let Some(current) = history.entries.get_mut(history.index)
            && current.same_document(&location)
            && *current != location
        {
            *current = location;
            self.session_revision += 1;
        }
    }

    pub(super) fn record_document_navigation(&mut self, source: Option<SourceLocation>, cx: &App) {
        if let Some(target) = self.document_history_location(cx) {
            self.record_navigation(source, target);
        }
    }

    pub(super) fn complete_navigation_replay(&mut self, index: usize) {
        if let Some(history) = self.source_history_mut() {
            history.replay(index, None);
        }
    }

    pub(super) fn skip_missing_source(
        &mut self,
        path: &Path,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(history) = self.source_history_mut() {
            let forward = index > history.index;
            history.remove(path);
            self.navigate_source(forward, window, cx);
        }
    }

    pub(super) fn source_history_mut(&mut self) -> Option<&mut SourceHistory> {
        if self.repository.is_none() && self.standalone.is_none() {
            return None;
        }
        self.session_revision += 1;
        Some(&mut self.session.source_history)
    }

    pub(super) fn track_source_caret(
        &mut self,
        editor: &Entity<gpui_component::input::EditorState>,
        window: &Window,
        cx: &App,
    ) {
        if self.view != WorkspaceView::Editor || self.repository.is_none() {
            return;
        }
        let Some(file) = self.active_file().filter(|file| file.editor == *editor) else {
            return;
        };
        let state = editor.read(cx);
        if file.restore_position.is_some()
            || !state.focus_handle(cx).is_focused(window)
            || state.is_composing()
        {
            return;
        }

        let Some(target) = self.source_location(cx) else {
            return;
        };
        let revision = state.document_revision();
        let previous = self.source_caret.replace((revision, target.clone()));
        let Some((previous_revision, previous)) = previous else {
            return;
        };
        let caret_row = |position: FilePosition| {
            if position.reversed {
                position.selection_start.0
            } else {
                position.selection_end.0
            }
        };
        let crossed_lines = previous
            .position
            .zip(target.position)
            .is_some_and(|(old, new)| caret_row(old).abs_diff(caret_row(new)) >= CARET_JUMP_LINES);
        if previous.path == target.path
            && previous_revision == revision
            && self.source_click.is_none()
            && crossed_lines
            && !previous.same_destination(&target)
        {
            self.record_source_navigation(Some(previous), cx);
        }
    }

    pub(super) fn begin_source_click(
        &mut self,
        event: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button == MouseButton::Left {
            self.source_click = self
                .active_file()
                .zip(self.source_location(cx))
                .map(|(file, source)| (file.editor.read(cx).document_revision(), source));
        }
    }

    pub(super) fn end_source_click(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        let Some((revision, source)) = self.source_click.take() else {
            return;
        };
        if let Some(file) = self
            .active_file()
            .filter(|file| Some(file.path.as_path()) == source.path.as_deref())
        {
            let editor = file.editor.read(cx);
            if editor.document_revision() == revision
                && editor.focus_handle(cx).is_focused(window)
                && !editor.is_composing()
            {
                self.record_source_navigation(Some(source), cx);
            }
        }
    }

    pub(super) fn source_location(&self, cx: &App) -> Option<SourceLocation> {
        if self.repository.is_none() && self.standalone.is_none() {
            return None;
        }
        let path = self.tabs.active()?.to_owned();
        let position = if let Some(file) = self.files.get(&path) {
            Some(source_file_position(
                file.editor.read(cx),
                file.restore_position,
            ))
        } else if self.images.contains_key(&path) {
            None
        } else {
            return None;
        };
        Some(SourceLocation {
            path: Some(path),
            position,
            document: None,
        })
    }

    pub(super) fn record_source_navigation(&mut self, source: Option<SourceLocation>, cx: &App) {
        let Some(target) = self.source_location(cx) else {
            return;
        };
        self.record_navigation(source, target);
    }

    pub(super) fn record_navigation(
        &mut self,
        source: Option<SourceLocation>,
        target: SourceLocation,
    ) {
        if source
            .as_ref()
            .is_some_and(|source| source.same_destination(&target))
        {
            return;
        }
        let history = &mut self.session.source_history;
        if source
            .as_ref()
            .is_none_or(|source| source.document.is_none() && target.document.is_none())
            && history
                .entries
                .get(history.index)
                .is_some_and(|entry| entry.same_destination(&target))
        {
            return;
        }
        history.record(source, target);
        self.session_revision += 1;
    }

    pub(super) fn capture_source_viewport(&mut self, cx: &App) {
        let Some(location) = self.source_location(cx) else {
            return;
        };
        let history = &mut self.session.source_history;
        if let Some(current) = history.entries.get_mut(history.index)
            && current.same_destination(&location)
            && *current != location
        {
            *current = location;
            self.session_revision += 1;
        }
    }

    pub(super) fn complete_source_replay(
        &mut self,
        path: &Path,
        index: usize,
        position: Option<FilePosition>,
        source: Option<SourceLocation>,
        cx: &mut Context<Self>,
    ) {
        if let Some(file) = self.files.get_mut(path)
            && let Some(position) = position
        {
            file.restore_position = Some(position);
            file.editor.update(cx, |editor, cx| {
                let (row, col) = position.selection_start;
                editor.unfold_at(Position::new(row, col), cx);
                let (row, col) = position.selection_end;
                editor.unfold_at(Position::new(row, col), cx);
                restore_file_selection(editor, position, cx);
            });
        }
        if let Some(history) = self.source_history_mut() {
            history.replay(index, source);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
