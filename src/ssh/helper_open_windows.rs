//! 仅按显式 UI 请求运行的系统文件打开器；不把文件名插入命令解释器。
use std::{
    ffi::c_void,
    io,
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr::{null, null_mut},
};
#[repr(C)]
struct Execute {
    size: u32,
    mask: u32,
    window: *mut c_void,
    verb: *const u16,
    file: *const u16,
    parameters: *const u16,
    directory: *const u16,
    show: i32,
    instance: *mut c_void,
    id_list: *mut c_void,
    class: *const u16,
    class_key: *mut c_void,
    hotkey: u32,
    icon: *mut c_void,
    process: *mut c_void,
}
#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<Execute>() == 112);
#[link(name = "shell32")]
unsafe extern "system" {
    fn ShellExecuteExW(info: *mut Execute) -> i32;
}
#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> i32;
    fn CoUninitialize();
}
fn path_units(path: &Path) -> io::Result<Vec<u16>> {
    if !path.is_absolute() || path.as_os_str().len() > 4096 || path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut units = Vec::new();
    units.try_reserve_exact(path.as_os_str().len() + 1).map_err(|_| io::ErrorKind::OutOfMemory)?;
    units.extend(path.as_os_str().encode_wide());
    units.push(0);
    Ok(units)
}
pub fn open(path: &Path) -> io::Result<i32> {
    let path = path_units(path)?;
    let result = unsafe { CoInitializeEx(null_mut(), 6) }; // STA | DISABLE_OLE1DDE
    if result < 0 {
        return Err(io::Error::other("system opener initialization failed"));
    }
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }
    let _apartment = Apartment;
    let mut info = Execute {
        size: std::mem::size_of::<Execute>() as u32,
        mask: 0x100 | 0x400,
        window: null_mut(),
        verb: null(),
        file: path.as_ptr(),
        parameters: null(),
        directory: null(),
        show: 1,
        instance: null_mut(),
        id_list: null_mut(),
        class: null(),
        class_key: null_mut(),
        hotkey: 0,
        icon: null_mut(),
        process: null_mut(),
    };
    // NOASYNC 在 helper 退出前完成 shell 的交接；不请求/等待用户编辑器的进程句柄。
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(0)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_file_arguments_are_passed_literally_and_protocol_or_relative_paths_are_rejected() {
        let path = Path::new(r"C:\folder & data\name;$(test).txt");
        let units = path_units(path).unwrap();
        assert_eq!(std::ffi::OsString::from_wide(&units[..units.len() - 1]), path.as_os_str());
        assert!(path_units(Path::new("https://example.invalid")).is_err());
        assert!(path_units(Path::new("relative.txt")).is_err());
    }
    use std::os::windows::ffi::OsStringExt;
}
