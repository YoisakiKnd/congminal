//! Windows 伪终端：ConPTY。
//!
//! 流程：
//! 1. 建两根匿名管道：一根写给 shell（输入），一根从 shell 读（输出）
//! 2. `CreatePseudoConsole` 用这两根管道创建伪控制台
//! 3. 通过 `STARTUPINFOEXW` 的属性列表把伪控制台交给 `CreateProcessW`
//!
//! ConPTY 会把 Windows 控制台 API 的调用翻译成 VT 序列，所以我们的解析器可以原样复用。

use super::{EventSink, PtyEvent};
use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::FromRawHandle;
use std::ptr::{null, null_mut};

type Handle = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct Coord {
    x: i16,
    y: i16,
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

const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;
const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
const INFINITE: u32 = 0xFFFF_FFFF;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreatePipe(read: *mut Handle, write: *mut Handle, attributes: *const c_void, size: u32) -> i32;
    fn CreatePseudoConsole(size: Coord, input: Handle, output: Handle, flags: u32, console: *mut Handle) -> i32;
    fn ResizePseudoConsole(console: Handle, size: Coord) -> i32;
    fn ClosePseudoConsole(console: Handle);
    fn InitializeProcThreadAttributeList(list: *mut c_void, count: u32, flags: u32, size: *mut usize) -> i32;
    fn UpdateProcThreadAttribute(
        list: *mut c_void,
        flags: u32,
        attribute: usize,
        value: *const c_void,
        size: usize,
        previous: *mut c_void,
        return_size: *mut usize,
    ) -> i32;
    fn DeleteProcThreadAttributeList(list: *mut c_void);
    fn CreateProcessW(
        application: *const u16,
        command_line: *mut u16,
        process_attributes: *const c_void,
        thread_attributes: *const c_void,
        inherit_handles: i32,
        flags: u32,
        environment: *const c_void,
        current_directory: *const u16,
        startup_info: *const StartupInfoW,
        process_information: *mut ProcessInformation,
    ) -> i32;
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    fn CloseHandle(handle: Handle) -> i32;
}

/// 让裸句柄可以移进线程
struct SendHandle(Handle);
unsafe impl Send for SendHandle {}

pub struct Pty {
    console: Handle,
    input: File,
}

impl Pty {
    pub fn spawn(cols: u16, rows: u16, sink: EventSink) -> io::Result<Pty> {
        unsafe {
            // 1. 两根管道
            let (mut input_read, mut input_write) = (null_mut(), null_mut());
            let (mut output_read, mut output_write) = (null_mut(), null_mut());
            if CreatePipe(&mut input_read, &mut input_write, null(), 0) == 0 {
                return Err(io::Error::last_os_error());
            }
            if CreatePipe(&mut output_read, &mut output_write, null(), 0) == 0 {
                let err = io::Error::last_os_error();
                CloseHandle(input_read);
                CloseHandle(input_write);
                return Err(err);
            }

            // 2. 伪控制台
            let mut console = null_mut();
            let hr = CreatePseudoConsole(coord(cols, rows), input_read, output_write, 0, &mut console);
            // ConPTY 已经复制了自己那一端，我们这边必须关掉，否则 shell 退出后读端收不到 EOF
            CloseHandle(input_read);
            CloseHandle(output_write);
            let input = File::from_raw_handle(input_write);
            let output = File::from_raw_handle(output_read);
            if hr < 0 {
                return Err(io::Error::other(format!("CreatePseudoConsole 失败：HRESULT {hr:#010x}")));
            }
            let pty = Pty { console, input };

            // 3. 属性列表：告诉 CreateProcessW 把这个伪控制台交给子进程
            let mut size = 0;
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size);
            let mut list = vec![0u8; size];
            let list_ptr = list.as_mut_ptr().cast::<c_void>();
            if InitializeProcThreadAttributeList(list_ptr, 1, 0, &mut size) == 0 {
                return Err(io::Error::last_os_error());
            }
            if UpdateProcThreadAttribute(
                list_ptr,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                console,
                size_of::<Handle>(),
                null_mut(),
                null_mut(),
            ) == 0
            {
                let err = io::Error::last_os_error();
                DeleteProcThreadAttributeList(list_ptr);
                return Err(err);
            }

            let mut startup: StartupInfoExW = std::mem::zeroed();
            startup.startup_info.cb = size_of::<StartupInfoExW>() as u32;
            // 不设这个标志的话，子进程可能继承我们自己的标准句柄，绕开 ConPTY
            startup.startup_info.flags = STARTF_USESTDHANDLES;
            startup.attribute_list = list_ptr;

            let mut command: Vec<u16> = find_shell().encode_utf16().chain(Some(0)).collect();
            let mut info: ProcessInformation = std::mem::zeroed();
            let ok = CreateProcessW(
                null(),
                command.as_mut_ptr(),
                null(),
                null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT,
                null(),
                null(),
                &startup.startup_info,
                &mut info,
            );
            DeleteProcThreadAttributeList(list_ptr);
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            CloseHandle(info.thread);

            // 4. 读线程：把 shell 输出送出去
            let data_sink = sink.clone();
            let mut output = output;
            std::thread::Builder::new().name("pty-reader".into()).spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                while let Ok(n @ 1..) = output.read(&mut buf) {
                    data_sink(PtyEvent::Data(buf[..n].to_vec()));
                }
            })?;

            // 5. 等待线程：shell 退出时通知。ConPTY 下读端不一定会马上收到 EOF，所以单独等进程句柄。
            let process = SendHandle(info.process);
            std::thread::Builder::new().name("pty-waiter".into()).spawn(move || {
                let process = process;
                WaitForSingleObject(process.0, INFINITE);
                CloseHandle(process.0);
                sink(PtyEvent::Exit);
            })?;

            Ok(pty)
        }
    }

    pub fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.input.write_all(data)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        let hr = unsafe { ResizePseudoConsole(self.console, coord(cols, rows)) };
        if hr < 0 {
            return Err(io::Error::other(format!("ResizePseudoConsole 失败：HRESULT {hr:#010x}")));
        }
        Ok(())
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // 关闭伪控制台会结束挂在上面的进程
        unsafe { ClosePseudoConsole(self.console) };
    }
}

fn coord(cols: u16, rows: u16) -> Coord {
    Coord { x: cols as i16, y: rows as i16 }
}

/// 优先用 CONGMINAL_SHELL，其次 COMSPEC（通常是 cmd.exe）
fn find_shell() -> String {
    std::env::var("CONGMINAL_SHELL").or_else(|_| std::env::var("COMSPEC")).unwrap_or_else(|_| "cmd.exe".into())
}
