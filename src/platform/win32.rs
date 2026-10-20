//! Windows 平台：Win32 API + GDI。
//!
//! 窗口过程（WndProc）只负责把消息翻译成 [`Event`] 放进队列，
//! 真正的处理都在 `run` 的主循环里，这样就不用在回调里拿到 App 的引用。

use super::{App, ClipboardRequest, Event, MouseButton, ScrollDelta};
use crate::input::{Key, Mods};
use crate::render::Framebuffer;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{null, null_mut};

type Hwnd = isize;
type Hinstance = isize;
type Hdc = isize;
type WndProc = unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize;

#[repr(C)]
struct WndClassExW {
    size: u32,
    style: u32,
    wnd_proc: WndProc,
    cls_extra: i32,
    wnd_extra: i32,
    instance: Hinstance,
    icon: isize,
    cursor: isize,
    background: isize,
    menu_name: *const u16,
    class_name: *const u16,
    icon_small: isize,
}

#[repr(C)]
struct Msg {
    hwnd: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    pt_x: i32,
    pt_y: i32,
    private: u32,
}

#[repr(C)]
#[derive(Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct PaintStruct {
    hdc: Hdc,
    erase: i32,
    paint: Rect,
    restore: i32,
    inc_update: i32,
    reserved: [u8; 32],
}

#[repr(C)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_pels_per_meter: i32,
    y_pels_per_meter: i32,
    clr_used: u32,
    clr_important: u32,
}

#[repr(C)]
struct BitmapInfo {
    header: BitmapInfoHeader,
    colors: [u32; 1],
}

const WM_SIZE: u32 = 0x0005;
const WM_PAINT: u32 = 0x000F;
const WM_CLOSE: u32 = 0x0010;
const WM_QUIT: u32 = 0x0012;
const WM_ERASEBKGND: u32 = 0x0014;
const WM_KEYDOWN: u32 = 0x0100;
const WM_CHAR: u32 = 0x0102;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSCHAR: u32 = 0x0106;
const WM_SETFOCUS: u32 = 0x0007;
const WM_KILLFOCUS: u32 = 0x0008;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_APP_WAKE: u32 = 0x8000 + 1;
/// 滚轮一格的 delta 值
const WHEEL_DELTA: f64 = 120.0;
const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;

const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
const CW_USEDEFAULT: i32 = 0x8000_0000_u32 as i32;
const SW_SHOW: i32 = 5;
const PM_REMOVE: u32 = 1;
/// 文本输入用的 I 形鼠标指针
const IDC_IBEAM: usize = 32513;
const LOGPIXELSX: i32 = 88;
const DIB_RGB_COLORS: u32 = 0;

const VK_SHIFT: i32 = 0x10;
const VK_CONTROL: i32 = 0x11;
const VK_MENU: i32 = 0x12;
const VK_LWIN: i32 = 0x5B;
const VK_RWIN: i32 = 0x5C;
const VK_F4: usize = 0x73;

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClassExW(class: *const WndClassExW) -> u16;
    fn CreateWindowExW(
        ex_style: u32,
        class_name: *const u16,
        window_name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: Hwnd,
        menu: isize,
        instance: Hinstance,
        param: *mut c_void,
    ) -> Hwnd;
    fn DestroyWindow(hwnd: Hwnd) -> i32;
    fn DefWindowProcW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
    fn GetMessageW(msg: *mut Msg, hwnd: Hwnd, min: u32, max: u32) -> i32;
    fn PeekMessageW(msg: *mut Msg, hwnd: Hwnd, min: u32, max: u32, remove: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn PostMessageW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> i32;
    fn ShowWindow(hwnd: Hwnd, cmd: i32) -> i32;
    fn GetClientRect(hwnd: Hwnd, rect: *mut Rect) -> i32;
    fn AdjustWindowRect(rect: *mut Rect, style: u32, menu: i32) -> i32;
    fn GetDC(hwnd: Hwnd) -> Hdc;
    fn ReleaseDC(hwnd: Hwnd, hdc: Hdc) -> i32;
    fn BeginPaint(hwnd: Hwnd, paint: *mut PaintStruct) -> Hdc;
    fn EndPaint(hwnd: Hwnd, paint: *const PaintStruct) -> i32;
    fn LoadCursorW(instance: Hinstance, name: usize) -> isize;
    fn GetKeyState(key: i32) -> i16;
    fn SetWindowTextW(hwnd: Hwnd, text: *const u16) -> i32;
    fn SetProcessDPIAware() -> i32;
    fn SetCapture(hwnd: Hwnd) -> Hwnd;
    fn ScreenToClient(hwnd: Hwnd, point: *mut Point) -> i32;
    fn ReleaseCapture() -> i32;
    fn OpenClipboard(hwnd: Hwnd) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn GetClipboardData(format: u32) -> isize;
    fn SetClipboardData(format: u32, memory: isize) -> isize;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn SetDIBitsToDevice(
        hdc: Hdc,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        src_x: i32,
        src_y: i32,
        start_scan: u32,
        lines: u32,
        bits: *const c_void,
        info: *const BitmapInfo,
        usage: u32,
    ) -> i32;
    fn GetDeviceCaps(hdc: Hdc, index: i32) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> Hinstance;
    fn GlobalAlloc(flags: u32, bytes: usize) -> isize;
    fn GlobalLock(memory: isize) -> *mut c_void;
    fn GlobalUnlock(memory: isize) -> i32;
    fn GlobalFree(memory: isize) -> isize;
}

#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
struct CompositionForm {
    style: u32,
    position: Point,
    area: Rect,
}

#[repr(C)]
struct CandidateForm {
    index: u32,
    style: u32,
    position: Point,
    area: Rect,
}

const CFS_POINT: u32 = 0x0002;
const CFS_EXCLUDE: u32 = 0x0080;

#[link(name = "imm32")]
unsafe extern "system" {
    fn ImmGetContext(hwnd: Hwnd) -> isize;
    fn ImmReleaseContext(hwnd: Hwnd, context: isize) -> i32;
    fn ImmSetCompositionWindow(context: isize, form: *const CompositionForm) -> i32;
    fn ImmSetCandidateWindow(context: isize, form: *const CandidateForm) -> i32;
}

thread_local! {
    /// WndProc 产生的事件，等主循环来取
    static QUEUE: RefCell<VecDeque<Event>> = const { RefCell::new(VecDeque::new()) };
    /// 刚按下的键已经作为功能键处理过，跳过 TranslateMessage 随后生成的 WM_CHAR
    static SUPPRESS_CHAR: Cell<bool> = const { Cell::new(false) };
    /// UTF-16 代理对的前半部分
    static HIGH_SURROGATE: Cell<u16> = const { Cell::new(0) };
}

fn push(event: Event) {
    QUEUE.with(|q| q.borrow_mut().push_back(event));
}

pub struct Window {
    hwnd: Hwnd,
    scale: f64,
    /// 上次告诉输入法的光标位置
    ime_area: Option<(u32, u32, u32, u32)>,
}

/// 跨线程唤醒：PostMessageW 是线程安全的
#[derive(Clone)]
pub struct Waker(Hwnd);

impl Waker {
    pub fn wake(&self) {
        unsafe { PostMessageW(self.0, WM_APP_WAKE, 0, 0) };
    }
}

impl Window {
    pub fn new(title: &str, size_for_scale: impl FnOnce(f64) -> (u32, u32)) -> Result<Window, String> {
        unsafe {
            // 告诉系统我们自己处理高 DPI，别把窗口拉伸模糊
            SetProcessDPIAware();
            let screen = GetDC(0);
            let scale = GetDeviceCaps(screen, LOGPIXELSX) as f64 / 96.0;
            ReleaseDC(0, screen);
            let (width, height) = size_for_scale(scale);

            let instance = GetModuleHandleW(null());
            let class_name = wide("congminal");
            let class = WndClassExW {
                size: size_of::<WndClassExW>() as u32,
                style: 0,
                wnd_proc,
                cls_extra: 0,
                wnd_extra: 0,
                instance,
                icon: 0,
                cursor: LoadCursorW(0, IDC_IBEAM),
                background: 0,
                menu_name: null(),
                class_name: class_name.as_ptr(),
                icon_small: 0,
            };
            if RegisterClassExW(&class) == 0 {
                return Err("RegisterClassExW 失败".into());
            }

            // 我们想要的是内容区域的尺寸，要换算成包括标题栏和边框的窗口尺寸
            let mut rect = Rect { left: 0, top: 0, right: width as i32, bottom: height as i32 };
            AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, 0);

            let title = wide(title);
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                rect.right - rect.left,
                rect.bottom - rect.top,
                0,
                0,
                instance,
                null_mut(),
            );
            if hwnd == 0 {
                return Err("CreateWindowExW 失败".into());
            }
            ShowWindow(hwnd, SW_SHOW);
            Ok(Window { hwnd, scale, ime_area: None })
        }
    }

    pub fn scale_factor(&self) -> f64 {
        self.scale
    }

    pub fn waker(&self) -> Waker {
        Waker(self.hwnd)
    }

    pub fn run(mut self, app: &mut dyn App) {
        let mut rect = Rect::default();
        unsafe { GetClientRect(self.hwnd, &mut rect) };
        push(Event::Resize { width: rect.right as u32, height: rect.bottom as u32 });

        let mut msg: Msg = unsafe { std::mem::zeroed() };
        loop {
            while let Some(event) = QUEUE.with(|q| q.borrow_mut().pop_front()) {
                if !app.event(event) {
                    return;
                }
            }
            if let Some(title) = app.take_title() {
                unsafe { SetWindowTextW(self.hwnd, wide(&title).as_ptr()) };
            }
            if app.needs_redraw() {
                self.present(app.draw());
            }
            if let Some(request) = app.take_clipboard_request() {
                match request {
                    ClipboardRequest::Copy(text) => self.set_clipboard(&text),
                    ClipboardRequest::Paste => {
                        if let Some(text) = self.clipboard_text()
                            && !app.event(Event::Paste(text))
                        {
                            return;
                        }
                    }
                }
            }
            self.update_ime_position(app.ime_cursor_area());

            // 阻塞等待下一条消息，然后把已经排队的消息一起处理掉
            if unsafe { GetMessageW(&mut msg, 0, 0, 0) } <= 0 {
                return;
            }
            loop {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if unsafe { PeekMessageW(&mut msg, 0, 0, 0, PM_REMOVE) } == 0 {
                    break;
                }
                if msg.message == WM_QUIT {
                    return;
                }
            }
        }
    }

    /// 写剪贴板：UTF-16 文字放进一块全局内存交给系统。Windows 的换行习惯是 \r\n。
    fn set_clipboard(&self, text: &str) {
        let wide: Vec<u16> = text.replace('\n', "\r\n").encode_utf16().chain(Some(0)).collect();
        unsafe {
            let memory = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
            if memory == 0 {
                return;
            }
            let pointer = GlobalLock(memory).cast::<u16>();
            if pointer.is_null() {
                GlobalFree(memory);
                return;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), pointer, wide.len());
            GlobalUnlock(memory);
            if OpenClipboard(self.hwnd) == 0 {
                GlobalFree(memory);
                return;
            }
            EmptyClipboard();
            // 成功后这块内存归系统所有，不能再释放
            if SetClipboardData(CF_UNICODETEXT, memory) == 0 {
                GlobalFree(memory);
            }
            CloseClipboard();
        }
    }

    fn clipboard_text(&self) -> Option<String> {
        unsafe {
            if OpenClipboard(self.hwnd) == 0 {
                return None;
            }
            let memory = GetClipboardData(CF_UNICODETEXT);
            let pointer = if memory == 0 { null_mut() } else { GlobalLock(memory).cast::<u16>() };
            let text = (!pointer.is_null()).then(|| {
                let len = (0..).take_while(|&i| *pointer.add(i) != 0).count();
                String::from_utf16_lossy(std::slice::from_raw_parts(pointer, len))
            });
            if !pointer.is_null() {
                GlobalUnlock(memory);
            }
            CloseClipboard();
            text
        }
    }

    /// 告诉输入法光标在哪：拼音输入框跟着光标走，候选词窗口避开光标所在的行
    fn update_ime_position(&mut self, area: (u32, u32, u32, u32)) {
        if self.ime_area == Some(area) {
            return;
        }
        self.ime_area = Some(area);
        let (x, y, w, h) = (area.0 as i32, area.1 as i32, area.2 as i32, area.3 as i32);
        let cursor = Rect { left: x, top: y, right: x + w, bottom: y + h };
        unsafe {
            let context = ImmGetContext(self.hwnd);
            if context == 0 {
                return;
            }
            let composition = CompositionForm { style: CFS_POINT, position: Point { x, y }, area: Rect::default() };
            ImmSetCompositionWindow(context, &composition);
            let candidate =
                CandidateForm { index: 0, style: CFS_EXCLUDE, position: Point { x, y: y + h }, area: cursor };
            ImmSetCandidateWindow(context, &candidate);
            ImmReleaseContext(self.hwnd, context);
        }
    }

    fn present(&self, fb: &Framebuffer) {
        if fb.width == 0 || fb.height == 0 {
            return;
        }
        let info = BitmapInfo {
            header: BitmapInfoHeader {
                size: size_of::<BitmapInfoHeader>() as u32,
                width: fb.width as i32,
                // 负数表示自上而下存储，和我们的缓冲区一致
                height: -(fb.height as i32),
                planes: 1,
                bit_count: 32,
                compression: 0,
                size_image: 0,
                x_pels_per_meter: 0,
                y_pels_per_meter: 0,
                clr_used: 0,
                clr_important: 0,
            },
            colors: [0],
        };
        unsafe {
            let hdc = GetDC(self.hwnd);
            SetDIBitsToDevice(
                hdc,
                0,
                0,
                fb.width as u32,
                fb.height as u32,
                0,
                0,
                0,
                fb.height as u32,
                fb.pixels.as_ptr().cast(),
                &info,
                DIB_RGB_COLORS,
            );
            ReleaseDC(self.hwnd, hdc);
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.hwnd) };
    }
}

unsafe extern "system" fn wnd_proc(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize {
    match msg {
        WM_CLOSE => {
            push(Event::CloseRequested);
            0
        }
        WM_SIZE => {
            let (width, height) = ((lparam & 0xFFFF) as u32, ((lparam >> 16) & 0xFFFF) as u32);
            // 最小化时尺寸是 0，忽略
            if width > 0 && height > 0 {
                push(Event::Resize { width, height });
            }
            0
        }
        WM_PAINT => {
            unsafe {
                let mut paint: PaintStruct = std::mem::zeroed();
                BeginPaint(hwnd, &mut paint);
                EndPaint(hwnd, &paint);
            }
            push(Event::Redraw);
            0
        }
        // 我们每次都会画满整个窗口，不需要系统先擦背景（否则会闪）
        WM_ERASEBKGND => 1,
        WM_APP_WAKE => {
            push(Event::Wake);
            0
        }
        WM_LBUTTONDOWN | WM_MBUTTONDOWN | WM_RBUTTONDOWN => {
            // 捕获鼠标：拖出窗口外也能继续收到移动和抬起的消息
            unsafe { SetCapture(hwnd) };
            let (x, y) = mouse_position(lparam);
            push(Event::MouseDown { button: mouse_button(msg), x, y, mods: current_mods() });
            0
        }
        WM_LBUTTONUP | WM_MBUTTONUP | WM_RBUTTONUP => {
            unsafe { ReleaseCapture() };
            let (x, y) = mouse_position(lparam);
            push(Event::MouseUp { button: mouse_button(msg), x, y, mods: current_mods() });
            0
        }
        WM_MOUSEMOVE => {
            let (x, y) = mouse_position(lparam);
            push(Event::MouseMove { x, y, mods: current_mods() });
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam >> 16) & 0xFFFF) as u16 as i16;
            // 滚轮消息给的是屏幕坐标，要换算成窗口里的坐标
            let (sx, sy) = mouse_position(lparam);
            let mut point = Point { x: sx as i32, y: sy as i32 };
            unsafe { ScreenToClient(hwnd, &mut point) };
            push(Event::Scroll {
                delta: ScrollDelta::Lines(delta as f64 / WHEEL_DELTA),
                x: point.x as f64,
                y: point.y as f64,
                mods: current_mods(),
            });
            0
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            push(Event::Focus(msg == WM_SETFOCUS));
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let mods = current_mods();
            match named_key(wparam) {
                // Alt+F4 留给系统关窗口
                Some(_) if mods.alt && wparam == VK_F4 => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
                Some(key) => {
                    if matches!(key, Key::Enter | Key::Backspace | Key::Tab | Key::Escape) {
                        SUPPRESS_CHAR.set(true);
                    }
                    push(Event::Key { key, mods });
                    0
                }
                None => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
            }
        }
        WM_CHAR | WM_SYSCHAR => {
            if SUPPRESS_CHAR.replace(false) {
                return 0;
            }
            let unit = wparam as u16;
            let c = match unit {
                0xD800..=0xDBFF => {
                    HIGH_SURROGATE.set(unit);
                    return 0;
                }
                0xDC00..=0xDFFF => {
                    let high = HIGH_SURROGATE.replace(0) as u32;
                    char::from_u32(0x10000 + ((high - 0xD800) << 10) + (unit as u32 - 0xDC00))
                }
                _ => char::from_u32(unit as u32),
            };
            let mut mods = current_mods();
            if msg == WM_SYSCHAR {
                mods.alt = true;
            }
            // 欧洲键盘布局的 AltGr 在系统里表现为 Ctrl+Alt，此时字符已经是最终结果
            if mods.ctrl && mods.alt {
                mods.ctrl = false;
                mods.alt = false;
            }
            if let Some(c) = c {
                push(Event::Key { key: Key::Char(c), mods });
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 鼠标消息的 lparam：低 16 位是 x，高 16 位是 y（有符号，拖到窗口外是负数）
fn mouse_position(lparam: isize) -> (f64, f64) {
    ((lparam & 0xFFFF) as u16 as i16 as f64, ((lparam >> 16) & 0xFFFF) as u16 as i16 as f64)
}

fn mouse_button(msg: u32) -> MouseButton {
    match msg {
        WM_MBUTTONDOWN | WM_MBUTTONUP => MouseButton::Middle,
        WM_RBUTTONDOWN | WM_RBUTTONUP => MouseButton::Right,
        _ => MouseButton::Left,
    }
}

fn current_mods() -> Mods {
    let down = |vk| unsafe { GetKeyState(vk) } < 0;
    Mods { shift: down(VK_SHIFT), ctrl: down(VK_CONTROL), alt: down(VK_MENU), logo: down(VK_LWIN) || down(VK_RWIN) }
}

/// 虚拟键码到功能键
fn named_key(vk: usize) -> Option<Key> {
    Some(match vk {
        0x08 => Key::Backspace,
        0x09 => Key::Tab,
        0x0D => Key::Enter,
        0x1B => Key::Escape,
        0x21 => Key::PageUp,
        0x22 => Key::PageDown,
        0x23 => Key::End,
        0x24 => Key::Home,
        0x25 => Key::Left,
        0x26 => Key::Up,
        0x27 => Key::Right,
        0x28 => Key::Down,
        0x2D => Key::Insert,
        0x2E => Key::Delete,
        vk @ 0x70..=0x7B => Key::F((vk - 0x70 + 1) as u8),
        _ => return None,
    })
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
