use std::{
    cell::RefCell,
    ops::{ControlFlow, Range},
    time::{Duration, Instant},
};

use gpui_component::{
    highlighter::LanguageRegistry,
    input::{Rope, RopeExt},
};
use tree_sitter::{Node, ParseOptions, ParseState, Parser, Tree};

const HORIZONTAL_SPACE: [char; 2] = [' ', '\t'];

pub(super) fn newline_edit_handler()
-> Option<impl Fn(&Rope, usize) -> Option<(Range<usize>, String)>> {
    let grammar = LanguageRegistry::singleton()
        .language("markdown")?
        .language?;
    let mut parser = Parser::new();
    parser.set_language(&grammar).ok()?;
    let parser = RefCell::new(parser);
    Some(move |text: &Rope, offset| newline_edit(&mut parser.borrow_mut(), text, offset))
}

fn parse(parser: &mut Parser, text: &Rope) -> Option<Tree> {
    let start = Instant::now();
    let mut progress = |_: &ParseState| {
        if start.elapsed() > Duration::from_millis(5) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let mut input = |offset, _| {
        if offset >= text.len() {
            return &[][..];
        }
        let (chunk, chunk_start) = text.chunk(offset);
        &chunk.as_bytes()[offset - chunk_start..]
    };
    let tree = parser.parse_with_options(
        &mut input,
        None,
        Some(ParseOptions::new().progress_callback(&mut progress)),
    );
    if tree.is_none() {
        parser.reset();
    }
    tree
}

fn newline_edit(parser: &mut Parser, text: &Rope, offset: usize) -> Option<(Range<usize>, String)> {
    let point = text.offset_to_point(offset);
    let line = text.slice_line(point.row);
    let line_end =
        line.len() - usize::from(line.get_byte(line.len().saturating_sub(1)) == Some(b'\r'));
    if point.column != line_end {
        return None;
    }
    let line = line.slice(..line_end).to_string();
    let start = offset - line_end;
    let first = line
        .trim_start_matches(HORIZONTAL_SPACE)
        .as_bytes()
        .first()?;
    if !matches!(first, b'>' | b'-' | b'+' | b'*' | b'0'..=b'9') {
        return None;
    }

    let end = start + line.trim_end_matches(HORIZONTAL_SPACE).len();
    let tree = parse(parser, text)?;
    let mut node = tree.root_node().descendant_for_byte_range(end - 1, end);
    let mut quotes = 0;
    let mut list_markers = Vec::new();
    while let Some(ancestor) = node {
        match ancestor.kind() {
            "fenced_code_block" | "indented_code_block" | "html_block" => return None,
            "block_quote" => quotes += 1,
            // An empty item at EOF may be reported as an error containing the
            // list marker and optionally a task marker.
            "list_item" | "ERROR" => {
                if let Some(marker) = ancestor.named_child(0)
                    && marker.kind().starts_with("list_marker_")
                    && marker.start_byte() >= start
                    && marker.end_byte() <= offset
                {
                    list_markers.push(marker);
                }
            }
            _ => {}
        }
        node = ancestor.parent();
    }

    let mut list_markers = list_markers.into_iter().rev().peekable();
    let mut cursor = 0;
    let mut prefix = "\n".to_owned();
    let mut last = None;
    while cursor < line.len() {
        let rest = line[cursor..].trim_start_matches(HORIZONTAL_SPACE);
        let marker_start = line.len() - rest.len();
        let whitespace = &line[cursor..marker_start];
        if let Some(marker) = list_markers.peek()
            && marker.start_byte() - start <= marker_start
            && marker.end_byte() - start >= marker_start
        {
            let marker = list_markers.next()?;
            let end = marker.end_byte() - start;
            let (item_end, continued) = list_prefix(&line, marker_start, end, marker);
            prefix.push_str(whitespace);
            let continuation = format!("{prefix}{continued}");
            last = Some((marker_start, item_end, continuation));
            // On a line such as "- > text", the new quote line is indented
            // under the existing list item.
            prefix.extend(
                line[marker_start..item_end]
                    .chars()
                    .map(|c| if c == '\t' { '\t' } else { ' ' }),
            );
            cursor = item_end;
        } else if quotes > 0 && rest.starts_with('>') {
            quotes -= 1;
            let separator = usize::from(rest[1..].starts_with(HORIZONTAL_SPACE));
            let end = marker_start + 1 + separator;
            prefix.push_str(&line[cursor..end]);
            let continuation = if separator == 0 {
                format!("{prefix} ")
            } else {
                prefix.clone()
            };
            last = Some((marker_start, end, continuation));
            cursor = end;
        } else {
            break;
        }
    }

    let (marker_start, prefix_end, continuation) = last?;
    if line[prefix_end..].trim_matches(HORIZONTAL_SPACE).is_empty() {
        Some((start + marker_start..offset, String::new()))
    } else {
        Some((offset..offset, continuation))
    }
}

fn list_prefix(line: &str, start: usize, end: usize, marker: Node<'_>) -> (usize, String) {
    let original = &line[start..end];
    let symbol = original.trim_end_matches(HORIZONTAL_SPACE);
    let spacing = if symbol.len() == original.len() {
        " "
    } else {
        &original[symbol.len()..]
    };
    let mut continued = if matches!(marker.kind(), "list_marker_dot" | "list_marker_parenthesis") {
        let (digits, separator) = symbol.split_at(symbol.len() - 1);
        let number: u32 = digits
            .parse()
            .expect("Markdown ordered list marker contains digits");
        // CommonMark limits an ordered marker to nine digits.
        format!(
            "{:0width$}{separator}{spacing}",
            (number + 1).min(999_999_999),
            width = digits.len()
        )
    } else {
        format!("{symbol}{spacing}")
    };

    let rest = &line[end..];
    // The grammar can parse an empty checkbox as inline text.
    let task = rest.get(..3).is_some_and(|token| {
        matches!(token, "[ ]" | "[x]" | "[X]")
            && (marker.next_named_sibling().is_some_and(|node| {
                node.start_byte() == marker.end_byte()
                    && node.kind().starts_with("task_list_marker_")
            }) || rest[3..].trim_matches(HORIZONTAL_SPACE).is_empty())
    });
    if task {
        let tail = &rest[3..];
        let whitespace = tail.len() - tail.trim_start_matches(HORIZONTAL_SPACE).len();
        continued.push_str("[ ]");
        continued.push_str(&tail[..whitespace]);
        (end + 3 + whitespace, continued)
    } else {
        (end, continued)
    }
}
