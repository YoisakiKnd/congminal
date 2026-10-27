//! 键盘输入编码：把按键翻译成终端程序能理解的字节序列。
//!
//! 规则参考 xterm 的默认行为（PC 风格功能键）。

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
    /// 数字小键盘上的键。应用程序键区模式（DECKPAM）下改发 SS3 序列。
    Keypad(Keypad),
}

impl Key {
    /// 作为文字输入时的字符：主键盘字符，以及小键盘上的数字与运算符。
    ///
    /// 文本框（搜索框、SFTP 名称输入）用这个入口，小键盘敲出的数字才不会被丢掉。
    pub fn text(self) -> Option<char> {
        match self {
            Key::Char(c) => Some(c),
            Key::Keypad(k) => k.as_char(),
            _ => None,
        }
    }
}

/// 数字小键盘的键：主键盘上重复的字符在这里单独区分，
/// 因为应用程序键区模式要把它们换成另一套字节。
///
/// 三个平台的键位并不完全一致（macOS 的小键盘没有分隔符键），
/// 所以单个平台上总会有变体用不到，这里按平台差异放行。
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Keypad {
    Digit(u8),
    Decimal,
    Separator,
    Add,
    Subtract,
    Multiply,
    Divide,
    Equal,
    Enter,
}

impl Keypad {
    /// 主键盘上对应的字符；小键盘回车没有对应文字。
    pub fn as_char(self) -> Option<char> {
        Some(match self {
            Keypad::Digit(d) if d <= 9 => char::from(b'0' + d),
            Keypad::Digit(_) => return None,
            Keypad::Decimal => '.',
            Keypad::Separator => ',',
            Keypad::Add => '+',
            Keypad::Subtract => '-',
            Keypad::Multiply => '*',
            Keypad::Divide => '/',
            Keypad::Equal => '=',
            Keypad::Enter => return None,
        })
    }
}

/// 终端模式里影响按键编码的两项；和 term::Modes 分开，避免 input 依赖终端内核。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    /// DECCKM：方向键发 SS3 而不是 CSI
    pub app_cursor: bool,
    /// DECKPAM：小键盘发 SS3 而不是数字和运算符
    pub app_keypad: bool,
    /// Kitty 键盘协议的 flags：1 = 消除歧义转义码，8 = 所有按键都发转义序列。
    ///
    /// 程序用 `CSI > 1 u` 之类的序列压栈申请，终端退出时弹栈回到 0（传统编码）。
    pub kitty: u32,
}

/// Kitty 协议里我们实现的位：消除歧义、所有按键转义。
const KITTY_DISAMBIGUATE: u32 = 1;
const KITTY_REPORT_ALL: u32 = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
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

pub fn encode(key: Key, mods: Mods, flags: Flags) -> Vec<u8> {
    let app_cursor = flags.app_cursor;
    // Kitty 协议：程序要求全部转义，或者这个键在传统编码下有歧义时，改用 CSI u。
    let wants_kitty =
        flags.kitty & KITTY_REPORT_ALL != 0 || (flags.kitty & KITTY_DISAMBIGUATE != 0 && ambiguous(key, mods));
    if wants_kitty && let Some(bytes) = kitty(key, mods) {
        return bytes;
    }
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
        Key::Keypad(keypad) => keypad_key(&mut out, keypad, mods, flags.app_keypad),
        Key::F(_) => {}
    }
    out
}

/// 数字小键盘。应用键区模式下按 xterm 的 Application Keypad Mode 发 SS3；
/// 普通模式发这个键在主键盘上对应的字符。带修饰键时不进应用模式，
/// 免得把 Shift/Ctrl/Alt 的信息丢掉（xterm 在这些组合上同样退回普通映射）。
fn keypad_key(out: &mut Vec<u8>, keypad: Keypad, mods: Mods, app_keypad: bool) {
    if app_keypad && !mods.any() {
        let final_byte = match keypad {
            Keypad::Digit(0) => b'p',
            Keypad::Digit(1) => b'q',
            Keypad::Digit(2) => b'r',
            Keypad::Digit(3) => b's',
            Keypad::Digit(4) => b't',
            Keypad::Digit(5) => b'u',
            Keypad::Digit(6) => b'v',
            Keypad::Digit(7) => b'w',
            Keypad::Digit(8) => b'x',
            Keypad::Digit(_) => b'y',
            Keypad::Multiply => b'j',
            Keypad::Add => b'k',
            Keypad::Separator => b'l',
            Keypad::Subtract => b'm',
            Keypad::Decimal => b'n',
            Keypad::Divide => b'o',
            Keypad::Equal => b'X',
            Keypad::Enter => b'M',
        };
        out.extend_from_slice(&[0x1b, b'O', final_byte]);
        return;
    }
    if mods.alt {
        out.push(0x1b);
    }
    out.push(match keypad {
        Keypad::Digit(digit) => b'0' + digit,
        Keypad::Decimal => b'.',
        Keypad::Separator => b',',
        Keypad::Add => b'+',
        Keypad::Subtract => b'-',
        Keypad::Multiply => b'*',
        Keypad::Divide => b'/',
        Keypad::Equal => b'=',
        Keypad::Enter => b'\r',
    });
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

/// 传统编码下这个按键会丢掉信息的组合：单独 ESC 和它做前缀分不清，
/// 带修饰的 Enter/Tab/Backspace 会被压成同一个控制字节，带修饰的字符拿不到修饰位。
fn ambiguous(key: Key, mods: Mods) -> bool {
    match key {
        Key::Escape => true,
        Key::Enter | Key::Tab | Key::Backspace => mods.shift || mods.ctrl || mods.alt || mods.logo,
        Key::Char(_) | Key::Keypad(_) => mods.ctrl || mods.alt || mods.logo,
        _ => false,
    }
}

/// Kitty 的 `CSI key;mods u` 形式。返回 None 表示这个键仍然用传统序列（方向键、功能键）。
fn kitty(key: Key, mods: Mods) -> Option<Vec<u8>> {
    let code = match key {
        Key::Char(c) => c as u32,
        Key::Keypad(k) => match k {
            Keypad::Enter => 13,
            k => k.as_char()? as u32,
        },
        Key::Enter => 13,
        Key::Tab => 9,
        Key::Backspace => 127,
        Key::Escape => 27,
        _ => return None,
    };
    let modifiers = 1 + mods.shift as u32 + 2 * mods.alt as u32 + 4 * mods.ctrl as u32 + 8 * mods.logo as u32;
    let mut out = Vec::new();
    if modifiers == 1 {
        out.extend_from_slice(format!("\x1b[{code}u").as_bytes());
    } else {
        out.extend_from_slice(format!("\x1b[{code};{modifiers}u").as_bytes());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods { shift: false, ctrl: false, alt: false, logo: false };
    const CTRL: Mods = Mods { ctrl: true, ..NONE };
    const ALT: Mods = Mods { alt: true, ..NONE };

    #[test]
    fn characters() {
        assert_eq!(encode(Key::Char('a'), NONE, Flags::default()), b"a");
        assert_eq!(encode(Key::Char('中'), NONE, Flags::default()), "中".as_bytes());
        assert_eq!(encode(Key::Char('c'), CTRL, Flags::default()), b"\x03");
        assert_eq!(encode(Key::Char('x'), ALT, Flags::default()), b"\x1bx");
        // 平台层已经给出控制字符时原样发送
        assert_eq!(encode(Key::Char('\x03'), CTRL, Flags::default()), b"\x03");
    }

    #[test]
    fn cursor_keys() {
        assert_eq!(encode(Key::Up, NONE, Flags::default()), b"\x1b[A");
        assert_eq!(encode(Key::Up, NONE, Flags { app_cursor: true, ..Flags::default() }), b"\x1bOA");
        assert_eq!(encode(Key::Left, CTRL, Flags { app_cursor: true, ..Flags::default() }), b"\x1b[1;5D");
    }

    #[test]
    fn function_keys() {
        assert_eq!(encode(Key::F(1), NONE, Flags::default()), b"\x1bOP");
        assert_eq!(encode(Key::F(5), NONE, Flags::default()), b"\x1b[15~");
        assert_eq!(encode(Key::F(12), ALT, Flags::default()), b"\x1b[24;3~");
        assert_eq!(encode(Key::Delete, NONE, Flags::default()), b"\x1b[3~");
    }

    #[test]
    fn kitty_flags_encode_ambiguous_keys() {
        let none = Mods::default();
        let ctrl = Mods { ctrl: true, ..Mods::default() };
        let shift_enter = Mods { shift: true, ..Mods::default() };

        // 没有申请就保持传统编码
        assert_eq!(encode(Key::Escape, none, Flags::default()), b"\x1b");
        assert_eq!(encode(Key::Enter, shift_enter, Flags::default()), b"\r");
        assert_eq!(encode(Key::Char('a'), ctrl, Flags::default()), b"\x01");

        // 消除歧义：只有传统编码会丢信息的组合改用 CSI u
        let disambiguate = Flags { kitty: 1, ..Flags::default() };
        assert_eq!(encode(Key::Escape, none, disambiguate), b"\x1b[27u");
        assert_eq!(encode(Key::Enter, shift_enter, disambiguate), b"\x1b[13;2u");
        assert_eq!(encode(Key::Tab, shift_enter, disambiguate), b"\x1b[9;2u");
        assert_eq!(encode(Key::Backspace, ctrl, disambiguate), b"\x1b[127;5u");
        assert_eq!(encode(Key::Char('a'), ctrl, disambiguate), b"\x1b[97;5u");
        // 单纯 Shift 打出来的字符本来就没歧义，仍然直接发文字
        assert_eq!(encode(Key::Char('A'), shift_enter, disambiguate), b"A");
        // 方向键、功能键的 CSI 1;mods 形式已经带修饰位，不重复改造
        assert_eq!(encode(Key::Up, ctrl, disambiguate), b"\x1b[1;5A");

        // 全部按键转义：连普通字符和小键盘也走 CSI u
        let all = Flags { kitty: 8, ..Flags::default() };
        assert_eq!(encode(Key::Char('a'), none, all), b"\x1b[97u");
        assert_eq!(encode(Key::Keypad(Keypad::Digit(7)), none, all), b"\x1b[55u");
        assert_eq!(encode(Key::Keypad(Keypad::Enter), none, all), b"\x1b[13u");
        // Alt 的语义进修饰位，不再加 ESC 前缀
        let alt = Mods { alt: true, ..Mods::default() };
        assert_eq!(encode(Key::Char('b'), alt, all), b"\x1b[98;3u");
        // 没实现的键仍然用传统序列
        assert_eq!(encode(Key::F(1), none, all), b"\x1bOP");
    }

    #[test]
    fn keypad_follows_application_keypad_mode() {
        let app = Flags { app_keypad: true, ..Flags::default() };
        // 应用键区模式：xterm 的 SS3 序列，程序能区分小键盘和主键盘
        assert_eq!(encode(Key::Keypad(Keypad::Digit(1)), NONE, app), b"\x1bOq");
        assert_eq!(encode(Key::Keypad(Keypad::Digit(0)), NONE, app), b"\x1bOp");
        assert_eq!(encode(Key::Keypad(Keypad::Digit(9)), NONE, app), b"\x1bOy");
        assert_eq!(encode(Key::Keypad(Keypad::Enter), NONE, app), b"\x1bOM");
        assert_eq!(encode(Key::Keypad(Keypad::Decimal), NONE, app), b"\x1bOn");
        assert_eq!(encode(Key::Keypad(Keypad::Divide), NONE, app), b"\x1bOo");
        assert_eq!(encode(Key::Keypad(Keypad::Multiply), NONE, app), b"\x1bOj");
        assert_eq!(encode(Key::Keypad(Keypad::Add), NONE, app), b"\x1bOk");
        assert_eq!(encode(Key::Keypad(Keypad::Subtract), NONE, app), b"\x1bOm");
        assert_eq!(encode(Key::Keypad(Keypad::Separator), NONE, app), b"\x1bOl");
        assert_eq!(encode(Key::Keypad(Keypad::Equal), NONE, app), b"\x1bOX");
        // 普通模式：等同主键盘上的字符
        assert_eq!(encode(Key::Keypad(Keypad::Digit(7)), NONE, Flags::default()), b"7");
        assert_eq!(encode(Key::Keypad(Keypad::Decimal), NONE, Flags::default()), b".");
        assert_eq!(encode(Key::Keypad(Keypad::Enter), NONE, Flags::default()), b"\r");
        assert_eq!(encode(Key::Keypad(Keypad::Add), NONE, Flags::default()), b"+");
        // 带修饰键时不进应用模式，Alt 前缀照旧
        assert_eq!(encode(Key::Keypad(Keypad::Digit(1)), ALT, app), b"\x1b1");
        assert_eq!(encode(Key::Keypad(Keypad::Digit(1)), Mods { shift: true, ..NONE }, app), b"1");
        // 应用模式不影响主键盘数字
        assert_eq!(encode(Key::Char('1'), NONE, app), b"1");
    }

    #[test]
    fn text_input_takes_keypad_characters() {
        // 文本框用 Key::text()：小键盘的数字与运算符要能敲进输入框。
        assert_eq!(Key::Keypad(Keypad::Digit(3)).text(), Some('3'));
        assert_eq!(Key::Keypad(Keypad::Digit(0)).text(), Some('0'));
        assert_eq!(Key::Keypad(Keypad::Decimal).text(), Some('.'));
        assert_eq!(Key::Keypad(Keypad::Separator).text(), Some(','));
        assert_eq!(Key::Keypad(Keypad::Add).text(), Some('+'));
        assert_eq!(Key::Keypad(Keypad::Subtract).text(), Some('-'));
        assert_eq!(Key::Keypad(Keypad::Multiply).text(), Some('*'));
        assert_eq!(Key::Keypad(Keypad::Divide).text(), Some('/'));
        assert_eq!(Key::Keypad(Keypad::Equal).text(), Some('='));
        assert_eq!(Key::Keypad(Keypad::Enter).text(), None, "小键盘回车是提交，不是文字");
        assert_eq!(Key::Keypad(Keypad::Digit(10)).text(), None, "越界数字不产生字符");
        assert_eq!(Key::Char('名').text(), Some('名'));
        assert_eq!(Key::Enter.text(), None);
        assert_eq!(Key::Up.text(), None);
        assert_eq!(Key::F(1).text(), None);
    }

    #[test]
    fn editing_keys() {
        assert_eq!(encode(Key::Backspace, NONE, Flags::default()), b"\x7f");
        assert_eq!(encode(Key::Enter, NONE, Flags::default()), b"\r");
        assert_eq!(encode(Key::Tab, Mods { shift: true, ..NONE }, Flags::default()), b"\x1b[Z");
    }
}
