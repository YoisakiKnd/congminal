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

// 发布版不要弹出额外的控制台窗口
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod font;
mod input;
mod platform;
mod pty;
mod render;
mod term;
mod vt;

use platform::{App, Event, Window};
use pty::{Pty, PtyEvent};
use render::{Framebuffer, Renderer};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use term::Terminal;
use vt::Parser;

const INITIAL_COLS: usize = 80;
const INITIAL_ROWS: usize = 24;

/// PTY 读线程和主线程之间共享的数据
#[derive(Default)]
struct Inbox {
    data: Vec<u8>,
    exited: bool,
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
    dirty: bool,
}

impl Congminal {
    fn write_to_pty(&mut self, bytes: &[u8]) {
        if let Err(e) = self.pty.write(bytes) {
            eprintln!("写入 PTY 失败：{e}");
        }
    }
}

impl App for Congminal {
    fn event(&mut self, event: Event) -> bool {
        match event {
            Event::Wake => {
                // 先清标志再取数据：取数据之后到来的新数据一定会再触发一次唤醒
                self.wake_pending.store(false, Ordering::SeqCst);
                let (data, exited) = {
                    let mut inbox = self.inbox.lock().unwrap();
                    (std::mem::take(&mut inbox.data), inbox.exited)
                };
                if !data.is_empty() {
                    self.parser.advance(&mut self.term, &data);
                    let response = self.term.take_response();
                    if !response.is_empty() {
                        self.write_to_pty(&response);
                    }
                    self.dirty = true;
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
            Event::Resize { width, height } => {
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

/// CONGMINAL_SCALE 可以覆盖系统报告的缩放比例
fn scale_override() -> Option<f64> {
    std::env::var("CONGMINAL_SCALE").ok()?.parse().ok()
}

fn run() -> Result<(), String> {
    // 加载字体要扫描系统字体目录，只做一次；除非窗口实际所在屏幕的缩放比例和预估的不一样
    let mut initial: Option<(f64, Renderer)> = None;
    let window = Window::new("congminal", |scale| {
        let scale = scale_override().unwrap_or(scale);
        let renderer = Renderer::new(scale);
        let size = renderer.pixel_size(INITIAL_COLS, INITIAL_ROWS);
        initial = Some((scale, renderer));
        size
    })?;
    let scale = scale_override().unwrap_or(window.scale_factor());
    let renderer = match initial {
        Some((initial_scale, renderer)) if initial_scale == scale => renderer,
        _ => Renderer::new(scale),
    };

    let inbox = Arc::new(Mutex::new(Inbox::default()));
    let wake_pending = Arc::new(AtomicBool::new(false));
    let sink = {
        let inbox = inbox.clone();
        let wake_pending = wake_pending.clone();
        let waker = window.waker();
        Arc::new(move |event: PtyEvent| {
            {
                let mut inbox = inbox.lock().unwrap();
                match event {
                    PtyEvent::Data(data) => inbox.data.extend_from_slice(&data),
                    PtyEvent::Exit => inbox.exited = true,
                }
            }
            if !wake_pending.swap(true, Ordering::SeqCst) {
                waker.wake();
            }
        })
    };
    let pty =
        Pty::spawn(INITIAL_COLS as u16, INITIAL_ROWS as u16, sink).map_err(|e| format!("启动 shell 失败：{e}"))?;

    let mut app = Congminal {
        term: Terminal::new(INITIAL_COLS, INITIAL_ROWS),
        parser: Parser::new(),
        pty,
        renderer,
        framebuffer: Framebuffer::new(0, 0),
        inbox,
        wake_pending,
        preedit: String::new(),
        dirty: true,
    };
    window.run(&mut app);
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("congminal: {e}");
        std::process::exit(1);
    }
}
