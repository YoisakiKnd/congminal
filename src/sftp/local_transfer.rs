//! Unix本地传输拥有者：只在工作器内创建/读写/提交/显式关闭，输入输出DTO不得携带File。
//! 私有同目录暂存，默认原子不覆盖；部分文件保留，发布后错误不会伪装成未提交。
use super::model::RemotePath;
use crate::{
    resources::{Budget, Kind, Lease},
    tasks::Cancellation,
};
use std::{
    ffi::{CString, OsStr},
    fs::{File, Metadata, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        io::{AsRawFd, FromRawFd, IntoRawFd},
    },
    path::Path,
    sync::Arc,
};
const WORK: usize = 32 * 1024;
#[derive(Clone)]
pub struct LocalPath(RemotePath);
impl LocalPath {
    pub fn new(path: &Path, pool: &Arc<Budget>) -> Result<Self, Error> {
        let raw = path.as_os_str().as_bytes();
        if !path.is_absolute()
            || path.file_name().is_none()
            || raw.contains(&0)
            || raw.rsplit(|b| *b == b'/').next().is_none_or(|n| matches!(n, b"" | b"." | b".."))
        {
            return Err(Error::Invalid);
        }
        let value = RemotePath::new(pool, path.as_os_str().as_bytes()).map_err(|e| match e {
            super::model::Error::Budget | super::model::Error::Allocation => Error::Budget,
            _ => Error::Invalid,
        })?;
        Ok(Self(value))
    }
    pub fn path(&self) -> &Path {
        Path::new(OsStr::from_bytes(self.0.bytes()))
    }
    pub fn label(&self) -> &str {
        self.0.label()
    }
    pub(super) fn value(&self) -> RemotePath {
        self.0.clone()
    }
    fn uses(&self, pool: &Arc<Budget>) -> bool {
        self.0.uses(pool)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Budget,
    WrongPool,
    Cancelled,
    FileType,
    Changed,
    Io(io::ErrorKind),
    Cleanup,
}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}
fn check(cancel: &Cancellation) -> Result<(), Error> {
    if cancel.is_cancelled() { Err(Error::Cancelled) } else { Ok(()) }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    device: u64,
    inode: u64,
    pub length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
impl Stamp {
    fn of(metadata: &Metadata) -> Result<Self, Error> {
        if !metadata.is_file() {
            return Err(Error::FileType);
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}
fn close(file: &mut Option<File>) -> bool {
    file.take().is_none_or(|file| {
        // close失败时描述符状态可能不确定，不能重试并误关被复用的数字。
        unsafe { sys::close(file.into_raw_fd()) == 0 }
    })
}
pub struct Source {
    file: Option<File>,
    stamp: Stamp,
    _memory: Lease,
}
pub enum Inspection {
    File(Stamp),
    Directory,
}
impl Source {
    /// 选择阶段只做lstat；不打开数据文件，也不占用进程创建互斥锁。
    pub fn inspect(path: &LocalPath, pool: &Arc<Budget>, cancel: &Cancellation) -> Result<Stamp, Error> {
        match Self::inspect_selection(path, pool, cancel)? {
            Inspection::File(stamp) => Ok(stamp),
            Inspection::Directory => Err(Error::FileType),
        }
    }
    pub fn inspect_selection(path: &LocalPath, pool: &Arc<Budget>, cancel: &Cancellation) -> Result<Inspection, Error> {
        check(cancel)?;
        if !path.uses(pool) {
            return Err(Error::WrongPool);
        }
        let _work = pool.lease(Kind::Sftp, WORK).map_err(|_| Error::Budget)?;
        let metadata = std::fs::symlink_metadata(path.path())?;
        let result = if metadata.is_dir() { Inspection::Directory } else { Inspection::File(Stamp::of(&metadata)?) };
        check(cancel)?;
        Ok(result)
    }
    pub fn open(path: &LocalPath, pool: &Arc<Budget>, cancel: &Cancellation) -> Result<Self, Error> {
        check(cancel)?;
        if !path.uses(pool) {
            return Err(Error::WrongPool);
        }
        let memory = pool.lease(Kind::Sftp, WORK).map_err(|_| Error::Budget)?;
        let file = {
            let _launch = crate::process_launch::guard();
            OpenOptions::new().read(true).custom_flags(sys::NOFOLLOW | sys::NONBLOCK).open(path.path())?
        };
        let stamp = match file.metadata().map_err(Error::from).and_then(|m| Stamp::of(&m)) {
            Ok(stamp) => stamp,
            Err(error) => {
                let mut file = Some(file);
                return Err(if close(&mut file) { error } else { Error::Cleanup });
            }
        };
        Ok(Self { file: Some(file), stamp, _memory: memory })
    }
    pub fn stamp(&self) -> Stamp {
        self.stamp
    }
    pub fn verify(&self, cancel: &Cancellation) -> Result<(), Error> {
        check(cancel)?;
        if Stamp::of(&self.file.as_ref().ok_or(Error::Invalid)?.metadata()?)? != self.stamp {
            return Err(Error::Changed);
        }
        Ok(())
    }
    pub fn finish(mut self) -> Result<(), Error> {
        if close(&mut self.file) { Ok(()) } else { Err(Error::Cleanup) }
    }
}
impl Read for Source {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.as_mut().ok_or(io::ErrorKind::NotConnected)?.read(buffer)
    }
}
/// 开始失败也返回已创建的暂存位置；不在Drop中扫描/删除文件。
pub struct StartError {
    pub error: Error,
    pub partial: Option<LocalPath>,
    pub cleanup_confirmed: bool,
}
impl std::fmt::Debug for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartError")
            .field("error", &self.error)
            .field("has_partial", &self.partial.is_some())
            .field("cleanup_confirmed", &self.cleanup_confirmed)
            .finish()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replace {
    No,
    ExplicitlyApproved,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Commit {
    pub published: bool,
    pub parent_synced: bool,
    pub staging_removed: bool,
}
pub struct Download {
    destination: LocalPath,
    temporary: LocalPath,
    stage: [u8; 44],
    name: CString,
    parent: Option<File>,
    directory: Option<File>,
    file: Option<File>,
    published: bool,
    prepared: Option<Stamp>,
    cleanup_failed: bool,
    _memory: Lease,
}
impl Download {
    pub fn create(destination: LocalPath, pool: &Arc<Budget>, cancel: &Cancellation) -> Result<Self, StartError> {
        let fail = |error| StartError { error, partial: None, cleanup_confirmed: true };
        check(cancel).map_err(fail)?;
        if !destination.uses(pool) {
            return Err(fail(Error::WrongPool));
        }
        let memory = pool.lease(Kind::Sftp, WORK).map_err(|_| fail(Error::Budget))?;
        let parent_path = destination.path().parent().ok_or_else(|| fail(Error::Invalid))?;
        let raw_name = destination.path().file_name().ok_or_else(|| fail(Error::Invalid))?.as_bytes();
        let mut name = Vec::new();
        name.try_reserve_exact(raw_name.len() + 1).map_err(|_| fail(Error::Budget))?;
        name.extend_from_slice(raw_name);
        name.push(0);
        let name = CString::from_vec_with_nul(name).map_err(|_| fail(Error::Invalid))?;
        let mut parent = Some({
            let _launch = crate::process_launch::guard();
            OpenOptions::new()
                .read(true)
                .custom_flags(sys::DIRECTORY | sys::NOFOLLOW | sys::NONBLOCK)
                .open(parent_path)
                .map_err(|e| fail(e.into()))?
        });
        // 从已验证目录FD执行后续操作；路径父目录被改名也不会重定向到替换目录。
        for _ in 0..16 {
            let prepared = (|| -> Result<([u8; 44], LocalPath), Error> {
                check(cancel)?;
                let random = crate::ssh::auth::wire::Capability::generate()?;
                let mut bytes = [0u8; 44];
                bytes[..11].copy_from_slice(b".congminal-");
                for (i, b) in random.bytes()[..16].iter().copied().enumerate() {
                    bytes[11 + 2 * i] = b"0123456789abcdef"[(b >> 4) as usize];
                    bytes[12 + 2 * i] = b"0123456789abcdef"[(b & 15) as usize];
                }
                let parent = parent_path.as_os_str().as_bytes();
                let separator = usize::from(!parent.ends_with(b"/"));
                let length = parent.len() + separator + 43 + 1 + b"payload.part".len();
                if length > super::wire::PATH_LIMIT {
                    return Err(Error::Invalid);
                }
                let mut full = Vec::new();
                full.try_reserve_exact(length).map_err(|_| Error::Budget)?;
                full.extend_from_slice(parent);
                if separator != 0 {
                    full.push(b'/');
                }
                full.extend_from_slice(&bytes[..43]);
                full.push(b'/');
                full.extend_from_slice(b"payload.part");
                let temporary = LocalPath::new(Path::new(OsStr::from_bytes(&full)), pool)?;
                Ok((bytes, temporary))
            })();
            let (stage, temporary) = match prepared {
                Ok(v) => v,
                Err(error) => {
                    let clean = close(&mut parent);
                    return Err(StartError { error, partial: None, cleanup_confirmed: clean });
                }
            };
            if unsafe { sys::mkdirat(parent.as_ref().unwrap().as_raw_fd(), stage.as_ptr().cast(), 0o700) } < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                let clean = close(&mut parent);
                return Err(StartError { error: error.into(), partial: None, cleanup_confirmed: clean });
            }
            let mut download = Self {
                destination,
                temporary,
                stage,
                name,
                parent,
                directory: None,
                file: None,
                published: false,
                prepared: None,
                cleanup_failed: false,
                _memory: memory,
            };
            let setup = download.initialize(cancel);
            if let Err(error) = setup {
                let partial = download.temporary.clone();
                download.cleanup_failed |= error == Error::Cleanup;
                let clean = download.close();
                return Err(StartError { error, partial: Some(partial), cleanup_confirmed: clean });
            }
            return Ok(download);
        }
        let clean = close(&mut parent);
        Err(StartError { error: Error::Io(io::ErrorKind::AlreadyExists), partial: None, cleanup_confirmed: clean })
    }
    fn initialize(&mut self, cancel: &Cancellation) -> Result<(), Error> {
        check(cancel)?;
        let directory = unsafe {
            sys::openat(
                self.parent.as_ref().unwrap().as_raw_fd(),
                self.stage.as_ptr().cast(),
                sys::DIRECTORY | sys::NOFOLLOW | sys::CLOEXEC | sys::NONBLOCK,
                0,
            )
        };
        if directory < 0 {
            return Err(io::Error::last_os_error().into());
        }
        self.directory = Some(unsafe { File::from_raw_fd(directory) });
        let metadata = self.directory.as_ref().unwrap().metadata()?;
        if !metadata.is_dir() || metadata.uid() != unsafe { sys::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(Error::FileType);
        }
        #[cfg(target_os = "macos")]
        {
            // 空目录尚无数据；在创建payload前清掉继承ACL，避免仅0700模式仍留下额外授权。
            let acl = unsafe { sys::acl_init(0) };
            if acl.is_null() {
                return Err(Error::Io(io::ErrorKind::OutOfMemory));
            }
            let result = unsafe { sys::acl_set_fd_np(directory, acl, 0x100) };
            let error = (result < 0).then(io::Error::last_os_error);
            if unsafe { sys::acl_free(acl) } < 0 {
                return Err(Error::Cleanup);
            }
            if let Some(error) = error {
                return Err(error.into());
            }
        }
        self.directory.as_ref().unwrap().set_permissions(std::fs::Permissions::from_mode(0o700))?;
        let file = unsafe {
            sys::openat(
                directory,
                c"payload.part".as_ptr(),
                sys::WRONLY | sys::CREATE | sys::EXCLUSIVE | sys::NOFOLLOW | sys::CLOEXEC,
                0o600,
            )
        };
        if file < 0 {
            return Err(io::Error::last_os_error().into());
        }
        self.file = Some(unsafe { File::from_raw_fd(file) });
        self.file.as_ref().unwrap().set_permissions(std::fs::Permissions::from_mode(0o600))?;
        check(cancel)
    }
    pub fn temporary(&self) -> &LocalPath {
        &self.temporary
    }
    pub fn destination(&self) -> &LocalPath {
        &self.destination
    }
    pub fn sync(&mut self, length: u64, cancel: &Cancellation) -> Result<(), Error> {
        if self.published {
            return Err(Error::Invalid);
        }
        self.prepared = None;
        check(cancel)?;
        let file = self.file.as_mut().ok_or(Error::Invalid)?;
        if file.metadata()?.len() != length {
            return Err(Error::Changed);
        }
        file.flush()?;
        file.sync_all()?;
        check(cancel)?;
        self.prepared = Some(Stamp::of(&file.metadata()?)?);
        Ok(())
    }
    pub fn commit(&mut self, replace: Replace, cancel: &Cancellation) -> Result<Commit, Error> {
        if self.cleanup_failed {
            return Err(Error::Cleanup);
        }
        if self.published {
            return Err(Error::Invalid);
        }
        check(cancel)?;
        let prepared = self.prepared.ok_or(Error::Invalid)?;
        if Stamp::of(&self.file.as_ref().ok_or(Error::Invalid)?.metadata()?)? != prepared {
            return Err(Error::Changed);
        }
        check(cancel)?;
        let mut current_parent = Some({
            let _launch = crate::process_launch::guard();
            OpenOptions::new()
                .read(true)
                .custom_flags(sys::DIRECTORY | sys::NOFOLLOW | sys::NONBLOCK)
                .open(self.destination.path().parent().ok_or(Error::Invalid)?)?
        });
        let same_parent = current_parent
            .as_ref()
            .unwrap()
            .metadata()
            .and_then(|a| self.parent.as_ref().unwrap().metadata().map(|b| same_file(&a, &b)));
        if !close(&mut current_parent) {
            self.cleanup_failed = true;
            return Err(Error::Cleanup);
        }
        if !same_parent? {
            return Err(Error::Changed);
        }
        let directory = self.directory.as_ref().ok_or(Error::Invalid)?.as_raw_fd();
        let parent = self.parent.as_ref().ok_or(Error::Invalid)?.as_raw_fd();
        // 拒绝暂存项被换成另一文件/符号链接；私有目录阻止其他用户更换其内部名称。
        let checked = unsafe {
            sys::openat(directory, c"payload.part".as_ptr(), sys::NOFOLLOW | sys::NONBLOCK | sys::CLOEXEC, 0)
        };
        if checked < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut checked = Some(unsafe { File::from_raw_fd(checked) });
        let identity = checked
            .as_ref()
            .unwrap()
            .metadata()
            .and_then(|a| self.file.as_ref().unwrap().metadata().map(|b| same_file(&a, &b) && a.is_file()));
        let cleaned = close(&mut checked);
        if !cleaned {
            self.cleanup_failed = true;
            return Err(Error::Cleanup);
        }
        if !identity? {
            return Err(Error::Changed);
        }
        check(cancel)?;
        let result = unsafe {
            match replace {
                Replace::No => sys::linkat(directory, c"payload.part".as_ptr(), parent, self.name.as_ptr(), 0),
                Replace::ExplicitlyApproved => {
                    sys::renameat(directory, c"payload.part".as_ptr(), parent, self.name.as_ptr())
                }
            }
        };
        if result < 0 {
            return Err(io::Error::last_os_error().into());
        }
        self.published = true; // 从这里起，取消/目录fsync失败也不能报告为未发布。
        let payload_removed = replace == Replace::ExplicitlyApproved
            || unsafe { sys::unlinkat(directory, c"payload.part".as_ptr(), 0) } == 0;
        // 非sticky且可被其他用户改名的父目录，不按可能已被替换的名字删除目录。
        let safe_parent =
            self.parent.as_ref().unwrap().metadata().is_ok_and(|m| m.mode() & 0o022 == 0 || m.mode() & 0o1000 != 0);
        let staging_removed = payload_removed
            && safe_parent
            && unsafe { sys::unlinkat(parent, self.stage.as_ptr().cast(), sys::REMOVEDIR) } == 0;
        let parent_synced = self.parent.as_ref().unwrap().sync_all().is_ok();
        Ok(Commit { published: true, parent_synced, staging_removed })
    }
    fn close(&mut self) -> bool {
        let a = close(&mut self.file);
        let b = close(&mut self.directory);
        let c = close(&mut self.parent);
        self.cleanup_failed |= !a || !b || !c;
        !self.cleanup_failed
    }
    pub fn finish(mut self) -> Result<(), Error> {
        if self.close() { Ok(()) } else { Err(Error::Cleanup) }
    }
}
impl Write for Download {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.published {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        self.prepared = None;
        self.file.as_mut().ok_or(io::ErrorKind::NotConnected)?.write(buffer)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().ok_or(io::ErrorKind::NotConnected)?.flush()
    }
}
mod sys {
    use std::ffi::{c_char, c_int};
    unsafe extern "C" {
        pub fn openat(fd: c_int, path: *const c_char, flags: c_int, ...) -> c_int;
        pub fn mkdirat(fd: c_int, path: *const c_char, mode: u32) -> c_int;
        pub fn linkat(oldfd: c_int, old: *const c_char, newfd: c_int, new: *const c_char, flags: c_int) -> c_int;
        pub fn renameat(oldfd: c_int, old: *const c_char, newfd: c_int, new: *const c_char) -> c_int;
        pub fn unlinkat(fd: c_int, path: *const c_char, flags: c_int) -> c_int;
        pub fn close(fd: c_int) -> c_int;
        pub fn geteuid() -> u32;
        #[cfg(target_os = "macos")]
        pub fn acl_init(count: c_int) -> *mut std::ffi::c_void;
        #[cfg(target_os = "macos")]
        pub fn acl_set_fd_np(fd: c_int, acl: *mut std::ffi::c_void, kind: c_int) -> c_int;
        #[cfg(target_os = "macos")]
        pub fn acl_free(acl: *mut std::ffi::c_void) -> c_int;
    }
    pub const WRONLY: c_int = 1;
    #[cfg(target_os = "linux")]
    pub use linux::*;
    #[cfg(target_os = "macos")]
    pub use mac::*;
    #[cfg(target_os = "macos")]
    mod mac {
        pub const DIRECTORY: i32 = 0x100000;
        pub const NOFOLLOW: i32 = 0x100;
        pub const CLOEXEC: i32 = 0x1000000;
        pub const NONBLOCK: i32 = 4;
        pub const CREATE: i32 = 0x200;
        pub const EXCLUSIVE: i32 = 0x800;
        pub const REMOVEDIR: i32 = 0x80;
    }
    #[cfg(target_os = "linux")]
    mod linux {
        #[cfg(target_arch = "x86_64")]
        pub const DIRECTORY: i32 = 0x10000;
        #[cfg(target_arch = "aarch64")]
        pub const DIRECTORY: i32 = 0x4000;
        #[cfg(target_arch = "x86_64")]
        pub const NOFOLLOW: i32 = 0x20000;
        #[cfg(target_arch = "aarch64")]
        pub const NOFOLLOW: i32 = 0x8000;
        pub const CLOEXEC: i32 = 0o2000000;
        pub const NONBLOCK: i32 = 0o4000;
        pub const CREATE: i32 = 0o100;
        pub const EXCLUSIVE: i32 = 0o200;
        pub const REMOVEDIR: i32 = 0x200;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        time::{Duration, Instant, SystemTime},
    };
    struct Folder(PathBuf);
    impl Folder {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "cgl-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn path(folder: &Folder, name: &str, pool: &Arc<Budget>) -> LocalPath {
        LocalPath::new(&folder.0.join(name), pool).unwrap()
    }
    #[test]
    fn private_stage_streams_larger_than_budget_and_only_publishes_after_verified_sync() {
        let folder = Folder::new();
        let pool = Budget::new(256 * 1024, 0);
        let cancel = Cancellation::default();
        let destination = path(&folder, "-space 中文;target", &pool);
        let mut download = Download::create(destination.clone(), &pool, &cancel).unwrap();
        let partial = download.temporary().clone();
        assert_eq!(download.destination().path(), destination.path());
        assert!(destination.label().contains("中文"));
        assert_eq!(std::fs::metadata(partial.path()).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(partial.path().parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(download.commit(Replace::No, &cancel), Err(Error::Invalid));
        let chunk = [91u8; 65536];
        let used = pool.snapshot().used;
        for _ in 0..64 {
            download.write_all(&chunk).unwrap();
            assert_eq!(pool.snapshot().used, used);
        }
        assert!(!destination.path().exists());
        download.sync(4 * 1024 * 1024, &cancel).unwrap();
        let commit = download.commit(Replace::No, &cancel).unwrap();
        assert!(commit.published);
        assert!(commit.staging_removed);
        assert!(!partial.path().exists());
        assert!(download.write(b"late").is_err());
        assert_eq!(download.commit(Replace::No, &cancel), Err(Error::Invalid));
        download.finish().unwrap();
        let mut source = Source::open(&destination, &pool, &cancel).unwrap();
        assert_eq!(source.stamp().length, 4 * 1024 * 1024);
        let mut read = [0u8; 65536];
        for _ in 0..64 {
            source.read_exact(&mut read).unwrap();
            assert_eq!(read, chunk);
        }
        assert_eq!(source.read(&mut read).unwrap(), 0);
        source.verify(&cancel).unwrap();
        source.finish().unwrap();
        drop((partial, destination));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn existing_target_and_symlink_are_not_overwritten_without_explicit_approval() {
        let folder = Folder::new();
        let pool = Budget::new(256 * 1024, 0);
        let cancel = Cancellation::default();
        let destination = path(&folder, "existing", &pool);
        std::fs::write(destination.path(), b"original").unwrap();
        let mut download = Download::create(destination.clone(), &pool, &cancel).unwrap();
        download.write_all(b"new").unwrap();
        download.sync(3, &cancel).unwrap();
        assert_eq!(download.commit(Replace::No, &cancel), Err(Error::Io(io::ErrorKind::AlreadyExists)));
        assert_eq!(std::fs::read(destination.path()).unwrap(), b"original");
        assert_eq!(std::fs::read(download.temporary().path()).unwrap(), b"new");
        assert!(download.commit(Replace::ExplicitlyApproved, &cancel).unwrap().published);
        download.finish().unwrap();
        assert_eq!(std::fs::read(destination.path()).unwrap(), b"new");
        let link = path(&folder, "symlink", &pool);
        std::os::unix::fs::symlink(destination.path(), link.path()).unwrap();
        let mut next = Download::create(link.clone(), &pool, &cancel).unwrap();
        next.write_all(b"replacement entry").unwrap();
        next.sync(17, &cancel).unwrap();
        assert_eq!(next.commit(Replace::No, &cancel), Err(Error::Io(io::ErrorKind::AlreadyExists)));
        next.commit(Replace::ExplicitlyApproved, &cancel).unwrap();
        next.finish().unwrap();
        assert!(!std::fs::symlink_metadata(link.path()).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(destination.path()).unwrap(), b"new");
        drop((link, destination));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn cancellation_and_failed_verification_preserve_partial_and_never_publish() {
        let folder = Folder::new();
        let pool = Budget::new(256 * 1024, 0);
        let cancel = Cancellation::default();
        let destination = path(&folder, "cancel", &pool);
        let mut download = Download::create(destination.clone(), &pool, &cancel).unwrap();
        let partial = download.temporary().clone();
        download.write_all(b"part").unwrap();
        assert_eq!(download.sync(5, &cancel), Err(Error::Changed));
        download.sync(4, &cancel).unwrap();
        download.write_all(b"x").unwrap();
        assert_eq!(download.commit(Replace::No, &cancel), Err(Error::Invalid));
        download.sync(5, &cancel).unwrap();
        cancel.cancel();
        assert_eq!(download.commit(Replace::No, &cancel), Err(Error::Cancelled));
        download.finish().unwrap();
        assert!(!destination.path().exists());
        assert_eq!(std::fs::read(partial.path()).unwrap(), b"partx");
        let denied = Download::create(destination.clone(), &pool, &cancel).err().unwrap();
        assert_eq!(denied.error, Error::Cancelled);
        assert!(denied.partial.is_none() && denied.cleanup_confirmed);
        drop((denied, partial, destination));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn observed_parent_replacement_and_payload_replacement_are_rejected() {
        let folder = Folder::new();
        let pool = Budget::new(256 * 1024, 0);
        let cancel = Cancellation::default();
        let parent = folder.0.join("parent");
        std::fs::create_dir(&parent).unwrap();
        let destination = LocalPath::new(&parent.join("file"), &pool).unwrap();
        let mut download = Download::create(destination.clone(), &pool, &cancel).unwrap();
        download.write_all(b"part").unwrap();
        download.sync(4, &cancel).unwrap();
        std::fs::rename(&parent, folder.0.join("moved")).unwrap();
        std::fs::create_dir(&parent).unwrap();
        assert_eq!(download.commit(Replace::No, &cancel), Err(Error::Changed));
        download.finish().unwrap();
        assert!(!parent.join("file").exists());
        assert!(!folder.0.join("moved/file").exists());
        let target = path(&folder, "payload-target", &pool);
        let mut download = Download::create(target.clone(), &pool, &cancel).unwrap();
        download.write_all(b"ours").unwrap();
        download.sync(4, &cancel).unwrap();
        std::fs::remove_file(download.temporary().path()).unwrap();
        std::fs::write(download.temporary().path(), b"other").unwrap();
        assert_eq!(download.commit(Replace::No, &cancel), Err(Error::Changed));
        download.finish().unwrap();
        assert!(!target.path().exists());
        drop((target, destination));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn writable_nonsticky_parent_keeps_empty_stage_instead_of_deleting_a_replaceable_name() {
        let folder = Folder::new();
        std::fs::set_permissions(&folder.0, std::fs::Permissions::from_mode(0o777)).unwrap();
        let pool = Budget::new(256 * 1024, 0);
        let cancel = Cancellation::default();
        let destination = path(&folder, "shared", &pool);
        let mut download = Download::create(destination.clone(), &pool, &cancel).unwrap();
        let partial = download.temporary().clone();
        download.sync(0, &cancel).unwrap();
        let committed = download.commit(Replace::No, &cancel).unwrap();
        assert!(committed.published && !committed.staging_removed);
        assert!(!partial.path().exists());
        assert!(partial.path().parent().unwrap().is_dir());
        download.finish().unwrap();
        assert_eq!(std::fs::metadata(destination.path()).unwrap().len(), 0);
        drop((partial, destination));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn source_checks_same_length_changes_and_rejects_symlinks_directories_and_fifo_without_waiting() {
        let folder = Folder::new();
        let pool = Budget::new(256 * 1024, 0);
        let cancel = Cancellation::default();
        let name = path(&folder, "source", &pool);
        std::fs::write(name.path(), b"same").unwrap();
        let source = Source::open(&name, &pool, &cancel).unwrap();
        std::fs::write(name.path(), b"else").unwrap();
        let file = OpenOptions::new().write(true).open(name.path()).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(source.stamp().modified.0 as u64 + 2)).unwrap();
        assert_eq!(source.verify(&cancel), Err(Error::Changed));
        source.finish().unwrap();
        let link = path(&folder, "link", &pool);
        std::os::unix::fs::symlink(name.path(), link.path()).unwrap();
        assert!(Source::open(&link, &pool, &cancel).is_err());
        let directory = LocalPath::new(&folder.0, &pool).unwrap();
        assert_eq!(Source::open(&directory, &pool, &cancel).err(), Some(Error::FileType));
        unsafe extern "C" {
            fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> i32;
        }
        let fifo = path(&folder, "fifo", &pool);
        let fifo_c = CString::new(fifo.path().as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        let start = Instant::now();
        assert_eq!(Source::open(&fifo, &pool, &cancel).err(), Some(Error::FileType));
        assert!(start.elapsed() < Duration::from_secs(1));
        drop((fifo, directory, link, name));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn cancellation_and_quota_precede_io_and_all_native_descriptors_are_close_on_exec() {
        let folder = Folder::new();
        let pool = Budget::new(256 * 1024, 0);
        let cancel = Cancellation::default();
        let destination = path(&folder, "missing", &pool);
        let empty = Budget::new(0, 0);
        assert_eq!(Source::open(&destination, &empty, &cancel).err(), Some(Error::WrongPool));
        let failure = Download::create(destination.clone(), &empty, &cancel).err().unwrap();
        assert_eq!(failure.error, Error::WrongPool);
        assert!(failure.partial.is_none() && failure.cleanup_confirmed);
        let small = Budget::new(4096, 0);
        let limited = path(&folder, "missing", &small);
        assert_eq!(Source::open(&limited, &small, &cancel).err(), Some(Error::Budget));
        let failure = Download::create(limited.clone(), &small, &cancel).err().unwrap();
        assert_eq!(failure.error, Error::Budget);
        assert!(failure.partial.is_none() && failure.cleanup_confirmed);
        drop((failure, limited));
        assert_eq!(small.snapshot().used, 0);
        let download = Download::create(destination.clone(), &pool, &cancel).unwrap();
        unsafe extern "C" {
            fn fcntl(fd: i32, command: i32, ...) -> i32;
        }
        for fd in
            [download.parent.as_ref().unwrap(), download.directory.as_ref().unwrap(), download.file.as_ref().unwrap()]
        {
            assert_eq!(unsafe { fcntl(fd.as_raw_fd(), 1) } & 1, 1);
        }
        let partial = download.temporary().clone();
        download.finish().unwrap();
        assert!(partial.path().exists());
        assert!(LocalPath::new(Path::new("relative"), &pool).is_err());
        assert!(LocalPath::new(Path::new("/"), &pool).is_err());
        assert!(LocalPath::new(Path::new("/tmp/target/"), &pool).is_err());
        assert!(LocalPath::new(Path::new("/tmp/target/."), &pool).is_err());
        assert!(LocalPath::new(Path::new("/tmp/target/.."), &pool).is_err());
        let cancel = Cancellation::default();
        cancel.cancel();
        assert_eq!(Source::open(&destination, &empty, &cancel).err(), Some(Error::Cancelled));
        drop((partial, destination));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    #[ignore = "tools/check_local_transfer.py：同32MiB顺序写/同步/不覆盖发布的组件成本，无SSH/窗口"]
    fn isolated_local_publish_cost() {
        let mode = std::env::var("CONGMINAL_LOCAL_MODE").unwrap();
        assert!(["baseline", "wrapped"].contains(&mode.as_str()));
        let folder = Folder::new();
        let pool = Budget::new(256 * 1024, 0);
        let cancel = Cancellation::default();
        let destination = path(&folder, "target", &pool);
        let buffer = [37u8; 65536];
        let length = 32 * 1024 * 1024u64;
        let random = crate::ssh::auth::wire::Capability::generate().unwrap();
        drop(random);
        let _ = Instant::now();
        eprintln!("LOCAL_IO_START mode={mode}");
        let start = Instant::now();
        let mut timings = [0u128; 5];
        if mode == "wrapped" {
            let stage = Instant::now();
            let mut output = Download::create(destination.clone(), &pool, &cancel).unwrap();
            timings[0] = stage.elapsed().as_nanos();
            let stage = Instant::now();
            for _ in 0..length / (buffer.len() as u64) {
                output.write_all(&buffer).unwrap();
            }
            timings[1] = stage.elapsed().as_nanos();
            let stage = Instant::now();
            output.sync(length, &cancel).unwrap();
            timings[2] = stage.elapsed().as_nanos();
            let stage = Instant::now();
            let commit = output.commit(Replace::No, &cancel).unwrap();
            assert!(commit.published && commit.staging_removed && commit.parent_synced);
            timings[3] = stage.elapsed().as_nanos();
            let stage = Instant::now();
            output.finish().unwrap();
            timings[4] = stage.elapsed().as_nanos();
        } else {
            // 正向普通路径对照：相同私有目录/0600文件/64KiB写/文件fsync/link不覆盖/父目录fsync。
            // 没有FD锚定/替换检测/确认关闭契约，不当作故障语义等价的可采用后端。
            let stage = Instant::now();
            let temporary = folder.0.join("baseline-private");
            std::fs::create_dir(&temporary).unwrap();
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o700)).unwrap();
            let part = temporary.join("payload.part");
            let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&part).unwrap();
            timings[0] = stage.elapsed().as_nanos();
            let stage = Instant::now();
            for _ in 0..length / (buffer.len() as u64) {
                file.write_all(&buffer).unwrap();
            }
            timings[1] = stage.elapsed().as_nanos();
            let stage = Instant::now();
            file.flush().unwrap();
            file.sync_all().unwrap();
            timings[2] = stage.elapsed().as_nanos();
            let stage = Instant::now();
            std::fs::hard_link(&part, destination.path()).unwrap();
            std::fs::remove_file(&part).unwrap();
            std::fs::remove_dir(&temporary).unwrap();
            File::open(&folder.0).unwrap().sync_all().unwrap();
            timings[3] = stage.elapsed().as_nanos();
            let stage = Instant::now();
            drop(file);
            timings[4] = stage.elapsed().as_nanos();
        }
        let elapsed = start.elapsed().as_nanos();
        eprintln!("LOCAL_IO_END mode={mode}");
        let mut verify = File::open(destination.path()).unwrap();
        let mut actual = [0u8; 65536];
        let mut used = 0;
        loop {
            let count = verify.read(&mut actual).unwrap();
            if count == 0 {
                break;
            }
            assert!(actual[..count].iter().all(|b| *b == 37));
            used += count as u64;
        }
        assert_eq!(used, length);
        drop((verify, destination));
        assert_eq!(pool.snapshot().used, 0);
        eprintln!(
            "LOCAL_COST mode={mode} bytes={length} total_ns={elapsed} create_ns={} write_ns={} sync_ns={} commit_ns={} close_ns={} peak={} final=0",
            timings[0],
            timings[1],
            timings[2],
            timings[3],
            timings[4],
            pool.snapshot().peak
        );
    }
}
