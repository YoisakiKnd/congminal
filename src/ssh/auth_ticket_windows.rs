//! Windows 本机消息管道；固定单个监听实例和至多两个活动客户端，无专用线程。
//! 创建时私有 DACL，拒绝远程客户端，首实例排他；helper 核对票据目录中的服务 PID。
use super::{Record, SIZE, Ticket, invalid, validate_path};
use crate::{
    resources::{Budget, Kind, Lease},
    tasks::Cancellation,
};
use std::{
    cell::RefCell,
    ffi::c_void,
    fs::File,
    io::{self, Read, Write},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::Arc,
};
#[path = "windows_private.rs"]
pub mod private;
use private::{Attributes, Descriptor};
type Handle = *mut c_void;
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateNamedPipeW(
        name: *const u16,
        access: u32,
        mode: u32,
        maximum: u32,
        out_size: u32,
        in_size: u32,
        timeout: u32,
        attributes: *const c_void,
    ) -> Handle;
    fn ConnectNamedPipe(pipe: Handle, overlapped: *mut c_void) -> i32;
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        attributes: *const c_void,
        creation: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn SetNamedPipeHandleState(pipe: Handle, mode: *const u32, count: *const u32, timeout: *const u32) -> i32;
    fn GetNamedPipeServerProcessId(pipe: Handle, pid: *mut u32) -> i32;
    fn ReadFile(file: Handle, data: *mut c_void, size: u32, count: *mut u32, overlapped: *mut c_void) -> i32;
    fn WriteFile(file: Handle, data: *const c_void, size: u32, count: *mut u32, overlapped: *mut c_void) -> i32;
    fn GetTempPathW(size: u32, buffer: *mut u16) -> u32;
}
pub fn temporary() -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let mut buffer = [0u16; 4097];
    let size = unsafe { GetTempPathW(buffer.len() as u32, buffer.as_mut_ptr()) } as usize;
    if size == 0 {
        return Err(io::Error::last_os_error());
    }
    if size >= buffer.len() {
        return Err(invalid());
    }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..size])))
}
fn would_block() -> io::Error {
    io::ErrorKind::WouldBlock.into()
}
fn local_path(path: &Path) -> io::Result<()> {
    validate_path(path)?;
    match path.components().next() {
        Some(std::path::Component::Prefix(prefix))
            if matches!(prefix.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)) =>
        {
            Ok(())
        }
        _ => Err(invalid()), // 票据和能力值不能落到 UNC 共享或设备命名空间。
    }
}
fn pipe_name(path: &Path) -> io::Result<([u16; 128], u32)> {
    let name = path.parent().and_then(Path::file_name).and_then(|n| n.to_str()).ok_or_else(invalid)?;
    let tail = name.strip_prefix("cgaw-").ok_or_else(invalid)?;
    let (pid, random) = tail.split_once('-').ok_or_else(invalid)?;
    let pid = pid.parse::<u32>().map_err(|_| invalid())?;
    if pid == 0 || random.len() != 32 || !random.bytes().all(|b| b.is_ascii_hexdigit()) || name.len() > 64 {
        return Err(invalid());
    }
    let mut value = [0u16; 128];
    for (at, unit) in "\\\\.\\pipe\\congminal-".encode_utf16().chain(name.encode_utf16()).enumerate() {
        if at + 1 >= value.len() {
            return Err(invalid());
        }
        value[at] = unit;
    }
    Ok((value, pid))
}
pub fn read_private(path: &Path) -> io::Result<Ticket> {
    local_path(path)?;
    if path.file_name().is_none_or(|n| n != "ticket") {
        return Err(invalid());
    }
    pipe_name(path)?;
    let parent = private::open_existing(path.parent().ok_or_else(invalid)?, 0x80, 3, true)?;
    private::validate(&parent, true)?;
    let mut file = private::open_existing(path, 0x8000_0000, 1, false)?;
    private::validate(&file, false)?;
    let before = file.metadata()?;
    if before.len() != SIZE as u64 {
        return Err(invalid());
    }
    let mut record = Record([0; SIZE]);
    file.read_exact(&mut record.0)?;
    if file.read(&mut [0; 1])? != 0 || before.modified()? != file.metadata()?.modified()? {
        return Err(invalid());
    }
    Ticket::decode(&record)
}
pub struct Stream {
    handle: OwnedHandle,
    message_end: bool,
    request_ended: bool,
    server: bool,
}
impl Stream {
    fn raw(&self) -> Handle {
        self.handle.as_raw_handle()
    }
    pub fn set_nonblocking(&self, _value: bool) -> io::Result<()> {
        Ok(())
    }
    pub fn connect(path: &Path) -> io::Result<Self> {
        let (name, expected) = pipe_name(path)?;
        // SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION：服务端不能借客户端令牌冒充用户。
        let handle = unsafe { CreateFileW(name.as_ptr(), 0xc000_0000, 0, null(), 3, 0x0011_0000, null_mut()) };
        if handle as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut pid = 0;
        if unsafe { GetNamedPipeServerProcessId(handle.as_raw_handle(), &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if pid != expected {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        // 客户端按字节读答案，服务端按完整消息验证唯一请求。
        if unsafe { SetNamedPipeHandleState(handle.as_raw_handle(), &1, null(), null()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { handle, message_end: false, request_ended: false, server: false })
    }
    pub fn acknowledged(&mut self) -> io::Result<bool> {
        let mut byte = [0u8; 2];
        // 请求的消息边界已消费；后继消息只能是单字节确认。
        self.message_end = false;
        match self.read(&mut byte) {
            Ok(1) if byte[0] == 0xa5 && self.message_end => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Ok(_) => Err(invalid()),
            Err(e) => Err(e),
        }
    }
    pub fn acknowledge(&mut self) -> io::Result<()> {
        self.write_all(&[0xa5])
    }
}
impl Read for Stream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.server && self.message_end && !self.request_ended {
            self.request_ended = true;
            return Ok(0); // 完整消息边界等价于 Unix 请求的写半关闭。
        }
        let mut count = 0;
        let ok = unsafe {
            ReadFile(
                self.raw(),
                bytes.as_mut_ptr().cast(),
                bytes.len().min(u32::MAX as usize) as u32,
                &mut count,
                null_mut(),
            )
        };
        let error = if ok == 0 { Some(io::Error::last_os_error()) } else { None };
        match error.as_ref().and_then(io::Error::raw_os_error) {
            Some(232 | 536) => Err(would_block()),
            Some(109 | 233) => Ok(0),
            Some(234) => Ok(count as usize),
            Some(_) => Err(error.unwrap()),
            None => {
                self.message_end = self.server;
                Ok(count as usize)
            }
        }
    }
}
impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut count = 0;
        if unsafe {
            WriteFile(
                self.raw(),
                bytes.as_ptr().cast(),
                bytes.len().min(u32::MAX as usize) as u32,
                &mut count,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if count == 0 && !bytes.is_empty() {
            return Err(would_block());
        }
        // 消息模式 NOWAIT 不拆分写；出现短写直接拒绝，不能发送第二个请求消息。
        if count as usize != bytes.len() {
            return Err(invalid());
        }
        Ok(count as usize)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub struct Listener {
    name: [u16; 128],
    pending: RefCell<Option<Stream>>,
}
impl Listener {
    fn instance(name: &[u16; 128], first: bool) -> io::Result<Stream> {
        let descriptor = Descriptor::new()?;
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                3 | if first { 0x0008_0000 } else { 0 },
                4 | 2 | 1 | 8,
                3,
                4096,
                (super::super::PROMPT_LIMIT + 64) as u32,
                0,
                (&descriptor.attributes() as *const Attributes).cast(),
            )
        };
        if handle as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        private::validate_acl(handle.as_raw_handle())?;
        Ok(Stream { handle, message_end: false, request_ended: false, server: true })
    }
    fn new(path: &Path) -> io::Result<Self> {
        let (name, _) = pipe_name(path)?;
        let stream = Self::instance(&name, true)?;
        Ok(Self { name, pending: RefCell::new(Some(stream)) })
    }
    pub fn accept(&self) -> io::Result<(Stream, ())> {
        let mut pending = self.pending.borrow_mut();
        if pending.is_none() {
            *pending = Some(Self::instance(&self.name, false)?);
        }
        let stream = pending.as_ref().unwrap();
        let result = unsafe { ConnectNamedPipe(stream.raw(), null_mut()) };
        if result == 0 {
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(535) => {}
                Some(536) => return Err(would_block()),
                Some(232 | 233 | 109) => {
                    pending.take();
                    return Err(would_block());
                }
                _ => return Err(error),
            }
        }
        Ok((pending.take().unwrap(), ()))
    }
}
#[derive(Debug)]
pub struct CreateError {
    pub error: io::Error,
    pub cleanup_confirmed: bool,
}
impl From<io::Error> for CreateError {
    fn from(error: io::Error) -> Self {
        Self { error, cleanup_confirmed: true }
    }
}
impl From<io::ErrorKind> for CreateError {
    fn from(error: io::ErrorKind) -> Self {
        io::Error::from(error).into()
    }
}
pub struct OwnedTicket {
    path: PathBuf,
    directory: PathBuf,
    parent: Option<File>,
    file: Option<File>,
    listener: Option<Listener>,
    ticket: Ticket,
    removed: bool,
    _memory: Lease,
}
impl OwnedTicket {
    pub fn create(base: &Path, pool: &Arc<Budget>, cancel: &Cancellation) -> io::Result<Self> {
        Self::create_checked(base, pool, cancel).map_err(|e| e.error)
    }
    pub fn create_checked(base: &Path, pool: &Arc<Budget>, cancel: &Cancellation) -> Result<Self, CreateError> {
        local_path(base)?;
        if base.as_os_str().len() > 3900 {
            return Err(invalid().into());
        }
        if cancel.is_cancelled() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        // 3 个管道实例：每个请求缓冲可容纳完整的最大消息，外加响应及端点元数据余量。
        let memory = pool.lease(Kind::Sftp, 128 * 1024).map_err(|_| io::ErrorKind::OutOfMemory)?;
        let ticket = Ticket::new()?;
        let descriptor = Descriptor::new()?;
        for _ in 0..16 {
            let random = super::Capability::generate()?;
            let mut name = format!("cgaw-{}-", std::process::id());
            use std::fmt::Write;
            for b in &random.bytes()[..16] {
                write!(name, "{b:02x}").unwrap();
            }
            let directory = base.join(name);
            match descriptor.directory(&directory) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
            let mut owned = Self {
                path: directory.join("ticket"),
                directory,
                parent: None,
                file: None,
                listener: None,
                ticket,
                removed: false,
                _memory: memory,
            };
            let result = (|| -> io::Result<()> {
                owned.parent = Some(private::open_existing(&owned.directory, 0x80, 3, true)?);
                private::validate(owned.parent.as_ref().unwrap(), true)?;
                owned.listener = Some(Listener::new(&owned.path)?);
                {
                    let mut file = descriptor.file(&owned.path, 0x4000_0000, 0, 1, 0)?;
                    file.write_all(&owned.ticket.record().0)?;
                }
                owned.file = Some(private::open_existing(&owned.path, 0x8000_0000, 1, false)?);
                private::validate(owned.file.as_ref().unwrap(), false)?;
                if cancel.is_cancelled() {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                Ok(())
            })();
            if let Err(error) = result {
                let cleanup = owned.cleanup();
                return Err(CreateError { cleanup_confirmed: cleanup.is_ok(), error: cleanup.err().unwrap_or(error) });
            }
            return Ok(owned);
        }
        Err(io::ErrorKind::AlreadyExists.into())
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn ticket(&self) -> &Ticket {
        &self.ticket
    }
    pub fn listener(&self) -> &Listener {
        self.listener.as_ref().unwrap()
    }
    fn cleanup(&mut self) -> io::Result<()> {
        if self.removed {
            return Ok(());
        }
        self.listener.take();
        self.file.take();
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        self.parent.take();
        match std::fs::remove_dir(&self.directory) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        self.removed = true;
        Ok(())
    }
    pub fn finish(mut self) -> io::Result<()> {
        self.cleanup()
    }
}
impl Drop for OwnedTicket {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_pipe_first_instance_is_exclusive_and_wrong_server_pid_is_rejected() {
        let random = super::super::Capability::generate().unwrap();
        let pid = std::process::id().checked_add(1).unwrap();
        let mut name = format!("cgaw-{pid}-");
        use std::fmt::Write;
        for byte in &random.bytes()[..16] {
            write!(name, "{byte:02x}").unwrap();
        }
        let path = temporary().unwrap().join(name).join("ticket");
        let listener = Listener::new(&path).unwrap();
        assert!(Listener::new(&path).is_err(), "existing named pipe cannot be silently reused");
        assert_eq!(Stream::connect(&path).err().unwrap().kind(), io::ErrorKind::PermissionDenied);
        drop(listener);
        assert!(local_path(Path::new(r"\\host\share\ticket")).is_err());
        assert!(local_path(Path::new(r"\\.\pipe\ticket")).is_err());
    }
}
