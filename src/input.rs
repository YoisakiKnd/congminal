//! 键盘输入编码：把按键翻译成终端程序能理解的字节序列。
//!
//! 规则参考 xterm 的默认行为（PC 风格功能键）。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// 普通字符（平台层已经处理过键盘布局和 Shift）
    Char(char),
    Enter,
    Backspace,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// F1-F12
    F(u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// Windows 键 / Command 键
    pub logo: bool,
}

impl Mods {
    fn any(self) -> bool {
        self.shift || self.ctrl || self.alt
    }

    /// xterm 的修饰键参数：1 + Shift(1) + Alt(2) + Ctrl(4)
    fn param(self) -> u8 {
        1 + self.shift as u8 + 2 * self.alt as u8 + 4 * self.ctrl as u8
    }
}

pub fn encode(key: Key, mods: Mods, app_cursor: bool) -> Vec<u8> {
    let mut out = Vec::new();
    // Alt 组合键：在前面加一个 ESC（xterm 的 metaSendsEscape）
    let alt_prefix = |out: &mut Vec<u8>| {
        if mods.alt {
            out.push(0x1b);
        }
    };

    match key {
        Key::Char(c) => {
            alt_prefix(&mut out);
            match ctrl_byte(c) {
                Some(b) if mods.ctrl => out.push(b),
                _ => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
            }
        }
        Key::Enter => {
            alt_prefix(&mut out);
            out.push(b'\r');
        }
        Key::Backspace => {
            alt_prefix(&mut out);
            out.push(if mods.ctrl { 0x08 } else { 0x7f });
        }
        Key::Tab if mods.shift => out.extend_from_slice(b"\x1b[Z"),
        Key::Tab => {
            alt_prefix(&mut out);
            out.push(b'\t');
        }
        Key::Escape => {
            alt_prefix(&mut out);
            out.push(0x1b);
        }
        Key::Up => cursor_key(&mut out, b'A', mods, app_cursor),
        Key::Down => cursor_key(&mut out, b'B', mods, app_cursor),
        Key::Right => cursor_key(&mut out, b'C', mods, app_cursor),
        Key::Left => cursor_key(&mut out, b'D', mods, app_cursor),
        Key::Home => cursor_key(&mut out, b'H', mods, app_cursor),
        Key::End => cursor_key(&mut out, b'F', mods, app_cursor),
        Key::Insert => tilde_key(&mut out, 2, mods),
        Key::Delete => tilde_key(&mut out, 3, mods),
        Key::PageUp => tilde_key(&mut out, 5, mods),
        Key::PageDown => tilde_key(&mut out, 6, mods),
        // F1-F4 是 SS3 序列：ESC O P/Q/R/S
        Key::F(n @ 1..=4) => {
            let final_byte = b'P' + (n - 1);
            if mods.any() {
                out.extend_from_slice(format!("\x1b[1;{}{}", mods.param(), final_byte as char).as_bytes());
            } else {
                out.extend_from_slice(&[0x1b, b'O', final_byte]);
            }
        }
        // F5-F12 的编号不连续，是历史遗留
        Key::F(n @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize];
            tilde_key(&mut out, code, mods);
        }
        Key::F(_) => {}
    }
    out
}

/// 方向键、Home、End：`ESC [ A`，应用模式下是 `ESC O A`，有修饰键时是 `ESC [ 1 ; 5 A`
fn cursor_key(out: &mut Vec<u8>, final_byte: u8, mods: Mods, app_cursor: bool) {
    if mods.any() {
        out.extend_from_slice(format!("\x1b[1;{}{}", mods.param(), final_byte as char).as_bytes());
    } else if app_cursor {
        out.extend_from_slice(&[0x1b, b'O', final_byte]);
    } else {
        out.extend_from_slice(&[0x1b, b'[', final_byte]);
    }
}

/// 编辑键和 F5 以后的功能键：`ESC [ 编号 ~`，有修饰键时是 `ESC [ 编号 ; 修饰 ~`
fn tilde_key(out: &mut Vec<u8>, code: u8, mods: Mods) {
    if mods.any() {
        out.extend_from_slice(format!("\x1b[{code};{}~", mods.param()).as_bytes());
    } else {
        out.extend_from_slice(format!("\x1b[{code}~").as_bytes());
    }
}

/// Ctrl + 字符对应的控制字符。例如 Ctrl+C = 0x03，Ctrl+[ = ESC
fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        '@' | ' ' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' | '~' => 0x1e,
        '_' | '7' | '/' | '-' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods { shift: false, ctrl: false, alt: false, logo: false };
    const CTRL: Mods = Mods { ctrl: true, ..NONE };
    const ALT: Mods = Mods { alt: true, ..NONE };

    #[test]
    fn characters() {
        assert_eq!(encode(Key::Char('a'), NONE, false), b"a");
        assert_eq!(encode(Key::Char('中'), NONE, false), "中".as_bytes());
        assert_eq!(encode(Key::Char('c'), CTRL, false), b"\x03");
        assert_eq!(encode(Key::Char('x'), ALT, false), b"\x1bx");
        // 平台层已经给出控制字符时原样发送
        assert_eq!(encode(Key::Char('\x03'), CTRL, false), b"\x03");
    }

    #[test]
    fn cursor_keys() {
        assert_eq!(encode(Key::Up, NONE, false), b"\x1b[A");
        assert_eq!(encode(Key::Up, NONE, true), b"\x1bOA");
        assert_eq!(encode(Key::Left, CTRL, true), b"\x1b[1;5D");
    }

    #[test]
    fn function_keys() {
        assert_eq!(encode(Key::F(1), NONE, false), b"\x1bOP");
        assert_eq!(encode(Key::F(5), NONE, false), b"\x1b[15~");
        assert_eq!(encode(Key::F(12), ALT, false), b"\x1b[24;3~");
        assert_eq!(encode(Key::Delete, NONE, false), b"\x1b[3~");
    }

    #[test]
    fn editing_keys() {
        assert_eq!(encode(Key::Backspace, NONE, false), b"\x7f");
        assert_eq!(encode(Key::Enter, NONE, false), b"\r");
        assert_eq!(encode(Key::Tab, Mods { shift: true, ..NONE }, false), b"\x1b[Z");
    }
}
