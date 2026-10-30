//! A single pinned local DLL, never searched on PATH/CWD and never downloaded at runtime.
//! Module stays mapped until process exit: ConPTY can finish native callbacks after Close returns.
use super::{Coord, Handle};
use crate::ssh::executable_path;
use std::{ffi::c_void, io, os::windows::ffi::OsStrExt, path::PathBuf, sync::OnceLock};
type Create = unsafe extern "system" fn(Coord, Handle, Handle, u32, *mut Handle) -> i32;
type Resize = unsafe extern "system" fn(Handle, Coord) -> i32;
type Close = unsafe extern "system" fn(Handle);
#[derive(Clone, Copy)]
pub(super) struct Api {
    pub create: Create,
    pub resize: Resize,
    pub close: Close,
    pub modern: bool,
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(path: *const u16, file: isize, flags: u32) -> isize;
    fn GetProcAddress(module: isize, name: *const u8) -> *mut c_void;
    fn FreeLibrary(module: isize) -> i32;
}
static API: OnceLock<Result<Api, u32>> = OnceLock::new();
impl Api {
    pub fn load() -> io::Result<Self> {
        #[cfg(test)]
        if FORCE_SYSTEM.with(|v| v.get()) {
            return Ok(Self::system());
        }
        match API.get_or_init(load) {
            Ok(api) => Ok(*api),
            Err(code) => Err(io::Error::from_raw_os_error(*code as i32)),
        }
    }
    fn system() -> Self {
        Self {
            create: super::CreatePseudoConsole,
            resize: super::ResizePseudoConsole,
            close: super::ClosePseudoConsole,
            modern: false,
        }
    }
}
fn load() -> Result<Api, u32> {
    let path = executable_path::current().map_err(code)?;
    let path = path.parent().ok_or(87u32)?.join("conpty.dll");
    if !path.try_exists().map_err(code)? {
        return Ok(Api::system());
    }
    let host = path.parent().unwrap().join("x64/OpenConsole.exe");
    if !host.is_file() {
        return Err(2);
    }
    let name = wide(path)?;
    let module = unsafe { LoadLibraryExW(name.as_ptr(), 0, 0x100 | 0x800) };
    if module == 0 {
        return Err(code(io::Error::last_os_error()));
    }
    let create = unsafe { GetProcAddress(module, c"ConptyCreatePseudoConsole".as_ptr().cast()) };
    let resize = unsafe { GetProcAddress(module, c"ConptyResizePseudoConsole".as_ptr().cast()) };
    let close = unsafe { GetProcAddress(module, c"ConptyClosePseudoConsole".as_ptr().cast()) };
    if create.is_null() || resize.is_null() || close.is_null() {
        unsafe { FreeLibrary(module) };
        return Err(127);
    }
    // Exact signatures from the Microsoft.Windows.Console.ConPTY inc/conpty.h ABI.
    Ok(Api {
        create: unsafe { std::mem::transmute::<*mut c_void, Create>(create) },
        resize: unsafe { std::mem::transmute::<*mut c_void, Resize>(resize) },
        close: unsafe { std::mem::transmute::<*mut c_void, Close>(close) },
        modern: true,
    })
}
fn code(error: io::Error) -> u32 {
    error.raw_os_error().unwrap_or(87) as u32
}
fn wide(path: PathBuf) -> Result<Vec<u16>, u32> {
    if path.as_os_str().encode_wide().count() > 4096 {
        return Err(206);
    }
    Ok(path.as_os_str().encode_wide().chain(Some(0)).collect())
}
#[cfg(test)]
thread_local! {static FORCE_SYSTEM:std::cell::Cell<bool>=const {std::cell::Cell::new(false)};}
#[cfg(test)]
pub(super) fn force_system(value: bool) {
    FORCE_SYSTEM.with(|v| v.set(value));
}
