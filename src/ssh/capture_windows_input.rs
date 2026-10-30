//! 独立流的异步写端：最多一个 64 KiB 请求，完成前拥有缓冲与 OVERLAPPED。
use super::*;
use crate::resources::{Budget, Kind, Tracked};
use std::{
    cell::{Cell, UnsafeCell},
    sync::Arc,
};

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: Handle,
}
#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<Overlapped>() == 32);
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateNamedPipeW(
        name: *const u16,
        access: u32,
        mode: u32,
        instances: u32,
        output: u32,
        input: u32,
        timeout: u32,
        security: *const c_void,
    ) -> Handle;
    fn ConnectNamedPipe(pipe: Handle, overlapped: *mut c_void) -> i32;
    fn CreateEventW(security: *const c_void, manual: i32, initial: i32, name: *const u16) -> Handle;
    fn ResetEvent(event: Handle) -> i32;
    fn WriteFile(pipe: Handle, bytes: *const c_void, length: u32, written: *mut u32, overlapped: *mut c_void) -> i32;
    fn GetOverlappedResult(pipe: Handle, overlapped: *mut Overlapped, transferred: *mut u32, wait: i32) -> i32;
    fn CancelIoEx(pipe: Handle, overlapped: *mut Overlapped) -> i32;
}
#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptGenRandom(algorithm: Handle, bytes: *mut u8, length: u32, flags: u32) -> i32;
}
struct State {
    pipe: OwnedHandle,
    event: OwnedHandle,
    // 内核可能在调用返回后访问这些地址；等待期间只借用共享 State。
    overlapped: UnsafeCell<Overlapped>,
    bytes: UnsafeCell<[u8; 65536]>,
    pending: Cell<bool>,
}
pub(super) struct Input {
    // Box（包括内联 64 KiB 数据）先释放，然后归还额度；异常保留两者。
    state: Option<Tracked<Box<State>>>,
    retained: bool,
}
// SAFETY: 所有请求地址都位于独占 Box 内，移动 Input 不移动 OVERLAPPED/数据。
// 句柄可跨线程使用；只有 &mut Input 能提交或结束请求，未实现 Sync。
// 完成前的内核访问及取消失败保留路径仍由 State 的原有拥有者约束。
unsafe impl Send for Input {}
impl Input {
    /// 调用者持有 process_launch guard；只将客户端读端放入继承白名单。
    pub(super) fn new(pool: &Arc<Budget>, security: &Security) -> Result<(Self, OwnedHandle), Error> {
        let memory = pool.lease(Kind::Sftp, std::mem::size_of::<State>() + 128).map_err(|_| Error::Budget)?;
        let mut random = [0u8; 16];
        if unsafe { BCryptGenRandom(null_mut(), random.as_mut_ptr(), random.len() as u32, 2) } < 0 {
            return Err(Error::Process);
        }
        let name = format!(r"\\.\pipe\congminal-sftp-{:032x}", u128::from_le_bytes(random));
        let name = wide(OsStr::new(&name))?;
        let pipe = owned(unsafe {
            // OUTBOUND | OVERLAPPED | FIRST_PIPE_INSTANCE, BYTE | REJECT_REMOTE_CLIENTS。
            CreateNamedPipeW(name.as_ptr(), 0x4008_0002, 8, 1, 65536, 0, 0, null())
        })?;
        let event = owned(unsafe { CreateEventW(null(), 1, 0, null()) })?;
        let overlapped =
            Overlapped { internal: 0, internal_high: 0, offset: 0, offset_high: 0, event: event.as_raw_handle() };
        let input = Self {
            retained: false,
            state: Some(Tracked::new(
                Box::new(State {
                    pipe,
                    event,
                    overlapped: UnsafeCell::new(overlapped),
                    bytes: UnsafeCell::new([0; 65536]),
                    pending: Cell::new(false),
                }),
                memory,
            )),
        };
        // 先打开自己的唯一客户端，再确认连接；若被抢先占用则失败，不向陌生端写数据。
        let client = owned(unsafe {
            CreateFileW(name.as_ptr(), 0x8000_0000, 0, (security as *const Security).cast(), 3, 0x80, null_mut())
        })?;
        let state = input.state.as_ref().unwrap();
        if unsafe { ConnectNamedPipe(state.pipe.as_raw_handle(), state.overlapped.get().cast()) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(535) {
                state.pending.set(error.raw_os_error() == Some(997));
                return Err(process_error(error));
            }
        }
        Ok((input, client))
    }
    pub(super) fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let state = self.state.as_ref().ok_or(io::ErrorKind::BrokenPipe)?;
        if !state.pending.get() {
            if bytes.len() > 65536 {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            if unsafe { ResetEvent(state.event.as_raw_handle()) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // 无未完成请求时独占写入；提交后直到完成都不再触碰缓冲内容。
            unsafe {
                (*state.overlapped.get()).internal = 0;
                (*state.overlapped.get()).internal_high = 0;
                (&mut *state.bytes.get())[..bytes.len()].copy_from_slice(bytes);
            }
            if unsafe {
                WriteFile(
                    state.pipe.as_raw_handle(),
                    state.bytes.get().cast(),
                    bytes.len() as u32,
                    null_mut(),
                    state.overlapped.get().cast(),
                )
            } == 0
            {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(997) {
                    return Err(error);
                }
            }
            state.pending.set(true);
        }
        let mut count = 0;
        if unsafe { GetOverlappedResult(state.pipe.as_raw_handle(), state.overlapped.get(), &mut count, 0) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(996) {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            state.pending.set(false);
            return Err(error);
        }
        state.pending.set(false);
        Ok(count as usize)
    }
    pub(super) fn wait(&self) -> Result<(), Error> {
        let state = self.state.as_ref().ok_or(Error::Process)?;
        match unsafe { WaitForSingleObject(state.event.as_raw_handle(), 5) } {
            0 | 258 => Ok(()),
            _ => Err(Error::Process),
        }
    }
    pub(super) fn close(&mut self) -> Result<(), Error> {
        if self.retained {
            return Err(Error::Cleanup(1460));
        }
        let Some(state) = self.state.as_ref() else { return Ok(()) };
        if state.pending.get() {
            unsafe { CancelIoEx(state.pipe.as_raw_handle(), state.overlapped.get()) };
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let mut count = 0;
                let ok =
                    unsafe { GetOverlappedResult(state.pipe.as_raw_handle(), state.overlapped.get(), &mut count, 0) };
                if ok != 0 || io::Error::last_os_error().raw_os_error() != Some(996) {
                    state.pending.set(false);
                    break;
                }
                if Instant::now() >= deadline {
                    // 只等待一次；保留仍可能被内核访问的对象和额度，报告清理失败。
                    self.retained = true;
                    std::mem::forget(self.state.take().unwrap());
                    return Err(Error::Cleanup(1460));
                }
                unsafe { WaitForSingleObject(state.event.as_raw_handle(), 5) };
            }
        }
        self.state.take();
        Ok(())
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        if self.close().is_err()
            && let Some(state) = self.state.take()
        {
            // CancelIoEx 不代表完成。不能在内核还可能写 OVERLAPPED 时释放或退额度。
            std::mem::forget(state);
        }
    }
}
