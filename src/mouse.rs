//! 鼠标上报：程序（vim、tmux、htop……）开启鼠标模式后，把点击、拖动、滚轮编码成转义序列发给它。
//!
//! 协议来自 xterm：<https://invisible-island.net/xterm/ctlseqs/ctlseqs.html#h2-Mouse-Tracking>

use crate::input::Mods;
use crate::platform::MouseButton;
use crate::term::{MouseEncoding, MouseMode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Press(MouseButton),
    Release(MouseButton),
    /// 移动；按住了哪个键（没按就是 None）
    Move(Option<MouseButton>),
    WheelUp,
    WheelDown,
}

/// 编码一次鼠标操作。`col`、`row` 从 0 开始。这个模式不需要上报这个操作、或者坐标没法表示时返回 None。
pub fn encode(
    action: Action,
    col: usize,
    row: usize,
    mods: Mods,
    mode: MouseMode,
    encoding: MouseEncoding,
) -> Option<Vec<u8>> {
    // 这个模式不需要上报的操作
    let unwanted = matches!(
        (mode, action),
        (MouseMode::Off, _)
            | (MouseMode::Press, Action::Release(_) | Action::Move(_))
            | (MouseMode::Click, Action::Move(_))
            | (MouseMode::Drag, Action::Move(None))
    );
    if unwanted {
        return None;
    }

    let button = |b: MouseButton| match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let mut code = match action {
        Action::Press(b) => button(b),
        // SGR 格式在结尾用 m 区分抬起，可以带上是哪个键；传统格式只能说"抬起了"（3）
        Action::Release(b) if encoding == MouseEncoding::Sgr => button(b),
        Action::Release(_) => 3,
        Action::Move(b) => 32 + b.map_or(3, button),
        Action::WheelUp => 64,
        Action::WheelDown => 65,
    };
    // 最早的 X10 模式不带修饰键
    if mode != MouseMode::Press {
        code += 4 * mods.shift as u32 + 8 * mods.alt as u32 + 16 * mods.ctrl as u32;
    }
    let (x, y) = (col as u32 + 1, row as u32 + 1);

    match encoding {
        MouseEncoding::Sgr => {
            let end = if matches!(action, Action::Release(_)) { 'm' } else { 'M' };
            Some(format!("\x1b[<{code};{x};{y}{end}").into_bytes())
        }
        MouseEncoding::Default => {
            // 每个值加 32 后占一个字节
            if x > 223 || y > 223 {
                return None;
            }
            Some(vec![0x1b, b'[', b'M', (32 + code) as u8, (32 + x) as u8, (32 + y) as u8])
        }
        MouseEncoding::Utf8 => {
            // 坐标加 32 后按 UTF-8 编码，最大能到 2015
            let mut out = vec![0x1b, b'[', b'M', (32 + code) as u8];
            for value in [x, y] {
                let c = char::from_u32(32 + value).filter(|_| value <= 2015)?;
                out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
            }
            Some(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods { shift: false, ctrl: false, alt: false, logo: false };
    const LEFT: MouseButton = MouseButton::Left;

    fn sgr(action: Action, col: usize, row: usize, mods: Mods, mode: MouseMode) -> Option<String> {
        encode(action, col, row, mods, mode, MouseEncoding::Sgr).map(|b| String::from_utf8(b).unwrap())
    }

    #[test]
    fn sgr_encoding() {
        assert_eq!(sgr(Action::Press(LEFT), 0, 0, NONE, MouseMode::Click).unwrap(), "\x1b[<0;1;1M");
        assert_eq!(sgr(Action::Release(LEFT), 9, 4, NONE, MouseMode::Click).unwrap(), "\x1b[<0;10;5m");
        assert_eq!(sgr(Action::Press(MouseButton::Right), 0, 0, NONE, MouseMode::Click).unwrap(), "\x1b[<2;1;1M");
        assert_eq!(sgr(Action::WheelUp, 2, 3, NONE, MouseMode::Click).unwrap(), "\x1b[<64;3;4M");
        let ctrl = Mods { ctrl: true, ..NONE };
        assert_eq!(sgr(Action::Press(LEFT), 0, 0, ctrl, MouseMode::Click).unwrap(), "\x1b[<16;1;1M");
        // 大坐标没问题
        assert_eq!(sgr(Action::Press(LEFT), 499, 0, NONE, MouseMode::Click).unwrap(), "\x1b[<0;500;1M");
    }

    #[test]
    fn modes_filter_actions() {
        assert!(sgr(Action::Move(Some(LEFT)), 0, 0, NONE, MouseMode::Click).is_none());
        assert_eq!(sgr(Action::Move(Some(LEFT)), 1, 1, NONE, MouseMode::Drag).unwrap(), "\x1b[<32;2;2M");
        assert!(sgr(Action::Move(None), 0, 0, NONE, MouseMode::Drag).is_none());
        assert_eq!(sgr(Action::Move(None), 0, 0, NONE, MouseMode::Motion).unwrap(), "\x1b[<35;1;1M");
        assert!(sgr(Action::Release(LEFT), 0, 0, NONE, MouseMode::Press).is_none());
        assert!(sgr(Action::Press(LEFT), 0, 0, NONE, MouseMode::Off).is_none());
    }

    #[test]
    fn default_encoding() {
        let bytes = encode(Action::Press(LEFT), 0, 0, NONE, MouseMode::Click, MouseEncoding::Default).unwrap();
        assert_eq!(bytes, b"\x1b[M !!");
        // 传统格式的抬起不知道是哪个键
        let bytes = encode(Action::Release(LEFT), 0, 0, NONE, MouseMode::Click, MouseEncoding::Default).unwrap();
        assert_eq!(bytes, b"\x1b[M#!!");
        // 超过 223 列表示不了
        assert!(encode(Action::Press(LEFT), 300, 0, NONE, MouseMode::Click, MouseEncoding::Default).is_none());
    }

    #[test]
    fn utf8_encoding() {
        let bytes = encode(Action::Press(LEFT), 299, 0, NONE, MouseMode::Click, MouseEncoding::Utf8).unwrap();
        let mut expected = b"\x1b[M ".to_vec();
        expected.extend("\u{14C}!".as_bytes()); // 32 + 300 = 332 = U+014C
        assert_eq!(bytes, expected);
    }
}
