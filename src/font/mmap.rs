//! 共享的只读字体数据。表切片持有同一份映射，最后一个使用者释放时解除映射。
//! 弱缓存只复用仍在使用且元数据未变的文件，不让热加载永久保留旧字体。

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::ops::{Deref, Range};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
pub fn live_bytes() -> usize {
    LIVE_BYTES.load(Ordering::Relaxed)
}
use std::time::SystemTime;

const MAX_FILE_SIZE: u64 = 512 * 1024 * 1024;

#[derive(Clone)]
pub struct Bytes {
    owner: Arc<Storage>,
    range: Range<usize>,
}

enum Storage {
    Mapped(Mapping),
    Owned(Box<[u8]>),
}

impl Storage {
    fn len(&self) -> usize {
        match self {
            Self::Mapped(m) => m.len,
            Self::Owned(b) => b.len(),
        }
    }
    fn shared(self) -> Arc<Self> {
        LIVE_BYTES.fetch_add(self.len(), Ordering::Relaxed);
        Arc::new(self)
    }
}
impl Drop for Storage {
    fn drop(&mut self) {
        LIVE_BYTES.fetch_sub(self.len(), Ordering::Relaxed);
    }
}

impl Bytes {
    pub fn slice(&self, range: Range<usize>) -> Option<Self> {
        self.get(range.clone())?;
        Some(Self { owner: self.owner.clone(), range: self.range.start + range.start..self.range.start + range.end })
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(data: Vec<u8>) -> Self {
        let len = data.len();
        Self { owner: Storage::Owned(data.into_boxed_slice()).shared(), range: 0..len }
    }
}

impl Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        let data = match &*self.owner {
            Storage::Mapped(mapping) => {
                // SAFETY: owner 保持映射存活；len 是创建映射时通过检查的文件长度。
                unsafe { std::slice::from_raw_parts(mapping.pointer.cast(), mapping.len) }
            }
            Storage::Owned(data) => data,
        };
        &data[self.range.clone()]
    }
}

struct Entry {
    len: u64,
    modified: Option<SystemTime>,
    owner: Weak<Storage>,
}

pub fn map(path: &Path) -> io::Result<Bytes> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Entry>>> = OnceLock::new();
    let path = path.canonicalize()?;
    let file = File::open(&path)?;
    let metadata = file.metadata()?;
    let len = metadata.len();
    if !metadata.is_file() || len > MAX_FILE_SIZE {
        return Err(io::Error::other("字体文件过大或不是普通文件"));
    }
    let modified = metadata.modified().ok();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    cache.retain(|_, entry| entry.owner.strong_count() > 0);
    let owner = cache
        .get(&path)
        .filter(|entry| entry.len == len && entry.modified == modified)
        .and_then(|entry| entry.owner.upgrade());
    let owner = match owner {
        Some(owner) => owner,
        None => {
            let storage = if len == 0 {
                Storage::Owned(Box::new([]))
            } else {
                // SAFETY: 文件是普通文件，长度非零且不超过 512 MiB；映射仅按此长度访问。
                match unsafe { map_file(&file, len as usize) } {
                    Ok(mapping) => Storage::Mapped(mapping),
                    Err(_) => {
                        // 只读取元数据声明的长度，不用 read_to_end 的倍增扩容。
                        let mut bytes = Vec::new();
                        bytes.try_reserve_exact(len as usize).map_err(|_| io::Error::other("无法分配字体缓冲区"))?;
                        bytes.resize(len as usize, 0);
                        let mut reader = file;
                        reader.read_exact(&mut bytes)?;
                        Storage::Owned(bytes.into_boxed_slice())
                    }
                }
            };
            let owner = storage.shared();
            // 缓存本身也有上限。已发出的 Bytes 不依赖缓存条目维持有效性。
            if cache.len() < 128 {
                cache.insert(path, Entry { len, modified, owner: Arc::downgrade(&owner) });
            }
            owner
        }
    };
    let len = match &*owner {
        Storage::Mapped(m) => m.len,
        Storage::Owned(b) => b.len(),
    };
    Ok(Bytes { owner, range: 0..len })
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
    fn slices_own_their_mapping_and_reloads_reuse_it() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/fonts/Terminus16.psf");
        let first = map(&path).unwrap();
        let second = map(&path).unwrap();
        assert!(Arc::ptr_eq(&first.owner, &second.owner));
        let weak = Arc::downgrade(&first.owner);
        let table = first.slice(0..4).unwrap();
        let expected = first[..4].to_vec();
        assert!(first.slice(0..first.len() + 1).is_none());
        drop(first);
        drop(second);
        assert_eq!(&*table, &expected);
        drop(table);
        assert!(weak.upgrade().is_none());
    }
}
