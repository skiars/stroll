use std::ops::Range;

use gpui_component::input::{Rope, RopeExt as _};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use super::{last_row, line_end, next, previous};

#[derive(Clone, Copy)]
pub(super) struct Block {
    pub first: usize,
    pub last: usize,
    pub left: usize,
    pub right: Option<usize>,
}

#[derive(Clone, Copy)]
pub(super) struct Shape {
    rows: usize,
    width: Option<usize>,
}

impl Block {
    pub(super) fn shape(self) -> Shape {
        Shape {
            rows: self.last - self.first + 1,
            width: self.right.map(|right| right - self.left),
        }
    }

    pub(super) fn slices(self, text: &Rope, tab: usize) -> impl Iterator<Item = Slice> {
        (self.first..=self.last)
            .map(move |row| Row::new(text, row, tab).slice(self.left, self.right))
    }

    pub(super) fn insertion(self, key: char) -> Insertion {
        Insertion {
            first: self.first,
            last: self.last,
            column: if key == 'A' {
                self.right
            } else {
                Some(self.left)
            },
            pad: key == 'A',
        }
    }
}

impl Shape {
    pub(super) fn at(self, row: usize, column: usize, text: &Rope) -> Block {
        Block {
            first: row,
            last: (row + self.rows - 1).min(text.lines_len() - 1),
            left: column,
            right: self.width.map(|width| column + width),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Insertion {
    pub first: usize,
    pub last: usize,
    pub column: Option<usize>,
    pub pad: bool,
}

impl Insertion {
    pub(super) fn edits(
        self,
        text: &Rope,
        start_row: usize,
        tab: usize,
        value: &str,
    ) -> Vec<(Range<usize>, String)> {
        (start_row..=self.last.min(text.lines_len() - 1))
            .filter_map(|row| Row::new(text, row, tab).insert(self.column, self.pad, value))
            .collect()
    }
}

pub(super) struct Slice {
    pub range: Range<usize>,
    before: usize,
    after: usize,
    pub selected: String,
    pub visual_offsets: (f32, f32),
}

impl Slice {
    pub(super) fn edit(&self, value: &str) -> (Range<usize>, String) {
        let insertion = format!(
            "{}{value}{}",
            " ".repeat(self.before),
            " ".repeat(self.after)
        );
        (self.range.clone(), insertion)
    }
}

struct Cell {
    bytes: Range<usize>,
    columns: Range<usize>,
    tab: bool,
}

pub(super) struct Row {
    text: String,
    start: usize,
    cells: Vec<Cell>,
    pub width: usize,
}

impl Row {
    pub(super) fn new(text: &Rope, row: usize, tab: usize) -> Self {
        let start = text.line_start_offset(row);
        let text = text.slice(start..line_end(text, start)).to_string();
        let mut width = 0;
        let cells = text
            .grapheme_indices(true)
            .map(|(at, glyph)| {
                let is_tab = glyph == "\t";
                let before = width;
                width += if is_tab {
                    tab - width % tab
                } else {
                    glyph.width().max(1)
                };
                Cell {
                    bytes: at..at + glyph.len(),
                    columns: before..width,
                    tab: is_tab,
                }
            })
            .collect();
        Self {
            text,
            start,
            cells,
            width,
        }
    }

    pub(super) fn column(&self, at: usize) -> usize {
        let at = at.saturating_sub(self.start);
        self.cells
            .iter()
            .find(|cell| cell.bytes.contains(&at))
            .map_or(self.width, |cell| cell.columns.start)
    }

    pub(super) fn offset(&self, column: usize) -> usize {
        self.start
            + self
                .cells
                .iter()
                .find(|cell| cell.columns.contains(&column))
                .map_or(self.text.len(), |cell| cell.bytes.start)
    }

    pub(super) fn end(&self) -> usize {
        self.start + self.text.len()
    }

    pub(super) fn last_column(&self) -> usize {
        self.cells.last().map_or(0, |cell| cell.columns.start)
    }

    pub(super) fn glyph_end(&self, at: usize) -> usize {
        self.cells
            .iter()
            .find(|cell| cell.bytes.start + self.start == at)
            .map_or(self.width + 1, |cell| cell.columns.end)
    }

    fn slice(&self, left: usize, right: Option<usize>) -> Slice {
        let right = right.unwrap_or(self.width);
        let mut cells = self
            .cells
            .iter()
            .filter(|cell| cell.columns.end > left && cell.columns.start < right);
        let mut result = Slice {
            range: self.end()..self.end(),
            before: 0,
            after: 0,
            selected: String::new(),
            visual_offsets: (
                (left as f32 - self.width as f32).max(0.),
                (right as f32 - self.width as f32).max(0.),
            ),
        };
        if left >= right {
            return result;
        }

        if let Some(first) = cells.next() {
            result.range.start = self.start + first.bytes.start;
            result.before = left.saturating_sub(first.columns.start);
            for cell in std::iter::once(first).chain(cells) {
                result.range.end = self.start + cell.bytes.end;
                result.after = cell.columns.end.saturating_sub(right);
                let width = cell.columns.end.min(right) - cell.columns.start.max(left);
                if cell.tab || width < cell.columns.len() {
                    result.selected.push_str(&" ".repeat(width));
                } else {
                    result.selected.push_str(&self.text[cell.bytes.clone()]);
                }
            }
        }
        if !result.range.is_empty() {
            result.visual_offsets = (
                result.before as f32,
                right.saturating_sub(self.width) as f32 - result.after as f32,
            );
        }
        result
    }

    pub(super) fn insertion_offset(&self, column: usize) -> usize {
        let offset = self.offset(column);
        if self.text.as_bytes().get(offset - self.start) == Some(&b'\t') || offset == self.end() {
            column - self.column(offset)
        } else {
            0
        }
    }

    pub(super) fn insert(
        &self,
        column: Option<usize>,
        pad: bool,
        value: &str,
    ) -> Option<(Range<usize>, String)> {
        let column = column.unwrap_or(self.width);
        if column > self.width && !pad {
            return None;
        }
        let offset = self.offset(column);
        let cell = self
            .cells
            .iter()
            .find(|cell| cell.columns.contains(&column));
        if let Some(cell) = cell.filter(|cell| cell.tab && cell.columns.start < column) {
            let before = column - cell.columns.start;
            let after = cell.columns.end - column;
            return Some((
                offset..self.start + cell.bytes.end,
                format!("{}{value}{}", " ".repeat(before), " ".repeat(after)),
            ));
        }
        Some((
            offset..offset,
            format!("{}{value}", " ".repeat(column.saturating_sub(self.width))),
        ))
    }
}

pub(super) fn put(
    text: &Rope,
    at: usize,
    rows: &[String],
    width: Option<usize>,
    count: usize,
    after: bool,
    tab: usize,
) -> Vec<(Range<usize>, String)> {
    let first = text.offset_to_point(at).row;
    let row = Row::new(text, first, tab);
    let column = row.column(if after {
        next(text, at).min(row.end())
    } else {
        at
    });
    put_at_column(text, first, column, rows, width, count, tab)
}

pub(super) fn put_at_column(
    text: &Rope,
    first: usize,
    column: usize,
    rows: &[String],
    width: Option<usize>,
    count: usize,
    tab: usize,
) -> Vec<(Range<usize>, String)> {
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let last = last_row(text);
    let end = previous(text, text.len());
    let insertion_end = if text.char_at(end) == Some('\n') {
        end
    } else {
        text.len()
    };
    for (index, value) in rows.iter().enumerate() {
        let row = first + index;
        let value = repeated(value, width, count);
        if row <= last {
            edits.extend(Row::new(text, row, tab).insert(Some(column), true, &value));
        } else {
            let tail = format!("\n{}{value}", " ".repeat(column));
            if let Some((_, value)) = edits
                .last_mut()
                .filter(|(range, _)| range.end == insertion_end)
            {
                value.push_str(&tail);
            } else {
                edits.push((insertion_end..insertion_end, tail));
            }
        }
    }
    edits
}

pub(super) fn repeated(value: &str, width: Option<usize>, count: usize) -> String {
    let padding = width.unwrap_or(0).saturating_sub(value.width());
    let cell = format!("{value}{}", " ".repeat(padding));
    format!("{}{value}", cell.repeat(count.saturating_sub(1)))
}
