//! 后台认证文件输入：只读普通文件，按实际大小先准入，不缓存私钥。
use super::native::Error;
use crate::{
    resources::{Budget, Kind, Lease},
    tasks::Cancellation,
};
use std::{
    fs::{File, Metadata, OpenOptions},
    io::{self, Read},
    path::Path,
    sync::Arc,
};
const LIMIT: usize = 1024 * 1024;
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    PrivateKey,
    HostRecords,
}
pub struct Blob {
    data: Vec<u8>,
    purpose: Purpose,
    _memory: Lease,
}
impl std::ops::Deref for Blob {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.data
    }
}
fn cancelled(token: &Cancellation) -> Result<(), Error> {
    if token.is_cancelled() { Err(Error::Cancelled) } else { Ok(()) }
}
fn check(metadata: &Metadata, purpose: Purpose) -> Result<usize, Error> {
    if !metadata.is_file() {
        return Err(Error::FileType);
    }
    let size = usize::try_from(metadata.len()).map_err(|_| Error::Limit)?;
    if size > LIMIT {
        return Err(Error::Limit);
    }
    if purpose == Purpose::PrivateKey {
        if size == 0 {
            return Err(Error::Credential);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // 与 OpenSSH 一样，当前用户所有的密钥不能向组/其他用户开放。
            if metadata.uid() == unsafe { libc::getuid() } && metadata.mode() & 0o077 != 0 {
                return Err(Error::FilePermissions);
            }
        }
        #[cfg(windows)]
        return Err(Error::FilePermissions); // 待实现 ACL 核对；不能借内存认证绕过原工具的权限检查。
    }
    Ok(size)
}
impl Blob {
    pub fn read(path: &Path, purpose: Purpose, pool: &Arc<Budget>, token: &Cancellation) -> Result<Self, Error> {
        Self::read_with(path, purpose, pool, token, |_| {})
    }
    fn read_with(
        path: &Path,
        purpose: Purpose,
        pool: &Arc<Budget>,
        token: &Cancellation,
        observed: impl FnOnce(&File),
    ) -> Result<Self, Error> {
        if path.as_os_str().is_empty()
            || path.as_os_str().len() > 16 * 1024
            || path.as_os_str().as_encoded_bytes().contains(&0)
        {
            return Err(Error::Invalid);
        }
        cancelled(token)?;
        // 覆盖路径转换、File/Metadata 及系统调用的固定 Rust 工作数据；非完整系统堆。
        let _work = pool.lease(Kind::Sftp, 32 * 1024).map_err(|_| Error::Budget)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
        }
        let mut file = options.open(path)?;
        let before = file.metadata()?;
        let size = check(&before, purpose)?;
        let memory = pool.lease(Kind::Sftp, size + 1 + 128).map_err(|_| Error::Budget)?;
        let mut data = Vec::new();
        data.try_reserve_exact(size + 1).map_err(|_| Error::Budget)?;
        data.resize(size + 1, 0);
        let mut result = Self { data, purpose, _memory: memory };
        observed(&file);
        let mut used = 0;
        while used < result.data.len() {
            cancelled(token)?;
            let end = (used + 64 * 1024).min(result.data.len());
            match file.read(&mut result.data[used..end]) {
                Ok(0) => break,
                Ok(count) => used += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        cancelled(token)?;
        let after = file.metadata()?;
        if used != size || check(&after, purpose)? != size || before.modified().ok() != after.modified().ok() {
            return Err(Error::FileChanged);
        }
        result.data.truncate(size);
        Ok(result)
    }
    fn clear(&mut self) {
        if self.purpose == Purpose::PrivateKey {
            for byte in &mut self.data {
                unsafe {
                    std::ptr::write_volatile(byte, 0);
                }
            }
        }
    }
}
impl Drop for Blob {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Folder(std::path::PathBuf);
    impl Folder {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "congminal-auth-input-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn file(&self, name: &str, data: &[u8]) -> std::path::PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, data).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            path
        }
    }
    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn admission_and_cancel_happen_before_file_open() {
        let path = Path::new("/definitely/missing/congminal-key");
        let pool = Budget::new(0, 0);
        let token = Cancellation::default();
        token.cancel();
        assert!(matches!(Blob::read(path, Purpose::PrivateKey, &pool, &token), Err(Error::Cancelled)));
        assert!(matches!(Blob::read(path, Purpose::PrivateKey, &pool, &Cancellation::default()), Err(Error::Budget)));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn binary_records_empty_records_and_size_limits_are_bounded() {
        let folder = Folder::new();
        let pool = Budget::new(2 * LIMIT, 0);
        let token = Cancellation::default();
        let file = folder.file("known", b"host key\0binary\n");
        let result = Blob::read(&file, Purpose::HostRecords, &pool, &token).unwrap();
        assert_eq!(&*result, b"host key\0binary\n");
        assert!(pool.snapshot().used < 4096);
        drop(result);
        let file = folder.file("empty", b"");
        assert!(Blob::read(&file, Purpose::HostRecords, &pool, &token).unwrap().is_empty());
        assert!(matches!(Blob::read(&file, Purpose::PrivateKey, &pool, &token), Err(Error::Credential)));
        let file = folder.file("large", b"");
        File::options().write(true).open(&file).unwrap().set_len((LIMIT + 1) as u64).unwrap();
        assert!(matches!(Blob::read(&file, Purpose::HostRecords, &pool, &token), Err(Error::Limit)));
        assert!(matches!(Blob::read(&folder.0, Purpose::HostRecords, &pool, &token), Err(Error::FileType)));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn shortened_grown_and_cancelled_inputs_never_escape_as_complete() {
        let folder = Folder::new();
        let pool = Budget::new(2 * LIMIT, 0);
        let token = Cancellation::default();
        for length in [2, 9] {
            let path = folder.file("changing", b"12345");
            let result = Blob::read_with(&path, Purpose::HostRecords, &pool, &token, |_| {
                File::options().write(true).open(&path).unwrap().set_len(length).unwrap();
            });
            assert!(matches!(result, Err(Error::FileChanged)));
            assert_eq!(pool.snapshot().used, 0);
        }
        let path = folder.file("cancel", b"data");
        assert!(matches!(
            Blob::read_with(&path, Purpose::HostRecords, &pool, &token, |_| token.cancel()),
            Err(Error::Cancelled)
        ));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[cfg(unix)]
    #[test]
    fn private_input_is_zeroed_and_insecure_or_fifo_inputs_are_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let folder = Folder::new();
        let pool = Budget::new(2 * LIMIT, 0);
        let token = Cancellation::default();
        let path = folder.file("key", b"private\0key");
        let mut result = Blob::read(&path, Purpose::PrivateKey, &pool, &token).unwrap();
        assert_eq!(&*result, b"private\0key");
        result.clear();
        assert!(result.iter().all(|b| *b == 0));
        drop(result);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(Blob::read(&path, Purpose::PrivateKey, &pool, &token), Err(Error::FilePermissions)));
        let fifo = folder.0.join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let start = std::time::Instant::now();
        assert!(matches!(Blob::read(&fifo, Purpose::HostRecords, &pool, &token), Err(Error::FileType)));
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(pool.snapshot().used, 0);
    }
}
