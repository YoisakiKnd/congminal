//! 短生命周期能力票据和受保护的本机端点，不含用户答案；只由后台操作磁盘。
use super::wire::Capability;
#[cfg(unix)]
use crate::{
    resources::{Budget, Kind, Lease},
    tasks::Cancellation,
};
#[cfg(unix)]
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::net::UnixListener,
    path::PathBuf,
    sync::Arc,
};
use std::{io, path::Path};
#[cfg(windows)]
#[path = "auth_ticket_windows.rs"]
pub mod windows;
#[cfg(windows)]
#[allow(unused_imports)] // 独立 helper 只消费 Ticket；主程序还消费拥有者及创建错误。
pub use windows::{CreateError, OwnedTicket};
const SIZE: usize = 40;
struct Record([u8; SIZE]);
impl Drop for Record {
    fn drop(&mut self) {
        for b in &mut self.0 {
            unsafe {
                std::ptr::write_volatile(b, 0);
            }
        }
    }
}
pub struct Ticket {
    pub capability: Capability,
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "private authentication ticket rejected")
}
fn validate_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute() || path.as_os_str().len() > 4096 || path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(invalid());
    }
    Ok(())
}
impl Ticket {
    pub fn new() -> io::Result<Self> {
        Ok(Self { capability: Capability::generate()? })
    }
    fn record(&self) -> Record {
        let mut bytes = Record([0; SIZE]);
        bytes.0[..4].copy_from_slice(b"CGAT");
        bytes.0[4] = 2;
        bytes.0[8..].copy_from_slice(self.capability.bytes());
        bytes
    }
    fn decode(bytes: &Record) -> io::Result<Self> {
        if &bytes.0[..4] != b"CGAT" || bytes.0[4] != 2 || bytes.0[5..8] != [0; 3] {
            return Err(invalid());
        }
        Ok(Self { capability: Capability::new(bytes.0[8..].try_into().unwrap()) })
    }
    pub fn read_private(path: &Path) -> io::Result<Self> {
        validate_path(path)?;
        #[cfg(windows)]
        {
            windows::read_private(path)
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            unsafe extern "C" {
                fn geteuid() -> u32;
            }
            let uid = unsafe { geteuid() };
            let parent = std::fs::symlink_metadata(path.parent().ok_or_else(invalid)?)?;
            if !parent.is_dir() || parent.uid() != uid || parent.mode() & 0o077 != 0 {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            #[cfg(target_os = "macos")]
            const FLAGS: i32 = 4 | 0x100;
            #[cfg(target_os = "linux")]
            const FLAGS: i32 = 2048 | 0x20000;
            let mut file = OpenOptions::new().read(true).custom_flags(FLAGS).open(path)?;
            let secure = |m: &std::fs::Metadata| {
                m.is_file() && m.len() == SIZE as u64 && m.uid() == uid && m.mode() & 0o077 == 0
            };
            let before = file.metadata()?;
            if !secure(&before) {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            let mut record = Record([0; SIZE]);
            file.read_exact(&mut record.0)?;
            if file.read(&mut [0; 1])? != 0 {
                return Err(invalid());
            }
            let after = file.metadata()?;
            if !secure(&after) || before.modified().ok() != after.modified().ok() {
                return Err(invalid());
            }
            Self::decode(&record)
        }
    }
}
#[cfg(unix)]
pub struct OwnedTicket {
    path: PathBuf,
    endpoint: PathBuf,
    directory: PathBuf,
    listener: Option<UnixListener>,
    ticket: Ticket,
    removed: bool,
    _memory: Lease,
}
#[cfg(unix)]
#[derive(Debug)]
pub struct CreateError {
    pub error: io::Error,
    pub cleanup_confirmed: bool,
}
#[cfg(unix)]
impl From<io::Error> for CreateError {
    fn from(error: io::Error) -> Self {
        Self { error, cleanup_confirmed: true }
    }
}
#[cfg(unix)]
impl From<io::ErrorKind> for CreateError {
    fn from(kind: io::ErrorKind) -> Self {
        io::Error::from(kind).into()
    }
}
#[cfg(unix)]
impl OwnedTicket {
    pub fn create(base: &Path, pool: &Arc<Budget>, cancel: &Cancellation) -> io::Result<Self> {
        Self::create_checked(base, pool, cancel).map_err(|failure| failure.error)
    }
    /// 创建后的每条错误路径显式清理；调用方可区分清理失败与普通创建失败。
    pub fn create_checked(base: &Path, pool: &Arc<Budget>, cancel: &Cancellation) -> Result<Self, CreateError> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
        validate_path(base)?;
        // 对两目标均保守采用 Darwin sun_path 的 104 字节（包括 NUL）。
        if base.as_os_str().len().checked_add(25).is_none_or(|n| n >= 104) {
            return Err(invalid().into());
        }
        if cancel.is_cancelled() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let memory = pool.lease(Kind::Sftp, 32 * 1024).map_err(|_| io::ErrorKind::OutOfMemory)?;
        let parent = std::fs::metadata(base)?;
        if !parent.is_dir() || (parent.mode() & 0o022 != 0 && parent.mode() & 0o1000 == 0) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let ticket = Ticket::new()?;
        let mut directory = PathBuf::new();
        let mut created = false;
        for _ in 0..16 {
            let identifier = Capability::generate()?; // 目录标识独立于能力值。
            directory =
                base.join(format!("cgap-{:016x}", u64::from_le_bytes(identifier.bytes()[..8].try_into().unwrap())));
            if cancel.is_cancelled() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => {
                    created = true;
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        if !created {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        let mut owned = Self {
            path: directory.join("ticket"),
            endpoint: directory.join("s"),
            directory,
            listener: None,
            ticket,
            removed: false,
            _memory: memory,
        };
        let result = (|| -> io::Result<()> {
            owned.listener = Some({
                let _guard = crate::process_launch::guard();
                UnixListener::bind(&owned.endpoint)?
            });
            owned.listener.as_ref().unwrap().set_nonblocking(true)?;
            {
                let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&owned.path)?;
                file.write_all(&owned.ticket.record().0)?;
            }
            if cancel.is_cancelled() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            owned
                ._memory
                .resize(owned.path.capacity() + owned.endpoint.capacity() + owned.directory.capacity() + 1024)
                .map_err(|_| io::ErrorKind::OutOfMemory)?;
            Ok(())
        })();
        if let Err(error) = result {
            let cleanup = owned.cleanup();
            return Err(CreateError { cleanup_confirmed: cleanup.is_ok(), error: cleanup.err().unwrap_or(error) });
        }
        Ok(owned)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn ticket(&self) -> &Ticket {
        &self.ticket
    }
    pub fn listener(&self) -> &UnixListener {
        self.listener.as_ref().unwrap()
    }
    fn cleanup(&mut self) -> io::Result<()> {
        if self.removed {
            return Ok(());
        }
        self.listener.take();
        for path in [&self.path, &self.endpoint] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
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
#[cfg(unix)]
impl Drop for OwnedTicket {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_record_requires_version_reserved_bits_and_system_random() {
        let ticket = Ticket::new().unwrap();
        let next = Ticket::new().unwrap();
        assert_ne!(ticket.capability.bytes(), next.capability.bytes());
        let record = ticket.record();
        assert_eq!(&record.0[..8], b"CGAT\x02\x00\x00\x00");
        assert_eq!(Ticket::decode(&record).unwrap().capability.bytes(), ticket.capability.bytes());
        for index in [0, 4, 5, 6, 7] {
            let mut bad = ticket.record();
            bad.0[index] ^= 1;
            assert!(Ticket::decode(&bad).is_err());
        }
        assert!(Ticket::read_private(Path::new("relative")).is_err());
    }
    #[cfg(unix)]
    struct Folder(PathBuf);
    #[cfg(unix)]
    impl Folder {
        fn new() -> Self {
            use std::os::unix::fs::DirBuilderExt;
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "cgt-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }
    #[cfg(unix)]
    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[cfg(unix)]
    #[test]
    fn private_endpoint_roundtrip_cancellation_and_cleanup_return_all_credit() {
        use std::os::unix::{fs::PermissionsExt, net::UnixStream};
        let folder = Folder::new();
        let pool = Budget::new(128 * 1024, 0);
        let cancel = Cancellation::default();
        let owned = OwnedTicket::create(&folder.0, &pool, &cancel).unwrap();
        let path = owned.path().to_owned();
        let directory = path.parent().unwrap().to_owned();
        assert_eq!(std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(Ticket::read_private(&path).unwrap().capability.bytes(), owned.ticket().capability.bytes());
        let _client = UnixStream::connect(directory.join("s")).unwrap();
        let _accepted = owned.listener().accept().unwrap();
        owned.finish().unwrap();
        assert!(!directory.exists());
        assert_eq!(pool.snapshot().used, 0);
        let cancelled = Cancellation::default();
        cancelled.cancel();
        assert!(
            matches!(OwnedTicket::create(&folder.0,&Budget::new(0,0),&cancelled),Err(e) if e.kind()==io::ErrorKind::Interrupted)
        );
        assert!(
            matches!(OwnedTicket::create(&folder.0,&Budget::new(0,0),&cancel),Err(e) if e.kind()==io::ErrorKind::OutOfMemory)
        );
        assert!(std::fs::read_dir(&folder.0).unwrap().next().is_none());
        let owned = OwnedTicket::create(&folder.0, &pool, &cancel).unwrap();
        let path = owned.path().to_owned();
        drop(owned);
        assert!(!path.exists());
        assert_eq!(pool.snapshot().used, 0);
    }
    #[cfg(unix)]
    #[test]
    fn insecure_symlink_special_and_wrong_length_tickets_are_rejected() {
        use std::os::unix::{
            ffi::OsStrExt,
            fs::{PermissionsExt, symlink},
        };
        let folder = Folder::new();
        let pool = Budget::new(128 * 1024, 0);
        // 可写且无 sticky 的父目录允许其他用户替换整个私有子目录。
        std::fs::set_permissions(&folder.0, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            matches!(OwnedTicket::create(&folder.0, &pool, &Cancellation::default()), Err(e) if e.kind() == io::ErrorKind::PermissionDenied)
        );
        assert_eq!(pool.snapshot().used, 0);
        assert!(std::fs::read_dir(&folder.0).unwrap().next().is_none());
        std::fs::set_permissions(&folder.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let owned = OwnedTicket::create(&folder.0, &pool, &Cancellation::default()).unwrap();
        let path = owned.path();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Ticket::read_private(path).is_err());
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = folder.0.join("link");
        symlink(path, &link).unwrap();
        assert!(Ticket::read_private(&link).is_err());
        std::fs::set_permissions(path.parent().unwrap(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Ticket::read_private(path).is_err());
        std::fs::set_permissions(path.parent().unwrap(), std::fs::Permissions::from_mode(0o700)).unwrap();
        for length in [0, 39, 41] {
            std::fs::write(path, vec![0; length]).unwrap();
            assert!(Ticket::read_private(path).is_err());
        }
        unsafe extern "C" {
            fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> i32;
        }
        let fifo = owned.directory.join("fifo");
        let raw = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { mkfifo(raw.as_ptr(), 0o600) }, 0);
        assert!(Ticket::read_private(&fifo).is_err());
        std::fs::remove_file(fifo).unwrap();
        owned.finish().unwrap();
        assert_eq!(pool.snapshot().used, 0);
    }
}
