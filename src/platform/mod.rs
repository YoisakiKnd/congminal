//! 平台层：窗口、事件循环、把像素贴到屏幕上。
//!
//! 每个平台各实现一份同样接口的 `Window` 和 `Waker`：
//! - `Window::new(title, size_for_scale)`：创建窗口。`size_for_scale` 根据屏幕缩放比例算出初始像素尺寸
//! - `Window::scale_factor()`：屏幕缩放比例（高分屏是 2.0）
//! - `Window::waker()`：可以跨线程使用的唤醒器，PTY 读线程用它叫醒事件循环
//! - `Window::run(app)`：进入事件循环，直到 `App::event` 返回 false
//!
//! 平台层只认识像素和按键，完全不知道终端的存在。

mod clipboard;
pub(crate) mod protected_input;
pub(crate) use clipboard::{CopyError, CopyText, PasteRequest, ProtectedPaste};
pub(crate) use clipboard::{PlainPaste, paste_bytes};
pub(crate) use protected_input::{InputContext, ProtectedInput};

use crate::config::OptionAsAlt;
use crate::input::{Key, Mods};
use crate::render::{Damage, Framebuffer};
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

/// 离截止时间还有多少毫秒（向上取整，免得提前醒来白转一圈）。没有截止时间返回 None。
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn timeout_ms(deadline: Option<Instant>) -> Option<u32> {
    let remaining = deadline?.saturating_duration_since(Instant::now());
    Some(remaining.as_micros().div_ceil(1000).min(u32::MAX as u128) as u32)
}

pub const CLIPBOARD_LIMIT: usize = crate::pty::flow::INPUT_LIMIT - 12;
#[derive(Clone, Copy, Debug)]
pub struct ClipboardRead {
    pub limit: usize,
    /// UTF-16 是否拒绝孤立代理；UTF-8 在所有模式都严格验证。
    #[cfg_attr(not(any(windows, test)), allow(dead_code))]
    pub strict_unicode: bool,
}
impl Default for ClipboardRead {
    fn default() -> Self {
        Self { limit: CLIPBOARD_LIMIT, strict_unicode: false }
    }
}
impl ClipboardRead {
    pub fn limit(self) -> usize {
        self.limit.min(CLIPBOARD_LIMIT)
    }
    pub(crate) fn utf8(self, input: &[u8]) -> Option<String> {
        if input.len() > self.limit() {
            return None;
        }
        std::str::from_utf8(input).ok().map(str::to_owned)
    }
    #[cfg(any(windows, test))]
    fn utf16_length(self, input: &[u16]) -> Option<usize> {
        if input.len() > self.limit() {
            return None;
        }
        char::decode_utf16(input.iter().copied()).try_fold(0usize, |sum, c| {
            let c = match c {
                Ok(c) => c,
                Err(_) if !self.strict_unicode => char::REPLACEMENT_CHARACTER,
                Err(_) => return None,
            };
            sum.checked_add(c.len_utf8()).filter(|n| *n <= self.limit())
        })
    }
    #[cfg(test)]
    fn utf16(self, input: &[u16]) -> Option<String> {
        let length = self.utf16_length(input)?;
        self.utf16_reserved(input, length)
    }
    #[cfg(any(windows, test))]
    fn utf16_reserved(self, input: &[u16], length: usize) -> Option<String> {
        if length > self.limit() {
            return None;
        }
        let mut text = String::new();
        text.try_reserve_exact(length).ok()?;
        for decoded in char::decode_utf16(input.iter().copied()) {
            let next = decoded
                .or_else(|e| if self.strict_unicode { Err(e) } else { Ok(char::REPLACEMENT_CHARACTER) })
                .ok()
                .filter(|c| text.len() + c.len_utf8() <= length);
            let Some(c) = next else {
                // 若外部缓冲在两遍读取间变化，拒绝且不让 String 自动扩容。
                for byte in unsafe { text.as_bytes_mut() } {
                    unsafe {
                        std::ptr::write_volatile(byte, 0);
                    }
                }
                return None;
            };
            text.push(c);
        }
        Some(text)
    }
}
pub enum Event {
    Key {
        key: Key,
        mods: Mods,
    },
    /// 输入法确认上屏的文字
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Text(String),
    /// 输入法正在输入、还没上屏的文字（比如拼音）。空字符串表示输入结束。
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Preedit(String),
    /// 窗口内容区域的新尺寸（物理像素）
    Resize {
        width: u32,
        height: u32,
    },
    /// 鼠标按键按下。坐标是窗口内容区的物理像素，原点在左上角。
    MouseDown {
        button: MouseButton,
        x: f64,
        y: f64,
        mods: Mods,
    },
    MouseUp {
        button: MouseButton,
        x: f64,
        y: f64,
        mods: Mods,
    },
    /// 鼠标移动（不管有没有按键）
    MouseMove {
        x: f64,
        y: f64,
        mods: Mods,
    },
    /// 滚轮，正数表示往上（往回看历史）
    Scroll {
        delta: ScrollDelta,
        x: f64,
        y: f64,
        mods: Mods,
    },
    /// 窗口获得（true）或失去（false）键盘焦点
    Focus(bool),
    /// 用户要求复制（macOS 的 Cmd+C 菜单）
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Copy,
    /// 用户要求粘贴（macOS 的 Cmd+V 菜单）。App 会通过 take_clipboard_request 要剪贴板内容。
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    PasteRequest,
    /// 剪贴板里的文字（回应 ClipboardRequest::Paste）
    #[cfg_attr(not(test), allow(dead_code))] // 兼容未归属输入的拒绝/隔离测试；生产读取使用 ManagedPaste。
    Paste(String),
    ManagedPaste(PlainPaste),
    /// 仅交给原始受保护视图；普通终端丢弃并由拥有者清理副本。
    ProtectedPaste(ProtectedPaste),
    /// 剪贴板无文本或读取失败，结束本次异步请求。
    PasteFailed,
    /// 写入剪贴板失败；额度拒绝发生在修改系统剪贴板之前。
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    CopyFailed,
    /// 窗口需要重画（比如被遮挡后重新露出来）。macOS 的图层会保留内容，用不到它。
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Redraw,
    /// 其他线程调用了 `Waker::wake`
    Wake,
    CloseRequested,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug)]
pub enum ScrollDelta {
    /// 鼠标滚轮的格数
    Lines(f64),
    /// 触控板的精确滚动（物理像素）
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Pixels(f64),
}

/// App 请平台层读写系统剪贴板
pub enum ClipboardRequest {
    Copy(CopyText),
    /// 读出剪贴板内容，用 Event::Paste 送回来
    Paste(PasteRequest),
}

pub trait App {
    /// 处理一个事件。返回 false 表示退出。
    fn event(&mut self, event: Event) -> bool;
    /// 受保护输入有独立拥有者；默认丢弃并清理，不能回落为 Text/Preedit。
    #[cfg_attr(not(test), allow(dead_code))]
    fn protected_input(&mut self, input: ProtectedInput) -> bool {
        drop(input);
        true
    }
    /// 平台只在原上下文构建 client；None 表示当前没有受保护输入接收器。
    #[cfg_attr(not(test), allow(dead_code))]
    fn protected_context(&self) -> Option<InputContext> {
        None
    }
    /// 每轮原生事件处理后执行一次有预算的后台工作，返回 false 表示退出。
    fn tick(&mut self) -> bool;
    fn needs_redraw(&self) -> bool;
    /// 画一帧，返回画面和变化了的区域；什么都没变时返回 None（不用提交）
    fn draw(&mut self) -> Option<(&Framebuffer, Damage)>;
    /// 如果窗口标题需要改，返回新标题
    fn take_title(&mut self) -> Option<String>;
    /// 最晚什么时候要被叫醒（比如推迟到下一帧的重画）。None 表示没事就一直等到有事件。
    fn deadline(&self) -> Option<Instant>;
    /// 需要读写剪贴板时返回请求，平台层处理
    fn take_clipboard_request(&mut self) -> Option<ClipboardRequest>;
    /// 光标的像素区域 (x, y, 宽, 高)，原点在窗口内容区左上角。输入法用它摆放候选词窗口。
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    fn ime_cursor_area(&self) -> (u32, u32, u32, u32);
    /// 整窗不透明度（0.0–1.0）；`None` 表示保持平台默认的不透明窗口。
    ///
    /// 平台层每轮 tick 后读一次，只有值变了才真的去设属性，不会每帧发请求。
    /// 半透明要窗口管理器（合成器）配合：X11 用 `_NET_WM_WINDOW_OPACITY`，
    /// 没有合成器时属性被忽略、窗口保持不透明——功能不因此失效。
    fn window_opacity(&self) -> Option<f32> {
        None
    }
}

/// macOS 上 Option 键是否当作 Alt（其他平台没有这个问题，忽略）
static OPTION_AS_ALT: AtomicU8 = AtomicU8::new(OptionAsAlt::Both as u8);

pub fn set_option_as_alt(setting: OptionAsAlt) {
    OPTION_AS_ALT.store(setting as u8, Ordering::Relaxed);
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn option_as_alt() -> OptionAsAlt {
    match OPTION_AS_ALT.load(Ordering::Relaxed) {
        x if x == OptionAsAlt::None as u8 => OptionAsAlt::None,
        x if x == OptionAsAlt::Left as u8 => OptionAsAlt::Left,
        x if x == OptionAsAlt::Right as u8 => OptionAsAlt::Right,
        _ => OptionAsAlt::Both,
    }
}

#[cfg(target_os = "linux")]
mod x11;
#[cfg(target_os = "linux")]
pub use x11::Window;

#[cfg(windows)]
mod win32;
#[cfg(windows)]
pub use win32::Window;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::Window;

#[cfg(test)]
mod clipboard_tests {
    use super::*;
    #[test]
    fn utf16_clipboard_checks_utf8_budget_before_allocating_output() {
        let decode = |input: &[u16], limit| ClipboardRead { limit, strict_unicode: false }.utf16(input);
        assert_eq!(decode(&[0x4e2d], 3).as_deref(), Some("中"));
        assert!(decode(&[0x4e2d], 2).is_none());
        assert_eq!(decode(&[0xd83d, 0xde00], 4).as_deref(), Some("😀"));
        assert!(decode(&[0xd83d, 0xde00], 3).is_none());
        assert_eq!(decode(&[0xd800], 3).as_deref(), Some("�"));
    }
    #[test]
    fn restricted_clipboard_admits_exact_utf8_and_rejects_invalid_utf16_without_replacement() {
        let read = ClipboardRead { limit: 1023, strict_unicode: true };
        let text = "x".repeat(1020) + "文";
        assert_eq!(read.utf8(text.as_bytes()).unwrap().len(), 1023);
        assert_eq!(read.utf16(&text.encode_utf16().collect::<Vec<_>>()).unwrap().len(), 1023);
        assert!(read.utf8((text.clone() + "x").as_bytes()).is_none());
        assert!(read.utf16(&(text + "x").encode_utf16().collect::<Vec<_>>()).is_none());
        for invalid in [[0xd800], [0xdc00]] {
            assert!(read.utf16(&invalid).is_none());
        }
        assert!(read.utf8(&[0xff]).is_none());
        assert_eq!(read.utf8(b"a\0b").unwrap().as_bytes(), b"a\0b");
    }
    #[test]
    fn clipboard_policy_clamps_global_limit_and_handles_empty_requests() {
        assert_eq!(ClipboardRead::default().limit(), CLIPBOARD_LIMIT);
        assert_eq!(ClipboardRead { limit: usize::MAX, strict_unicode: true }.limit(), CLIPBOARD_LIMIT);
        let empty = ClipboardRead { limit: 0, strict_unicode: true };
        assert_eq!(empty.utf8(b"").as_deref(), Some(""));
        assert_eq!(empty.utf16(&[]).as_deref(), Some(""));
        assert!(empty.utf8(b"x").is_none());
        assert!(empty.utf16(&[b'x' as u16]).is_none());
    }
}
