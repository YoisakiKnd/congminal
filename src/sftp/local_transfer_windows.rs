//! Windows 本地传输拥有者：私有暂存、目录相对创建和句柄相对原子发布。
//! 不依赖 shell/临时文件命令；失败和取消保留私有 partial，不递归删除未知内容。
use super::model::RemotePath;
use crate::{
    resources::{Budget, Kind, Lease},
    ssh::auth::ticket::windows::private,
    tasks::Cancellation,
};
use std::{
    ffi::{OsStr, c_void},
    fs::File,
    io::{self, Read, Write},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, IntoRawHandle},
    },
    path::{Component, Path},
    ptr::{null, null_mut},
    sync::Arc,
};
const WORK: usize = 32 * 1024;
#[path = "local_tree_windows.rs"]
mod tree;
pub fn create_directory(root: &Path, directory: &Path, pool: &Arc<Budget>, cancel: &Cancellation) -> Result<(), Error> {
    let _work = pool.lease(Kind::Sftp, WORK).map_err(|_| Error::Budget)?;
    let path = LocalPath::new(&directory.join("_"), pool)?;
    let mut file = Some(tree::parent(root, path.path(), cancel)?);
    if closed(&mut file) { Ok(()) } else { Err(Error::Cleanup) }
}
type Handle = *mut c_void;
#[derive(Clone)]
pub struct LocalPath(RemotePath);
impl LocalPath {
    pub fn new(path: &Path, pool: &Arc<Budget>) -> Result<Self, Error> {
        let text = path.to_str().ok_or(Error::Invalid)?;
        if !path.is_absolute() || path.file_name().is_none() || text.len() > 4096 || text.contains('\0') {
            return Err(Error::Invalid);
        }
        // 不允许 DOS 设备、ADS、路径折叠或设备命名空间。输入显示必须能无损回读。
        for component in path.components() {
            match component {
                Component::Prefix(prefix)
                    if !matches!(prefix.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)) =>
                {
                    return Err(Error::Invalid);
                }
                Component::Normal(name) if !normal(name) => return Err(Error::Invalid),
                Component::ParentDir | Component::CurDir => return Err(Error::Invalid),
                _ => {}
            }
        }
        let value = RemotePath::new(pool, text.as_bytes()).map_err(|e| match e {
            super::model::Error::Budget | super::model::Error::Allocation => Error::Budget,
            _ => Error::Invalid,
        })?;
        Ok(Self(value))
    }
    pub fn path(&self) -> &Path {
        Path::new(std::str::from_utf8(self.0.bytes()).expect("validated local UTF-8 path"))
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
fn normal(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if name.is_empty()
        || name.ends_with(['.', ' '])
        || name.bytes().any(|b| b < 32 || matches!(b, b':' | b'<' | b'>' | b'"' | b'|' | b'?' | b'*' | b'/' | b'\\'))
    {
        return false;
    }
    let base_text = name.split('.').next().unwrap().trim_end_matches(' ');
    let base = base_text.as_bytes();
    if [b"CON".as_slice(), b"PRN", b"AUX", b"NUL", b"CONIN$", b"CONOUT$"]
        .iter()
        .any(|word| base.eq_ignore_ascii_case(word))
    {
        return false;
    }
    !(base.len() >= 4
        && (base[..3].eq_ignore_ascii_case(b"COM") || base[..3].eq_ignore_ascii_case(b"LPT"))
        && matches!(base_text.get(3..), Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")))
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
#[repr(C)]
struct Basic {
    creation: i64,
    access: i64,
    write: i64,
    change: i64,
    attributes: u32,
}
#[repr(C)]
struct Identity {
    volume: u64,
    file: [u8; 16],
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetFileInformationByHandleEx(file: Handle, class: i32, data: *mut c_void, size: u32) -> i32;
    fn GetFileType(file: Handle) -> u32;
    fn CloseHandle(handle: Handle) -> i32;
}
fn closed(file: &mut Option<File>) -> bool {
    file.take().is_none_or(|f| unsafe { CloseHandle(f.into_raw_handle()) } != 0)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    volume: u64,
    file: [u8; 16],
    pub length: u64,
    modified: i64,
    changed: i64,
}
impl Stamp {
    fn of(file: &File, directory: bool) -> Result<Self, Error> {
        let mut basic = Basic { creation: 0, access: 0, write: 0, change: 0, attributes: 0 };
        let mut identity = Identity { volume: 0, file: [0; 16] };
        if unsafe { GetFileType(file.as_raw_handle()) } != 1 {
            return Err(Error::FileType);
        }
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                0,
                (&mut basic as *mut Basic).cast(),
                std::mem::size_of::<Basic>() as u32,
            )
        } == 0
            || unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    18,
                    (&mut identity as *mut Identity).cast(),
                    std::mem::size_of::<Identity>() as u32,
                )
            } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        if basic.attributes & 0x400 != 0 || (basic.attributes & 0x10 != 0) != directory {
            return Err(Error::FileType);
        }
        Ok(Self {
            volume: identity.volume,
            file: identity.file,
            length: file.metadata()?.len(),
            modified: basic.write,
            changed: basic.change,
        })
    }
    fn same_file(self, other: Self) -> bool {
        self.volume == other.volume && self.file == other.file
    }
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
        let mut file = Some(private::open_existing(path.path(), 0x80, 3, true)?);
        let result = (|| -> Result<Inspection, Error> {
            let handle = file.as_ref().unwrap();
            let directory = handle.metadata()?.is_dir();
            let stamp = Stamp::of(handle, directory)?;
            check(cancel)?;
            Ok(if directory { Inspection::Directory } else { Inspection::File(stamp) })
        })();
        if !closed(&mut file) {
            return Err(Error::Cleanup);
        }
        result
    }
    pub fn open(path: &LocalPath, pool: &Arc<Budget>, cancel: &Cancellation) -> Result<Self, Error> {
        check(cancel)?;
        if !path.uses(pool) {
            return Err(Error::WrongPool);
        }
        let memory = pool.lease(Kind::Sftp, WORK).map_err(|_| Error::Budget)?;
        let mut file = Some(private::open_existing(path.path(), 0x8000_0000, 3, false)?);
        let stamp = match Stamp::of(file.as_ref().unwrap(), false) {
            Ok(stamp) => stamp,
            Err(error) => {
                return Err(if closed(&mut file) { error } else { Error::Cleanup });
            }
        };
        Ok(Self { file, stamp, _memory: memory })
    }
    pub fn stamp(&self) -> Stamp {
        self.stamp
    }
    pub fn verify(&self, cancel: &Cancellation) -> Result<(), Error> {
        check(cancel)?;
        if Stamp::of(self.file.as_ref().ok_or(Error::Invalid)?, false)? != self.stamp {
            return Err(Error::Changed);
        }
        Ok(())
    }
    pub fn finish(mut self) -> Result<(), Error> {
        if closed(&mut self.file) { Ok(()) } else { Err(Error::Cleanup) }
    }
}
impl Read for Source {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.file.as_mut().ok_or(io::ErrorKind::NotConnected)?.read(bytes)
    }
}
#[derive(Debug)]
pub struct StartError {
    pub error: Error,
    pub partial: Option<LocalPath>,
    pub cleanup_confirmed: bool,
}
impl std::fmt::Debug for LocalPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalPath(..)")
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
#[repr(C)]
struct Unicode {
    length: u16,
    capacity: u16,
    buffer: *const u16,
}
#[repr(C)]
struct Object {
    length: u32,
    root: Handle,
    name: *const Unicode,
    attributes: u32,
    descriptor: *mut c_void,
    qos: *mut c_void,
}
#[repr(C)]
struct IoStatus {
    status: usize,
    information: usize,
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateFile(
        file: *mut Handle,
        access: u32,
        object: *const Object,
        status: *mut IoStatus,
        allocation: *const i64,
        attributes: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea: *const c_void,
        ea_len: u32,
    ) -> i32;
    fn NtSetInformationFile(file: Handle, status: *mut IoStatus, data: *const c_void, size: u32, class: u32) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}
fn native(status: i32) -> io::Result<()> {
    if status < 0 { Err(io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)) } else { Ok(()) }
}
fn child(parent: &File, name: &OsStr, directory: bool, descriptor: &private::Descriptor) -> Result<File, Error> {
    child_mode(parent, name, directory, Some(descriptor), true)
}
fn child_mode(
    parent: &File,
    name: &OsStr,
    directory: bool,
    descriptor: Option<&private::Descriptor>,
    delete: bool,
) -> Result<File, Error> {
    if !normal(name) {
        return Err(Error::Invalid);
    }
    let mut units = [0u16; 4096];
    let mut count = 0;
    for (at, unit) in name.encode_wide().enumerate() {
        if at >= units.len() {
            return Err(Error::Invalid);
        }
        units[at] = unit;
        count = at + 1;
    }
    let name = Unicode { length: (count * 2) as u16, capacity: (count * 2) as u16, buffer: units.as_ptr() };
    let object = Object {
        length: std::mem::size_of::<Object>() as u32,
        root: parent.as_raw_handle(),
        name: &name,
        attributes: 0x40,
        descriptor: descriptor.map_or(null_mut(), private::Descriptor::pointer),
        qos: null_mut(),
    };
    let mut handle = null_mut();
    let mut status = IoStatus { status: 0, information: 0 };
    let access = if directory { 0x0012_00a0 | if delete { 0x0001_0000 } else { 0 } } else { 0xc013_0000 }; // DELETE | READ_CONTROL | SYNCHRONIZE
    native(unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &object,
            &mut status,
            null(),
            0,
            if directory { 3 } else { 1 },
            if descriptor.is_some() { 2 } else { 1 },
            0x0020_0020 | if directory { 1 } else { 0x40 },
            null(),
            0,
        )
    })?;
    Ok(unsafe { File::from_raw_handle(handle) })
}
#[repr(C)]
struct Rename {
    replace: u32,
    root: Handle,
    length: u32,
    name: [u16; 4096],
}
pub struct Download {
    destination: LocalPath,
    temporary: LocalPath,
    parent: Option<File>,
    directory: Option<File>,
    file: Option<File>,
    parent_stamp: Stamp,
    published: bool,
    prepared: Option<Stamp>,
    cleanup_failed: bool,
    _memory: Lease,
}
impl Download {
    pub fn create(destination: LocalPath, pool: &Arc<Budget>, cancel: &Cancellation) -> Result<Self, StartError> {
        Self::create_checked(destination, None, pool, cancel)
    }
    pub fn create_tree(
        destination: LocalPath,
        root: &Path,
        pool: &Arc<Budget>,
        cancel: &Cancellation,
    ) -> Result<Self, StartError> {
        Self::create_checked(destination, Some(root), pool, cancel)
    }
    fn create_checked(
        destination: LocalPath,
        root: Option<&Path>,
        pool: &Arc<Budget>,
        cancel: &Cancellation,
    ) -> Result<Self, StartError> {
        let fail = |error| StartError { error, partial: None, cleanup_confirmed: true };
        check(cancel).map_err(fail)?;
        if !destination.uses(pool) {
            return Err(fail(Error::WrongPool));
        }
        let memory = pool.lease(Kind::Sftp, WORK).map_err(|_| fail(Error::Budget))?;
        let parent_path = destination.path().parent().ok_or_else(|| fail(Error::Invalid))?;
        let mut parent = Some(if let Some(root) = root {
            tree::parent(root, destination.path(), cancel).map_err(|error| StartError {
                error,
                partial: None,
                cleanup_confirmed: error != Error::Cleanup,
            })?
        } else {
            private::open_existing(parent_path, 0xa0, 3, true).map_err(|e| fail(e.into()))?
        });
        let prepared = (|| -> Result<_, Error> {
            let stamp = Stamp::of(parent.as_ref().unwrap(), true)?;
            let descriptor = private::Descriptor::new()?;
            for _ in 0..16 {
                check(cancel)?;
                let random = crate::ssh::auth::wire::Capability::generate()?;
                let mut name = String::from(".congminal-");
                use std::fmt::Write;
                for byte in &random.bytes()[..16] {
                    write!(name, "{byte:02x}").unwrap();
                }
                let temporary = LocalPath::new(&parent_path.join(&name).join("payload.part"), pool)?;
                match child(parent.as_ref().unwrap(), OsStr::new(&name), true, &descriptor) {
                    Ok(directory) => return Ok((stamp, descriptor, temporary, directory)),
                    Err(Error::Io(io::ErrorKind::AlreadyExists)) => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(Error::Io(io::ErrorKind::AlreadyExists))
        })();
        let (parent_stamp, descriptor, temporary, directory) = match prepared {
            Ok(value) => value,
            Err(error) => {
                let clean = closed(&mut parent);
                return Err(StartError { error, partial: None, cleanup_confirmed: clean });
            }
        };
        let mut owned = Self {
            destination,
            temporary,
            parent,
            directory: Some(directory),
            file: None,
            parent_stamp,
            published: false,
            prepared: None,
            cleanup_failed: false,
            _memory: memory,
        };
        let initialized = (|| -> Result<(), Error> {
            private::validate(owned.directory.as_ref().unwrap(), true)?;
            owned.file =
                Some(child(owned.directory.as_ref().unwrap(), OsStr::new("payload.part"), false, &descriptor)?);
            private::validate(owned.file.as_ref().unwrap(), false)?;
            check(cancel)
        })();
        if let Err(error) = initialized {
            let partial = Some(owned.temporary.clone());
            let clean = owned.close();
            return Err(StartError { error, partial, cleanup_confirmed: clean });
        }
        Ok(owned)
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
        self.prepared = Some(Stamp::of(file, false)?);
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
        if Stamp::of(self.file.as_ref().ok_or(Error::Invalid)?, false)? != prepared {
            return Err(Error::Changed);
        }
        let mut current =
            Some(private::open_existing(self.destination.path().parent().ok_or(Error::Invalid)?, 0xa0, 3, true)?);
        let identity = Stamp::of(current.as_ref().unwrap(), true);
        if !closed(&mut current) {
            self.cleanup_failed = true;
            return Err(Error::Cleanup);
        }
        if !identity?.same_file(self.parent_stamp) {
            return Err(Error::Changed);
        }
        let mut rename = Rename {
            replace: u32::from(replace == Replace::ExplicitlyApproved),
            root: self.parent.as_ref().ok_or(Error::Invalid)?.as_raw_handle(),
            length: 0,
            name: [0; 4096],
        };
        for (at, unit) in self.destination.path().file_name().ok_or(Error::Invalid)?.encode_wide().enumerate() {
            if at >= rename.name.len() {
                return Err(Error::Invalid);
            }
            rename.name[at] = unit;
            rename.length += 2;
        }
        check(cancel)?;
        let mut status = IoStatus { status: 0, information: 0 };
        let size = (std::mem::offset_of!(Rename, name) as u32 + rename.length).max(24);
        native(unsafe {
            NtSetInformationFile(
                self.file.as_ref().unwrap().as_raw_handle(),
                &mut status,
                (&rename as *const Rename).cast(),
                size,
                10,
            )
        })?;
        self.published = true; // 从这里起任何失败/取消都不能回报未发布。
        let remove = 1u8;
        let staging_removed = unsafe {
            NtSetInformationFile(
                self.directory.as_ref().unwrap().as_raw_handle(),
                &mut status,
                (&remove as *const u8).cast(),
                1,
                13,
            )
        } >= 0;
        // Windows 的普通目录句柄通常不支持 FlushFileBuffers；如失败准确报告。
        let parent_synced = self.parent.as_ref().unwrap().sync_all().is_ok();
        Ok(Commit { published: true, parent_synced, staging_removed })
    }
    fn close(&mut self) -> bool {
        let a = closed(&mut self.file);
        let b = closed(&mut self.directory);
        let c = closed(&mut self.parent);
        self.cleanup_failed |= !a || !b || !c;
        !self.cleanup_failed
    }
    pub fn finish(mut self) -> Result<(), Error> {
        if self.close() { Ok(()) } else { Err(Error::Cleanup) }
    }
}
impl Write for Download {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.published {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        self.prepared = None;
        self.file.as_mut().ok_or(io::ErrorKind::NotConnected)?.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().ok_or(io::ErrorKind::NotConnected)?.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Folder(std::path::PathBuf);
    impl Folder {
        fn new() -> Self {
            let random = crate::ssh::auth::wire::Capability::generate().unwrap();
            let path = std::env::temp_dir().join(format!(
                "cg-local-test-{}-{:016x}",
                std::process::id(),
                u64::from_le_bytes(random.bytes()[..8].try_into().unwrap())
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self, name: &str, pool: &Arc<Budget>) -> LocalPath {
            LocalPath::new(&self.0.join(name), pool).unwrap()
        }
    }
    impl Drop for Folder {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[test]
    fn windows_tree_download_creates_private_directories_and_rejects_junctions() {
        let folder = Folder::new();
        let outside = Folder::new();
        let pool = Budget::new(512 * 1024, 0);
        let cancel = Cancellation::default();
        let root = folder.0.join("new-root");
        let destination = LocalPath::new(&root.join("one/two/file"), &pool).unwrap();
        let mut output = Download::create_tree(destination.clone(), &root, &pool, &cancel).unwrap();
        output.write_all(b"tree").unwrap();
        output.sync(4, &cancel).unwrap();
        assert!(output.commit(Replace::No, &cancel).unwrap().published);
        output.finish().unwrap();
        assert_eq!(std::fs::read(destination.path()).unwrap(), b"tree");
        let junction = root.join("redirect");
        let status = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside.0)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "isolated junction fixture must be available for the native boundary test");
        let redirected = LocalPath::new(&junction.join("leak"), &pool).unwrap();
        assert!(Download::create_tree(redirected.clone(), &root, &pool, &cancel).is_err());
        assert_eq!(std::fs::read_dir(&outside.0).unwrap().count(), 0);
        std::fs::remove_dir(junction).unwrap();
        drop((destination, redirected));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn windows_local_path_rejects_devices_streams_lossy_names_and_relative_paths() {
        let pool = Budget::new(128 * 1024, 0);
        for path in [
            r"relative",
            r"C:\NUL",
            r"C:\COM1.txt",
            r"C:\tmp\file:stream",
            r"C:\tmp\trailing. ",
            r"\\.\pipe\local",
            r"C:\tmp\..\file",
            r"\\host\share\file",
        ] {
            assert!(matches!(LocalPath::new(Path::new(path), &pool), Err(Error::Invalid)), "{path}");
        }
        use std::os::windows::ffi::OsStringExt;
        let bad = std::ffi::OsString::from_wide(&[67, 58, 92, 0xd800]);
        assert!(LocalPath::new(Path::new(&bad), &pool).is_err());
        let path = LocalPath::new(Path::new(r"C:\tmp\数据.txt"), &pool).unwrap();
        assert!(path.label().contains("数据.txt"));
        assert_eq!(path.value().bytes(), path.path().to_str().unwrap().as_bytes());
        drop(path);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn windows_source_identity_detects_same_length_changes_and_releases_handles() {
        let folder = Folder::new();
        let pool = Budget::new(512 * 1024, 0);
        let cancel = Cancellation::default();
        let path = folder.path("source", &pool);
        std::fs::write(path.path(), b"original").unwrap();
        let selected = Source::inspect(&path, &pool, &cancel).unwrap();
        let mut source = Source::open(&path, &pool, &cancel).unwrap();
        assert_eq!(source.stamp(), selected);
        let mut bytes = [0u8; 8];
        source.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"original");
        source.verify(&cancel).unwrap();
        {
            let mut changed = std::fs::OpenOptions::new().write(true).open(path.path()).unwrap();
            changed.write_all(b"modified").unwrap();
            changed.sync_all().unwrap();
            changed
                .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000))
                .unwrap();
        }
        assert_eq!(source.verify(&cancel), Err(Error::Changed));
        source.finish().unwrap();
        std::fs::rename(path.path(), folder.0.join("closed")).unwrap();
        drop(path);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn windows_download_is_private_atomic_and_never_overwrites_without_approval() {
        let folder = Folder::new();
        let pool = Budget::new(512 * 1024, 0);
        let cancel = Cancellation::default();
        let path = folder.path("destination", &pool);
        std::fs::write(path.path(), b"keep").unwrap();
        let mut output = Download::create(path.clone(), &pool, &cancel).unwrap();
        assert_eq!(output.destination().path(), path.path());
        let partial = output.temporary().clone();
        assert!(partial.path().exists());
        assert!(
            std::fs::OpenOptions::new().write(true).open(partial.path()).is_err(),
            "other handles cannot mutate the staging payload"
        );
        output.write_all(b"new content").unwrap();
        output.sync(11, &cancel).unwrap();
        assert!(output.commit(Replace::No, &cancel).is_err());
        assert!(!output.published);
        assert_eq!(std::fs::read(path.path()).unwrap(), b"keep");
        assert!(partial.path().exists());
        let result = output.commit(Replace::ExplicitlyApproved, &cancel).unwrap();
        assert!(result.published);
        assert_eq!(std::fs::read(path.path()).unwrap(), b"new content");
        assert_eq!(output.commit(Replace::No, &cancel), Err(Error::Invalid));
        assert!(output.write(b"bad").is_err());
        output.finish().unwrap();
        assert!(!partial.path().exists());
        if result.staging_removed {
            assert!(!partial.path().parent().unwrap().exists());
        }
        drop((partial, path));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn windows_cancel_preserves_partial_and_no_admission_creates_no_output() {
        let folder = Folder::new();
        let pool = Budget::new(512 * 1024, 0);
        let cancel = Cancellation::default();
        let path = folder.path("cancelled", &pool);
        let wrong = Budget::new(512 * 1024, 0);
        let denied = Download::create(path.clone(), &wrong, &cancel).err().unwrap();
        assert_eq!(denied.error, Error::WrongPool);
        assert!(denied.cleanup_confirmed && denied.partial.is_none());
        assert_eq!(std::fs::read_dir(&folder.0).unwrap().count(), 0);
        let mut output = Download::create(path.clone(), &pool, &cancel).unwrap();
        let partial = output.temporary().clone();
        output.write_all(b"partial").unwrap();
        output.sync(7, &cancel).unwrap();
        cancel.cancel();
        assert_eq!(output.commit(Replace::No, &cancel), Err(Error::Cancelled));
        output.finish().unwrap();
        assert!(!path.path().exists());
        assert_eq!(std::fs::read(partial.path()).unwrap(), b"partial");
        let denied = Download::create(path.clone(), &pool, &cancel).err().unwrap();
        assert!(denied.cleanup_confirmed && denied.partial.is_none());
        drop((partial, path));
        assert_eq!(pool.snapshot().used, 0);
    }
}
