//! 平台层：窗口、事件循环、把像素贴到屏幕上。
//!
//! 每个平台各实现一份同样接口的 `Window` 和 `Waker`：
//! - `Window::new(title, size_for_scale)`：创建窗口。`size_for_scale` 根据屏幕缩放比例算出初始像素尺寸
//! - `Window::scale_factor()`：屏幕缩放比例（高分屏是 2.0）
//! - `Window::waker()`：可以跨线程使用的唤醒器，PTY 读线程用它叫醒事件循环
//! - `Window::run(app)`：进入事件循环，直到 `App::event` 返回 false
//!
//! 平台层只认识像素和按键，完全不知道终端的存在。

use crate::config::OptionAsAlt;
use crate::input::{Key, Mods};
use crate::render::Framebuffer;
use std::sync::atomic::{AtomicU8, Ordering};

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
    },
    MouseUp {
        button: MouseButton,
    },
    MouseMove {
        x: f64,
        y: f64,
    },
    /// 滚轮，正数表示往上（往回看历史）
    Scroll(ScrollDelta),
    /// 用户要求复制（macOS 的 Cmd+C 菜单）
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Copy,
    /// 用户要求粘贴（macOS 的 Cmd+V 菜单）。App 会通过 take_clipboard_request 要剪贴板内容。
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    PasteRequest,
    /// 剪贴板里的文字（回应 ClipboardRequest::Paste）
    Paste(String),
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
    Copy(String),
    /// 读出剪贴板内容，用 Event::Paste 送回来
    Paste,
}

pub trait App {
    /// 处理一个事件。返回 false 表示退出。
    fn event(&mut self, event: Event) -> bool;
    fn needs_redraw(&self) -> bool;
    fn draw(&mut self) -> &Framebuffer;
    /// 如果窗口标题需要改，返回新标题
    fn take_title(&mut self) -> Option<String>;
    /// 需要读写剪贴板时返回请求，平台层处理
    fn take_clipboard_request(&mut self) -> Option<ClipboardRequest>;
    /// 光标的像素区域 (x, y, 宽, 高)，原点在窗口内容区左上角。输入法用它摆放候选词窗口。
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    fn ime_cursor_area(&self) -> (u32, u32, u32, u32);
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
