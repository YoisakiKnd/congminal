//! Windows 伪终端：ConPTY。
//!
//! 流程：
//! 1. 建两根匿名管道：一根写给 shell（输入），一根从 shell 读（输出）
//! 2. `CreatePseudoConsole` 用这两根管道创建伪控制台
//! 3. 通过 `STARTUPINFOEXW` 的属性列表把伪控制台交给 `CreateProcessW`
//!
//! ConPTY 会把 Windows 控制台 API 的调用翻译成 VT 序列，所以我们的解析器可以原样复用。

#[cfg(test)]
use super::Command;
use super::flow::{AsyncWriter, ByteQueue, INPUT_LIMIT, READ_CHUNK};
use super::{CommandSource, EventSink, PtyEvent};
use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Read};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};

#[path = "windows_shell.rs"]
mod shell;
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
const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
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
    fn GetExitCodeProcess(handle: Handle, code: *mut u32) -> i32;
}

/// 让裸句柄可以移进线程
struct SendHandle(Handle);
unsafe impl Send for SendHandle {}

struct Console(Mutex<Option<SendHandle>>);
impl Console {
    fn close(&self) {
        // 只取一次句柄，不持锁等待 ConPTY 排空。读线程必须在关闭期间继续读。
        let console = self.0.lock().unwrap().take();
        if let Some(console) = console {
            unsafe {
                ClosePseudoConsole(console.0);
            }
        }
    }
}

pub struct Pty {
    console: Arc<Console>,
    input: Option<AsyncWriter>,
}

impl Pty {
    pub fn spawn(command: &impl CommandSource, cols: u16, rows: u16, sink: EventSink) -> io::Result<Pty> {
        let command = command.view();
        let pool = crate::resources::current();
        let reader_permit = crate::runtime::prepare("pty-reader", &pool)?;
        let waiter_permit = crate::runtime::prepare("pty-waiter", &pool)?;
        let writer_permit = crate::runtime::prepare("pty-writer", &pool)?;
        let input_queue = Arc::new(ByteQueue::input(INPUT_LIMIT)?);
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
            let mut pty = Pty { console: Arc::new(Console(Mutex::new(Some(SendHandle(console))))), input: None };

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

            let mut command_line: Vec<u16> = command_line(&command).encode_utf16().chain(Some(0)).collect();
            // 有额外的环境变量时自己拼一个环境块，否则继承当前进程的
            let environment = Some(environment_block(command.env));
            let directory: Option<Vec<u16>> =
                command.working_directory.as_ref().map(|dir| dir.as_os_str().encode_wide().chain(Some(0)).collect());
            let mut info: ProcessInformation = std::mem::zeroed();
            let ok = CreateProcessW(
                null(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                environment.as_ref().map_or(null(), |block| block.as_ptr().cast()),
                directory.as_ref().map_or(null(), |dir| dir.as_ptr()),
                &startup.startup_info,
                &mut info,
            );
            DeleteProcThreadAttributeList(list_ptr);
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            CloseHandle(info.thread);
            let process = OwnedHandle::from_raw_handle(info.process);

            // 4. 读线程：把 shell 输出送出去
            let data_sink = sink.clone();
            let mut output = output;
            reader_permit.spawn(move || {
                let mut buf = vec![0u8; READ_CHUNK];
                let mut forward = true;
                while let Ok(n @ 1..) = output.read(&mut buf) {
                    if forward {
                        forward = data_sink(PtyEvent::Data(buf[..n].to_vec()));
                    }
                    // UI 关闭后继续排空 ConPTY 的关闭消息，避免 ClosePseudoConsole 等待读端。
                }
                data_sink(PtyEvent::Eof);
            })?;

            // 5. 等待线程：shell 退出时通知。ConPTY 下读端不一定会马上收到 EOF，所以单独等进程句柄。
            let console = pty.console.clone();
            let sink_for_writer = sink.clone();
            waiter_permit.spawn(move || {
                let process = process;
                WaitForSingleObject(process.as_raw_handle(), INFINITE);
                let mut code = 0;
                let code = (GetExitCodeProcess(process.as_raw_handle(), &mut code) != 0).then_some(code as i32);
                // shell 已退出后主动关闭 ConPTY，读线程排完最后输出才会收到 EOF。
                console.close();
                sink(PtyEvent::Exit(code));
            })?;

            // 读端已能排空关闭消息，等待端已拥有进程；最后发布可取消的写端。
            pty.input = Some(AsyncWriter::spawn_reserved(writer_permit, input_queue, input, sink_for_writer, || {
                std::thread::park_timeout(std::time::Duration::from_millis(2))
            })?);

            Ok(pty)
        }
    }

    pub fn pending_bytes(&self) -> usize {
        self.input.as_ref().map_or(0, AsyncWriter::pending_bytes)
    }

    pub fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.input.as_ref().unwrap().write(data)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        let console = self.console.0.lock().unwrap();
        let console = console.as_ref().ok_or(io::ErrorKind::BrokenPipe)?;
        let hr = unsafe { ResizePseudoConsole(console.0, coord(cols, rows)) };
        if hr < 0 {
            return Err(io::Error::other(format!("ResizePseudoConsole 失败：HRESULT {hr:#010x}")));
        }
        Ok(())
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // 关闭伪控制台会结束挂在上面的进程
        drop(self.input.take());
        self.console.close();
    }
}

fn coord(cols: u16, rows: u16) -> Coord {
    Coord { x: cols as i16, y: rows as i16 }
}

/// 默认优先 PowerShell 7、Windows PowerShell，最后 COMSPEC；显式配置优先。
///
/// Windows 把整条命令行作为一个字符串交给新进程，由它自己切分参数，
/// 所以含空格或引号的参数要按 C 运行库（CommandLineToArgvW）的规则加引号和转义。
fn command_line(command: &impl CommandSource) -> String {
    let command = command.view();
    let fallback = command.program.is_none().then(shell::default_program);
    let program = command.program.or(fallback.as_deref()).unwrap();
    let mut line = String::new();
    for (i, arg) in std::iter::once(program).chain(command.args.iter().map(String::as_str)).enumerate() {
        if i > 0 {
            line.push(' ');
        }
        quote_arg(arg, &mut line);
    }
    if command.program.is_none()
        && command.args.is_empty()
        && (program.to_ascii_lowercase().ends_with("pwsh.exe")
            || program.to_ascii_lowercase().ends_with("powershell.exe"))
    {
        line.push_str(" -NoLogo");
    }
    line
}

fn quote_arg(arg: &str, out: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\u{b}', '"']) {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            // 引号前面的反斜杠要加倍，引号本身再加一个反斜杠
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    // 结尾的反斜杠后面紧跟着收尾的引号，也要加倍
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
}

/// 环境块：一串 "名字=值\0"，最后再加一个 \0，按名字排序（不区分大小写）
fn environment_block(extra: &[(String, String)]) -> Vec<u16> {
    let mut vars: Vec<(String, String)> = std::env::vars_os()
        .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
        .filter(|(k, _)| !extra.iter().any(|(e, _)| e.eq_ignore_ascii_case(k)))
        .collect();
    for (key, value) in [("TERM", "xterm-256color"), ("COLORTERM", "truecolor"), ("TERM_PROGRAM", "congminal")] {
        if !extra.iter().any(|(k, _)| k.eq_ignore_ascii_case(key)) {
            vars.retain(|(k, _)| !k.eq_ignore_ascii_case(key));
            vars.push((key.into(), value.into()));
        }
    }
    vars.extend(extra.iter().cloned());
    vars.sort_by_key(|(k, _)| k.to_uppercase());
    let mut block = Vec::new();
    for (key, value) in vars {
        block.extend(format!("{key}={value}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(program: &str, args: &[&str]) -> String {
        command_line(&Command {
            program: Some(program.into()),
            args: args.iter().map(|s| s.to_string()).collect(),
            ..Command::default()
        })
    }

    #[test]
    fn quoting() {
        assert_eq!(line("pwsh", &["-NoProfile"]), "pwsh -NoProfile");
        assert_eq!(line(r"C:\Program Files\pwsh.exe", &[]), r#""C:\Program Files\pwsh.exe""#);
        assert_eq!(line("cmd", &[r#"say "hi""#]), r#"cmd "say \"hi\"""#);
        assert_eq!(line("x", &[r"C:\dir with space\"]), r#"x "C:\dir with space\\""#);
        assert_eq!(line("x", &[r"C:\no\space"]), r"x C:\no\space");
        assert_eq!(line("x", &[""]), r#"x """#);
    }
}
