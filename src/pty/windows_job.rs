//! 会话的 Job 对象：shell 和它启动的进程都在里面，用来找前台程序。
//!
//! 只用于查询：不限制资源，不设 KILL_ON_JOB_CLOSE，关闭标签页时进程的去留和以前一样；
//! 允许子进程用 CREATE_BREAKAWAY_FROM_JOB 脱离，否则这类程序会启动失败。

use super::super::foreground::{Foreground, Proc, pick};
use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};

/// 一次最多看这么多进程；超出时只用列表里的部分，探测照常进行
const LIST_LIMIT: usize = 64;
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
const JOB_OBJECT_BASIC_PROCESS_ID_LIST: i32 = 3;
const JOB_OBJECT_LIMIT_BREAKAWAY_OK: u32 = 0x0000_0800;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
pub(super) const PROC_THREAD_ATTRIBUTE_JOB_LIST: usize = 0x0002_000D;

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
struct BasicLimit {
    per_process_user_time: i64,
    per_job_user_time: i64,
    flags: u32,
    minimum_working_set: usize,
    maximum_working_set: usize,
    active_processes: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[repr(C)]
struct ExtendedLimit {
    basic: BasicLimit,
    io: [u64; 6],
    process_memory: usize,
    job_memory: usize,
    peak_process_memory: usize,
    peak_job_memory: usize,
}

#[repr(C)]
struct ProcessIdList {
    assigned: u32,
    listed: u32,
    ids: [usize; LIST_LIMIT],
}

#[repr(C)]
struct BasicInformation {
    exit_status: i32,
    peb: *mut c_void,
    affinity: usize,
    base_priority: i32,
    pid: usize,
    parent: usize,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> *mut c_void;
    fn SetInformationJobObject(job: *mut c_void, class: i32, info: *const c_void, size: u32) -> i32;
    fn QueryInformationJobObject(job: *mut c_void, class: i32, info: *mut c_void, size: u32, used: *mut u32) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn GetProcessTimes(
        handle: *mut c_void,
        created: *mut FileTime,
        exited: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    fn QueryFullProcessImageNameW(handle: *mut c_void, flags: u32, path: *mut u16, size: *mut u32) -> i32;
    fn GetSystemTimeAsFileTime(time: *mut FileTime);
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationProcess(handle: *mut c_void, class: u32, info: *mut c_void, size: u32, used: *mut u32) -> i32;
}

pub(super) struct Job(OwnedHandle);

impl Job {
    /// 失败时返回 None：会话照常启动，只是标签不显示前台程序
    pub fn create() -> Option<Self> {
        let raw = unsafe { CreateJobObjectW(null(), null()) };
        if raw.is_null() {
            return None;
        }
        let job = Job(unsafe { OwnedHandle::from_raw_handle(raw) });
        let mut limit: ExtendedLimit = unsafe { std::mem::zeroed() };
        limit.basic.flags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        let ok = unsafe {
            SetInformationJobObject(
                job.raw(),
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                (&raw const limit).cast(),
                size_of::<ExtendedLimit>() as u32,
            )
        };
        (ok != 0).then_some(job)
    }

    pub fn raw(&self) -> *mut c_void {
        self.0.as_raw_handle()
    }

    pub fn foreground(&self, root: u32) -> Option<Foreground> {
        let mut list: ProcessIdList = unsafe { std::mem::zeroed() };
        // 进程比列表长时返回 ERROR_MORE_DATA，但前 LIST_LIMIT 个 PID 已经填好
        unsafe {
            QueryInformationJobObject(
                self.raw(),
                JOB_OBJECT_BASIC_PROCESS_ID_LIST,
                (&raw mut list).cast(),
                size_of::<ProcessIdList>() as u32,
                null_mut(),
            );
        }
        let listed = (list.listed as usize).min(LIST_LIMIT);
        if listed == 0 {
            return None;
        }
        let procs: Vec<Proc> = list.ids[..listed].iter().filter_map(|&pid| query(u32::try_from(pid).ok()?)).collect();
        let mut now = FileTime::default();
        unsafe { GetSystemTimeAsFileTime(&mut now) };
        pick(root, &procs, now.value())
    }
}

fn query(pid: u32) -> Option<Proc> {
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return None;
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    let (mut created, mut exited, mut kernel, mut user) =
        (FileTime::default(), FileTime::default(), FileTime::default(), FileTime::default());
    if unsafe { GetProcessTimes(handle.as_raw_handle(), &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return None;
    }
    let mut info: BasicInformation = unsafe { std::mem::zeroed() };
    let status = unsafe {
        NtQueryInformationProcess(
            handle.as_raw_handle(),
            0,
            (&raw mut info).cast(),
            size_of::<BasicInformation>() as u32,
            null_mut(),
        )
    };
    if status < 0 {
        return None;
    }
    let mut path = [0u16; 1024];
    let mut length = path.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle.as_raw_handle(), 0, path.as_mut_ptr(), &mut length) } == 0 {
        return None;
    }
    let path = path.get(..length as usize)?;
    let start = path.iter().rposition(|&c| c == u16::from(b'\\') || c == u16::from(b'/')).map_or(0, |i| i + 1);
    let mut name = String::from_utf16_lossy(&path[start..]);
    if name.len() > 4 && name.is_char_boundary(name.len() - 4) && name[name.len() - 4..].eq_ignore_ascii_case(".exe") {
        name.truncate(name.len() - 4);
    }
    Some(Proc { pid, parent: u32::try_from(info.parent).ok()?, created: created.value(), name })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_lists_its_own_processes() {
        let job = Job::create().unwrap();
        assert_eq!(job.foreground(std::process::id()), None);
        let me = query(std::process::id()).unwrap();
        assert!(me.created > 0 && !me.name.is_empty() && !me.name.ends_with(".exe"));
    }
}
