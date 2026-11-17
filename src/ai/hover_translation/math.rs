use std::ops::Range;

use markdown::mdast::Node;

/// The Web KaTeX plugin checks the adjacent characters of inline dollar
/// delimiters. The desktop Markdown parser accepts more pairs by default.
/// Keep byte offsets stable so the same paragraphs can be found in the hover.
pub(super) const LITERAL_DOLLAR: char = '\u{1f}';

pub(super) fn mask_literal_dollars(source: &str, root: &Node) -> String {
    let mut output = source.as_bytes().to_vec();
    visit_paragraphs(source.as_bytes(), &mut output, root);
    String::from_utf8(output).expect("Only ASCII dollar bytes are replaced")
}

fn visit_paragraphs(source: &[u8], output: &mut [u8], node: &Node) {
    if let Node::Paragraph(paragraph) = node {
        let Some(pos) = &paragraph.position else {
            return;
        };
        let mut protected = Vec::new();
        for child in &paragraph.children {
            collect_protected(child, &mut protected);
        }
        protected.sort_unstable_by_key(|range| range.start);
        let range = pos.start.offset..pos.end.offset;
        mask_paragraph(
            &source[range.clone()],
            &mut output[range],
            pos.start.offset,
            &protected,
        );
    } else if matches!(
        node,
        Node::Root(_) | Node::Blockquote(_) | Node::List(_) | Node::ListItem(_)
    ) && let Some(children) = node.children()
    {
        for child in children {
            visit_paragraphs(source, output, child);
        }
    }
}

fn collect_protected(node: &Node, ranges: &mut Vec<Range<usize>>) {
    let Some(pos) = node.position() else { return };
    let range = pos.start.offset..pos.end.offset;
    match node {
        Node::InlineCode(_) | Node::Image(_) | Node::ImageReference(_) => ranges.push(range),
        Node::Link(link) if let Some(last_label_node) = link.children.last() => {
            if let Some(pos) = last_label_node.position() {
                ranges.push(pos.end.offset..range.end);
            }
            for child in &link.children {
                collect_protected(child, ranges);
            }
        }
        _ => {
            if let Some(children) = node.children() {
                for child in children {
                    collect_protected(child, ranges);
                }
            }
        }
    }
}

struct Delimiter {
    offset: usize,
    width: usize,
    escaped: bool,
    next_any: Option<usize>,
    next_double: Option<usize>,
}

fn mask_paragraph(source: &[u8], output: &mut [u8], base: usize, protected: &[Range<usize>]) {
    let mut markers = Vec::new();
    let mut offset = 0;
    while let Some(next) = source[offset..].iter().position(|&byte| byte == b'$') {
        offset += next;
        let width = source[offset..]
            .iter()
            .take_while(|&&byte| byte == b'$')
            .count();
        let escapes = source[..offset]
            .iter()
            .rev()
            .take_while(|&&byte| byte == b'\\')
            .count();
        markers.push(Delimiter {
            offset,
            width,
            escaped: escapes % 2 == 1,
            next_any: None,
            next_double: None,
        });
        offset += width;
    }
    let mut next_any = None;
    let mut next_double = None;
    for (index, delimiter) in markers.iter_mut().enumerate().rev() {
        delimiter.next_any = next_any;
        delimiter.next_double = next_double;
        if !delimiter.escaped {
            next_any = Some(index);
            if delimiter.width >= 2 {
                next_double = Some(index);
            }
        }
    }
    let is_word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let mut index = 0;
    let mut protected = protected.iter().peekable();
    while let Some(open) = markers.get(index) {
        let position = base + open.offset;
        while protected.peek().is_some_and(|range| range.end <= position) {
            protected.next();
        }
        if open.escaped
            || protected
                .peek()
                .is_some_and(|range| range.contains(&position))
        {
            index += 1;
            continue;
        }
        let previous = open.offset.checked_sub(1).map(|pos| source[pos]);
        let end = match open.width {
            1 if !previous.is_some_and(|byte| is_word(byte) || byte == b'\\') => {
                open.next_any.filter(|&index| {
                    let close = &markers[index];
                    close.width == 1 && !source.get(close.offset + 1).copied().is_some_and(is_word)
                })
            }
            2 if previous != Some(b'\\') => {
                open.next_double.filter(|&index| markers[index].width == 2)
            }
            _ => None,
        };
        if let Some(end) = end {
            for marker in &markers[index + 1..end] {
                output[marker.offset..marker.offset + marker.width].fill(LITERAL_DOLLAR as u8);
            }
            index = end + 1;
        } else {
            output[open.offset..open.offset + open.width].fill(LITERAL_DOLLAR as u8);
            index += 1;
        }
    }
}
