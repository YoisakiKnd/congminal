use super::*;
use std::{ffi::c_void, mem::MaybeUninit};
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFURLCreateFromFileSystemRepresentation(
        allocator: *const c_void,
        path: *const u8,
        length: isize,
        directory: u8,
    ) -> *const c_void;
    fn CFURLCopyResourcePropertyForKey(
        url: *const c_void,
        key: *const c_void,
        value: *mut *const c_void,
        error: *mut *const c_void,
    ) -> u8;
    fn CFRelease(value: *const c_void);
    fn CFGetTypeID(value: *const c_void) -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(value: *const c_void) -> u8;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(value: *const c_void, kind: isize, out: *mut c_void) -> u8;
    static kCFURLVolumeIsBrowsableKey: *const c_void;
    static kCFURLVolumeAvailableCapacityForImportantUsageKey: *const c_void;
}
#[link(name = "objc")]
unsafe extern "C" {
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}
struct Autorelease(*mut c_void);
impl Drop for Autorelease {
    fn drop(&mut self) {
        unsafe {
            objc_autoreleasePoolPop(self.0);
        }
    }
}
struct Cf(*const c_void);
impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}
fn property(path: &[u8], key: *const c_void) -> Option<Cf> {
    unsafe {
        let url = Cf(CFURLCreateFromFileSystemRepresentation(std::ptr::null(), path.as_ptr(), path.len() as isize, 1));
        if url.0.is_null() {
            return None;
        }
        let mut value = std::ptr::null();
        let mut error = std::ptr::null();
        let ok = CFURLCopyResourcePropertyForKey(url.0, key, &mut value, &mut error);
        let _error = Cf(error);
        let value = Cf(value);
        if ok == 0 || value.0.is_null() { None } else { Some(value) }
    }
}
fn bytes(chars: &[i8]) -> Option<&[u8]> {
    let end = chars.iter().position(|c| *c == 0)?;
    Some(unsafe { std::slice::from_raw_parts(chars.as_ptr().cast(), end) })
}
pub(super) fn collect(table: &mut Table, pool: &Arc<Budget>, cancel: &Cancellation) -> Result<(), Error> {
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if count < 0 {
        return Err(Error::Read);
    }
    if count as usize > MOUNTS {
        return Err(Error::Limit);
    }
    let mut raw =
        Tracked::new(Vec::<MaybeUninit<libc::statfs>>::new(), pool.lease(Kind::Monitor, 0).map_err(|_| Error::Budget)?);
    raw.reserve_capacity(count as usize + 1).ok_or(Error::Budget)?;
    let length = raw.data.capacity() * std::mem::size_of::<libc::statfs>();
    let read = unsafe { libc::getfsstat(raw.data.as_mut_ptr().cast(), length as i32, libc::MNT_NOWAIT) };
    if read < 0 {
        return Err(Error::Read);
    }
    if read as usize >= raw.data.capacity() || read as usize > MOUNTS {
        return Err(Error::Limit);
    }
    unsafe { raw.data.set_len(read as usize) };
    // URL/number/bool ownership is fixed per query; reserve opaque native work
    // separately. This models CF working storage, not every native allocator byte.
    let _native = pool.lease(Kind::Monitor, 64 * 1024).map_err(|_| Error::Budget)?;
    for entry in raw.iter() {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let entry = unsafe { entry.assume_init_ref() };
        if entry.f_flags & libc::MNT_LOCAL as u32 == 0 || entry.f_flags & libc::MNT_RDONLY as u32 != 0 {
            continue;
        }
        let path = bytes(&entry.f_mntonname).ok_or(Error::Read)?;
        if path.len() > PATH_BYTES {
            return Err(Error::Limit);
        }
        // CF's Foundation internals may autorelease objects on this worker.
        // Drain each volume before releasing the opaque native-work reservation.
        let _autorelease = Autorelease(unsafe { objc_autoreleasePoolPush() });
        let browsable = property(path, unsafe { kCFURLVolumeIsBrowsableKey }).ok_or(Error::Read)?;
        if unsafe { CFGetTypeID(browsable.0) } != unsafe { CFBooleanGetTypeID() } {
            return Err(Error::Read);
        }
        if unsafe { CFBooleanGetValue(browsable.0) } == 0 {
            continue;
        }
        let mut current: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(entry.f_mntonname.as_ptr(), &mut current) } != 0 {
            return Err(Error::Read);
        }
        if current.f_flags & libc::MNT_RDONLY as u32 != 0 {
            continue;
        }
        let total = current.f_blocks.checked_mul(u64::from(current.f_bsize)).ok_or(Error::Read)?;
        if total == 0 {
            continue;
        }
        let mut free = current.f_bavail.checked_mul(u64::from(current.f_bsize)).ok_or(Error::Read)?;
        if let Some(value) = property(path, unsafe { kCFURLVolumeAvailableCapacityForImportantUsageKey }) {
            let mut n = 0i64;
            if unsafe { CFGetTypeID(value.0) } == unsafe { CFNumberGetTypeID() }
                && unsafe { CFNumberGetValue(value.0, 4, (&mut n as *mut i64).cast()) } != 0
                && n > 0
            {
                free = n as u64
            }
        }
        use std::os::unix::ffi::OsStrExt;
        table.push(OsStr::from_bytes(path), total, free)?;
    }
    let after = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if after < 0 {
        return Err(Error::Read);
    }
    if after > read {
        return Err(Error::Limit);
    }
    Ok(())
}
