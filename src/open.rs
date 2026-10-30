//! 用系统程序打开本地文件（打开远端文件时用的临时副本）。
//!
//! 只做一次 `spawn`：不等待、不读输出、不新增线程。子进程由工作区每帧 `reap` 回收，
//! 数量有上限（`MAX_CHILDREN`），避免长期堆积成僵尸进程或句柄。
use std::path::Path;
use std::process::{Child, Command, Stdio};

/// 同时存在的打开器进程上限；超出的先回收已退出的，再拒绝新的。
pub const MAX_CHILDREN: usize = 8;

/// 启动打开器：标准输入/输出/错误都接到空设备，不给终端或管道留句柄。
pub fn spawn(program: &Path, args: &[&str], path: &Path) -> std::io::Result<Child> {
    let _launch = crate::process_launch::guard();
    Command::new(program).args(args).arg(path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
}

/// 回收已经退出的子进程；返回仍在运行的数量。
pub fn reap(children: &mut Vec<Child>) -> usize {
    children.retain_mut(|child| match child.try_wait() {
        // 还在跑：保留。
        Ok(None) => true,
        // 已退出（拿到退出码）或已经不可查询：不再需要。
        Ok(Some(_)) | Err(_) => false,
    });
    children.len()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_missing_program_reports_an_error_instead_of_panicking() {
        let error = spawn(Path::new("/nonexistent/congminal/opener"), &[], Path::new("/tmp")).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn children_are_reaped_after_they_exit_and_the_cap_counts_only_live_ones() {
        let mut children = Vec::new();
        for _ in 0..3 {
            children.push(spawn(Path::new("/bin/sh"), &["-c", "true"], Path::new("ignored")).unwrap());
        }
        // 给它们一点时间退出，然后应当全部回收。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while reap(&mut children) != 0 {
            assert!(std::time::Instant::now() < deadline, "打开器进程没有退出");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(children.is_empty());
    }
}
