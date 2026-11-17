use std::ops::Range;

use gpui::SharedString;

/// A read-only label inserted in the display at a UTF-8 source offset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InlayHint {
    pub offset: usize,
    pub label: SharedString,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct InlayLine {
    pub(crate) text: String,
    pub(crate) spans: Vec<InlaySpan>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct InlaySpan {
    source: usize,
    pub(crate) display: Range<usize>,
}

impl InlayLine {
    pub(crate) fn new(source: &str, start: usize, hints: &[InlayHint]) -> Self {
        let mut text = String::new();
        let mut spans = Vec::with_capacity(hints.len());
        let mut tail = 0;
        for hint in hints {
            let offset = hint.offset - start;
            text.push_str(&source[tail..offset]);
            let begin = text.len();
            text.push_str(&hint.label);
            spans.push(InlaySpan {
                source: offset,
                display: begin..text.len(),
            });
            tail = offset;
        }
        text.push_str(&source[tail..]);
        Self { text, spans }
    }

    pub(crate) fn display_offset(&self, source: usize, after: bool) -> usize {
        let index = self
            .spans
            .partition_point(|span| span.source < source || (after && span.source == source));
        index.checked_sub(1).map_or(source, |index| {
            let span = &self.spans[index];
            source.saturating_add(span.display.end - span.source)
        })
    }

    pub(crate) fn source_offset(&self, display: usize) -> usize {
        self.span_at(display).map_or(display, |span| {
            span.source + display.saturating_sub(span.display.end)
        })
    }

    pub(crate) fn contains(&self, display: usize) -> bool {
        self.span_at(display)
            .is_some_and(|span| span.display.contains(&display))
    }

    fn span_at(&self, display: usize) -> Option<&InlaySpan> {
        let index = self
            .spans
            .partition_point(|span| span.display.start <= display);
        Some(&self.spans[index.checked_sub(1)?])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_share_a_source_anchor_and_keep_response_order() {
        let hints = [
            InlayHint {
                offset: 4,
                label: "first: ".into(),
            },
            InlayHint {
                offset: 4,
                label: "次 ".into(),
            },
            InlayHint {
                offset: 5,
                label: "!".into(),
            },
        ];
        let line = InlayLine::new("😺x", 0, &hints);
        assert_eq!(line.text, "😺first: 次 x!");
        assert_eq!(line.display_offset(4, false), 4);
        assert_eq!(line.display_offset(4, true), 15);
        assert_eq!(line.source_offset(12), 4);
        assert_eq!(line.source_offset(15), 4);
        assert_eq!(line.source_offset(17), 5);
        assert!(line.contains(12));
        assert!(!line.contains(15));
    }
}
