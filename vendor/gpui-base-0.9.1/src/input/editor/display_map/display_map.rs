/// DisplayMap: Public facade for Editor/Input display mapping.
///
/// This combines WrapMap and FoldMap to provide a unified API:
/// - BufferPoint ↔ DisplayPoint conversion
/// - Fold management (candidates, toggle, query)
/// - Automatic projection updates on text/layout changes
use std::ops::Range;

use gpui::{App, Font, Pixels};
use ropey::Rope;

use super::fold_map::FoldMap;
use super::folding::FoldRange;
pub use super::text_wrapper::WrappingIndent;
use super::text_wrapper::{LineItem, WrapDisplayPoint};
use super::wrap_map::WrapMap;
use super::{BufferPoint, DisplayPoint};
use crate::input::display_map::WrapPoint;
use crate::input::rope_ext::RopeExt as _;
use crate::input::{InlayHint, inlay_hints::InlayLine};

/// DisplayMap is the main interface for Editor/Input coordinate mapping.
///
/// It manages the two-layer projection:
/// 1. Buffer → Wrap (soft-wrapping)
/// 2. Wrap → Display (folding)
///
/// Editor/Input only needs to work with BufferPoint and DisplayPoint.
pub struct DisplayMap {
    wrap_map: WrapMap,
    wrap_revision: usize,
    fold_map: FoldMap,
    blocks: Vec<(usize, usize)>,
    // Cumulative pixel adjustment after each custom-height display row.
    row_height_adjustments: Vec<(usize, Pixels)>,
}

impl DisplayMap {
    pub fn new(font: Font, font_size: Pixels, wrap_width: Option<Pixels>) -> Self {
        Self {
            wrap_map: WrapMap::new(font, font_size, wrap_width),
            wrap_revision: 0,
            fold_map: FoldMap::new(),
            blocks: Vec::new(),
            row_height_adjustments: Vec::new(),
        }
    }

    pub(crate) fn enable_progressive_wrap(&mut self) {
        self.wrap_map.enable_progressive_wrap();
    }

    pub(crate) fn wrap_pending(&self) -> bool {
        self.wrap_map.wrap_pending()
    }

    pub(crate) fn prepare_wrap_rows(&mut self, rows: Range<usize>, cx: &mut App) {
        if self.wrap_map.prepare_wrap_rows(rows, cx) {
            self.fold_map.invalidate_projection();
            self.rebuild_fold_projection();
        }
    }

    pub(crate) fn wrap_batch(&mut self, cx: &App) -> Option<super::text_wrapper::WrapBatch> {
        self.wrap_map.wrapper_mut().wrap_batch(cx)
    }

    pub(crate) fn apply_wrap_batch(
        &mut self,
        job: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        rows: Range<usize>,
        items: Vec<LineItem>,
    ) -> bool {
        if !self
            .wrap_map
            .wrapper_mut()
            .apply_wrap_batch(job, rows, items)
        {
            return false;
        }
        self.fold_map.invalidate_projection();
        self.rebuild_fold_projection();
        true
    }

    // ==================== Core Coordinate Mapping ====================

    /// Convert buffer position to display position
    pub fn buffer_pos_to_display_pos(&self, pos: BufferPoint) -> DisplayPoint {
        // Buffer → Wrap
        let wrap_pos = self.wrap_map.buffer_pos_to_wrap_pos(pos);

        // Wrap → Display
        if let Some(display_row) = self.wrap_row_to_display_row(wrap_pos.row) {
            DisplayPoint::new(display_row, wrap_pos.col)
        } else {
            // Cursor is in a folded region, find nearest visible row
            let display_row = self.nearest_visible_display_row(wrap_pos.row);
            DisplayPoint::new(display_row, 0) // Column 0 at fold boundary
        }
    }

    /// Convert display position to buffer position
    pub fn display_pos_to_buffer_pos(&self, pos: DisplayPoint) -> BufferPoint {
        // Display → Wrap
        let wrap_row = self.display_row_to_wrap_row(pos.row).unwrap_or(0);

        // Wrap → Buffer
        let wrap_pos = WrapPoint::new(wrap_row, pos.col);
        self.wrap_map.wrap_pos_to_buffer_pos(wrap_pos)
    }

    /// Get total number of visible display rows
    #[inline]
    pub fn display_row_count(&self) -> usize {
        self.fold_map.display_row_count() + self.block_row_count()
    }

    /// Get the buffer line for a given display row
    pub fn display_row_to_buffer_line(&self, display_row: usize) -> usize {
        match self.display_row_to_wrap_row(display_row) {
            Some(wrap_row) => self.wrap_map.wrap_row_to_buffer_line(wrap_row),
            None if self.blocks.is_empty() => 0,
            None => self.buffer_line_count().saturating_sub(1),
        }
    }

    /// In a projected document, virtual rows pad the preceding line.
    pub(crate) fn projected_buffer_line(&self, display_row: usize) -> usize {
        let line = self.display_row_to_buffer_line(display_row);
        if line > 0 && self.buffer_line_to_display_row(line) > display_row {
            line - 1
        } else {
            line
        }
    }

    /// Get the display row range for a buffer line: [start, end)
    /// Returns None if the buffer line is completely hidden
    pub fn buffer_line_to_display_row_range(&self, line: usize) -> Option<Range<usize>> {
        // Buffer line → Wrap row range
        let wrap_row_range = self.wrap_map.buffer_line_to_wrap_row_range(line);

        // Find first and last visible display rows in this range
        let mut first_display_row = None;
        let mut last_display_row = None;

        for wrap_row in wrap_row_range {
            if let Some(display_row) = self.wrap_row_to_display_row(wrap_row) {
                if first_display_row.is_none() {
                    first_display_row = Some(display_row);
                }
                last_display_row = Some(display_row);
            }
        }

        if let (Some(start), Some(end)) = (first_display_row, last_display_row) {
            Some(start..end + 1)
        } else {
            None // Completely folded
        }
    }

    /// Check if a buffer line is completely hidden
    #[inline]
    pub fn is_buffer_line_hidden(&self, line: usize) -> bool {
        self.buffer_line_to_display_row_range(line).is_none()
    }

    /// First display row of a buffer line. If the line is fully folded, returns the
    /// nearest visible display row.
    pub fn buffer_line_to_display_row(&self, line: usize) -> usize {
        match self.buffer_line_to_display_row_range(line) {
            Some(range) => range.start,
            None => {
                let wrap_row = self.wrap_map.buffer_line_to_first_wrap_row(line);
                self.nearest_visible_display_row(wrap_row)
            }
        }
    }

    /// Set fold candidates (from tree-sitter/LSP)
    pub fn set_fold_candidates(&mut self, candidates: Vec<FoldRange>) {
        self.fold_map.set_candidates(candidates);
        self.rebuild_fold_projection();
    }

    /// Set a fold at the given start_line (must be in candidates)
    pub fn set_folded(&mut self, start_line: usize, folded: bool) {
        self.fold_map.set_folded(start_line, folded);
        self.rebuild_fold_projection();
    }

    /// Toggle fold at the given start_line
    pub fn toggle_fold(&mut self, start_line: usize) {
        self.fold_map.toggle_fold(start_line);
        self.rebuild_fold_projection();
    }

    /// Check if a line is currently folded
    #[inline]
    pub fn is_folded_at(&self, start_line: usize) -> bool {
        self.fold_map.is_folded_at(start_line)
    }

    /// Check if a line is a fold candidate
    #[inline]
    pub fn is_fold_candidate(&self, start_line: usize) -> bool {
        self.fold_map.is_fold_candidate(start_line)
    }

    /// Get all currently folded ranges
    #[inline]
    pub fn folded_ranges(&self) -> &[FoldRange] {
        self.fold_map.folded_ranges()
    }

    /// Clear all folds
    pub fn clear_folds(&mut self) {
        self.fold_map.clear_folds();
        self.rebuild_fold_projection();
    }

    // ==================== Text and Layout Updates ====================

    /// Adjust folds and candidates for a text edit before updating the wrap map.
    ///
    /// Must be called with the OLD text (before replacement) and the edit range/new_text
    /// so we can compute which old lines were affected.
    pub fn adjust_folds_for_edit(&mut self, old_text: &Rope, range: &Range<usize>, new_text: &str) {
        if self.fold_map.folded_ranges().is_empty() && self.fold_map.fold_candidates().is_empty() {
            return;
        }

        let edit_start = old_text.offset_to_point(range.start);
        let edit_end = old_text.offset_to_point(range.end.min(old_text.len()));
        let new_end_line = edit_start.row + new_text.bytes().filter(|&byte| byte == b'\n').count();
        self.fold_map
            .adjust_folds_for_edit(edit_start, edit_end, new_end_line);
    }

    /// Incrementally update fold candidates after a text edit.
    ///
    /// Extracts new fold candidates only within the edited byte range
    /// and merges them with existing (already adjusted) candidates.
    pub fn update_fold_candidates_for_edit(
        &mut self,
        extract_fold_ranges: impl FnOnce(Range<usize>, &Rope) -> Vec<FoldRange>,
        edit_byte_range: Range<usize>,
        new_text: &Rope,
    ) {
        let new_start_line = new_text.offset_to_point(edit_byte_range.start).row;
        let new_end_line = new_text
            .offset_to_point(edit_byte_range.end.min(new_text.len()))
            .row;

        let new_candidates = extract_fold_ranges(edit_byte_range, new_text);
        self.fold_map
            .merge_candidates_for_edit(new_start_line, new_end_line, new_candidates);
    }

    /// Update text (incremental or full)
    pub fn on_text_changed(
        &mut self,
        changed_text: &Rope,
        range: &Range<usize>,
        new_text: &Rope,
        cx: &mut App,
    ) {
        self.wrap_map
            .on_text_changed(changed_text, range, new_text, cx);
        self.rebuild_fold_projection();
    }

    /// Update layout parameters (wrap width or font)
    pub(crate) fn set_inline_metrics(
        &mut self,
        metrics: std::rc::Rc<[(std::ops::Range<usize>, Pixels)]>,
        cx: &mut App,
    ) {
        self.wrap_map.set_inline_metrics(metrics, cx);
        self.rebuild_fold_projection();
    }

    pub fn on_layout_changed(&mut self, wrap_width: Option<Pixels>, cx: &mut App) {
        self.wrap_map.on_layout_changed(wrap_width, cx);
        self.rebuild_fold_projection();
    }

    pub(crate) fn set_inlays(&mut self, text: &Rope, hints: Vec<InlayHint>, cx: &mut App) -> bool {
        if !self.wrap_map.set_inlays(text, hints, cx) {
            return false;
        }
        self.rebuild_fold_projection();
        true
    }

    pub(crate) fn inlay_line(&self, row: usize) -> Option<&std::rc::Rc<InlayLine>> {
        self.wrap_map.inlay_line(row)
    }

    pub(crate) fn source_column(&self, row: usize, column: usize) -> usize {
        self.inlay_line(row)
            .map_or(column, |line| line.source_offset(column))
    }

    pub(crate) fn rendered_text(&self) -> &Rope {
        self.wrap_map.wrapper().text()
    }

    /// Set the wrapping indent for continuation lines.
    pub fn set_wrapping_indent(&mut self, wrapping_indent: WrappingIndent, cx: &mut App) {
        self.wrap_map.set_wrapping_indent(wrapping_indent, cx);
        self.rebuild_fold_projection();
    }

    /// Set the number of spaces per tab stop.
    pub(crate) fn set_tab_size(&mut self, tab_size: usize, cx: &mut App) {
        if self.wrap_map.set_tab_size(tab_size, cx) {
            self.rebuild_fold_projection();
        }
    }

    /// Set font parameters
    pub fn set_font(&mut self, font: Font, font_size: Pixels, cx: &mut App) {
        self.wrap_map.set_font(font, font_size, cx);
        self.rebuild_fold_projection();
    }

    /// Ensure text is prepared (initializes wrapper if needed)
    pub fn ensure_text_prepared(&mut self, text: &Rope, cx: &mut App) {
        let did_initialize = self.wrap_map.ensure_text_prepared(text, cx);
        if did_initialize {
            self.rebuild_fold_projection();
        }
    }

    /// Initialize with text
    pub fn set_text(&mut self, text: &Rope, cx: &mut App) {
        self.wrap_map.set_text(text, cx);
        self.rebuild_fold_projection();
    }

    // ==================== Internal Helpers ====================

    /// Rebuild fold projection after wrap_map or fold state changes
    /// Only rebuilds if there are actually folded ranges
    fn rebuild_fold_projection(&mut self) {
        self.wrap_revision = self.wrap_revision.wrapping_add(1);
        if !self.fold_map.folded_ranges().is_empty() {
            self.fold_map.rebuild(&self.wrap_map);
        } else {
            // No active folds: identity mapping (wrap_row == display_row).
            // Just update cached count so query methods work without Vec allocation.
            self.fold_map
                .mark_dirty_with_wrap_count(self.wrap_map.wrap_row_count());
        }
    }

    // ==================== Wrap Display Point Operations ====================

    /// Convert byte offset to wrap display point (with soft wrap info).
    #[inline]
    pub(crate) fn offset_to_wrap_display_point(&self, offset: usize) -> WrapDisplayPoint {
        self.offset_to_wrap_display_point_with_affinity(offset, false)
    }

    /// Like [`Self::offset_to_wrap_display_point`], but honours the caret's line-end affinity so
    /// an offset on a soft wrap boundary resolves to the row the caret is drawn on.
    #[inline]
    pub(crate) fn offset_to_wrap_display_point_with_affinity(
        &self,
        offset: usize,
        line_end_affinity: bool,
    ) -> WrapDisplayPoint {
        self.wrap_map
            .wrapper()
            .offset_to_display_point_with_affinity(
                self.wrap_map.display_offset(offset),
                line_end_affinity,
            )
    }

    /// Resolve the caret's visual row through both wrapping and folded regions.
    pub(crate) fn offset_to_display_row(&self, offset: usize, line_end_affinity: bool) -> usize {
        let point = self.offset_to_wrap_display_point_with_affinity(offset, line_end_affinity);
        self.wrap_row_to_display_row(point.row)
            .unwrap_or_else(|| self.nearest_visible_display_row(point.row))
    }

    /// Convert wrap display point to byte offset.
    #[inline]
    pub(crate) fn wrap_display_point_to_offset(&self, point: WrapDisplayPoint) -> usize {
        self.wrap_map
            .source_offset(self.wrap_map.wrapper().display_point_to_offset(point))
    }

    /// Return the source line and the continuation index of a wrap row.
    pub(crate) fn wrap_row_location(&self, row: usize) -> (usize, usize) {
        let line = self.wrap_map.wrap_row_to_buffer_line(row);
        (
            line,
            row.saturating_sub(self.wrap_map.buffer_line_to_first_wrap_row(line)),
        )
    }

    /// Convert a wrap row to a display row, accounting for folds and virtual rows.
    pub fn wrap_row_to_display_row(&self, wrap_row: usize) -> Option<usize> {
        self.fold_map
            .wrap_row_to_display_row(wrap_row)
            .map(|row| row + self.block_rows_through(row))
    }

    /// Find the nearest visible display row for a given wrap row.
    pub fn nearest_visible_display_row(&self, wrap_row: usize) -> usize {
        let row = self.fold_map.nearest_visible_display_row(wrap_row);
        row + self.block_rows_through(row)
    }

    /// Resolve non-editable block rows to the following buffer row.
    pub fn display_row_to_wrap_row(&self, mut display_row: usize) -> Option<usize> {
        for &(before, count) in &self.blocks {
            if display_row < before {
                break;
            }
            if display_row < before + count {
                display_row = before;
                break;
            }
            display_row -= count;
        }
        self.fold_map.display_row_to_wrap_row(display_row)
    }

    /// Place virtual rows at the boundaries of the folded display, in order.
    pub(crate) fn set_blocks(&mut self, blocks: Vec<(usize, usize)>) {
        self.blocks = blocks;
    }

    pub(crate) fn set_row_heights(
        &mut self,
        heights: impl IntoIterator<Item = (usize, Pixels)>,
        line_height: Pixels,
    ) -> bool {
        let mut delta = gpui::px(0.);
        let adjustments = heights
            .into_iter()
            .filter_map(|(line, height)| {
                let row = self.buffer_line_to_display_row_range(line)?.start;
                delta += height - line_height;
                Some((row + 1, delta))
            })
            .collect::<Vec<_>>();
        if self.row_height_adjustments == adjustments {
            return false;
        }
        self.row_height_adjustments = adjustments;
        true
    }

    pub(crate) fn display_row_top(&self, row: usize, line_height: Pixels) -> Pixels {
        let index = self
            .row_height_adjustments
            .partition_point(|&(boundary, _)| boundary <= row);
        let delta = index
            .checked_sub(1)
            .map_or(gpui::px(0.), |ix| self.row_height_adjustments[ix].1);
        row * line_height + delta
    }

    /// The row containing a y coordinate measured from the start of the text.
    pub(crate) fn display_row_at_y(&self, y: Pixels, line_height: Pixels) -> usize {
        let count = self.display_row_count();
        if self.row_height_adjustments.is_empty() {
            return ((y / line_height).max(0.) as usize).min(count.saturating_sub(1));
        }
        let (mut first, mut end) = (0, count);
        while first < end {
            let mid = first + (end - first) / 2;
            if self.display_row_top(mid, line_height) <= y {
                first = mid + 1;
            } else {
                end = mid;
            }
        }
        first.saturating_sub(1)
    }

    pub(crate) fn blocks(&self) -> &[(usize, usize)] {
        &self.blocks
    }

    pub(crate) fn block_row_count(&self) -> usize {
        self.blocks.iter().map(|&(_, count)| count).sum()
    }

    fn block_rows_through(&self, row: usize) -> usize {
        self.blocks
            .iter()
            .take_while(|&&(before, _)| before <= row)
            .map(|&(_, count)| count)
            .sum()
    }

    /// A buffer-line boundary in the folded display before virtual rows are inserted.
    pub(crate) fn block_boundary(&self, line: usize) -> Option<usize> {
        if line >= self.buffer_line_count() {
            return Some(self.fold_map.display_row_count());
        }
        let wrap_row = self.wrap_map.buffer_line_to_first_wrap_row(line);
        self.fold_map.wrap_row_to_display_row(wrap_row)
    }

    /// The first virtual display row at this buffer-line boundary.
    pub(crate) fn block_start(&self, line: usize) -> Option<usize> {
        let boundary = self.block_boundary(line)?;
        let inserted = boundary
            .checked_sub(1)
            .map_or(0, |row| self.block_rows_through(row));
        Some(boundary + inserted)
    }

    /// Move through editable rows, skipping virtual blocks.
    pub(crate) fn move_wrap_row(&self, wrap_row: usize, delta: isize) -> usize {
        let row = self.fold_map.nearest_visible_display_row(wrap_row);
        let target = row
            .saturating_add_signed(delta)
            .min(self.fold_map.display_row_count().saturating_sub(1));
        self.fold_map
            .display_row_to_wrap_row(target)
            .unwrap_or(wrap_row)
    }

    pub(crate) fn display_row_column_to_offset(&self, display_row: usize, column: usize) -> usize {
        let wrap_row = self.display_row_to_wrap_row(display_row).unwrap_or(0);
        self.wrap_display_point_to_offset(WrapDisplayPoint::new(wrap_row, 0, column))
    }

    /// Get the longest row index (by byte length).
    #[inline]
    pub(crate) fn longest_row(&self) -> usize {
        self.wrap_map.wrapper().longest_row()
    }

    // ==================== Access Methods ====================

    /// Get the line item by buffer row index.
    #[inline]
    pub(crate) fn line(&self, row: usize) -> Option<&LineItem> {
        self.wrap_map.line(row)
    }

    /// Get the rope text
    #[inline]
    pub fn text(&self) -> &Rope {
        self.wrap_map.text()
    }

    /// Calculate how many wrap rows of a buffer line are visible (not folded)
    #[inline]
    pub fn visible_wrap_row_count_for_buffer_line(&self, line: usize) -> usize {
        self.wrap_map
            .visible_wrap_row_count_for_line(line, &self.fold_map)
    }

    pub(crate) fn wrap_revision(&self) -> usize {
        self.wrap_revision
    }

    pub(crate) fn visible_wrap_counts(&self) -> impl Iterator<Item = usize> + '_ {
        let folded = !self.fold_map.folded_ranges().is_empty();
        self.wrap_map
            .wrapper()
            .iter_lines()
            .enumerate()
            .map(move |(line, item)| {
                if folded {
                    self.visible_wrap_row_count_for_buffer_line(line)
                } else {
                    item.lines_len()
                }
            })
    }

    /// Get the wrap row count (before folding)
    #[inline]
    pub fn wrap_row_count(&self) -> usize {
        self.wrap_map.wrap_row_count()
    }

    /// Get the buffer line count (logical lines)
    #[inline]
    pub fn buffer_line_count(&self) -> usize {
        self.wrap_map.buffer_line_count()
    }
}
