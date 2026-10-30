//! Linux 平台：Xlib。
//!
//! 在 Wayland 桌面上会通过 XWayland 运行。以后可以加一个原生 Wayland 后端，
//! 或者干脆绕开 Xlib，直接通过 Unix socket 说 X11 协议。
//!
//! 事件循环：用 `poll` 同时等待 X 服务器的连接和一个唤醒用的 socket。

use super::{App, ClipboardRequest, Event, MouseButton, ScrollDelta};
use crate::input::{Key, Keypad, Mods};
use crate::render::{Damage, Framebuffer};
use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uint, c_ulong, c_void};
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

/// 鼠标按键事件，布局和 XKeyEvent 一样，只是最后是按键编号
#[repr(C)]
#[derive(Clone, Copy)]
struct XButtonEvent {
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
    button: c_uint,
    same_screen: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XMotionEvent {
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
    is_hint: c_char,
    same_screen: c_int,
}

/// 别的程序来要剪贴板内容
#[repr(C)]
#[derive(Clone, Copy)]
struct XSelectionRequestEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut Display,
    owner: XWindow,
    requestor: XWindow,
    selection: Atom,
    target: Atom,
    property: Atom,
    time: c_ulong,
}

/// 剪贴板内容准备好了（或者对方拒绝了）
#[repr(C)]
#[derive(Clone, Copy)]
struct XSelectionEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut Display,
    requestor: XWindow,
    selection: Atom,
    target: Atom,
    property: Atom,
    time: c_ulong,
}

/// XEvent 是一个联合体，C 里定义为 24 个 long 那么大
#[repr(C)]
union XEvent {
    type_: c_int,
    key: XKeyEvent,
    button: XButtonEvent,
    motion: XMotionEvent,
    configure: XConfigureEvent,
    expose: XExposeEvent,
    client: XClientMessageEvent,
    selection_request: XSelectionRequestEvent,
    selection: XSelectionEvent,
    pad: [c_long; 24],
}

const KEY_PRESS: c_int = 2;
const BUTTON_PRESS: c_int = 4;
const BUTTON_RELEASE: c_int = 5;
const MOTION_NOTIFY: c_int = 6;
const SELECTION_CLEAR: c_int = 29;
const SELECTION_REQUEST: c_int = 30;
const SELECTION_NOTIFY: c_int = 31;
const FOCUS_IN: c_int = 9;
const FOCUS_OUT: c_int = 10;
const EXPOSE: c_int = 12;
const CONFIGURE_NOTIFY: c_int = 22;
const CLIENT_MESSAGE: c_int = 33;
const DESTROY_NOTIFY: c_int = 17;

const KEY_PRESS_MASK: c_long = 1 << 0;
const EXPOSURE_MASK: c_long = 1 << 15;
const STRUCTURE_NOTIFY_MASK: c_long = 1 << 17;
const FOCUS_CHANGE_MASK: c_long = 1 << 21;
const BUTTON_PRESS_MASK: c_long = 1 << 2;
const BUTTON_RELEASE_MASK: c_long = 1 << 3;
/// 只在按住鼠标键时报告移动
/// 所有鼠标移动（程序开了 1003 模式时要报告没按键时的移动）
const POINTER_MOTION_MASK: c_long = 1 << 6;

const XA_ATOM: Atom = 4;
const XA_CARDINAL: Atom = 6;
const XA_STRING: Atom = 31;
const PROP_MODE_REPLACE: c_int = 0;
const CURRENT_TIME: c_ulong = 0;
/// 文本输入用的 I 形鼠标指针（X11/cursorfont.h 里的 XC_xterm）
const XC_XTERM: c_uint = 152;

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
    fn XSetSelectionOwner(display: *mut Display, selection: Atom, owner: XWindow, time: c_ulong) -> c_int;
    fn XDeleteProperty(display: *mut Display, window: XWindow, property: Atom) -> c_int;
    fn XGetSelectionOwner(display: *mut Display, selection: Atom) -> XWindow;
    fn XConvertSelection(
        display: *mut Display,
        selection: Atom,
        target: Atom,
        property: Atom,
        requestor: XWindow,
        time: c_ulong,
    ) -> c_int;
    fn XChangeProperty(
        display: *mut Display,
        window: XWindow,
        property: Atom,
        kind: Atom,
        format: c_int,
        mode: c_int,
        data: *const u8,
        count: c_int,
    ) -> c_int;
    fn XGetWindowProperty(
        display: *mut Display,
        window: XWindow,
        property: Atom,
        offset: c_long,
        length: c_long,
        delete: c_int,
        requested_type: Atom,
        actual_type: *mut Atom,
        actual_format: *mut c_int,
        count: *mut c_ulong,
        bytes_after: *mut c_ulong,
        data: *mut *mut u8,
    ) -> c_int;
    fn XSendEvent(display: *mut Display, window: XWindow, propagate: c_int, mask: c_long, event: *mut XEvent) -> c_int;
    fn XCreateFontCursor(display: *mut Display, shape: c_uint) -> c_ulong;
    fn XDefineCursor(display: *mut Display, window: XWindow, cursor: c_ulong) -> c_int;
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
    _icon_memory: Option<crate::resources::Lease>,
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
    atoms: Atoms,
    /// 我们复制的文字。X11 的剪贴板只是"所有权"，内容一直留在复制它的程序手里，别人来要时才给。
    clipboard: Option<super::CopyText>,
    paste_read: Option<super::PasteRequest>,
    /// 已经设过的 `_NET_WM_WINDOW_OPACITY`（32 位定点），只在变化时才发请求。
    opacity: Option<c_ulong>,
}

struct Atoms {
    clipboard: Atom,
    utf8_string: Atom,
    targets: Atom,
    /// 粘贴时请对方把内容放在我们窗口的这个属性上
    paste_property: Atom,
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
            XSelectInput(
                display,
                window,
                KEY_PRESS_MASK
                    | EXPOSURE_MASK
                    | STRUCTURE_NOTIFY_MASK
                    | FOCUS_CHANGE_MASK
                    | BUTTON_PRESS_MASK
                    | BUTTON_RELEASE_MASK
                    | POINTER_MOTION_MASK,
            );
            XDefineCursor(display, window, XCreateFontCursor(display, XC_XTERM));
            let atom = |name: &CStr| XInternAtom(display, name.as_ptr(), 0);
            // _NET_WM_ICON is CARDINAL[width,height,ARGB...]; Xlib uses native longs for format 32.
            let icon_memory = crate::resources::current().lease(crate::resources::Kind::Image, 64 * 1024).ok();
            if icon_memory.is_some()
                && let Ok(icon) = crate::image::png::decode(include_bytes!("../../assets/icons/congminal-64.png"))
                && icon.width == 64
                && icon.height == 64
            {
                let mut pixels = [0 as c_ulong; 64 * 64 + 2];
                pixels[0] = 64;
                pixels[1] = 64;
                for (i, pixel) in icon.rgba().as_chunks::<4>().0.iter().enumerate() {
                    pixels[i + 2] = ((u32::from(pixel[3]) << 24)
                        | (u32::from(pixel[0]) << 16)
                        | (u32::from(pixel[1]) << 8)
                        | u32::from(pixel[2])) as c_ulong;
                }
                XChangeProperty(
                    display,
                    window,
                    atom(c"_NET_WM_ICON"),
                    6,
                    32,
                    0,
                    pixels.as_ptr().cast(),
                    pixels.len() as c_int,
                );
            }
            let class = b"congminal\0congminal\0";
            XChangeProperty(display, window, atom(c"WM_CLASS"), 31, 8, 0, class.as_ptr(), 20);
            let atoms = Atoms {
                clipboard: atom(c"CLIPBOARD"),
                utf8_string: atom(c"UTF8_STRING"),
                targets: atom(c"TARGETS"),
                paste_property: atom(c"CONGMINAL_PASTE"),
            };
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
                _icon_memory: icon_memory,
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
                atoms,
                clipboard: None,
                paste_read: None,
                opacity: None,
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

    /// 设整窗不透明度：`_NET_WM_WINDOW_OPACITY` 是 EWMH 约定的 32 位定点数，
    /// 由合成器（picom/mutter/kwin 等）读取后做混合。没有合成器时属性被忽略，
    /// 窗口保持不透明——不报错，也不影响其它功能。
    fn apply_opacity(&mut self, opacity: f32) {
        let value = (opacity.clamp(0.0, 1.0) as f64 * u32::MAX as f64).round() as c_ulong;
        if self.opacity == Some(value) {
            return;
        }
        self.opacity = Some(value);
        unsafe {
            let atom = XInternAtom(self.display, c"_NET_WM_WINDOW_OPACITY".as_ptr(), 0);
            if atom == 0 {
                return;
            }
            XChangeProperty(
                self.display,
                self.window,
                atom,
                XA_CARDINAL,
                32,
                PROP_MODE_REPLACE,
                (&value as *const c_ulong).cast(),
                1,
            );
            XFlush(self.display);
        }
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

            if !app.tick() {
                return;
            }

            // 3. 改标题、重画
            if let Some(title) = app.take_title() {
                let title = CString::new(title).unwrap_or_default();
                unsafe { XStoreName(self.display, self.window, title.as_ptr()) };
            }
            if let Some(request) = app.take_clipboard_request() {
                match request {
                    ClipboardRequest::Copy(text) => self.copy(text),
                    ClipboardRequest::Paste(policy) => {
                        if let Some(event) = self.request_paste(policy)
                            && !app.event(event)
                        {
                            return;
                        }
                    }
                }
            }
            if let Some(opacity) = app.window_opacity() {
                self.apply_opacity(opacity);
            }
            if app.needs_redraw()
                && let Some((fb, damage)) = app.draw()
            {
                self.present(fb, damage);
            }
            unsafe { XFlush(self.display) };

            // 4. 没事做就睡觉，直到 X 服务器或唤醒 socket 有数据
            if unsafe { XPending(self.display) } == 0 {
                let timeout = super::timeout_ms(app.deadline()).map_or(-1, |ms| ms.min(i32::MAX as u32) as c_int);
                unsafe { poll(fds.as_mut_ptr(), fds.len() as c_ulong, timeout) };
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
                // 窗口被直接销毁（`xdotool windowclose`、窗口管理器崩溃、别的进程 XDestroyWindow）时
                // 不会经过 WM_DELETE_WINDOW；不退出的话进程会没有窗口地留在后台，连带子进程一起。
                DESTROY_NOTIFY => vec![Event::CloseRequested],
                FOCUS_IN | FOCUS_OUT => {
                    let focused = event.type_ == FOCUS_IN;
                    if !self.ic.is_null() {
                        if focused { XSetICFocus(self.ic) } else { XUnsetICFocus(self.ic) }
                    }
                    vec![Event::Focus(focused)]
                }
                BUTTON_PRESS | BUTTON_RELEASE => {
                    let b = event.button;
                    let (x, y, mods) = (b.x as f64, b.y as f64, mods_from_state(b.state));
                    let pressed = event.type_ == BUTTON_PRESS;
                    let scroll = |lines| vec![Event::Scroll { delta: ScrollDelta::Lines(lines), x, y, mods }];
                    let button = match b.button {
                        1 => MouseButton::Left,
                        2 => MouseButton::Middle,
                        3 => MouseButton::Right,
                        // 滚轮在 X11 里是 4（上）、5（下）号按键，按下一次算一格
                        4 if pressed => return scroll(1.0),
                        5 if pressed => return scroll(-1.0),
                        _ => return vec![],
                    };
                    if pressed {
                        vec![Event::MouseDown { button, x, y, mods }]
                    } else {
                        vec![Event::MouseUp { button, x, y, mods }]
                    }
                }
                MOTION_NOTIFY => {
                    let m = event.motion;
                    vec![Event::MouseMove { x: m.x as f64, y: m.y as f64, mods: mods_from_state(m.state) }]
                }
                SELECTION_REQUEST => {
                    self.answer_selection_request(&event.selection_request);
                    vec![]
                }
                SELECTION_CLEAR => {
                    self.clipboard = None;
                    vec![]
                }
                SELECTION_NOTIFY
                    if event.selection.selection == self.atoms.clipboard
                        && event.selection.requestor == self.window =>
                {
                    if let Some(mut request) = self.paste_read.take() {
                        let text = self.read_paste(&event.selection, &mut request);
                        vec![request.finish(text)]
                    } else {
                        vec![]
                    }
                }
                _ => vec![],
            }
        }
    }

    // ---- 剪贴板 ----

    fn copy(&mut self, text: super::CopyText) {
        self.clipboard = Some(text);
        unsafe { XSetSelectionOwner(self.display, self.atoms.clipboard, self.window, CURRENT_TIME) };
    }

    /// 剪贴板是自己的就直接给出内容；否则请剪贴板的主人把内容转成 UTF-8 放到我们窗口上，
    /// 结果在 SelectionNotify 事件里到达
    fn request_paste(&mut self, mut request: super::PasteRequest) -> Option<Event> {
        // 工作区只允许一次在途读取；在答复前不覆盖该次的限量策略。
        if self.paste_read.is_some() {
            return None;
        }
        let policy = request.policy();
        unsafe {
            let owner = XGetSelectionOwner(self.display, self.atoms.clipboard);
            if owner == self.window {
                let text = self
                    .clipboard
                    .as_ref()
                    .filter(|s| s.len() <= policy.limit())
                    .and_then(|s| request.utf8(s.as_bytes()));
                return Some(request.finish(text));
            }
            if owner == 0 {
                return Some(request.finish(None));
            }
            self.paste_read = Some(request);
            XConvertSelection(
                self.display,
                self.atoms.clipboard,
                self.atoms.utf8_string,
                self.atoms.paste_property,
                self.window,
                CURRENT_TIME,
            );
        }
        None
    }

    fn read_paste(&mut self, event: &XSelectionEvent, request: &mut super::PasteRequest) -> Option<String> {
        let policy = request.policy();
        if event.property == 0 || event.property != self.atoms.paste_property {
            return None; // 剪贴板是空的，或者对方不支持 UTF-8
        }
        let Some(_external) = request.external_memory() else {
            unsafe { XDeleteProperty(self.display, self.window, event.property) };
            return None;
        };
        unsafe {
            let (mut kind, mut format, mut count, mut remaining) = (0, 0, 0, 0);
            let mut data: *mut u8 = null_mut();
            XGetWindowProperty(
                self.display,
                self.window,
                event.property,
                0,
                // 单位是 4 字节；分配前限制剪贴板读取上限。
                policy.limit().div_ceil(4) as c_long,
                1,
                0,
                &mut kind,
                &mut format,
                &mut count,
                &mut remaining,
                &mut data,
            );
            XDeleteProperty(self.display, self.window, event.property);
            if data.is_null() {
                return None;
            }
            let text =
                (kind == self.atoms.utf8_string && format == 8 && remaining == 0 && count as usize <= policy.limit())
                    .then(|| request.utf8(std::slice::from_raw_parts(data, count as usize)))
                    .flatten();
            XFree(data.cast());
            text
        }
    }

    /// 别的程序要我们的剪贴板内容：先告诉它支持哪些格式（TARGETS），再给 UTF-8 文字
    fn answer_selection_request(&self, request: &XSelectionRequestEvent) {
        let property = if request.property == 0 { request.target } else { request.property };
        let mut reply = XSelectionEvent {
            type_: SELECTION_NOTIFY,
            serial: 0,
            send_event: 1,
            display: self.display,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property: 0,
            time: request.time,
        };
        unsafe {
            if let Some(text) = &self.clipboard {
                if request.target == self.atoms.targets {
                    // 格式为 32 的属性在 Xlib 里按 long 传
                    let targets: [c_ulong; 3] = [self.atoms.targets, self.atoms.utf8_string, XA_STRING];
                    XChangeProperty(
                        self.display,
                        request.requestor,
                        property,
                        XA_ATOM,
                        32,
                        PROP_MODE_REPLACE,
                        targets.as_ptr().cast(),
                        targets.len() as c_int,
                    );
                    reply.property = property;
                } else if request.target == self.atoms.utf8_string || request.target == XA_STRING {
                    XChangeProperty(
                        self.display,
                        request.requestor,
                        property,
                        request.target,
                        8,
                        PROP_MODE_REPLACE,
                        text.as_ptr(),
                        text.len() as c_int,
                    );
                    reply.property = property;
                }
            }
            let mut event = XEvent { selection: reply };
            XSendEvent(self.display, request.requestor, 0, 0, &mut event);
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

        let mods = mods_from_state(event.state);
        if let Some(key) = named_key(keysym) {
            return vec![Event::Key { key, mods }];
        }
        // 控制转换会把 Ctrl+Shift+Minus 变成与 Ctrl+/ 相同的 0x1f；
        // 对缩放符号保留布局产生的 keysym，让动作分发能区分，VT 编码仍在 input 层完成。
        if mods.ctrl && matches!(keysym, 0x2b | 0x2d | 0x30 | 0x3d | 0x5f | 0x29) {
            return vec![Event::Key { key: Key::Char(char::from_u32(keysym as u32).unwrap()), mods }];
        }
        text.chars().map(|c| Event::Key { key: Key::Char(c), mods }).collect()
    }

    /// 只把变化的那几行像素送给 X 服务器
    fn present(&self, fb: &Framebuffer, damage: Damage) {
        if fb.width == 0 || fb.height == 0 || damage.height == 0 {
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
                damage.y as c_int,
                0,
                damage.y as c_int,
                fb.width as c_uint,
                damage.height as c_uint,
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

/// X 事件里的修饰键状态
fn mods_from_state(state: c_uint) -> Mods {
    Mods {
        shift: state & SHIFT_MASK != 0,
        ctrl: state & CONTROL_MASK != 0,
        alt: state & MOD1_MASK != 0,
        logo: state & MOD4_MASK != 0,
    }
}

/// X11 keysym 到功能键（数值来自 X11/keysymdef.h）
fn named_key(keysym: KeySym) -> Option<Key> {
    Some(match keysym {
        0xff08 => Key::Backspace,
        0xff09 | 0xfe20 => Key::Tab, // Tab、ISO_Left_Tab（Shift+Tab）
        0xff0d => Key::Enter,
        // 小键盘：应用程序键区模式下 input 层改发 SS3，普通模式发数字/运算符
        0xff8d => Key::Keypad(Keypad::Enter),
        k @ 0xffb0..=0xffb9 => Key::Keypad(Keypad::Digit((k - 0xffb0) as u8)),
        0xffae => Key::Keypad(Keypad::Decimal),
        0xffac => Key::Keypad(Keypad::Separator),
        0xffab => Key::Keypad(Keypad::Add),
        0xffad => Key::Keypad(Keypad::Subtract),
        0xffaa => Key::Keypad(Keypad::Multiply),
        0xffaf => Key::Keypad(Keypad::Divide),
        0xffbd => Key::Keypad(Keypad::Equal),
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keypad_keysyms_are_distinguished_from_main_keyboard() {
        assert_eq!(named_key(0xffb0), Some(Key::Keypad(Keypad::Digit(0))));
        assert_eq!(named_key(0xffb9), Some(Key::Keypad(Keypad::Digit(9))));
        assert_eq!(named_key(0xffae), Some(Key::Keypad(Keypad::Decimal)));
        assert_eq!(named_key(0xffac), Some(Key::Keypad(Keypad::Separator)));
        assert_eq!(named_key(0xffab), Some(Key::Keypad(Keypad::Add)));
        assert_eq!(named_key(0xffad), Some(Key::Keypad(Keypad::Subtract)));
        assert_eq!(named_key(0xffaa), Some(Key::Keypad(Keypad::Multiply)));
        assert_eq!(named_key(0xffaf), Some(Key::Keypad(Keypad::Divide)));
        assert_eq!(named_key(0xffbd), Some(Key::Keypad(Keypad::Equal)));
        // 小键盘回车（KP_Enter 0xff8d）要和主键盘回车区分开
        assert_eq!(named_key(0xff8d), Some(Key::Keypad(Keypad::Enter)));
        assert_eq!(named_key(0xff0d), Some(Key::Enter));
        // 主键盘数字仍然是普通字符，不受键区模式影响
        assert_eq!(named_key(0x31), None);
        assert_eq!(named_key(0xff52), Some(Key::Up));
    }
}
