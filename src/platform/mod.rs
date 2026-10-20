//! 平台层：窗口、事件循环、把像素贴到屏幕上。
//!
//! 每个平台各实现一份同样接口的 `Window` 和 `Waker`：
//! - `Window::new(title, size_for_scale)`：创建窗口。`size_for_scale` 根据屏幕缩放比例算出初始像素尺寸
//! - `Window::scale_factor()`：屏幕缩放比例（高分屏是 2.0）
//! - `Window::waker()`：可以跨线程使用的唤醒器，PTY 读线程用它叫醒事件循环
//! - `Window::run(app)`：进入事件循环，直到 `App::event` 返回 false
//!
//! 平台层只认识像素和按键，完全不知道终端的存在。

use crate::input::{Key, Mods};
use crate::render::Framebuffer;

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
    /// 窗口需要重画（比如被遮挡后重新露出来）。macOS 的图层会保留内容，用不到它。
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Redraw,
    /// 其他线程调用了 `Waker::wake`
    Wake,
    CloseRequested,
}

pub trait App {
    /// 处理一个事件。返回 false 表示退出。
    fn event(&mut self, event: Event) -> bool;
    fn needs_redraw(&self) -> bool;
    fn draw(&mut self) -> &Framebuffer;
    /// 如果窗口标题需要改，返回新标题
    fn take_title(&mut self) -> Option<String>;
    /// 光标的像素区域 (x, y, 宽, 高)，原点在窗口内容区左上角。输入法用它摆放候选词窗口。
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    fn ime_cursor_area(&self) -> (u32, u32, u32, u32);
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
