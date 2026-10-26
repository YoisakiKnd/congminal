//! 共享的只读字体数据。表切片持有同一份映射，最后一个使用者释放时解除映射。
//! 弱缓存只复用仍在使用且元数据未变的文件，不让热加载永久保留旧字体。

use std::fs::File;
use std::io::{self, Read};
use std::ops::{Deref, Range};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
#[path = "mmap_cache.rs"]
mod cache;
#[path = "mmap_lock.rs"]
mod lock;
use crate::resources::{Budget, Kind, Lease};

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
pub fn live_bytes() -> usize {
    LIVE_BYTES.load(Ordering::Relaxed)
}

const WORK_BYTES: usize = 64 * 1024;
const PATH_LIMIT: usize = 16 * 1024;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
fn next_id(counter: &AtomicU64) -> Result<u64, Error> {
    let mut id = counter.load(Ordering::Relaxed);
    loop {
        let next = id.checked_add(1).ok_or(Error::IdentityExhausted)?;
        match counter.compare_exchange_weak(id, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Ok(id),
            Err(current) => id = current,
        }
    }
}

const MAX_FILE_SIZE: u64 = 512 * 1024 * 1024;

#[derive(Clone)]
pub struct Bytes {
    owner: Shared,
    range: Range<usize>,
}

#[derive(Debug)]
pub(super) enum Error {
    Io { code: Option<i32>, kind: io::ErrorKind },
    Budget,
    InvalidPath,
    FileType,
    TooLarge,
    IdentityExhausted,
}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io { code: e.raw_os_error(), kind: e.kind() }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { code: Some(code), .. } => write!(f, "字体文件系统错误 {code}"),
            Self::Io { kind, .. } => write!(f, "字体文件系统错误 {kind:?}"),
            Self::Budget => f.write_str("字体共享资源额度不足"),
            Self::InvalidPath => f.write_str("字体路径无效或超过长度上限"),
            Self::FileType => f.write_str("字体文件不是普通文件"),
            Self::TooLarge => f.write_str("字体文件超过512MiB上限"),
            Self::IdentityExhausted => f.write_str("字体拥有者标识已耗尽"),
        }
    }
}
#[derive(Clone)]
struct Shared(Option<Arc<Storage>>);
impl Deref for Shared {
    type Target = Storage;
    fn deref(&self) -> &Storage {
        self.0.as_ref().unwrap()
    }
}
impl Drop for Shared {
    fn drop(&mut self) {
        if let Some(storage) = Arc::into_inner(self.0.take().unwrap()) {
            // 唯一最后拥有者先退役弱引用；Arc块先释放，再释放数据和最后的元数据额度。
            // 用不复用的id移除，避免Arc块已释放/地址复用时误删另一份缓存。
            if storage.cached.load(Ordering::Relaxed) {
                cache::remove(storage.id);
            }
            drop(storage);
        }
    }
}
pub(super) const fn storage_bytes() -> usize {
    std::mem::size_of::<Storage>() + 32
}

enum Data {
    Mapped(Mapping),
    Owned(Box<[u8]>),
}
struct Storage {
    data: Data,
    // 字节/映射先销毁，最后归还额度；切片只共享这一份票据。
    memory: Lease,
    id: u64,
    cached: AtomicBool,
    metadata: Option<Lease>,
}

impl Storage {
    fn new(data: Data, memory: Lease) -> Result<Self, Error> {
        let id = next_id(&NEXT_ID)?;
        let storage = Self { data, memory, id, cached: AtomicBool::new(false), metadata: None };
        LIVE_BYTES.fetch_add(storage.len(), Ordering::Relaxed);
        Ok(storage)
    }
    fn len(&self) -> usize {
        match &self.data {
            Data::Mapped(m) => m.len,
            Data::Owned(b) => b.len(),
        }
    }
    fn shared(mut self) -> Result<Shared, Error> {
        self.metadata =
            Some(self.memory.budget().lease(Kind::FontMetadata, storage_bytes()).map_err(|_| Error::Budget)?);
        Ok(Shared(Some(Arc::new(self))))
    }
}
impl Drop for Storage {
    fn drop(&mut self) {
        LIVE_BYTES.fetch_sub(self.len(), Ordering::Relaxed);
    }
}

impl Bytes {
    #[cfg(test)]
    pub(super) fn owner_id(&self) -> u64 {
        self.owner.id
    }
    pub(super) fn budget(&self) -> &Arc<crate::resources::Budget> {
        self.owner.memory.budget()
    }
    pub fn slice(&self, range: Range<usize>) -> Option<Self> {
        self.get(range.clone())?;
        Some(Self { owner: self.owner.clone(), range: self.range.start + range.start..self.range.start + range.end })
    }
}

#[cfg(test)]
impl From<Vec<u8>> for Bytes {
    fn from(data: Vec<u8>) -> Self {
        let len = data.len();
        let memory = crate::resources::current().lease(crate::resources::Kind::FontOwned, len).unwrap();
        Self {
            owner: Storage::new(Data::Owned(data.into_boxed_slice()), memory).unwrap().shared().unwrap(),
            range: 0..len,
        }
    }
}

impl Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        let data = match &self.owner.data {
            Data::Mapped(mapping) => {
                // SAFETY: owner 保持映射存活；len 是创建映射时通过检查的文件长度。
                unsafe { std::slice::from_raw_parts(mapping.pointer.cast(), mapping.len) }
            }
            Data::Owned(data) => data,
        };
        &data[self.range.clone()]
    }
}

pub fn map(path: &Path) -> Result<Bytes, Error> {
    if path.as_os_str().is_empty()
        || path.as_os_str().len() > PATH_LIMIT
        || path.as_os_str().as_encoded_bytes().contains(&0)
    {
        return Err(Error::InvalidPath);
    }
    let budget = crate::resources::current();
    let _work = budget.lease(Kind::FontMetadata, WORK_BYTES).map_err(|_| Error::Budget)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(if cfg!(target_os = "macos") { 4 } else { 2048 });
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    let len = metadata.len();
    if !metadata.is_file() {
        return Err(Error::FileType);
    }
    if len > MAX_FILE_SIZE {
        return Err(Error::TooLarge);
    }
    let key = cache::key(&file, &metadata, &budget)
        .zip(metadata.modified().ok())
        .map(|(key, modified)| (key, cache::Stamp { len, modified }));
    if let Some((key, stamp)) = key
        && let Some(owner) = cache::get(key, stamp)
    {
        return Ok(Bytes { owner, range: 0..len as usize });
    }
    let storage = if len == 0 {
        Storage::new(Data::Owned(Box::new([])), budget.lease(Kind::FontOwned, 0).unwrap())?
    } else {
        let memory = budget.lease(Kind::FontMap, len as usize).map_err(|_| Error::Budget)?;
        // SAFETY: 已验证普通文件/非零长度/512MiB界限，只映射这个长度。
        match unsafe { map_file(&file, len as usize) } {
            Ok(mapping) => Storage::new(Data::Mapped(mapping), memory)?,
            Err(_) => {
                drop(memory);
                read_owned(file, len as usize, &budget)?
            }
        }
    };
    let owner = storage.shared()?;
    let owner = if let Some((key, stamp)) = key { cache::publish(key, stamp, owner) } else { owner };
    Ok(Bytes { owner, range: 0..len as usize })
}
fn read_owned(mut file: File, len: usize, budget: &Arc<Budget>) -> Result<Storage, Error> {
    let mut memory = budget.lease(Kind::FontOwned, len).map_err(|_| Error::Budget)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(|_| Error::Budget)?;
    memory.resize(bytes.capacity()).map_err(|_| Error::Budget)?;
    bytes.resize(len, 0);
    file.read_exact(&mut bytes)?;
    Storage::new(Data::Owned(bytes.into_boxed_slice()), memory)
}
#[cfg(test)]
pub(super) fn owned_probe(file: File, len: usize, budget: &Arc<Budget>) -> Bytes {
    Bytes { owner: read_owned(file, len, budget).unwrap().shared().unwrap(), range: 0..len }
}

struct Mapping {
    pointer: *mut std::ffi::c_void,
    len: usize,
}

// SAFETY: 只读映射没有可变引用；Arc 保证任何线程访问时映射都存活。
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

#[cfg(unix)]
unsafe fn map_file(file: &File, len: usize) -> io::Result<Mapping> {
    use std::ffi::{c_int, c_void};
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, offset: i64) -> *mut c_void;
    }
    let pointer = unsafe { mmap(std::ptr::null_mut(), len, 1, 2, file.as_raw_fd(), 0) };
    if pointer as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(Mapping { pointer, len })
}

#[cfg(unix)]
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe extern "C" {
            fn munmap(addr: *mut std::ffi::c_void, len: usize) -> i32;
        }
        // SAFETY: 这是 mmap 返回的地址和长度，最后一个 Arc 才会调用这里。
        unsafe {
            munmap(self.pointer, self.len);
        }
    }
}

#[cfg(windows)]
unsafe fn map_file(file: &File, len: usize) -> io::Result<Mapping> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileMappingW(
            file: *mut c_void,
            attributes: *const c_void,
            protect: u32,
            high: u32,
            low: u32,
            name: *const u16,
        ) -> *mut c_void;
        fn MapViewOfFile(mapping: *mut c_void, access: u32, high: u32, low: u32, len: usize) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    unsafe {
        let mapping = CreateFileMappingW(file.as_raw_handle(), std::ptr::null(), 2, 0, 0, std::ptr::null());
        if mapping.is_null() {
            return Err(io::Error::last_os_error());
        }
        let pointer = MapViewOfFile(mapping, 4, 0, 0, len);
        let error = io::Error::last_os_error();
        CloseHandle(mapping);
        if pointer.is_null() {
            return Err(error);
        }
        Ok(Mapping { pointer, len })
    }
}

#[cfg(windows)]
impl Drop for Mapping {
    fn drop(&mut self) {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn UnmapViewOfFile(addr: *const std::ffi::c_void) -> i32;
        }
        unsafe {
            UnmapViewOfFile(self.pointer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_mapping_is_charged_once_until_last_slice_and_denial_does_not_copy() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/fonts/Terminus16.psf");
        let len = std::fs::metadata(&path).unwrap().len() as usize;
        let pool = crate::resources::Budget::new(WORK_BYTES + storage_bytes(), len);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let first = map(&path).unwrap();
        let second = map(&path).unwrap();
        assert_eq!(first.owner_id(), second.owner_id());
        assert_eq!(pool.snapshot().kinds[4], len);
        let slice = first.slice(0..4).unwrap();
        drop(first);
        drop(second);
        assert_eq!(pool.snapshot().mappings, 1);
        drop(slice);
        assert_eq!(pool.snapshot().kinds[4], 0);
        let denied = crate::resources::Budget::new(WORK_BYTES + storage_bytes(), len - 1);
        let _scope = crate::resources::Scope::enter(denied.clone());
        assert!(map(&path).is_err());
        assert_eq!(denied.snapshot().used, 0);
        assert_eq!(denied.snapshot().mappings, 0);
    }
    #[test]
    fn owned_fallback_reserves_before_read_and_returns_credit_on_error() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/fonts/Terminus16.psf");
        let len = std::fs::metadata(&path).unwrap().len() as usize;
        let small = crate::resources::Budget::new(len - 1, 0);
        assert!(read_owned(File::open(&path).unwrap(), len, &small).is_err());
        assert_eq!(small.snapshot().used, 0);
        let pool = crate::resources::Budget::new(len + storage_bytes() + 1, 0);
        assert!(read_owned(File::open(&path).unwrap(), len + 1, &pool).is_err());
        assert_eq!(pool.snapshot().used, 0);
        let owner = owned_probe(File::open(path).unwrap(), len, &pool);
        assert_eq!(pool.snapshot().kinds[3], len);
        drop(owner);
        assert_eq!(pool.snapshot().used, 0);
    }

    #[test]
    fn slices_own_their_mapping_and_reloads_reuse_it() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/fonts/Terminus16.psf");
        let first = map(&path).unwrap();
        let second = map(&path).unwrap();
        assert!(Arc::ptr_eq(first.owner.0.as_ref().unwrap(), second.owner.0.as_ref().unwrap()));
        let pool = first.owner.memory.budget().clone();
        let table = first.slice(0..4).unwrap();
        let expected = first[..4].to_vec();
        assert!(first.slice(0..first.len() + 1).is_none());
        drop(first);
        drop(second);
        assert_eq!(&*table, &expected);
        drop(table);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn identities_never_wrap_or_reuse_the_exhausted_generation() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(next_id(&counter).unwrap(), u64::MAX - 1);
        assert!(matches!(next_id(&counter), Err(Error::IdentityExhausted)));
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }
    #[cfg(unix)]
    #[test]
    fn aliases_share_but_file_replacement_and_other_budgets_do_not() {
        struct Folder(std::path::PathBuf);
        impl Drop for Folder {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let folder = Folder(std::env::temp_dir().join(format!(
            "cg-font-map-{}-{}",
            std::process::id(),
            next_id(&NEXT_ID).unwrap()
        )));
        std::fs::create_dir(&folder.0).unwrap();
        let path = folder.0.join("font");
        let alias = folder.0.join("alias");
        std::fs::write(&path, b"old!").unwrap();
        std::fs::hard_link(&path, &alias).unwrap();
        let pool = Budget::new(1024 * 1024, 1024);
        let _scope = crate::resources::Scope::enter(pool.clone());
        let first = map(&path).unwrap();
        let aliased = map(&alias).unwrap();
        assert_eq!(first.owner_id(), aliased.owner_id());
        let replacement = folder.0.join("replacement");
        std::fs::write(&replacement, b"new!").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let changed = map(&path).unwrap();
        assert_ne!(first.owner.id, changed.owner.id);
        assert_eq!(&*first, b"old!");
        assert_eq!(&*changed, b"new!");
        let other = Budget::new(1024 * 1024, 1024);
        let scope = crate::resources::Scope::enter(other.clone());
        let separate = map(&alias).unwrap();
        assert_ne!(separate.owner.id, first.owner.id);
        drop(separate);
        assert_eq!(other.snapshot().used, 0);
        drop(scope);
        drop((first, aliased, changed));
        assert_eq!(pool.snapshot().used, 0);
        assert_eq!(pool.snapshot().mappings, 0);
    }
    #[test]
    fn concurrent_cold_publish_and_last_references_return_all_credit() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/fonts/Terminus16.psf");
        let pool = Budget::new(4 * 1024 * 1024, 16 * 1024 * 1024);
        for _ in 0..8 {
            let gate = Arc::new(std::sync::Barrier::new(4));
            let mut threads = Vec::new();
            for _ in 0..4 {
                let (path, pool, gate) = (path.clone(), pool.clone(), gate.clone());
                threads.push(std::thread::spawn(move || {
                    let _scope = crate::resources::Scope::enter(pool);
                    gate.wait();
                    let bytes = map(&path).unwrap();
                    let id = bytes.owner.id;
                    gate.wait();
                    drop(bytes);
                    id
                }));
            }
            let ids: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
            assert!(ids.iter().all(|id| *id == ids[0]));
            assert_eq!(pool.snapshot().used, 0);
            assert_eq!(pool.snapshot().mappings, 0);
        }
    }
}
