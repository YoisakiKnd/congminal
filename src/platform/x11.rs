//! Linux 平台：Xlib。
//!
//! 在 Wayland 桌面上会通过 XWayland 运行。以后可以加一个原生 Wayland 后端，
//! 或者干脆绕开 Xlib，直接通过 Unix socket 说 X11 协议。
//!
//! 事件循环：用 `poll` 同时等待 X 服务器的连接和一个唤醒用的 socket。

use super::{App, Event};
use crate::input::{Key, Mods};
use crate::render::Framebuffer;
use std::ffi::{CString, c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::ptr::{null, null_mut};
use std::sync::Arc;

type Display = c_void;
type XWindow = c_ulong;
type Atom = c_ulong;
type KeySym = c_ulong;
type Xim = *mut c_void;
type Xic = *mut c_void;
type Gc = *mut c_void;
type Visual = c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct XKeyEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut Display,
    window: XWindow,
    root: XWindow,
    subwindow: XWindow,
    time: c_ulong,
    x: c_int,
    y: c_int,
    x_root: c_int,
    y_root: c_int,
    state: c_uint,
    keycode: c_uint,
    same_screen: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XConfigureEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut Display,
    event: XWindow,
    window: XWindow,
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
    border_width: c_int,
    above: XWindow,
    override_redirect: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XExposeEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut Display,
    window: XWindow,
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
    count: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XClientMessageEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut Display,
    window: XWindow,
    message_type: Atom,
    format: c_int,
    data: [c_long; 5],
}

/// XEvent 是一个联合体，C 里定义为 24 个 long 那么大
#[repr(C)]
union XEvent {
    type_: c_int,
    key: XKeyEvent,
    configure: XConfigureEvent,
    expose: XExposeEvent,
    client: XClientMessageEvent,
    pad: [c_long; 24],
}

const KEY_PRESS: c_int = 2;
const FOCUS_IN: c_int = 9;
const FOCUS_OUT: c_int = 10;
const EXPOSE: c_int = 12;
const CONFIGURE_NOTIFY: c_int = 22;
const CLIENT_MESSAGE: c_int = 33;

const KEY_PRESS_MASK: c_long = 1 << 0;
const EXPOSURE_MASK: c_long = 1 << 15;
const STRUCTURE_NOTIFY_MASK: c_long = 1 << 17;
const FOCUS_CHANGE_MASK: c_long = 1 << 21;

const SHIFT_MASK: c_uint = 1 << 0;
const CONTROL_MASK: c_uint = 1 << 2;
const MOD1_MASK: c_uint = 1 << 3; // 通常是 Alt
const MOD4_MASK: c_uint = 1 << 6; // 通常是 Super

const Z_PIXMAP: c_int = 2;
const XIM_PREEDIT_NOTHING: c_ulong = 0x0008;
const XIM_STATUS_NOTHING: c_ulong = 0x0400;
const LC_ALL: c_int = 6;

#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut Display;
    fn XCloseDisplay(display: *mut Display) -> c_int;
    fn XDefaultScreen(display: *mut Display) -> c_int;
    fn XRootWindow(display: *mut Display, screen: c_int) -> XWindow;
    fn XDefaultVisual(display: *mut Display, screen: c_int) -> *mut Visual;
    fn XDefaultDepth(display: *mut Display, screen: c_int) -> c_int;
    fn XDefaultGC(display: *mut Display, screen: c_int) -> Gc;
    fn XBlackPixel(display: *mut Display, screen: c_int) -> c_ulong;
    fn XCreateSimpleWindow(
        display: *mut Display,
        parent: XWindow,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
        border_width: c_uint,
        border: c_ulong,
        background: c_ulong,
    ) -> XWindow;
    fn XSelectInput(display: *mut Display, window: XWindow, mask: c_long) -> c_int;
    fn XStoreName(display: *mut Display, window: XWindow, name: *const c_char) -> c_int;
    fn XMapWindow(display: *mut Display, window: XWindow) -> c_int;
    fn XInternAtom(display: *mut Display, name: *const c_char, only_if_exists: c_int) -> Atom;
    fn XSetWMProtocols(display: *mut Display, window: XWindow, protocols: *mut Atom, count: c_int) -> c_int;
    fn XPending(display: *mut Display) -> c_int;
    fn XNextEvent(display: *mut Display, event: *mut XEvent) -> c_int;
    fn XConnectionNumber(display: *mut Display) -> c_int;
    fn XFlush(display: *mut Display) -> c_int;
    fn XCreateImage(
        display: *mut Display,
        visual: *mut Visual,
        depth: c_uint,
        format: c_int,
        offset: c_int,
        data: *mut c_char,
        width: c_uint,
        height: c_uint,
        bitmap_pad: c_int,
        bytes_per_line: c_int,
    ) -> *mut c_void;
    fn XPutImage(
        display: *mut Display,
        drawable: XWindow,
        gc: Gc,
        image: *mut c_void,
        src_x: c_int,
        src_y: c_int,
        dest_x: c_int,
        dest_y: c_int,
        width: c_uint,
        height: c_uint,
    ) -> c_int;
    fn XFree(data: *mut c_void) -> c_int;
    fn XSetLocaleModifiers(modifiers: *const c_char) -> *mut c_char;
    fn XOpenIM(display: *mut Display, db: *mut c_void, name: *mut c_char, class: *mut c_char) -> Xim;
    fn XCloseIM(im: Xim) -> c_int;
    fn XCreateIC(im: Xim, ...) -> Xic;
    fn XDestroyIC(ic: Xic);
    fn XSetICFocus(ic: Xic);
    fn XUnsetICFocus(ic: Xic);
    fn XFilterEvent(event: *mut XEvent, window: XWindow) -> c_int;
    fn Xutf8LookupString(
        ic: Xic,
        event: *mut XKeyEvent,
        buffer: *mut c_char,
        len: c_int,
        keysym: *mut KeySym,
        status: *mut c_int,
    ) -> c_int;
    fn XLookupString(
        event: *mut XKeyEvent,
        buffer: *mut c_char,
        len: c_int,
        keysym: *mut KeySym,
        compose: *mut c_void,
    ) -> c_int;
}

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

const POLLIN: i16 = 1;

unsafe extern "C" {
    fn setlocale(category: c_int, locale: *const c_char) -> *mut c_char;
    fn poll(fds: *mut PollFd, count: c_ulong, timeout: c_int) -> c_int;
}

pub struct Window {
    display: *mut Display,
    window: XWindow,
    screen: c_int,
    wm_delete: Atom,
    im: Xim,
    ic: Xic,
    width: u32,
    height: u32,
    wake_read: UnixStream,
    wake_write: Arc<UnixStream>,
}

/// 跨线程唤醒：往 socket 里写一个字节，事件循环的 poll 就会返回
#[derive(Clone)]
pub struct Waker(Arc<UnixStream>);

impl Waker {
    pub fn wake(&self) {
        // socket 满了说明已经有未处理的唤醒，丢掉也没关系
        let _ = (&*self.0).write(&[1]);
    }
}

impl Window {
    pub fn new(title: &str, size_for_scale: impl FnOnce(f64) -> (u32, u32)) -> Result<Window, String> {
        let scale = Self::scale_from_env();
        let (width, height) = size_for_scale(scale);
        unsafe {
            // 让 Xlib 按系统 locale 处理输入法和 UTF-8
            setlocale(LC_ALL, c"".as_ptr());
            XSetLocaleModifiers(c"".as_ptr());

            let display = XOpenDisplay(null());
            if display.is_null() {
                return Err("无法连接 X 服务器（检查 DISPLAY 环境变量）".into());
            }
            let screen = XDefaultScreen(display);
            let depth = XDefaultDepth(display, screen);
            if depth != 24 && depth != 32 {
                XCloseDisplay(display);
                return Err(format!("不支持 {depth} 位色深的显示器"));
            }

            let window = XCreateSimpleWindow(
                display,
                XRootWindow(display, screen),
                0,
                0,
                width,
                height,
                0,
                0,
                XBlackPixel(display, screen),
            );
            XSelectInput(display, window, KEY_PRESS_MASK | EXPOSURE_MASK | STRUCTURE_NOTIFY_MASK | FOCUS_CHANGE_MASK);
            let title = CString::new(title).unwrap_or_default();
            XStoreName(display, window, title.as_ptr());

            // 窗口管理器点关闭按钮时，发消息给我们而不是直接断开连接
            let mut wm_delete = XInternAtom(display, c"WM_DELETE_WINDOW".as_ptr(), 0);
            XSetWMProtocols(display, window, &mut wm_delete, 1);

            // 输入法上下文：Xutf8LookupString 需要它才能输出 UTF-8，也顺便支持了系统输入法
            let im = XOpenIM(display, null_mut(), null_mut(), null_mut());
            let ic = if im.is_null() {
                null_mut()
            } else {
                XCreateIC(
                    im,
                    c"inputStyle".as_ptr(),
                    XIM_PREEDIT_NOTHING | XIM_STATUS_NOTHING,
                    c"clientWindow".as_ptr(),
                    window,
                    c"focusWindow".as_ptr(),
                    window,
                    null::<c_char>(),
                )
            };

            XMapWindow(display, window);
            XFlush(display);

            let (wake_read, wake_write) = UnixStream::pair().map_err(|e| e.to_string())?;
            wake_read.set_nonblocking(true).map_err(|e| e.to_string())?;
            wake_write.set_nonblocking(true).map_err(|e| e.to_string())?;

            Ok(Window {
                display,
                window,
                screen,
                wm_delete,
                im,
                ic,
                width,
                height,
                wake_read,
                wake_write: Arc::new(wake_write),
            })
        }
    }

    /// X11 没有统一的缩放设置，先用环境变量 CONGMINAL_SCALE 指定
    fn scale_from_env() -> f64 {
        std::env::var("CONGMINAL_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0)
    }

    pub fn scale_factor(&self) -> f64 {
        Self::scale_from_env()
    }

    pub fn waker(&self) -> Waker {
        Waker(self.wake_write.clone())
    }

    pub fn run(mut self, app: &mut dyn App) {
        if !app.event(Event::Resize { width: self.width, height: self.height }) {
            return;
        }
        let mut fds = [
            PollFd { fd: unsafe { XConnectionNumber(self.display) }, events: POLLIN, revents: 0 },
            PollFd { fd: self.wake_read.as_raw_fd(), events: POLLIN, revents: 0 },
        ];
        loop {
            // 1. 处理 Xlib 队列里所有的事件
            while unsafe { XPending(self.display) } > 0 {
                let mut event: XEvent = unsafe { std::mem::zeroed() };
                unsafe { XNextEvent(self.display, &mut event) };
                // 输入法可能要拦截这个事件
                if unsafe { XFilterEvent(&mut event, 0) } != 0 {
                    continue;
                }
                for e in self.translate(&mut event) {
                    if !app.event(e) {
                        return;
                    }
                }
            }

            // 2. 其他线程的唤醒
            if self.drain_wakeups() && !app.event(Event::Wake) {
                return;
            }

            // 3. 改标题、重画
            if let Some(title) = app.take_title() {
                let title = CString::new(title).unwrap_or_default();
                unsafe { XStoreName(self.display, self.window, title.as_ptr()) };
            }
            if app.needs_redraw() {
                self.present(app.draw());
            }
            unsafe { XFlush(self.display) };

            // 4. 没事做就睡觉，直到 X 服务器或唤醒 socket 有数据
            if unsafe { XPending(self.display) } == 0 {
                unsafe { poll(fds.as_mut_ptr(), fds.len() as c_ulong, -1) };
            }
        }
    }

    fn drain_wakeups(&mut self) -> bool {
        let mut buf = [0u8; 64];
        let mut woken = false;
        loop {
            match self.wake_read.read(&mut buf) {
                Ok(n) if n > 0 => woken = true,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                _ => return woken,
            }
        }
    }

    fn translate(&mut self, event: &mut XEvent) -> Vec<Event> {
        unsafe {
            match event.type_ {
                KEY_PRESS => self.key_press(&mut event.key),
                CONFIGURE_NOTIFY => {
                    let (w, h) = (event.configure.width as u32, event.configure.height as u32);
                    if (w, h) == (self.width, self.height) {
                        return vec![];
                    }
                    self.width = w;
                    self.height = h;
                    vec![Event::Resize { width: w, height: h }]
                }
                EXPOSE if event.expose.count == 0 => vec![Event::Redraw],
                CLIENT_MESSAGE if event.client.data[0] as Atom == self.wm_delete => vec![Event::CloseRequested],
                FOCUS_IN if !self.ic.is_null() => {
                    XSetICFocus(self.ic);
                    vec![]
                }
                FOCUS_OUT if !self.ic.is_null() => {
                    XUnsetICFocus(self.ic);
                    vec![]
                }
                _ => vec![],
            }
        }
    }

    fn key_press(&mut self, event: &mut XKeyEvent) -> Vec<Event> {
        let mut buf = [0u8; 128];
        let mut keysym: KeySym = 0;
        let text = unsafe {
            if self.ic.is_null() {
                let n = XLookupString(event, buf.as_mut_ptr().cast(), buf.len() as c_int, &mut keysym, null_mut());
                // 没有输入法上下文时，XLookupString 返回的是 Latin-1
                buf[..n.max(0) as usize].iter().map(|&b| b as char).collect()
            } else {
                let mut status = 0;
                let n = Xutf8LookupString(
                    self.ic,
                    event,
                    buf.as_mut_ptr().cast(),
                    buf.len() as c_int,
                    &mut keysym,
                    &mut status,
                );
                String::from_utf8_lossy(&buf[..n.max(0) as usize]).into_owned()
            }
        };

        let state = event.state;
        let mods = Mods {
            shift: state & SHIFT_MASK != 0,
            ctrl: state & CONTROL_MASK != 0,
            alt: state & MOD1_MASK != 0,
            logo: state & MOD4_MASK != 0,
        };
        if let Some(key) = named_key(keysym) {
            return vec![Event::Key { key, mods }];
        }
        text.chars().map(|c| Event::Key { key: Key::Char(c), mods }).collect()
    }

    fn present(&self, fb: &Framebuffer) {
        if fb.width == 0 || fb.height == 0 {
            return;
        }
        unsafe {
            // XImage 只是借用我们的像素数据，用完只释放结构体本身（XFree），不释放数据
            let image = XCreateImage(
                self.display,
                XDefaultVisual(self.display, self.screen),
                24,
                Z_PIXMAP,
                0,
                fb.pixels.as_ptr() as *mut c_char,
                fb.width as c_uint,
                fb.height as c_uint,
                32,
                (fb.width * 4) as c_int,
            );
            if image.is_null() {
                return;
            }
            XPutImage(
                self.display,
                self.window,
                XDefaultGC(self.display, self.screen),
                image,
                0,
                0,
                0,
                0,
                fb.width as c_uint,
                fb.height as c_uint,
            );
            XFree(image);
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            if !self.ic.is_null() {
                XDestroyIC(self.ic);
            }
            if !self.im.is_null() {
                XCloseIM(self.im);
            }
            XCloseDisplay(self.display);
        }
    }
}

/// X11 keysym 到功能键（数值来自 X11/keysymdef.h）
fn named_key(keysym: KeySym) -> Option<Key> {
    Some(match keysym {
        0xff08 => Key::Backspace,
        0xff09 | 0xfe20 => Key::Tab, // Tab、ISO_Left_Tab（Shift+Tab）
        0xff0d | 0xff8d => Key::Enter,
        0xff1b => Key::Escape,
        0xff50 | 0xff95 => Key::Home,
        0xff51 | 0xff96 => Key::Left,
        0xff52 | 0xff97 => Key::Up,
        0xff53 | 0xff98 => Key::Right,
        0xff54 | 0xff99 => Key::Down,
        0xff55 | 0xff9a => Key::PageUp,
        0xff56 | 0xff9b => Key::PageDown,
        0xff57 | 0xff9c => Key::End,
        0xff63 | 0xff9e => Key::Insert,
        0xffff | 0xff9f => Key::Delete,
        k @ 0xffbe..=0xffc9 => Key::F((k - 0xffbe + 1) as u8),
        _ => return None,
    })
}
