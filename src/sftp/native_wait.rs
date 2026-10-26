//! 原生 SSH 非阻塞等待：只在活动请求等待网络时 poll；空闲会话不启动循环。
use super::native::Error;
use crate::tasks::Cancellation;
use std::{
    io,
    net::TcpStream,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawSocket;

#[cfg(unix)]
#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}
#[cfg(windows)]
#[repr(C)]
struct PollFd {
    fd: usize,
    events: i16,
    revents: i16,
}
#[cfg(target_os = "macos")]
type Count = u32;
#[cfg(target_os = "linux")]
type Count = std::ffi::c_ulong;
#[cfg(unix)]
unsafe extern "C" {
    fn poll(fds: *mut PollFd, count: Count, timeout: i32) -> i32;
}
#[cfg(windows)]
#[link(name = "ws2_32")]
unsafe extern "system" {
    fn WSAPoll(fds: *mut PollFd, count: u32, timeout: i32) -> i32;
    fn WSAGetLastError() -> i32;
}
#[cfg(unix)]
const READ: i16 = 1;
#[cfg(unix)]
const WRITE: i16 = 4;
// Windows SDK WinSock2.h：POLLRDNORM / POLLWRNORM。
#[cfg(windows)]
const READ: i16 = 0x0100;
#[cfg(windows)]
const WRITE: i16 = 0x0010;

pub fn check(cancel: &Cancellation, deadline: Instant) -> Result<(), Error> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else if Instant::now() >= deadline {
        Err(Error::Timeout)
    } else {
        Ok(())
    }
}
pub fn ready(socket: &TcpStream, directions: i32, cancel: &Cancellation, deadline: Instant) -> Result<bool, Error> {
    check(cancel, deadline)?;
    let mut events = 0;
    if directions & 1 != 0 {
        events |= READ;
    }
    if directions & 2 != 0 {
        events |= WRITE;
    }
    if events == 0 {
        events = READ;
    }
    let millis =
        deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(20)).as_millis().max(1) as i32;
    #[cfg(unix)]
    let mut fd = PollFd { fd: socket.as_raw_fd(), events, revents: 0 };
    #[cfg(windows)]
    let mut fd = PollFd { fd: socket.as_raw_socket() as usize, events, revents: 0 };
    // SAFETY: fd 指向一个按当前平台头文件布局的可写元素；socket 在调用期间仍持有。
    #[cfg(unix)]
    let result = unsafe { poll(&mut fd, 1, millis) };
    #[cfg(windows)]
    let result = unsafe { WSAPoll(&mut fd, 1, millis) };
    if result < 0 {
        #[cfg(unix)]
        let error = io::Error::last_os_error();
        #[cfg(windows)]
        let error = io::Error::from_raw_os_error(unsafe { WSAGetLastError() });
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(Error::Io(error.kind()));
        }
    }
    // 不先丢弃 HUP：库先消费已接收数据，再根据真实通道 EOF 判定关闭。
    check(cancel, deadline)?;
    Ok(result > 0)
}
