//! 固定槽弱缓存；只在短暂查找/发布/移除时持锁，不持锁执行文件IO。
use super::{Shared, Storage, lock::Mutex};
use std::{
    fs::{File, Metadata},
    sync::{Arc, Weak},
    time::SystemTime,
};
const SLOTS: usize = 128;
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Key {
    pool: usize,
    volume: u64,
    file: [u8; 16],
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Stamp {
    pub len: u64,
    pub modified: SystemTime,
}
struct Entry {
    key: Key,
    stamp: Stamp,
    owner: Weak<Storage>,
    id: u64,
}
struct Cache {
    slots: [Option<Entry>; SLOTS],
}
impl Cache {
    const fn new() -> Self {
        Self { slots: [const { None }; SLOTS] }
    }
    fn get(&self, key: Key, stamp: Stamp) -> Option<Shared> {
        self.slots
            .iter()
            .flatten()
            .find(|e| e.key == key && e.stamp == stamp)
            .and_then(|e| e.owner.upgrade())
            .map(|owner| Shared(Some(owner)))
    }
    fn insert(&mut self, key: Key, stamp: Stamp, owner: &Shared) {
        let index = self
            .slots
            .iter()
            .position(|s| s.as_ref().is_some_and(|e| e.key == key))
            .or_else(|| self.slots.iter().position(Option::is_none));
        if let Some(index) = index {
            owner.cached.store(true, std::sync::atomic::Ordering::Relaxed);
            self.slots[index] =
                Some(Entry { key, stamp, owner: Arc::downgrade(owner.0.as_ref().unwrap()), id: owner.id });
        }
    }
    fn remove(&mut self, id: u64) {
        for slot in &mut self.slots {
            if slot.as_ref().is_some_and(|e| e.id == id) {
                drop(slot.take());
            }
        }
    }
}
static CACHE: Mutex<Cache> = Mutex::new(Cache::new());
pub(super) fn get(key: Key, stamp: Stamp) -> Option<Shared> {
    CACHE.lock().get(key, stamp)
}
pub(super) fn publish(key: Key, stamp: Stamp, owner: Shared) -> Shared {
    let existing = {
        let mut cache = CACHE.lock();
        let existing = cache.get(key, stamp);
        if existing.is_none() {
            cache.insert(key, stamp, &owner);
        }
        existing
    };
    // 新旧Shared的最后释放会访问缓存，必须先退出上面的锁域。
    existing.unwrap_or(owner)
}
pub(super) fn remove(id: u64) {
    CACHE.lock().remove(id);
}
pub(super) fn key(file: &File, metadata: &Metadata, pool: &Arc<crate::resources::Budget>) -> Option<Key> {
    let (volume, file) = identity(file, metadata)?;
    Some(Key { pool: Arc::as_ptr(pool) as usize, volume, file })
}
#[cfg(unix)]
fn identity(_file: &File, metadata: &Metadata) -> Option<(u64, [u8; 16])> {
    use std::os::unix::fs::MetadataExt;
    if metadata.ino() == 0 {
        return None;
    }
    let mut id = [0; 16];
    id[..8].copy_from_slice(&metadata.ino().to_le_bytes());
    Some((metadata.dev(), id))
}
#[cfg(windows)]
fn identity(file: &File, _metadata: &Metadata) -> Option<(u64, [u8; 16])> {
    use std::{ffi::c_void, os::windows::io::AsRawHandle};
    // FILE_ID_INFO / FileIdInfo(18)，使用完整128位ID；不支持时只是不缓存。
    // https://learn.microsoft.com/windows/win32/api/winbase/ns-winbase-file_id_info
    #[repr(C)]
    struct Info {
        volume: u64,
        id: [u8; 16],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandleEx(file: *mut c_void, class: i32, info: *mut c_void, size: u32) -> i32;
    }
    let mut info = Info { volume: 0, id: [0; 16] };
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            18,
            (&mut info as *mut Info).cast(),
            std::mem::size_of::<Info>() as u32,
        )
    };
    (ok != 0 && info.id != [0; 16]).then_some((info.volume, info.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::mmap::{Data, storage_bytes};
    use crate::resources::{Budget, Kind};
    #[test]
    fn fixed_cache_full_replacement_holes_and_old_generation_removal() {
        let pool = Budget::new(65536, 0);
        let mut owners = Vec::new();
        let mut cache = Cache::new();
        let stamp = Stamp { len: 0, modified: SystemTime::UNIX_EPOCH };
        let key = |n| Key { pool: 1, volume: n, file: [1; 16] };
        for n in 0..SLOTS + 1 {
            let owner = Storage::new(Data::Owned(Box::new([])), pool.lease(Kind::FontOwned, 0).unwrap())
                .unwrap()
                .shared()
                .unwrap();
            cache.insert(key(n as u64), stamp, &owner);
            owners.push(owner);
        }
        assert_eq!(cache.slots.iter().flatten().count(), SLOTS);
        assert!(cache.get(key(SLOTS as u64), stamp).is_none());
        assert_eq!(pool.snapshot().used, owners.len() * storage_bytes());
        // 在旧key前留下空洞，更新必须复用已有key的槽，而不是产生重复。
        cache.remove(owners[0].id);
        let replacement =
            Storage::new(Data::Owned(Box::new([])), pool.lease(Kind::FontOwned, 0).unwrap()).unwrap().shared().unwrap();
        cache.insert(key(1), stamp, &replacement);
        cache.remove(owners[1].id);
        assert_eq!(cache.slots.iter().flatten().filter(|e| e.key == key(1)).count(), 1);
        let got = cache.get(key(1), stamp).unwrap();
        assert_eq!(got.id, replacement.id);
        drop(got);
        // 此夹具的局部缓存先退役所有弱引用，再销毁拥有者；生产由Shared最后释放退役全局槽。
        drop(cache);
        for owner in owners.iter().chain(std::iter::once(&replacement)) {
            owner.cached.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        drop((owners, replacement));
        assert_eq!(pool.snapshot().used, 0);
    }
}
