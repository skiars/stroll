use gpui_component::input::RopeExt as _;
use unicode_segmentation::UnicodeSegmentation as _;

use super::{
    App, BLOCKWISE, Block, Change, ClipboardItem, Context, EditorState, Indent, IndentTarget,
    InsertSession, LINEWISE, Mode, Motion, Range, Register, Rope, Row, SelectionShape, Source,
    SurroundEdit, Target, Window, block, first_non_blank, last_row, line, line_end, move_cursor,
    next, previous, row_boundary, selection_motion, selection_range,
};

impl Source {
    pub(super) fn finish_insert(
        &mut self,
        editor: &mut EditorState,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) -> usize {
        let at = editor.cursor();
        let Some(session) = self.insert.take() else {
            return previous(editor.text(), at).max(line(editor.text(), at).start);
        };
        let block = session.block.map(|(block, _)| block);
        let suffix = session.baseline.len() - session.start;
        let text = editor.text();
        let entered = text.len().checked_sub(suffix).and_then(|end| {
            (end >= session.start
                && text.slice(..session.start) == session.baseline.slice(..session.start)
                && text.slice(end..) == session.baseline.slice(session.start..))
            .then(|| text.slice(session.start..end).to_string())
        });
        let mut at = at;
        if let Some(text) = entered {
            if let Some((block, insert)) = session.block {
                if !text.contains('\n') {
                    if !text.is_empty() {
                        let tab = editor.tab_width();
                        let mut edits = insert.edits(editor.text(), insert.first + 1, tab, &text);
                        let top = Row::new(&session.baseline, insert.first, tab);
                        let column = insert.column.unwrap_or(top.width);
                        let correction = top.insertion_offset(column);
                        if correction > 0
                            && let Some((range, replacement)) =
                                top.insert(Some(column), insert.pad, &text)
                        {
                            edits.insert(0, (range.start..range.end + text.len(), replacement));
                            at += correction;
                        }
                        editor.edit_ranges(&edits, window, cx);
                    }
                    if !text.is_empty() || session.key == 'c' {
                        self.last_change = Some(Change::BlockEdit {
                            shape: block.shape(),
                            key: session.key,
                            text,
                        });
                    }
                }
            } else if !text.is_empty()
                || session.motion.is_some()
                || matches!(session.key, 'o' | 'O')
            {
                if session.count > 1 {
                    if matches!(session.key, 'o' | 'O') {
                        let start = line(editor.text(), session.start).start;
                        let body = editor
                            .text()
                            .slice(start..session.start + text.len())
                            .to_string();
                        let extra = format!("\n{body}").repeat(session.count - 1);
                        let end = session.start + text.len();
                        editor.edit_ranges(&[(end..end, extra.clone())], window, cx);
                        at = end + extra.len();
                    } else {
                        let extra = text.repeat(session.count - 1);
                        let end = session.start + text.len();
                        editor.edit_ranges(&[(end..end, extra.clone())], window, cx);
                        at = end + extra.len();
                    }
                }
                self.last_change = Some(Change::Insert {
                    key: session.key,
                    motion: session.motion,
                    count: session.count,
                    text,
                });
            }
        }
        editor.end_edit_transaction();
        if let Some(block) = block {
            Row::new(editor.text(), block.first, editor.tab_width()).offset(block.left)
        } else {
            previous(editor.text(), at).max(line(editor.text(), at).start)
        }
    }

    pub(super) fn start_insert(
        &mut self,
        editor: &mut EditorState,
        start: usize,
        key: char,
        motion: Option<Motion>,
        count: usize,
        cx: &mut Context<EditorState>,
    ) {
        editor.set_selected_range(start..start, cx);
        editor.set_modal_editing(false, cx);
        self.mode = Mode::Insert;
        self.clear_selection();
        self.goal_column = None;
        self.insert = Some(InsertSession {
            baseline: editor.text().clone(),
            start,
            key,
            motion,
            count,
            block: None,
        });
    }

    pub(super) fn run_operator(
        &mut self,
        editor: &mut EditorState,
        operator: char,
        motion: Motion,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        if let Some((range, linewise)) = motion.range(editor.text(), editor.cursor()) {
            self.operate(editor, operator, range, linewise, motion, window, cx);
        }
    }

    pub(super) fn operate(
        &mut self,
        editor: &mut EditorState,
        operator: char,
        range: Range<usize>,
        linewise: bool,
        motion: Motion,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        if range.is_empty() && operator != 'c' && !linewise {
            if self.mode.visual() {
                self.leave_visual(editor, range.start, cx);
            }
            return;
        }
        let edit = match operator {
            'c' => Some(change_edit(editor.text(), range.clone(), linewise, "")),
            'd' => Some((
                delete_range(editor.text(), range.clone(), linewise),
                String::new(),
            )),
            _ => None,
        };
        if let Some((range, _)) = &edit
            && !self.allows_edit(std::slice::from_ref(range), cx)
        {
            return;
        }
        let value = editor.text().slice(range.clone()).to_string();
        write_register(
            if linewise {
                Register::Lines(value)
            } else {
                Register::Characters(value)
            },
            cx,
        );
        let at = editor.cursor();
        if operator == 'c' {
            let edit = edit.unwrap();
            let start = edit.0.start;
            editor.begin_edit_transaction();
            editor.edit_ranges(&[edit], window, cx);
            self.start_insert(editor, start, 'i', Some(motion), 1, cx);
        } else if operator == 'd' {
            let edit = edit.unwrap();
            let start = edit.0.start;
            editor.edit_ranges(&[edit], window, cx);
            let at = if linewise {
                let text = editor.text();
                let row = text.offset_to_point(start).row.min(last_row(text));
                first_non_blank(text, text.line_start_offset(row))
            } else {
                start
            };
            self.leave_visual(editor, at, cx);
            self.last_change = Some(Change::Delete(motion));
        } else {
            let head = if self.mode.visual() { range.start } else { at };
            self.leave_visual(editor, head, cx);
            self.highlight_yank(editor, [range], cx);
        }
    }

    pub(super) fn indent_selection(
        &mut self,
        editor: &mut EditorState,
        right: bool,
        levels: usize,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        let text = editor.text();
        let (target, at) = if let Some(block) = self.block(editor) {
            let at = Row::new(text, block.first, editor.tab_width()).offset(block.left);
            (IndentTarget::Block(block.shape()), at)
        } else if let Some((anchor, head)) = self.selection {
            let first = text.offset_to_point(anchor.min(head)).row;
            let last = text.offset_to_point(anchor.max(head)).row;
            let target = IndentTarget::Motion(Motion {
                target: Target::Lines,
                count: last - first + 1,
                explicit: true,
            });
            (target, text.line_start_offset(first))
        } else {
            return;
        };
        self.indent(
            editor,
            Indent {
                target,
                right,
                levels,
            },
            at,
            window,
            cx,
        );
    }

    pub(super) fn indent(
        &mut self,
        editor: &mut EditorState,
        indent: Indent,
        at: usize,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        let text = editor.text();
        let tab = editor.tab_settings();
        let columns = indent.levels.saturating_mul(tab.tab_size);
        let (row, column, edits) = match indent.target {
            IndentTarget::Motion(motion) => {
                let Some((range, _)) = motion.range(text, at) else {
                    return;
                };
                let first = text.offset_to_point(range.start).row;
                let last = text
                    .offset_to_point(previous(text, range.end).max(range.start))
                    .row;
                let edits = (first..=last)
                    .filter_map(|row| {
                        let start = text.line_start_offset(row);
                        let end = line_end(text, start);
                        if start == end {
                            return None;
                        }
                        let line = text.slice(start..end);
                        let whitespace = line
                            .bytes()
                            .take_while(|byte| matches!(byte, b' ' | b'\t'))
                            .count();
                        let current = tab.indent_count(&line);
                        let shifted = if indent.right {
                            current.saturating_add(columns)
                        } else {
                            current.saturating_sub(columns)
                        };
                        (shifted != current).then(|| {
                            (
                                start..start + whitespace,
                                block::indent_text(tab, 0, shifted),
                            )
                        })
                    })
                    .collect::<Vec<_>>();
                (first, None, edits)
            }
            IndentTarget::Block(shape) => {
                let row = text.offset_to_point(at).row;
                let column = if self.mode == Mode::VisualBlock {
                    self.block(editor).map_or(0, |block| block.left)
                } else {
                    Row::new(text, row, tab.tab_size).column(at)
                };
                let span = shape.at(row, column, text);
                let edits = (span.first..=span.last)
                    .filter_map(|row| {
                        Row::new(text, row, tab.tab_size).shift_indent(
                            column,
                            indent.right,
                            columns,
                            tab,
                        )
                    })
                    .collect::<Vec<_>>();
                (row, Some(column), edits)
            }
        };
        if !self.allows_edit(
            &edits
                .iter()
                .map(|(range, _)| range.clone())
                .collect::<Vec<_>>(),
            cx,
        ) {
            return;
        }
        if !edits.is_empty() {
            let revision = editor.document_revision();
            editor.edit_ranges(&edits, window, cx);
            if editor.document_revision() != revision {
                self.last_change = Some(Change::Indent(indent));
            }
        }
        let text = editor.text();
        let head = column.map_or_else(
            || first_non_blank(text, text.line_start_offset(row)),
            |column| Row::new(text, row, tab.tab_size).offset(column),
        );
        self.leave_visual(editor, head, cx);
    }

    pub(super) fn block_edit(
        &mut self,
        editor: &mut EditorState,
        span: Block,
        key: char,
        replacement: Option<&str>,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        let tab = editor.tab_width();
        if matches!(key, 'd' | 'c' | 'y') {
            let slices = span.slices(editor.text(), tab).collect::<Vec<_>>();
            let edits = if key != 'y' {
                let edits = slices
                    .iter()
                    .filter(|slice| !slice.range.is_empty())
                    .map(|slice| slice.edit(""))
                    .collect::<Vec<_>>();
                if !self.allows_edits(&edits, cx) {
                    return;
                }
                edits
            } else {
                Vec::new()
            };
            write_register(
                Register::Block(
                    slices.iter().map(|slice| slice.selected.clone()).collect(),
                    span.right.map(|right| right - span.left),
                ),
                cx,
            );
            if key == 'y' {
                self.highlight_yank(editor, slices.into_iter().map(|slice| slice.range), cx);
            }
            if key == 'c' {
                editor.begin_edit_transaction();
            }
            if !edits.is_empty() {
                editor.edit_ranges(&edits, window, cx);
            }
        }
        let shape = span.shape();
        if matches!(key, 'c' | 'I' | 'A') {
            let mut insertion = span.insertion(key);
            if key == 'I' {
                let Some(first) = (insertion.first..=insertion.last)
                    .find(|row| Row::new(editor.text(), *row, tab).width >= span.left)
                else {
                    return;
                };
                insertion.first = first;
            }
            if key != 'c' {
                editor.begin_edit_transaction();
            }
            let row = Row::new(editor.text(), insertion.first, tab);
            let column = insertion.column.unwrap_or(row.width);
            let start = row.offset(column);
            if let Some(text) = replacement {
                let edits = insertion.edits(editor.text(), insertion.first, tab, text);
                editor.edit_ranges(&edits, window, cx);
                let at = Row::new(editor.text(), span.first, tab).offset(span.left);
                self.leave_visual(editor, at, cx);
                editor.end_edit_transaction();
            } else {
                self.start_insert(editor, start, key, None, 1, cx);
                self.insert.as_mut().unwrap().block = Some((span, insertion));
            }
        } else {
            let at = Row::new(editor.text(), span.first, tab).offset(span.left);
            self.leave_visual(editor, at, cx);
            if key == 'd' {
                self.last_change = Some(Change::BlockEdit {
                    shape,
                    key,
                    text: String::new(),
                });
            }
        }
    }

    fn allows_edits(&self, edits: &[(Range<usize>, String)], cx: &App) -> bool {
        self.edit_guard.is_none()
            || self.allows_edit(
                &edits
                    .iter()
                    .map(|(range, _)| range.clone())
                    .collect::<Vec<_>>(),
                cx,
            )
    }

    pub(super) fn join(
        &mut self,
        editor: &mut EditorState,
        count: usize,
        spaced: bool,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        let text = editor.text();
        let row = self.selection.map_or(
            text.offset_to_point(editor.cursor()).row,
            |(anchor, head)| text.offset_to_point(anchor.min(head)).row,
        );
        let edits = join_edits(text, row, count, spaced);
        if let Some((first, _)) = edits.first()
            && self.allows_edits(&edits, cx)
        {
            // Vim puts the cursor at the final join, after earlier corrections.
            let at = edits
                .iter()
                .take(edits.len() - 1)
                .fold(edits.last().unwrap().0.start, |at, (range, value)| {
                    at - range.len() + value.len()
                });
            let at = at.max(first.start);
            editor.edit_ranges(&edits, window, cx);
            self.leave_visual(editor, at, cx);
            self.last_change = Some(Change::Join(count, spaced));
        }
    }

    pub(super) fn replace_selection_or_count(
        &mut self,
        editor: &mut EditorState,
        key: char,
        count: usize,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        if let Some(block) = self.block(editor) {
            if key == '\n' {
                return;
            }
            let edits = block
                .slices(editor.text(), editor.tab_width())
                .filter(|slice| !slice.range.is_empty())
                .map(|slice| {
                    let value = key
                        .to_string()
                        .repeat(slice.selected.graphemes(true).count());
                    slice.edit(&value)
                })
                .collect::<Vec<_>>();
            let at = Row::new(editor.text(), block.first, editor.tab_width()).offset(block.left);
            if self.allows_edits(&edits, cx) {
                editor.edit_ranges(&edits, window, cx);
                self.leave_visual(editor, at, cx);
            }
        } else {
            let motion = if self.mode.visual() {
                selection_motion(editor, self.mode)
            } else {
                Motion {
                    target: Target::Key('l'),
                    count,
                    explicit: true,
                }
            };
            let range = if self.mode.visual() {
                editor.selected_range()
            } else if let Some((range, _)) = motion.range(editor.text(), editor.cursor()) {
                if editor
                    .text()
                    .slice(range.clone())
                    .to_string()
                    .graphemes(true)
                    .count()
                    < count
                {
                    return;
                }
                range
            } else {
                return;
            };
            if let Some(at) = replace(self, editor, key, range, self.mode.visual(), window, cx) {
                self.leave_visual(editor, at, cx);
                self.last_change = Some(Change::Replace(key, motion));
            }
        }
    }

    pub(super) fn toggle_case(
        &mut self,
        editor: &mut EditorState,
        count: usize,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        let motion = Motion {
            target: Target::Key('l'),
            count,
            explicit: true,
        };
        let Some((range, _)) = motion.range(editor.text(), editor.cursor()) else {
            return;
        };
        if range.is_empty() || !self.allows_edit(std::slice::from_ref(&range), cx) {
            return;
        }

        let value = editor.text().slice(range.clone()).to_string();
        let mut replacement = String::with_capacity(value.len());
        for glyph in value.graphemes(true) {
            let mut chars = glyph.chars();
            let ch = chars.next().unwrap();
            if ch.is_uppercase() {
                replacement.extend(ch.to_lowercase());
            } else {
                replacement.extend(ch.to_uppercase());
            }
            replacement.push_str(chars.as_str());
        }
        let head = range.start + replacement.len();
        if value != replacement {
            editor.edit_ranges(&[(range, replacement)], window, cx);
        }
        self.last_change = Some(Change::ToggleCase(count));
        self.leave_visual(editor, head, cx);
    }

    pub(super) fn surround(
        &mut self,
        editor: &mut EditorState,
        edit: SurroundEdit,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        let text = editor.text();
        let origin = editor.cursor();
        let tab = editor.tab_width();
        let (edits, at) = match &edit {
            SurroundEdit::Add(motion, pair, visual_lines) => {
                let Some((mut range, linewise)) = (if self.mode.visual() {
                    Some((editor.selected_range(), *visual_lines))
                } else {
                    motion.range(text, origin)
                }) else {
                    return;
                };
                if linewise && !visual_lines {
                    range.start = first_non_blank(text, range.start);
                    range.end = line_end(text, previous(text, range.end));
                }
                if range.is_empty() {
                    return;
                }
                let value = text.slice(range.clone()).to_string();
                let result = if *visual_lines {
                    pair.wrap_lines(&value)
                } else {
                    pair.wrap(&value)
                };
                let at = range.start;
                (vec![(range, result)], at)
            }
            SurroundEdit::Existing { from, to, count } => {
                let Some(range) = from.enclosing(text, origin, *count) else {
                    return;
                };
                let value = from.body(text, range.clone());
                let value = to.map_or_else(|| value.clone(), |pair| pair.wrap(&value));
                let at = range.start;
                (vec![(range, value)], at)
            }
            SurroundEdit::Block(shape, pair) => {
                let block = self.block(editor).unwrap_or_else(|| {
                    let row = text.offset_to_point(origin).row;
                    shape.at(row, Row::new(text, row, tab).column(origin), text)
                });
                let edits = block
                    .slices(text, tab)
                    .filter(|slice| !slice.range.is_empty())
                    .map(|slice| slice.edit(&pair.wrap(&slice.selected)))
                    .collect::<Vec<_>>();
                let at = edits.first().map_or(origin, |(range, _)| range.start);
                (edits, at)
            }
        };
        if !self.allows_edits(&edits, cx) {
            return;
        }
        editor.edit_ranges(&edits, window, cx);
        self.leave_visual(editor, at, cx);
        self.last_change = Some(Change::Surround(edit));
    }

    pub(super) fn repeat(
        &mut self,
        editor: &mut EditorState,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        let Some(change) = self.last_change.clone() else {
            return;
        };
        let at = editor.cursor();
        self.goal_column = None;
        match change {
            Change::Delete(motion) => self.run_operator(editor, 'd', motion, window, cx),
            Change::Indent(indent) => self.indent(editor, indent, editor.cursor(), window, cx),
            Change::Insert {
                key,
                motion,
                count,
                text,
            } => {
                let edit = if let Some(motion) = motion {
                    let Some((range, linewise)) = motion.range(editor.text(), at) else {
                        return;
                    };
                    let edit = change_edit(editor.text(), range, linewise, &text);
                    if !self.allows_edit(std::slice::from_ref(&edit.0), cx) {
                        return;
                    }
                    Some(edit)
                } else {
                    None
                };
                editor.begin_edit_transaction();
                let head = if let Some(edit) = edit {
                    let head = edit.0.start + text.len();
                    editor.edit_ranges(&[edit], window, cx);
                    head
                } else {
                    let start = insert_point(editor, key, window, cx);
                    let value = if count > 1 && matches!(key, 'o' | 'O') {
                        let indent = editor
                            .text()
                            .slice(line(editor.text(), start).start..start)
                            .to_string();
                        format!("{text}{}", format!("\n{indent}{text}").repeat(count - 1))
                    } else {
                        text.repeat(count)
                    };
                    let end = start + value.len();
                    editor.edit_ranges(&[(start..start, value)], window, cx);
                    end
                };
                editor.end_edit_transaction();
                move_cursor(
                    editor,
                    previous(editor.text(), head).max(line(editor.text(), head).start),
                    cx,
                );
            }
            Change::Put(register, after, count) => {
                self.put(editor, register, after, count, window, cx)
            }
            Change::VisualPut(register, shape) => {
                if let Some((range, linewise)) = selection_range(editor.text(), at, shape) {
                    self.put_visual(editor, register, range, linewise, shape, window, cx);
                }
            }
            Change::BlockPut(register, shape, count) => {
                let row = editor.text().offset_to_point(at).row;
                let col = Row::new(editor.text(), row, editor.tab_width()).column(at);
                let block = shape.at(row, col, editor.text());
                self.put_block(editor, register, block, count, window, cx);
            }
            Change::BlockEdit { shape, key, text } => {
                let row = editor.text().offset_to_point(at).row;
                let col = Row::new(editor.text(), row, editor.tab_width()).column(at);
                let block = shape.at(row, col, editor.text());
                self.block_edit(editor, block, key, Some(&text), window, cx);
            }
            Change::Join(count, spaced) => self.join(editor, count, spaced, window, cx),
            Change::Replace(key, motion) => {
                if let Some((range, _)) = motion.range(editor.text(), at)
                    && (!matches!(motion.target, Target::Key('l'))
                        || editor
                            .text()
                            .slice(range.clone())
                            .to_string()
                            .graphemes(true)
                            .count()
                            == motion.count)
                    && let Some(at) = replace(
                        self,
                        editor,
                        key,
                        range,
                        matches!(motion.target, Target::Selection(_)),
                        window,
                        cx,
                    )
                {
                    move_cursor(editor, at, cx);
                }
            }
            Change::ToggleCase(count) => self.toggle_case(editor, count, window, cx),
            Change::Surround(edit) => self.surround(editor, edit, window, cx),
        }
    }
}

impl Source {
    pub(super) fn put(
        &mut self,
        editor: &mut EditorState,
        register: Register,
        after: bool,
        count: usize,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        if let Some(block) = self.block(editor) {
            self.put_block(editor, register, block, count, window, cx);
            return;
        }
        if self.mode.visual() {
            let range = editor.selected_range();
            let Target::Selection(shape) = selection_motion(editor, self.mode).target else {
                unreachable!()
            };
            let register = match register {
                Register::Characters(value) => Register::Characters(value.repeat(count)),
                Register::Lines(value) => Register::Lines(linewise_value(&value, count)),
                Register::Block(rows, width) => Register::Block(
                    rows.iter()
                        .map(|row| block::repeated(row, width, count))
                        .collect(),
                    width.map(|width| width * count),
                ),
            };
            self.put_visual(
                editor,
                register,
                range,
                self.mode == Mode::VisualLine,
                shape,
                window,
                cx,
            );
            return;
        }
        match &register {
            Register::Characters(value) => {
                paste(editor, &value.repeat(count), false, after, window, cx)
            }
            Register::Lines(value) => paste(
                editor,
                &linewise_value(value, count),
                true,
                after,
                window,
                cx,
            ),
            Register::Block(rows, width) => {
                let at = editor.cursor();
                let head = if after {
                    next(editor.text(), at).min(line_end(editor.text(), at))
                } else {
                    at
                };
                let edits = block::put(
                    editor.text(),
                    at,
                    rows,
                    *width,
                    count,
                    after,
                    editor.tab_width(),
                );
                editor.edit_ranges(&edits, window, cx);
                move_cursor(editor, head, cx);
            }
        }
        self.last_change = Some(Change::Put(register, after, count));
    }

    fn put_visual(
        &mut self,
        editor: &mut EditorState,
        register: Register,
        range: Range<usize>,
        linewise: bool,
        shape: SelectionShape,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        if !self.allows_edit(std::slice::from_ref(&range), cx) {
            return;
        }
        let at = range.start;
        if let Register::Block(rows, width) = &register
            && !linewise
        {
            editor.begin_edit_transaction();
            editor.edit_ranges(&[(range, String::new())], window, cx);
            let edits = block::put(
                editor.text(),
                at,
                rows,
                *width,
                1,
                false,
                editor.tab_width(),
            );
            editor.edit_ranges(&edits, window, cx);
            editor.end_edit_transaction();
            self.leave_visual(editor, at, cx);
        } else {
            let (value, is_lines) = match &register {
                Register::Characters(value) => (value.clone(), linewise),
                Register::Lines(value) => (linewise_value(value, 1), true),
                Register::Block(rows, _) => (rows.join("\n"), true),
            };
            let value = if linewise {
                if editor.text().char_at(previous(editor.text(), range.end)) == Some('\n') {
                    linewise_value(&value, 1)
                } else {
                    value.strip_suffix('\n').unwrap_or(&value).to_owned()
                }
            } else if is_lines {
                let body = linewise_value(&value, 1);
                if range.start == line(editor.text(), at).start {
                    body
                } else {
                    format!("\n{body}")
                }
            } else {
                value
            };
            let head = at + value.len();
            editor.edit_ranges(&[(range, value)], window, cx);
            self.leave_visual(
                editor,
                if is_lines {
                    first_non_blank(editor.text(), at)
                } else {
                    previous(editor.text(), head).max(at)
                },
                cx,
            );
        }
        self.last_change = Some(Change::VisualPut(register, shape));
    }

    fn put_block(
        &mut self,
        editor: &mut EditorState,
        register: Register,
        span: Block,
        count: usize,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        let tab = editor.tab_width();
        let text = editor.text();
        let slices = span.slices(text, tab).collect::<Vec<_>>();
        let mut edits = Vec::new();
        match &register {
            Register::Lines(value) => {
                edits.extend(
                    slices
                        .iter()
                        .filter(|slice| !slice.range.is_empty())
                        .map(|slice| slice.edit("")),
                );
                let at = row_boundary(text, span.last + 1);
                let value = linewise_value(value, count);
                let value = if at == text.len() && text.char_at(previous(text, at)) != Some('\n') {
                    format!("\n{}", value.strip_suffix('\n').unwrap_or(&value))
                } else {
                    value
                };
                edits.push((at..at, value));
            }
            Register::Characters(_) | Register::Block(_, _) => {
                let rows;
                let (values, width) = match &register {
                    Register::Characters(value) => {
                        rows = value.split('\n').map(str::to_owned).collect::<Vec<_>>();
                        (rows.as_slice(), None)
                    }
                    Register::Block(rows, width) => (rows.as_slice(), *width),
                    Register::Lines(_) => unreachable!(),
                };
                for (index, slice) in slices.iter().enumerate() {
                    let value = if matches!(register, Register::Characters(_)) && values.len() == 1
                    {
                        values.first()
                    } else {
                        values.get(index)
                    };
                    if let Some(value) = value {
                        let value = block::repeated(value, width, count);
                        if slice.range.is_empty() && !value.is_empty() {
                            let row = Row::new(text, span.first + index, tab);
                            edits.extend(row.insert(Some(span.left), true, &value));
                        } else if !slice.range.is_empty() {
                            edits.push(slice.edit(&value));
                        }
                    } else if !slice.range.is_empty() {
                        edits.push(slice.edit(""));
                    }
                }
                if values.len() > slices.len() {
                    edits.extend(block::put_at_column(
                        text,
                        span.last + 1,
                        span.left,
                        &values[slices.len()..],
                        width,
                        count,
                        tab,
                    ));
                }
            }
        }
        if !self.allows_edits(&edits, cx) {
            return;
        }
        editor.edit_ranges(&edits, window, cx);
        let at = Row::new(editor.text(), span.first, tab).offset(span.left);
        self.leave_visual(editor, at, cx);
        self.last_change = Some(Change::BlockPut(register, span.shape(), count));
    }
}

fn linewise_value(value: &str, count: usize) -> String {
    if value.ends_with('\n') {
        value.repeat(count)
    } else {
        format!("{value}\n").repeat(count)
    }
}

pub(super) fn read_register(cx: &App) -> Option<Register> {
    let item = cx.read_from_clipboard()?;
    let value = item.text()?;
    Some(match item.metadata().map(String::as_str) {
        Some(LINEWISE) => Register::Lines(value),
        Some(tag) if tag.starts_with(BLOCKWISE) => {
            let width = tag[BLOCKWISE.len()..].parse().ok();
            Register::Block(value.split('\n').map(str::to_owned).collect(), width)
        }
        _ => Register::Characters(value),
    })
}

fn write_register(register: Register, cx: &App) {
    let item = match register {
        Register::Characters(value) => ClipboardItem::new_string(value),
        Register::Lines(value) => ClipboardItem::new_string_with_metadata(value, LINEWISE.into()),
        Register::Block(rows, width) => {
            let width = width.map_or_else(String::new, |width| width.to_string());
            ClipboardItem::new_string_with_metadata(rows.join("\n"), format!("{BLOCKWISE}{width}"))
        }
    };
    cx.write_to_clipboard(item);
}

fn delete_range(text: &Rope, mut range: Range<usize>, linewise: bool) -> Range<usize> {
    if linewise
        && range.end == text.len()
        && range.start > 0
        && !text.slice(range.clone()).to_string().ends_with('\n')
    {
        range.start = previous(text, range.start);
    }
    range
}

fn change_edit(
    text: &Rope,
    range: Range<usize>,
    linewise: bool,
    value: &str,
) -> (Range<usize>, String) {
    let indent_end = if linewise {
        let first = first_non_blank(text, range.start);
        if first == range.start
            && text
                .char_at(first)
                .is_some_and(|ch| matches!(ch, ' ' | '\t'))
        {
            line_end(text, first)
        } else {
            first
        }
    } else {
        range.start
    };
    let newline = if linewise
        && range.end > range.start
        && text.char_at(previous(text, range.end)) == Some('\n')
    {
        "\n"
    } else {
        ""
    };
    (indent_end..range.end, format!("{value}{newline}"))
}

fn join_edits(text: &Rope, row: usize, count: usize, spaced: bool) -> Vec<(Range<usize>, String)> {
    let end_row = row.saturating_add(count - 1).min(last_row(text));
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut last = None;
    for row in row..end_row {
        let start = text.line_start_offset(row);
        let end = line_end(text, start);
        let next = text.line_start_offset(row + 1);
        let next_end = line_end(text, next);
        let next_start = if spaced {
            next + text
                .slice(next..next_end)
                .chars()
                .take_while(|ch| matches!(ch, ' ' | '\t'))
                .count()
        } else {
            next
        };
        if end > start {
            last = Some(previous(text, end));
        }
        let head = text.char_at(next_start);
        let space = spaced
            && next_start < next_end
            && head != Some(')')
            && last
                .and_then(|at| text.char_at(at))
                .is_some_and(|ch| !matches!(ch, ' ' | '\t'));
        if let Some((range, _)) = edits.last_mut().filter(|(range, _)| range.end == end) {
            range.end = next_start;
            if space {
                edits.last_mut().unwrap().1.push(' ');
            }
        } else {
            edits.push((
                end..next_start,
                if space { " ".to_owned() } else { String::new() },
            ));
        }
    }
    edits
}

fn replace(
    vim: &Source,
    editor: &mut EditorState,
    key: char,
    range: Range<usize>,
    visual: bool,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) -> Option<usize> {
    if range.is_empty() || !vim.allows_edit(std::slice::from_ref(&range), cx) {
        return None;
    }
    let value = editor.text().slice(range.clone()).to_string();
    let replacement = if key == '\n' && !visual {
        let start = line(editor.text(), range.start).start;
        let indent = editor
            .text()
            .slice(start..first_non_blank(editor.text(), start))
            .to_string();
        format!("\n{indent}")
    } else {
        let mut replacement = String::new();
        for glyph in value.graphemes(true) {
            if matches!(glyph, "\n" | "\r\n") {
                replacement.push_str(glyph);
            } else {
                replacement.push(key);
            }
        }
        replacement
    };
    let start = range.start;
    let end = start + replacement.len();
    editor.edit_ranges(&[(range, replacement)], window, cx);
    Some(if visual {
        start
    } else if key == '\n' {
        end
    } else {
        previous(editor.text(), end)
    })
}

pub(super) fn insert_point(
    editor: &mut EditorState,
    key: char,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) -> usize {
    let text = editor.text();
    let at = editor.cursor();
    match key {
        'a' => next(text, at).min(line_end(text, at)),
        'I' => first_non_blank(text, at),
        'A' => line_end(text, at),
        'o' | 'O' => {
            let start = line(text, at).start;
            let indent = text.slice(start..first_non_blank(text, at)).to_string();
            let offset = if key == 'o' {
                line_end(text, at)
            } else {
                start
            };
            let value = if key == 'o' {
                format!("\n{indent}")
            } else {
                format!("{indent}\n")
            };
            editor.edit_ranges(&[(offset..offset, value)], window, cx);
            offset + indent.len() + usize::from(key == 'o')
        }
        _ => at,
    }
}

fn paste(
    editor: &mut EditorState,
    value: &str,
    linewise: bool,
    after: bool,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) {
    let at = editor.cursor();
    let text = editor.text();
    let row = line(text, at);
    let (offset, text, head) = if linewise {
        let value = value.strip_suffix('\n').unwrap_or(value);
        if after && line_end(text, at) == text.len() {
            (row.end, format!("\n{value}"), row.end + 1)
        } else {
            let offset = if after { row.end } else { row.start };
            (offset, format!("{value}\n"), offset)
        }
    } else {
        let offset = if after {
            next(text, at).min(line_end(text, at))
        } else {
            at
        };
        (offset, value.to_owned(), offset + value.len())
    };
    editor.edit_ranges(&[(offset..offset, text)], window, cx);
    let head = if linewise {
        head
    } else {
        previous(editor.text(), head)
    };
    move_cursor(editor, head, cx);
}
