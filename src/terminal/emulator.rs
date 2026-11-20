use std::{cell::RefCell, rc::Rc, sync::mpsc::Sender, time::Instant};

use alacritty_terminal::{
    Term,
    event::{Event, EventListener, WindowSize},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionType},
    term::{Config, Osc52, TermMode, cell::Flags, color::Colors},
    vte::ansi::{Color, CursorShape, CursorStyle, Handler, NamedColor, Processor, Rgb as VtRgb},
};

use crate::pty::{Command, PtySize};

mod input;
mod links;

pub(crate) use links::{Link, LinkTarget};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct Rgb(pub u8, pub u8, pub u8);

impl From<u32> for Rgb {
    fn from(value: u32) -> Self {
        Self((value >> 16) as u8, (value >> 8) as u8, value as u8)
    }
}

impl Rgb {
    pub(crate) fn hex(self) -> u32 {
        u32::from_be_bytes([0, self.0, self.1, self.2])
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Cursor {
    pub x: usize,
    pub y: usize,
    pub style: u8,
    pub color: Rgb,
    pub blinking: bool,
    pub visible: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) struct Cell {
    pub foreground: Rgb,
    pub background: Rgb,
    pub underline: Rgb,
    pub flags: u16,
    pub width: u8,
    pub has_underline: bool,
    pub selected: bool,
}

impl Cell {
    pub(crate) const BOLD: u16 = 1 << 0;
    pub(crate) const ITALIC: u16 = 1 << 1;
    pub(crate) const FAINT: u16 = 1 << 2;
    pub(crate) const INVERSE: u16 = 1 << 4;
    pub(crate) const INVISIBLE: u16 = 1 << 5;
    pub(crate) const STRIKE: u16 = 1 << 6;

    pub(crate) fn underline_style(self) -> u16 {
        (self.flags >> 8) & 7
    }
}

#[derive(Default)]
pub(crate) struct Frame {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Option<Cursor>,
    pub scroll_total: usize,
    pub scroll_offset: usize,
    pub scroll_visible: usize,
    pub rows: Vec<Vec<(Cell, String)>>,
    pub links: Vec<Link>,
}

impl Frame {
    pub(crate) fn scroll_maximum(&self) -> usize {
        self.scroll_total.saturating_sub(self.scroll_visible)
    }

    pub(crate) fn link_at(&self, column: u16, row: u16) -> Option<&Link> {
        let cols = self.rows.get(usize::from(row))?.len();
        self.links.iter().find(|link| {
            link.columns(usize::from(row), cols)
                .is_some_and(|columns| columns.contains(&usize::from(column)))
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) enum MouseAction {
    Press,
    Release,
    Move,
}

#[derive(Clone, Copy)]
pub(crate) enum SelectionAction {
    Press,
    Release,
    Drag,
    AutoScroll,
}

#[derive(Clone, Default)]
struct TerminalEvents(Rc<RefCell<Vec<Event>>>);

impl EventListener for TerminalEvents {
    fn send_event(&self, event: Event) {
        if matches!(
            event,
            Event::PtyWrite(_) | Event::ColorRequest(..) | Event::TextAreaSizeRequest(_)
        ) {
            self.0.borrow_mut().push(event);
        }
    }
}

struct GridSize(PtySize);

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        usize::from(self.0.rows).max(1)
    }

    fn columns(&self) -> usize {
        usize::from(self.0.cols).max(2)
    }
}

pub(crate) struct Emulator {
    term: Term<TerminalEvents>,
    parser: Processor,
    events: TerminalEvents,
    commands: Sender<Command>,
    size: PtySize,
    foreground: Rgb,
    background: Rgb,
    ansi: [Rgb; 16],
    selection_autoscroll: i32,
    last_mouse_cell: Option<(u16, u16)>,
}

impl Emulator {
    pub(crate) fn new(size: PtySize, commands: Sender<Command>) -> Self {
        let config = Config {
            scrolling_history: 5000,
            default_cursor_style: CursorStyle {
                shape: CursorShape::Block,
                blinking: true,
            },
            osc52: Osc52::Disabled,
            ..Config::default()
        };
        let events = TerminalEvents::default();
        Self {
            term: Term::new(config, &GridSize(size), events.clone()),
            parser: Processor::new(),
            events,
            commands,
            size,
            foreground: Rgb(0, 0, 0),
            background: Rgb(255, 255, 255),
            ansi: [Rgb::default(); 16],
            selection_autoscroll: 0,
            last_mouse_cell: None,
        }
    }

    pub(crate) fn resize(&mut self, size: PtySize, conpty: bool) {
        if conpty && size.rows > self.size.rows {
            let grid = self.term.grid();
            let pulled = usize::from(size.rows - self.size.rows).min(grid.history_size());
            if pulled > 0 {
                let offset = grid.display_offset();
                // ConPTY repaints from the top of the viewport. Keep rows that
                // Alacritty would pull into that viewport in scrollback instead.
                self.term.resize(GridSize(PtySize {
                    cols: self.size.cols,
                    ..size
                }));
                self.term.scroll_up(pulled);
                let grid = self.term.grid_mut();
                grid.cursor.point.line -= pulled;
                grid.saved_cursor.point.line -= pulled;
                grid.scroll_display(Scroll::Delta(offset as i32 - grid.display_offset() as i32));
            }
        }
        self.term.resize(GridSize(size));
        self.size = size;
        self.reply();
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        self.reply();
    }

    pub(crate) fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    pub(crate) fn finish_sync(&mut self) {
        if self.sync_deadline().is_some() {
            self.parser.stop_sync(&mut self.term);
            self.reply();
        }
    }

    pub(crate) fn theme(&mut self, fg: Rgb, bg: Rgb, ansi: &[Rgb; 16]) {
        self.foreground = fg;
        self.background = bg;
        self.ansi = *ansi;
    }

    fn reply(&self) {
        for event in self.events.0.borrow_mut().drain(..) {
            let text = match event {
                Event::PtyWrite(text) => text,
                Event::ColorRequest(index, reply) => {
                    let Rgb(r, g, b) = self.indexed_color(index, self.term.colors());
                    reply(VtRgb { r, g, b })
                }
                Event::TextAreaSizeRequest(reply) => reply(WindowSize {
                    num_lines: self.size.rows,
                    num_cols: self.size.cols,
                    cell_width: self.size.cell_width,
                    cell_height: self.size.cell_height,
                }),
                _ => continue,
            };
            self.send(text.into_bytes());
        }
    }

    fn send(&self, bytes: Vec<u8>) {
        _ = self.commands.send(Command::Input(bytes));
    }

    fn indexed_color(&self, index: usize, colors: &Colors) -> Rgb {
        if index < alacritty_terminal::term::color::COUNT
            && let Some(VtRgb { r, g, b }) = colors[index]
        {
            return Rgb(r, g, b);
        }
        match index {
            0..=15 => self.ansi[index],
            16..=231 => {
                let cube = index - 16;
                let level = |value: usize| {
                    if value == 0 {
                        0
                    } else {
                        (55 + 40 * value) as u8
                    }
                };
                Rgb(level(cube / 36), level(cube / 6 % 6), level(cube % 6))
            }
            232..=255 => {
                let gray = 8 + 10 * (index - 232) as u8;
                Rgb(gray, gray, gray)
            }
            257 => self.background,
            259..=266 => self.ansi[index - 259],
            _ => self.foreground,
        }
    }

    fn color(&self, color: Color, colors: &Colors) -> Rgb {
        match color {
            Color::Spec(VtRgb { r, g, b }) => Rgb(r, g, b),
            Color::Named(color) => self.indexed_color(color as usize, colors),
            Color::Indexed(index) => self.indexed_color(index as usize, colors),
        }
    }

    pub(crate) fn frame(&self, frame: &mut Frame) {
        let content = self.term.renderable_content();
        let colors = content.colors;
        let rows = self.term.screen_lines();
        let cols = self.term.columns();
        frame.scroll_visible = rows;
        frame.scroll_total = self.term.total_lines();
        frame.scroll_offset = frame
            .scroll_maximum()
            .saturating_sub(content.display_offset);
        frame.foreground = self.indexed_color(NamedColor::Foreground as usize, colors);
        frame.background = self.indexed_color(NamedColor::Background as usize, colors);
        let cursor_y = content.cursor.point.line.0 + content.display_offset as i32;
        // A TUI can hide the cursor while still positioning it for IME.
        frame.cursor = (0..rows as i32).contains(&cursor_y).then(|| Cursor {
            x: content.cursor.point.column.0,
            y: cursor_y as usize,
            style: match content.cursor.shape {
                CursorShape::Beam => 0,
                CursorShape::Underline => 2,
                CursorShape::HollowBlock => 3,
                _ => 1,
            },
            color: self.indexed_color(NamedColor::Cursor as usize, colors),
            blinking: self.term.cursor_style().blinking,
            visible: content.cursor.shape != CursorShape::Hidden,
        });
        frame.rows.resize_with(rows, Vec::new);
        for row in &mut frame.rows {
            row.resize_with(cols, Default::default);
        }
        for indexed in content.display_iter {
            let y = (indexed.point.line.0 + content.display_offset as i32) as usize;
            let (cell, text) = &mut frame.rows[y][indexed.point.column.0];
            let value = indexed.cell;
            let flags = value.flags;
            let mut attrs = 0;
            for (flag, attr) in [
                (Flags::BOLD, Cell::BOLD),
                (Flags::ITALIC, Cell::ITALIC),
                (Flags::DIM, Cell::FAINT),
                (Flags::INVERSE, Cell::INVERSE),
                (Flags::HIDDEN, Cell::INVISIBLE),
                (Flags::STRIKEOUT, Cell::STRIKE),
            ] {
                if flags.contains(flag) {
                    attrs |= attr;
                }
            }
            if flags.intersects(Flags::ALL_UNDERLINES) {
                attrs |= if flags.contains(Flags::UNDERCURL) {
                    3 << 8
                } else {
                    1 << 8
                };
            }
            let width = if flags.contains(Flags::WIDE_CHAR) {
                2
            } else if flags.contains(Flags::WIDE_CHAR_SPACER) {
                0
            } else {
                1
            };
            *cell = Cell {
                width,
                flags: attrs,
                foreground: self.color(value.fg, colors),
                background: self.color(value.bg, colors),
                underline: value
                    .underline_color()
                    .map(|color| self.color(color, colors))
                    .unwrap_or_default(),
                has_underline: value.underline_color().is_some(),
                selected: content.selection.is_some_and(|selection| {
                    selection.contains_cell(&indexed, content.cursor.point, content.cursor.shape)
                }),
            };
            text.clear();
            let marks = value.zerowidth().unwrap_or_default();
            if width != 0 && (value.c != ' ' || !marks.is_empty()) {
                text.push(value.c);
                text.extend(marks);
            }
        }
        self.links(&mut frame.links);
    }

    pub(crate) fn reports_mouse(&self) -> bool {
        self.term.mode().intersects(TermMode::MOUSE_MODE)
    }

    pub(crate) fn scroll(&mut self, rows: isize) {
        let delta = rows
            .saturating_neg()
            .clamp(i32::MIN as isize, i32::MAX as isize) as i32;
        self.term.scroll_display(Scroll::Delta(delta));
    }

    pub(crate) fn scroll_to(&mut self, row: usize) {
        let history = self.term.history_size();
        let offset = history.saturating_sub(row);
        self.term.scroll_display(Scroll::Delta(
            offset as i32 - self.term.grid().display_offset() as i32,
        ));
    }

    pub(crate) fn select(
        &mut self,
        action: SelectionAction,
        cell: (u16, u16),
        point: (f32, f32),
        click_count: usize,
    ) {
        let edge = f32::from(self.size.cell_width.max(1));
        let side = if point.0.rem_euclid(edge) < edge * 0.5 {
            Side::Left
        } else {
            Side::Right
        };
        let position = |term: &Term<_>| {
            Point::new(
                Line(i32::from(cell.1) - term.grid().display_offset() as i32),
                Column(usize::from(cell.0)),
            )
        };
        match action {
            SelectionAction::Press => {
                self.selection_autoscroll = 0;
                let kind = match click_count {
                    2 => SelectionType::Semantic,
                    3.. => SelectionType::Lines,
                    _ => SelectionType::Simple,
                };
                self.term.selection = Some(Selection::new(kind, position(&self.term), side));
            }
            SelectionAction::Release => self.selection_autoscroll = 0,
            SelectionAction::Drag | SelectionAction::AutoScroll => {
                if matches!(action, SelectionAction::AutoScroll) {
                    self.scroll(self.selection_autoscroll as isize);
                } else {
                    let height = f32::from(self.size.rows) * f32::from(self.size.cell_height);
                    self.selection_autoscroll = if point.1 < 0. {
                        -1
                    } else if point.1 >= height {
                        1
                    } else {
                        0
                    };
                }
                let position = position(&self.term);
                if let Some(selection) = &mut self.term.selection {
                    selection.update(position, side);
                }
            }
        }
    }

    pub(crate) fn selection_autoscroll(&self) -> bool {
        self.selection_autoscroll != 0
    }

    pub(crate) fn clear_selection(&mut self) {
        self.term.selection = None;
        self.selection_autoscroll = 0;
    }

    pub(crate) fn copy(&self) -> Option<String> {
        self.term.selection_to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn terminal(cols: u16, rows: u16) -> (Emulator, mpsc::Receiver<Command>) {
        let (tx, rx) = mpsc::channel();
        (
            Emulator::new(
                PtySize {
                    cols,
                    rows,
                    cell_width: 8,
                    cell_height: 16,
                },
                tx,
            ),
            rx,
        )
    }

    #[test]
    fn cjk_simple_emoji_and_combining_marks_keep_alacritty_cell_widths() {
        let (mut terminal, _) = terminal(8, 4);
        let mut frame = Frame::default();
        for sample in ["|AAAAAA|", "|中。文|", "|中，文|", "|（中）|", "|😀🚀😀|"] {
            terminal.write(b"\x1bc");
            for byte in sample.bytes() {
                terminal.write(&[byte]);
            }
            terminal.frame(&mut frame);
            assert_eq!(frame.rows[0][7].1, "|", "{sample}");
            assert_eq!(
                frame.rows[0]
                    .iter()
                    .map(|(_, text)| text.as_str())
                    .collect::<String>(),
                sample
            );
        }

        terminal.write("\x1bc\x1b[7G😀X".as_bytes());
        terminal.frame(&mut frame);
        assert_eq!(frame.rows[0][6].1, "😀");
        assert_eq!(frame.rows[0][6].0.width, 2);
        assert_eq!(frame.rows[0][7].0.width, 0);
        assert_eq!(frame.rows[1][0].1, "X");

        terminal.write("\x1bce\u{0301}👍🏽|".as_bytes());
        terminal.frame(&mut frame);
        assert_eq!(frame.rows[0][0].1, "e\u{0301}");
        assert_eq!(frame.rows[0][1].1, "👍");
        assert_eq!(frame.rows[0][3].1, "🏽");
        assert_eq!(frame.rows[0][5].1, "|");
    }

    #[test]
    fn synchronized_updates_and_terminal_replies() {
        let (mut terminal, commands) = terminal(16, 4);
        let mut frame = Frame::default();
        terminal.write(b"\x1b[?2026hA");
        terminal.frame(&mut frame);
        assert!(frame.rows[0][0].1.is_empty());
        assert!(terminal.sync_deadline().is_some());
        terminal.write(b"B\x1b[?2026l\x1b[6n");
        terminal.frame(&mut frame);
        assert_eq!(frame.rows[0][0].1, "A");
        assert_eq!(frame.rows[0][1].1, "B");
        assert!(matches!(commands.try_recv(), Ok(Command::Input(bytes)) if bytes == b"\x1b[1;3R"));

        terminal.write(b"\x1b[?2026hC");
        terminal.finish_sync();
        terminal.frame(&mut frame);
        assert_eq!(frame.rows[0][2].1, "C");
    }

    #[test]
    fn conpty_resize_preserves_history_across_viewport_repaints() {
        let (mut terminal, _) = terminal(80, 8);
        for number in 1..=80 {
            terminal.write(format!("rr-mark-{number:03}\r\n").as_bytes());
        }
        terminal.scroll(-6);
        let offset = terminal.term.grid().display_offset();

        for (cols, rows) in [(80, 38), (80, 8), (100, 38), (80, 8)] {
            terminal.resize(
                PtySize {
                    cols,
                    rows,
                    ..terminal.size
                },
                true,
            );
            assert_eq!(terminal.term.grid().display_offset(), offset);

            // ConPTY redraws the old viewport at the top and clears the new rows.
            terminal.write(b"\x1b[H");
            for number in 74..=80 {
                terminal.write(format!("rr-mark-{number:03}\x1b[K\r\n").as_bytes());
            }
            for _ in 8..rows {
                terminal.write(b"\x1b[K\r\n");
            }
            terminal.write(b"\x1b[K\x1b[8;1H");

            let grid = terminal.term.grid();
            let markers: Vec<_> = (grid.topmost_line().0..=grid.bottommost_line().0)
                .filter_map(|line| {
                    let text: String = (0..11).map(|col| grid[Line(line)][Column(col)].c).collect();
                    text.strip_prefix("rr-mark-")?.parse::<u32>().ok()
                })
                .collect();
            assert_eq!(markers, (1..=80).collect::<Vec<_>>());
        }
    }

    #[test]
    fn shell_keys_bracketed_paste_and_mouse_reporting() {
        let (mut terminal, commands) = terminal(16, 4);
        terminal.key("w", 2, "");
        terminal.key("c", 2, "");
        terminal.write(b"\x1b[?1h\x1b[?2004h\x1b[?1000h\x1b[?1006h");
        terminal.key("up", 0, "");
        terminal.paste("中文\x1b\r\n😀");
        assert!(terminal.mouse(MouseAction::Press, 1, 0, true, (8., 16.)));
        for expected in [
            &b"\x17"[..],
            b"\x03",
            b"\x1bOA",
            "\x1b[200~中文\n😀\x1b[201~".as_bytes(),
            b"\x1b[<0;2;2M",
        ] {
            assert!(matches!(commands.try_recv(), Ok(Command::Input(bytes)) if bytes == expected));
        }
    }

    #[test]
    fn web_links_keep_soft_wraps_scrollback_and_unicode_cell_positions() {
        let (mut terminal, _) = terminal(12, 3);
        let url = "https://example.test/中文?q=(x)";
        terminal.write(format!("({url}).\r\nend").as_bytes());
        let mut frame = Frame::default();
        terminal.frame(&mut frame);
        assert_eq!(frame.links.len(), 1);
        assert_eq!(frame.link_at(0, 0).unwrap().target.text(), url);
        assert_eq!(frame.link_at(10, 0), frame.link_at(11, 0));
        assert!(
            frame.link_at(8, 1).is_none(),
            "the closing punctuation is outside the link"
        );

        terminal.scroll(-1);
        terminal.frame(&mut frame);
        assert!(
            frame.link_at(0, 0).is_none(),
            "the opening parenthesis is outside the link"
        );
        assert_eq!(frame.link_at(1, 0).unwrap().target.text(), url);
        assert_eq!(frame.link_at(0, 2).unwrap().target.text(), url);

        terminal.write(b"\r\nhttps://\r\nexample.test\r\n");
        terminal.scroll_to(usize::MAX);
        terminal.frame(&mut frame);
        assert!(
            !frame
                .links
                .iter()
                .any(|link| matches!(link.target, LinkTarget::Web(_))),
            "hard line breaks cannot join URLs"
        );
    }

    #[test]
    fn osc8_targets_take_precedence_and_are_limited_to_web_urls() {
        let (mut terminal, _) = terminal(80, 5);
        terminal.write(
            concat!(
                "\x1b]8;;HTTPS://target.test/real\x1b\\https://label.test\x1b]8;;\x1b\\\r\n",
                "\x1b]8;;file:///var/log\x1b\\https://label.test\x1b]8;;\x1b\\\r\n",
                "\x1b]8;;https:///bad.test\x1b\\malformed\x1b]8;;\x1b\\\r\n",
                "\x1b]8;;http://[::1]:3000\x1b\\中\x1b]8;;\x1b\\ （https://a.test.）\r\n",
                "\x1b[8mhttps://hidden.test\x1b[0m",
            )
            .as_bytes(),
        );
        let mut frame = Frame::default();
        terminal.frame(&mut frame);
        assert_eq!(frame.links.len(), 3);
        assert_eq!(
            frame.link_at(0, 0).unwrap().target.text(),
            "HTTPS://target.test/real"
        );
        assert!(frame.link_at(0, 1).is_none());
        assert!(frame.link_at(0, 2).is_none());
        assert_eq!(
            frame.link_at(0, 3).unwrap().target.text(),
            "http://[::1]:3000"
        );
        assert_eq!(frame.link_at(0, 3), frame.link_at(1, 3));
        assert_eq!(frame.link_at(5, 3).unwrap().target.text(), "https://a.test");
        assert!(frame.link_at(0, 4).is_none());
    }

    #[test]
    fn file_links_keep_wrapped_cells_and_parse_positions() {
        let (mut wrapped, _) = terminal(13, 3);
        wrapped.write("(src/中文.rs:12:3):".as_bytes());
        let mut frame = Frame::default();
        wrapped.frame(&mut frame);
        assert_eq!(frame.links.len(), 1);
        assert_eq!(frame.link_at(5, 0), frame.link_at(6, 0));
        assert_eq!(frame.link_at(1, 0), frame.link_at(3, 1));
        assert!(frame.link_at(0, 0).is_none());
        assert!(frame.link_at(4, 1).is_none());
        assert_eq!(
            frame.links[0].target,
            LinkTarget::File {
                text: "src/中文.rs:12:3".into(),
                path_end: "src/中文.rs".len(),
                position: Some(lsp_types::Position::new(11, 2)),
            },
        );

        let (mut terminal, _) = terminal(100, 3);
        terminal.write(
            br"https://host/path/to/file:4 /tmp/file Makefile:5 C:\src\main.rs:7:8 12:34 foo.rs:0 foo.rs:3:0",
        );
        terminal.frame(&mut frame);
        assert_eq!(
            frame
                .links
                .iter()
                .map(|link| link.target.text())
                .collect::<Vec<_>>(),
            [
                "https://host/path/to/file:4",
                "/tmp/file",
                "Makefile:5",
                r"C:\src\main.rs:7:8"
            ],
        );
        assert!(matches!(
            frame.links[1].target,
            LinkTarget::File { position: None, .. }
        ));
        assert!(matches!(
            frame.links[2].target,
            LinkTarget::File {
                position: Some(lsp_types::Position {
                    line: 4,
                    character: 0
                }),
                ..
            }
        ));
    }
}
