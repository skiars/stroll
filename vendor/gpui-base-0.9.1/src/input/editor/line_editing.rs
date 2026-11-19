use std::ops::Range;

use gpui::{Action, App, Context, KeyBinding, Window};
use ropey::Rope;

use super::{
    InputBaseState, InputModeKind, RopeExt, cursor::CursorSelection, state::EditSelection,
};

#[derive(Clone, Copy, PartialEq)]
enum Direction {
    Up,
    Down,
}

#[derive(Clone, Copy, PartialEq)]
enum Operation {
    Delete,
    Duplicate(Direction),
    Move(Direction),
    ToggleComment,
}

#[derive(Action, Clone, PartialEq)]
#[action(namespace = input, no_json)]
pub(super) struct EditLines(Operation);

pub(super) fn init(cx: &mut App) {
    use Direction::{Down, Up};
    use Operation::{Delete, Duplicate, Move, ToggleComment};

    const CONTEXT: Option<&str> = Some("Input && LineCommands");
    let (delete, comment) = if cfg!(target_os = "macos") {
        ("cmd-shift-k", "cmd-/")
    } else {
        ("ctrl-shift-k", "ctrl-/")
    };
    // Linux already uses Alt+Shift+arrows for adding cursors.
    let (copy_up, copy_down) = if cfg!(any(target_os = "macos", target_os = "windows")) {
        ("alt-shift-up", "alt-shift-down")
    } else {
        ("ctrl-shift-up", "ctrl-shift-down")
    };
    cx.bind_keys([
        KeyBinding::new(delete, EditLines(Delete), CONTEXT),
        KeyBinding::new("alt-up", EditLines(Move(Up)), CONTEXT),
        KeyBinding::new("alt-down", EditLines(Move(Down)), CONTEXT),
        KeyBinding::new(copy_up, EditLines(Duplicate(Up)), CONTEXT),
        KeyBinding::new(copy_down, EditLines(Duplicate(Down)), CONTEXT),
        KeyBinding::new(comment, EditLines(ToggleComment), CONTEXT),
    ]);
}

type LineReplacement = (Range<usize>, String);

impl<M: InputModeKind> InputBaseState<M> {
    pub(super) fn edit_lines(
        &mut self,
        command: &EditLines,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_editable() || self.masked || self.ime_marked_range.is_some() {
            return;
        }

        let runs = self.selected_line_runs();
        let revision = self.document_revision;
        match command.0 {
            Operation::Delete => {
                let edits = self.line_deletion_edits(&runs);
                self.replace_text_in_ranges_with_selection(
                    &edits,
                    EditSelection::WholeLines,
                    window,
                    cx,
                );
            }
            Operation::Duplicate(direction) => self.duplicate_lines(&runs, direction, window, cx),
            Operation::Move(direction) => self.move_lines(&runs, direction, window, cx),
            Operation::ToggleComment => self.toggle_line_comments(&runs, window, cx),
        }
        self.reveal_after_interactive_edit(revision);
    }

    pub(super) fn selected_line_runs(&self) -> Vec<Range<usize>> {
        let mut rows: Vec<_> = self
            .selections
            .iter()
            .map(|selection| selected_rows(&self.text, *selection))
            .collect();
        rows.sort_unstable_by_key(|range| range.start);
        let mut runs: Vec<Range<usize>> = Vec::with_capacity(rows.len());
        for range in rows {
            if let Some(last) = runs.last_mut().filter(|last| range.start <= last.end) {
                last.end = last.end.max(range.end);
            } else {
                runs.push(range);
            }
        }
        runs
    }

    pub(super) fn line_deletion_edits(&self, runs: &[Range<usize>]) -> Vec<LineReplacement> {
        runs.iter()
            .filter_map(|run| {
                let start = self.text.line_start_offset(run.start);
                let end = self.text.line_start_offset(run.end);
                // The last run has no following separator. Remove the preceding
                // one so cutting or deleting the last lines removes those rows.
                let start = if run.end == self.text.lines_len() {
                    self.preceding_line_break_start(start)
                } else {
                    start
                };
                (start < end).then_some((start..end, String::new()))
            })
            .collect()
    }

    fn duplicate_lines(
        &mut self,
        runs: &[Range<usize>],
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut edits = Vec::with_capacity(runs.len());
        let mut destinations = Vec::with_capacity(runs.len());
        let mut inserted_rows = 0;
        for run in runs {
            let start = self.text.line_start_offset(run.start);
            let end = self.line_content_end(run.end - 1);
            let next = self.text.line_start_offset(run.end);
            let body = self.text.slice(start..end);
            let separator = if run.end < self.text.lines_len() {
                self.text.slice(end..next).to_string()
            } else if run.start > 0 {
                self.text
                    .slice(self.preceding_line_break_start(start)..start)
                    .to_string()
            } else if run.len() > 1 {
                let first_end = self.line_content_end(run.start);
                let second = self.text.line_start_offset(run.start + 1);
                self.text.slice(first_end..second).to_string()
            } else {
                "\n".to_owned()
            };
            let (insertion, text, row) = match direction {
                Direction::Up => (start, format!("{body}{separator}"), run.start),
                Direction::Down if run.end == self.text.lines_len() => {
                    (next, format!("{separator}{body}"), run.end)
                }
                Direction::Down => (next, format!("{body}{separator}"), run.end),
            };
            edits.push((insertion..insertion, text));
            destinations.push(row + inserted_rows);
            inserted_rows += run.len();
        }
        self.apply_line_transfers(runs, &destinations, &edits, window, cx);
    }

    fn move_lines(
        &mut self,
        runs: &[Range<usize>],
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut edits = Vec::with_capacity(runs.len());
        let mut destinations = Vec::with_capacity(runs.len());
        let last_row = self.text.lines_len() - 1;
        // The empty row after a final newline represents that file terminator.
        let movable_end = if self.text.line_start_offset(last_row) == self.text.len() {
            last_row
        } else {
            last_row + 1
        };

        for run in runs {
            let swap = match direction {
                Direction::Up if run.start > 0 && run.end <= movable_end => {
                    Some((run.start - 1, run.start, run.end, run.start - 1))
                }
                Direction::Down if run.end < movable_end => {
                    Some((run.start, run.end, run.end + 1, run.start + 1))
                }
                _ => None,
            };
            let Some((first, middle, end, destination)) = swap else {
                destinations.push(run.start);
                continue;
            };

            let first = self.text.line_start_offset(first);
            let left_end = self.line_content_end(middle - 1);
            let right_start = self.text.line_start_offset(middle);
            let end = self.line_content_end(end - 1);
            let left = self.text.slice(first..left_end);
            let separator = self.text.slice(left_end..right_start);
            let right = self.text.slice(right_start..end);
            edits.push((first..end, format!("{right}{separator}{left}")));
            destinations.push(destination);
        }

        self.apply_line_transfers(runs, &destinations, &edits, window, cx);
    }

    fn apply_line_transfers(
        &mut self,
        runs: &[Range<usize>],
        destinations: &[usize],
        edits: &[LineReplacement],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source = self.text.clone();
        let selections: Vec<_> = self.selections.iter().copied().collect();
        let map = |text: &Rope| {
            selections
                .iter()
                .map(|&selection| {
                    let first = source.offset_to_point(selection.start).row;
                    let run = runs.partition_point(|run| run.start <= first) - 1;
                    let map_offset = |offset| {
                        let point = source.offset_to_point(offset);
                        let row = destinations[run] + point.row - runs[run].start;
                        text.line_start_offset(row) + point.column
                    };
                    map_selection(selection, map_offset)
                })
                .collect()
        };
        self.replace_text_in_ranges_with_selection(edits, EditSelection::Mapped(&map), window, cx);
    }

    fn toggle_line_comments(
        &mut self,
        runs: &[Range<usize>],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(config) = self.mode.language_config() else {
            return;
        };
        let Some(marker) = &config.line_comment else {
            return;
        };

        let mut lines = Vec::new();
        for row in runs.iter().cloned().flatten() {
            let start = self.text.line_start_offset(row);
            let line = self.text.slice(start..self.line_content_end(row));
            let indent = line
                .chars()
                .take_while(|ch| matches!(ch, ' ' | '\t'))
                .count();
            if indent == line.len() {
                continue;
            }
            let after_indent = start + indent;
            let content = line.slice(indent..);
            let has_comment = content
                .chars()
                .take(marker.chars().count())
                .eq(marker.chars());
            let space = has_comment && content.get_char(marker.len()).ok() == Some(' ');
            lines.push((after_indent, has_comment, space));
        }

        let uncomment = lines.iter().all(|(_, commented, _)| *commented);
        let mut changes = Vec::with_capacity(lines.len());
        let mut edits = Vec::with_capacity(lines.len());
        let mut delta = 0isize;
        for (start, _, space) in lines {
            let (range, replacement) = if uncomment {
                (
                    start..start + marker.len() + usize::from(space),
                    String::new(),
                )
            } else {
                (start..start, format!("{marker} "))
            };
            changes.push(delta);
            delta += replacement.len() as isize - range.len() as isize;
            edits.push((range, replacement));
        }
        changes.push(delta);

        let selections: Vec<_> = self.selections.iter().copied().collect();
        let map = |_: &Rope| {
            let map_offset = |offset| {
                let index = edits.partition_point(|(range, _)| range.end <= offset);
                let removed = edits
                    .get(index)
                    .map_or(0, |(range, _)| offset.saturating_sub(range.start));
                (offset - removed).saturating_add_signed(changes[index])
            };
            selections
                .iter()
                .map(|&selection| map_selection(selection, map_offset))
                .collect()
        };
        self.replace_text_in_ranges_with_selection(&edits, EditSelection::Mapped(&map), window, cx);
    }
}

fn selected_rows(text: &Rope, selection: CursorSelection) -> Range<usize> {
    let first = text.offset_to_point(selection.start).row;
    let end = text.offset_to_point(selection.end);
    let include_end_row = selection.is_empty() || end.column != 0;
    first..end.row + usize::from(include_end_row)
}

fn map_selection(
    mut selection: CursorSelection,
    map_offset: impl Fn(usize) -> usize,
) -> CursorSelection {
    selection.start = map_offset(selection.start);
    selection.end = map_offset(selection.end);
    selection.column_anchor = None;
    selection
}
