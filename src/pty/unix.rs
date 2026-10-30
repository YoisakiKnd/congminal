//! Unix 伪终端。
//!
//! 流程：
//! 1. `posix_openpt` 打开主设备（master），`grantpt`/`unlockpt` 解锁从设备（slave）
//! 2. 父进程打开从设备并设置窗口尺寸（和 BSD 的 `openpty()` 一样）
//! 3. `fork` 出子进程。子进程里：`setsid` 新建会话 → 把从设备设为控制终端
//!    → 接到 stdin/stdout/stderr → `execve` 启动 shell。父进程关掉自己手里的从设备
//! 4. 父进程读写主设备，就等于在和 shell 的终端交互

#[cfg(test)]
use super::Command;
use super::flow::{AsyncWriter, ByteQueue, INPUT_LIMIT, READ_CHUNK};
use super::{CommandSource, EventSink, PtyEvent};
use std::ffi::{CStr, CString, c_char, c_int, c_ulong, c_ushort};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
    pub const O_NONBLOCK: c_int = 0o4000;
    pub const WNOWAIT: c_int = 0x0100_0000;
    pub const TIOCSWINSZ: c_ulong = 0x5414;
    pub const TIOCSCTTY: c_ulong = 0x540E;
}

#[cfg(target_os = "macos")]
mod consts {
    use std::ffi::{c_int, c_ulong};
    pub const O_NOCTTY: c_int = 0x20000;
    pub const O_NONBLOCK: c_int = 0x4;
    pub const WNOWAIT: c_int = 0x20;
    pub const TIOCSWINSZ: c_ulong = 0x8008_7467;
    pub const TIOCSCTTY: c_ulong = 0x2000_7461;
}

use consts::*;

const O_RDWR: c_int = 2;
const F_SETFD: c_int = 2;
const FD_CLOEXEC: c_int = 1;
const SIGHUP: c_int = 1;
/// Linux 和 macOS 的标准信号都是 1..=31
const NSIG: c_int = 32;
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
    fn chdir(path: *const c_char) -> c_int;
    fn execve(path: *const c_char, argv: *const *const c_char, envp: *const *const c_char) -> c_int;
    fn _exit(status: c_int) -> !;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn kill(pid: c_int, signal: c_int) -> c_int;
    fn signal(signal: c_int, handler: usize) -> usize;
    fn waitid(kind: c_int, id: u32, info: *mut std::ffi::c_void, options: c_int) -> c_int;
    fn poll(fds: *mut PollFd, count: PollCount, timeout: c_int) -> c_int;
    fn write(fd: c_int, data: *const std::ffi::c_void, len: usize) -> isize;
    #[cfg(target_os = "macos")]
    fn __error() -> *mut c_int;
    #[cfg(target_os = "linux")]
    fn __errno_location() -> *mut c_int;
}

#[cfg(target_os = "macos")]
type PollCount = u32;
#[cfg(target_os = "linux")]
type PollCount = c_ulong;

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

struct Process {
    pid: Mutex<Option<c_int>>,
    running: AtomicBool,
    wake: UnixStream,
}

impl Process {
    fn stop(&self) {
        if !self.running.swap(false, Ordering::AcqRel) {
            return;
        }
        unsafe {
            write(self.wake.as_raw_fd(), b"x".as_ptr().cast(), 1);
        }
    }
}

pub struct Pty {
    master: File,
    writer: Option<AsyncWriter>,
    process: Arc<Process>,
}

impl Pty {
    pub fn respond(&mut self, data: &[u8]) -> io::Result<()> {
        self.write(data)
    }
    pub fn spawn(command: &impl CommandSource, cols: u16, rows: u16, sink: EventSink) -> io::Result<Pty> {
        let command = command.view();
        let pool = crate::resources::current();
        let waiter = crate::runtime::prepare("pty-waiter", &pool)?;
        let writer_permit = crate::runtime::prepare("pty-writer", &pool)?;
        let reader_permit = crate::runtime::prepare("pty-reader", &pool)?;
        let input_queue = Arc::new(ByteQueue::input(INPUT_LIMIT)?);
        // 1. 打开主设备
        let fd = unsafe { posix_openpt(O_RDWR | O_NOCTTY) };
        if fd < 0 {
            return Err(os_error("posix_openpt"));
        }
        let master = above_stdio(unsafe { OwnedFd::from_raw_fd(fd) })?;
        let fd = master.as_raw_fd();
        // 避免以后启动的其他子进程继承这个 fd
        if unsafe { fcntl(fd, F_SETFD, FD_CLOEXEC) } < 0 {
            return Err(os_error("fcntl(CLOEXEC)"));
        }
        // dup 后的 fd 共享非阻塞状态。读写线程分别处理 WouldBlock，UI 不做系统写入。
        if unsafe { fcntl(fd, 4 as c_int, O_RDWR | O_NONBLOCK) } < 0 {
            return Err(os_error("fcntl(NONBLOCK)"));
        }
        if unsafe { grantpt(fd) } != 0 {
            return Err(os_error("grantpt"));
        }
        if unsafe { unlockpt(fd) } != 0 {
            return Err(os_error("unlockpt"));
        }
        let name = unsafe { ptsname(fd) };
        if name.is_null() {
            return Err(os_error("ptsname"));
        }
        let slave_fd = unsafe { open(name, O_RDWR | O_NOCTTY) };
        if slave_fd < 0 {
            return Err(os_error("open(slave)"));
        }
        let slave = above_stdio(unsafe { OwnedFd::from_raw_fd(slave_fd) })?;
        let slave_fd = slave.as_raw_fd();
        if unsafe { fcntl(slave_fd, F_SETFD, FD_CLOEXEC) } < 0 {
            return Err(os_error("fcntl(CLOEXEC)"));
        }
        // macOS 上从设备被打开之前，对主设备设置尺寸会失败（ENOTTY），所以在从设备上设置
        set_winsize(slave_fd, cols, rows)?;

        // 2. 在 fork 之前准备好子进程要用的所有数据。
        //    fork 之后的子进程里只能调用 async-signal-safe 的函数，不能分配内存。
        let to_c = |s: &[u8]| CString::new(s).map_err(|_| io::Error::other("参数里不能有 NUL 字符"));
        let shell = to_c(find_shell(command.program).as_os_str().as_bytes())?;
        let args: Vec<CString> = command.args.iter().map(|a| to_c(a.as_bytes())).collect::<io::Result<_>>()?;
        let mut argv: Vec<*const c_char> = vec![shell.as_ptr()];
        argv.extend(args.iter().map(|a| a.as_ptr()));
        argv.push(std::ptr::null());
        let env = build_env(command.env);
        let mut envp: Vec<*const c_char> = env.iter().map(|s| s.as_ptr()).collect();
        envp.push(std::ptr::null());
        let cwd = command.working_directory.as_ref().map(|dir| to_c(dir.as_os_str().as_bytes())).transpose()?;

        // CLOEXEC 的启动通道：成功 exec 自动关闭；失败写回步骤和 errno。
        let (mut startup, child_startup) = channel()?;
        let (exit_read, exit_write) = channel()?;
        // 3. fork；与后台原生 Socket 的 CLOEXEC 设置协调，子进程直接 exec/_exit。
        let launch_guard = crate::process_launch::guard();
        let pid = unsafe { fork() };
        if pid < 0 {
            return Err(os_error("fork"));
        }
        if pid == 0 {
            unsafe {
                close(startup.as_raw_fd());
                exec_child(
                    slave_fd,
                    fd,
                    child_startup.as_raw_fd(),
                    cwd.as_deref(),
                    &shell,
                    argv.as_ptr(),
                    envp.as_ptr(),
                )
            }
        }
        drop(launch_guard);
        // 父进程不需要从设备。必须关掉，否则 shell 退出后读主设备永远等不到 EOF。
        drop(slave);

        drop(child_startup);
        let mut report = Vec::new();
        let handshake = startup.by_ref().take(9).read_to_end(&mut report);
        if handshake.is_err() || !report.is_empty() {
            unsafe {
                kill(pid, 9);
                waitpid(pid, std::ptr::null_mut(), 0);
            }
            handshake?;
            if report.len() != 8 {
                return Err(io::Error::other("shell 启动错误报告不完整"));
            }
            let stage = i32::from_ne_bytes(report[..4].try_into().unwrap());
            let error = io::Error::from_raw_os_error(i32::from_ne_bytes(report[4..].try_into().unwrap()));
            let stage = match stage {
                1 => "setsid",
                2 => "TIOCSCTTY",
                3 => "dup2",
                4 => "chdir",
                _ => "execve",
            };
            return Err(io::Error::new(error.kind(), format!("{stage}: {error}")));
        }
        drop(startup);

        let process =
            Arc::new(Process { pid: Mutex::new(Some(pid)), running: AtomicBool::new(true), wake: exit_write });
        let mut pty = Pty { master: File::from(master), writer: None, process: process.clone() };
        // waitid 不回收 PID；持锁执行 waitpid 和清除 PID，避免关闭窗口时误杀复用的 PID。
        let exit_sink = sink.clone();
        let child = process.clone();
        let waiter = waiter.spawn(move || {
            #[repr(C, align(16))]
            struct SigInfo([u8; 128]); // Linux 为 128 字节，Darwin 更小；只让系统写入，不解释布局。
            let mut info = SigInfo([0; 128]);
            loop {
                if unsafe { waitid(1, pid as u32, info.0.as_mut_ptr().cast(), 4 | WNOWAIT) } == 0 {
                    break;
                }
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.raw_os_error() == Some(10) {
                    *child.pid.lock().unwrap() = None;
                }
                child.stop();
                exit_sink(PtyEvent::Error(error));
                exit_sink(PtyEvent::Exit(None));
                return;
            }
            let mut status = 0;
            let result = {
                let mut current = child.pid.lock().unwrap();
                let result = loop {
                    let result = unsafe { waitpid(pid, &mut status, 0) };
                    if result >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                        break result;
                    }
                };
                *current = None;
                result
            };
            child.stop();
            let code =
                (result > 0).then(|| if status & 0x7f == 0 { (status >> 8) & 0xff } else { 128 + (status & 0x7f) });
            exit_sink(PtyEvent::Exit(code));
        });
        if let Err(error) = waiter {
            unsafe {
                kill(pid, 9);
                waitpid(pid, std::ptr::null_mut(), 0);
            }
            *process.pid.lock().unwrap() = None;
            return Err(error);
        }
        let writer = pty.master.try_clone()?;
        let mut writable = PollFd { fd: writer.as_raw_fd(), events: 4, revents: 0 };
        pty.writer = Some(AsyncWriter::spawn_reserved(writer_permit, input_queue, writer, sink.clone(), move || {
            // 仅输入管道已满时等待，读端一消费就能继续写。超时用于有界检查关闭。
            unsafe {
                poll(&mut writable, 1, 50);
            }
        })?);
        let mut reader = pty.master.try_clone()?;
        reader_permit.spawn(move || {
            let mut buf = vec![0u8; READ_CHUNK];
            let mut poll_fds = [
                PollFd { fd: reader.as_raw_fd(), events: 1, revents: 0 },
                PollFd { fd: exit_read.as_raw_fd(), events: 1, revents: 0 },
            ];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if !sink(PtyEvent::Data(buf[..n].to_vec())) {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // 进程退出后仍先排空内核缓冲；后代持有 slave 也不会让窗口一直等 EOF。
                        if !process.running.load(Ordering::Acquire) {
                            break;
                        }
                        unsafe {
                            poll(poll_fds.as_mut_ptr(), 2, -1);
                        }
                    }
                    Err(e) => {
                        // Linux 在从设备关闭后返回 EIO，这是正常 EOF。
                        if e.raw_os_error() != Some(5) {
                            sink(PtyEvent::Error(e));
                        }
                        break;
                    }
                }
            }
            sink(PtyEvent::Eof);
        })?;
        Ok(pty)
    }

    pub fn pending_bytes(&self) -> usize {
        self.writer.as_ref().map_or(0, AsyncWriter::pending_bytes)
    }

    pub fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.writer.as_ref().unwrap().write(data)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        // 内核会自动给 shell 的前台进程组发 SIGWINCH
        set_winsize(self.master.as_raw_fd(), cols, rows)
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.process.stop();
        drop(self.writer.take());
        if let Some(pid) = *self.process.pid.lock().unwrap() {
            // 新会话的进程组，以及 shell 本身。所有 master 关闭后内核还会通知前台进程组。
            unsafe {
                kill(-pid, SIGHUP);
                kill(pid, SIGHUP);
            }
        }
    }
}

/// fork 出来的子进程执行的代码，最终变成 shell
unsafe fn exec_child(
    slave: c_int,
    master_fd: c_int,
    report_fd: c_int,
    cwd: Option<&CStr>,
    shell: &CStr,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> ! {
    unsafe {
        // 新建会话，脱离父进程的控制终端
        if setsid() < 0 {
            child_failed(report_fd, 1);
        }
        // 让从设备成为这个会话的控制终端，这样 Ctrl+C 才能变成 SIGINT
        if ioctl(slave, TIOCSCTTY, 0 as c_int) < 0 {
            child_failed(report_fd, 2);
        }
        for target in 0..=2 {
            if dup2(slave, target) < 0 {
                child_failed(report_fd, 3);
            }
        }
        if slave > 2 {
            close(slave);
        }
        close(master_fd);
        if let Some(dir) = cwd
            && chdir(dir.as_ptr()) < 0
        {
            child_failed(report_fd, 4);
        }
        // 被忽略的信号会经过 exec 继承下去：Rust 运行时忽略了 SIGPIPE，
        // 从脚本后台启动时 SIGINT 也可能被忽略（那样 Ctrl+C 就失效了）。全部恢复默认。
        for sig in 1..NSIG {
            signal(sig, SIG_DFL);
        }
        execve(shell.as_ptr(), argv, envp);
        child_failed(report_fd, 5);
    }
}

// 即使从 stdin/stdout 已关闭的进程启动，PTY 和启动通道也不能占用 0/1/2。
fn above_stdio(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() >= 3 {
        return Ok(fd);
    }
    let raw = unsafe { fcntl(fd.as_raw_fd(), 0 as c_int, 3 as c_int) };
    if raw < 0 {
        return Err(os_error("fcntl(DUPFD)"));
    }
    let moved = unsafe { OwnedFd::from_raw_fd(raw) };
    if unsafe { fcntl(raw, F_SETFD, FD_CLOEXEC) } < 0 {
        return Err(os_error("fcntl(CLOEXEC)"));
    }
    Ok(moved)
}

fn channel() -> io::Result<(UnixStream, UnixStream)> {
    let (a, b) = UnixStream::pair()?;
    Ok((above_stdio(a.into())?.into(), above_stdio(b.into())?.into()))
}

/// fork 后只用栈内存和 async-signal-safe 的 write/_exit 报告错误。
unsafe fn child_failed(fd: c_int, stage: i32) -> ! {
    unsafe {
        #[cfg(target_os = "macos")]
        let errno = *__error();
        #[cfg(target_os = "linux")]
        let errno = *__errno_location();
        let mut report = [0u8; 8];
        report[..4].copy_from_slice(&stage.to_ne_bytes());
        report[4..].copy_from_slice(&errno.to_ne_bytes());
        let mut offset = 0;
        while offset < report.len() {
            let n = write(fd, report[offset..].as_ptr().cast(), report.len() - offset);
            if n <= 0 {
                break;
            }
            offset += n as usize;
        }
        _exit(127);
    }
}

fn set_winsize(fd: c_int, cols: u16, rows: u16) -> io::Result<()> {
    let ws = Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    if unsafe { ioctl(fd, TIOCSWINSZ, &ws as *const Winsize) } != 0 {
        return Err(os_error("ioctl(TIOCSWINSZ)"));
    }
    Ok(())
}

/// 带上失败步骤名的系统错误，方便定位是哪一个调用出了问题
fn os_error(call: &str) -> io::Error {
    let err = io::Error::last_os_error();
    io::Error::new(err.kind(), format!("{call}: {err}"))
}

/// 配置里指定的程序，否则用 $SHELL，最后是 /bin/sh。不是绝对路径时在 PATH 里找。
fn find_shell(program: Option<&str>) -> PathBuf {
    let name = program
        .map(Into::into)
        .or_else(|| std::env::var_os("SHELL"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".into());
    let path = PathBuf::from(&name);
    if path.is_absolute() {
        return path;
    }
    std::env::var_os("PATH")
        .and_then(|paths| std::env::split_paths(&paths).map(|dir| dir.join(&name)).find(|p| p.is_file()))
        .unwrap_or(path)
}

/// 继承当前的环境变量，加上终端相关的几个，最后是配置里的（配置优先）
fn build_env(extra: &[(String, String)]) -> Vec<CString> {
    let mut overrides: Vec<(&str, &str)> =
        vec![("TERM", "xterm-256color"), ("COLORTERM", "truecolor"), ("TERM_PROGRAM", "congminal")];
    overrides.retain(|(key, _)| !extra.iter().any(|(k, _)| k == key));
    overrides.extend(extra.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let mut env: Vec<CString> = std::env::vars_os()
        .filter(|(key, _)| !overrides.iter().any(|(k, _)| key == k))
        .filter_map(|(key, value)| {
            let mut entry = key.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            CString::new(entry).ok()
        })
        .collect();
    env.extend(overrides.iter().filter_map(|(key, value)| CString::new(format!("{key}={value}")).ok()));
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::flow::{ByteQueue, OUTPUT_LIMIT};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn startup_errors_reach_the_parent() {
        let sink: EventSink = Arc::new(|_| true);
        let command = Command { program: Some("/congminal-missing-shell".into()), ..Default::default() };
        let error = Pty::spawn(&command, 80, 24, sink.clone()).err().unwrap();
        assert!(error.to_string().contains("execve"), "{error}");
        let command = Command {
            program: Some("/bin/sh".into()),
            working_directory: Some("/congminal-missing-cwd".into()),
            ..Default::default()
        };
        let error = Pty::spawn(&command, 80, 24, sink).err().unwrap();
        assert!(error.to_string().contains("chdir"), "{error}");
    }

    #[test]
    fn real_pty_preserves_output_across_backpressure_and_exit() {
        let output = Arc::new(ByteQueue::new(OUTPUT_LIMIT));
        let pending = output.clone();
        let (send, receive) = mpsc::channel();
        let sink: EventSink = Arc::new(move |event| match event {
            PtyEvent::Data(data) => pending.push(data),
            other => send.send(other).is_ok(),
        });
        let command = Command {
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), "head -c 2097152 /dev/zero; printf tail; exit 7".into()],
            ..Default::default()
        };
        let _pty = Pty::spawn(&command, 80, 24, sink).unwrap();
        // 先让读线程触及背压，再开始消费。整个测试仍有明确超时。
        std::thread::sleep(Duration::from_millis(30));
        let until = Instant::now() + Duration::from_secs(10);
        let (mut exited, mut eof) = (false, false);
        let mut bytes = Vec::new();
        while Instant::now() < until {
            while let Some(data) = output.pop() {
                bytes.extend(data);
            }
            while let Ok(event) = receive.try_recv() {
                match event {
                    PtyEvent::Exit(code) => {
                        assert_eq!(code, Some(7));
                        exited = true;
                    }
                    PtyEvent::Eof => eof = true,
                    PtyEvent::Error(error) => panic!("{error}"),
                    _ => unreachable!(),
                }
            }
            if eof && exited && output.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        output.close();
        assert!(exited && eof, "PTY 超时");
        assert_eq!(bytes.len(), 2097152 + 4);
        assert!(bytes[..2097152].iter().all(|&b| b == 0));
        assert_eq!(&bytes[2097152..], b"tail");
    }

    #[test]
    fn nonreading_child_does_not_block_the_caller() {
        let (send, receive) = mpsc::channel();
        let sink: EventSink = Arc::new(move |event| {
            if let PtyEvent::Data(data) = event {
                let _ = send.send(data);
            }
            true
        });
        let command = Command {
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), "stty -echo -icanon; printf ready; sleep 10".into()],
            ..Default::default()
        };
        let mut pty = Pty::spawn(&command, 80, 24, sink).unwrap();
        assert_eq!(receive.recv_timeout(Duration::from_secs(2)).unwrap(), b"ready");
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            pty.write(&vec![b'x'; crate::pty::flow::INPUT_LIMIT]).unwrap();
            let result = pty.write(b"extra");
            send.send(result.map_err(|e| e.kind())).unwrap();
            drop(pty);
        });
        let result = receive.recv_timeout(Duration::from_secs(2)).expect("写入阻塞了调用线程");
        assert_eq!(result, Err(io::ErrorKind::WouldBlock));
        worker.join().unwrap();
    }
}
