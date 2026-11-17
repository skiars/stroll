/// WrapMap: Soft-wrapping layer (Buffer → Wrap rows).
///
/// This module wraps the existing TextWrapper and provides:
/// - BufferPoint ↔ WrapPoint mapping
/// - Efficient buffer_line → wrap_row queries via prefix sum cache
/// - Incremental updates when text or layout changes
use std::{collections::BTreeMap, ops::Range, rc::Rc};

use gpui::{App, Font, Pixels};
use ropey::Rope;

use super::fold_map::FoldMap;
use super::text_wrapper::{LineItem, TextWrapper, WrapDisplayPoint, WrappingIndent};
use super::{BufferPoint, WrapPoint};
use crate::input::RopeExt;
use crate::input::{InlayHint, inlay_hints::InlayLine};

/// WrapMap manages soft-wrapping and provides buffer ↔ wrap coordinate mapping.
///
/// Buffer line ↔ wrap row mapping is backed by the [`TextWrapper`]'s `SumTree`.
pub(super) struct WrapMap {
    /// The underlying text wrapper (reuses existing implementation)
    wrapper: TextWrapper,
    inlays: BTreeMap<usize, Rc<InlayLine>>,
    source: Option<Rope>,
}

impl WrapMap {
    pub(super) fn new(font: Font, font_size: Pixels, wrap_width: Option<Pixels>) -> Self {
        Self {
            wrapper: TextWrapper::new(font, font_size, wrap_width),
            inlays: BTreeMap::new(),
            source: None,
        }
    }

    pub(super) fn enable_progressive_wrap(&mut self) {
        self.wrapper.enable_progressive_wrap();
    }

    pub(super) fn wrap_pending(&self) -> bool {
        self.wrapper.wrap_pending()
    }

    pub(super) fn prepare_wrap_rows(&mut self, rows: Range<usize>, cx: &mut App) -> bool {
        self.wrapper.prepare_wrap_rows(rows, cx)
    }

    pub(super) fn wrapper_mut(&mut self) -> &mut TextWrapper {
        &mut self.wrapper
    }

    /// Get total number of wrap rows (visual rows after soft-wrapping)
    #[inline]
    pub(super) fn wrap_row_count(&self) -> usize {
        self.wrapper.len()
    }

    /// Get total number of buffer lines (logical lines)
    #[inline]
    pub(super) fn buffer_line_count(&self) -> usize {
        self.wrapper.lines_count()
    }

    /// Convert buffer position to wrap position
    pub(super) fn buffer_pos_to_wrap_pos(&self, pos: BufferPoint) -> WrapPoint {
        let BufferPoint { line, col } = pos;

        // Clamp to valid range
        let line = line.min(self.buffer_line_count().saturating_sub(1));
        let col = self
            .inlay_line(line)
            .map_or(col, |inlays| inlays.display_offset(col, true));
        let col = self
            .wrapper
            .line(line)
            .map_or(0, |item| col.min(item.len()));
        let offset = self.wrapper.text().line_start_offset(line) + col;

        // Use TextWrapper's existing conversion
        let display_point = self.wrapper.offset_to_display_point(offset);

        WrapPoint::new(display_point.row, display_point.column)
    }

    /// Convert wrap position to buffer position
    pub(super) fn wrap_pos_to_buffer_pos(&self, pos: WrapPoint) -> BufferPoint {
        let WrapPoint { row, col } = pos;

        // Clamp wrap_row to valid range
        let row = row.min(self.wrap_row_count().saturating_sub(1));

        // Use TextWrapper's existing conversion
        let display_point = WrapDisplayPoint::new(row, 0, col);
        let offset = self.source_offset(self.wrapper.display_point_to_offset(display_point));
        let source = self.text();
        let point = source.offset_to_point(offset);
        BufferPoint::new(point.row, offset - source.line_start_offset(point.row))
    }

    /// Get the buffer line for a given wrap row
    pub(super) fn wrap_row_to_buffer_line(&self, wrap_row: usize) -> usize {
        self.wrapper.wrap_row_to_buffer_line(wrap_row)
    }

    /// Get the first wrap row for a given buffer line
    pub(super) fn buffer_line_to_first_wrap_row(&self, line: usize) -> usize {
        self.wrapper.buffer_line_to_first_wrap_row(line)
    }

    /// Get the wrap row range for a buffer line: [start, end)
    pub(super) fn buffer_line_to_wrap_row_range(&self, line: usize) -> Range<usize> {
        self.wrapper.buffer_line_to_wrap_row_range(line)
    }

    /// Update text (incremental or full)
    pub(super) fn on_text_changed(
        &mut self,
        changed_text: &Rope,
        range: &Range<usize>,
        new_text: &Rope,
        cx: &mut App,
    ) {
        let Some(source) = self.source.take() else {
            self.update_wrapper(changed_text, range, new_text, cx);
            return;
        };
        if range.is_empty() && new_text.len() == 0 {
            self.source = Some(source);
            return;
        }

        let start = source.offset_to_point(range.start);
        let end = source.offset_to_point(range.end);
        let inserted_end = changed_text.offset_to_point(range.start + new_text.len());
        // When both ends are at the start of a row, the suffix row has
        // intact source text and still owns its original hints.
        let first_unchanged = end.row + usize::from(end.column != 0 || inserted_end.column != 0);
        let touched: Vec<_> = self
            .inlays
            .range(start.row..first_unchanged)
            .map(|(&row, _)| row)
            .collect();
        for row in touched {
            self.inlays.remove(&row);
            self.replace_inlay_row(row, &source, cx);
        }

        let mut display = self.wrapper.text().clone();
        let display_range = display.line_start_offset(start.row) + start.column
            ..display.line_start_offset(end.row) + end.column;
        display.replace(display_range.clone(), &new_text.to_string());
        self.update_wrapper(&display, &display_range, new_text, cx);

        if inserted_end.row != end.row {
            let delta = inserted_end.row as isize - end.row as isize;
            let suffix = self.inlays.split_off(&first_unchanged);
            self.inlays.extend(
                suffix
                    .into_iter()
                    .map(|(row, hints)| (row.saturating_add_signed(delta), hints)),
            );
        }
        self.source = (!self.inlays.is_empty()).then(|| changed_text.clone());
    }

    fn update_wrapper(
        &mut self,
        changed_text: &Rope,
        range: &Range<usize>,
        new_text: &Rope,
        cx: &mut App,
    ) {
        self.wrapper.adjust_inline_metrics(range, new_text.len());
        // Before the first layout the font is still the window default; layout shapes it all.
        if !self.wrapper.is_prepared() {
            self.wrapper.set_default_text(changed_text);
            return;
        }
        self.wrapper.update(changed_text, range, new_text, cx);
    }

    pub(super) fn set_inlays(
        &mut self,
        text: &Rope,
        mut hints: Vec<InlayHint>,
        cx: &mut App,
    ) -> bool {
        hints.retain(|hint| {
            hint.offset <= text.len()
                && text.is_char_boundary(hint.offset)
                && !hint.label.trim().is_empty()
                && !hint
                    .label
                    .contains(['\r', '\n', '\u{0085}', '\u{2028}', '\u{2029}'])
        });
        hints.sort_by_key(|hint| hint.offset);
        let mut inlays = BTreeMap::new();
        for hints in hints.chunk_by(|a, b| {
            text.offset_to_point(a.offset).row == text.offset_to_point(b.offset).row
        }) {
            let row = text.offset_to_point(hints[0].offset).row;
            let start = text.line_start_offset(row);
            let source = text.slice_line(row).to_string();
            let end = start + source.len();
            let hints = &hints[..hints.partition_point(|hint| hint.offset <= end)];
            if !hints.is_empty() {
                inlays.insert(row, Rc::new(InlayLine::new(&source, start, hints)));
            }
        }
        if self.inlays == inlays {
            return false;
        }
        let old = std::mem::replace(&mut self.inlays, inlays);
        let mut rows: Vec<_> = old.keys().chain(self.inlays.keys()).copied().collect();
        rows.sort_unstable();
        rows.dedup();
        for row in rows {
            if old.get(&row) != self.inlays.get(&row) {
                self.replace_inlay_row(row, text, cx);
            }
        }
        self.source = (!self.inlays.is_empty()).then(|| text.clone());
        true
    }

    fn clear_inlays(&mut self, cx: &mut App) {
        if let Some(source) = self.source.take() {
            for row in std::mem::take(&mut self.inlays).into_keys() {
                self.replace_inlay_row(row, &source, cx);
            }
        }
    }

    fn replace_inlay_row(&mut self, row: usize, source: &Rope, cx: &mut App) {
        let replacement = self.inlays.get(&row).map_or_else(
            || source.slice_line(row).to_string(),
            |line| line.text.clone(),
        );
        let mut display = self.wrapper.text().clone();
        let range = display.line_start_offset(row)..display.line_end_offset(row);
        display.replace(range.clone(), &replacement);
        self.update_wrapper(&display, &range, &Rope::from(replacement), cx);
    }

    pub(super) fn inlay_line(&self, row: usize) -> Option<&Rc<InlayLine>> {
        self.inlays.get(&row)
    }

    pub(super) fn display_offset(&self, source: usize) -> usize {
        if self.inlays.is_empty() {
            return source;
        }
        let point = self.text().offset_to_point(source);
        let column = self
            .inlay_line(point.row)
            .map_or(point.column, |line| line.display_offset(point.column, true));
        self.wrapper.text().line_start_offset(point.row) + column
    }

    pub(super) fn source_offset(&self, display: usize) -> usize {
        if self.inlays.is_empty() {
            return display;
        }
        let point = self.wrapper.text().offset_to_point(display);
        let column = self
            .inlay_line(point.row)
            .map_or(point.column, |line| line.source_offset(point.column));
        self.text().line_start_offset(point.row) + column
    }

    /// Update layout parameters (wrap width or font)
    pub(super) fn set_inline_metrics(
        &mut self,
        metrics: std::rc::Rc<[(std::ops::Range<usize>, Pixels)]>,
        cx: &mut App,
    ) {
        self.wrapper.set_inline_metrics(metrics, cx);
    }

    pub(super) fn on_layout_changed(&mut self, wrap_width: Option<Pixels>, cx: &mut App) {
        self.wrapper.set_wrap_width(wrap_width, cx);
    }

    /// Set the wrapping indent mode for continuation lines.
    pub(super) fn set_wrapping_indent(&mut self, wrapping_indent: WrappingIndent, cx: &mut App) {
        self.wrapper.set_wrapping_indent(wrapping_indent, cx);
    }

    /// Set the number of spaces per tab stop. Returns whether it changed.
    pub(super) fn set_tab_size(&mut self, tab_size: usize, cx: &mut App) -> bool {
        self.wrapper.set_tab_size(tab_size, cx)
    }

    /// Set font parameters
    pub(super) fn set_font(&mut self, font: Font, font_size: Pixels, cx: &mut App) {
        self.wrapper.set_font(font, font_size, cx);
    }

    /// Ensure text is prepared (initializes wrapper if needed)
    pub(super) fn ensure_text_prepared(&mut self, text: &Rope, cx: &mut App) -> bool {
        if self.source.is_some() && !self.wrapper.is_prepared() {
            let display = self.wrapper.text().clone();
            return self.wrapper.prepare_if_need(&display, cx);
        }
        self.wrapper.prepare_if_need(text, cx)
    }

    /// Initialize with text
    pub(super) fn set_text(&mut self, text: &Rope, cx: &mut App) {
        self.clear_inlays(cx);
        self.wrapper.set_default_text(text);
        self.wrapper.prepare_if_need(text, cx);
    }

    /// Get access to the underlying wrapper (for rendering/hit-testing)
    pub(crate) fn wrapper(&self) -> &TextWrapper {
        &self.wrapper
    }

    /// Get the line item by buffer row index.
    #[inline]
    pub(crate) fn line(&self, row: usize) -> Option<&LineItem> {
        self.wrapper.line(row)
    }

    /// Get the rope text
    pub(super) fn text(&self) -> &Rope {
        self.source.as_ref().unwrap_or_else(|| self.wrapper.text())
    }

    /// Calculate how many wrap rows of a buffer line are visible (not folded)
    pub(super) fn visible_wrap_row_count_for_line(&self, line: usize, fold_map: &FoldMap) -> usize {
        let wrap_range = self.buffer_line_to_wrap_row_range(line);
        wrap_range
            .filter(|&wr| fold_map.wrap_row_to_display_row(wr).is_some())
            .count()
    }
}
