use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

use super::{
    FileOpen, Workspace, WorkspaceView,
    session::{restore_file_selection, source_file_position},
};
use crate::session::FilePosition;
use gpui::*;
use gpui_component::{WindowExt as _, input::Position};

const HISTORY_LIMIT: usize = 80;

actions!(workspace, [SourceBack, SourceForward]);

#[derive(Clone)]
pub(super) struct SourceLocation {
    path: PathBuf,
    position: Option<FilePosition>,
}

impl SourceLocation {
    fn same_destination(&self, other: &Self) -> bool {
        self.path == other.path
            && match (self.position, other.position) {
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

#[derive(Default)]
pub(super) struct SourceHistory {
    entries: VecDeque<SourceLocation>,
    index: usize,
}

impl SourceHistory {
    fn record(&mut self, source: Option<SourceLocation>, target: SourceLocation) {
        self.entries.truncate(self.index + 1);
        if let Some(source) = source {
            if let Some(current) = self
                .entries
                .back_mut()
                .filter(|entry| entry.path == source.path)
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
            && current.path == source.path
        {
            *current = source;
        }
        self.index = index;
    }

    pub(super) fn remap(&mut self, map: impl Fn(&Path) -> PathBuf) {
        for entry in &mut self.entries {
            entry.path = map(&entry.path);
        }
    }

    pub(super) fn remove(&mut self, path: &Path) {
        let removed_before = self
            .entries
            .iter()
            .take(self.index)
            .filter(|entry| entry.path.starts_with(path))
            .count();
        self.entries.retain(|entry| !entry.path.starts_with(path));
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
        let context = Some("WorkspaceEditor && !Terminal && !Locator");
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
        if self.view != WorkspaceView::Editor
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
        let Some(history) = self.source_history() else {
            return;
        };
        let cursor = match &self.loading_file {
            Some((_, FileOpen::History { index, .. })) => *index,
            _ => history.index,
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
        if index == history.index {
            self.cancel_file_open();
            cx.notify();
            return;
        }
        self.request_file(
            location.path,
            FileOpen::History {
                index,
                position: location.position,
            },
            window,
            cx,
        );
    }

    fn source_history(&self) -> Option<&SourceHistory> {
        self.source_histories
            .get(self.repository.as_ref()?.storage_key())
    }

    pub(super) fn source_history_mut(&mut self) -> Option<&mut SourceHistory> {
        self.source_histories
            .get_mut(self.repository.as_ref()?.storage_key())
    }

    pub(super) fn source_location(&self, cx: &App) -> Option<SourceLocation> {
        self.repository.as_ref()?;
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
        Some(SourceLocation { path, position })
    }

    pub(super) fn record_source_navigation(&mut self, source: Option<SourceLocation>, cx: &App) {
        let Some((repo, target)) = self.repository.as_ref().zip(self.source_location(cx)) else {
            return;
        };
        if source
            .as_ref()
            .is_some_and(|source| source.same_destination(&target))
        {
            return;
        }
        self.source_histories
            .entry(repo.storage_key().to_vec())
            .or_default()
            .record(source, target);
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
