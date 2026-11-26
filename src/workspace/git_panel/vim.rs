use gpui::*;
use gpui_component::input::{self, EditorState};

use super::{GitPanel, review::has_live_target};
use crate::session::{FilePosition, GitHistoryPane, SourceLocation};
use crate::workspace::{
    SaveFile,
    session::{restore_file_selection, source_file_position},
    vim::{
        plain_key,
        source::{Action, EditGuard, Find, Mode, Source},
    },
};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Column {
    #[default]
    Modified,
    Original,
    Snapshot,
}

pub(super) struct DiffVim {
    enabled: bool,
    columns: [Source; 3],
    active: Column,
    hunk_prefix: Option<char>,
    find: Option<Find>,
    insert_source: Option<Entity<EditorState>>,
}

impl DiffVim {
    pub(super) fn new(guard: EditGuard) -> Self {
        let mut columns = std::array::from_fn(|_| Source::default());
        columns[Column::Modified as usize].set_edit_guard(guard);
        Self {
            enabled: false,
            columns,
            active: Column::Modified,
            hunk_prefix: None,
            find: None,
            insert_source: None,
        }
    }
}

impl GitPanel {
    fn diff_editor(&self, column: Column) -> Option<&Entity<EditorState>> {
        match column {
            Column::Original => Some(self.editor.original_state()),
            Column::Modified => Some(self.editor.modified_state()),
            Column::Snapshot => self.snapshot.as_ref()?.editor.as_ref(),
        }
    }

    fn focused_diff_column(&self, window: &Window, cx: &App) -> Option<Column> {
        let editor = self.search_editor(window, cx);
        if !editor.read(cx).focus_handle(cx).is_focused(window) {
            return None;
        }
        if self.snapshot.is_some() {
            return self.diff_editor(Column::Snapshot).map(|_| Column::Snapshot);
        }
        self.review.as_ref()?;
        Some(if &editor == self.editor.original_state() {
            Column::Original
        } else {
            Column::Modified
        })
    }

    pub(in crate::workspace) fn navigation_history_location(
        &self,
        window: &Window,
        cx: &App,
    ) -> SourceLocation {
        let pane = if let Some(column) = self.focused_diff_column(window, cx) {
            match column {
                Column::Original => GitHistoryPane::Original,
                Column::Modified => GitHistoryPane::Modified,
                Column::Snapshot => GitHistoryPane::Snapshot,
            }
        } else if self.history_focus.is_focused(window) {
            GitHistoryPane::History
        } else if self.vim_tree_focus.is_focused(window) {
            GitHistoryPane::ChangedFiles
        } else {
            GitHistoryPane::Review
        };
        let position = self
            .history_diff_editor(pane)
            .map(|editor| source_file_position(editor.read(cx), None));
        SourceLocation {
            path: None,
            position,
            document: None,
            git: Some(pane),
        }
    }

    pub(in crate::workspace) fn restore_history_location(
        &mut self,
        pane: GitHistoryPane,
        position: Option<FilePosition>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self.history_diff_editor(pane) {
            editor.update(cx, |editor, cx| {
                if let Some(position) = position {
                    use gpui_component::input::Position;
                    restore_file_selection(editor, position, cx);
                    editor.scroll_to_viewport_anchor(
                        Position::new(position.top.0, position.top.1),
                        px(position.inset),
                        px(position.horizontal),
                        false,
                        cx,
                    );
                }
                editor.focus(window, cx);
            });
            return;
        }
        match pane {
            GitHistoryPane::History => self.history_focus.focus(window, cx),
            GitHistoryPane::ChangedFiles => self.focus_vim_file_tree(window, cx),
            _ => self.focus(window, cx),
        }
    }

    fn history_diff_editor(&self, pane: GitHistoryPane) -> Option<&Entity<EditorState>> {
        let column = match pane {
            GitHistoryPane::Original => Column::Original,
            GitHistoryPane::Modified => Column::Modified,
            GitHistoryPane::Snapshot => Column::Snapshot,
            _ => return None,
        };
        self.diff_editor(column)
    }

    pub(super) fn observe_vim_diff(
        editor: &Entity<EditorState>,
        column: Column,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> [Subscription; 3] {
        let focus = editor.read(cx).focus_handle(cx);
        [
            cx.on_focus(&focus, window, move |panel, _, cx| {
                if panel.diff_vim.enabled && panel.diff_vim.active != column {
                    panel.diff_vim.active = column;
                    App::notify(cx, panel.workspace.entity_id());
                }
            }),
            cx.on_focus_out(&focus, window, |panel, _, _, cx| {
                panel.finish_diff_insert_history(cx);
                if panel.clear_diff_vim_keys() {
                    App::notify(cx, panel.workspace.entity_id());
                }
            }),
            cx.observe_in(editor, window, move |panel, editor, window, cx| {
                if !panel.diff_vim.enabled || !editor.read(cx).focus_handle(cx).is_focused(window) {
                    return;
                }
                let vim = &mut panel.diff_vim.columns[column as usize];
                if vim.sync_selection(editor.read(cx)) {
                    editor.update(cx, |editor, cx| vim.adopt_selection(editor, cx));
                    App::notify(cx, panel.workspace.entity_id());
                }
            }),
        ]
    }

    pub(in crate::workspace) fn set_diff_vim(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.diff_vim.enabled = enabled;
        self.reset_diff_vim(cx);
    }

    pub(super) fn reset_diff_vim(&mut self, cx: &mut Context<Self>) {
        self.finish_diff_insert_history(cx);
        self.diff_vim.find = None;
        self.diff_vim.hunk_prefix = None;
        let enabled = self.diff_vim.enabled;
        for column in [Column::Original, Column::Modified, Column::Snapshot] {
            if let Some(editor) = self.diff_editor(column).cloned() {
                self.diff_vim.columns[column as usize].reset(&editor, cx);
                editor.update(cx, |editor, cx| editor.set_modal_editing(enabled, cx));
            }
        }
        App::notify(cx, self.review_pane.entity_id());
    }

    pub(super) fn focus_loaded_vim_diff(&self, window: &mut Window, cx: &mut App) {
        if self.diff_vim.enabled && self.focus.is_focused(window) {
            self.editor.focus(window, cx);
        }
    }

    pub(super) fn group_diff_insert_history(&mut self, editor: &Entity<EditorState>, cx: &mut App) {
        if self.diff_vim.columns[Column::Modified as usize].mode() == Mode::Insert
            && self.diff_vim.insert_source.as_ref() != Some(editor)
        {
            self.finish_diff_insert_history(cx);
            editor.update(cx, |editor, _| editor.begin_edit_transaction());
            self.diff_vim.insert_source = Some(editor.clone());
        }
    }

    fn finish_diff_insert_history(&mut self, cx: &mut App) {
        if let Some(editor) = self.diff_vim.insert_source.take() {
            editor.update(cx, |editor, _| editor.end_edit_transaction());
        }
    }

    pub(super) fn install_snapshot_vim(
        &mut self,
        editor: &Entity<EditorState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> [Subscription; 3] {
        self.diff_vim.columns[Column::Snapshot as usize] = Source::default();
        self.diff_vim.find = None;
        editor.update(cx, |editor, cx| {
            editor.set_modal_editing(self.diff_vim.enabled, cx)
        });
        Self::observe_vim_diff(editor, Column::Snapshot, window, cx)
    }

    pub(in crate::workspace) fn clear_diff_vim_keys(&mut self) -> bool {
        let mut changed = self.diff_vim.hunk_prefix.take().is_some();
        for vim in &mut self.diff_vim.columns {
            changed |= vim.clear_pending();
        }
        changed
    }

    pub(super) fn vim_diff_accepts_leader(&self, window: &Window, cx: &App) -> bool {
        self.focused_diff_column(window, cx).is_some_and(|column| {
            self.diff_vim.hunk_prefix.is_none()
                && self.diff_vim.columns[column as usize].accepts_leader()
                && !self.vim_diff_composing(window, cx)
        })
    }

    pub(in crate::workspace) fn vim_diff_command_mode(&self, window: &Window, cx: &App) -> bool {
        self.focused_diff_column(window, cx)
            .is_some_and(|column| self.diff_vim.columns[column as usize].mode() != Mode::Insert)
    }

    pub(in crate::workspace) fn vim_diff_composing(&self, window: &Window, cx: &App) -> bool {
        self.focused_diff_column(window, cx).is_some()
            && self.search_editor(window, cx).read(cx).is_composing()
    }

    pub(in crate::workspace) fn vim_diff_key(
        &mut self,
        key: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let editor = self.search_editor(window, cx);
        if !key.modifiers.modified()
            && matches!(key.key.as_str(), "enter" | "escape")
            && self
                .diff_vim
                .find
                .as_ref()
                .is_some_and(|find| find.is_active(&editor, window, cx))
        {
            let find = self.diff_vim.find.take().unwrap();
            let vim = &mut self.diff_vim.columns[self.diff_vim.active as usize];
            find.finish(vim, key.key == "enter", window, cx);
            App::notify(cx, self.workspace.entity_id());
            return true;
        }
        let Some(column) = self.focused_diff_column(window, cx) else {
            return false;
        };
        let state = editor.read(cx);
        if state.is_composing()
            || state.completion_menu_state().open
            || state.code_action_menu_state().open
        {
            return false;
        }
        self.diff_vim.active = column;
        if plain_key(key) == Some(' ') && self.vim_diff_accepts_leader(window, cx) {
            return false;
        }
        let locked = column != Column::Modified
            || self
                .review
                .as_ref()
                .is_none_or(|review| !has_live_target(&review.detail.comparison));
        let vim = &mut self.diff_vim.columns[column as usize];
        let handled = if key.key == "escape" && !key.modifiers.modified() {
            let insert = vim.mode() == Mode::Insert;
            let escaped =
                self.diff_vim.hunk_prefix.take().is_some() || vim.escape(&editor, window, cx);
            if escaped
                && insert
                && let Some(source) = self.diff_vim.insert_source.take()
            {
                // Escape can generate a final edit. Commit after it reaches the source.
                cx.defer(move |cx| source.update(cx, |editor, _| editor.end_edit_transaction()));
            }
            escaped
        } else if vim.mode() == Mode::Insert {
            false
        } else {
            let previous = self.diff_vim.hunk_prefix.take();
            match (previous, plain_key(key)) {
                (None, Some(prefix @ ('[' | ']')))
                    if column != Column::Snapshot && vim.pending().is_empty() =>
                {
                    self.diff_vim.hunk_prefix = Some(prefix);
                    true
                }
                (Some(prefix), Some('c')) => {
                    self.pending_file_reveal = None;
                    self.pending_hunk_navigation = None;
                    self.diff.update(cx, |diff, cx| {
                        if prefix == '[' {
                            diff.previous_change(cx);
                        } else {
                            diff.next_change(cx);
                        }
                    });
                    let diff = self.diff.read(cx);
                    if diff.has_projected_navigation()
                        && let Some(offset) = self
                            .editor
                            .hunk_caret_offset(&editor, diff.editor_viewport_position().0)
                    {
                        editor.update(cx, |editor, cx| vim.jump_head(editor, offset, cx));
                    }
                    true
                }
                (Some(_), _) => true,
                _ => match vim.key_down(&editor, locked, key, self.wrap, window, cx) {
                    Some(action) => {
                        match action {
                            Action::Find(backward) => {
                                self.diff_vim.find = Some(Find::capture(&editor, backward, cx));
                                self.find(window, cx);
                            }
                            Action::Definition => {
                                window.dispatch_action(Box::new(input::GoToDefinition), cx)
                            }
                            Action::Save if !locked => {
                                window.dispatch_action(Box::new(SaveFile), cx)
                            }
                            _ => {}
                        }
                        true
                    }
                    None => {
                        !key.modifiers.modified() && matches!(key.key.as_str(), "tab" | "enter")
                    }
                },
            }
        };
        if handled {
            App::notify(cx, self.workspace.entity_id());
        }
        handled
    }
}
