//! Windows 10+：创建时绑定 Job，标准句柄白名单；无挂起后再分配 Job 的窗口。
use super::{Error, Spec, process_error, windows_wire};
use crate::tasks::Cancellation;
use std::{
    ffi::{OsStr, c_void},
    fs::File,
    io::{self, Read},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::ExitStatusExt,
    },
    path::Path,
    process::ExitStatus,
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
#[path = "capture_windows_input.rs"]
mod input;
type Handle = *mut c_void;
#[repr(C)]
struct Security {
    length: u32,
    descriptor: *mut c_void,
    inherit: i32,
}
#[repr(C)]
struct StartupInfoW {
    cb: u32,
    reserved: *mut u16,
    desktop: *mut u16,
    title: *mut u16,
    x: u32,
    y: u32,
    x_size: u32,
    y_size: u32,
    x_count_chars: u32,
    y_count_chars: u32,
    fill_attribute: u32,
    flags: u32,
    show_window: u16,
    reserved2_len: u16,
    reserved2: *mut u8,
    std_input: Handle,
    std_output: Handle,
    std_error: Handle,
}
#[repr(C)]
struct StartupInfoExW {
    startup_info: StartupInfoW,
    attribute_list: *mut c_void,
}
#[repr(C)]
struct ProcessInformation {
    process: Handle,
    thread: Handle,
    process_id: u32,
    thread_id: u32,
}
#[repr(C)]
struct BasicLimit {
    process_time: i64,
    job_time: i64,
    flags: u32,
    minimum_working: usize,
    maximum_working: usize,
    active_limit: u32,
    affinity: usize,
    priority: u32,
    scheduling: u32,
}
#[repr(C)]
struct ExtendedLimit {
    basic: BasicLimit,
    io: [u64; 6],
    process_memory: usize,
    job_memory: usize,
    peak_process: usize,
    peak_job: usize,
}
#[repr(C)]
struct Accounting {
    times: [i64; 4],
    faults: u32,
    total: u32,
    active: u32,
    terminated: u32,
}
#[repr(align(16))]
struct Block([u8; 16]);
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<Security>() == 24);
    assert!(std::mem::size_of::<StartupInfoW>() == 104);
    assert!(std::mem::size_of::<StartupInfoExW>() == 112);
    assert!(std::mem::size_of::<ProcessInformation>() == 24);
    assert!(std::mem::size_of::<ExtendedLimit>() == 144);
    assert!(std::mem::size_of::<Accounting>() == 48);
    assert!(std::mem::offset_of!(Accounting, active) == 40);
};
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CompareStringOrdinal(a: *const u16, a_len: i32, b: *const u16, b_len: i32, ignore_case: i32) -> i32;
    fn GetEnvironmentStringsW() -> *mut u16;
    fn FreeEnvironmentStringsW(block: *mut u16) -> i32;
    fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(job: Handle, class: i32, information: *const c_void, size: u32) -> i32;
    fn QueryInformationJobObject(
        job: Handle,
        class: i32,
        information: *mut c_void,
        size: u32,
        returned: *mut u32,
    ) -> i32;
    fn TerminateJobObject(job: Handle, code: u32) -> i32;
    fn CreatePipe(read: *mut Handle, write: *mut Handle, attributes: *const c_void, size: u32) -> i32;
    fn SetHandleInformation(handle: Handle, mask: u32, flags: u32) -> i32;
    fn CreateFileW(
        name: *const u16,
        access: u32,
        sharing: u32,
        attributes: *const c_void,
        creation: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn InitializeProcThreadAttributeList(list: *mut c_void, count: u32, flags: u32, size: *mut usize) -> i32;
    fn UpdateProcThreadAttribute(
        list: *mut c_void,
        flags: u32,
        attribute: usize,
        value: *const c_void,
        size: usize,
        previous: *mut c_void,
        returned: *mut usize,
    ) -> i32;
    fn DeleteProcThreadAttributeList(list: *mut c_void);
    fn CreateProcessW(
        application: *const u16,
        line: *mut u16,
        process_attributes: *const c_void,
        thread_attributes: *const c_void,
        inherit: i32,
        flags: u32,
        environment: *const c_void,
        directory: *const u16,
        startup: *const StartupInfoW,
        information: *mut ProcessInformation,
    ) -> i32;
    fn PeekNamedPipe(
        pipe: Handle,
        buffer: *mut c_void,
        size: u32,
        read: *mut u32,
        available: *mut u32,
        remaining: *mut u32,
    ) -> i32;
    fn WaitForSingleObject(handle: Handle, millis: u32) -> u32;
    fn GetExitCodeProcess(handle: Handle, code: *mut u32) -> i32;
    fn SearchPathW(
        path: *const u16,
        file: *const u16,
        extension: *const u16,
        size: u32,
        output: *mut u16,
        part: *mut *mut u16,
    ) -> u32;
}
fn owned(raw: Handle) -> Result<OwnedHandle, Error> {
    if raw.is_null() || raw as isize == -1 {
        Err(process_error(io::Error::last_os_error()))
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
    }
}
fn failure() -> Error {
    process_error(io::Error::last_os_error())
}
fn cleanup_error() -> Error {
    Error::Cleanup(io::Error::last_os_error().raw_os_error().unwrap_or(0))
}
fn wide(value: &OsStr) -> Result<Vec<u16>, Error> {
    let count = value.encode_wide().count();
    if count > 32766 {
        return Err(Error::Limit);
    }
    let mut output = Vec::new();
    output.try_reserve_exact(count + 1).map_err(|_| Error::Budget)?;
    for unit in value.encode_wide() {
        if unit == 0 {
            return Err(Error::Invalid);
        }
        output.push(unit);
    }
    output.push(0);
    Ok(output)
}
pub(super) fn executable(program: &OsStr) -> Result<Vec<u16>, Error> {
    let mut path = Path::new(program).to_path_buf();
    match path.extension() {
        Some(extension) if !extension.to_str().is_some_and(|s| s.eq_ignore_ascii_case("exe")) => {
            return Err(Error::Unsupported);
        }
        None => {
            path.set_extension("exe");
        }
        _ => {}
    }
    if program.encode_wide().any(|u| u == 34 || u == 0) {
        return Err(Error::Invalid);
    }
    if path.is_absolute() || path.components().count() > 1 {
        let absolute = std::path::absolute(&path).map_err(process_error)?;
        if absolute.as_os_str().len() > 4096 {
            return Err(Error::Limit);
        }
        return wide(absolute.as_os_str());
    }
    // 使用明确 PATH，不让 CreateProcess 在应用目录/当前目录隐式抢先搜索。
    let search = std::env::var_os("PATH").ok_or(Error::Missing)?;
    if search.len() > 32768 {
        return Err(Error::Limit);
    }
    let search = wide(&search)?;
    let file = wide(path.as_os_str())?;
    let mut output = Vec::new();
    output.try_reserve_exact(4096).map_err(|_| Error::Budget)?;
    output.resize(4096, 0);
    let length = unsafe {
        SearchPathW(search.as_ptr(), file.as_ptr(), null(), output.len() as u32, output.as_mut_ptr(), null_mut())
    } as usize;
    if length == 0 {
        return Err(failure());
    }
    if length >= output.len() {
        return Err(Error::Limit);
    }
    output.truncate(length + 1);
    Ok(output)
}
struct Attributes {
    storage: Vec<Block>,
}
impl Attributes {
    fn new() -> Result<Self, Error> {
        let mut size = 0;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut size);
        }
        if size == 0 || size > 4096 {
            return Err(Error::Limit);
        }
        let mut storage = Vec::new();
        storage.try_reserve_exact(size.div_ceil(16)).map_err(|_| Error::Budget)?;
        storage.resize_with(size.div_ceil(16), || Block([0; 16]));
        if unsafe { InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 2, 0, &mut size) } == 0 {
            return Err(failure());
        }
        Ok(Self { storage })
    }
    fn pointer(&mut self) -> *mut c_void {
        self.storage[0].0.as_mut_ptr().cast()
    }
    // value 必须一直存活至本属性表销毁；仅 spawn 中固定的 handles/jobs 数组调用。
    unsafe fn add(&mut self, key: usize, values: &[Handle]) -> Result<(), Error> {
        if unsafe {
            UpdateProcThreadAttribute(
                self.pointer(),
                0,
                key,
                values.as_ptr().cast(),
                std::mem::size_of_val(values),
                null_mut(),
                null_mut(),
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            return Err(if matches!(error.raw_os_error(), Some(50 | 87)) {
                Error::Unsupported
            } else {
                process_error(error)
            });
        }
        Ok(())
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.pointer());
        }
    }
}
// The only override mode is a fixed Git environment. Keep inherited non-Git entries,
// including Windows drive variables, within one bounded UTF-16 work buffer.
fn git_environment() -> Result<Vec<u16>, Error> {
    const LIMIT: usize = 128 * 1024;
    struct Native(*mut u16);
    impl Drop for Native {
        fn drop(&mut self) {
            unsafe {
                FreeEnvironmentStringsW(self.0);
            }
        }
    }
    let block = Native(unsafe { GetEnvironmentStringsW() });
    if block.0.is_null() {
        return Err(failure());
    }
    let names = [
        "GIT_GLOB_PATHSPECS",
        "GIT_NOGLOB_PATHSPECS",
        "GIT_ICASE_PATHSPECS",
        "GIT_LITERAL_PATHSPECS",
        "GIT_OPTIONAL_LOCKS",
        "GIT_TERMINAL_PROMPT",
        "GIT_PAGER",
        "LC_ALL",
    ];
    let mut out = Vec::new();
    out.try_reserve_exact(LIMIT).map_err(|_| Error::Budget)?;
    let mut at = 0;
    loop {
        if at >= LIMIT {
            return Err(Error::Limit);
        }
        if unsafe { *block.0.add(at) } == 0 {
            break;
        }
        let start = at;
        while at < LIMIT && unsafe { *block.0.add(at) } != 0 {
            at += 1;
        }
        if at >= LIMIT {
            return Err(Error::Limit);
        }
        let entry = unsafe { std::slice::from_raw_parts(block.0.add(start), at - start) };
        let key = &entry[..entry.iter().position(|&c| c == 61).unwrap_or(entry.len())];
        let overridden = names.iter().any(|name| {
            key.len() == name.len()
                && key.iter().zip(name.bytes()).all(|(&a, b)| a < 128 && (a as u8).eq_ignore_ascii_case(&b))
        });
        if !overridden {
            if out.len() + entry.len() + 512 >= LIMIT {
                return Err(Error::Limit);
            }
            out.extend_from_slice(entry);
            out.push(0);
        }
        at += 1;
    }
    for entry in
        ["GIT_LITERAL_PATHSPECS=1", "GIT_OPTIONAL_LOCKS=0", "GIT_TERMINAL_PROMPT=0", "GIT_PAGER=cat", "LC_ALL=C"]
    {
        out.extend(entry.encode_utf16());
        out.push(0);
    }
    out.push(0);
    // CreateProcess expects a case-insensitively sorted environment block.
    let mut ranges = [(0usize, 0usize); 4096];
    let mut count = 0;
    let mut at = 0;
    while at + 1 < out.len() {
        if count == ranges.len() {
            return Err(Error::Limit);
        }
        let end = at + out[at..].iter().position(|&c| c == 0).ok_or(Error::Invalid)?;
        ranges[count] = (at, end);
        count += 1;
        at = end + 1;
    }
    ranges[..count].sort_unstable_by(|&(a, end_a), &(b, end_b)| {
        let key = |start: usize, end: usize| {
            start + out[start + 1..end].iter().position(|&c| c == 61).map_or(end - start, |i| i + 1)
        };
        let end_a = key(a, end_a);
        let end_b = key(b, end_b);
        let order = unsafe {
            CompareStringOrdinal(out[a..].as_ptr(), (end_a - a) as i32, out[b..].as_ptr(), (end_b - b) as i32, 1)
        };
        match order {
            1 => std::cmp::Ordering::Less,
            3 => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        }
    });
    let mut sorted = Vec::new();
    sorted.try_reserve_exact(out.len()).map_err(|_| Error::Budget)?;
    for &(start, end) in &ranges[..count] {
        sorted.extend_from_slice(&out[start..=end]);
    }
    sorted.push(0);
    Ok(sorted)
}

pub struct Process {
    process: OwnedHandle,
    output: File,
    error: Option<File>,
    input: Option<input::Input>,
    job: OwnedHandle,
    stopped: bool,
}
impl Process {
    pub fn spawn(spec: &mut Spec, duplex: Option<&std::sync::Arc<crate::resources::Budget>>) -> Result<Self, Error> {
        let application = executable(spec.get_program())?;
        let mut line = Vec::new();
        line.try_reserve_exact(windows_wire::LIMIT).map_err(|_| Error::Budget)?;
        windows_wire::argument(&mut line, &application[..application.len() - 1])?;
        for arg in spec.get_args() {
            let arg = wide(arg)?;
            windows_wire::argument(&mut line, &arg[..arg.len() - 1])?;
        }
        line.push(0);
        let job = owned(unsafe { CreateJobObjectW(null(), null()) })?;
        let mut limits: ExtendedLimit = unsafe { std::mem::zeroed() };
        limits.basic.flags = 0x2000; // KILL_ON_JOB_CLOSE，无 breakaway。
        if unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                9,
                (&limits as *const ExtendedLimit).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(failure());
        }
        // 只在这段短临界区存在临时可继承句柄；不持锁等待子进程或读输出。
        let _guard = crate::process_launch::guard();
        let security = Security { length: std::mem::size_of::<Security>() as u32, descriptor: null_mut(), inherit: 1 };
        let (mut read, mut write) = (null_mut(), null_mut());
        if unsafe { CreatePipe(&mut read, &mut write, (&security as *const Security).cast(), 0) } == 0 {
            return Err(failure());
        }
        let read = owned(read)?;
        let write = owned(write)?;
        if unsafe { SetHandleInformation(read.as_raw_handle(), 1, 0) } == 0 {
            return Err(failure());
        }
        let (error_read, error_write) = if spec.split {
            let (mut read, mut write) = (null_mut(), null_mut());
            if unsafe { CreatePipe(&mut read, &mut write, (&security as *const Security).cast(), 0) } == 0 {
                return Err(failure());
            }
            let read = owned(read)?;
            let write = owned(write)?;
            if unsafe { SetHandleInformation(read.as_raw_handle(), 1, 0) } == 0 {
                return Err(failure());
            }
            (Some(read), Some(write))
        } else {
            (None, None)
        };
        let environment = if spec.git_environment { Some(git_environment()?) } else { None };
        let nul = [78u16, 85, 76, 0];
        let nil = owned(unsafe {
            CreateFileW(nul.as_ptr(), 0xc000_0000, 3, (&security as *const Security).cast(), 3, 0x80, null_mut())
        })?;
        let (input, client) = match duplex {
            Some(pool) => {
                let (input, client) = input::Input::new(pool, &security)?;
                (Some(input), Some(client))
            }
            None => (None, None),
        };
        let mut handles = [write.as_raw_handle(), nil.as_raw_handle(), null_mut(), null_mut()];
        let mut handle_count = 2;
        if let Some(client) = &client {
            handles[handle_count] = client.as_raw_handle();
            handle_count += 1;
        }
        if let Some(error) = &error_write {
            handles[handle_count] = error.as_raw_handle();
            handle_count += 1;
        }
        let jobs = [job.as_raw_handle()];
        let mut attributes = Attributes::new()?;
        unsafe {
            attributes.add(0x0002_0002, &handles[..handle_count])?;
            attributes.add(0x0002_000d, &jobs)?;
        }
        let mut startup: StartupInfoExW = unsafe { std::mem::zeroed() };
        startup.startup_info.cb = std::mem::size_of::<StartupInfoExW>() as u32;
        startup.startup_info.flags = 0x100;
        startup.startup_info.std_input = client.as_ref().map_or(nil.as_raw_handle(), AsRawHandle::as_raw_handle);
        startup.startup_info.std_error = error_write.as_ref().map_or(nil.as_raw_handle(), AsRawHandle::as_raw_handle);
        startup.startup_info.std_output = write.as_raw_handle();
        startup.attribute_list = attributes.pointer();
        let mut information: ProcessInformation = unsafe { std::mem::zeroed() };
        if unsafe {
            CreateProcessW(
                application.as_ptr(),
                line.as_mut_ptr(),
                null(),
                null(),
                1,
                0x0808_0400,
                environment.as_ref().map_or(null(), |block| block.as_ptr().cast()),
                null(),
                &startup.startup_info,
                &mut information,
            )
        } == 0
        {
            return Err(failure());
        }
        // Job 在第一个用户线程运行前已生效；此后任何错误由 job 句柄关闭兜底。
        let process = owned(information.process)?;
        let thread = owned(information.thread)?;
        drop(thread);
        drop(attributes);
        drop(write);
        drop(error_write);
        drop(nil);
        drop(client);
        Ok(Self { process, output: File::from(read), error: error_read.map(File::from), input, job, stopped: false })
    }
    fn read_pipe(pipe: &mut File, bytes: &mut [u8]) -> io::Result<usize> {
        let mut available = 0;
        if unsafe { PeekNamedPipe(pipe.as_raw_handle(), null_mut(), 0, null_mut(), &mut available, null_mut()) } == 0 {
            let error = io::Error::last_os_error();
            return if matches!(error.raw_os_error(), Some(109 | 233)) { Ok(0) } else { Err(error) };
        }
        if available == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let count = bytes.len().min(available as usize);
        // 此读端没有副本或其他读者，Peek 返回的字节不会被其他任务取走。
        pipe.read(&mut bytes[..count])
    }
    pub fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        Self::read_pipe(&mut self.output, bytes)
    }
    pub fn read_error(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match &mut self.error {
            Some(pipe) => Self::read_pipe(pipe, bytes),
            None => Ok(0),
        }
    }
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.input.as_mut().ok_or(io::ErrorKind::BrokenPipe)?.write(bytes)
    }
    pub fn wait_write(&self, _cancel: &Cancellation) -> Result<(), Error> {
        self.input.as_ref().ok_or(Error::Process)?.wait()
    }
    pub fn exited(&mut self) -> Result<bool, Error> {
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
            0 => Ok(true),
            258 => Ok(false),
            _ => Err(Error::Process),
        }
    }
    fn code(&self) -> Result<u32, Error> {
        let mut code = 0;
        if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) } == 0 {
            return Err(cleanup_error());
        }
        Ok(code)
    }
    fn active(&self) -> Result<u32, Error> {
        let mut information: Accounting = unsafe { std::mem::zeroed() };
        if unsafe {
            QueryInformationJobObject(
                self.job.as_raw_handle(),
                1,
                (&mut information as *mut Accounting).cast(),
                std::mem::size_of::<Accounting>() as u32,
                null_mut(),
            )
        } == 0
        {
            return Err(cleanup_error());
        }
        Ok(information.active)
    }
    pub fn stop(&mut self) -> Result<ExitStatus, Error> {
        assert!(!self.stopped);
        let previous = if self.exited()? { Some(self.code()?) } else { None };
        if unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
            let error = cleanup_error();
            if self.active()? != 0 {
                return Err(error);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        if unsafe { WaitForSingleObject(self.process.as_raw_handle(), 2000) } != 0 {
            return Err(Error::Cleanup(1460));
        }
        while self.active()? != 0 {
            if Instant::now() >= deadline {
                return Err(Error::Cleanup(1460));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let code = match previous {
            Some(code) => code,
            None => self.code()?,
        };
        self.stopped = true;
        Ok(ExitStatus::from_raw(code))
    }
    pub fn cleanup(&mut self) -> Result<(), Error> {
        let result = if self.stopped { Ok(()) } else { self.stop().map(|_| ()) };
        let input = self.input.as_mut().map_or(Ok(()), input::Input::close);
        result.and(input)
    }
    pub fn wait(&self, cancel: &Cancellation, eof: bool, closing: bool) -> Result<(), Error> {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let millis = if eof && closing { 1 } else { 5 };
        if self.stopped {
            cancel.wait(Duration::from_millis(millis));
            return Ok(());
        }
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), millis as u32) } {
            0 | 258 => Ok(()),
            _ => Err(Error::Process),
        }
    }
    pub fn wait_split(&self, cancel: &Cancellation, eof: [bool; 2], closing: bool) -> Result<(), Error> {
        self.wait(cancel, eof == [true; 2], closing)
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::Budget;
    use std::os::windows::ffi::OsStringExt;
    #[test]
    #[ignore = "Windows 真机及编译好的 tools/fixtures/windows_capture.c；不使用用户 SSH 配置"]
    fn native_job_capture_preserves_arguments_bounds_output_and_reaps_descendants() {
        use std::ffi::OsString;
        let fixture = std::env::var_os("CONGMINAL_TEST_CAPTURE_FIXTURE").expect("需指定独立 Windows 原生测试程序");
        let pool = Budget::new(2 * 1024 * 1024, 0);
        let cancel = Cancellation::default();
        let run = |mode: &str, limit: usize, token: &Cancellation, timeout: Duration| {
            super::super::run(
                || {
                    let mut spec = Spec::new(&fixture);
                    spec.arg(mode);
                    spec
                },
                &pool,
                token,
                timeout,
                limit,
            )
        };
        let output = run("emit", 4096, &cancel, Duration::from_secs(5)).unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.bytes.len(), 4096);
        for (index, byte) in output.bytes.iter().enumerate() {
            assert_eq!(*byte, (index % 251) as u8);
        }
        drop(output);
        assert_eq!(pool.snapshot().used, 0);
        {
            use super::super::Stream;
            use std::io::{Read, Write};
            let spec = |mode: &str| {
                let mut spec = Spec::new(&fixture);
                spec.arg(mode);
                spec
            };
            let mut stream = Stream::spawn(|| spec("echo"), &pool, &cancel, Duration::from_secs(5)).unwrap();
            let bytes: [u8; 8192] = std::array::from_fn(|i| (i % 251) as u8);
            let mut received = [0; 8192];
            let charged = pool.snapshot().used;
            for _ in 0..128 {
                stream.write_all(&bytes).unwrap();
                stream.read_exact(&mut received).unwrap();
                assert_eq!(bytes, received);
                assert_eq!(pool.snapshot().used, charged);
            }
            stream.finish().unwrap();
            for writing in [false, true] {
                let mut stream = Stream::spawn(|| spec("wait"), &pool, &cancel, Duration::from_millis(100)).unwrap();
                let error = if writing {
                    let bytes = [0; 65536];
                    loop {
                        if let Err(error) = stream.write_all(&bytes) {
                            break error;
                        }
                    }
                } else {
                    stream.read(&mut [0; 1]).unwrap_err()
                };
                assert_eq!(error.kind(), io::ErrorKind::TimedOut);
                assert_eq!(stream.failure(), Some(Error::Timeout));
                stream.finish().unwrap();
                assert_eq!(pool.snapshot().used, 0);
            }
            let token = Cancellation::default();
            let copy = token.clone();
            let mut stream = Stream::spawn(|| spec("wait"), &pool, &token, Duration::from_secs(5)).unwrap();
            let timer = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                copy.cancel();
            });
            let bytes = [0; 65536];
            let error = loop {
                if let Err(error) = stream.write_all(&bytes) {
                    break error;
                }
            };
            assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
            assert_eq!(stream.failure(), Some(Error::Cancelled));
            stream.finish().unwrap();
            timer.join().unwrap();
            assert_eq!(pool.snapshot().used, 0);
        }
        let args = [
            OsString::from(""),
            OsString::from("a b"),
            OsString::from("quote\"x"),
            OsString::from("slash\\"),
            OsString::from("x\\\"y"),
            OsString::from("中文;$()"),
            OsString::from_wide(&[0xd800, 92, 34]),
        ];
        let output = super::super::run(
            || {
                let mut spec = Spec::new(&fixture);
                spec.arg("argv").args(&args);
                spec
            },
            &pool,
            &cancel,
            Duration::from_secs(5),
            65536,
        )
        .unwrap();
        assert!(output.status.success());
        let mut bytes = output.bytes.as_slice();
        assert_eq!(&bytes[..4], b"CGA1");
        bytes = &bytes[4..];
        fn number(bytes: &mut &[u8]) -> usize {
            let value = u32::from_le_bytes(bytes[..4].try_into().unwrap());
            *bytes = &bytes[4..];
            value as usize
        }
        assert_eq!(number(&mut bytes), args.len());
        for arg in &args {
            let units = number(&mut bytes);
            let actual =
                bytes[..units * 2].as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect::<Vec<_>>();
            assert_eq!(actual, arg.encode_wide().collect::<Vec<_>>());
            bytes = &bytes[units * 2..];
        }
        assert!(bytes.is_empty());
        drop(output);
        assert!(matches!(run("flood", 1024, &cancel, Duration::from_secs(5)), Err(Error::Limit)));
        assert!(matches!(run("wait", 1024, &cancel, Duration::from_millis(100)), Err(Error::Timeout)));
        let folder = std::env::temp_dir().join(format!("congminal-capture-win-{}", std::process::id()));
        std::fs::create_dir(&folder).unwrap();
        struct Folder(std::path::PathBuf);
        impl Drop for Folder {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let folder = Folder(folder);
        let marker = folder.0.join("started");
        let worker_marker = marker.clone();
        let token = Cancellation::default();
        let copy = token.clone();
        let timer = std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(5);
            while !worker_marker.exists() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(worker_marker.exists(), "测试子程序没有进入等待阶段");
            copy.cancel();
        });
        let result = super::super::run(
            || {
                let mut spec = Spec::new(&fixture);
                spec.arg("wait").arg(&marker);
                spec
            },
            &pool,
            &token,
            Duration::from_secs(8),
            1024,
        );
        timer.join().unwrap();
        assert!(matches!(result, Err(Error::Cancelled)));
        let parent_marker = folder.0.join("parent-ready");
        let output = super::super::run(
            || {
                let mut spec = Spec::new(&fixture);
                spec.arg("parent").arg(&parent_marker);
                spec
            },
            &pool,
            &cancel,
            Duration::from_secs(8),
            4,
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.bytes.len(), 4);
        drop(output);
        assert_eq!(pool.snapshot().used, 0);
        eprintln!(
            "WINDOWS_CAPTURE argv=ok job=ok output=ok timeout=ok cancel=ok duplex=ok backpressure=ok descendant_pipe=ok peak={} final=0",
            pool.snapshot().peak
        );
    }
}
