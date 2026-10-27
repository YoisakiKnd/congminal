//! 受限 libssh2 会话。仅后台单线程持有，C 对象及所有句柄由会话统一回收。
use super::{
    allocation::{NativePool, Stats},
    client::Source,
    native_wait as wait,
    wire::{Attributes, CHUNK_LIMIT, PATH_LIMIT},
};
use crate::{
    resources::{Budget, Kind, Lease},
    tasks::Cancellation,
};
use libssh2_sys as ffi;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawSocket;
use std::{
    ffi::{CStr, CString},
    io,
    net::{Shutdown, TcpStream},
    ptr::{self, NonNull},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const HANDLES: usize = 32;
const RECORD_LIMIT: usize = 1024 * 1024;
const LINE_LIMIT: usize = 16 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Resolution,
    Limit,
    Budget,
    Cancelled,
    Timeout,
    HostUnknown,
    HostChanged,
    HostRecords,
    Authentication,
    Credential,
    FileType,
    FilePermissions,
    FileChanged,
    Io(io::ErrorKind),
    Protocol(i32),
    Remote(u64),
    Closed,
    Handle,
    Capacity,
    Library,
    Cleanup,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SFTP 后端：{self:?}")
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}
/// 内容由调用方的有界、短生命周期所有者提供；会话不保留密钥/口令或主机记录副本。
pub struct Verification<'a> {
    pub host: &'a str,
    pub port: u16,
    pub host_key_alias: Option<&'a str>,
    pub records: &'a [u8],
}
pub struct Credentials<'a> {
    pub user: &'a str,
    pub private_key: &'a [u8],
    pub passphrase: Option<&'a CStr>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handle {
    owner: u64,
    nonce: u64,
    slot: u8,
}
#[derive(Clone, Copy)]
struct Slot {
    pointer: NonNull<ffi::LIBSSH2_SFTP_HANDLE>,
    nonce: u64,
    file: bool,
}
/// 不暴露 C 指针，复制的旧令牌不能访问新会话或已重用的槽。
pub struct Session {
    socket: TcpStream,
    raw: NonNull<ffi::LIBSSH2_SESSION>,
    sftp: Option<NonNull<ffi::LIBSSH2_SFTP>>,
    handles: [Option<Slot>; HANDLES],
    source: Source,
    owner: u64,
    nonce: u64,
    failed: bool,
    closed: bool,
    timeout: Duration,
    allocation: Option<NativePool>,
    _memory: Lease,
}
// libssh2-sys 0.3.3 未声明此公开 API，签名来自 libssh2_sftp.h。
unsafe extern "C" {
    fn libssh2_sftp_get_channel(sftp: *mut ffi::LIBSSH2_SFTP) -> *mut ffi::LIBSSH2_CHANNEL;
}
impl Session {
    /// socket 已由连接监督器建立；本函数不做 DNS 或读取配置/凭据文件。
    pub fn connect(
        socket: TcpStream,
        source: Source,
        verification: Verification<'_>,
        credentials: Credentials<'_>,
        pool: Arc<Budget>,
        cancel: &Cancellation,
    ) -> Result<Self, Error> {
        validate(&verification, &credentials)?;
        wait::check(cancel, Instant::now() + TIMEOUT)?;
        let memory = pool.lease(Kind::Sftp, std::mem::size_of::<Self>() + 1024).map_err(|_| Error::Budget)?;
        let mut allocation = NativePool::new(pool, 8 * 1024 * 1024).map_err(|_| Error::Budget)?;
        // 初始化只发生在显式使用本后端时；绑定负责平台加密库初始化。
        ffi::init();
        socket.set_nonblocking(true)?;
        socket.set_nodelay(true)?;
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let mut current = NEXT.load(Ordering::Relaxed);
        let owner = loop {
            let next = current.checked_add(1).ok_or(Error::Capacity)?;
            match NEXT.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break current,
                Err(actual) => current = actual,
            }
        };
        // SAFETY: allocation 的 State 地址稳定并最后销毁；本对象不可 Send/Sync。
        let callbacks = unsafe { allocation.callbacks() };
        let raw = NonNull::new(unsafe {
            ffi::libssh2_session_init_ex(
                Some(callbacks.allocate),
                Some(callbacks.release),
                Some(callbacks.resize),
                callbacks.context,
            )
        })
        .ok_or(Error::Budget)?;
        let mut session = Self {
            socket,
            raw,
            sftp: None,
            handles: [None; HANDLES],
            source,
            owner,
            nonce: 1,
            failed: false,
            closed: false,
            timeout: TIMEOUT,
            allocation: Some(allocation),
            _memory: memory,
        };
        unsafe {
            ffi::libssh2_session_set_blocking(raw.as_ptr(), 0);
            ffi::libssh2_session_flag(raw.as_ptr(), ffi::LIBSSH2_FLAG_SIGPIPE, 0);
        }
        #[cfg(unix)]
        let descriptor = session.socket.as_raw_fd();
        #[cfg(windows)]
        let descriptor = session.socket.as_raw_socket() as ffi::libssh2_socket_t;
        let result = (|| -> Result<(), Error> {
            session.run(cancel, |_| unsafe { ffi::libssh2_session_handshake(raw.as_ptr(), descriptor) as i64 })?;
            session.verify(verification, cancel)?;
            let username = CString::new(credentials.user).map_err(|_| Error::Invalid)?;
            let private_key = super::secret::Secret::copy(credentials.private_key, session._memory.budget())?;
            let authenticated = session.run(cancel, |_| unsafe {
                ffi::libssh2_userauth_publickey_frommemory(
                    raw.as_ptr(),
                    username.as_ptr(),
                    credentials.user.len(),
                    ptr::null(),
                    0,
                    private_key.as_ptr(),
                    private_key.len(),
                    credentials.passphrase.map_or(ptr::null(), CStr::as_ptr),
                ) as i64
            });
            drop(private_key);
            authenticated.map_err(|error| match error {
                Error::Protocol(ffi::LIBSSH2_ERROR_FILE | ffi::LIBSSH2_ERROR_KEYFILE_AUTH_FAILED) => Error::Credential,
                other => other,
            })?;
            let sftp = session.pointer(cancel, |_| unsafe { ffi::libssh2_sftp_init(raw.as_ptr()) })?;
            session.sftp = Some(sftp);
            Ok(())
        })();
        if let Err(error) = result {
            session.close_native()?;
            return Err(error);
        }
        Ok(session)
    }
    pub fn source(&self) -> Source {
        self.source
    }
    pub fn stats(&self) -> Stats {
        self.allocation.as_ref().unwrap().stats()
    }
    fn verify(&mut self, verification: Verification<'_>, cancel: &Cancellation) -> Result<(), Error> {
        let host =
            CString::new(verification.host_key_alias.unwrap_or(verification.host)).map_err(|_| Error::Invalid)?;
        let raw = self.raw.as_ptr();
        let known = NonNull::new(unsafe { ffi::libssh2_knownhost_init(raw) }).ok_or(Error::Budget)?;
        struct Known(NonNull<ffi::LIBSSH2_KNOWNHOSTS>);
        impl Drop for Known {
            fn drop(&mut self) {
                unsafe {
                    ffi::libssh2_knownhost_free(self.0.as_ptr());
                }
            }
        }
        let known = Known(known);
        let _line_memory = self._memory.budget().lease(Kind::Sftp, LINE_LIMIT + 1).map_err(|_| Error::Budget)?;
        // 此 C API 会查看 len 后的终止字节，不能直接传入不以 NUL 结束的借用切片。
        let mut scratch = Vec::new();
        scratch.try_reserve_exact(LINE_LIMIT + 1).map_err(|_| Error::Budget)?;
        let deadline = Instant::now() + TIMEOUT;
        for (index, line) in verification.records.split(|&b| b == b'\n').enumerate() {
            wait::check(cancel, deadline)?;
            if index >= 8192 || line.len() > LINE_LIMIT {
                return Err(Error::Limit);
            }
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let line = &line[line.iter().position(|b| !matches!(b, b' ' | b'\t')).unwrap_or(line.len())..];
            if line.is_empty() || line.iter().all(u8::is_ascii_whitespace) || line.starts_with(b"#") {
                continue;
            }
            // 不忽略 @revoked / CA 等不支持记录；交由兼容后端或可见确认处理。
            if line.starts_with(b"@") || line.contains(&0) {
                return Err(Error::HostRecords);
            }
            scratch.clear();
            scratch.extend_from_slice(line);
            scratch.push(0);
            let code = unsafe {
                ffi::libssh2_knownhost_readline(
                    known.0.as_ptr(),
                    scratch.as_ptr().cast(),
                    line.len(),
                    ffi::LIBSSH2_KNOWNHOST_FILE_OPENSSH,
                )
            };
            if code < 0 {
                return Err(if self.stats().denied > 0 { Error::Budget } else { Error::HostRecords });
            }
        }
        let (mut length, mut kind) = (0, 0);
        let key = unsafe { ffi::libssh2_session_hostkey(raw, &mut length, &mut kind) };
        if key.is_null() || length == 0 || length > LINE_LIMIT {
            return Err(Error::HostRecords);
        }
        let format = match kind {
            ffi::LIBSSH2_HOSTKEY_TYPE_RSA => ffi::LIBSSH2_KNOWNHOST_KEY_SSHRSA,
            ffi::LIBSSH2_HOSTKEY_TYPE_ECDSA_256 => ffi::LIBSSH2_KNOWNHOST_KEY_ECDSA_256,
            ffi::LIBSSH2_HOSTKEY_TYPE_ECDSA_384 => ffi::LIBSSH2_KNOWNHOST_KEY_ECDSA_384,
            ffi::LIBSSH2_HOSTKEY_TYPE_ECDSA_521 => ffi::LIBSSH2_KNOWNHOST_KEY_ECDSA_521,
            ffi::LIBSSH2_HOSTKEY_TYPE_ED25519 => ffi::LIBSSH2_KNOWNHOST_KEY_ED25519,
            _ => return Err(Error::HostRecords),
        };
        let mask = ffi::LIBSSH2_KNOWNHOST_TYPE_PLAIN | ffi::LIBSSH2_KNOWNHOST_KEYENC_RAW | format;
        let checked = if verification.host_key_alias.is_some() {
            // HostKeyAlias 是完整查找名；OpenSSH 不再给它添加非默认端口。
            unsafe { ffi::libssh2_knownhost_check(known.0.as_ptr(), host.as_ptr(), key, length, mask, ptr::null_mut()) }
        } else {
            unsafe {
                ffi::libssh2_knownhost_checkp(
                    known.0.as_ptr(),
                    host.as_ptr(),
                    verification.port.into(),
                    key,
                    length,
                    mask,
                    ptr::null_mut(),
                )
            }
        };
        match checked {
            ffi::LIBSSH2_KNOWNHOST_CHECK_MATCH => Ok(()),
            ffi::LIBSSH2_KNOWNHOST_CHECK_NOTFOUND => Err(Error::HostUnknown),
            ffi::LIBSSH2_KNOWNHOST_CHECK_MISMATCH => Err(Error::HostChanged),
            _ => Err(Error::HostRecords),
        }
    }
    fn code(&self, code: i32) -> Error {
        match code {
            ffi::LIBSSH2_ERROR_ALLOC => Error::Budget,
            ffi::LIBSSH2_ERROR_AUTHENTICATION_FAILED | ffi::LIBSSH2_ERROR_PUBLICKEY_UNVERIFIED => Error::Authentication,
            ffi::LIBSSH2_ERROR_KEYFILE_AUTH_FAILED => Error::Credential,
            ffi::LIBSSH2_ERROR_SOCKET_DISCONNECT | ffi::LIBSSH2_ERROR_CHANNEL_CLOSED => Error::Closed,
            ffi::LIBSSH2_ERROR_SFTP_PROTOCOL => self.sftp.map_or(Error::Protocol(code), |p| {
                let status = unsafe { ffi::libssh2_sftp_last_error(p.as_ptr()) as u64 };
                if status == 0 { Error::Protocol(code) } else { Error::Remote(status) }
            }),
            other => Error::Protocol(other),
        }
    }
    fn pending(&mut self, cancel: &Cancellation, deadline: Instant) -> Result<(), Error> {
        if let Some(sftp) = self.sftp {
            let channel = unsafe { libssh2_sftp_get_channel(sftp.as_ptr()) };
            if channel.is_null() || unsafe { ffi::libssh2_channel_eof(channel) } != 0 {
                return Err(Error::Closed);
            }
        }
        let directions = unsafe { ffi::libssh2_session_block_directions(self.raw.as_ptr()) };
        wait::ready(&self.socket, directions, cancel, deadline).map(|_| ())
    }
    fn run(&mut self, cancel: &Cancellation, mut call: impl FnMut(&Self) -> i64) -> Result<i64, Error> {
        let deadline = Instant::now() + self.timeout;
        let result = (|| {
            loop {
                if self.failed || self.closed {
                    return Err(Error::Closed);
                }
                wait::check(cancel, deadline)?;
                let code = call(self);
                if code >= 0 {
                    return Ok(code);
                }
                if code != i64::from(ffi::LIBSSH2_ERROR_EAGAIN) {
                    return Err(self.code(code as i32));
                }
                self.pending(cancel, deadline)?;
            }
        })();
        if matches!(result, Err(Error::Cancelled | Error::Timeout | Error::Closed | Error::Budget | Error::Io(_)))
            || matches!(result, Err(Error::Protocol(code)) if code!=ffi::LIBSSH2_ERROR_BUFFER_TOO_SMALL)
        {
            self.failed = true;
        }
        result
    }
    fn pointer<T>(
        &mut self,
        cancel: &Cancellation,
        mut call: impl FnMut(&Self) -> *mut T,
    ) -> Result<NonNull<T>, Error> {
        let mut pointer = ptr::null_mut();
        self.run(cancel, |this| {
            pointer = call(this);
            if pointer.is_null() { unsafe { ffi::libssh2_session_last_errno(this.raw.as_ptr()) as i64 } } else { 0 }
        })?;
        NonNull::new(pointer).ok_or(Error::Library)
    }
    fn slot(&self, handle: Handle) -> Result<NonNull<ffi::LIBSSH2_SFTP_HANDLE>, Error> {
        if handle.owner != self.owner {
            return Err(Error::Handle);
        }
        self.handles
            .get(handle.slot as usize)
            .and_then(|s| *s)
            .filter(|s| s.nonce == handle.nonce)
            .map(|s| s.pointer)
            .ok_or(Error::Handle)
    }
    fn slot_kind(&self, handle: Handle, file: bool) -> Result<NonNull<ffi::LIBSSH2_SFTP_HANDLE>, Error> {
        let pointer = self.slot(handle)?;
        if self.handles[handle.slot as usize].is_some_and(|s| s.file == file) {
            Ok(pointer)
        } else {
            Err(Error::Handle)
        }
    }
    fn open(&mut self, path: &[u8], flags: u32, directory: bool, cancel: &Cancellation) -> Result<Handle, Error> {
        valid_path(path)?;
        if !directory && (flags & !0x3f != 0 || flags & 3 == 0) {
            return Err(Error::Invalid);
        }
        // 一个连接只保留一个文件流的预取，目录句柄仍可独立持有。
        if !directory && self.handles.iter().flatten().any(|s| s.file) {
            return Err(Error::Capacity);
        }
        let index = self.handles.iter().position(Option::is_none).ok_or(Error::Capacity)?;
        let nonce = self.nonce;
        self.nonce = self.nonce.checked_add(1).ok_or(Error::Capacity)?;
        let sftp = self.sftp.ok_or(Error::Closed)?.as_ptr();
        let pointer = self.pointer(cancel, |_| unsafe {
            ffi::libssh2_sftp_open_ex(
                sftp,
                path.as_ptr().cast(),
                path.len() as u32,
                flags.into(),
                0o600,
                if directory { ffi::LIBSSH2_SFTP_OPENDIR } else { ffi::LIBSSH2_SFTP_OPENFILE },
            )
        })?;
        self.handles[index] = Some(Slot { pointer, nonce, file: !directory });
        Ok(Handle { owner: self.owner, nonce, slot: index as u8 })
    }
    pub fn listing(
        &mut self,
        path: super::model::RemotePath,
        hidden: bool,
        cancel: &Cancellation,
    ) -> Result<super::model::Listing, Error> {
        let model_error = |error| match error {
            super::model::Error::Budget => Error::Budget,
            super::model::Error::Cancelled => Error::Cancelled,
            _ => Error::Limit,
        };
        // 原生（libssh2）后端还没接排序选项：目录读取始终按名字排，界面上的排序只影响 OpenSSH 后端。
        let mut scan = super::model::Scan::new(
            self.source.session,
            self.source.generation,
            path.clone(),
            hidden,
            super::model::Sort::Name,
        )
        .map_err(model_error)?;
        let handle = self.open_dir(path.bytes(), cancel)?;
        let result = (|| {
            let mut name = [0u8; 1025];
            let eof = loop {
                match self.read_dir(handle, &mut name, cancel) {
                    Ok(None) => break true,
                    Ok(Some((length, attrs))) => {
                        if scan.push_entry(&name[..length], attrs, cancel).map_err(model_error)? {
                            break false;
                        }
                    }
                    Err(Error::Protocol(ffi::LIBSSH2_ERROR_BUFFER_TOO_SMALL)) => {
                        scan.truncate();
                        break false;
                    }
                    Err(error) => return Err(error),
                }
            };
            scan.finish(eof).map_err(model_error)
        })();
        let closed = self.close(handle, cancel);
        match result {
            Ok(listing) => {
                closed?;
                Ok(listing)
            }
            Err(error) => Err(error),
        }
    }
    pub fn open_file(&mut self, path: &[u8], flags: u32, cancel: &Cancellation) -> Result<Handle, Error> {
        self.open(path, flags, false, cancel)
    }
    pub fn open_dir(&mut self, path: &[u8], cancel: &Cancellation) -> Result<Handle, Error> {
        self.open(path, 0, true, cancel)
    }
    pub fn close(&mut self, handle: Handle, cancel: &Cancellation) -> Result<(), Error> {
        let pointer = self.slot(handle)?;
        // 原生 close 除 EAGAIN 外都会销毁句柄，即使最终状态是错误。
        let mut completed = false;
        let result = self.run(cancel, |_| {
            let code = unsafe { ffi::libssh2_sftp_close_handle(pointer.as_ptr()) };
            if code != ffi::LIBSSH2_ERROR_EAGAIN {
                completed = true;
            }
            code.into()
        });
        if completed {
            self.handles[handle.slot as usize] = None;
        }
        result.map(|_| ())
    }
    pub fn attrs(&mut self, handle: Handle, cancel: &Cancellation) -> Result<Attributes, Error> {
        let pointer = self.slot(handle)?;
        let mut attrs = empty_attrs();
        self.run(cancel, |_| unsafe { ffi::libssh2_sftp_fstat_ex(pointer.as_ptr(), &mut attrs, 0) as i64 })?;
        attributes(attrs)
    }
    pub fn read(
        &mut self,
        handle: Handle,
        offset: u64,
        buffer: &mut [u8],
        cancel: &Cancellation,
    ) -> Result<usize, Error> {
        if buffer.is_empty() || buffer.len() > CHUNK_LIMIT || offset.checked_add(buffer.len() as u64).is_none() {
            return Err(Error::Limit);
        }
        let pointer = self.slot_kind(handle, true)?;
        unsafe {
            if ffi::libssh2_sftp_tell64(pointer.as_ptr()) != offset {
                ffi::libssh2_sftp_seek64(pointer.as_ptr(), offset);
            }
        }
        self.run(cancel, |_| unsafe {
            ffi::libssh2_sftp_read(pointer.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len()) as i64
        })
        .map(|n| n as usize)
    }
    pub fn write(&mut self, handle: Handle, offset: u64, buffer: &[u8], cancel: &Cancellation) -> Result<usize, Error> {
        if buffer.is_empty() || buffer.len() > CHUNK_LIMIT || offset.checked_add(buffer.len() as u64).is_none() {
            return Err(Error::Limit);
        }
        let pointer = self.slot_kind(handle, true)?;
        unsafe {
            if ffi::libssh2_sftp_tell64(pointer.as_ptr()) != offset {
                ffi::libssh2_sftp_seek64(pointer.as_ptr(), offset);
            }
        }
        let n = self.run(cancel, |_| unsafe {
            ffi::libssh2_sftp_write(pointer.as_ptr(), buffer.as_ptr().cast(), buffer.len()) as i64
        })? as usize;
        if n == 0 {
            self.failed = true;
            Err(Error::Closed)
        } else {
            Ok(n)
        }
    }
    pub fn read_dir(
        &mut self,
        handle: Handle,
        buffer: &mut [u8; 1025],
        cancel: &Cancellation,
    ) -> Result<Option<(usize, Attributes)>, Error> {
        let pointer = self.slot_kind(handle, false)?;
        let mut attrs = empty_attrs();
        let n = self.run(cancel, |_| unsafe {
            ffi::libssh2_sftp_readdir_ex(
                pointer.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                ptr::null_mut(),
                0,
                &mut attrs,
            ) as i64
        })? as usize;
        if n == 0 {
            Ok(None)
        } else if n >= buffer.len() {
            self.failed = true;
            Err(Error::Limit)
        } else {
            Ok(Some((n, attributes(attrs)?)))
        }
    }
    pub fn fsync(&mut self, handle: Handle, cancel: &Cancellation) -> Result<(), Error> {
        let p = self.slot_kind(handle, true)?;
        self.run(cancel, |_| unsafe { ffi::libssh2_sftp_fsync(p.as_ptr()) as i64 }).map(|_| ())
    }
    pub fn rename(&mut self, old: &[u8], new: &[u8], cancel: &Cancellation) -> Result<(), Error> {
        valid_path(old)?;
        valid_path(new)?;
        let p = self.sftp.ok_or(Error::Closed)?.as_ptr();
        self.run(cancel, |_| unsafe {
            ffi::libssh2_sftp_rename_ex(
                p,
                old.as_ptr().cast(),
                old.len() as u32,
                new.as_ptr().cast(),
                new.len() as u32,
                0,
            ) as i64
        })
        .map(|_| ())
    }
    pub fn remove(&mut self, path: &[u8], cancel: &Cancellation) -> Result<(), Error> {
        valid_path(path)?;
        let p = self.sftp.ok_or(Error::Closed)?.as_ptr();
        self.run(cancel, |_| unsafe { ffi::libssh2_sftp_unlink_ex(p, path.as_ptr().cast(), path.len() as u32) as i64 })
            .map(|_| ())
    }
}
impl Session {
    /// 工作任务显式结束会话，清理异常不能报告为已释放或传输完成。
    pub fn finish(mut self) -> Result<Stats, Error> {
        self.close_native()
    }
    fn close_native(&mut self) -> Result<Stats, Error> {
        if self.closed {
            return self.allocation.as_ref().map(NativePool::stats).ok_or(Error::Cleanup);
        }
        self.closed = true;
        let _ = self.socket.shutdown(Shutdown::Both);
        // shutdown 后不再等待远端确认；仍必须按顺序销毁本地 C 对象。
        for slot in &mut self.handles {
            if let Some(handle) = slot.take() {
                for _ in 0..16 {
                    if unsafe { ffi::libssh2_sftp_close_handle(handle.pointer.as_ptr()) } != ffi::LIBSSH2_ERROR_EAGAIN {
                        break;
                    }
                }
            }
        }
        if let Some(sftp) = self.sftp.take() {
            for _ in 0..16 {
                if unsafe { ffi::libssh2_sftp_shutdown(sftp.as_ptr()) } != ffi::LIBSSH2_ERROR_EAGAIN {
                    break;
                }
            }
        }
        for _ in 0..16 {
            if unsafe { ffi::libssh2_session_free(self.raw.as_ptr()) } != ffi::LIBSSH2_ERROR_EAGAIN {
                break;
            }
        }
        if self.allocation.as_ref().is_some_and(|p| p.stats().used != 0) {
            // 若 C 库无法回收，保留其回调上下文和已收费额度，绝不释放后继续回调或错误归还。
            // 每块仍占共享额度，后续准入会拒绝；原生故障验收要求此分支不发生。
            std::mem::forget(self.allocation.take());
            return Err(Error::Cleanup);
        }
        Ok(self.stats())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.close_native();
    }
}

fn validate(v: &Verification<'_>, c: &Credentials<'_>) -> Result<(), Error> {
    let text = |s: &str, max| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control);
    if !text(v.host, 255)
        || v.host.starts_with('-')
        || v.host.contains('\0')
        || v.port == 0
        || v.host_key_alias.is_some_and(|s| !text(s, 255) || s.chars().any(char::is_whitespace))
        || v.records.len() > RECORD_LIMIT
        || !text(c.user, 128)
        || c.private_key.is_empty()
        || c.private_key.len() > RECORD_LIMIT
        || c.passphrase.is_some_and(|s| s.to_bytes().len() > 4096)
    {
        return Err(Error::Invalid);
    }
    Ok(())
}
fn valid_path(path: &[u8]) -> Result<(), Error> {
    if path.is_empty() || path.len() > PATH_LIMIT || path.contains(&0) { Err(Error::Invalid) } else { Ok(()) }
}
fn empty_attrs() -> ffi::LIBSSH2_SFTP_ATTRIBUTES {
    ffi::LIBSSH2_SFTP_ATTRIBUTES { flags: 0, filesize: 0, uid: 0, gid: 0, permissions: 0, atime: 0, mtime: 0 }
}
fn attributes(a: ffi::LIBSSH2_SFTP_ATTRIBUTES) -> Result<Attributes, Error> {
    let integer = |n| u32::try_from(n).map_err(|_| Error::Limit);
    Ok(Attributes {
        size: (a.flags & ffi::LIBSSH2_SFTP_ATTR_SIZE != 0).then_some(a.filesize),
        owner: if a.flags & ffi::LIBSSH2_SFTP_ATTR_UIDGID != 0 {
            Some((integer(a.uid)?, integer(a.gid)?))
        } else {
            None
        },
        permissions: if a.flags & ffi::LIBSSH2_SFTP_ATTR_PERMISSIONS != 0 {
            Some(integer(a.permissions)?)
        } else {
            None
        },
        times: if a.flags & ffi::LIBSSH2_SFTP_ATTR_ACMODTIME != 0 {
            Some((integer(a.atime)?, integer(a.mtime)?))
        } else {
            None
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preflight_rejects_invalid_targets_and_credentials_without_io() {
        let verification = Verification { host: "host", port: 22, host_key_alias: None, records: b"" };
        let credentials = Credentials { user: "user", private_key: b"key", passphrase: None };
        assert_eq!(validate(&verification, &credentials), Ok(()));
        assert_eq!(validate(&Verification { host: "-option", ..verification }, &credentials), Err(Error::Invalid));
        assert_eq!(
            validate(&Verification { host_key_alias: Some("bad alias"), ..verification }, &credentials),
            Err(Error::Invalid)
        );
        assert_eq!(validate(&verification, &Credentials { user: "bad\0user", ..credentials }), Err(Error::Invalid));
        assert_eq!(valid_path(b"- space\n\xff"), Ok(()));
        assert_eq!(valid_path(b"bad\0path"), Err(Error::Invalid));
        let cancel = Cancellation::default();
        cancel.cancel();
        assert_eq!(wait::check(&cancel, Instant::now() + TIMEOUT), Err(Error::Cancelled));
        assert_eq!(wait::check(&Cancellation::default(), Instant::now()), Err(Error::Timeout));
    }
    #[test]
    fn absent_attributes_stay_unknown_and_present_values_do_not_wrap() {
        assert_eq!(attributes(empty_attrs()).unwrap(), Attributes::default());
        let mut raw = empty_attrs();
        raw.flags = ffi::LIBSSH2_SFTP_ATTR_SIZE
            | ffi::LIBSSH2_SFTP_ATTR_UIDGID
            | ffi::LIBSSH2_SFTP_ATTR_PERMISSIONS
            | ffi::LIBSSH2_SFTP_ATTR_ACMODTIME;
        raw.filesize = u64::MAX;
        raw.uid = 42;
        raw.gid = 43;
        raw.permissions = 0o100600;
        raw.atime = 1;
        raw.mtime = 2;
        assert_eq!(
            attributes(raw).unwrap(),
            Attributes {
                size: Some(u64::MAX),
                owner: Some((42, 43)),
                permissions: Some(0o100600),
                times: Some((1, 2))
            }
        );
        if std::mem::size_of::<std::ffi::c_ulong>() > 4 {
            raw.uid = std::ffi::c_ulong::MAX;
            assert_eq!(attributes(raw), Err(Error::Limit));
        }
    }
    struct Fixture {
        root: std::path::PathBuf,
        key: super::super::files::Blob,
        known: super::super::files::Blob,
        changed: super::super::files::Blob,
        wrong: super::super::files::Blob,
        user: String,
        port: u16,
        host: String,
        host_key_alias: Option<String>,
        resolver: std::cell::RefCell<crate::resolve::Cache>,
        phrase: Option<CString>,
        _memory: Lease,
    }
    impl Fixture {
        fn load(pool: &Arc<Budget>) -> Self {
            let root = std::path::PathBuf::from(
                std::env::var_os("CONGMINAL_TEST_SFTP_ROOT").expect("需 tools/check_sftp_transport.py 隔离环境"),
            );
            let memory = pool.lease(Kind::Sftp, 4 * 1024 * 1024).unwrap();
            let mode = std::env::var("CONGMINAL_TEST_SFTP_SCENARIO").unwrap_or_default();
            let encrypted = matches!(mode.as_str(), "encrypted" | "bad-passphrase");
            use super::super::files::{Blob, Purpose};
            let token = Cancellation::default();
            let queried = if matches!(mode.as_str(), "paths" | "host-alias") {
                let profile = crate::config::profiles::Ssh {
                    host: "bounded-path-alias".into(),
                    user: None,
                    port: None,
                    identity_file: None,
                    jump: None,
                    forwards: Vec::new(),
                    program: Some("/usr/bin/ssh".into()),
                };
                Some(
                    crate::ssh::settings::query(
                        &profile,
                        crate::ssh::settings::Configuration::File(&root.join("path oracle")),
                        pool,
                        &token,
                    )
                    .unwrap(),
                )
            } else {
                None
            };
            let (user, port, host, host_key_alias) = if let Some(settings) = &queried {
                let endpoint = settings.endpoint().unwrap();
                (
                    endpoint.user.to_string(),
                    endpoint.port,
                    endpoint.host.to_string(),
                    endpoint.host_key_alias.map(str::to_string),
                )
            } else {
                (
                    std::env::var("CONGMINAL_TEST_SFTP_USER").unwrap(),
                    std::env::var("CONGMINAL_TEST_SFTP_PORT").unwrap().parse().unwrap(),
                    std::env::var("CONGMINAL_TEST_SFTP_NAME").unwrap_or_else(|_| "127.0.0.1".into()),
                    None,
                )
            };
            let context = crate::ssh::paths::Context {
                home: &root,
                host: &host,
                original: if matches!(mode.as_str(), "paths" | "host-alias") {
                    "bounded-path-alias"
                } else {
                    "fixture-alias"
                },
                user: &user,
                port,
                host_key_alias: host_key_alias.as_deref(),
            };
            let read = |name: &str, purpose| {
                let path = crate::ssh::paths::expand(name, context, |_| None, pool, &token).unwrap();
                Blob::read(&path, purpose, pool, &token).unwrap()
            };
            let key_path = queried
                .as_ref()
                .map(|s| s.single("identityfile").unwrap().unwrap())
                .unwrap_or(if encrypted { "~/encrypted" } else { "~/client" });
            let known_path = queried
                .as_ref()
                .map(|s| s.host_file(crate::ssh::settings::HostFile::User).unwrap().unwrap())
                .unwrap_or("~/known");
            let key = read(key_path, Purpose::PrivateKey);
            let known = read(known_path, Purpose::HostRecords);
            let changed = read("~/changed", Purpose::HostRecords);
            let wrong = read("~/wrong", Purpose::PrivateKey);
            Self {
                key,
                known,
                changed,
                wrong,
                root,
                user,
                port,
                host,
                host_key_alias,
                resolver: std::cell::RefCell::new(crate::resolve::Cache::new(pool).unwrap()),
                phrase: encrypted.then(|| {
                    CString::new(if mode == "bad-passphrase" { "wrong fixture" } else { "fixture only" }).unwrap()
                }),
                _memory: memory,
            }
        }
        fn connect(
            &self,
            pool: &Arc<Budget>,
            cancel: &Cancellation,
            records: &[u8],
            key: &[u8],
        ) -> Result<Session, Error> {
            let socket =
                super::super::connect::host(&self.host, self.port, pool, cancel, &mut self.resolver.borrow_mut())?;
            Session::connect(
                socket,
                Source { session: 87, generation: 4 },
                Verification {
                    host: &self.host,
                    port: self.port,
                    host_key_alias: self.host_key_alias.as_deref(),
                    records,
                },
                Credentials { user: &self.user, private_key: key, passphrase: self.phrase.as_deref() },
                pool.clone(),
                cancel,
            )
        }
        fn path(&self, name: &str) -> Vec<u8> {
            self.root.join("remote").join(name).to_str().unwrap().as_bytes().to_vec()
        }
    }
    #[test]
    #[ignore = "原生库真实隔离 SSH/SFTP、仅临时密钥和文件；由 tools/check_sftp_transport.py 调用"]
    fn native_isolated_session_validates_hosts_handles_streams_and_cleanup() {
        let pool = Budget::new(8 * 1024 * 1024, 0);
        let fixture = Fixture::load(&pool);
        let baseline = pool.snapshot().used;
        let cancel = Cancellation::default();
        let mode = std::env::var("CONGMINAL_TEST_SFTP_SCENARIO").unwrap_or_else(|_| "positive".into());
        if mode == "host-alias" {
            let error = fixture.connect(&pool, &cancel, &fixture.changed, &fixture.key).err().unwrap();
            assert_eq!(error, Error::HostChanged);
            assert_eq!(pool.snapshot().used, baseline);
        }
        if matches!(mode.as_str(), "unknown" | "changed" | "revoked" | "auth" | "bad-passphrase") {
            let mut revoked = b" \t@revoked ".to_vec();
            revoked.extend_from_slice(&fixture.known);
            let records = match mode.as_str() {
                "unknown" => &[][..],
                "changed" => &fixture.changed,
                "revoked" => &revoked,
                _ => &fixture.known,
            };
            let key = if mode == "auth" { &fixture.wrong } else { &fixture.key };
            let error = fixture.connect(&pool, &cancel, records, key).err().unwrap();
            let expected = match mode.as_str() {
                "unknown" => Error::HostUnknown,
                "changed" => Error::HostChanged,
                "revoked" => Error::HostRecords,
                "bad-passphrase" => Error::Credential,
                _ => Error::Authentication,
            };
            assert_eq!(error, expected);
            assert_eq!(pool.snapshot().used, baseline);
            drop(fixture);
            assert_eq!(pool.snapshot().used, 0);
            eprintln!("SFTP_NATIVE_REJECTED scenario={mode} error={error:?} used=0");
            return;
        }
        let mut session = fixture.connect(&pool, &cancel, &fixture.known, &fixture.key).unwrap();
        assert_eq!(session.source(), Source { session: 87, generation: 4 });
        if matches!(mode.as_str(), "huge" | "name" | "stall" | "close" | "timeout") {
            let dir = session.open_dir(b"test", &cancel).unwrap();
            let token = cancel.clone();
            let thread = if mode == "stall" {
                Some(std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(50));
                    token.cancel();
                }))
            } else {
                None
            };
            if mode == "timeout" {
                session.timeout = Duration::from_millis(50);
            }
            let started = Instant::now();
            let error = session.read_dir(dir, &mut [0; 1025], &cancel).unwrap_err();
            let expected = match mode.as_str() {
                "huge" => Error::Budget,
                "name" => Error::Protocol(ffi::LIBSSH2_ERROR_BUFFER_TOO_SMALL),
                "stall" => Error::Cancelled,
                "timeout" => Error::Timeout,
                _ => Error::Closed,
            };
            assert_eq!(error, expected);
            if mode == "huge" {
                assert!(session.stats().denied > 0);
            }
            if let Some(thread) = thread {
                thread.join().unwrap();
            }
            assert_eq!(session.finish().unwrap().used, 0);
            assert_eq!(pool.snapshot().used, baseline);
            drop(fixture);
            assert_eq!(pool.snapshot().used, 0);
            eprintln!(
                "SFTP_NATIVE_FAULT scenario={mode} error={error:?} elapsed_us={} used=0",
                started.elapsed().as_micros()
            );
            return;
        }
        let listpath = super::super::model::RemotePath::new(&pool, &fixture.path("list")).unwrap();
        let listing = session.listing(listpath, false, &cancel).unwrap();
        assert_eq!(listing.entries.len(), 2);
        assert!(!listing.incomplete);
        assert_eq!(listing.entries[0].kind, super::super::model::FileKind::Directory);
        drop(listing);
        let dir = session.open_dir(&fixture.path("list"), &cancel).unwrap();
        assert_eq!(session.read(dir, 0, &mut [0; 1], &cancel), Err(Error::Handle));
        assert_eq!(session.write(dir, 0, b"x", &cancel), Err(Error::Handle));
        assert_eq!(session.fsync(dir, &cancel), Err(Error::Handle));
        session.close(dir, &cancel).unwrap();
        let fresh = session.open_dir(&fixture.path("list"), &cancel).unwrap();
        assert_eq!(session.close(dir, &cancel), Err(Error::Handle));
        let records = fixture.known.strip_suffix(b"\n").unwrap();
        let mut padded = b"# generated fixture\r\n\t".to_vec();
        padded.extend_from_slice(records);
        let mut other = fixture.connect(&pool, &cancel, &padded, &fixture.key).unwrap();
        assert_eq!(other.attrs(fresh, &cancel), Err(Error::Handle));
        assert_eq!(other.finish().unwrap().used, 0);
        let tiny = Budget::new(0, 0);
        assert!(matches!(fixture.connect(&tiny, &cancel, &fixture.known, &fixture.key), Err(Error::Budget)));
        assert_eq!(tiny.snapshot().used, 0);
        assert!(session.read_dir(fresh, &mut [0; 1025], &cancel).unwrap().is_some());
        session.close(fresh, &cancel).unwrap();
        let temporary = fixture.path("- 中文\n.part");
        let destination = fixture.path("- 中文\n.bin");
        let handle = session.open_file(&temporary, 2 | 8 | 32, &cancel).unwrap();
        assert!(matches!(session.open_file(&temporary, 1, &cancel), Err(Error::Capacity)));
        assert!(matches!(session.read_dir(handle, &mut [0; 1025], &cancel), Err(Error::Handle)));
        let _buffer_memory = pool.lease(Kind::Sftp, CHUNK_LIMIT).unwrap();
        let mut buffer = [0u8; CHUNK_LIMIT];
        let length = 32 * 1024 * 1024u64;
        fn byte(at: u64) -> u8 {
            (at ^ (at >> 13) ^ (at >> 23)) as u8
        }
        let started = Instant::now();
        for block in (0..length).step_by(CHUNK_LIMIT) {
            for (i, c) in buffer.iter_mut().enumerate() {
                *c = byte(block + i as u64);
            }
            let mut written = 0;
            while written < buffer.len() {
                written += session.write(handle, block + written as u64, &buffer[written..], &cancel).unwrap();
            }
        }
        assert_eq!(session.attrs(handle, &cancel).unwrap().size, Some(length));
        session.fsync(handle, &cancel).unwrap();
        session.close(handle, &cancel).unwrap();
        session.rename(&temporary, &destination, &cancel).unwrap();
        let upload = started.elapsed();
        let handle = session.open_file(&destination, 1, &cancel).unwrap();
        let mut offset = 0;
        let started = Instant::now();
        while offset < length {
            let n = session.read(handle, offset, &mut buffer, &cancel).unwrap();
            assert!(n > 0);
            for (i, &c) in buffer[..n].iter().enumerate() {
                assert_eq!(c, byte(offset + i as u64));
            }
            offset += n as u64;
        }
        assert_eq!(session.read(handle, offset, &mut buffer[..1], &cancel).unwrap(), 0);
        assert_eq!(session.attrs(handle, &cancel).unwrap().size, Some(length));
        session.close(handle, &cancel).unwrap();
        let download = started.elapsed();
        let collision = session.open_file(&temporary, 2 | 8 | 32, &cancel).unwrap();
        assert_eq!(session.write(collision, 0, b"new", &cancel).unwrap(), 3);
        session.close(collision, &cancel).unwrap();
        assert!(matches!(session.rename(&temporary,&destination,&cancel),Err(Error::Remote(code)) if code!=0));
        assert_eq!(
            std::fs::metadata(std::path::Path::new(std::str::from_utf8(&destination).unwrap())).unwrap().len(),
            length
        );
        session.remove(&temporary, &cancel).unwrap();
        session.remove(&destination, &cancel).unwrap();
        let stats = session.finish().unwrap();
        assert_eq!(stats.used, 0);
        assert_eq!(stats.allocations, stats.releases);
        drop(_buffer_memory);
        assert_eq!(pool.snapshot().used, baseline);
        drop(fixture);
        assert_eq!(pool.snapshot().used, 0);
        eprintln!(
            "SFTP_NATIVE_STREAM bytes={length} upload_us={} download_us={} native_peak={} shared_peak={} final=0",
            upload.as_micros(),
            download.as_micros(),
            stats.peak,
            pool.snapshot().peak
        );
    }
}
