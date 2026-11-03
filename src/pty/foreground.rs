//! 前台程序：会话标签在程序没有设置标题时显示它的名字。
//!
//! - Unix：`tcgetpgrp` 给出终端的前台进程组，组长就是 shell 正在等的那个作业
//! - Windows：没有作业控制，从会话的 Job 对象里取进程列表，按进程树推断

/// 一次探测的结果。探测失败（PTY 已关闭、系统调用出错）时调用方拿到 `None`，保持原状。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Foreground {
    /// shell 自己在前台：处于提示符，或程序在后台运行
    Shell,
    /// 前台程序的进程名，不含路径和 `.exe`
    Program(String),
}

/// Windows 进程树里的一项；`created` 是 FILETIME（100 ns）。
#[cfg(any(windows, test))]
pub(super) struct Proc {
    pub pid: u32,
    pub parent: u32,
    pub created: u64,
    pub name: String,
}

/// 刚启动不满 250 ms 的进程不算前台：pwsh 每次画提示符都会短暂启动 starship，避免标签闪烁。
#[cfg(any(windows, test))]
const SETTLE: u64 = 2_500_000;

/// 从会话根进程推断前台程序：
/// 1. 先跳过同名包装进程（scoop shim 的 `pwsh.exe` 会再启动真正的 `pwsh.exe`），得到 shell
/// 2. shell 最新启动的子进程就是正在运行的命令，同样跳过同名包装进程
/// 3. 命令是 `cmd`（npm 等生成的 `.cmd` 包装）且还有子进程时继续往下找
///
/// 不再往更深处找：nvim 启动的语言服务这类子进程不是前台程序。
#[cfg(any(windows, test))]
pub(super) fn pick(root: u32, procs: &[Proc], now: u64) -> Option<Foreground> {
    fn child<'a>(procs: &'a [Proc], parent: &Proc, now: u64) -> Option<&'a Proc> {
        procs
            .iter()
            .filter(|p| {
                // PID 会复用：父进程必须比子进程先启动
                p.parent == parent.pid
                    && p.pid != parent.pid
                    && p.created >= parent.created
                    && now.saturating_sub(p.created) >= SETTLE
                    && !["conhost", "OpenConsole"].iter().any(|n| p.name.eq_ignore_ascii_case(n))
            })
            .max_by_key(|p| p.created)
    }
    fn unwrap<'a>(procs: &'a [Proc], mut p: &'a Proc, now: u64) -> &'a Proc {
        while let Some(c) = child(procs, p, now).filter(|c| c.name.eq_ignore_ascii_case(&p.name)) {
            p = c;
        }
        p
    }
    let shell = unwrap(procs, procs.iter().find(|p| p.pid == root)?, now);
    let Some(command) = child(procs, shell, now) else { return Some(Foreground::Shell) };
    let mut command = unwrap(procs, command, now);
    while command.name.eq_ignore_ascii_case("cmd")
        && let Some(c) = child(procs, command, now)
    {
        command = unwrap(procs, c, now);
    }
    Some(Foreground::Program(command.name.clone()))
}

/// ConPTY 在程序没设标题时把控制台默认标题（shell 的完整路径）作为 OSC 标题发来：只显示文件名。
pub fn console_default_title(title: &str) -> Option<&str> {
    let name = title.rsplit(['\\', '/']).next()?;
    let stem = name.len().checked_sub(4).filter(|&n| n > 0 && name.is_char_boundary(n))?;
    (name.len() < title.len() && name[stem..].eq_ignore_ascii_case(".exe")).then(|| &name[..stem])
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000_000;
    fn p(pid: u32, parent: u32, age_ms: u64, name: &str) -> Proc {
        Proc { pid, parent, created: NOW - age_ms * 10_000, name: name.into() }
    }

    #[test]
    fn shims_are_skipped_and_the_newest_command_wins() {
        // scoop shim pwsh → pwsh → nvim shim → nvim → 语言服务
        let procs = [
            p(10, 1, 9000, "pwsh"),
            p(11, 10, 8999, "pwsh"),
            p(12, 11, 5000, "nvim"),
            p(13, 12, 4999, "nvim"),
            p(14, 13, 4000, "node"),
            p(15, 13, 3000, "conhost"),
            p(16, 11, 7000, "git"),
        ];
        assert_eq!(pick(10, &procs, NOW), Some(Foreground::Program("nvim".into())));
    }

    #[test]
    fn idle_shell_and_short_lived_prompt_helpers_are_the_shell() {
        let procs = [p(10, 1, 9000, "pwsh"), p(11, 10, 8999, "pwsh"), p(12, 11, 50, "starship")];
        assert_eq!(pick(10, &procs, NOW), Some(Foreground::Shell));
        assert_eq!(pick(10, &procs[..2], NOW), Some(Foreground::Shell));
        assert_eq!(pick(99, &procs, NOW), None);
    }

    #[test]
    fn batch_wrappers_resolve_to_their_program() {
        let procs = [p(10, 1, 9000, "pwsh"), p(11, 10, 5000, "cmd"), p(12, 11, 4900, "node")];
        assert_eq!(pick(10, &procs, NOW), Some(Foreground::Program("node".into())));
        assert_eq!(pick(10, &procs[..2], NOW), Some(Foreground::Program("cmd".into())));
    }

    #[test]
    fn reused_parent_ids_are_not_children() {
        // PID 11 先退出又被复用：比 12 晚启动的“父进程”不是真正的父进程
        let procs = [p(10, 1, 9000, "pwsh"), p(11, 10, 1000, "less"), p(12, 11, 5000, "orphan")];
        assert_eq!(pick(10, &procs, NOW), Some(Foreground::Program("less".into())));
    }

    #[test]
    fn console_default_titles_show_the_program_name() {
        assert_eq!(console_default_title(r"C:\Users\me\scoop\shims\pwsh.exe"), Some("pwsh"));
        assert_eq!(console_default_title(r"C:\Windows\System32\CMD.EXE"), Some("CMD"));
        assert_eq!(console_default_title("pwsh.exe"), None);
        assert_eq!(console_default_title(r"C:\.exe"), None);
        assert_eq!(console_default_title("me@host: ~/src"), None);
        assert_eq!(console_default_title(r"D:\项目\工具.exe"), Some("工具"));
    }
}
