//! Unix 伪终端。
//!
//! 流程：
//! 1. `posix_openpt` 打开主设备（master），`grantpt`/`unlockpt` 解锁从设备（slave）
//! 2. `fork` 出子进程。子进程里：`setsid` 新建会话 → 打开从设备并设为控制终端
//!    → 把它接到 stdin/stdout/stderr → `execve` 启动 shell
//! 3. 父进程读写主设备，就等于在和 shell 的终端交互

use super::{EventSink, PtyEvent};
use std::ffi::{CStr, CString, c_char, c_int, c_ulong, c_ushort};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[repr(C)]
struct Winsize {
    ws_row: c_ushort,
    ws_col: c_ushort,
    ws_xpixel: c_ushort,
    ws_ypixel: c_ushort,
}

// 这些常量在不同系统上取值不同，来自各自的系统头文件
#[cfg(target_os = "linux")]
mod consts {
    use std::ffi::{c_int, c_ulong};
    pub const O_NOCTTY: c_int = 0o400;
    pub const TIOCSWINSZ: c_ulong = 0x5414;
    pub const TIOCSCTTY: c_ulong = 0x540E;
}

#[cfg(target_os = "macos")]
mod consts {
    use std::ffi::{c_int, c_ulong};
    pub const O_NOCTTY: c_int = 0x20000;
    pub const TIOCSWINSZ: c_ulong = 0x8008_7467;
    pub const TIOCSCTTY: c_ulong = 0x2000_7461;
}

use consts::*;

const O_RDWR: c_int = 2;
const F_SETFD: c_int = 2;
const FD_CLOEXEC: c_int = 1;
const SIGHUP: c_int = 1;
const SIGPIPE: c_int = 13;
const SIG_DFL: usize = 0;

unsafe extern "C" {
    fn posix_openpt(flags: c_int) -> c_int;
    fn grantpt(fd: c_int) -> c_int;
    fn unlockpt(fd: c_int) -> c_int;
    fn ptsname(fd: c_int) -> *mut c_char;
    fn fork() -> c_int;
    fn setsid() -> c_int;
    fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    fn dup2(from: c_int, to: c_int) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn execve(path: *const c_char, argv: *const *const c_char, envp: *const *const c_char) -> c_int;
    fn _exit(status: c_int) -> !;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn kill(pid: c_int, signal: c_int) -> c_int;
    fn signal(signal: c_int, handler: usize) -> usize;
}

pub struct Pty {
    master: File,
    pid: c_int,
}

impl Pty {
    pub fn spawn(cols: u16, rows: u16, sink: EventSink) -> io::Result<Pty> {
        // 1. 打开主设备
        let fd = unsafe { posix_openpt(O_RDWR | O_NOCTTY) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let master = unsafe { OwnedFd::from_raw_fd(fd) };
        // 避免以后启动的其他子进程继承这个 fd
        unsafe { fcntl(fd, F_SETFD, FD_CLOEXEC) };
        if unsafe { grantpt(fd) } != 0 || unsafe { unlockpt(fd) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let name = unsafe { ptsname(fd) };
        if name.is_null() {
            return Err(io::Error::last_os_error());
        }
        let slave_path = unsafe { CStr::from_ptr(name) }.to_owned();
        set_winsize(fd, cols, rows)?;

        // 2. 在 fork 之前准备好子进程要用的所有数据。
        //    fork 之后的子进程里只能调用 async-signal-safe 的函数，不能分配内存。
        let shell = find_shell();
        let shell = CString::new(shell.as_os_str().as_bytes()).map_err(io::Error::other)?;
        let argv = [shell.as_ptr(), std::ptr::null()];
        let env = build_env();
        let mut envp: Vec<*const c_char> = env.iter().map(|s| s.as_ptr()).collect();
        envp.push(std::ptr::null());

        // 3. fork
        let pid = unsafe { fork() };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            unsafe { exec_child(&slave_path, fd, &shell, argv.as_ptr(), envp.as_ptr()) }
        }

        // 4. 父进程：开一个线程专门读 shell 的输出
        let master = File::from(master);
        let mut reader = master.try_clone()?;
        std::thread::Builder::new().name("pty-reader".into()).spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => sink(PtyEvent::Data(buf[..n].to_vec())),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    // 子进程退出、从设备全部关闭后，Linux 上 read 会返回 EIO
                    Err(_) => break,
                }
            }
            let mut status = 0;
            unsafe { waitpid(pid, &mut status, 0) };
            sink(PtyEvent::Exit);
        })?;

        Ok(Pty { master, pid })
    }

    pub fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.master.write_all(data)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        // 内核会自动给 shell 的前台进程组发 SIGWINCH
        set_winsize(self.master.as_raw_fd(), cols, rows)
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        unsafe { kill(self.pid, SIGHUP) };
    }
}

/// fork 出来的子进程执行的代码，最终变成 shell
unsafe fn exec_child(
    slave_path: &CStr,
    master_fd: c_int,
    shell: &CStr,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> ! {
    unsafe {
        // 新建会话，脱离父进程的控制终端
        setsid();
        let slave = open(slave_path.as_ptr(), O_RDWR);
        if slave < 0 {
            _exit(126);
        }
        // 让从设备成为这个会话的控制终端，这样 Ctrl+C 才能变成 SIGINT
        ioctl(slave, TIOCSCTTY, 0 as c_int);
        dup2(slave, 0);
        dup2(slave, 1);
        dup2(slave, 2);
        if slave > 2 {
            close(slave);
        }
        close(master_fd);
        // Rust 运行时把 SIGPIPE 设成了忽略，这个设置会被 exec 继承，要恢复默认
        signal(SIGPIPE, SIG_DFL);
        execve(shell.as_ptr(), argv, envp);
        _exit(127);
    }
}

fn set_winsize(fd: c_int, cols: u16, rows: u16) -> io::Result<()> {
    let ws = Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    if unsafe { ioctl(fd, TIOCSWINSZ, &ws as *const Winsize) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 优先用 CONGMINAL_SHELL，其次 SHELL，最后 /bin/sh。不是绝对路径时在 PATH 里找。
fn find_shell() -> PathBuf {
    let name = std::env::var_os("CONGMINAL_SHELL")
        .or_else(|| std::env::var_os("SHELL"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".into());
    let path = PathBuf::from(&name);
    if path.is_absolute() {
        return path;
    }
    std::env::var_os("PATH")
        .and_then(|paths| std::env::split_paths(&paths).map(|dir| dir.join(&name)).find(|p| p.is_file()))
        .unwrap_or_else(|| Path::new("/bin/sh").to_path_buf())
}

fn build_env() -> Vec<CString> {
    let overrides = [("TERM", "xterm-256color"), ("COLORTERM", "truecolor"), ("TERM_PROGRAM", "congminal")];
    let mut env: Vec<CString> = std::env::vars_os()
        .filter(|(key, _)| !overrides.iter().any(|(k, _)| key == k))
        .filter_map(|(key, value)| {
            let mut entry = key.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            CString::new(entry).ok()
        })
        .collect();
    for (key, value) in overrides {
        env.push(CString::new(format!("{key}={value}")).unwrap());
    }
    env
}
