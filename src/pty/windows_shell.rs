//! Fixed local candidates; no PATH scan, subprocess probe, or background worker.
use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf, Prefix},
};
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetEnvironmentVariableW(name: *const u16, value: *mut u16, size: u32) -> u32;
    fn GetSystemDirectoryW(value: *mut u16, size: u32) -> u32;
}
fn environment(name: &str) -> Option<PathBuf> {
    let key = name.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let mut value = [0u16; 4097];
    let n = unsafe { GetEnvironmentVariableW(key.as_ptr(), value.as_mut_ptr(), value.len() as u32) } as usize;
    if n == 0 || n >= value.len() {
        return None;
    }
    let path = PathBuf::from(OsString::from_wide(&value[..n]));
    local(&path).then_some(path)
}
fn local(path: &Path) -> bool {
    path.is_absolute()
        && matches!(path.components().next(),Some(Component::Prefix(p)) if matches!(p.kind(),Prefix::Disk(_)|Prefix::VerbatimDisk(_)))
        && path.as_os_str().encode_wide().count() <= 4096
}
pub(super) fn home_directory() -> Option<PathBuf> {
    environment("USERPROFILE").filter(|path| path.as_os_str().len() <= 4096)
}
pub(super) fn default_program() -> String {
    let program_files = environment("ProgramFiles").map(|p| p.join("PowerShell/7/pwsh.exe"));
    let scoop = environment("USERPROFILE").map(|p| p.join("scoop/apps/pwsh/current/pwsh.exe"));
    let mut system = [0u16; 4097];
    let n = unsafe { GetSystemDirectoryW(system.as_mut_ptr(), system.len() as u32) } as usize;
    let powershell = (n > 0 && n < system.len())
        .then(|| PathBuf::from(OsString::from_wide(&system[..n])).join("WindowsPowerShell/v1.0/powershell.exe"));
    for path in [program_files, scoop, powershell].into_iter().flatten() {
        if local(&path)
            && path.is_file()
            && let Some(value) = path.to_str()
        {
            return value.to_owned();
        }
    }
    environment("COMSPEC").and_then(|p| p.to_str().map(str::to_owned)).unwrap_or_else(|| "cmd.exe".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_stay_local_and_explicit_paths_remain_borrowed() {
        let program = default_program();
        assert!(!program.is_empty());
        assert!(!program.starts_with("\\\\"));
        assert!(local(Path::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")));
        assert!(!local(Path::new(r"\\server\share\pwsh.exe")));
        assert!(!local(Path::new("pwsh.exe")));
    }
}
