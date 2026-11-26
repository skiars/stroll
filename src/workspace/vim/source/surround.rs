use std::ops::Range;

use gpui_component::input::Rope;

use super::line;

#[derive(Clone, Copy)]
pub(super) struct Pair {
    open: char,
    close: char,
    pad: bool,
}

impl Pair {
    pub(super) fn for_key(key: char) -> Option<Self> {
        let (open, close) = match key {
            '(' | ')' | 'b' => ('(', ')'),
            '[' | ']' => ('[', ']'),
            '{' | '}' | 'B' => ('{', '}'),
            '<' | '>' | 'a' => ('<', '>'),
            '\'' | '"' | '`' => (key, key),
            _ => return None,
        };
        Some(Self {
            open,
            close,
            pad: key == open && open != close,
        })
    }

    pub(super) fn wrap(self, value: &str) -> String {
        let pad = if self.pad { " " } else { "" };
        format!("{}{pad}{value}{pad}{}", self.open, self.close)
    }

    pub(super) fn wrap_lines(self, value: &str) -> String {
        let indent = &value[..value.len() - value.trim_start_matches([' ', '\t']).len()];
        let body = value.strip_suffix('\n').unwrap_or(value);
        let ending = if value.ends_with('\n') { "\n" } else { "" };
        format!(
            "{indent}{}\n{body}\n{indent}{}{ending}",
            self.open, self.close
        )
    }

    pub(super) fn enclosing(self, text: &Rope, at: usize, count: usize) -> Option<Range<usize>> {
        let mut remaining = count;
        let scope = if self.open == self.close {
            line(text, at)
        } else {
            0..text.len()
        };
        let mut opens = Vec::new();
        let mut escaped = false;
        let mut offset = scope.start;
        for ch in text.slice(scope).chars() {
            let index = offset;
            offset += ch.len_utf8();
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == self.close && !opens.is_empty() {
                let start = opens.pop().unwrap();
                if (start..=index).contains(&at) {
                    remaining -= 1;
                    if remaining == 0 {
                        return Some(start..offset);
                    }
                }
            } else if ch == self.open {
                opens.push(index);
            }
        }
        None
    }

    pub(super) fn body(self, text: &Rope, range: Range<usize>) -> String {
        let body = text.slice(range.start + 1..range.end - 1).to_string();
        if self.open == self.close {
            body
        } else {
            body.trim_matches([' ', '\t']).to_owned()
        }
    }
}

pub(super) fn matching(text: &Rope, at: usize) -> Option<usize> {
    let mut from = at;
    let (open, close, forward) = text.slice(at..line(text, at).end).chars().find_map(|ch| {
        let pair = match ch {
            '(' => ('(', ')', true),
            '[' => ('[', ']', true),
            '{' => ('{', '}', true),
            ')' => (')', '(', false),
            ']' => (']', '[', false),
            '}' => ('}', '{', false),
            _ => {
                from += ch.len_utf8();
                return None;
            }
        };
        Some(pair)
    })?;

    let mut depth = 0usize;
    let mut visit = |ch| {
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
        }
        depth == 0
    };
    if forward {
        let mut offset = from;
        for ch in text.slice(from..).chars() {
            if visit(ch) {
                return Some(offset);
            }
            offset += ch.len_utf8();
        }
    } else {
        let mut offset = from + 1;
        let (chunk, start) = text.chunk(offset - 1);
        let mut chunk = &chunk[..offset - start];
        loop {
            for ch in chunk.chars().rev() {
                offset -= ch.len_utf8();
                if visit(ch) {
                    return Some(offset);
                }
            }
            if offset == 0 {
                break;
            }
            (chunk, _) = text.chunk(offset - 1);
        }
    }
    None
}
