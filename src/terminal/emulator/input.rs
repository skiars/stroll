use super::{Emulator, MouseAction, TermMode};

const SHIFT: u16 = 1;
const CTRL: u16 = 2;
const ALT: u16 = 4;
const SUPER: u16 = 8;

impl Emulator {
    pub(crate) fn key(&self, key: &str, mods: u16, text: &str) {
        if let Some(bytes) = key_bytes(key, mods, text, *self.term.mode()) {
            self.send(bytes);
        }
    }

    pub(crate) fn paste(&self, text: &str) {
        let bracketed = self.term.mode().contains(TermMode::BRACKETED_PASTE);
        let text = text.replace("\r\n", "\n");
        let mut bytes = Vec::with_capacity(text.len() + 12);
        if bracketed {
            bytes.extend(b"\x1b[200~");
            bytes.extend(text.bytes().filter(|&byte| byte != 0x1b));
            bytes.extend(b"\x1b[201~");
        } else {
            bytes.extend(
                text.bytes()
                    .map(|byte| if byte == b'\n' { b'\r' } else { byte }),
            );
        }
        self.send(bytes);
    }

    pub(crate) fn focus(&self, focused: bool) {
        if self.term.mode().contains(TermMode::FOCUS_IN_OUT) {
            self.send(if focused { b"\x1b[I" } else { b"\x1b[O" }.to_vec());
        }
    }

    pub(crate) fn mouse(
        &mut self,
        action: MouseAction,
        button: u8,
        mods: u16,
        pressed: bool,
        position: (f32, f32),
    ) -> bool {
        let mode = *self.term.mode();
        if !self.reports_mouse() {
            return false;
        }
        let (x, y) = position;
        let col = (x / f32::from(self.size.cell_width.max(1))).floor() as u16 + 1;
        let row = (y / f32::from(self.size.cell_height.max(1))).floor() as u16 + 1;
        let cell = (col.min(self.size.cols), row.min(self.size.rows));
        let release = matches!(action, MouseAction::Release);
        let motion = matches!(action, MouseAction::Move);
        if motion
            && ((!mode.contains(TermMode::MOUSE_MOTION)
                && !(pressed && mode.contains(TermMode::MOUSE_DRAG)))
                || self.last_mouse_cell == Some(cell))
        {
            return true;
        }
        self.last_mouse_cell = Some(cell);
        let button = match button {
            1 => 0,
            2 => 2,
            3 => 1,
            4 => 64,
            5 => 65,
            _ if motion => 3,
            _ => return true,
        };
        let modifiers = (if mods & SHIFT != 0 { 4 } else { 0 })
            | (if mods & ALT != 0 { 8 } else { 0 })
            | (if mods & CTRL != 0 { 16 } else { 0 });
        let mut code = button | modifiers | if motion { 32 } else { 0 };
        if mode.contains(TermMode::SGR_MOUSE) {
            let suffix = if release { 'm' } else { 'M' };
            self.send(format!("\x1b[<{code};{};{}{suffix}", cell.0, cell.1).into_bytes());
        } else {
            if release {
                code = 3 | modifiers;
            }
            let utf8 = mode.contains(TermMode::UTF8_MOUSE);
            let limit = if utf8 { 2015 } else { 223 };
            if cell.0 <= limit && cell.1 <= limit {
                let mut bytes = vec![0x1b, b'[', b'M', 32 + code];
                for value in [cell.0, cell.1] {
                    if utf8 {
                        let mut buffer = [0; 4];
                        bytes.extend(
                            char::from_u32(32 + u32::from(value))
                                .unwrap()
                                .encode_utf8(&mut buffer)
                                .as_bytes(),
                        );
                    } else {
                        bytes.push(32 + value as u8);
                    }
                }
                self.send(bytes);
            }
        }
        true
    }

    pub(crate) fn alternate_scroll(&self, rows: isize) -> bool {
        if !self
            .term
            .mode()
            .contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
        {
            return false;
        }
        let key = if rows < 0 { "up" } else { "down" };
        if let Some(bytes) = key_bytes(key, 0, "", *self.term.mode()) {
            self.send(bytes.repeat(rows.unsigned_abs().min(30)));
        }
        true
    }
}

fn key_bytes(key: &str, mods: u16, text: &str, mode: TermMode) -> Option<Vec<u8>> {
    let alt = mods & ALT != 0;
    let ctrl = mods & CTRL != 0;
    if mods & SUPER != 0 {
        return None;
    }
    let modifier = 1 + (mods & SHIFT) + u16::from(alt) * 2 + u16::from(ctrl) * 4;
    let cursor = match key {
        "up" => Some('A'),
        "down" => Some('B'),
        "right" => Some('C'),
        "left" => Some('D'),
        "home" => Some('H'),
        "end" => Some('F'),
        _ => None,
    };
    if let Some(suffix) = cursor {
        return Some(
            if modifier > 1 {
                format!("\x1b[1;{modifier}{suffix}")
            } else if mode.contains(TermMode::APP_CURSOR) {
                format!("\x1bO{suffix}")
            } else {
                format!("\x1b[{suffix}")
            }
            .into_bytes(),
        );
    }
    let function = match key {
        "f1" => Some('P'),
        "f2" => Some('Q'),
        "f3" => Some('R'),
        "f4" => Some('S'),
        _ => None,
    };
    if let Some(suffix) = function {
        return Some(
            if modifier > 1 {
                format!("\x1b[1;{modifier}{suffix}")
            } else {
                format!("\x1bO{suffix}")
            }
            .into_bytes(),
        );
    }
    let number = match key {
        "insert" => 2,
        "delete" => 3,
        "pageup" => 5,
        "pagedown" => 6,
        "f5" => 15,
        "f6" => 17,
        "f7" => 18,
        "f8" => 19,
        "f9" => 20,
        "f10" => 21,
        "f11" => 23,
        "f12" => 24,
        _ => 0,
    };
    if number != 0 {
        return Some(
            if modifier > 1 {
                format!("\x1b[{number};{modifier}~")
            } else {
                format!("\x1b[{number}~")
            }
            .into_bytes(),
        );
    }
    if key == "tab" && mods & SHIFT != 0 {
        return Some(b"\x1b[Z".to_vec());
    }
    let byte = match key {
        "enter" => Some(b'\r'),
        "tab" => Some(b'\t'),
        "escape" => Some(0x1b),
        "backspace" => Some(if ctrl { 0x08 } else { 0x7f }),
        "space" if ctrl => Some(0),
        _ if ctrl => {
            let character = if text.len() == 1 { text } else { key };
            match character.as_bytes() {
                [b'@' | b' ' | b'2'] => Some(0),
                [c @ (b'a'..=b'z' | b'A'..=b'Z' | b'['..=b'_')] => Some(c & 0x1f),
                [b'3'] => Some(0x1b),
                [b'4'] => Some(0x1c),
                [b'5'] => Some(0x1d),
                [b'6'] => Some(0x1e),
                [b'7' | b'/'] => Some(0x1f),
                [b'8' | b'?'] => Some(0x7f),
                _ => None,
            }
        }
        _ => None,
    };
    let mut bytes = Vec::new();
    if alt {
        bytes.push(0x1b);
    }
    if let Some(byte) = byte {
        bytes.push(byte);
    } else if !ctrl && !text.is_empty() {
        bytes.extend(text.as_bytes());
    } else if !ctrl && key.len() == 1 {
        bytes.extend(key.as_bytes());
    } else {
        return None;
    }
    Some(bytes)
}
