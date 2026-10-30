//! 仅按需运行的 OpenSSH askpass/启动器。没有 GUI、配置观察器或凭据日志。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
// 此程序只使用共用组件的子集，父程序继续检查其完整接口。
#[allow(dead_code)]
#[path = "helper_auth.rs"]
mod auth;
mod executable_path;
#[cfg(windows)]
#[path = "helper_open_windows.rs"]
mod opener;
#[allow(dead_code)]
#[path = "../process_launch.rs"]
mod process_launch;
#[allow(dead_code)]
#[path = "helper_resources.rs"]
mod resources;
#[allow(dead_code)]
#[path = "../cancellation.rs"]
mod tasks;
#[cfg(windows)]
use auth::ticket::windows::Stream as AuthStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream as AuthStream;
use std::{
    ffi::OsString,
    io,
    path::Path,
    process::{Command, Stdio},
};
use std::{
    io::{Read, Write},
    time::{Duration, Instant},
};
const TICKET_ENV: &str = "CONGMINAL_ASKPASS_TICKET";
fn rejected() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "authentication helper input rejected")
}
#[cfg(unix)]
fn output() -> io::Result<std::fs::File> {
    use std::os::fd::BorrowedFd;
    Ok(std::fs::File::from(unsafe { BorrowedFd::borrow_raw(1) }.try_clone_to_owned()?))
}
#[cfg(windows)]
fn output() -> io::Result<std::fs::File> {
    use std::{ffi::c_void, os::windows::io::BorrowedHandle};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(kind: u32) -> *mut c_void;
    }
    let handle = unsafe { GetStdHandle((-11i32) as u32) };
    if handle.is_null() || handle as isize == -1 {
        return Err(rejected());
    }
    Ok(std::fs::File::from(unsafe { BorrowedHandle::borrow_raw(handle) }.try_clone_to_owned()?))
}
struct Scratch([u8; 256]);
impl Scratch {
    fn clear(&mut self) {
        for byte in &mut self.0 {
            unsafe {
                std::ptr::write_volatile(byte, 0);
            }
        }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        self.clear();
    }
}
fn askpass(prompt: &std::ffi::OsStr, pool: &std::sync::Arc<resources::Budget>) -> io::Result<i32> {
    if prompt.len() > auth::PROMPT_LIMIT {
        return Err(rejected());
    }
    let path = std::env::var_os(TICKET_ENV).ok_or_else(rejected)?;
    let ticket = auth::ticket::Ticket::read_private(Path::new(&path))?;
    let hint = std::env::var_os("SSH_ASKPASS_PROMPT");
    let kind = auth::Kind::hint(match &hint {
        Some(value) => Some(value.to_str().ok_or_else(rejected)?),
        None => None,
    })
    .map_err(|_| rejected())?;
    exchange(prompt, pool, Path::new(&path), ticket, kind)
}
#[cfg(windows)]
fn ready(_stream: &AuthStream, _events: i16, deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        return Err(io::ErrorKind::TimedOut.into());
    }
    // 只有按需运行的 helper 等待；主程序仍由工作槽 tick 推进，没有额外线程。
    std::thread::sleep(Duration::from_millis(5));
    Ok(())
}
#[cfg(unix)]
fn ready(stream: &AuthStream, events: i16, deadline: Instant) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }
    #[cfg(target_os = "macos")]
    type Count = u32;
    #[cfg(target_os = "linux")]
    type Count = std::ffi::c_ulong;
    unsafe extern "C" {
        fn poll(fds: *mut PollFd, count: Count, timeout: i32) -> i32;
    }
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let mut fd = PollFd { fd: stream.as_raw_fd(), events, revents: 0 };
        let millis = remaining.as_millis().saturating_add(1).min(i32::MAX as u128) as i32;
        match unsafe { poll(&mut fd, 1, millis) } {
            n if n > 0 => return Ok(()), // HUP 时也先读完已排队的答案。
            0 => continue,
            _ => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}
fn exchange(
    prompt: &std::ffi::OsStr,
    pool: &std::sync::Arc<resources::Budget>,
    path: &Path,
    ticket: auth::ticket::Ticket,
    kind: auth::Kind,
) -> io::Result<i32> {
    let cancel = tasks::Cancellation::default();
    let packet = auth::wire::request(&ticket.capability, kind, prompt.as_encoded_bytes(), pool, &cancel)
        .map_err(|_| rejected())?;
    #[cfg(unix)]
    let mut stream = AuthStream::connect(path.with_file_name("s"))?;
    #[cfg(windows)]
    let mut stream = {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match AuthStream::connect(path) {
                Ok(stream) => break stream,
                Err(e) if e.raw_os_error() == Some(231) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => return Err(e),
            }
        }
    };
    stream.set_nonblocking(true)?;
    let write_deadline = Instant::now() + Duration::from_secs(5);
    let mut offset = 0;
    while offset < packet.bytes().len() {
        ready(&stream, 4, write_deadline)?;
        match stream.write(&packet.bytes()[offset..]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => offset += n,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
            Err(e) => return Err(e),
        }
    }
    drop(packet);
    #[cfg(unix)]
    stream.shutdown(std::net::Shutdown::Write)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut decoder = auth::wire::AnswerDecoder::new(pool.clone());
    let mut bytes = Scratch([0; 256]);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        ready(&stream, 1, deadline)?;
        let n = match stream.read(&mut bytes.0) {
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => continue,
            result => result?,
        };
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let decoded = decoder.push(&bytes.0[..n], &cancel).map_err(|_| rejected());
        // 网络读暂存也可能含答案，不等到函数返回才清理旧分片。
        bytes.clear();
        match decoded? {
            Some(auth::wire::Answer::Cancelled) => return Ok(1),
            Some(auth::wire::Answer::Accepted(secret)) => {
                #[cfg(windows)]
                {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        match stream.acknowledge() {
                            Ok(()) => break,
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                                std::thread::sleep(Duration::from_millis(5))
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
                // 恰好 1023 字节时不追加换行；OpenSSH 以 EOF/读取上限完成。
                // File 直接写系统句柄，避免 stdout 的 LineWriter 保留明文副本。
                let mut output = output()?;
                output.write_all(secret.bytes())?;
                output.flush()?;
                return Ok(0);
            }
            None => {}
        }
    }
}
fn launch(args: &[OsString]) -> io::Result<i32> {
    if args.len() < 3 || args.len() > 64 || args.iter().map(|a| a.len()).sum::<usize>() > 16 * 1024 {
        return Err(rejected());
    }
    let ticket = Path::new(&args[1]);
    drop(auth::ticket::Ticket::read_private(ticket)?);
    let helper = executable_path::current()?;
    let mut child = Command::new(&args[2]);
    child
        .args(&args[3..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .env("SSH_ASKPASS", helper)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env(TICKET_ENV, ticket);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(child.exec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Windows 子进程继续属于主程序创建的 Job；绝对路径避免启动器搜索歧义。
        if !Path::new(&args[2]).is_absolute() {
            return Err(rejected());
        }
        child.creation_flags(0x0800_0000);
        Ok(child.status()?.code().unwrap_or(1))
    }
}
fn main() {
    let pool = resources::Budget::root(128 * 1024, 0).unwrap_or_else(|_| std::process::exit(2));
    let _work = pool.lease(resources::Kind::Sftp, 64 * 1024).unwrap_or_else(|_| std::process::exit(2));
    let mut args = Vec::new();
    if args.try_reserve_exact(64).is_err() {
        std::process::exit(2);
    }
    let mut length = 0usize;
    for arg in std::env::args_os().skip(1) {
        length = match length.checked_add(arg.len()) {
            Some(n) if n <= 16 * 1024 => n,
            _ => std::process::exit(2),
        };
        if args.len() == 64 {
            std::process::exit(2);
        }
        args.push(arg);
    }
    #[cfg(windows)]
    if args.len() == 2 && args[0] == "--open-file" {
        std::process::exit(opener::open(Path::new(&args[1])).unwrap_or(2));
    }
    let result = if args.len() >= 3 && args[0] == "--launch" {
        launch(&args)
    } else if args.len() == 1 {
        askpass(&args[0], &pool)
    } else {
        Err(rejected())
    };
    // 不将提示、答案、票据或环境输出到 stderr；退出码区分成功/取消/无效。
    std::process::exit(result.unwrap_or(2));
}
