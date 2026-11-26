use std::{ops::Range, rc::Rc, sync::LazyLock};

use gpui::{App, ClipboardItem, Context, Entity, Keystroke, Window};
use gpui_component::input::{self, EditorState, Rope, RopeExt as _};
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};

use super::{Workspace, plain_key};

mod block;
mod edits;
mod motion;
mod surround;

use block::{Block, Row};
use edits::{insert_point, read_register};
use motion::{
    count_prefix, first_non_blank, last_row, line, line_end, motion, move_cursor, next,
    parse_target, previous, row_boundary, selection_motion, selection_range, text_object,
};
use surround::Pair;

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub(in crate::workspace) enum Mode {
    #[default]
    Normal,
    Insert,
    Visual,
    VisualLine,
    VisualBlock,
}

impl Mode {
    pub(super) const ALL: [Self; 5] = [
        Self::Normal,
        Self::Insert,
        Self::Visual,
        Self::VisualLine,
        Self::VisualBlock,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Insert => "INSERT",
            Self::Visual => "VISUAL",
            Self::VisualLine => "V-LINE",
            Self::VisualBlock => "V-BLOCK",
        }
    }
    fn visual(self) -> bool {
        matches!(self, Self::Visual | Self::VisualLine | Self::VisualBlock)
    }
}

#[derive(Clone, Copy)]
pub(in crate::workspace) enum Action {
    Handled,
    Comment,
    Definition,
    References,
    Find(bool),
    Close,
    ClosePrevious,
    Save,
    SaveAndClose,
    Tab(bool),
}

pub(super) fn advance_file_command(
    pending: &mut String,
    stroke: &Keystroke,
    key: Option<char>,
) -> Option<Action> {
    match stroke.key.as_str() {
        "enter" => {
            return Some(match std::mem::take(pending).as_str() {
                ":w" => Action::Save,
                ":q" => Action::Close,
                ":wq" => Action::SaveAndClose,
                _ => Action::Handled,
            });
        }
        "backspace" => {
            pending.pop();
        }
        _ => {
            if let Some(key) = key
                && pending.len() < 12
            {
                pending.push(key);
            }
        }
    }
    None
}

#[derive(Clone, Copy)]
enum Target {
    Key(char),
    FirstLine,
    Lines,
    Find(char, char, bool),
    ChangeWord,
    PercentLine,
    Selection(SelectionShape),
    Object(char, bool),
}

#[derive(Clone, Copy)]
enum SelectionShape {
    Lines(usize),
    Characters(usize),
    Multiline { rows: usize, last_column: usize },
}

#[derive(Clone, Copy)]
struct Motion {
    target: Target,
    count: usize,
    explicit: bool,
}

#[derive(Clone)]
enum Register {
    Characters(String),
    Lines(String),
    Block(Vec<String>, Option<usize>),
}

#[derive(Clone)]
enum Change {
    Delete(Motion),
    Indent(Indent),
    Insert {
        key: char,
        motion: Option<Motion>,
        count: usize,
        text: String,
    },
    Put(Register, bool, usize),
    VisualPut(Register, SelectionShape),
    BlockPut(Register, block::Shape, usize),
    BlockEdit {
        shape: block::Shape,
        key: char,
        text: String,
    },
    Join(usize, bool),
    Replace(char, Motion),
    ToggleCase(usize),
    Surround(SurroundEdit),
}

#[derive(Clone, Copy)]
enum IndentTarget {
    Motion(Motion),
    Block(block::Shape),
}

#[derive(Clone, Copy)]
struct Indent {
    target: IndentTarget,
    right: bool,
    levels: usize,
}

#[derive(Clone)]
enum SurroundEdit {
    Add(Motion, Pair, bool),
    Existing {
        from: Pair,
        to: Option<Pair>,
        count: usize,
    },
    Block(block::Shape, Pair),
}

struct InsertSession {
    baseline: Rope,
    start: usize,
    key: char,
    motion: Option<Motion>,
    count: usize,
    block: Option<(Block, block::Insertion)>,
}

#[derive(Default)]
pub(in crate::workspace) struct Source {
    mode: Mode,
    pending: String,
    pending_surround: Option<Motion>,
    selection: Option<(usize, usize)>,
    block_columns: Option<(usize, usize)>,
    block_end: bool,
    goal_column: Option<usize>,
    page_inset: Option<gpui::Pixels>,
    page_count: Option<usize>,
    last_find: Option<(char, char)>,
    last_change: Option<Change>,
    insert: Option<InsertSession>,
    edit_guard: Option<EditGuard>,
}

pub(in crate::workspace) type EditGuard = Rc<dyn Fn(&[Range<usize>], &App) -> bool>;

const LINEWISE: &str = "stroll.vim.linewise";
const BLOCKWISE: &str = "stroll.vim.blockwise:";

pub(in crate::workspace) struct Find {
    editor: Entity<EditorState>,
    origin: usize,
    query: String,
    case_insensitive: bool,
    options: gpui_base::input::SearchOptions,
    backward: bool,
}

impl Source {
    pub(in crate::workspace) fn mode(&self) -> Mode {
        self.mode
    }

    pub(in crate::workspace) fn pending(&self) -> &str {
        &self.pending
    }

    pub(in crate::workspace) fn accepts_leader(&self) -> bool {
        self.mode != Mode::Insert && self.pending.is_empty()
    }

    pub(in crate::workspace) fn clear_pending(&mut self) -> bool {
        self.pending_surround = None;
        !std::mem::take(&mut self.pending).is_empty()
    }

    pub(in crate::workspace) fn set_edit_guard(&mut self, guard: EditGuard) {
        self.edit_guard = Some(guard);
    }

    fn allows_edit(&self, ranges: &[Range<usize>], cx: &App) -> bool {
        self.edit_guard
            .as_ref()
            .is_none_or(|guard| guard(ranges, cx))
    }

    pub(in crate::workspace) fn reset(&mut self, state: &Entity<EditorState>, cx: &mut App) {
        state.update(cx, |editor, cx| {
            if self.insert.take().is_some() {
                editor.end_edit_transaction();
            }
            self.mode = Mode::Normal;
            self.clear_selection();
            self.clear_pending();
            self.goal_column = None;
            self.page_inset = None;
            self.page_count = None;
            editor.set_modal_editing(false, cx);
        });
    }

    fn clear_selection(&mut self) {
        self.selection = None;
        self.block_columns = None;
        self.block_end = false;
    }

    pub(in crate::workspace) fn escape(
        &mut self,
        state: &Entity<EditorState>,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        if self.clear_pending() {
            return true;
        }
        if self.mode == Mode::Normal {
            if state.read(cx).search_session().is_active() {
                state.update(cx, |editor, cx| editor.close_search(cx));
                return true;
            }
            return false;
        }
        state.update(cx, |editor, cx| {
            let head = if self.mode == Mode::Insert {
                self.finish_insert(editor, window, cx)
            } else {
                self.selection.map_or(editor.cursor(), |(_, head)| head)
            };
            self.mode = Mode::Normal;
            self.clear_selection();
            self.goal_column = None;
            self.page_inset = None;
            editor.set_modal_editing(true, cx);
            move_cursor(editor, head, cx);
        });
        true
    }

    pub(in crate::workspace) fn key_down(
        &mut self,
        state: &Entity<EditorState>,
        locked: bool,
        stroke: &Keystroke,
        soft_wrap: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Action> {
        let editor = state.read(cx);
        if editor.is_composing() {
            return None;
        }
        let at = editor.cursor();
        let locked = locked || !self.allows_edit(&[at..at], cx);
        let key = plain_key(stroke);
        if self.pending.starts_with(':') {
            return Some(
                advance_file_command(&mut self.pending, stroke, key).unwrap_or(Action::Handled),
            );
        }
        if self.mode == Mode::Insert {
            return None;
        }

        let control = stroke.modifiers.control
            && !stroke.modifiers.alt
            && !stroke.modifiers.platform
            && !stroke.modifiers.function;
        let navigation = !stroke.modifiers.modified();
        let (count, pending) = count_prefix(&self.pending);
        let page = match (control, navigation, stroke.key.as_str()) {
            (true, _, "d") => Some((true, true)),
            (true, _, "u") => Some((false, true)),
            (true, _, "f") | (_, true, "pagedown") => Some((true, false)),
            (true, _, "b") | (_, true, "pageup") => Some((false, false)),
            _ => None,
        };
        if let Some((down, half)) = page {
            let explicit = pending.is_empty() && !self.pending.is_empty();
            self.clear_pending();
            state.update(cx, |editor, cx| {
                self.sync_selection(editor);
                let height = editor.line_height().unwrap_or(gpui::px(16.));
                let visible = (editor.input_bounds().size.height / height).max(1.) as usize;
                let rows = if half {
                    if explicit {
                        self.page_count = Some(count);
                    }
                    self.page_count.unwrap_or((visible / 2).max(1))
                } else {
                    visible.saturating_sub(2).max(1).saturating_mul(count)
                };
                if !editor.is_scroll_navigating() {
                    self.page_inset = None;
                }
                let inset = *self
                    .page_inset
                    .get_or_insert_with(|| editor.cursor_viewport_inset().unwrap_or(height));
                if self.mode == Mode::Normal && soft_wrap && editor.line_height().is_some() {
                    let rows = rows.min(isize::MAX as usize) as isize;
                    self.goal_column = None;
                    editor.move_modal_display_rows(if down { rows } else { -rows }, window, cx);
                } else {
                    self.move_head(editor, Target::Key(if down { 'j' } else { 'k' }), rows, cx);
                }
                editor.animate_caret_to_inset(inset, cx);
            });
            return Some(Action::Handled);
        }
        self.page_inset = None;
        if control {
            match stroke.key.as_str() {
                "v" => {
                    self.clear_pending();
                    state.update(cx, |editor, cx| {
                        self.sync_selection(editor);
                        self.toggle_visual(editor, Mode::VisualBlock, cx);
                    });
                }
                "r" if !locked && self.mode == Mode::Normal => {
                    window.dispatch_action(Box::new(input::Redo), cx);
                }
                _ => return None,
            }
            return Some(Action::Handled);
        }

        let key = key.or_else(|| {
            navigation
                .then(|| match stroke.key.as_str() {
                    "left" | "backspace" => Some('h'),
                    "right" => Some('l'),
                    "up" => Some('k'),
                    "down" => Some('j'),
                    "home" => Some('0'),
                    "end" => Some('$'),
                    "enter" if pending == "r" => Some('\n'),
                    "enter" => Some('+'),
                    "delete" => Some('x'),
                    _ => None,
                })
                .flatten()
        })?;
        if key == 'u' && self.pending.is_empty() && self.mode == Mode::Normal {
            if !locked {
                window.dispatch_action(Box::new(input::Undo), cx);
            }
            return Some(Action::Handled);
        }
        state.update(cx, |editor, cx| {
            self.edit_key(
                editor,
                key,
                !locked && editor.is_editable(),
                soft_wrap,
                window,
                cx,
            )
        })
    }

    pub(in crate::workspace) fn sync_selection(&mut self, editor: &EditorState) -> bool {
        if self.mode == Mode::Insert || editor.has_modal_selection() {
            return false;
        }
        let selected = editor.selected_range();
        let old_selection = self.selection;
        let before = self.mode;
        self.clear_selection();
        if selected.is_empty() {
            self.mode = Mode::Normal;
        } else {
            let end = previous(editor.text(), selected.end);
            self.selection = Some(if editor.is_selection_reversed() {
                (end, selected.start)
            } else {
                (selected.start, end)
            });
            self.mode = Mode::Visual;
        }
        if self.selection != old_selection || self.mode != before {
            self.goal_column = None;
            return true;
        }
        false
    }

    pub(in crate::workspace) fn adopt_selection(
        &mut self,
        editor: &mut EditorState,
        cx: &mut Context<EditorState>,
    ) {
        if let Some((anchor, head)) = self.selection {
            self.select(editor, anchor, head, cx);
        }
    }

    fn toggle_visual(
        &mut self,
        editor: &mut EditorState,
        mode: Mode,
        cx: &mut Context<EditorState>,
    ) {
        self.goal_column = None;
        let (anchor, head) = self.selection.unwrap_or((editor.cursor(), editor.cursor()));
        if self.mode == mode {
            self.leave_visual(editor, head, cx);
        } else {
            self.mode = mode;
            self.block_end = false;
            self.block_columns = None;
            self.select(editor, anchor, head, cx);
        }
    }

    pub(in crate::workspace) fn locate(
        &mut self,
        editor: &mut EditorState,
        head: usize,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) {
        if self.mode == Mode::Insert {
            self.finish_insert(editor, window, cx);
        }
        self.clear_pending();
        self.page_inset = None;
        self.page_count = None;
        editor.set_modal_editing(true, cx);
        self.leave_visual(editor, head, cx);
    }

    fn leave_visual(
        &mut self,
        editor: &mut EditorState,
        head: usize,
        cx: &mut Context<EditorState>,
    ) {
        self.mode = Mode::Normal;
        self.clear_selection();
        self.goal_column = None;
        move_cursor(editor, head, cx);
    }

    fn edit_key(
        &mut self,
        editor: &mut EditorState,
        key: char,
        editable: bool,
        soft_wrap: bool,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) -> Option<Action> {
        self.sync_selection(editor);
        let prefix = std::mem::take(&mut self.pending);
        let (count, rest) = count_prefix(&prefix);
        let count = count.min(100_000);
        let explicit = prefix.len() != rest.len();
        let handled = Some(Action::Handled);

        if let Some(motion) = self.pending_surround.take() {
            if editable && let Some(pair) = Pair::for_key(key) {
                self.surround(editor, SurroundEdit::Add(motion, pair, false), window, cx);
            }
            return handled;
        }

        match (rest, key) {
            ("r", _) => {
                if editable {
                    self.replace_selection_or_count(editor, key, count, window, cx);
                }
                return handled;
            }
            ("S", _) if self.mode.visual() => {
                if editable && let Some(pair) = Pair::for_key(key) {
                    let edit = if let Some(block) = self.block(editor) {
                        SurroundEdit::Block(block.shape(), pair)
                    } else {
                        let motion = selection_motion(editor, self.mode);
                        SurroundEdit::Add(motion, pair, self.mode == Mode::VisualLine)
                    };
                    self.surround(editor, edit, window, cx);
                }
                return handled;
            }
            ("ds", _) => {
                if editable && let Some(from) = Pair::for_key(key) {
                    self.surround(
                        editor,
                        SurroundEdit::Existing {
                            from,
                            to: None,
                            count,
                        },
                        window,
                        cx,
                    );
                }
                return handled;
            }
            ("cs", _) => {
                if Pair::for_key(key).is_some() {
                    self.pending = prefix;
                    self.pending.push(key);
                }
                return handled;
            }
            (rest, _) if rest.starts_with("cs") && rest.chars().count() == 3 => {
                if editable
                    && let Some(from) = rest.chars().nth(2).and_then(Pair::for_key)
                    && let Some(to) = Pair::for_key(key)
                {
                    self.surround(
                        editor,
                        SurroundEdit::Existing {
                            from,
                            to: Some(to),
                            count,
                        },
                        window,
                        cx,
                    );
                }
                return handled;
            }
            ("z", 't' | 'z' | 'b') => {
                if explicit {
                    self.move_head(editor, Target::FirstLine, count, cx);
                }
                let height = editor.line_height().unwrap_or(gpui::px(16.));
                let available = (editor.input_bounds().size.height - height).max(gpui::px(0.));
                let inset = match key {
                    't' => gpui::px(0.),
                    'z' => available / 2.,
                    _ => available,
                };
                editor.animate_caret_to_inset(inset, cx);
                return handled;
            }
            ("", 'v' | 'V') => {
                self.toggle_visual(
                    editor,
                    if key == 'v' {
                        Mode::Visual
                    } else {
                        Mode::VisualLine
                    },
                    cx,
                );
                return handled;
            }
            ("", ':') => {
                self.pending.push(':');
                return handled;
            }
            ("", 'K') if !explicit && self.mode == Mode::Normal => {
                editor.show_hover_at_caret(window, cx);
                return handled;
            }
            ("", '/' | '?') => return Some(Action::Find(key == '?')),
            ("", '*' | '#') if self.mode == Mode::Normal => {
                self.search_word(editor, key == '#', count, cx);
                return handled;
            }
            ("", 'n' | 'N') => {
                if let Some(at) = editor.modal_search_match(editor.cursor(), key == 'N', count, cx)
                {
                    self.jump_head(editor, at, cx);
                }
                return handled;
            }
            ("", 'H' | 'L') => return Some(Action::Tab(key == 'L')),
            ("", 'X') => return Some(Action::ClosePrevious),
            ("", 'i' | 'a' | 'I' | 'A' | 'o' | 'O') if !self.mode.visual() => {
                if editable {
                    editor.begin_edit_transaction();
                    let start = insert_point(editor, key, window, cx);
                    self.start_insert(editor, start, key, None, count, cx);
                }
                return handled;
            }
            ("", 'o' | 'O') if self.mode.visual() => {
                if let Some((anchor, head)) = self.selection {
                    if let Some((left, right)) = self.block_columns {
                        self.block_columns = Some((right, left));
                        if key == 'O' {
                            let row = editor.text().offset_to_point(head).row;
                            let head =
                                Row::new(editor.text(), row, editor.tab_width()).offset(left);
                            self.select(editor, anchor, head, cx);
                            return handled;
                        }
                    }
                    self.select(editor, head, anchor, cx);
                }
                return handled;
            }
            ("", '.') => {
                if editable {
                    self.repeat(editor, window, cx);
                }
                return handled;
            }
            ("", 'p' | 'P') => {
                if editable && let Some(register) = read_register(cx) {
                    self.put(editor, register, key == 'p', count, window, cx);
                }
                return handled;
            }
            ("", '~') if self.mode == Mode::Normal => {
                if editable {
                    self.toggle_case(editor, count, window, cx);
                }
                return handled;
            }
            ("", 'd' | 'c' | 's' | 'y' | 'x' | 'D' | 'C' | 'Y') if self.mode.visual() => {
                let operator = match key {
                    'd' | 'x' | 'D' => 'd',
                    'c' | 's' | 'C' => 'c',
                    _ => 'y',
                };
                if editable || operator == 'y' {
                    let span = self.block(editor);
                    if let Some(mut block) = span {
                        if matches!(key, 'D' | 'C') {
                            block.right = None;
                        }
                        self.block_edit(editor, block, operator, None, window, cx);
                    } else {
                        let motion = selection_motion(editor, self.mode);
                        self.operate(
                            editor,
                            operator,
                            editor.selected_range(),
                            self.mode == Mode::VisualLine,
                            motion,
                            window,
                            cx,
                        );
                    }
                }
                return handled;
            }
            ("", 'I' | 'A') if self.mode == Mode::VisualBlock => {
                if editable && let Some(block) = self.block(editor) {
                    self.block_edit(editor, block, key, None, window, cx);
                }
                return handled;
            }
            ("", '>' | '<') if self.mode.visual() => {
                if editable {
                    self.indent_selection(editor, key == '>', count.max(1), window, cx);
                }
                return handled;
            }
            ("" | "g", 'J') => {
                if editable {
                    let count = if self.mode.visual() {
                        let (anchor, head) = self.selection.unwrap();
                        let text = editor.text();
                        text.offset_to_point(anchor)
                            .row
                            .abs_diff(text.offset_to_point(head).row)
                            + 1
                    } else {
                        count
                    };
                    self.join(editor, count.max(2), rest.is_empty(), window, cx);
                }
                return handled;
            }
            ("g", 'd') => return Some(Action::Definition),
            ("g", 'r') => return Some(Action::References),
            ("gc", 'c') => return Some(Action::Comment),
            ("g", 'c') | ("", 'z' | 'r') | ("", 'S') if key != 'S' || self.mode.visual() => {
                self.pending = prefix;
                self.pending.push(key);
                return handled;
            }
            ("d" | "c" | "y", 's') => {
                self.pending = prefix;
                self.pending.push(key);
                return handled;
            }
            _ => {}
        }

        let (operator, tail) = match rest.as_bytes().first().copied() {
            Some(key @ (b'd' | b'c' | b'y' | b'>' | b'<')) => (Some(key as char), &rest[1..]),
            _ => (None, rest),
        };
        let (surround_add, tail) = if let Some(tail) = rest.strip_prefix("ys") {
            (true, tail)
        } else {
            (false, tail)
        };
        let tail = if surround_add {
            tail
        } else if operator.is_some() {
            &rest[1..]
        } else {
            tail
        };
        let (nested, tail) = count_prefix(tail);
        let count = count.saturating_mul(nested).min(100_000);
        if key.is_ascii_digit()
            && tail.is_empty()
            && (key != '0' || prefix.ends_with(|ch: char| ch.is_ascii_digit()))
        {
            self.pending = prefix;
            if self.pending.len() < 26 {
                self.pending.push(key);
            }
            return handled;
        }

        let target = if tail.is_empty() && !surround_add && operator == Some(key) {
            Target::Lines
        } else if tail.is_empty() && surround_add && key == 's' {
            Target::Lines
        } else if tail.is_empty()
            && (matches!(
                key,
                'g' | 'f' | 'F' | 't' | 'T' | 'd' | 'c' | 'y' | '>' | '<'
            ) || (operator.is_some() || self.mode.visual()) && matches!(key, 'i' | 'a'))
        {
            self.pending = prefix;
            self.pending.push(key);
            return handled;
        } else if tail.is_empty()
            && operator.is_none()
            && matches!(key, 'x' | 's' | 'D' | 'C' | 'S' | 'Y')
        {
            let (operator, target) = match key {
                'x' => ('d', Target::Key('l')),
                's' => ('c', Target::Key('l')),
                'D' => ('d', Target::Key('$')),
                'C' => ('c', Target::Key('$')),
                'S' => ('c', Target::Lines),
                'Y' => ('y', Target::Lines),
                _ => unreachable!(),
            };
            if editable || operator == 'y' {
                self.run_operator(
                    editor,
                    operator,
                    Motion {
                        target,
                        count,
                        explicit,
                    },
                    window,
                    cx,
                );
            }
            return handled;
        } else {
            let mut keys = tail.to_owned();
            keys.push(key);
            let Some(mut target) = parse_target(&keys, self.last_find) else {
                return handled;
            };
            if explicit && matches!(target, Target::Key('%')) {
                target = Target::PercentLine;
            }
            target
        };

        if let Target::Find(kind, ch, false) = target {
            self.last_find = Some((kind, ch));
        }
        if surround_add {
            if editable {
                self.pending = prefix;
                self.pending.push(key);
                self.pending_surround = Some(Motion {
                    target,
                    count,
                    explicit,
                });
            }
        } else if let Some(operator @ ('>' | '<')) = operator {
            if editable {
                self.indent(
                    editor,
                    Indent {
                        target: IndentTarget::Motion(Motion {
                            target,
                            count,
                            explicit,
                        }),
                        right: operator == '>',
                        levels: 1,
                    },
                    editor.cursor(),
                    window,
                    cx,
                );
            }
        } else if let Some(operator) = operator {
            if editable || operator == 'y' {
                let target = if operator == 'c' && matches!(target, Target::Key('w')) {
                    Target::ChangeWord
                } else {
                    target
                };
                self.run_operator(
                    editor,
                    operator,
                    Motion {
                        target,
                        count,
                        explicit,
                    },
                    window,
                    cx,
                );
            }
        } else if !explicit
            && self.mode == Mode::Normal
            && soft_wrap
            && editor.line_height().is_some()
            && matches!(target, Target::Key('j' | 'k'))
        {
            self.goal_column = None;
            editor.move_modal_display_rows(
                if matches!(target, Target::Key('j')) {
                    1
                } else {
                    -1
                },
                window,
                cx,
            );
        } else {
            let count = if matches!(target, Target::Key('G')) && !explicit {
                usize::MAX
            } else {
                count
            };
            self.move_head(editor, target, count, cx);
        }
        handled
    }

    fn select(
        &mut self,
        editor: &mut EditorState,
        anchor: usize,
        head: usize,
        cx: &mut Context<EditorState>,
    ) {
        self.selection = Some((anchor, head));
        let text = editor.text();
        if self.mode == Mode::VisualBlock {
            let a_row = text.offset_to_point(anchor).row;
            let h_row = text.offset_to_point(head).row;
            let tab = editor.tab_width();
            self.block_columns.get_or_insert_with(|| {
                (
                    Row::new(text, a_row, tab).column(anchor),
                    Row::new(text, h_row, tab).column(head),
                )
            });
            let span = self.block(editor).unwrap();
            let slices = span.slices(text, tab).collect::<Vec<_>>();
            let active = h_row - span.first;
            let mut endpoints = slices
                .iter()
                .map(|slice| (slice.range.start, slice.range.end))
                .collect::<Vec<_>>();
            endpoints.swap(0, active);
            let geometry = slices
                .into_iter()
                .map(|slice| (slice.range, slice.visual_offsets))
                .collect();
            let row = Row::new(text, h_row, tab);
            let column = self.block_columns.unwrap().1;
            let extra = if self.block_end
                || editor.text().char_at(head) != Some('\t') && head < row.end()
            {
                0
            } else {
                column.saturating_sub(row.column(head))
            };
            editor.set_modal_selection(&endpoints, head, cx);
            editor.set_modal_block(span.first, geometry, extra, cx);
            return;
        }
        let range = anchor.min(head)..anchor.max(head);
        let range = if self.mode == Mode::VisualLine {
            line(text, range.start).start..line(text, range.end).end
        } else {
            range.start..next(text, range.end)
        };
        let endpoints = if head < anchor {
            (range.end, range.start)
        } else {
            (range.start, range.end)
        };
        editor.set_modal_selection(&[endpoints], head, cx);
    }

    fn block(&self, editor: &EditorState) -> Option<Block> {
        let ((anchor, head), (a_col, h_col)) = self.selection.zip(self.block_columns)?;
        let text = editor.text();
        let a_row = text.offset_to_point(anchor).row;
        let h_row = text.offset_to_point(head).row;
        let right = (!self.block_end).then(|| {
            let (row, at, col) = if h_col >= a_col {
                (h_row, head, h_col)
            } else {
                (a_row, anchor, a_col)
            };
            let layout = Row::new(text, row, editor.tab_width());
            if layout.column(at) >= col {
                layout.glyph_end(at).max(col + 1)
            } else {
                col + 1
            }
        });
        Some(Block {
            first: a_row.min(h_row),
            last: a_row.max(h_row),
            left: if self.block_end {
                a_col
            } else {
                a_col.min(h_col)
            },
            right,
        })
    }

    fn search_word(
        &mut self,
        editor: &mut EditorState,
        previous: bool,
        count: usize,
        cx: &mut Context<EditorState>,
    ) {
        static WORD: LazyLock<regex::Regex> = LazyLock::new(|| {
            regex::Regex::new(&format!("{}+", crate::locator::WORD_CHARACTERS)).unwrap()
        });
        let caret = editor.cursor();
        let bounds = line(editor.text(), caret);
        let row = editor.text().slice(bounds.clone()).to_string();
        let Some(word) = WORD
            .find_iter(&row)
            .find(|word| word.range().contains(&(caret - bounds.start)))
        else {
            return;
        };
        let origin = bounds.start + word.start();
        let word = word.as_str().to_owned();
        let case_insensitive = editor.search_session().case_insensitive;
        if editor.search_session().in_selection {
            editor.set_find_in_selection(false, cx);
        }
        if editor.has_advanced_find() {
            let options = gpui_base::input::SearchOptions {
                whole_word: true,
                regex: false,
            };
            editor.set_find_query(word, case_insensitive, options, cx);
        } else {
            // Git diff Find uses the plain panel and can still match a whole word.
            let query = crate::locator::TextQuery {
                query: word,
                case_sensitive: !case_insensitive,
                whole_word: true,
                ..Default::default()
            };
            let Ok(pattern) = query.pattern() else {
                return;
            };
            editor.set_search_pattern(
                query.query,
                case_insensitive,
                move |text| crate::workspace::find::scan(&pattern, text, None).unwrap_or_default(),
                cx,
            );
        }
        if let Some(at) = editor.modal_search_match(origin, previous, count, cx) {
            self.jump_head(editor, at, cx);
        }
    }

    pub(in crate::workspace) fn jump_head(
        &mut self,
        editor: &mut EditorState,
        to: usize,
        cx: &mut Context<EditorState>,
    ) {
        self.goal_column = None;
        if let Some((_, column)) = &mut self.block_columns {
            let text = editor.text();
            *column = Row::new(text, text.offset_to_point(to).row, editor.tab_width()).column(to);
            self.block_end = false;
        }
        self.goto_head(editor, to, cx);
    }

    fn goto_head(&mut self, editor: &mut EditorState, to: usize, cx: &mut Context<EditorState>) {
        if let Some((anchor, _)) = self.selection {
            self.select(editor, anchor, to, cx);
        } else {
            move_cursor(editor, to, cx);
        }
    }

    fn move_head(
        &mut self,
        editor: &mut EditorState,
        target: Target,
        count: usize,
        cx: &mut Context<EditorState>,
    ) {
        let head = editor.cursor();
        let text = editor.text();
        let row = text.offset_to_point(head).row;
        let tab = editor.tab_width();
        if let Target::Object(key, around) = target {
            self.goal_column = None;
            if let Some(range) = text_object(text, head, key, around, count) {
                if self.mode.visual() && self.mode != Mode::VisualBlock {
                    let reverse = self.selection.is_some_and(|(anchor, head)| head < anchor);
                    let end = previous(text, range.end).max(range.start);
                    let (anchor, head) = if reverse {
                        (end, range.start)
                    } else {
                        (range.start, end)
                    };
                    self.select(editor, anchor, head, cx);
                }
            }
            return;
        }
        let to = if let Target::Key(key @ ('j' | 'k')) = target {
            let base = Row::new(text, row, tab);
            let col = *self.goal_column.get_or_insert_with(|| base.column(head));
            let row = if key == 'j' {
                row.saturating_add(count)
            } else {
                row.saturating_sub(count)
            };
            let row = row.min(last_row(text));
            let dest = Row::new(text, row, tab);
            if let Some((_, h_col)) = &mut self.block_columns {
                if self.block_end {
                    dest.offset(dest.last_column())
                } else {
                    dest.offset(*h_col)
                }
            } else {
                dest.offset(col.min(dest.last_column()))
            }
        } else {
            self.goal_column = None;
            let Some((mut to, _)) = motion(text, head, target, count) else {
                return;
            };
            if let Some((_, h_col)) = &mut self.block_columns {
                let dest = Row::new(text, text.offset_to_point(to).row, tab);
                self.block_end = matches!(target, Target::Key('$'));
                *h_col = match target {
                    Target::Key('h') => h_col.saturating_sub(count),
                    Target::Key('l') => h_col.saturating_add(count),
                    Target::Key('$') => dest.last_column(),
                    _ => dest.column(to),
                };
                to = dest.offset(*h_col);
            }
            to
        };
        self.goto_head(editor, to, cx);
    }
}

impl Find {
    pub(in crate::workspace) fn capture(
        editor: &Entity<EditorState>,
        backward: bool,
        cx: &App,
    ) -> Self {
        let state = editor.read(cx);
        let previous = state.search_session();
        Self {
            editor: editor.clone(),
            origin: state.cursor(),
            query: previous.query.clone(),
            case_insensitive: previous.case_insensitive,
            options: previous.options,
            backward,
        }
    }

    pub(in crate::workspace) fn is_active(
        &self,
        editor: &Entity<EditorState>,
        window: &Window,
        cx: &App,
    ) -> bool {
        self.editor == *editor
            && input::Editor::search_has_focus(editor, window, cx)
            && !input::Editor::search_is_composing(editor, cx)
    }

    pub(in crate::workspace) fn finish(
        self,
        source: &mut Source,
        commit: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.editor.update(cx, |editor, cx| {
            if commit {
                if let Some(at) = editor.modal_search_match(self.origin, self.backward, 1, cx) {
                    source.jump_head(editor, at, cx);
                }
            } else {
                editor.set_find_query(self.query, self.case_insensitive, self.options, cx);
                editor.close_search(cx);
                source.goto_head(editor, self.origin, cx);
            }
            editor.focus(window, cx);
        });
    }
}

impl Workspace {
    pub(super) fn finish_vim_find(
        &mut self,
        commit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(find) = &self.vim.search else {
            return false;
        };
        if !self
            .active_file()
            .is_some_and(|file| find.is_active(&file.editor, window, cx))
        {
            return false;
        }
        let find = self.vim.search.take().unwrap();
        let source = &mut self.files.get_mut(self.tabs.active().unwrap()).unwrap().vim;
        find.finish(source, commit, window, cx);
        true
    }

    pub(super) fn vim_source_action(
        &mut self,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            Action::Handled => {}
            Action::Comment => {
                if let Some(file) = self
                    .active_file()
                    .filter(|file| !self.file_editor_locked(file))
                {
                    file.editor
                        .update(cx, |editor, cx| editor.toggle_line_comment(window, cx));
                }
            }
            Action::Definition => window.dispatch_action(Box::new(input::GoToDefinition), cx),
            Action::References => self.find_references(&super::super::FindReferences, window, cx),
            Action::Find(backward) => {
                if let Some(file) = self.active_file() {
                    self.vim.search = Some(Find::capture(&file.editor, backward, cx));
                    self.find(window, cx);
                }
            }
            Action::Close => self.close_active_tab(&super::super::CloseTab, window, cx),
            Action::ClosePrevious => {
                if let Some(path) = self.tabs.active().map(std::path::Path::to_owned) {
                    let previous = self
                        .tabs
                        .index(&path)
                        .and_then(|index| index.checked_sub(1))
                        .and_then(|index| self.tabs.paths().get(index))
                        .cloned();
                    self.close_tabs_preferring(vec![path], previous, window, cx);
                }
            }
            Action::Save => self.save(&super::super::SaveFile, window, cx),
            Action::SaveAndClose => {
                if let Some(path) = self.tabs.active().map(std::path::Path::to_owned) {
                    self.start_save(path, true, window, cx);
                }
            }
            Action::Tab(next) => self.cycle_tabs(next, window, cx),
        }
    }
}
