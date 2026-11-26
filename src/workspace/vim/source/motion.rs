use gpui_component::input::RopeExt as _;
use unicode_segmentation::UnicodeSegmentation as _;

use super::{
    Context, EditorState, GraphemeCursor, GraphemeIncomplete, Mode, Motion, Pair, Range, Rope,
    SelectionShape, Target, surround,
};

pub(super) fn count_prefix(text: &str) -> (usize, &str) {
    let end = text.bytes().take_while(u8::is_ascii_digit).count();
    (
        text[..end]
            .parse()
            .unwrap_or(if end == 0 { 1 } else { usize::MAX }),
        &text[end..],
    )
}

pub(super) fn line(text: &Rope, at: usize) -> Range<usize> {
    let row = text.offset_to_point(at.min(text.len())).row;
    text.line_start_offset(row)..text.line_start_offset(row + 1)
}

pub(super) fn previous(text: &Rope, at: usize) -> usize {
    boundary(text, at.min(text.len()), false)
}

pub(super) fn next(text: &Rope, at: usize) -> usize {
    boundary(text, at.min(text.len()), true)
}

fn boundary(text: &Rope, at: usize, forward: bool) -> usize {
    if (forward && at == text.len()) || (!forward && at == 0) {
        return at;
    }
    let mut cursor = GraphemeCursor::new(at, text.len(), true);
    let (mut chunk, mut start) = text.chunk(if forward { at } else { at - 1 });
    loop {
        let result = if forward {
            cursor.next_boundary(chunk, start)
        } else {
            cursor.prev_boundary(chunk, start)
        };
        match result {
            Ok(boundary) => return boundary.unwrap_or(at),
            Err(GraphemeIncomplete::PreContext(end)) => {
                let (context, context_start) = text.chunk(end - 1);
                cursor.provide_context(context, context_start);
            }
            Err(GraphemeIncomplete::PrevChunk) => (chunk, start) = text.chunk(start - 1),
            Err(GraphemeIncomplete::NextChunk) => (chunk, start) = text.chunk(start + chunk.len()),
            Err(GraphemeIncomplete::InvalidOffset) => return at,
        }
    }
}

pub(super) fn line_end(text: &Rope, at: usize) -> usize {
    let range = line(text, at);
    if range.end > range.start && range.end < text.len() {
        previous(text, range.end)
    } else if range.end > range.start {
        let (chunk, start) = text.chunk(range.end - 1);
        if chunk.as_bytes()[range.end - 1 - start] == b'\n' {
            previous(text, range.end)
        } else {
            range.end
        }
    } else {
        range.end
    }
}

pub(super) fn first_non_blank(text: &Rope, at: usize) -> usize {
    let start = line(text, at).start;
    (start..line_end(text, at))
        .find(|index| !matches!(text.char_at(*index), Some(' ' | '\t')))
        .unwrap_or(start)
}

pub(super) fn move_cursor(editor: &mut EditorState, at: usize, cx: &mut Context<EditorState>) {
    let text = editor.text();
    let at = at.min(text.len());
    let end = line_end(text, at);
    let at = if at >= end && end > line(text, at).start {
        previous(text, end)
    } else {
        at
    };
    editor.set_modal_selection(&[(at, at)], at, cx);
}

pub(super) fn motion(
    text: &Rope,
    at: usize,
    target: Target,
    count: usize,
    last_row: usize,
) -> Option<(usize, bool)> {
    let row = text.offset_to_point(at).row;
    let start = line(text, at).start;
    let end = line_end(text, at);
    let row_start = |row: usize| text.line_start_offset(row.min(last_row));
    let steps = count.min(text.len().saturating_add(1));
    let to = match target {
        Target::FirstLine | Target::Key('G') => {
            first_non_blank(text, row_start(count.saturating_sub(1)))
        }
        Target::Key('j' | 'k') => {
            let to = if matches!(target, Target::Key('j')) {
                row.saturating_add(count)
            } else {
                row.saturating_sub(count)
            };
            let to = row_start(to);
            let column = text.slice(start..at).to_string().graphemes(true).count();
            let end = line_end(text, to);
            let content = text.slice(to..end).to_string();
            to + content
                .grapheme_indices(true)
                .nth(column)
                .map_or(content.len(), |(at, _)| at)
        }
        Target::Key('h') => {
            (0..steps.min(at - start)).fold(at, |at, _| previous(text, at).max(start))
        }
        Target::Key('l') => {
            (0..steps.min(end.saturating_sub(at))).fold(at, |at, _| next(text, at).min(end))
        }
        Target::Key('0') => start,
        Target::Key('^') => first_non_blank(text, at),
        Target::PercentLine if count <= 100 => {
            let total = last_row + 1;
            first_non_blank(
                text,
                row_start((total * count).div_ceil(100).saturating_sub(1)),
            )
        }
        Target::Key('%') => return surround::matching(text, at).map(|to| (to, true)),
        Target::Key('+' | '-' | '_') => {
            let dest = match target {
                Target::Key('-') => row.saturating_sub(count),
                Target::Key('_') => row.saturating_add(count.saturating_sub(1)),
                _ => row.saturating_add(count),
            };
            first_non_blank(text, row_start(dest))
        }
        Target::ChangeWord => return Some((word_motion(text, at, 'w', steps), false)),
        Target::Key('$') => line_end(text, row_start(row.saturating_add(count.saturating_sub(1)))),
        Target::Key(key @ ('w' | 'b' | 'e')) => {
            return Some((word_motion(text, at, key, steps), key == 'e'));
        }
        Target::Find(kind, ch, repeat) => {
            let content = text.slice(start..end).to_string();
            let forward = matches!(kind, 'f' | 't');
            let local = at - start;
            let skip = if repeat && kind == 't' {
                next(text, at) - start
            } else if repeat && kind == 'T' {
                previous(text, at).saturating_sub(start)
            } else {
                local
            };
            let found = if forward {
                content
                    .char_indices()
                    .filter(|(index, value)| *index > skip && *value == ch)
                    .nth(count - 1)
            } else {
                content
                    .char_indices()
                    .rev()
                    .filter(|(index, value)| *index < skip && *value == ch)
                    .nth(count - 1)
            }?;
            let to = start + found.0;
            return Some((
                match kind {
                    't' => previous(text, to),
                    'T' => next(text, to),
                    _ => to,
                },
                forward,
            ));
        }
        _ => return None,
    };
    Some((to, false))
}

fn word_class(ch: Option<char>) -> u8 {
    match ch {
        None => 0,
        Some(ch) if ch.is_whitespace() => 0,
        Some(ch) if ch.is_alphanumeric() || ch == '_' => 1,
        Some(_) => 2,
    }
}

fn word_motion(text: &Rope, mut at: usize, key: char, count: usize) -> usize {
    let class = |at| word_class(text.char_at(at));
    for _ in 0..count {
        let from = at;
        if key == 'b' {
            at = previous(text, at);
            while at > 0 && class(at) == 0 {
                at = previous(text, at);
            }
            while at > 0 && class(previous(text, at)) == class(at) {
                at = previous(text, at);
            }
        } else if key == 'w' {
            let initial = class(at);
            while initial != 0 && at < text.len() && class(at) == initial {
                at = next(text, at);
            }
            while at < text.len() && class(at) == 0 {
                at = next(text, at);
            }
        } else {
            at = next(text, at);
            while at < text.len() && class(at) == 0 {
                at = next(text, at);
            }
            while at < text.len() && class(next(text, at)) == class(at) {
                at = next(text, at);
            }
        }
        if at == from {
            break;
        }
    }
    at
}

pub(super) fn parse_target(keys: &str, last_find: Option<(char, char)>) -> Option<Target> {
    let mut keys = keys.chars();
    let key = keys.next()?;
    let second = keys.next();
    Some(match (key, second) {
        ('g', Some('g')) => Target::FirstLine,
        ('f' | 'F' | 't' | 'T', Some(ch)) => Target::Find(key, ch, false),
        ('i' | 'a', Some(ch)) => Target::Object(ch, key == 'a'),
        (';' | ',', None) => {
            let (mut kind, ch) = last_find?;
            if key == ',' {
                kind = match kind {
                    'f' => 'F',
                    'F' => 'f',
                    't' => 'T',
                    _ => 't',
                };
            }
            Target::Find(kind, ch, true)
        }
        (
            'h' | 'j' | 'k' | 'l' | 'w' | 'b' | 'e' | '0' | '^' | '$' | 'G' | '%' | '+' | '-' | '_',
            None,
        ) => Target::Key(key),
        _ => return None,
    })
}

impl Motion {
    pub(super) fn range(self, text: &Rope, at: usize) -> Option<(Range<usize>, bool)> {
        let Self {
            target,
            count,
            explicit,
        } = self;
        let count = count.max(1).min(text.len().saturating_add(1));
        let row = text.offset_to_point(at).row;
        match target {
            Target::Lines => return selection_range(text, at, SelectionShape::Lines(count)),
            Target::Selection(shape) => return selection_range(text, at, shape),
            Target::Object(key, around) => {
                return text_object(text, at, key, around, count).map(|range| (range, false));
            }
            Target::ChangeWord if word_class(text.char_at(at)) != 0 => {
                let end =
                    if word_class(text.char_at(next(text, at))) == word_class(text.char_at(at)) {
                        word_motion(text, at, 'e', count)
                    } else if count > 1 {
                        word_motion(text, at, 'e', count - 1)
                    } else {
                        at
                    };
                return Some((at..next(text, end), false));
            }
            _ => {}
        }
        let count = if matches!(target, Target::Key('G')) && !explicit {
            usize::MAX
        } else {
            count
        };
        let (mut to, inclusive) = motion(text, at, target, count, last_row(text).max(row))?;
        if matches!(
            target,
            Target::FirstLine
                | Target::PercentLine
                | Target::Key('G' | 'j' | 'k' | '+' | '-' | '_')
        ) {
            if at == to && matches!(target, Target::Key('j' | 'k')) {
                return None;
            }
            return Some((
                line(text, at.min(to)).start..line(text, at.max(to)).end,
                true,
            ));
        }
        if matches!(target, Target::Key('w') | Target::ChangeWord)
            && to > at
            && text.offset_to_point(to).row > row
        {
            let step = word_motion(text, at, 'w', count.saturating_sub(1));
            if text.offset_to_point(step).row < text.offset_to_point(to).row {
                to = line_end(text, step);
            }
        }
        Some((
            at.min(to)..if inclusive {
                next(text, at.max(to))
            } else {
                at.max(to)
            },
            false,
        ))
    }
}

pub(super) fn selection_motion(editor: &EditorState, mode: Mode) -> Motion {
    let text = editor.text();
    let range = editor.selected_range();
    let start = text.offset_to_point(range.start);
    let last = text.offset_to_point(previous(text, range.end).max(range.start));
    let rows = last.row - start.row;
    let shape = if mode == Mode::VisualLine {
        SelectionShape::Lines(rows + 1)
    } else if rows > 0 {
        let line_start = text.line_start_offset(last.row);
        let last_column = text
            .slice(line_start..range.end)
            .to_string()
            .graphemes(true)
            .count();
        SelectionShape::Multiline { rows, last_column }
    } else {
        SelectionShape::Characters(text.slice(range).to_string().graphemes(true).count())
    };
    Motion {
        target: Target::Selection(shape),
        count: 1,
        explicit: true,
    }
}

pub(super) fn selection_range(
    text: &Rope,
    at: usize,
    shape: SelectionShape,
) -> Option<(Range<usize>, bool)> {
    let row = text.offset_to_point(at).row;
    Some(match shape {
        SelectionShape::Lines(lines) => (
            text.line_start_offset(row)..row_boundary(text, row.saturating_add(lines)),
            true,
        ),
        SelectionShape::Characters(count) => {
            (at..advance(text, at, count).min(line(text, at).end), false)
        }
        SelectionShape::Multiline { rows, last_column } => {
            let start = row_boundary(text, row.saturating_add(rows));
            (
                at..advance(text, start, last_column).min(line(text, start).end),
                false,
            )
        }
    })
}

/// Last row counted by linewise edits. The empty row after a final newline has no bytes.
pub(super) fn last_row(text: &Rope) -> usize {
    let eof = text.offset_to_point(text.len());
    eof.row.saturating_sub(usize::from(eof.column == 0))
}

pub(super) fn row_boundary(text: &Rope, row: usize) -> usize {
    if row >= text.lines_len() {
        text.len()
    } else {
        text.line_start_offset(row)
    }
}

fn advance(text: &Rope, mut at: usize, count: usize) -> usize {
    for _ in 0..count {
        if at >= text.len() {
            break;
        }
        at = next(text, at);
    }
    at
}

pub(super) fn text_object(
    text: &Rope,
    at: usize,
    key: char,
    around: bool,
    count: usize,
) -> Option<Range<usize>> {
    if key != 'w' {
        let range = Pair::for_key(key)?.enclosing(text, at, count)?;
        return Some(if around {
            range
        } else {
            range.start + 1..range.end - 1
        });
    }
    let class = word_class(text.char_at(at));
    let bounds = line(text, at);
    let mut start = at;
    while start > bounds.start && word_class(text.char_at(previous(text, start))) == class {
        start = previous(text, start);
    }
    let mut end = at;
    for index in 0..count {
        if around && index != 0 {
            while end < text.len() && word_class(text.char_at(end)) == 0 {
                end = next(text, end);
            }
        }
        let class = word_class(text.char_at(end));
        while end < text.len() && word_class(text.char_at(end)) == class {
            if class == 0 && text.char_at(end) == Some('\n') {
                if at != end {
                    end = next(text, end);
                }
                break;
            }
            end = next(text, end);
        }
        if end == text.len() {
            break;
        }
    }
    if around {
        let before = end;
        while matches!(text.char_at(end), Some(' ' | '\t')) {
            end = next(text, end);
        }
        if before == end {
            while start > 0 && matches!(text.char_at(previous(text, start)), Some(' ' | '\t')) {
                start = previous(text, start);
            }
        }
    }
    Some(start..end)
}
