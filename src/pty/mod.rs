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
#[cfg(any(windows, test))]
mod response;

use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;

/// 测试用拥有命令；生产启动只借用配置，不复制参数/环境。
#[cfg(test)]
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

/// 借用仅持续到同步启动结束；后台工作器不保留配置引用。
#[derive(Clone, Copy, Debug, Default)]
pub struct CommandRef<'a> {
    pub program: Option<&'a str>,
    pub args: &'a [String],
    pub working_directory: Option<&'a Path>,
    pub env: &'a [(String, String)],
}
pub trait CommandSource {
    fn view(&self) -> CommandRef<'_>;
}
impl CommandSource for CommandRef<'_> {
    fn view(&self) -> CommandRef<'_> {
        *self
    }
}
#[cfg(test)]
impl CommandSource for Command {
    fn view(&self) -> CommandRef<'_> {
        CommandRef {
            program: self.program.as_deref(),
            args: &self.args,
            working_directory: self.working_directory.as_deref(),
            env: &self.env,
        }
    }
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

#[cfg(all(test, windows))]
pub(crate) use windows::force_system_backend;
