//! 独立目录候选夹具；不链接窗口/字体/SSH 认证模块。尚非默认打包产物。
#[allow(dead_code)]
#[path = "../local_directory.rs"]
mod local_directory;
#[allow(dead_code)]
#[path = "../ssh/helper_resources.rs"]
mod resources;
#[allow(dead_code)]
#[path = "../cancellation.rs"]
mod tasks;

fn main() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(local_directory::isolated::FLAG)) {
        std::process::exit(64);
    }
    std::process::exit(local_directory::isolated::child_mode(args));
}
