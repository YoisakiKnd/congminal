//! macOS 平台：通过 Objective-C 运行时直接调用 AppKit。
//!
//! Cocoa 是 Objective-C 写的，"调用方法"其实就是调用 C 函数 `objc_msgSend(对象, 方法名, 参数...)`。
//! 我们把它强转成具体的函数指针类型来调用，这就是 [`msg!`] 宏做的事。
//!
//! 为了少写 Objective-C 类，这里没有子类化 NSView，而是：
//! - 自己写事件循环（`nextEventMatchingMask:`），直接从事件里取按键
//! - 把画好的像素做成 CGImage，设置为视图图层（CALayer）的内容

use super::{App, Event};
use crate::input::{Key, Mods};
use crate::render::Framebuffer;
use std::ffi::{CStr, c_char, c_void};
use std::ptr::null_mut;

type Id = *mut c_void;
type Sel = *const c_void;
const NIL: Id = null_mut();

#[repr(C)]
#[derive(Clone, Copy)]
struct NSPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NSSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NSRect {
    origin: NSPoint,
    size: NSSize,
}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
    #[cfg(target_arch = "x86_64")]
    fn objc_msgSend_stret();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

#[link(name = "QuartzCore", kind = "framework")]
unsafe extern "C" {}

#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {
    static NSDefaultRunLoopMode: Id;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataCreate(allocator: *const c_void, bytes: *const u8, length: isize) -> *const c_void;
    fn CFRelease(object: *const c_void);
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
    fn CGColorSpaceRelease(space: *mut c_void);
    fn CGDataProviderCreateWithCFData(data: *const c_void) -> *mut c_void;
    fn CGDataProviderRelease(provider: *mut c_void);
    fn CGImageCreate(
        width: usize,
        height: usize,
        bits_per_component: usize,
        bits_per_pixel: usize,
        bytes_per_row: usize,
        space: *mut c_void,
        bitmap_info: u32,
        provider: *mut c_void,
        decode: *const f64,
        should_interpolate: bool,
        intent: i32,
    ) -> *mut c_void;
    fn CGImageRelease(image: *mut c_void);
}

/// 发送 Objective-C 消息：`msg![对象, "方法名:", 参数 => 类型, ...; 返回类型]`
macro_rules! msg {
    ($obj:expr, $sel:literal $(, $arg:expr => $ty:ty)* ; $ret:ty) => {{
        let f: unsafe extern "C" fn(Id, Sel $(, $ty)*) -> $ret =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f($obj, sel_registerName(concat!($sel, "\0").as_ptr().cast()) $(, $arg)*)
    }};
}

unsafe fn class(name: &CStr) -> Id {
    unsafe { objc_getClass(name.as_ptr()) }
}

unsafe fn nsstring(s: &str) -> Id {
    const UTF8: usize = 4;
    unsafe {
        let obj: Id = msg![class(c"NSString"), "alloc"; Id];
        let obj: Id = msg![obj, "initWithBytes:length:encoding:",
            s.as_ptr() => *const u8, s.len() => usize, UTF8 => usize; Id];
        msg![obj, "autorelease"; Id]
    }
}

/// 读取 NSView 的 frame。返回 32 字节的结构体，x86_64 上要用 objc_msgSend_stret。
unsafe fn view_frame(view: Id) -> NSRect {
    unsafe {
        #[cfg(target_arch = "x86_64")]
        {
            let f: unsafe extern "C" fn(*mut NSRect, Id, Sel) =
                std::mem::transmute(objc_msgSend_stret as unsafe extern "C" fn());
            let mut rect = std::mem::zeroed();
            f(&mut rect, view, sel_registerName(c"frame".as_ptr()));
            rect
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            msg![view, "frame"; NSRect]
        }
    }
}

const NS_EVENT_TYPE_KEY_DOWN: usize = 10;
const NS_EVENT_TYPE_APPLICATION_DEFINED: usize = 15;
const NS_SHIFT: usize = 1 << 17;
const NS_CONTROL: usize = 1 << 18;
const NS_OPTION: usize = 1 << 19;
const NS_COMMAND: usize = 1 << 20;

pub struct Window {
    app: Id,
    window: Id,
    view: Id,
    color_space: *mut c_void,
    width: u32,
    height: u32,
}

/// 跨线程唤醒：往主线程的事件队列里塞一个自定义事件
#[derive(Clone)]
pub struct Waker;

impl Waker {
    pub fn wake(&self) {
        unsafe {
            let pool = objc_autoreleasePoolPush();
            let event: Id = msg![class(c"NSEvent"),
                "otherEventWithType:location:modifierFlags:timestamp:windowNumber:context:subtype:data1:data2:",
                NS_EVENT_TYPE_APPLICATION_DEFINED => usize,
                NSPoint { x: 0.0, y: 0.0 } => NSPoint,
                0usize => usize,
                0.0f64 => f64,
                0isize => isize,
                NIL => Id,
                0i16 => i16,
                0isize => isize,
                0isize => isize;
                Id];
            let app: Id = msg![class(c"NSApplication"), "sharedApplication"; Id];
            // postEvent:atStart: 可以在其他线程调用
            msg![app, "postEvent:atStart:", event => Id, 0i8 => i8; ()];
            objc_autoreleasePoolPop(pool);
        }
    }
}

impl Window {
    pub fn new(title: &str, size_for_scale: impl FnOnce(f64) -> (u32, u32)) -> Result<Window, String> {
        unsafe {
            let pool = objc_autoreleasePoolPush();
            let app: Id = msg![class(c"NSApplication"), "sharedApplication"; Id];
            // 普通应用：有 Dock 图标、能获得焦点
            msg![app, "setActivationPolicy:", 0isize => isize; i8];
            Self::create_menu(app);

            let screen: Id = msg![class(c"NSScreen"), "mainScreen"; Id];
            let scale = if screen.is_null() { 1.0 } else { msg![screen, "backingScaleFactor"; f64] };
            let (width, height) = size_for_scale(scale);

            // NSWindow 用的是"点"，像素 = 点 × 缩放比例
            let rect = NSRect {
                origin: NSPoint { x: 0.0, y: 0.0 },
                size: NSSize { width: width as f64 / scale, height: height as f64 / scale },
            };
            // 有标题栏 | 可关闭 | 可最小化 | 可调整大小
            let style = 1usize | 2 | 4 | 8;
            let backing_buffered = 2usize;
            let window: Id = msg![class(c"NSWindow"), "alloc"; Id];
            let window: Id = msg![window, "initWithContentRect:styleMask:backing:defer:",
                rect => NSRect, style => usize, backing_buffered => usize, 0i8 => i8; Id];
            if window.is_null() {
                objc_autoreleasePoolPop(pool);
                return Err("创建 NSWindow 失败".into());
            }
            msg![window, "setReleasedWhenClosed:", 0i8 => i8; ()];
            msg![window, "setTitle:", nsstring(title) => Id; ()];
            msg![window, "center"; ()];

            // 让视图由 CALayer 支撑，之后直接设置图层内容
            let view: Id = msg![window, "contentView"; Id];
            msg![view, "setWantsLayer:", 1i8 => i8; ()];

            msg![window, "makeKeyAndOrderFront:", NIL => Id; ()];
            msg![app, "finishLaunching"; ()];
            msg![app, "activateIgnoringOtherApps:", 1i8 => i8; ()];
            objc_autoreleasePoolPop(pool);

            Ok(Window { app, window, view, color_space: CGColorSpaceCreateDeviceRGB(), width, height })
        }
    }

    /// 最小的菜单栏：只有"退出"，这样 Cmd+Q 能用
    unsafe fn create_menu(app: Id) {
        unsafe {
            let menubar: Id = msg![msg![class(c"NSMenu"), "alloc"; Id], "init"; Id];
            let app_item: Id = msg![msg![class(c"NSMenuItem"), "alloc"; Id], "init"; Id];
            msg![menubar, "addItem:", app_item => Id; ()];
            let app_menu: Id = msg![msg![class(c"NSMenu"), "alloc"; Id], "init"; Id];
            let quit: Id = msg![msg![class(c"NSMenuItem"), "alloc"; Id],
                "initWithTitle:action:keyEquivalent:",
                nsstring("Quit congminal") => Id,
                sel_registerName(c"terminate:".as_ptr()) => Sel,
                nsstring("q") => Id;
                Id];
            msg![app_menu, "addItem:", quit => Id; ()];
            msg![app_item, "setSubmenu:", app_menu => Id; ()];
            msg![app, "setMainMenu:", menubar => Id; ()];
        }
    }

    pub fn scale_factor(&self) -> f64 {
        unsafe { msg![self.window, "backingScaleFactor"; f64] }
    }

    pub fn waker(&self) -> Waker {
        Waker
    }

    pub fn run(mut self, app: &mut dyn App) {
        if !app.event(Event::Resize { width: self.width, height: self.height }) {
            return;
        }
        unsafe {
            let distant_future: Id = msg![class(c"NSDate"), "distantFuture"; Id];
            let distant_past: Id = msg![class(c"NSDate"), "distantPast"; Id];
            loop {
                let pool = objc_autoreleasePoolPush();
                let keep_running = self.pump(app, distant_future, distant_past);
                objc_autoreleasePoolPop(pool);
                if !keep_running {
                    return;
                }
            }
        }
    }

    /// 等到至少一个事件，把排队的事件处理完，然后检查尺寸、重画
    unsafe fn pump(&mut self, app: &mut dyn App, distant_future: Id, distant_past: Id) -> bool {
        unsafe {
            let mut until = distant_future;
            loop {
                let event: Id = msg![self.app, "nextEventMatchingMask:untilDate:inMode:dequeue:",
                    u64::MAX => u64, until => Id, NSDefaultRunLoopMode => Id, 1i8 => i8; Id];
                if event.is_null() {
                    break;
                }
                // 第一个事件到了之后，剩下的只取已经排队的，不再等待
                until = distant_past;

                let kind: usize = msg![event, "type"; usize];
                let flags: usize = msg![event, "modifierFlags"; usize];
                if kind == NS_EVENT_TYPE_APPLICATION_DEFINED {
                    if !app.event(Event::Wake) {
                        return false;
                    }
                } else if kind == NS_EVENT_TYPE_KEY_DOWN && flags & NS_COMMAND == 0 {
                    // 按键自己处理，不交给 AppKit（否则会发出"嘟"的一声）
                    for e in key_events(event, flags) {
                        if !app.event(e) {
                            return false;
                        }
                    }
                } else {
                    msg![self.app, "sendEvent:", event => Id; ()];
                }
            }
            msg![self.app, "updateWindows"; ()];

            // 关闭按钮会把窗口藏起来（不是最小化）
            let visible: i8 = msg![self.window, "isVisible"; i8];
            let minimized: i8 = msg![self.window, "isMiniaturized"; i8];
            if visible == 0 && minimized == 0 && !app.event(Event::CloseRequested) {
                return false;
            }

            let frame = view_frame(self.view);
            let scale = self.scale_factor();
            let width = (frame.size.width * scale).round() as u32;
            let height = (frame.size.height * scale).round() as u32;
            if (width, height) != (self.width, self.height) {
                self.width = width;
                self.height = height;
                if !app.event(Event::Resize { width, height }) {
                    return false;
                }
            }

            if let Some(title) = app.take_title() {
                msg![self.window, "setTitle:", nsstring(&title) => Id; ()];
            }
            if app.needs_redraw() {
                self.present(app.draw());
            }
            true
        }
    }

    fn present(&self, fb: &Framebuffer) {
        if fb.width == 0 || fb.height == 0 {
            return;
        }
        // 小端序 + 跳过最高字节：内存里的 u32 0x00RRGGBB 正好被解读成 RGB
        const BYTE_ORDER_32_LITTLE: u32 = 2 << 12;
        const ALPHA_NONE_SKIP_FIRST: u32 = 6;
        unsafe {
            // 复制一份像素给 CoreGraphics，因为图层可能在我们修改缓冲区之后才去读它
            let data = CFDataCreate(std::ptr::null(), fb.pixels.as_ptr().cast(), (fb.pixels.len() * 4) as isize);
            let provider = CGDataProviderCreateWithCFData(data);
            let image = CGImageCreate(
                fb.width,
                fb.height,
                8,
                32,
                fb.width * 4,
                self.color_space,
                BYTE_ORDER_32_LITTLE | ALPHA_NONE_SKIP_FIRST,
                provider,
                std::ptr::null(),
                false,
                0,
            );
            let layer: Id = msg![self.view, "layer"; Id];
            msg![layer, "setContents:", image => *mut c_void; ()];
            CGImageRelease(image);
            CGDataProviderRelease(provider);
            CFRelease(data);
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            CGColorSpaceRelease(self.color_space);
            msg![self.window, "close"; ()];
            msg![self.window, "release"; ()];
        }
    }
}

unsafe fn key_events(event: Id, flags: usize) -> Vec<Event> {
    let mods = Mods {
        shift: flags & NS_SHIFT != 0,
        ctrl: flags & NS_CONTROL != 0,
        alt: flags & NS_OPTION != 0,
        logo: flags & NS_COMMAND != 0,
    };
    unsafe {
        let code: u16 = msg![event, "keyCode"; u16];
        if let Some(key) = named_key(code) {
            return vec![Event::Key { key, mods }];
        }
        // Option 当作 Alt 用：取不受 Option 影响的字符（否则 Option+f 会变成 ƒ）
        let chars: Id =
            if mods.alt { msg![event, "charactersIgnoringModifiers"; Id] } else { msg![event, "characters"; Id] };
        if chars.is_null() {
            return vec![];
        }
        let utf8: *const c_char = msg![chars, "UTF8String"; *const c_char];
        if utf8.is_null() {
            return vec![];
        }
        CStr::from_ptr(utf8).to_string_lossy().chars().map(|c| Event::Key { key: Key::Char(c), mods }).collect()
    }
}

/// macOS 虚拟键码（来自 Carbon 的 Events.h，和键盘布局无关）
fn named_key(code: u16) -> Option<Key> {
    Some(match code {
        36 | 76 => Key::Enter,
        48 => Key::Tab,
        51 => Key::Backspace,
        53 => Key::Escape,
        115 => Key::Home,
        116 => Key::PageUp,
        117 => Key::Delete,
        119 => Key::End,
        121 => Key::PageDown,
        123 => Key::Left,
        124 => Key::Right,
        125 => Key::Down,
        126 => Key::Up,
        114 => Key::Insert,
        122 => Key::F(1),
        120 => Key::F(2),
        99 => Key::F(3),
        118 => Key::F(4),
        96 => Key::F(5),
        97 => Key::F(6),
        98 => Key::F(7),
        100 => Key::F(8),
        101 => Key::F(9),
        109 => Key::F(10),
        103 => Key::F(11),
        111 => Key::F(12),
        _ => return None,
    })
}
