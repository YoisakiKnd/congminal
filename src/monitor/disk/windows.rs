use super::*;
use std::{ffi::c_void, os::windows::ffi::OsStringExt};
#[link(name = "kernel32")]
unsafe extern "system" {
    fn FindFirstVolumeW(name: *mut u16, size: u32) -> *mut c_void;
    fn FindNextVolumeW(handle: *mut c_void, name: *mut u16, size: u32) -> i32;
    fn FindVolumeClose(handle: *mut c_void) -> i32;
    fn GetLastError() -> u32;
    fn GetVolumePathNamesForVolumeNameW(volume: *const u16, paths: *mut u16, size: u32, needed: *mut u32) -> i32;
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn GetVolumeInformationW(
        root: *const u16,
        name: *mut u16,
        name_size: u32,
        serial: *mut u32,
        component: *mut u32,
        flags: *mut u32,
        fs: *mut u16,
        fs_size: u32,
    ) -> i32;
    fn GetDiskFreeSpaceExW(root: *const u16, caller: *mut u64, total: *mut u64, free: *mut u64) -> i32;
}
struct Search(*mut c_void);
impl Drop for Search {
    fn drop(&mut self) {
        unsafe {
            FindVolumeClose(self.0);
        }
    }
}
pub(super) fn collect(table: &mut Table, pool: &Arc<Budget>, cancel: &Cancellation) -> Result<(), Error> {
    let mut volume = [0u16; 1024];
    let raw = unsafe { FindFirstVolumeW(volume.as_mut_ptr(), volume.len() as u32) };
    if raw as isize == -1 {
        return if unsafe { GetLastError() } == 18 { Ok(()) } else { Err(Error::Read) };
    }
    let handle = Search(raw);
    let mut count = 0;
    loop {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        count += 1;
        if count > MOUNTS {
            return Err(Error::Limit);
        }
        if volume[0] == 0 || !volume.contains(&0) {
            return Err(Error::Limit);
        }
        let kind = unsafe { GetDriveTypeW(volume.as_ptr()) };
        if matches!(kind, 2 | 3) {
            collect_volume(table, pool, &volume, kind)?;
        }
        volume.fill(0);
        if unsafe { FindNextVolumeW(handle.0, volume.as_mut_ptr(), volume.len() as u32) } == 0 {
            return if unsafe { GetLastError() } == 18 { Ok(()) } else { Err(Error::Read) };
        }
    }
}

fn collect_volume(table: &mut Table, pool: &Arc<Budget>, volume: &[u16], kind: u32) -> Result<(), Error> {
    let mut paths = [0u16; 8192];
    let mut needed = 0;
    if unsafe { GetVolumePathNamesForVolumeNameW(volume.as_ptr(), paths.as_mut_ptr(), paths.len() as u32, &mut needed) }
        == 0
    {
        return Err(if unsafe { GetLastError() } == 234 { Error::Limit } else { Error::Read });
    }
    if needed == 0 || needed as usize > paths.len() {
        return Err(Error::Limit);
    }
    if paths[0] == 0 {
        return Ok(());
    }
    let mut flags = 0;
    if unsafe {
        GetVolumeInformationW(
            volume.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            std::ptr::null_mut(),
            0,
        )
    } == 0
    {
        return if kind == 2 && unsafe { GetLastError() } == 21 { Ok(()) } else { Err(Error::Read) };
    }
    if flags & 0x80000 != 0 {
        return Ok(());
    }
    let mut total = 0;
    let mut free = 0;
    if unsafe { GetDiskFreeSpaceExW(volume.as_ptr(), std::ptr::null_mut(), &mut total, &mut free) } == 0 {
        return Err(Error::Read);
    }
    let mut start = 0;
    let end = needed as usize;
    while start < end && paths[start] != 0 {
        let length = paths[start..end].iter().position(|c| *c == 0).ok_or(Error::Read)?;
        if length > PATH_BYTES {
            return Err(Error::Limit);
        }
        let memory = pool.lease(Kind::Monitor, length * 6 + 128).map_err(|_| Error::Budget)?;
        let path = Tracked::new(std::ffi::OsString::from_wide(&paths[start..start + length]), memory);
        table.push(&path, total, free)?;
        start += length + 1;
    }
    if start >= end || paths[start] != 0 {
        return Err(Error::Read);
    }
    Ok(())
}
