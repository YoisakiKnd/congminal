//! 受控地址列表的非阻塞 TCP 建立；数值地址不经 DNS。
use super::{native::Error, native_wait as wait};
use crate::{
    resources::{Budget, Kind},
    tasks::Cancellation,
};
use std::{
    io,
    net::{SocketAddr, TcpStream},
    sync::Arc,
    time::{Duration, Instant},
};

pub fn host(
    name: &str,
    port: u16,
    pool: &Arc<Budget>,
    cancel: &Cancellation,
    cache: &mut crate::resolve::Cache,
) -> Result<TcpStream, Error> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let addresses = cache.resolve(name, port, pool, cancel).map_err(|error| match error {
        crate::resolve::Error::Budget => Error::Budget,
        crate::resolve::Error::Cancelled => Error::Cancelled,
        crate::resolve::Error::Timeout => Error::Timeout,
        crate::resolve::Error::Invalid => Error::Invalid,
        _ => Error::Resolution,
    })?;
    addresses_connect(&addresses, pool, cancel, deadline)
}
pub fn addresses_connect(
    addresses: &[SocketAddr],
    pool: &Arc<Budget>,
    cancel: &Cancellation,
    deadline: Instant,
) -> Result<TcpStream, Error> {
    if addresses.is_empty() || addresses.len() > 16 || addresses.iter().any(|a| a.port() == 0) {
        return Err(Error::Invalid);
    }
    wait::check(cancel, deadline)?;
    let _memory = pool.lease(Kind::Sftp, 1024).map_err(|_| Error::Budget)?;
    let mut last = Error::Resolution;
    let mut pending: [Option<TcpStream>; 2] = [None, None];
    let mut next = 0;
    let mut launch = Instant::now();
    loop {
        wait::check(cancel, deadline)?;
        if next < addresses.len()
            && pending.iter().any(Option::is_none)
            && (pending.iter().all(Option::is_none) || Instant::now() >= launch)
        {
            let address = addresses[next];
            next += 1;
            match platform::start(address) {
                Ok((socket, true)) => return Ok(socket),
                Ok((socket, false)) => {
                    let slot = pending.iter_mut().find(|p| p.is_none()).unwrap();
                    *slot = Some(socket);
                    launch = Instant::now() + Duration::from_millis(200);
                }
                Err(error) => {
                    last = error;
                    launch = Instant::now();
                }
            }
        }
        for slot in &mut pending {
            let Some(socket) = slot.as_ref() else { continue };
            if !wait::ready(socket, 2, cancel, deadline)? {
                continue;
            }
            let result = socket.take_error().and_then(|error| match error {
                Some(error) => Err(error),
                None => socket.peer_addr().map(|_| ()),
            });
            match result {
                Ok(()) => return Ok(slot.take().unwrap()),
                Err(error) => {
                    last = error.into();
                    *slot = None;
                    launch = Instant::now();
                }
            }
        }
        if next == addresses.len() && pending.iter().all(Option::is_none) {
            return Err(last);
        }
    }
}
#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd};
    pub fn start(address: SocketAddr) -> Result<(TcpStream, bool), Error> {
        let socket = {
            // Darwin 的 socket+CLOEXEC 非原子，与自有 PTY fork 配合；不持锁执行 connect/poll。
            let _guard = crate::process_launch::guard();
            let domain = if address.is_ipv4() { libc::AF_INET } else { libc::AF_INET6 };
            #[cfg(target_os = "linux")]
            let flags = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
            #[cfg(target_os = "macos")]
            let flags = libc::SOCK_STREAM;
            let fd = unsafe { libc::socket(domain, flags, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error().into());
            }
            // SAFETY: 独占接管刚创建的 TCP socket；所有错误路径由 TcpStream 关闭。
            let socket = unsafe { TcpStream::from_raw_fd(fd) };
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error().into());
            }
            socket.set_nonblocking(true)?;
            socket
        };
        let code = match address {
            SocketAddr::V4(a) => {
                let mut raw: libc::sockaddr_in = unsafe { std::mem::zeroed() };
                raw.sin_family = libc::AF_INET as _;
                raw.sin_port = a.port().to_be();
                raw.sin_addr.s_addr = u32::from_ne_bytes(a.ip().octets());
                #[cfg(target_os = "macos")]
                {
                    raw.sin_len = std::mem::size_of_val(&raw) as u8;
                }
                unsafe {
                    libc::connect(
                        socket.as_raw_fd(),
                        (&raw as *const libc::sockaddr_in).cast(),
                        std::mem::size_of_val(&raw) as libc::socklen_t,
                    )
                }
            }
            SocketAddr::V6(a) => {
                let mut raw: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
                raw.sin6_family = libc::AF_INET6 as _;
                raw.sin6_port = a.port().to_be();
                raw.sin6_flowinfo = a.flowinfo().to_be();
                raw.sin6_scope_id = a.scope_id();
                raw.sin6_addr.s6_addr = a.ip().octets();
                #[cfg(target_os = "macos")]
                {
                    raw.sin6_len = std::mem::size_of_val(&raw) as u8;
                }
                unsafe {
                    libc::connect(
                        socket.as_raw_fd(),
                        (&raw as *const libc::sockaddr_in6).cast(),
                        std::mem::size_of_val(&raw) as libc::socklen_t,
                    )
                }
            }
        };
        if code == 0 {
            return Ok((socket, true));
        }
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::EINPROGRESS | libc::EALREADY | libc::EWOULDBLOCK | libc::EINTR)) {
            Ok((socket, false))
        } else {
            Err(error.into())
        }
    }
}
#[cfg(windows)]
mod platform {
    use super::*;
    use std::{ffi::c_void, os::windows::io::FromRawSocket, sync::OnceLock};
    #[repr(C)]
    struct V4 {
        family: u16,
        port: u16,
        ip: [u8; 4],
        zero: [u8; 8],
    }
    #[repr(C)]
    struct V6 {
        family: u16,
        port: u16,
        flow: u32,
        ip: [u8; 16],
        scope: u32,
    }
    #[repr(align(16))]
    struct Startup([u8; 512]);
    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn WSAStartup(version: u16, data: *mut c_void) -> i32;
        fn WSASocketW(af: i32, kind: i32, protocol: i32, info: *const c_void, group: u32, flags: u32) -> usize;
        fn WSAGetLastError() -> i32;
        fn connect(socket: usize, address: *const c_void, length: i32) -> i32;
    }
    fn last() -> io::Error {
        io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
    }
    pub fn start(address: SocketAddr) -> Result<(TcpStream, bool), Error> {
        static INITIALIZED: OnceLock<Result<(), i32>> = OnceLock::new();
        // Winsock 进程级引用只初始化一次，随进程结束；工作区大于 Win32/64 WSADATA 且正确对齐。
        if let Err(code) = *INITIALIZED.get_or_init(|| {
            let mut data = Startup([0; 512]);
            let code = unsafe { WSAStartup(0x0202, data.0.as_mut_ptr().cast()) };
            if code == 0 { Ok(()) } else { Err(code) }
        }) {
            return Err(io::Error::from_raw_os_error(code).into());
        }
        // OVERLAPPED | NO_HANDLE_INHERIT；与 ConPTY 所需最低系统版本兼容。
        let raw = unsafe { WSASocketW(if address.is_ipv4() { 2 } else { 23 }, 1, 6, std::ptr::null(), 0, 0x81) };
        if raw == usize::MAX {
            return Err(last().into());
        }
        let socket = unsafe { TcpStream::from_raw_socket(raw as _) };
        socket.set_nonblocking(true)?;
        let code = match address {
            SocketAddr::V4(a) => {
                let value = V4 { family: 2, port: a.port().to_be(), ip: a.ip().octets(), zero: [0; 8] };
                unsafe { connect(raw, (&value as *const V4).cast(), std::mem::size_of_val(&value) as i32) }
            }
            SocketAddr::V6(a) => {
                let value = V6 {
                    family: 23,
                    port: a.port().to_be(),
                    flow: a.flowinfo().to_be(),
                    ip: a.ip().octets(),
                    scope: a.scope_id(),
                };
                unsafe { connect(raw, (&value as *const V6).cast(), std::mem::size_of_val(&value) as i32) }
            }
        };
        if code == 0 {
            return Ok((socket, true));
        }
        let error = last();
        if matches!(error.raw_os_error(), Some(10035 | 10036 | 10037 | 10004)) {
            Ok((socket, false))
        } else {
            Err(error.into())
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_cancelled_and_expired_attempts_do_not_allocate_work() {
        let pool = Budget::new(0, 0);
        let token = Cancellation::default();
        token.cancel();
        assert!(matches!(
            addresses_connect(
                &["127.0.0.1:22".parse().unwrap()],
                &pool,
                &token,
                Instant::now() + Duration::from_secs(1)
            ),
            Err(Error::Cancelled)
        ));
        assert!(matches!(addresses_connect(&[], &pool, &Cancellation::default(), Instant::now()), Err(Error::Invalid)));
        assert!(matches!(
            addresses_connect(&["127.0.0.1:22".parse().unwrap()], &pool, &Cancellation::default(), Instant::now()),
            Err(Error::Timeout)
        ));
        assert_eq!(pool.snapshot().denied, 0);
        assert_eq!(pool.snapshot().used, 0);
        assert!(matches!(
            host(
                "127.0.0.1",
                22,
                &pool,
                &Cancellation::default(),
                &mut crate::resolve::Cache::new(&Budget::new(64 * 1024, 0)).unwrap()
            ),
            Err(Error::Budget)
        ));
    }
    #[test]
    #[ignore = "真实回环 IPv4/IPv6 连接、地址回退与端点关闭；由隔离 SFTP 工具运行"]
    fn numeric_connect_handles_dual_stack_refusal_fallback_and_descriptor_flags() {
        use std::net::TcpListener;
        let pool = Budget::new(4096, 0);
        for endpoint in ["127.0.0.1:0", "[::1]:0"] {
            let listener = TcpListener::bind(endpoint).unwrap();
            let address = listener.local_addr().unwrap();
            let token = Cancellation::default();
            let stream = addresses_connect(&[address], &pool, &token, Instant::now() + Duration::from_secs(2)).unwrap();
            let (peer, _) = listener.accept().unwrap();
            assert_eq!(stream.peer_addr().unwrap(), address);
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFD) };
                assert_ne!(flags & libc::FD_CLOEXEC, 0);
            }
            drop((stream, peer, listener));
            assert_eq!(pool.snapshot().used, 0);
        }
        let refused = TcpListener::bind("127.0.0.1:0").unwrap();
        let bad = refused.local_addr().unwrap();
        let good = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = good.local_addr().unwrap();
        assert_ne!(bad, address);
        drop(refused);
        let stream = addresses_connect(
            &[bad, address],
            &pool,
            &Cancellation::default(),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        let (peer, _) = good.accept().unwrap();
        assert_eq!(stream.peer_addr().unwrap(), address);
        drop((stream, peer));
        assert_eq!(pool.snapshot().used, 0);
    }
}
