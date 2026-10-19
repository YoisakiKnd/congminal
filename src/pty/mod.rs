//! 伪终端（PTY）：启动 shell，并在它和我们之间架一条双向管道。
//!
//! - Unix（Linux/macOS）：`posix_openpt` + `fork` + `execve`
//! - Windows：ConPTY（`CreatePseudoConsole`，需要 Windows 10 1809 及以上）
//!
//! 两边对外的接口一致：
//! - `Pty::spawn(cols, rows, sink)` 启动 shell，后台线程把读到的输出通过 `sink` 回调送出
//! - `Pty::write` 把键盘输入写给 shell
//! - `Pty::resize` 通知 shell 窗口尺寸变了

use std::sync::Arc;

pub enum PtyEvent {
    /// shell 的输出
    Data(Vec<u8>),
    /// shell 已退出
    Exit,
}

/// 后台线程收到数据时调用的回调
pub type EventSink = Arc<dyn Fn(PtyEvent) + Send + Sync>;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::Pty;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::Pty;
