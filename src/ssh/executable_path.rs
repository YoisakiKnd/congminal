//! helper 发现与 askpass 自路径；Windows 在转换/分配前拒绝超长模块名。
use std::{io, path::PathBuf};
pub fn current() -> io::Result<PathBuf> {
    #[cfg(windows)]
    let path = {
        use std::{ffi::c_void, os::windows::ffi::OsStringExt};
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetModuleFileNameW(module: *mut c_void, name: *mut u16, capacity: u32) -> u32;
        }
        // std::env::current_exe 会为 Windows 的 32K 模块名增长缓冲；本程序只接受 4096 字节路径。
        let mut units = [0u16; 4097];
        let count = unsafe { GetModuleFileNameW(std::ptr::null_mut(), units.as_mut_ptr(), units.len() as u32) };
        if count == 0 {
            return Err(io::Error::last_os_error());
        }
        if count as usize >= units.len() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        PathBuf::from(std::ffi::OsString::from_wide(&units[..count as usize]))
    };
    // Unix 的内核可执行路径在已有路径工作额度内；保留标准库的真实路径/符号链接解析。
    #[cfg(unix)]
    let path = std::env::current_exe()?;
    if !path.is_absolute() || path.as_os_str().len() > 4096 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(path)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_module_path_matches_the_current_executable_within_the_supported_bound() {
        let actual = current().unwrap();
        assert!(actual.is_absolute() && actual.as_os_str().len() <= 4096);
        assert_eq!(actual, std::env::current_exe().unwrap());
    }
}
