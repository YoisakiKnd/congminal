//! 把字体文件映射进内存（mmap），而不是整个读进来。
//!
//! Apple Color Emoji 有将近 200 MB，整个读进来太浪费；映射之后，只有真正用到的那几页才会被系统从磁盘读入。
//! 映射一直不解除（字体在程序运行期间一直要用），所以可以当作 'static。

use std::fs::File;
use std::io;
use std::path::Path;

pub fn map(path: &Path) -> io::Result<&'static [u8]> {
    let file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if len == 0 {
        return Ok(&[]);
    }
    // 映射建立之后，关掉文件句柄也不影响
    unsafe { map_file(&file, len) }
}

#[cfg(unix)]
unsafe fn map_file(file: &File, len: usize) -> io::Result<&'static [u8]> {
    use std::ffi::{c_int, c_void};
    use std::os::fd::AsRawFd;

    const PROT_READ: c_int = 1;
    const MAP_PRIVATE: c_int = 2;
    unsafe extern "C" {
        fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, offset: i64) -> *mut c_void;
    }
    unsafe {
        let pointer = mmap(std::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, file.as_raw_fd(), 0);
        // 失败时返回 MAP_FAILED，也就是 (void *)-1
        if pointer as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(std::slice::from_raw_parts(pointer.cast(), len))
    }
}

#[cfg(windows)]
unsafe fn map_file(file: &File, len: usize) -> io::Result<&'static [u8]> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    const PAGE_READONLY: u32 = 0x02;
    const FILE_MAP_READ: u32 = 0x04;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileMappingW(
            file: *mut c_void,
            attributes: *const c_void,
            protect: u32,
            size_high: u32,
            size_low: u32,
            name: *const u16,
        ) -> *mut c_void;
        fn MapViewOfFile(
            mapping: *mut c_void,
            access: u32,
            offset_high: u32,
            offset_low: u32,
            len: usize,
        ) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    unsafe {
        let mapping = CreateFileMappingW(file.as_raw_handle(), std::ptr::null(), PAGE_READONLY, 0, 0, std::ptr::null());
        if mapping.is_null() {
            return Err(io::Error::last_os_error());
        }
        let pointer = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
        // 视图会让映射对象一直有效，映射对象的句柄可以关掉
        CloseHandle(mapping);
        if pointer.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(std::slice::from_raw_parts(pointer.cast(), len))
    }
}
