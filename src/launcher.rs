//! 显式用户动作的桌面 URL 适配。调用在有界工作线程中，参数不经过 shell。
use crate::tasks::Cancellation;

pub fn open(url: crate::links::Url, cancel: &Cancellation) -> Result<(), String> {
    if !crate::links::valid_url(&url) {
        return Err("只支持不超过 2 KiB 的 HTTP/HTTPS 链接".into());
    }
    if cancel.is_cancelled() {
        return Ok(());
    }
    open_platform(&url, cancel)
}
#[cfg(unix)]
fn open_platform(url: &str, cancel: &Cancellation) -> Result<(), String> {
    // gio 将请求交给桌面应用，不把浏览器本身作为等待/超时终止的子进程。
    let mut command = if cfg!(target_os = "macos") {
        std::process::Command::new("/usr/bin/open")
    } else {
        let mut command = std::process::Command::new("gio");
        command.arg("open");
        command
    };
    command.arg(url);
    run(&mut command, cancel)
}
#[cfg(unix)]
fn run(command: &mut std::process::Command, cancel: &Cancellation) -> Result<(), String> {
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("无法调用桌面链接程序：{e}"))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() { Ok(()) } else { Err(format!("桌面链接程序退出：{status}")) };
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
            Ok(None) => {}
        }
        if cancel.is_cancelled() || start.elapsed() >= Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("桌面链接调用已取消或超时".into());
        }
        cancel.wait(Duration::from_millis(25));
    }
}
#[cfg(windows)]
fn open_platform(url: &str, _cancel: &Cancellation) -> Result<(), String> {
    use std::{ptr::null, sync::OnceLock};
    type Open = unsafe extern "system" fn(isize, *const u16, *const u16, *const u16, *const u16, i32) -> isize;
    struct Api {
        _module: isize,
        open: Open,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryExW(name: *const u16, file: isize, flags: u32) -> isize;
        fn GetProcAddress(module: isize, name: *const u8) -> *mut std::ffi::c_void;
        fn FreeLibrary(module: isize) -> i32;
    }
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    let api = API
        .get_or_init(|| {
            let name: Vec<_> = "shell32.dll\0".encode_utf16().collect();
            // 仅从系统目录加载；首次显式打开才加载并持有一份，避免增加默认启动依赖。
            let module = unsafe { LoadLibraryExW(name.as_ptr(), 0, 0x800) };
            if module == 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let address = unsafe { GetProcAddress(module, c"ShellExecuteW".as_ptr().cast()) };
            if address.is_null() {
                unsafe {
                    FreeLibrary(module);
                }
                return Err("系统缺少 ShellExecuteW".into());
            }
            let open = unsafe { std::mem::transmute::<*mut std::ffi::c_void, Open>(address) };
            Ok(Api { _module: module, open })
        })
        .as_ref()
        .map_err(Clone::clone)?;
    let operation: Vec<u16> = "open\0".encode_utf16().collect();
    let url: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
    let result = unsafe { (api.open)(0, operation.as_ptr(), url.as_ptr(), null(), null(), 1) };
    if result > 32 { Ok(()) } else { Err(format!("系统无法打开链接（{result}）")) }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn launcher_success_failure_and_missing_program_are_reported() {
        let cancel = Cancellation::default();
        assert!(run(std::process::Command::new("/bin/sh").args(["-c", "exit 0"]), &cancel).is_ok());
        assert!(run(std::process::Command::new("/bin/sh").args(["-c", "exit 7"]), &cancel).is_err());
        assert!(run(&mut std::process::Command::new("/congminal-nonexistent-desktop-opener"), &cancel).is_err());
    }

    #[test]
    fn cancellation_reaps_the_launcher_without_opening_any_browser() {
        let cancel = Cancellation::default();
        let stop = cancel.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            send.send(run(std::process::Command::new("/bin/sh").args(["-c", "exec sleep 30"]), &stop)).unwrap();
        });
        std::thread::sleep(std::time::Duration::from_millis(30));
        cancel.cancel();
        assert!(receive.recv_timeout(std::time::Duration::from_secs(2)).unwrap().is_err());
        thread.join().unwrap();
        assert!(open("file:///tmp/x".into(), &Cancellation::default()).is_err());
    }
}
