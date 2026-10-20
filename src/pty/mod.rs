//! 伪终端（PTY）：启动 shell，并在它和我们之间架一条双向管道。
//!
//! - Unix（Linux/macOS）：`posix_openpt` + `fork` + `execve`
//! - Windows：ConPTY（`CreatePseudoConsole`，需要 Windows 10 1809 及以上）
//!
//! 两边对外的接口一致：
//! - `Pty::spawn(command, cols, rows, sink)` 启动 shell，后台线程把读到的输出通过 `sink` 回调送出
//! - `Pty::write` 把键盘输入写给 shell
//! - `Pty::resize` 通知 shell 窗口尺寸变了

pub mod flow;

use std::path::PathBuf;
use std::sync::Arc;

/// 要在伪终端里启动的程序
#[derive(Clone, Debug, Default)]
pub struct Command {
    /// None 表示用系统默认的 shell
    pub program: Option<String>,
    pub args: Vec<String>,
    /// None 表示继承当前目录
    pub working_directory: Option<PathBuf>,
    /// 额外的环境变量，覆盖继承来的同名变量
    pub env: Vec<(String, String)>,
}

pub enum PtyEvent {
    /// shell 的输出
    Data(Vec<u8>),
    /// shell 已退出
    Exit(Option<i32>),
    /// 输出已经读完（独立于进程退出，保证退出前最后一段输出也被处理）。
    Eof,
    Error(std::io::Error),
}

/// 后台线程收到数据时调用的回调
pub type EventSink = Arc<dyn Fn(PtyEvent) -> bool + Send + Sync>;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::Pty;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::Pty;
