//! macOS 平台：通过 Objective-C 运行时直接调用 AppKit。
//!
//! Cocoa 是 Objective-C 写的，"调用方法"其实就是调用 C 函数 `objc_msgSend(对象, 方法名, 参数...)`。
//! 我们把它强转成具体的函数指针类型来调用，这就是 [`msg!`] 宏做的事。
//!
//! 结构：
//! - 自己写事件循环（`nextEventMatchingMask:`），每个事件交给 AppKit 分发
//! - 在运行时注册一个 NSView 子类 `CongminalView`，实现 `NSTextInputClient` 协议，这样系统输入法（拼音等）才能工作
//! - 把画好的像素做成 CGImage，设置为视图图层（CALayer）的内容
//!
//! 视图的方法是 AppKit 回调的，拿不到 App，所以它们把事件放进线程局部的队列，事件循环再取出来交给 App。

use super::{App, ClipboardRequest, Event, MouseButton, ScrollDelta};
use crate::config::OptionAsAlt;
use crate::input::{Key, Keypad, Mods};
use crate::render::Framebuffer;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_void};
use std::ptr::null_mut;
use std::sync::OnceLock;

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

#[repr(C)]
#[derive(Clone, Copy)]
struct NSRange {
    location: usize,
    length: usize,
}

/// NSNotFound（NSIntegerMax）
const NOT_FOUND: usize = isize::MAX as usize;

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn sel_getName(sel: Sel) -> *const c_char;
    fn objc_allocateClassPair(superclass: Id, name: *const c_char, extra_bytes: usize) -> Id;
    fn objc_registerClassPair(class: Id);
    fn class_addMethod(class: Id, name: Sel, imp: *const c_void, types: *const c_char) -> i8;
    fn class_addProtocol(class: Id, protocol: Id) -> i8;
    fn objc_getProtocol(name: *const c_char) -> Id;
    fn objc_msgSend();
    #[cfg(target_arch = "x86_64")]
    fn objc_msgSend_stret();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    static NSPasteboardTypeString: Id;
}

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

/// 读取返回 NSRect 的属性（frame、bounds）。返回 32 字节的结构体，x86_64 上要用 objc_msgSend_stret。
unsafe fn rect_property(object: Id, name: &CStr) -> NSRect {
    unsafe {
        let sel = sel_registerName(name.as_ptr());
        #[cfg(target_arch = "x86_64")]
        {
            let f: unsafe extern "C" fn(*mut NSRect, Id, Sel) =
                std::mem::transmute(objc_msgSend_stret as unsafe extern "C" fn());
            let mut rect = std::mem::zeroed();
            f(&mut rect, object, sel);
            rect
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let f: unsafe extern "C" fn(Id, Sel) -> NSRect =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            f(object, sel)
        }
    }
}

unsafe fn view_frame(view: Id) -> NSRect {
    unsafe { rect_property(view, c"frame") }
}

/// 把矩形从窗口坐标换算成屏幕坐标。返回结构体，x86_64 上同样要用 objc_msgSend_stret。
unsafe fn rect_to_screen(window: Id, rect: NSRect) -> NSRect {
    unsafe {
        let sel = sel_registerName(c"convertRectToScreen:".as_ptr());
        #[cfg(target_arch = "x86_64")]
        {
            let f: unsafe extern "C" fn(*mut NSRect, Id, Sel, NSRect) =
                std::mem::transmute(objc_msgSend_stret as unsafe extern "C" fn());
            let mut out = std::mem::zeroed();
            f(&mut out, window, sel, rect);
            out
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let f: unsafe extern "C" fn(Id, Sel, NSRect) -> NSRect =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            f(window, sel, rect)
        }
    }
}

/// 先查 UTF-8 字节数再创建应用副本；保留内嵌 NUL，不走 C 字符串截断。
/// AppKit 自己的对象/编码缓存不属于这项应用分配上限。
unsafe fn string_from(obj: Id, limit: usize) -> Option<String> {
    unsafe { with_string(obj, limit, str::to_owned) }
}

unsafe fn with_string<T>(obj: Id, limit: usize, consume: impl FnOnce(&str) -> T) -> Option<T> {
    unsafe {
        if obj.is_null() {
            return None;
        }
        let attributed: i8 = msg![obj, "isKindOfClass:", class(c"NSAttributedString") => Id; i8];
        let string: Id = if attributed != 0 { msg![obj, "string"; Id] } else { obj };
        if string.is_null() {
            return None;
        }
        let policy = super::ClipboardRead { limit, strict_unicode: true };
        // UTF-8 字节数至少等于 UTF-16 单元数；明显超限时无需请求 UTF-8 编码。
        let units: usize = msg![string, "length"; usize];
        if units > policy.limit() {
            return None;
        }
        let bytes: usize = msg![string, "lengthOfBytesUsingEncoding:", 4usize => usize; usize];
        if bytes > policy.limit() {
            return None;
        }
        let utf8: *const u8 = msg![string, "UTF8String"; *const u8];
        if utf8.is_null() {
            return None;
        }
        std::str::from_utf8(std::slice::from_raw_parts(utf8, bytes)).ok().map(consume)
    }
}

fn clear_text(mut text: String) {
    wipe_text(&mut text);
}
fn wipe_text(text: &mut str) {
    // 清理尚未交给 App 的应用自有副本；不承诺清理系统输入法对象。
    for byte in unsafe { text.as_bytes_mut() } {
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
}

const NS_EVENT_TYPE_APPLICATION_DEFINED: usize = 15;
const NS_SHIFT: usize = 1 << 17;
const NS_CONTROL: usize = 1 << 18;
const NS_OPTION: usize = 1 << 19;
/// modifierFlags 低位里区分左右 Option 的设备相关位（NX_DEVICELALTKEYMASK / NX_DEVICERALTKEYMASK）
const NS_LEFT_OPTION: usize = 0x20;
const NS_RIGHT_OPTION: usize = 0x40;

/// 按下的 Option 是否按配置当作 Alt
fn option_is_alt(flags: usize) -> bool {
    if flags & NS_OPTION == 0 {
        return false;
    }
    match super::option_as_alt() {
        OptionAsAlt::None => false,
        OptionAsAlt::Left => flags & NS_LEFT_OPTION != 0,
        OptionAsAlt::Right => flags & NS_RIGHT_OPTION != 0,
        OptionAsAlt::Both => true,
    }
}
const NS_COMMAND: usize = 1 << 20;

pub struct Window {
    app: Id,
    window: Id,
    view: Id,
    color_space: *mut c_void,
    width: u32,
    height: u32,
    /// 上次看到的窗口是否是键盘焦点所在（变化时通知 App）
    focused: bool,
    /// 已经设过的窗口 alpha，只在变化时才发消息。
    alpha: Option<f64>,
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

            // 换成我们自己的视图（能接收输入法），并让它由 CALayer 支撑，之后直接设置图层内容
            let view: Id = msg![view_class(), "alloc"; Id];
            let view: Id = msg![view, "initWithFrame:", rect => NSRect; Id];
            msg![window, "setContentView:", view => Id; ()];
            msg![view, "release"; ()];
            msg![view, "setWantsLayer:", 1i8 => i8; ()];
            msg![window, "makeFirstResponder:", view => Id; i8];
            // Cmd+Q、Dock 退出也先返回 Rust 事件循环，执行布局保存及 PTY 清理。
            msg![app, "setDelegate:", view => Id; ()];
            msg![window,"setDelegate:",view=>Id;()];
            // 没按键时的鼠标移动默认不发，程序开了 1003 模式时要用
            msg![window, "setAcceptsMouseMovedEvents:", 1i8 => i8; ()];
            let protocol = objc_getProtocol(c"NSTextInputClient".as_ptr());
            let conforms: i8 = msg![view, "conformsToProtocol:", protocol => Id; i8];
            let context: Id = msg![view, "inputContext"; Id];
            crate::term::trace(format_args!(
                "输入法：NSTextInputClient 协议={} 视图遵守协议={} 输入上下文={}",
                !protocol.is_null(),
                conforms != 0,
                !context.is_null()
            ));

            msg![window, "makeKeyAndOrderFront:", NIL => Id; ()];
            msg![app, "finishLaunching"; ()];
            msg![app, "activateIgnoringOtherApps:", 1i8 => i8; ()];
            objc_autoreleasePoolPop(pool);

            Ok(Window {
                app,
                window,
                view,
                color_space: CGColorSpaceCreateDeviceRGB(),
                width,
                height,
                focused: true,
                alpha: None,
            })
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

            // "编辑"菜单：复制、粘贴。菜单项没有指定目标，命令会发给第一响应者，也就是我们的视图
            let edit_item: Id = msg![msg![class(c"NSMenuItem"), "alloc"; Id], "init"; Id];
            msg![menubar, "addItem:", edit_item => Id; ()];
            let edit_menu: Id = msg![msg![class(c"NSMenu"), "alloc"; Id], "initWithTitle:", nsstring("Edit") => Id; Id];
            for (title, action, key) in [("Copy", c"copy:", ""), ("Paste", c"paste:", "")] {
                let item: Id = msg![msg![class(c"NSMenuItem"), "alloc"; Id],
                    "initWithTitle:action:keyEquivalent:",
                    nsstring(title) => Id,
                    sel_registerName(action.as_ptr()) => Sel,
                    nsstring(key) => Id;
                    Id];
                msg![edit_menu, "addItem:", item => Id; ()];
            }
            msg![edit_item, "setSubmenu:", edit_menu => Id; ()];
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

    /// 整窗 alpha：`NSWindow.setAlphaValue:`，系统合成器直接生效。
    /// 设成半透明后窗口仍可交互，透明度只影响绘制。
    fn apply_opacity(&mut self, opacity: f32) {
        let alpha = f64::from(opacity.clamp(0.0, 1.0));
        if self.alpha == Some(alpha) {
            return;
        }
        self.alpha = Some(alpha);
        unsafe { msg![self.window, "setAlphaValue:", alpha => f64; ()] };
    }

    /// 等到至少一个事件，把排队的事件处理完，然后检查尺寸、重画
    unsafe fn pump(&mut self, app: &mut dyn App, distant_future: Id, distant_past: Id) -> bool {
        unsafe {
            if !self.observe_focus(app)
                || !protected_window::sync(self.view, self.focused, app)
                || !protected_window::drain(self.view, self.focused, app)
            {
                return false;
            }
            // 没有截止时间就一直等到有事件
            let mut until = match app.deadline() {
                Some(deadline) => {
                    let seconds = deadline.saturating_duration_since(std::time::Instant::now()).as_secs_f64();
                    msg![class(c"NSDate"), "dateWithTimeIntervalSinceNow:", seconds => f64; Id]
                }
                None => distant_future,
            };
            let started = std::time::Instant::now();
            let mut events = 0usize;
            loop {
                let event: Id = msg![self.app, "nextEventMatchingMask:untilDate:inMode:dequeue:",
                    u64::MAX => u64, until => Id, NSDefaultRunLoopMode => Id, 1i8 => i8; Id];
                if event.is_null() {
                    break;
                }
                if !self.observe_focus(app) || !protected_window::sync(self.view, self.focused, app) {
                    return false;
                }
                // 第一个事件到了之后，剩下的只取已经排队的，不再等待
                until = distant_past;

                let kind: usize = msg![event, "type"; usize];
                if kind == NS_EVENT_TYPE_APPLICATION_DEFINED {
                    if !app.event(Event::Wake) || !protected_window::sync(self.view, self.focused, app) {
                        return false;
                    }
                } else {
                    // 按键也交给 AppKit：它先检查菜单快捷键（Cmd+Q），再交给视图的 keyDown:
                    msg![self.app, "sendEvent:", event => Id; ()];
                }
                while let Some(e) = IME.with(|s| s.borrow_mut().events.pop_front()) {
                    if !protected_window::plain_event(self.view, e, app)
                        || !protected_window::sync(self.view, self.focused, app)
                    {
                        return false;
                    }
                }
                if !self.observe_focus(app)
                    || !protected_window::sync(self.view, self.focused, app)
                    || !protected_window::drain(self.view, self.focused, app)
                {
                    return false;
                }
                events += 1;
                if protected_window::managed(self.view)
                    && (events >= 64 || started.elapsed() >= std::time::Duration::from_millis(2))
                {
                    break;
                }
            }
            if !app.tick() {
                return false;
            }
            if !self.observe_focus(app)
                || !protected_window::sync(self.view, self.focused, app)
                || !protected_window::drain(self.view, self.focused, app)
            {
                return false;
            }
            msg![self.app, "updateWindows"; ()];

            // 关闭按钮会把窗口藏起来（不是最小化）
            let visible: i8 = msg![self.window, "isVisible"; i8];
            let minimized: i8 = msg![self.window, "isMiniaturized"; i8];
            if visible == 0 && minimized == 0 && !app.event(Event::CloseRequested) {
                return false;
            }
            if !self.observe_focus(app) || !protected_window::sync(self.view, self.focused, app) {
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
            if let Some(opacity) = app.window_opacity() {
                self.apply_opacity(opacity);
            }
            // 图层的内容只能整张替换，所以变化的区域用不上，送整张图
            if app.needs_redraw()
                && let Some((fb, _)) = app.draw()
            {
                self.present(fb);
            }
            if let Some(request) = app.take_clipboard_request() {
                let pasteboard: Id = msg![class(c"NSPasteboard"), "generalPasteboard"; Id];
                match request {
                    ClipboardRequest::Copy(text) => {
                        msg![pasteboard, "clearContents"; isize];
                        let copied = msg![pasteboard, "setString:forType:", nsstring(&text) => Id, NSPasteboardTypeString => Id; i8];
                        if copied == 0 && !app.event(Event::CopyFailed) {
                            return false;
                        }
                    }
                    ClipboardRequest::Paste(mut request) => {
                        let policy = request.policy();
                        let text: Id = msg![pasteboard, "stringForType:", NSPasteboardTypeString => Id; Id];
                        let units = if text.is_null() { 0 } else { msg![text, "length"; usize] };
                        let bytes = if text.is_null() || units > policy.limit() {
                            0
                        } else {
                            msg![text,"lengthOfBytesUsingEncoding:",4usize => usize;usize]
                        };
                        let text = if text.is_null() || units > policy.limit() || bytes > policy.limit() {
                            None
                        } else {
                            let pointer: *const u8 = msg![text,"UTF8String";*const u8];
                            if pointer.is_null() {
                                None
                            } else {
                                request.utf8(std::slice::from_raw_parts(pointer, bytes))
                            }
                        };
                        if !app.event(request.finish(text)) || !protected_window::sync(self.view, self.focused, app) {
                            return false;
                        }
                    }
                }
            }
            self.update_ime_position(app.ime_cursor_area());
            true
        }
    }

    unsafe fn observe_focus(&mut self, app: &mut dyn App) -> bool {
        let focused = unsafe { msg![self.window,"isKeyWindow";i8] } != 0;
        if focused != self.focused {
            if !focused {
                protected_window::blur(self.view);
            }
            self.focused = focused;
            if !app.event(Event::Focus(focused)) {
                return false;
            }
        }
        true
    }

    /// 告诉输入法光标在屏幕上的位置（它会把候选词窗口放在下面）
    unsafe fn update_ime_position(&self, (x, y, w, h): (u32, u32, u32, u32)) {
        unsafe {
            let scale = self.scale_factor();
            let view_height = view_frame(self.view).size.height;
            // 视图坐标原点在左下角，y 轴向上
            let rect = NSRect {
                origin: NSPoint { x: x as f64 / scale, y: view_height - (y + h) as f64 / scale },
                size: NSSize { width: w as f64 / scale, height: h as f64 / scale },
            };
            let screen = rect_to_screen(self.window, rect);
            IME.with(|s| s.borrow_mut().cursor_rect = screen);
            if let protected_window::Lookup::Active(handle) = protected_window::lookup(self.view) {
                handle.set_cursor(screen);
            }
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
            protected_window::reset(self.view);
            msg![self.app, "setDelegate:", NIL => Id; ()];
            msg![self.window,"setDelegate:",NIL=>Id;()];
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
        alt: option_is_alt(flags),
        logo: flags & NS_COMMAND != 0,
    };
    unsafe {
        let code: u16 = msg![event, "keyCode"; u16];
        if let Some(key) = named_key(code) {
            return vec![Event::Key { key, mods }];
        }
        // Option 当作 Alt 用：取不受 Option 影响的字符（否则 Option+f 会变成 ƒ）
        let chars: Id = if mods.alt || mods.logo {
            msg![event, "charactersIgnoringModifiers"; Id]
        } else {
            msg![event, "characters"; Id]
        };
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
        36 => Key::Enter,
        // 小键盘：应用程序键区模式下 input 层改发 SS3，普通模式发数字/运算符
        76 => Key::Keypad(Keypad::Enter),
        65 => Key::Keypad(Keypad::Decimal),
        67 => Key::Keypad(Keypad::Multiply),
        69 => Key::Keypad(Keypad::Add),
        75 => Key::Keypad(Keypad::Divide),
        78 => Key::Keypad(Keypad::Subtract),
        81 => Key::Keypad(Keypad::Equal),
        82 => Key::Keypad(Keypad::Digit(0)),
        83 => Key::Keypad(Keypad::Digit(1)),
        84 => Key::Keypad(Keypad::Digit(2)),
        85 => Key::Keypad(Keypad::Digit(3)),
        86 => Key::Keypad(Keypad::Digit(4)),
        87 => Key::Keypad(Keypad::Digit(5)),
        88 => Key::Keypad(Keypad::Digit(6)),
        89 => Key::Keypad(Keypad::Digit(7)),
        91 => Key::Keypad(Keypad::Digit(8)),
        92 => Key::Keypad(Keypad::Digit(9)),
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

// ---- 输入法（NSTextInputClient） ----

struct ImeState {
    /// 交给 App 的事件
    events: VecDeque<Event>,
    /// Cocoa 只需要组合范围，保留 UTF-16 长度而不另存一份明文。
    marked_utf16: usize,
    /// 正在 keyDown: 里调用 interpretKeyEvents:
    in_key_down: bool,
    /// 这次按键开始时输入法是否已经在输入中
    composing_at_start: bool,
    /// 这次按键被输入法用掉了
    consumed: bool,
    /// 输入法把这个键转成了编辑命令（说明它不要这个键）
    command: bool,
    /// 不在输入中时 insertText: 给出的文字，等 keyDown: 决定怎么发
    pending_text: Option<String>,
    /// 光标的屏幕坐标
    cursor_rect: NSRect,
}

impl ImeState {
    fn clear_pending(&mut self) {
        if let Some(text) = self.pending_text.take() {
            clear_text(text);
        }
    }
}

thread_local! {
    static IME: RefCell<ImeState> = const { RefCell::new(ImeState {
        events: VecDeque::new(),
        marked_utf16: 0,
        in_key_down: false,
        composing_at_start: false,
        consumed: false,
        command: false,
        pending_text: None,
        cursor_rect: NSRect { origin: NSPoint { x: 0.0, y: 0.0 }, size: NSSize { width: 0.0, height: 0.0 } },
    }) };
}

/// 在运行时注册 CongminalView（NSView 的子类），只注册一次
fn view_class() -> Id {
    static CLASS: OnceLock<usize> = OnceLock::new();
    *CLASS.get_or_init(|| unsafe { register_view_class() as usize }) as Id
}

unsafe fn register_view_class() -> Id {
    unsafe {
        let cls = objc_allocateClassPair(class(c"NSView"), c"CongminalView".as_ptr(), 0);
        // 类型编码字符串描述参数类型：v=void c=BOOL @=对象 :=SEL Q=无符号整数 {...}=结构体 ^=指针
        let methods: &[(&CStr, *const c_void, &CStr)] = &[
            (c"acceptsFirstMouse:", accepts_first_mouse as *const c_void, c"c@:@"),
            (c"mouseDown:", mouse_down as *const c_void, c"v@:@"),
            (c"mouseDragged:", mouse_moved as *const c_void, c"v@:@"),
            (c"rightMouseDragged:", mouse_moved as *const c_void, c"v@:@"),
            (c"otherMouseDragged:", mouse_moved as *const c_void, c"v@:@"),
            (c"mouseMoved:", mouse_moved as *const c_void, c"v@:@"),
            (c"mouseUp:", mouse_up as *const c_void, c"v@:@"),
            (c"rightMouseDown:", right_mouse_down as *const c_void, c"v@:@"),
            (c"rightMouseUp:", right_mouse_up as *const c_void, c"v@:@"),
            (c"otherMouseDown:", other_mouse_down as *const c_void, c"v@:@"),
            (c"otherMouseUp:", other_mouse_up as *const c_void, c"v@:@"),
            (c"scrollWheel:", scroll_wheel as *const c_void, c"v@:@"),
            (c"copy:", copy as *const c_void, c"v@:@"),
            (c"paste:", paste as *const c_void, c"v@:@"),
            (c"applicationShouldTerminate:", should_terminate as *const c_void, c"Q@:@"),
            (c"resetCursorRects", reset_cursor_rects as *const c_void, c"v@:"),
            (c"acceptsFirstResponder", accepts_first_responder as *const c_void, c"c@:"),
            (c"keyDown:", key_down as *const c_void, c"v@:@"),
            (c"inputContext", protected_window::input_context as *const c_void, c"@@:"),
            (c"windowDidResignKey:", protected_window::resigned as *const c_void, c"v@:@"),
            (c"windowDidBecomeKey:", protected_window::became as *const c_void, c"v@:@"),
            (c"insertText:replacementRange:", insert_text as *const c_void, c"v@:@{_NSRange=QQ}"),
            (
                c"setMarkedText:selectedRange:replacementRange:",
                set_marked_text as *const c_void,
                c"v@:@{_NSRange=QQ}{_NSRange=QQ}",
            ),
            (c"unmarkText", unmark_text as *const c_void, c"v@:"),
            (c"hasMarkedText", has_marked_text as *const c_void, c"c@:"),
            (c"markedRange", marked_range as *const c_void, c"{_NSRange=QQ}@:"),
            (c"selectedRange", selected_range as *const c_void, c"{_NSRange=QQ}@:"),
            (c"validAttributesForMarkedText", valid_attributes as *const c_void, c"@@:"),
            (
                c"attributedSubstringForProposedRange:actualRange:",
                attributed_substring as *const c_void,
                c"@@:{_NSRange=QQ}^{_NSRange=QQ}",
            ),
            (c"characterIndexForPoint:", character_index as *const c_void, c"Q@:{CGPoint=dd}"),
            (
                c"firstRectForCharacterRange:actualRange:",
                first_rect as *const c_void,
                c"{CGRect={CGPoint=dd}{CGSize=dd}}@:{_NSRange=QQ}^{_NSRange=QQ}",
            ),
            (c"doCommandBySelector:", do_command as *const c_void, c"v@::"),
        ];
        for &(name, imp, types) in methods {
            class_addMethod(cls, sel_registerName(name.as_ptr()), imp, types.as_ptr());
        }
        for name in [c"NSTextInputClient", c"NSWindowDelegate", c"NSApplicationDelegate"] {
            let protocol = objc_getProtocol(name.as_ptr());
            if !protocol.is_null() {
                class_addProtocol(cls, protocol);
            }
        }
        objc_registerClassPair(cls);
        cls
    }
}

extern "C" fn should_terminate(_: Id, _: Sel, _: Id) -> usize {
    crate::term::trace(format_args!("收到系统退出请求，交由 Rust 关闭路径处理"));
    let reply = defer_termination();
    // Dock/AppleEvent 可能在 nextEvent 内回调，必须叫醒正在等待 NSEvent 的循环。
    Waker.wake();
    reply
}
fn defer_termination() -> usize {
    push_event(Event::CloseRequested);
    // 当前应用使用自己的事件循环；Later 会进入 AppKit 的嵌套等待，Rust 无法继续。
    // 取消 AppKit 的直接终止，由 CloseRequested 让 Rust 返回 main 完成保存并退出。
    0 // NSTerminateCancel
}

fn push_event(event: Event) {
    IME.with(|s| s.borrow_mut().events.push_back(event));
}

/// 输入法的回调可能在 keyDown: 之外异步到达（输入法在另一个进程里），这时事件循环可能正睡着，要叫醒它
fn wake_if_async() {
    if IME.with(|s| !s.borrow().in_key_down) {
        Waker.wake();
    }
}

extern "C" fn accepts_first_responder(_: Id, _: Sel) -> i8 {
    1
}

// ---- 鼠标、滚轮、复制粘贴 ----

/// 窗口没激活时点一下，也直接当作点击（而不是只用来激活窗口）
extern "C" fn accepts_first_mouse(_: Id, _: Sel, _: Id) -> i8 {
    1
}

/// 事件位置换算成视图里的物理像素：原点从左下角换到左上角，点换成像素
unsafe fn event_position(view: Id, event: Id) -> (f64, f64) {
    unsafe {
        let point: NSPoint = msg![event, "locationInWindow"; NSPoint];
        let window: Id = msg![view, "window"; Id];
        let scale: f64 = msg![window, "backingScaleFactor"; f64];
        let height = view_frame(view).size.height;
        (point.x * scale, (height - point.y) * scale)
    }
}

/// 鼠标事件的修饰键（Option 按配置决定算不算 Alt）
unsafe fn event_mods(event: Id) -> Mods {
    let flags: usize = unsafe { msg![event, "modifierFlags"; usize] };
    Mods {
        shift: flags & NS_SHIFT != 0,
        ctrl: flags & NS_CONTROL != 0,
        alt: option_is_alt(flags),
        logo: flags & NS_COMMAND != 0,
    }
}

/// 鼠标按下 / 抬起。`button` 为 None 时看事件的按键编号（2 号是中键，其他的不管）
fn push_button(view: Id, event: Id, button: Option<MouseButton>, down: bool) {
    unsafe {
        let button = match button {
            Some(button) => button,
            None if msg![event, "buttonNumber"; isize] == 2 => MouseButton::Middle,
            None => return,
        };
        let (x, y) = event_position(view, event);
        let mods = event_mods(event);
        push_event(if down { Event::MouseDown { button, x, y, mods } } else { Event::MouseUp { button, x, y, mods } });
    }
}

extern "C" fn mouse_down(this: Id, _: Sel, event: Id) {
    push_button(this, event, Some(MouseButton::Left), true);
}

extern "C" fn mouse_up(this: Id, _: Sel, event: Id) {
    push_button(this, event, Some(MouseButton::Left), false);
}

extern "C" fn right_mouse_down(this: Id, _: Sel, event: Id) {
    push_button(this, event, Some(MouseButton::Right), true);
}

extern "C" fn right_mouse_up(this: Id, _: Sel, event: Id) {
    push_button(this, event, Some(MouseButton::Right), false);
}

extern "C" fn other_mouse_down(this: Id, _: Sel, event: Id) {
    push_button(this, event, None, true);
}

extern "C" fn other_mouse_up(this: Id, _: Sel, event: Id) {
    push_button(this, event, None, false);
}

/// 鼠标移动：按着键拖动（mouseDragged: 等）和没按键的移动（mouseMoved:）都走这里
extern "C" fn mouse_moved(this: Id, _: Sel, event: Id) {
    unsafe {
        let (x, y) = event_position(this, event);
        push_event(Event::MouseMove { x, y, mods: event_mods(event) });
    }
}

/// 触控板给出精确的像素距离，鼠标滚轮给出格数
extern "C" fn scroll_wheel(this: Id, _: Sel, event: Id) {
    unsafe {
        let dy: f64 = msg![event, "scrollingDeltaY"; f64];
        let precise: i8 = msg![event, "hasPreciseScrollingDeltas"; i8];
        let delta = if precise != 0 {
            let window: Id = msg![this, "window"; Id];
            let scale: f64 = msg![window, "backingScaleFactor"; f64];
            ScrollDelta::Pixels(dy * scale)
        } else {
            ScrollDelta::Lines(dy)
        };
        let (x, y) = event_position(this, event);
        push_event(Event::Scroll { delta, x, y, mods: event_mods(event) });
    }
}

/// "编辑"菜单的复制（Cmd+C）
extern "C" fn copy(this: Id, _: Sel, _: Id) {
    if protected_window::managed(this) {
        return;
    }
    push_event(Event::Copy);
}

/// "编辑"菜单的粘贴（Cmd+V）
extern "C" fn paste(this: Id, _: Sel, _: Id) {
    if protected_window::paste(this) {
        return;
    }
    push_event(Event::PasteRequest);
}

/// 默认使用箭头，避免侧栏、按钮与终端都显示成文本编辑区域。
extern "C" fn reset_cursor_rects(this: Id, _: Sel) {
    unsafe {
        let bounds = rect_property(this, c"bounds");
        let cursor: Id = msg![class(c"NSCursor"), "arrowCursor"; Id];
        msg![this, "addCursorRect:cursor:", bounds => NSRect, cursor => Id; ()];
    }
}

extern "C" fn key_down(this: Id, _: Sel, event: Id) {
    unsafe {
        let flags: usize = msg![event, "modifierFlags"; usize];
        if protected_window::key_down(this, event, flags) {
            return;
        }
        let code: u16 = msg![event, "keyCode"; u16];
        let composing = IME.with(|s| s.borrow().marked_utf16 != 0);
        // 没在输入拼音时，功能键、Ctrl/Option 组合键直接按终端规则处理，不经过输入法
        if !composing && (named_key(code).is_some() || flags & (NS_CONTROL | NS_COMMAND) != 0 || option_is_alt(flags)) {
            key_events(event, flags).into_iter().for_each(push_event);
            return;
        }

        IME.with(|s| {
            let mut s = s.borrow_mut();
            s.in_key_down = true;
            s.composing_at_start = composing;
            s.consumed = false;
            s.command = false;
            s.clear_pending();
        });
        // 交给输入法。返回值表示输入法（或系统按键绑定）接手了这个键；
        // 它的结果（setMarkedText / insertText）可能就在这次调用里回调，也可能稍后异步到达。
        let context: Id = msg![this, "inputContext"; Id];
        let handled = !context.is_null() && msg![context, "handleEvent:", event => Id; i8] != 0;
        let (consumed, command, pending) = IME.with(|s| {
            let mut s = s.borrow_mut();
            s.in_key_down = false;
            (s.consumed, s.command, s.pending_text.take())
        });
        crate::term::trace(format_args!(
            "输入法 keyDown: 有输入上下文={} handled={handled} 拼音中={composing} \
             consumed={consumed} command={command} 有待交接文字={}",
            !context.is_null(),
            pending.is_some()
        ));

        if consumed || composing {
            if let Some(text) = pending {
                clear_text(text);
            }
            return;
        }
        match pending {
            // 普通文字（包括中文输入法直接给出的全角标点）
            Some(text) => push_event(Event::Text(text)),
            // 输入法接手了，结果会异步到达
            None if handled && !command => {}
            // 输入法不要这个键：按终端规则处理
            None => key_events(event, flags).into_iter().for_each(push_event),
        }
    }
}

/// 拒绝整次超限/无效输入，同时阻止 keyDown 回落为普通按键输入。
fn reject_ime_text() {
    IME.with(|state| {
        let mut state = state.borrow_mut();
        state.consumed = true;
        state.marked_utf16 = 0;
        state.clear_pending();
        state.events.push_back(Event::Preedit(String::new()));
    });
    wake_if_async();
}

extern "C" fn insert_text(this: Id, _: Sel, text: Id, _: NSRange) {
    if protected_window::managed(this) {
        return;
    }
    // 下一回调已取代旧暂存；先释放旧副本，避免两份最大输入重叠。
    IME.with(|state| state.borrow_mut().clear_pending());
    let Some(text) = (unsafe { string_from(text, super::CLIPBOARD_LIMIT) }) else {
        reject_ime_text();
        return;
    };
    let in_key_down = IME.with(|s| {
        let mut s = s.borrow_mut();
        crate::term::trace(format_args!("输入法提交文字，在keyDown中={}", s.in_key_down));
        if s.marked_utf16 != 0 {
            s.marked_utf16 = 0;
            s.events.push_back(Event::Preedit(String::new()));
        }
        if s.in_key_down && !s.composing_at_start {
            s.pending_text = Some(text);
        } else {
            // 输入法选好了字（上屏）
            s.consumed = true;
            s.events.push_back(Event::Text(text));
        }
        s.in_key_down
    });
    if !in_key_down {
        wake_if_async();
    }
}

extern "C" fn set_marked_text(this: Id, _: Sel, text: Id, _: NSRange, _: NSRange) {
    if protected_window::managed(this) {
        return;
    }
    IME.with(|state| state.borrow_mut().clear_pending());
    let Some(text) = (unsafe { string_from(text, crate::search::QUERY_LIMIT) }) else {
        reject_ime_text();
        return;
    };
    IME.with(|s| {
        let mut s = s.borrow_mut();
        crate::term::trace(format_args!("输入法更新组合，在keyDown中={}", s.in_key_down));
        s.consumed = true;
        s.marked_utf16 = text.encode_utf16().count();
        s.events.push_back(Event::Preedit(text));
    });
    wake_if_async();
}

extern "C" fn unmark_text(_: Id, _: Sel) {
    IME.with(|s| {
        let mut s = s.borrow_mut();
        crate::term::trace(format_args!("输入法 unmarkText"));
        if s.marked_utf16 != 0 {
            s.marked_utf16 = 0;
            s.events.push_back(Event::Preedit(String::new()));
        }
    });
    wake_if_async();
}

extern "C" fn has_marked_text(_: Id, _: Sel) -> i8 {
    IME.with(|s| s.borrow().marked_utf16 != 0) as i8
}

extern "C" fn marked_range(_: Id, _: Sel) -> NSRange {
    IME.with(|s| {
        let s = s.borrow();
        if s.marked_utf16 == 0 {
            NSRange { location: NOT_FOUND, length: 0 }
        } else {
            NSRange { location: 0, length: s.marked_utf16 }
        }
    })
}

extern "C" fn selected_range(_: Id, _: Sel) -> NSRange {
    NSRange { location: NOT_FOUND, length: 0 }
}

extern "C" fn valid_attributes(_: Id, _: Sel) -> Id {
    unsafe { msg![class(c"NSArray"), "array"; Id] }
}

extern "C" fn attributed_substring(_: Id, _: Sel, _: NSRange, _: *mut NSRange) -> Id {
    NIL
}

extern "C" fn character_index(_: Id, _: Sel, _: NSPoint) -> usize {
    NOT_FOUND
}

/// 输入法问"文字在屏幕上的哪里"，用来摆放候选词窗口
extern "C" fn first_rect(_: Id, _: Sel, _: NSRange, actual: *mut NSRange) -> NSRect {
    if !actual.is_null() {
        unsafe { *actual = NSRange { location: NOT_FOUND, length: 0 } };
    }
    IME.with(|s| s.borrow().cursor_rect)
}

/// 输入法没用掉的按键会变成编辑命令（insertNewline: 之类），记下来，由 keyDown: 按终端规则处理
extern "C" fn do_command(_: Id, _: Sel, command: Sel) {
    let name = unsafe { CStr::from_ptr(sel_getName(command)) };
    crate::term::trace(format_args!("输入法 doCommandBySelector: {}", name.to_string_lossy()));
    IME.with(|s| s.borrow_mut().command = true);
}

#[cfg(test)]
mod termination_tests {
    use super::*;
    struct Pool(*mut c_void);
    impl Pool {
        fn new() -> Self {
            Self(unsafe { objc_autoreleasePoolPush() })
        }
    }
    impl Drop for Pool {
        fn drop(&mut self) {
            unsafe { objc_autoreleasePoolPop(self.0) };
        }
    }

    #[test]
    fn native_strings_preserve_nul_and_admit_exact_utf8_bytes() {
        let _pool = Pool::new();
        unsafe {
            assert!(string_from(NIL, 1023).is_none());
            assert_eq!(string_from(nsstring(""), 0).as_deref(), Some(""));
            let text = "x".repeat(1020) + "文";
            let object = nsstring(&text);
            assert_eq!(string_from(object, 1023).as_deref(), Some(text.as_str()));
            assert!(string_from(object, 1022).is_none());
            assert_eq!(string_from(nsstring("a\0b"), 3).as_deref(), Some("a\0b"));
            let attributed: Id = msg![class(c"NSAttributedString"), "alloc"; Id];
            let attributed: Id = msg![attributed, "initWithString:", nsstring("😀中") => Id; Id];
            let attributed: Id = msg![attributed, "autorelease"; Id];
            assert_eq!(string_from(attributed, 7).as_deref(), Some("😀中"));
            assert!(string_from(attributed, 6).is_none());
            let oversized = "x".repeat(super::super::CLIPBOARD_LIMIT + 1);
            assert!(string_from(nsstring(&oversized), usize::MAX).is_none());
        }
    }

    #[test]
    fn native_composition_range_and_oversized_callbacks_reject_without_partial_commit() {
        let _pool = Pool::new();
        IME.with(|state| {
            let mut state = state.borrow_mut();
            state.events.clear();
            state.marked_utf16 = 0;
            state.in_key_down = true; // 不在测试线程调用 NSApplication 唤醒路径。
            state.composing_at_start = false;
            state.consumed = false;
            assert!(state.pending_text.is_none());
        });
        set_marked_text(
            NIL,
            std::ptr::null(),
            unsafe { nsstring("😀中") },
            NSRange { location: 0, length: 0 },
            NSRange { location: 0, length: 0 },
        );
        assert_eq!(has_marked_text(NIL, std::ptr::null()), 1);
        let range = marked_range(NIL, std::ptr::null());
        assert_eq!((range.location, range.length), (0, 3));
        IME.with(|state| {
            let mut state = state.borrow_mut();
            assert!(matches!(state.events.pop_front(), Some(Event::Preedit(text)) if text == "😀中"));
        });
        let large = "界".repeat(crate::search::QUERY_LIMIT / 3 + 1);
        set_marked_text(
            NIL,
            std::ptr::null(),
            unsafe { nsstring(&large) },
            NSRange { location: 0, length: 0 },
            NSRange { location: 0, length: 0 },
        );
        assert_eq!(has_marked_text(NIL, std::ptr::null()), 0);
        let range = marked_range(NIL, std::ptr::null());
        assert_eq!((range.location, range.length), (NOT_FOUND, 0));
        IME.with(|state| {
            let mut state = state.borrow_mut();
            assert!(state.consumed);
            assert!(matches!(state.events.pop_front(), Some(Event::Preedit(text)) if text.is_empty()));
            state.consumed = false;
        });
        insert_text(NIL, std::ptr::null(), unsafe { nsstring("待替换") }, NSRange { location: 0, length: 0 });
        let large = "x".repeat(super::super::CLIPBOARD_LIMIT + 1);
        insert_text(NIL, std::ptr::null(), unsafe { nsstring(&large) }, NSRange { location: 0, length: 0 });
        IME.with(|state| {
            let mut state = state.borrow_mut();
            assert!(state.consumed);
            assert!(state.pending_text.is_none());
            assert!(matches!(state.events.pop_front(), Some(Event::Preedit(text)) if text.is_empty()));
            assert!(state.events.is_empty());
            state.in_key_down = false;
            state.consumed = false;
        });
    }

    #[test]
    fn termination_is_deferred_to_the_rust_close_path() {
        IME.with(|state| state.borrow_mut().events.clear());
        assert_eq!(defer_termination(), 0);
        IME.with(|state| {
            let mut state = state.borrow_mut();
            assert!(matches!(state.events.pop_front(), Some(Event::CloseRequested)));
            assert!(state.events.is_empty());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keypad_keycodes_are_distinguished_from_main_keyboard() {
        assert_eq!(named_key(82), Some(Key::Keypad(Keypad::Digit(0))));
        assert_eq!(named_key(92), Some(Key::Keypad(Keypad::Digit(9))));
        assert_eq!(named_key(65), Some(Key::Keypad(Keypad::Decimal)));
        assert_eq!(named_key(67), Some(Key::Keypad(Keypad::Multiply)));
        assert_eq!(named_key(69), Some(Key::Keypad(Keypad::Add)));
        assert_eq!(named_key(75), Some(Key::Keypad(Keypad::Divide)));
        assert_eq!(named_key(78), Some(Key::Keypad(Keypad::Subtract)));
        assert_eq!(named_key(81), Some(Key::Keypad(Keypad::Equal)));
        // 小键盘回车（keycode 76）要和主键盘回车区分开
        assert_eq!(named_key(76), Some(Key::Keypad(Keypad::Enter)));
        assert_eq!(named_key(36), Some(Key::Enter));
        assert_eq!(named_key(0), None);
    }
}

mod protected_ime;

mod protected_window;
