use std::{
    ops::{Range, RangeInclusive},
    rc::Rc,
    sync::LazyLock,
};

use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line, Point},
    term::cell::Hyperlink,
};
use regex::Regex;
use url::Url;

use super::{Emulator, Flags};

static WEB_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i-u:https?)://[^\s<>"'«»“”‘’〈〉《》「」『』【】（）［］｛｝，。！？；：]+"#)
        .expect("valid terminal URL pattern")
});

static PATH_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"[^\s\p{Cc}<>"'«»“”‘’〈〉《》「」『』【】（）［］｛｝，。！？；：()\[\]{},;=|]+"#)
        .expect("valid terminal path token pattern")
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Link {
    pub target: LinkTarget,
    cells: RangeInclusive<Point>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LinkTarget {
    Web(Rc<str>),
    File {
        text: Rc<str>,
        path_end: usize,
        position: Option<lsp_types::Position>,
    },
}

impl LinkTarget {
    pub(crate) fn text(&self) -> &str {
        match self {
            Self::Web(text) | Self::File { text, .. } => text,
        }
    }
}

impl Link {
    pub(crate) fn rows(&self, visible_rows: usize) -> Range<usize> {
        let first = self.cells.start().line.0.max(0) as usize;
        let end = (self.cells.end().line.0 as usize + 1).min(visible_rows);
        first..end
    }

    pub(crate) fn columns(&self, row: usize, cols: usize) -> Option<Range<usize>> {
        let row = Line(row as i32);
        let start = self.cells.start();
        let end = self.cells.end();
        (start.line..=end.line).contains(&row).then(|| {
            let first = if row == start.line { start.column.0 } else { 0 };
            let last = if row == end.line {
                end.column.0 + 1
            } else {
                cols
            };
            first..last
        })
    }
}

impl Emulator {
    pub(super) fn links(&self, links: &mut Vec<Link>) {
        links.clear();
        let grid = self.term.grid();
        let top = Line(-(grid.display_offset() as i32));
        let bottom = top + grid.screen_lines() - 1;
        let last_col = grid.last_column();
        let first = self.term.line_search_left(Point::new(top, last_col)).line;
        let last = self
            .term
            .line_search_right(Point::new(bottom, last_col))
            .line;
        let mut text = String::new();
        let mut cells = Vec::new();
        let mut explicit: Option<(Hyperlink, Point, Point)> = None;
        let mut add_link = |target: LinkTarget, mut start: Point, mut end: Point| {
            if start.line <= bottom && end.line >= top {
                start.line -= top.0;
                end.line -= top.0;
                links.push(Link {
                    target,
                    cells: start..=end,
                });
            }
        };

        for line in (first.0..=last.0).map(Line) {
            for column in 0..grid.columns() {
                let start = Point::new(line, Column(column));
                let cell = &grid[start];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let end = if cell.flags.contains(Flags::WIDE_CHAR) && start.column < last_col {
                    Point::new(line, start.column + 1)
                } else {
                    start
                };
                let hidden = cell.flags.contains(Flags::HIDDEN);
                let hyperlink = if hidden { None } else { cell.hyperlink() };
                if let Some((current, _, last)) = explicit.as_mut()
                    && hyperlink.as_ref() == Some(current)
                {
                    *last = end;
                } else {
                    if let Some((previous, start, end)) = explicit.take()
                        && let Some(target) = web_target(previous.uri())
                    {
                        add_link(target, start, end);
                    }
                    explicit = hyperlink.clone().map(|link| (link, start, end));
                }

                cells.push((text.len(), start, end));
                if hidden || hyperlink.is_some() {
                    text.push(' ');
                } else {
                    text.push(cell.c);
                    text.extend(cell.zerowidth().unwrap_or_default());
                }
            }
            if line == last
                || !grid[Point::new(line, last_col)]
                    .flags
                    .contains(Flags::WRAPLINE)
            {
                if let Some((link, start, end)) = explicit.take()
                    && let Some(target) = web_target(link.uri())
                {
                    add_link(target, start, end);
                }
                let mut add_text_link = |target: LinkTarget, start: usize| {
                    let first = cells.partition_point(|(offset, _, _)| *offset < start);
                    let end = cells
                        .partition_point(|(offset, _, _)| *offset < start + target.text().len());
                    add_link(target, cells[first].1, cells[end - 1].2);
                };
                let mut after_url = 0;
                for candidate in WEB_URL.find_iter(&text) {
                    let start = candidate.start();
                    file_links(&text[after_url..start], after_url, &mut add_text_link);
                    after_url = candidate.end();
                    if text
                        .as_bytes()
                        .get(start.wrapping_sub(1))
                        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                    {
                        continue;
                    }
                    if let Some(target) = web_target(trim_punctuation(candidate.as_str())) {
                        add_text_link(target, start);
                    }
                }
                file_links(&text[after_url..], after_url, &mut add_text_link);
                text.clear();
                cells.clear();
            }
        }
    }
}

fn web_target(text: &str) -> Option<LinkTarget> {
    let (_, rest) = text.split_once("://")?;
    if rest.is_empty()
        || rest.starts_with(['/', '?', '#'])
        || text.contains('\\')
        || text.chars().any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return None;
    }
    let url = Url::parse(text).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.has_host())
        .then(|| LinkTarget::Web(text.into()))
}

fn file_links(text: &str, offset: usize, add_link: &mut impl FnMut(LinkTarget, usize)) {
    if !text.contains(['/', '\\', '.', ':']) {
        return;
    }
    for candidate in PATH_TOKEN.find_iter(text) {
        if let Some(target) = file_target(trim_punctuation(candidate.as_str())) {
            add_link(target, offset + candidate.start());
        }
    }
}

fn file_target(text: &str) -> Option<LinkTarget> {
    // A drive letter belongs to the path, and a numeric suffix belongs to the position.
    let has_drive = matches!(
        text.as_bytes(),
        [b'a'..=b'z' | b'A'..=b'Z', b':', b'/' | b'\\', ..]
    );
    let path_start = if has_drive { 2 } else { 0 };
    let (path, position) = match text[path_start..].split_once(':') {
        Some((path, suffix)) => {
            let (line, column) = suffix.split_once(':').unwrap_or((suffix, "1"));
            let index = |value: &str| {
                if !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return None;
                }
                value.parse::<u32>().ok()?.checked_sub(1)
            };
            (
                &text[..path_start + path.len()],
                Some(lsp_types::Position::new(index(line)?, index(column)?)),
            )
        }
        None => (text, None),
    };
    let name = path.rsplit(['/', '\\']).next()?;
    if matches!(name, "" | "." | "..")
        || !path.chars().any(|ch| ch.is_alphabetic() || ch == '_')
        || !(position.is_some() || path.contains(['/', '\\']) || name.contains('.'))
    {
        return None;
    }
    Some(LinkTarget::File {
        text: text.into(),
        path_end: path.len(),
        position,
    })
}

fn trim_punctuation(mut text: &str) -> &str {
    let mut unbalanced = [0i32; 3];
    for ch in text.bytes() {
        match ch {
            b'(' => unbalanced[0] -= 1,
            b')' => unbalanced[0] += 1,
            b'[' => unbalanced[1] -= 1,
            b']' => unbalanced[1] += 1,
            b'{' => unbalanced[2] -= 1,
            b'}' => unbalanced[2] += 1,
            _ => {}
        }
    }
    while let Some(&last) = text.as_bytes().last() {
        match last {
            b'.' | b',' | b';' | b':' | b'!' | b'?' => {}
            b')' if unbalanced[0] > 0 => unbalanced[0] -= 1,
            b']' if unbalanced[1] > 0 => unbalanced[1] -= 1,
            b'}' if unbalanced[2] > 0 => unbalanced[2] -= 1,
            _ => break,
        }
        text = &text[..text.len() - 1];
    }
    text
}
