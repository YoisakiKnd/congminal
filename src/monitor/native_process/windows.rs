//! Query-only process handles, fixed buffers, no global process metadata table.
use super::{Proc, Record, label};
use std::ffi::c_void;
pub(crate) struct Clock;
impl Clock {
    pub fn new() -> Option<Self> {
        Some(Self)
    }
}
#[repr(C)]
#[derive(Default)]
struct FileTime {
    low: u32,
    high: u32,
}
impl FileTime {
    fn value(&self) -> u64 {
        u64::from(self.low) | (u64::from(self.high) << 32)
    }
}
#[repr(C)]
struct Memory {
    size: u32,
    faults: u32,
    peak_working: usize,
    working: usize,
    peak_paged: usize,
    paged: usize,
    peak_nonpaged: usize,
    nonpaged: usize,
    pagefile: usize,
    peak_pagefile: usize,
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetProcessTimes(
        handle: *mut c_void,
        created: *mut FileTime,
        exited: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    fn K32GetProcessMemoryInfo(handle: *mut c_void, memory: *mut Memory, size: u32) -> i32;
    fn QueryFullProcessImageNameW(handle: *mut c_void, flags: u32, path: *mut u16, size: *mut u32) -> i32;
    fn GetExitCodeProcess(handle: *mut c_void, code: *mut u32) -> i32;
}
struct Handle(*mut c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
pub(crate) fn read(pid: u32, _clock: &Clock) -> Option<Record> {
    if pid == 0 {
        return None;
    }
    let raw = unsafe { OpenProcess(0x1000, 0, pid) };
    if raw.is_null() {
        return None;
    }
    let handle = Handle(raw);
    let mut created = FileTime::default();
    let mut exited = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    if unsafe { GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return None;
    }
    let mut memory: Memory = unsafe { std::mem::zeroed() };
    memory.size = std::mem::size_of::<Memory>() as u32;
    let memory_size = memory.size;
    if unsafe { K32GetProcessMemoryInfo(handle.0, &mut memory, memory_size) } == 0 {
        return None;
    }
    let mut path = [0u16; 4096];
    let mut length = path.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle.0, 0, path.as_mut_ptr(), &mut length) } == 0
        || length == 0
        || length as usize >= path.len()
    {
        return None;
    }
    let path = &path[..length as usize];
    let start = path.iter().rposition(|c| *c == b'\\' as u16 || *c == b'/' as u16).map_or(0, |i| i + 1);
    let name =
        label(char::decode_utf16(path[start..].iter().copied()).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)));
    let mut code = 0;
    if unsafe { GetExitCodeProcess(handle.0, &mut code) } == 0 || code != 259 {
        return None;
    }
    Some(Record {
        start: created.value(),
        milliseconds: kernel.value().checked_add(user.value())? / 10_000,
        row: Proc { pid, name, cpu: 0.0, memory: memory.working as u64 },
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn own_process_uses_a_bounded_query_handle() {
        let row = read(std::process::id(), &Clock).unwrap();
        assert!(row.start > 0 && row.row.memory > 0);
        assert!(row.row.name.len() <= crate::monitor::PROCESS_NAME_BYTES);
        assert!(read(0, &Clock).is_none());
    }
}

#[cfg(test)]
mod handle_acceptance {
    use super::*;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetProcessHandleCount(handle: *mut c_void, count: *mut u32) -> i32;
    }
    #[test]
    #[ignore = "process-global handle count; run alone on Windows"]
    fn repeated_reads_release_the_query_handle() {
        let current = unsafe { GetCurrentProcess() };
        let mut before = 0;
        assert_ne!(unsafe { GetProcessHandleCount(current, &mut before) }, 0);
        for _ in 0..1000 {
            assert!(read(std::process::id(), &Clock).is_some());
        }
        let mut after = 0;
        assert_ne!(unsafe { GetProcessHandleCount(current, &mut after) }, 0);
        assert_eq!(before, after);
    }
}
