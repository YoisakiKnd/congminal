//! congminal：一个从零手写的跨平台终端模拟器。
//!
//! 数据流：
//!
//! ```text
//!  键盘 ──> platform ──Event::Key──> input::encode ──字节──> pty ──> shell
//!                                                                     │
//!  屏幕 <── platform <──像素── render <──网格── term <──动作── vt <──字节┘
//! ```
//!
//! PTY 读线程收到数据后放进共享缓冲区，再用 `Waker` 叫醒主线程的事件循环。
//! 配置文件由另一个线程盯着，改动后同样通过 `Waker` 通知主线程重新加载。

// 发布版不要弹出额外的控制台窗口
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod config;
mod font;
mod input;
mod platform;
mod pty;
mod render;
mod term;
mod vt;

use config::Config;
use platform::{App, Event, Window};
use pty::{Pty, PtyEvent};
use render::{Framebuffer, Renderer};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use term::Terminal;
use vt::Parser;

/// 其他线程交给主线程的东西
#[derive(Default)]
struct Inbox {
    data: Vec<u8>,
    exited: bool,
    config_changed: bool,
}

struct Congminal {
    term: Terminal,
    parser: Parser,
    pty: Pty,
    renderer: Renderer,
    framebuffer: Framebuffer,
    inbox: Arc<Mutex<Inbox>>,
    /// 已经发出唤醒但主线程还没处理。用来合并唤醒，避免 shell 狂刷输出时塞爆事件队列。
    wake_pending: Arc<AtomicBool>,
    /// 输入法正在输入、还没上屏的文字
    preedit: String,
    config: Config,
    config_path: Option<PathBuf>,
    scale: f64,
    dirty: bool,
}

impl Congminal {
    fn write_to_pty(&mut self, bytes: &[u8]) {
        if let Err(e) = self.pty.write(bytes) {
            eprintln!("写入 PTY 失败：{e}");
        }
    }

    /// 配置里的问题用黄色文字显示在终端里（GUI 程序的 stderr 用户通常看不到）
    fn show_warnings(&mut self, warnings: &[String]) {
        for warning in warnings {
            eprintln!("congminal: {warning}");
            let line = format!("\x1b[33m[congminal] {warning}\x1b[0m\r\n");
            self.parser.advance(&mut self.term, line.as_bytes());
        }
        self.dirty = true;
    }

    fn reload_config(&mut self) {
        let (config, warnings) = Config::load(self.config_path.as_deref());
        let warnings: Vec<String> = warnings.iter().map(|w| format!("配置文件：{w}")).collect();
        self.show_warnings(&warnings);

        if config.font != self.config.font || config.colors != self.config.colors || config.window != self.config.window
        {
            self.renderer = Renderer::new(&config, self.scale);
            let warnings = self.renderer.warnings().to_vec();
            self.show_warnings(&warnings);
            // 字体变了格子大小就变了，按现在的窗口尺寸重新算行列数
            let (width, height) = (self.framebuffer.width as u32, self.framebuffer.height as u32);
            self.resize(width, height);
        }
        apply_runtime_settings(&config, &mut self.term);
        self.config = config;
        self.dirty = true;
        term::trace(format_args!("已重新加载配置"));
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.framebuffer.resize(width as usize, height as usize);
        let (cols, rows) = self.renderer.grid_size(width as usize, height as usize);
        if (cols, rows) != (self.term.cols(), self.term.rows()) {
            self.term.resize(cols, rows);
            if let Err(e) = self.pty.resize(cols as u16, rows as u16) {
                eprintln!("调整 PTY 尺寸失败：{e}");
            }
        }
        self.dirty = true;
    }
}

impl App for Congminal {
    fn event(&mut self, event: Event) -> bool {
        match event {
            Event::Wake => {
                // 先清标志再取数据：取数据之后到来的新数据一定会再触发一次唤醒
                self.wake_pending.store(false, Ordering::SeqCst);
                let (data, exited, config_changed) = {
                    let mut inbox = self.inbox.lock().unwrap();
                    (std::mem::take(&mut inbox.data), inbox.exited, std::mem::take(&mut inbox.config_changed))
                };
                if !data.is_empty() {
                    self.parser.advance(&mut self.term, &data);
                    let response = self.term.take_response();
                    if !response.is_empty() {
                        self.write_to_pty(&response);
                    }
                    self.dirty = true;
                }
                if config_changed {
                    self.reload_config();
                }
                if exited {
                    return false;
                }
            }
            Event::Key { key, mods } => {
                let bytes = input::encode(key, mods, self.term.modes().app_cursor);
                if !bytes.is_empty() {
                    self.write_to_pty(&bytes);
                }
            }
            Event::Text(text) => {
                self.preedit.clear();
                self.dirty = true;
                self.write_to_pty(text.as_bytes());
            }
            Event::Preedit(text) => {
                self.preedit = text;
                self.dirty = true;
            }
            Event::Resize { width, height } => self.resize(width, height),
            Event::Redraw => self.dirty = true,
            Event::CloseRequested => return false,
        }
        true
    }

    fn needs_redraw(&self) -> bool {
        self.dirty
    }

    fn draw(&mut self) -> &Framebuffer {
        self.renderer.draw(&self.term, &self.preedit, &mut self.framebuffer);
        self.dirty = false;
        &self.framebuffer
    }

    fn take_title(&mut self) -> Option<String> {
        self.term.take_title()
    }

    fn ime_cursor_area(&self) -> (u32, u32, u32, u32) {
        self.renderer.cursor_rect(&self.term)
    }
}

/// 不需要重建渲染器、可以直接改的设置
fn apply_runtime_settings(config: &Config, term: &mut Terminal) {
    platform::set_option_as_alt(config.option_as_alt);
    term.set_default_cursor_shape(config.cursor_shape);
}

/// CONGMINAL_SCALE 可以覆盖系统报告的缩放比例
fn scale_override() -> Option<f64> {
    std::env::var("CONGMINAL_SCALE").ok()?.parse().ok()
}

/// 每秒看一眼配置文件的修改时间，变了就通知主线程
fn watch_config(path: PathBuf, notify: impl Fn() + Send + 'static) {
    let modified = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let spawned = std::thread::Builder::new().name("config-watcher".into()).spawn(move || {
        let mut last = modified(&path);
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let now = modified(&path);
            if now != last {
                last = now;
                notify();
            }
        }
    });
    if let Err(e) = spawned {
        eprintln!("congminal: 无法监视配置文件：{e}");
    }
}

fn run(config_path: Option<PathBuf>) -> Result<(), String> {
    let (config, config_warnings) = Config::load(config_path.as_deref());
    let (cols, rows) = (config.window.columns, config.window.rows);

    // 加载字体要扫描系统字体目录，只做一次；除非窗口实际所在屏幕的缩放比例和预估的不一样
    let mut initial: Option<(f64, Renderer)> = None;
    let window = Window::new("congminal", |scale| {
        let scale = scale_override().unwrap_or(scale);
        let renderer = Renderer::new(&config, scale);
        let size = renderer.pixel_size(cols, rows);
        initial = Some((scale, renderer));
        size
    })?;
    let scale = scale_override().unwrap_or(window.scale_factor());
    let renderer = match initial {
        Some((initial_scale, renderer)) if initial_scale == scale => renderer,
        _ => Renderer::new(&config, scale),
    };

    let inbox = Arc::new(Mutex::new(Inbox::default()));
    let wake_pending = Arc::new(AtomicBool::new(false));
    // 其他线程往 inbox 里放东西之后调用它叫醒主线程
    let notify = {
        let wake_pending = wake_pending.clone();
        let waker = window.waker();
        Arc::new(move || {
            if !wake_pending.swap(true, Ordering::SeqCst) {
                waker.wake();
            }
        })
    };
    let sink = {
        let inbox = inbox.clone();
        let notify = notify.clone();
        Arc::new(move |event: PtyEvent| {
            {
                let mut inbox = inbox.lock().unwrap();
                match event {
                    PtyEvent::Data(data) => inbox.data.extend_from_slice(&data),
                    PtyEvent::Exit => inbox.exited = true,
                }
            }
            notify();
        })
    };
    let command = pty::Command {
        program: config.shell.program.clone(),
        args: config.shell.args.clone(),
        working_directory: config.shell.working_directory.clone(),
        env: config.env.clone(),
    };
    let pty = Pty::spawn(&command, cols as u16, rows as u16, sink).map_err(|e| format!("启动 shell 失败：{e}"))?;

    if let Some(path) = config_path.clone() {
        let inbox = inbox.clone();
        watch_config(path, move || {
            inbox.lock().unwrap().config_changed = true;
            notify();
        });
    }

    let mut term = Terminal::new(cols, rows);
    apply_runtime_settings(&config, &mut term);
    let mut app = Congminal {
        term,
        parser: Parser::new(),
        pty,
        renderer,
        framebuffer: Framebuffer::new(0, 0),
        inbox,
        wake_pending,
        preedit: String::new(),
        config,
        config_path,
        scale,
        dirty: true,
    };
    let mut warnings: Vec<String> = config_warnings.iter().map(|w| format!("配置文件：{w}")).collect();
    warnings.extend(app.renderer.warnings().iter().cloned());
    app.show_warnings(&warnings);

    window.run(&mut app);
    Ok(())
}

const HELP: &str = "congminal：从零手写的终端模拟器

用法：congminal [选项]

选项：
  --config <路径>    使用指定的配置文件
  --print-config     打印带注释的默认配置（可以保存成配置文件再修改）
  -h, --help         显示这份帮助
  -V, --version      显示版本号

配置文件默认位置：";

fn main() {
    let mut config_path = Config::default_path();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--print-config" => {
                print!("{}", config::EXAMPLE);
                return;
            }
            "-h" | "--help" => {
                let default = Config::default_path().map(|p| p.display().to_string()).unwrap_or_default();
                println!("{HELP}{default}");
                return;
            }
            "-V" | "--version" => {
                println!("congminal {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--config" => match args.next() {
                Some(path) => config_path = Some(PathBuf::from(path)),
                None => {
                    eprintln!("congminal: --config 后面要跟配置文件的路径");
                    std::process::exit(2);
                }
            },
            _ if arg.starts_with("--config=") => config_path = Some(PathBuf::from(&arg["--config=".len()..])),
            // 从 Finder 启动时 macOS 可能会加上这个参数
            _ if arg.starts_with("-psn_") => {}
            _ => {
                eprintln!("congminal: 不认识的参数“{arg}”，用 --help 查看用法");
                std::process::exit(2);
            }
        }
    }

    if let Err(e) = run(config_path) {
        eprintln!("congminal: {e}");
        std::process::exit(1);
    }
}
