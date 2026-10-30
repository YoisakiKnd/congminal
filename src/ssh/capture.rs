//! 后台短命令的有界输出捕获；不读取或转发认证输出。
use crate::{
    resources::{Budget, Kind, Lease, Tracked},
    tasks::Cancellation,
};
#[cfg(unix)]
use std::process::{Command, Stdio};
use std::{
    ffi::{OsStr, OsString},
    io,
    process::ExitStatus,
    sync::Arc,
    time::{Duration, Instant},
};
#[cfg(any(windows, test))]
#[path = "capture_windows_wire.rs"]
mod windows_wire;

/// 必须在已准入的 Stream 命令工厂内调用；复用受限 PATH 搜索，排除隐式 CWD 搜索。
#[cfg(windows)]
pub fn resolved_program(program: &OsStr) -> Result<OsString, Error> {
    use std::os::windows::ffi::OsStringExt;
    let wide = platform::executable(program)?;
    Ok(OsString::from_wide(&wide[..wide.len() - 1]))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Budget,
    Cancelled,
    Timeout,
    Limit,
    Missing,
    Permission,
    Process,
    Cleanup(i32),
    #[cfg(windows)]
    Unsupported,
}
pub struct Output {
    pub bytes: Tracked<Vec<u8>>,
    pub status: ExitStatus,
}

/// Split machine output with one owner and no pipe-reader threads. Buffers carry
/// their own leases through growth and delivery; cancellation covers drain and wait.
pub(crate) struct SplitOutput {
    pub stdout: Tracked<Vec<u8>>,
    pub stderr: Tracked<Vec<u8>>,
    pub truncated: bool,
    pub status: ExitStatus,
}

// If native cleanup cannot be confirmed, keep the process identity and its work
// lease in a fixed quarantine. Returning an error never discards the last owner.
// No allocation or lock is needed to reserve a slot, and no slot is reused while
// a retained process could still exist.
struct Retained {
    process: platform::Process,
    _memory: Lease,
}
struct RetirementCell {
    state: std::sync::atomic::AtomicU8, // free / reserved / retained / inspecting / initializing
    owner: std::sync::atomic::AtomicUsize,
    data: std::cell::UnsafeCell<std::mem::MaybeUninit<Retained>>,
}
// The state CAS grants exclusive access; readers only inspect the atomic state.
unsafe impl Sync for RetirementCell {}
impl RetirementCell {
    const fn new() -> Self {
        Self {
            state: std::sync::atomic::AtomicU8::new(0),
            owner: std::sync::atomic::AtomicUsize::new(0),
            data: std::cell::UnsafeCell::new(std::mem::MaybeUninit::uninit()),
        }
    }
}
static RETIREMENTS: [RetirementCell; 64] = [const { RetirementCell::new() }; 64];
struct RetirementSlot(Option<usize>);
impl RetirementSlot {
    fn reserve(pool: &Arc<Budget>) -> Result<Self, Error> {
        use std::sync::atomic::Ordering::{Acquire, Relaxed};
        RETIREMENTS
            .iter()
            .position(|cell| cell.state.compare_exchange(0, 4, Acquire, Relaxed).is_ok())
            .map(|index| {
                RETIREMENTS[index].owner.store(Arc::as_ptr(pool) as usize, Relaxed);
                RETIREMENTS[index].state.store(1, std::sync::atomic::Ordering::Release);
                Self(Some(index))
            })
            .ok_or(Error::Process)
    }
    fn retain(mut self, value: Retained) {
        let cell = &RETIREMENTS[self.0.take().unwrap()];
        // This reserved slot is exclusively owned until state=2 is published.
        unsafe {
            (*cell.data.get()).write(value);
        }
        cell.state.store(2, std::sync::atomic::Ordering::Release);
    }
}
impl Drop for RetirementSlot {
    fn drop(&mut self) {
        if let Some(index) = self.0 {
            RETIREMENTS[index].state.store(0, std::sync::atomic::Ordering::Release);
        }
    }
}
pub(crate) fn active() -> usize {
    RETIREMENTS.iter().filter(|cell| cell.state.load(std::sync::atomic::Ordering::Acquire) != 0).count()
}
pub(crate) fn unconfirmed() -> usize {
    RETIREMENTS.iter().filter(|cell| matches!(cell.state.load(std::sync::atomic::Ordering::Acquire), 2 | 3)).count()
}
pub(crate) fn active_in(pool: &Arc<Budget>) -> usize {
    RETIREMENTS
        .iter()
        .filter(|cell| {
            matches!(cell.state.load(std::sync::atomic::Ordering::Acquire), 1..=3)
                && cell.owner.load(std::sync::atomic::Ordering::Relaxed) == Arc::as_ptr(pool) as usize
        })
        .count()
}
pub(crate) fn unconfirmed_in(pool: &Arc<Budget>) -> usize {
    RETIREMENTS
        .iter()
        .filter(|cell| {
            matches!(cell.state.load(std::sync::atomic::Ordering::Acquire), 2 | 3)
                && cell.owner.load(std::sync::atomic::Ordering::Relaxed) == Arc::as_ptr(pool) as usize
        })
        .count()
}
/// Explicit background retry; no new threads, and never called on the UI thread.
pub(crate) fn retry_cleanup_in(pool: &Arc<Budget>) {
    use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let start = NEXT.fetch_add(1, Relaxed) % RETIREMENTS.len();
    for offset in 0..RETIREMENTS.len() {
        let cell = &RETIREMENTS[(start + offset) % RETIREMENTS.len()];
        if cell.owner.load(Relaxed) != Arc::as_ptr(pool) as usize {
            continue;
        }
        if cell.state.compare_exchange(2, 3, Acquire, Relaxed).is_err() {
            continue;
        }
        // State 3 excludes all other writers and guarantees initialized data.
        let value = unsafe { (*cell.data.get()).assume_init_mut() };
        if value.process.cleanup().is_ok() {
            let value = unsafe { (*cell.data.get()).assume_init_read() };
            drop(value); // Native handles first, then their lease, outside any lock.
            cell.state.store(0, Release);
        } else {
            cell.state.store(2, Release);
        }
        break; // At most one potentially slow native retirement per background request.
    }
}
struct CaptureChild {
    process: Option<platform::Process>,
    memory: Option<Lease>,
    retirement: Option<RetirementSlot>,
}
impl std::ops::Deref for CaptureChild {
    type Target = platform::Process;
    fn deref(&self) -> &Self::Target {
        self.process.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for CaptureChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.process.as_mut().unwrap()
    }
}
impl CaptureChild {
    fn finish(&mut self) -> Result<(), Error> {
        let Some(process) = &mut self.process else { return Ok(()) };
        if let Err(error) = process.cleanup() {
            self.retirement
                .take()
                .unwrap()
                .retain(Retained { process: self.process.take().unwrap(), _memory: self.memory.take().unwrap() });
            return Err(error);
        }
        drop(self.process.take());
        drop(self.memory.take());
        drop(self.retirement.take());
        Ok(())
    }
}
impl Drop for CaptureChild {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}
pub(crate) fn run_split(
    factory: impl FnOnce() -> Result<Spec, Error>,
    pool: &Arc<Budget>,
    kind: Kind,
    cancel: &Cancellation,
    timeout: Duration,
    limits: [usize; 2],
) -> Result<SplitOutput, Error> {
    if timeout.is_zero() || timeout > Duration::from_secs(120) || limits.iter().any(|&n| n > 32 * 1024 * 1024) {
        return Err(Error::Invalid);
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    // Includes bounded argv/Windows UTF-16 environment work. Native kernel memory
    // remains outside the managed-heap accounting, as in the original capture API.
    let deadline = Instant::now() + timeout;
    if unconfirmed_in(pool) != 0 {
        retry_cleanup_in(pool);
    }
    let work = pool.lease(kind, 1024 * 1024).map_err(|_| Error::Budget)?;
    let retirement = RetirementSlot::reserve(pool)?;
    let mut out = [
        Tracked::new(Vec::new(), pool.lease(kind, 0).map_err(|_| Error::Budget)?),
        Tracked::new(Vec::new(), pool.lease(kind, 0).map_err(|_| Error::Budget)?),
    ];
    let mut spec = factory()?;
    if spec.get_program().is_empty()
        || spec.get_program().len() > 4096
        || spec.get_args().count() > 64
        || spec.get_args().map(OsStr::len).sum::<usize>() > 128 * 1024
        || spec.get_args().any(|arg| arg.as_encoded_bytes().contains(&0))
    {
        return Err(Error::Invalid);
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(Error::Timeout);
    }
    spec.split = true;
    let mut child = CaptureChild {
        process: Some(platform::Process::spawn(&mut spec, None)?),
        memory: Some(work),
        retirement: Some(retirement),
    };
    drop(spec);
    let mut eof = [false; 2];
    let mut truncated = [false; 2];
    let mut status = None;
    let mut closing_checks = 0;
    let mut chunk = [0u8; 8192];
    let result = (|| {
        loop {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout);
            }
            let mut progress = false;
            for stream in 0..2 {
                if eof[stream] {
                    continue;
                }
                let read = if stream == 0 { child.read(&mut chunk) } else { child.read_error(&mut chunk) };
                match read {
                    Ok(0) => eof[stream] = true,
                    Ok(n) => {
                        let take = n.min(limits[stream].saturating_sub(out[stream].len()));
                        let needed = out[stream].len() + take;
                        if needed > out[stream].data.capacity() {
                            let capacity =
                                needed.max(out[stream].data.capacity().saturating_mul(2)).min(limits[stream]);
                            out[stream].reserve_capacity(capacity).ok_or(Error::Budget)?;
                        }
                        out[stream].data.extend_from_slice(&chunk[..take]);
                        truncated[stream] |= take < n;
                        progress = true;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => return Err(Error::Process),
                }
            }
            if status.is_none() && child.exited()? {
                status = Some(child.stop()?);
            }
            if eof == [true; 2]
                && let Some(status) = status
            {
                return Ok(status);
            }
            if !progress {
                child.wait_split(cancel, eof, closing_checks < 2)?;
                if eof == [true; 2] {
                    closing_checks += 1;
                }
            }
        }
    })();
    child.finish()?;
    let status = result?;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let [stdout, stderr] = out;
    Ok(SplitOutput { stdout, stderr, truncated: truncated[0], status })
}
fn process_error(error: io::Error) -> Error {
    match error.kind() {
        io::ErrorKind::NotFound => Error::Missing,
        io::ErrorKind::PermissionDenied => Error::Permission,
        _ => Error::Process,
    }
}
fn validate(command: &Spec) -> Result<(), Error> {
    if command.get_program().is_empty()
        || command.get_program().len() > 4096
        || command.get_program().as_encoded_bytes().contains(&0)
        || command.get_args().count() > 64
        || command.get_args().any(|arg| arg.as_encoded_bytes().contains(&0))
        || command.get_args().map(|a| a.len()).sum::<usize>() > 16 * 1024
    {
        Err(Error::Invalid)
    } else {
        Ok(())
    }
}
/// 有意不提供环境覆盖、工作目录或隐式 shell；两平台执行同一份参数规格。
pub struct Spec {
    program: OsString,
    args: Vec<OsString>,
    pub(crate) split: bool,
    pub(crate) git_environment: bool,
}
impl Spec {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self { program: program.as_ref().into(), args: Vec::new(), split: false, git_environment: false }
    }
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().into());
        self
    }
    pub fn args(&mut self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> &mut Self {
        for arg in args {
            self.arg(arg);
        }
        self
    }
    pub fn get_program(&self) -> &OsStr {
        &self.program
    }
    pub fn get_args(&self) -> impl ExactSizeIterator<Item = &OsStr> {
        self.args.iter().map(OsString::as_os_str)
    }
    #[cfg(unix)]
    pub(super) fn standard(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if self.git_environment {
            command
                .env_remove("GIT_GLOB_PATHSPECS")
                .env_remove("GIT_NOGLOB_PATHSPECS")
                .env_remove("GIT_ICASE_PATHSPECS")
                .env("GIT_LITERAL_PATHSPECS", "1")
                .env("GIT_OPTIONAL_LOCKS", "0")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_PAGER", "cat")
                .env("LC_ALL", "C");
        }
        command
    }
}
/// 只在已有的有界后台槽执行。工厂在额度准入后调用；输出最多 128 KiB。
pub fn run(
    command: impl FnOnce() -> Spec,
    pool: &Arc<Budget>,
    cancel: &Cancellation,
    timeout: Duration,
    limit: usize,
) -> Result<Output, Error> {
    if timeout.is_zero() || timeout > Duration::from_secs(30) || limit > 128 * 1024 {
        return Err(Error::Invalid);
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    // 固定工作预留与返回缓冲分别持有；系统进程/内核缓冲不属于受管堆。
    let _work = pool.lease(Kind::Ssh, 256 * 1024).map_err(|_| Error::Budget)?;
    let memory = pool.lease(Kind::Ssh, limit + 128).map_err(|_| Error::Budget)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(limit).map_err(|_| Error::Budget)?;
    let deadline = Instant::now() + timeout;
    let mut command = command();
    validate(&command)?;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(Error::Timeout);
    }
    let mut child = platform::Process::spawn(&mut command, None)?;
    let mut status = None;
    let mut eof = false;
    let mut closing_checks = 0;
    let mut chunk = [0u8; 8192];
    let result = (|| {
        loop {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout);
            }
            let mut progress = false;
            if !eof {
                match child.read(&mut chunk) {
                    Ok(0) => eof = true,
                    Ok(n) => {
                        if n > limit - bytes.len() {
                            return Err(Error::Limit);
                        }
                        bytes.extend_from_slice(&chunk[..n]);
                        progress = true;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => return Err(Error::Process),
                }
            }
            // 先观察退出但保留进程身份，结束整个受控组后才回收主进程。
            if status.is_none() && child.exited()? {
                status = Some(child.stop()?);
            }
            if eof && let Some(status) = status {
                return Ok(status);
            }
            if !progress {
                // 管道 EOF 往往略早于进程退出；给退出尾声两个短等待，
                // 避免每次查询固定增加 5 ms。提前关闭 stdout 的长任务随后恢复慢等待。
                child.wait(cancel, eof, closing_checks < 2)?;
                if eof {
                    closing_checks += 1;
                }
            }
        }
    })();
    // 普通错误路径也检查清理结果；Drop 只作为异常展开时的兜底。
    let cleanup = child.cleanup();
    cleanup?;
    Ok(Output { bytes: Tracked::new(bytes, memory), status: result? })
}

/// 独立二进制管道，仅在后台线程使用；不分配 PTY，不接触终端输入输出。
/// timeout 限制单次 Read/Write 的等待；整个任务的生命周期由 cancel 控制。
/// 所有错误立即清理受控子进程。普通结束须显式 finish，以便报告清理失败。
pub struct Stream {
    process: platform::Process,
    cancel: Cancellation,
    timeout: Duration,
    failed: Option<Error>,
    closed: bool,
    eof: bool,
    _memory: Lease,
}
struct IoTiming {
    deadline: Instant,
    observed: Instant,
    waiting: bool,
}
impl IoTiming {
    fn new(timeout: Duration) -> Self {
        let observed = Instant::now();
        Self { deadline: observed + timeout, observed, waiting: false }
    }
    fn advance(&mut self, waiting: bool) -> Result<(), Error> {
        let now = Instant::now();
        if self.waiting || waiting {
            self.deadline =
                self.deadline.checked_add(now.saturating_duration_since(self.observed)).ok_or(Error::Timeout)?;
        }
        self.observed = now;
        self.waiting = waiting;
        Ok(())
    }
}
impl Stream {
    pub fn spawn(
        command: impl FnOnce() -> Spec,
        pool: &Arc<Budget>,
        cancel: &Cancellation,
        timeout: Duration,
    ) -> Result<Self, Error> {
        Self::spawn_checked(|| Ok(command()), pool, cancel, timeout)
    }
    pub fn spawn_checked(
        command: impl FnOnce() -> Result<Spec, Error>,
        pool: &Arc<Budget>,
        cancel: &Cancellation,
        timeout: Duration,
    ) -> Result<Self, Error> {
        if timeout.is_zero() || timeout > Duration::from_secs(30) {
            return Err(Error::Invalid);
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let memory = pool.lease(Kind::Sftp, 256 * 1024).map_err(|_| Error::Budget)?;
        let mut command = command()?;
        validate(&command)?;
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let process = platform::Process::spawn(&mut command, Some(pool))?;
        Ok(Self { process, cancel: cancel.clone(), timeout, failed: None, closed: false, eof: false, _memory: memory })
    }
    pub fn failure(&self) -> Option<Error> {
        self.failed
    }
    fn close(&mut self) -> Result<(), Error> {
        self.closed = true;
        if let Err(error) = self.process.cleanup() {
            self.failed = Some(error);
            return Err(error);
        }
        if let Some(error @ Error::Cleanup(_)) = self.failed { Err(error) } else { Ok(()) }
    }
    pub fn finish(mut self) -> Result<(), Error> {
        self.close()
    }
    fn fail(&mut self, error: Error) -> io::Error {
        let error = self.close().err().unwrap_or(error);
        self.failed = Some(error);
        Self::io_error(error)
    }
    fn io_error(error: Error) -> io::Error {
        let kind = match error {
            Error::Cancelled => io::ErrorKind::ConnectionAborted,
            Error::Timeout => io::ErrorKind::TimedOut,
            _ => io::ErrorKind::Other,
        };
        io::Error::new(kind, format!("independent SSH stream: {error:?}"))
    }
    fn check(&mut self, deadline: Instant) -> io::Result<()> {
        if let Some(error) = self.failed {
            return Err(Self::io_error(error));
        }
        if self.cancel.is_cancelled() {
            return Err(self.fail(Error::Cancelled));
        }
        if Instant::now() >= deadline {
            return Err(self.fail(Error::Timeout));
        }
        Ok(())
    }
}
impl Stream {
    fn service(&mut self, timing: &mut IoTiming, service: &mut impl FnMut() -> Result<bool, Error>) -> io::Result<()> {
        if let Some(error) = self.failed {
            return Err(Self::io_error(error));
        }
        let waiting = service().map_err(|e| self.fail(e))?;
        timing.advance(waiting).map_err(|e| self.fail(e))?;
        self.check(timing.deadline)
    }
    /// 同一工作槽推进有界服务；true 只用于有独立期限的用户认证等待。
    /// 回调不得阻塞或靠每次 true 无限延长认证生命周期。
    pub fn read_serviced(
        &mut self,
        bytes: &mut [u8],
        mut service: impl FnMut() -> Result<bool, Error>,
    ) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut timing = IoTiming::new(self.timeout);
        loop {
            self.service(&mut timing, &mut service)?;
            if self.eof {
                return Ok(0);
            }
            match self.process.read(bytes) {
                Ok(0) => {
                    self.close().map_err(|e| self.fail(e))?;
                    self.eof = true;
                    return Ok(0);
                }
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return Err(self.fail(Error::Process)),
            }
            if self.closed {
                self.eof = true;
                return Ok(0);
            }
            match self.process.exited() {
                Ok(true) => {
                    self.close().map_err(|e| self.fail(e))?;
                    // 退出与上面的非阻塞读取可能交错；回收后仍逐次排空已有输出。
                    continue;
                }
                Ok(false) => {}
                Err(e) => return Err(self.fail(e)),
            }
            self.process.wait(&self.cancel, false, false).map_err(|e| self.fail(e))?;
        }
    }
}
impl Stream {
    pub fn flush_serviced(&mut self, mut service: impl FnMut() -> Result<bool, Error>) -> io::Result<()> {
        let mut timing = IoTiming::new(self.timeout);
        self.service(&mut timing, &mut service)
    }
    pub fn write_serviced(
        &mut self,
        bytes: &[u8],
        mut service: impl FnMut() -> Result<bool, Error>,
    ) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut timing = IoTiming::new(self.timeout);
        loop {
            self.service(&mut timing, &mut service)?;
            if self.closed {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            match self.process.write(&bytes[..bytes.len().min(65536)]) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return Err(self.fail(Error::Process)),
            }
            self.process.wait_write(&self.cancel).map_err(|e| self.fail(e))?;
        }
    }
}
impl io::Read for Stream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.read_serviced(bytes, || Ok(false))
    }
}
impl io::Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_serviced(bytes, || Ok(false))
    }
    fn flush(&mut self) -> io::Result<()> {
        // 每次 write 在实际管道写入完成后才返回；没有另设用户态队列。
        self.flush_serviced(|| Ok(false))
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::{
        io::{Read, Write},
        os::{fd::AsRawFd, unix::process::CommandExt},
        process::{Child, ChildStderr, ChildStdin, ChildStdout},
    };
    #[repr(C, align(16))]
    struct Info {
        signo: i32,
        rest: [u8; 252],
    }
    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }
    #[cfg(target_os = "macos")]
    type Count = u32;
    #[cfg(target_os = "linux")]
    type Count = std::ffi::c_ulong;
    unsafe extern "C" {
        fn fcntl(fd: i32, command: i32, ...) -> i32;
        fn kill(pid: i32, signal: i32) -> i32;
        fn waitid(kind: i32, id: u32, info: *mut std::ffi::c_void, options: i32) -> i32;
        fn poll(fds: *mut PollFd, count: Count, timeout: i32) -> i32;
    }
    #[cfg(target_os = "macos")]
    const NONBLOCK: i32 = 4;
    #[cfg(target_os = "linux")]
    const NONBLOCK: i32 = 2048;
    #[cfg(target_os = "macos")]
    const NOWAIT: i32 = 0x20;
    #[cfg(target_os = "linux")]
    const NOWAIT: i32 = 0x0100_0000;
    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        fn proc_listpids(kind: u32, info: u32, buffer: *mut std::ffi::c_void, size: i32) -> i32;
    }
    #[cfg(target_os = "macos")]
    fn only_exited_leader(pid: u32) -> bool {
        // Darwin 对仅剩 zombie 组返回 EPERM。只在确认该组唯一成员就是
        // 尚未回收的已退出主进程时接受；其他权限错误继续报告，不能笼统忽略 EPERM。
        let mut members = [0i32; 2];
        let bytes =
            unsafe { proc_listpids(2, pid, members.as_mut_ptr().cast(), std::mem::size_of_val(&members) as i32) };
        bytes == 4 && members[0] as u32 == pid
    }
    pub struct Process {
        child: Child,
        output: Option<ChildStdout>,
        error: Option<ChildStderr>,
        input: Option<ChildStdin>,
        stopped: bool,
    }
    impl Process {
        pub fn spawn(spec: &mut Spec, duplex: Option<&Arc<Budget>>) -> Result<Self, Error> {
            let mut command = spec.standard();
            command.stdin(if duplex.is_some() { Stdio::piped() } else { Stdio::null() });
            command.stdout(Stdio::piped()).stderr(if spec.split { Stdio::piped() } else { Stdio::null() });
            command.process_group(0);
            let child = {
                let _guard = crate::process_launch::guard();
                command.spawn().map_err(process_error)?
            };
            let mut process = Self { child, output: None, error: None, input: None, stopped: false };
            process.output = process.child.stdout.take();
            process.error = process.child.stderr.take();
            process.input = process.child.stdin.take();
            let setup = (|| {
                for fd in std::iter::once(process.output.as_ref().ok_or(Error::Process)?.as_raw_fd())
                    .chain(process.input.as_ref().map(AsRawFd::as_raw_fd))
                    .chain(process.error.as_ref().map(AsRawFd::as_raw_fd))
                {
                    let flags = unsafe { fcntl(fd, 3) };
                    if flags < 0 || unsafe { fcntl(fd, 4, flags | NONBLOCK) } < 0 {
                        return Err(Error::Process);
                    }
                }
                Ok(())
            })();
            if let Err(error) = setup {
                process.cleanup()?;
                return Err(error);
            }
            Ok(process)
        }
        pub fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.output.as_mut().unwrap().read(bytes)
        }
        pub fn read_error(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            match self.error.as_mut() {
                Some(error) => error.read(bytes),
                None => Ok(0),
            }
        }
        pub fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.input.as_mut().ok_or(io::ErrorKind::BrokenPipe)?.write(bytes)
        }
        pub fn wait_write(&self, _cancel: &Cancellation) -> Result<(), Error> {
            let mut fd = PollFd { fd: self.input.as_ref().ok_or(Error::Process)?.as_raw_fd(), events: 4, revents: 0 };
            if unsafe { poll(&mut fd, 1, 5) } < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(Error::Process);
            }
            Ok(())
        }
        pub fn wait(&self, cancel: &Cancellation, eof: bool, closing: bool) -> Result<(), Error> {
            if eof {
                cancel.wait(if closing { Duration::from_micros(100) } else { Duration::from_millis(5) });
                return Ok(());
            }
            let mut fd = PollFd { fd: self.output.as_ref().unwrap().as_raw_fd(), events: 1, revents: 0 };
            // 输出就绪立即唤醒；仅在活动查询等待时使用最多 5 ms 的取消检查间隔。
            if unsafe { poll(&mut fd, 1, 5) } < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(Error::Process);
            }
            Ok(())
        }
        pub fn wait_split(&self, cancel: &Cancellation, eof: [bool; 2], closing: bool) -> Result<(), Error> {
            if cancel.is_cancelled() {
                return Ok(());
            }
            if eof == [true; 2] {
                return self.wait(cancel, true, closing);
            }
            let mut fds = [PollFd { fd: -1, events: 1, revents: 0 }, PollFd { fd: -1, events: 1, revents: 0 }];
            if !eof[0] {
                fds[0].fd = self.output.as_ref().unwrap().as_raw_fd();
            }
            if !eof[1] {
                fds[1].fd = self.error.as_ref().unwrap().as_raw_fd();
            }
            // Wait on whichever stream is still open. Polling an already closed
            // stdout can spin, and sleeping on its EOF adds 5ms per short command.
            if unsafe { poll(fds.as_mut_ptr(), 2, 5) } < 0
                && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
            {
                return Err(Error::Process);
            }
            Ok(())
        }
        pub fn exited(&mut self) -> Result<bool, Error> {
            if self.stopped {
                return Ok(true);
            }
            // macOS/Linux siginfo 的首字段是 si_signo；存储大于两者的完整结构且正确对齐。
            // WNOWAIT 保留 PID/PGID，避免退出回收后再 kill(-pgid) 误伤复用的进程组。
            let mut info = Info { signo: 0, rest: [0; 252] };
            if unsafe { waitid(1, self.child.id(), (&mut info as *mut Info).cast(), 1 | 4 | NOWAIT) } < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    return Ok(false);
                }
                return Err(Error::Process);
            }
            Ok(info.signo != 0)
        }
        pub fn stop(&mut self) -> Result<ExitStatus, Error> {
            assert!(!self.stopped);
            let result = unsafe { kill(-(self.child.id() as i32), 9) };
            let group_error =
                if result < 0 { io::Error::last_os_error().raw_os_error().filter(|e| *e != 3) } else { None };
            // 即使主进程自行离开了原组，也终止仍由 Child 持有身份的主进程。
            let _ = self.child.kill();
            #[cfg(target_os = "macos")]
            let group_error = if group_error == Some(1) {
                // 最后一次写端关闭可先于 waitid 可见的退出。保持未回收身份，
                // 给已终止主进程进入 zombie 的尾声；仍只接受组内唯一的退出主进程。
                let deadline = Instant::now() + Duration::from_secs(2);
                while !self.exited().unwrap_or(false) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
                if self.exited().unwrap_or(false) && only_exited_leader(self.child.id()) { None } else { group_error }
            } else {
                group_error
            };
            // Keep the unreaped leader identity if group termination is unknown.
            if let Some(error) = group_error {
                return Err(Error::Cleanup(error));
            }
            let status = self.child.wait().map_err(|e| Error::Cleanup(e.raw_os_error().unwrap_or(0)))?;
            self.stopped = true;
            Ok(status)
        }
        pub fn cleanup(&mut self) -> Result<(), Error> {
            if self.stopped { Ok(()) } else { self.stop().map(|_| ()) }
        }
    }
    impl Drop for Process {
        fn drop(&mut self) {
            if !self.stopped {
                let _ = self.stop();
            }
        }
    }
}

#[cfg(windows)]
#[path = "capture_windows.rs"]
mod platform;

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn unknown_native_owner_retains_credit_and_only_its_budget_can_reclaim_it() {
        let pool = Budget::new(4 * 1024 * 1024, 0);
        let other = Budget::new(4 * 1024 * 1024, 0);
        struct Drain(Arc<Budget>);
        impl Drop for Drain {
            fn drop(&mut self) {
                retry_cleanup_in(&self.0);
            }
        }
        let _drain = Drain(pool.clone());
        let memory = pool.lease(Kind::Git, 1024 * 1024).unwrap();
        let slot = RetirementSlot::reserve(&pool).unwrap();
        let mut spec = shell("exec sleep 30");
        let process = platform::Process::spawn(&mut spec, None).unwrap();
        drop(spec);
        // Fault fixture: native ownership is handed to the unknown-cleanup path.
        slot.retain(Retained { process, _memory: memory });
        assert_eq!(unconfirmed_in(&pool), 1);
        assert_eq!(unconfirmed_in(&other), 0);
        retry_cleanup_in(&other);
        assert_eq!(pool.snapshot().used, 1024 * 1024);
        retry_cleanup_in(&pool);
        assert_eq!(unconfirmed_in(&pool), 0);
        assert_eq!(active_in(&pool), 0);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[cfg(unix)]
    #[test]
    fn split_capture_drains_both_streams_and_limits_before_allocation() {
        let pool = Budget::new(8 * 1024 * 1024, 0);
        let output = run_split(
            || Ok(shell("head -c 1048576 /dev/zero; head -c 1048576 /dev/zero >&2")),
            &pool,
            Kind::Git,
            &Cancellation::default(),
            Duration::from_secs(5),
            [1024, 2048],
        )
        .unwrap();
        assert!(output.status.success());
        assert!(output.truncated);
        assert_eq!(output.stdout.len(), 1024);
        assert_eq!(output.stderr.len(), 2048);
        assert_eq!(pool.snapshot().used, output.stdout.data.capacity() + output.stderr.data.capacity());
        drop(output);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[cfg(unix)]
    #[test]
    fn exited_parent_does_not_leave_drain_waiting_for_inherited_pipe() {
        let pool = Budget::new(8 * 1024 * 1024, 0);
        let at = Instant::now();
        let output = run_split(
            || Ok(shell("sleep 30 & printf READY; exec /usr/bin/true")),
            &pool,
            Kind::Monitor,
            &Cancellation::default(),
            Duration::from_secs(5),
            [4096, 0],
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.data, b"READY");
        assert!(at.elapsed() < Duration::from_secs(3));
        drop(output);
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn invalid_cancel_and_quota_do_not_construct_a_command() {
        let pool = Budget::new(0, 0);
        let cancel = Cancellation::default();
        assert!(matches!(run(|| panic!(), &pool, &cancel, Duration::ZERO, 0), Err(Error::Invalid)));
        cancel.cancel();
        assert!(matches!(run(|| panic!(), &pool, &cancel, Duration::from_secs(1), 0), Err(Error::Cancelled)));
        assert!(matches!(
            run(|| panic!(), &pool, &Cancellation::default(), Duration::from_secs(1), 0),
            Err(Error::Budget)
        ));
        assert_eq!(pool.snapshot().used, 0);
        assert!(matches!(Stream::spawn(|| panic!(), &pool, &cancel, Duration::from_secs(1)), Err(Error::Cancelled)));
        assert!(matches!(
            Stream::spawn(|| panic!(), &pool, &Cancellation::default(), Duration::from_secs(1)),
            Err(Error::Budget)
        ));
    }
    #[cfg(unix)]
    fn shell(body: &str) -> Spec {
        let mut command = Spec::new("/bin/sh");
        command.args(["-c", body]);
        command
    }
    #[cfg(unix)]
    #[test]
    fn serviced_wait_has_separate_deadline_and_callback_failure_reaps_process() {
        use std::io::Read;
        let pool = Budget::new(512 * 1024, 0);
        let cancel = Cancellation::default();
        let mut stream =
            Stream::spawn(|| shell("sleep 0.15; printf x; sleep 30"), &pool, &cancel, Duration::from_millis(40))
                .unwrap();
        let started = Instant::now();
        assert_eq!(stream.read_serviced(&mut [0; 1], || Ok(started.elapsed() < Duration::from_secs(1))).unwrap(), 1);
        // 服务拒绝后立刻结束受控组；不等待原来的 30 秒子进程。
        assert!(stream.read_serviced(&mut [0; 1], || Err(Error::Cancelled)).is_err());
        assert_eq!(stream.failure(), Some(Error::Cancelled));
        stream.finish().unwrap();
        assert_eq!(pool.snapshot().used, 0);
        let mut stream = Stream::spawn(|| shell("sleep 30"), &pool, &cancel, Duration::from_millis(40)).unwrap();
        let started = Instant::now();
        assert!(stream.read_serviced(&mut [0; 1], || Ok(started.elapsed() < Duration::from_millis(100))).is_err());
        assert_eq!(stream.failure(), Some(Error::Timeout));
        assert!(started.elapsed() >= Duration::from_millis(100));
        // 普通 Read 的取消和失败仍由相同监督器执行。
        assert!(stream.read(&mut [0; 1]).is_err());
        stream.finish().unwrap();
        assert_eq!(pool.snapshot().used, 0);
    }
    #[cfg(unix)]
    #[test]
    fn duplex_stream_roundtrips_binary_without_accumulating_a_queue() {
        use std::io::{Read, Write};
        let pool = Budget::new(1024 * 1024, 0);
        let mut stream =
            Stream::spawn(|| shell("exec cat"), &pool, &Cancellation::default(), Duration::from_secs(2)).unwrap();
        let input: [u8; 8192] = std::array::from_fn(|i| (i % 251) as u8);
        let mut output = [0; 8192];
        let charged = pool.snapshot().used;
        for _ in 0..128 {
            stream.write_all(&input).unwrap();
            stream.read_exact(&mut output).unwrap();
            assert_eq!(input, output);
            assert_eq!(pool.snapshot().used, charged);
        }
        stream.finish().unwrap();
        assert_eq!(pool.snapshot().used, 0);
    }
    #[cfg(unix)]
    #[test]
    fn duplex_read_and_backpressured_write_timeout_close_the_process() {
        use std::io::{Read, Write};
        let pool = Budget::new(1024 * 1024, 0);
        for writing in [false, true] {
            let mut stream =
                Stream::spawn(|| shell("sleep 30"), &pool, &Cancellation::default(), Duration::from_millis(60))
                    .unwrap();
            let start = Instant::now();
            let error = if writing {
                let bytes = [0; 65536];
                loop {
                    if let Err(e) = stream.write_all(&bytes) {
                        break e;
                    }
                }
            } else {
                stream.read(&mut [0; 1]).unwrap_err()
            };
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert_eq!(stream.failure(), Some(Error::Timeout));
            assert!(start.elapsed() < Duration::from_secs(3));
            stream.finish().unwrap();
            assert_eq!(pool.snapshot().used, 0);
        }
    }
    #[cfg(unix)]
    #[test]
    fn duplex_cancel_after_ready_and_exited_leader_return_credit() {
        use std::io::Read;
        let pool = Budget::new(1024 * 1024, 0);
        let cancel = Cancellation::default();
        let mut stream = Stream::spawn(|| shell("printf x; sleep 30"), &pool, &cancel, Duration::from_secs(2)).unwrap();
        let mut bytes = [0; 1];
        stream.read_exact(&mut bytes).unwrap();
        cancel.cancel();
        assert_eq!(stream.read(&mut bytes).unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        stream.finish().unwrap();
        let mut stream = Stream::spawn(
            || shell("sleep 30 & printf x; exit 0"),
            &pool,
            &Cancellation::default(),
            Duration::from_secs(2),
        )
        .unwrap();
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(stream.read(&mut bytes).unwrap(), 0);
        stream.finish().unwrap();
        assert_eq!(pool.snapshot().used, 0);
    }
    #[cfg(unix)]
    #[test]
    fn duplex_stopped_process_still_drains_all_buffered_output() {
        use std::io::Read;
        let pool = Budget::new(1024 * 1024, 0);
        let mut stream = Stream::spawn(
            || shell("exec dd if=/dev/zero bs=8192 count=1"),
            &pool,
            &Cancellation::default(),
            Duration::from_secs(2),
        )
        .unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        while !stream.process.exited().unwrap() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        stream.close().unwrap();
        let mut chunk = [1; 97];
        let mut total = 0;
        loop {
            let n = stream.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            assert!(chunk[..n].iter().all(|b| *b == 0));
            total += n;
        }
        assert_eq!(total, 8192);
        stream.finish().unwrap();
        assert_eq!(pool.snapshot().used, 0);
    }
    #[cfg(unix)]
    #[test]
    fn bounded_output_exit_failure_and_cancel_return_credit() {
        let pool = Budget::new(1024 * 1024, 0);
        let cancel = Cancellation::default();
        let output =
            run(|| shell("printf 'host value\\n'; exit 7"), &pool, &cancel, Duration::from_secs(2), 11).unwrap();
        assert_eq!(output.bytes.as_slice(), b"host value\n");
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(pool.snapshot().used, 11 + 128);
        drop(output);
        assert_eq!(pool.snapshot().used, 0);
        assert!(matches!(run(|| shell("yes x"), &pool, &cancel, Duration::from_secs(2), 1024), Err(Error::Limit)));
        assert!(matches!(
            run(|| shell("sleep 30"), &pool, &cancel, Duration::from_millis(40), 1024),
            Err(Error::Timeout)
        ));
        assert!(matches!(
            run(|| shell("exec 1>&-; sleep 30"), &pool, &cancel, Duration::from_millis(40), 1024),
            Err(Error::Timeout)
        ));
        let copy = cancel.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            copy.cancel();
        });
        assert!(matches!(
            run(|| shell("sleep 30"), &pool, &cancel, Duration::from_secs(2), 1024),
            Err(Error::Cancelled)
        ));
        thread.join().unwrap();
        assert_eq!(pool.snapshot().used, 0);
    }
    #[cfg(unix)]
    #[test]
    fn exited_leader_with_descendant_holding_pipe_does_not_hang() {
        let pool = Budget::new(1024 * 1024, 0);
        let start = Instant::now();
        let output = run(
            || shell("sleep 30 & printf 'done'; exit 0"),
            &pool,
            &Cancellation::default(),
            Duration::from_secs(2),
            4,
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.bytes.as_slice(), b"done");
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(output);
        assert_eq!(pool.snapshot().used, 0);
    }
}
