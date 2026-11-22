use std::{collections::VecDeque, ops::Range, time::Duration};

use gpui::{App, Context, Entity, Focusable as _, Task, Window};
use gpui_component::input::{
    EditorState, RangeDecoration, RangeDecorationCollection, RangeDecorationStyle, RopeExt as _,
};

use super::{Workspace, WorkspaceView};

const HIGHLIGHT_LIMIT: usize = 2_000;

#[derive(Clone, PartialEq, Eq)]
struct Selection {
    range: Range<usize>,
    revision: u64,
}

pub(super) struct SelectionHighlight {
    selection: Option<Selection>,
    decorations: RangeDecorationCollection,
    task: Task<()>,
}

impl SelectionHighlight {
    pub(super) fn new(editor: &mut EditorState, cx: &mut Context<EditorState>) -> Self {
        Self {
            selection: None,
            decorations: editor.create_range_decorations_collection(Vec::new(), cx),
            task: Task::ready(()),
        }
    }

    #[cfg(test)]
    pub(super) fn test_ranges(&self, cx: &App) -> Vec<Range<usize>> {
        self.decorations.get_ranges(cx)
    }

    fn clear(&mut self, cx: &mut App) {
        if self.selection.take().is_some() {
            self.task = Task::ready(());
            self.decorations.clear(cx);
        }
    }
}

impl Workspace {
    pub(super) fn refresh_active_selection_highlight(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self.active_file().map(|file| file.editor.clone()) {
            self.refresh_selection_highlight(&editor, window, cx);
        }
    }

    pub(super) fn refresh_selection_highlight(
        &mut self,
        editor: &Entity<EditorState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.highlighted_selection(editor, window, cx);
        let Some(file) = self.files.values_mut().find(|file| file.editor == *editor) else {
            return;
        };
        let highlight = &mut file.selection_highlight;
        if highlight.selection == range {
            return;
        }
        highlight.clear(cx);
        let Some(selection) = range else {
            return;
        };

        let state = editor.read(cx);
        let text = state.text().clone();
        let priority = state
            .visible_row_range()
            .map_or(0, |rows| text.line_start_offset(rows.start));
        let needle = text.slice(selection.range.clone()).to_string();
        highlight.selection = Some(selection.clone());
        let editor = editor.downgrade();
        highlight.task = cx.spawn_in(window, async move |workspace, cx| {
            let executor = cx.background_executor();
            executor.timer(Duration::from_millis(65)).await;
            let selected = selection.range.clone();
            let ranges = executor
                .spawn(async move { occurrences(&text.to_string(), &needle, selected, priority) })
                .await;
            _ = workspace.update_in(cx, |this, window, cx| {
                let Some(editor) = editor.upgrade() else {
                    return;
                };
                if this.highlighted_selection(&editor, window, cx).as_ref() != Some(&selection) {
                    return;
                }
                let Some(file) = this.files.values().find(|file| file.editor == editor) else {
                    return;
                };
                file.selection_highlight.decorations.set(
                    ranges
                        .into_iter()
                        .map(|range| {
                            RangeDecoration::new(range).with_style(RangeDecorationStyle::Fill)
                        })
                        .collect(),
                    cx,
                );
            });
        });
    }

    fn highlighted_selection(
        &self,
        editor: &Entity<EditorState>,
        window: &Window,
        cx: &App,
    ) -> Option<Selection> {
        let state = editor.read(cx);
        if !self.config.current().editor.selection_highlight
            || self.view != WorkspaceView::Editor
            || !self
                .active_file()
                .is_some_and(|file| file.editor == *editor && !file.missing)
            || !window.is_window_active()
            || !state.focus_handle(cx).is_focused(window)
            || state.is_composing()
            || state.is_selecting_text()
            || state.search_session().is_active()
        {
            return None;
        }
        let range = state.explicit_search_selection()?;
        if range.len() > 128 * 4 {
            return None;
        }
        let selected = state.text().slice(range.clone()).to_string();
        (!selected.contains(['\r', '\n'])
            && !selected.trim().is_empty()
            && selected.chars().count() <= 128)
            .then_some(Selection {
                range,
                revision: state.document_revision(),
            })
    }
}

fn occurrences(
    text: &str,
    needle: &str,
    selected: Range<usize>,
    priority: usize,
) -> Vec<Range<usize>> {
    let mut preceding = VecDeque::new();
    let mut matches = Vec::new();
    for (start, _) in text.match_indices(needle) {
        let range = start..start + needle.len();
        if range.start < selected.end && selected.start < range.end {
            continue;
        }
        if range.end <= priority {
            if preceding.len() == HIGHLIGHT_LIMIT {
                preceding.pop_front();
            }
            preceding.push_back(range);
        } else {
            matches.push(range);
            if matches.len() == HIGHLIGHT_LIMIT {
                break;
            }
        }
    }
    preceding
        .into_iter()
        .rev()
        .take(HIGHLIGHT_LIMIT - matches.len())
        .chain(matches)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_selection_occurrences_exclude_the_selection_and_prefer_the_viewport() {
        assert_eq!(
            occurrences("foo Foo greatfoo foo", "foo", 0..3, 0),
            [13..16, 17..20]
        );
        let text = "x ".repeat(5_000);
        let matches = occurrences(&text, "x", 0..1, 6_000);
        assert_eq!(matches.len(), HIGHLIGHT_LIMIT);
        assert_eq!(matches[0], 6_000..6_001);
        assert_eq!(matches.last(), Some(&(9_998..9_999)));
    }
}
