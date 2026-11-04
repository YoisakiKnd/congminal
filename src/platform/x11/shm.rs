//! MIT-SHM 提交：像素经共享内存交给 X 服务器，省掉逐帧经 socket 的拷贝。
//!
//! libXext 在运行时加载，缺库、远程 DISPLAY、扩展不可用、额度不足或附加失败时都退回 XPutImage，
//! 不新增编译期依赖。共享段附加成功后立即标记删除，进程异常退出也不会留在系统里。
use super::{Display, Gc, Visual, XFree, XWindow, Z_PIXMAP};
use crate::render::{Damage, Framebuffer};
use crate::resources::{Kind, Lease};
use std::ffi::{CStr, c_char, c_int, c_uint, c_ulong, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

/// XShmSegmentInfo
#[repr(C)]
struct SegmentInfo {
    shmseg: c_ulong,
    shmid: c_int,
    shmaddr: *mut c_char,
    read_only: c_int,
}

/// XImage 开头的公开字段，只用来核对行距和像素位数与我们的缓冲一致。
#[repr(C)]
struct ImageHead {
    width: c_int,
    height: c_int,
    xoffset: c_int,
    format: c_int,
    data: *mut c_char,
    byte_order: c_int,
    bitmap_unit: c_int,
    bitmap_bit_order: c_int,
    bitmap_pad: c_int,
    depth: c_int,
    bytes_per_line: c_int,
    bits_per_pixel: c_int,
}

type QueryExtension = unsafe extern "C" fn(*mut Display) -> c_int;
type CreateImage = unsafe extern "C" fn(
    *mut Display,
    *mut Visual,
    c_uint,
    c_int,
    *mut c_char,
    *mut SegmentInfo,
    c_uint,
    c_uint,
) -> *mut c_void;
type Attach = unsafe extern "C" fn(*mut Display, *mut SegmentInfo) -> c_int;
type PutImage = unsafe extern "C" fn(
    *mut Display,
    XWindow,
    Gc,
    *mut c_void,
    c_int,
    c_int,
    c_int,
    c_int,
    c_uint,
    c_uint,
    c_int,
) -> c_int;

#[repr(C)]
struct ErrorEvent {
    _opaque: [u8; 0],
}
type ErrorHandler = unsafe extern "C" fn(*mut Display, *mut ErrorEvent) -> c_int;
#[link(name = "X11")]
unsafe extern "C" {
    fn XSetErrorHandler(handler: Option<ErrorHandler>) -> Option<ErrorHandler>;
    fn XSync(display: *mut Display, discard: c_int) -> c_int;
}

struct Api {
    create: CreateImage,
    attach: Attach,
    detach: Attach,
    put: PutImage,
}

struct Segment {
    // Xlib 的图像结构会记住这个地址，放在堆上保证移动 Segment 时不变。
    info: Box<SegmentInfo>,
    pixels: usize,
    _memory: Lease,
}

pub(super) struct Shm {
    api: Option<Api>,
    segment: Option<Segment>,
    /// 上次提交后服务器可能还在读共享段；下次写入前先同步。
    pending: bool,
}

static ATTACH_FAILED: AtomicBool = AtomicBool::new(false);
unsafe extern "C" fn record_error(_: *mut Display, _: *mut ErrorEvent) -> c_int {
    ATTACH_FAILED.store(true, Ordering::Relaxed);
    0
}

fn symbol<T>(library: *mut c_void, name: &CStr) -> Option<T> {
    let address = unsafe { libc::dlsym(library, name.as_ptr()) };
    // 函数指针与数据指针大小相同（POSIX dlsym 的约定）。
    (!address.is_null()).then(|| unsafe { std::mem::transmute_copy(&address) })
}

/// DISPLAY 带主机名时服务器在别处，共享内存不可用。
pub(super) fn local_display() -> bool {
    std::env::var("DISPLAY").is_ok_and(|display| {
        let host = display.split(':').next().unwrap_or("");
        host.is_empty() || host == "unix" || host.starts_with('/')
    })
}

impl Shm {
    pub(super) fn new(display: *mut Display) -> Self {
        let api = Self::load(display);
        crate::term::trace(format_args!(
            "X11 MIT-SHM {}",
            if api.is_some() { "可用" } else { "不可用，使用 XPutImage" }
        ));
        Self { api, segment: None, pending: false }
    }

    fn load(display: *mut Display) -> Option<Api> {
        if std::env::var_os("CONGMINAL_NO_SHM").is_some() || !local_display() {
            return None;
        }
        // 句柄不关闭：函数指针在整个进程期间有效。
        let library = unsafe { libc::dlopen(c"libXext.so.6".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if library.is_null() {
            return None;
        }
        let query: QueryExtension = symbol(library, c"XShmQueryExtension")?;
        let api = Api {
            create: symbol(library, c"XShmCreateImage")?,
            attach: symbol(library, c"XShmAttach")?,
            detach: symbol(library, c"XShmDetach")?,
            put: symbol(library, c"XShmPutImage")?,
        };
        (unsafe { query(display) } != 0).then_some(api)
    }

    /// 关掉共享内存，之后都走 XPutImage。
    fn disable(&mut self, display: *mut Display) {
        self.release(display);
        self.api = None;
    }

    /// 共享段至少能放下 `pixels` 个像素；失败时返回 false，调用方退回 XPutImage。
    fn ensure(&mut self, display: *mut Display, pixels: usize) -> bool {
        // 窗口明显缩小后换成小段，不长期占着最大时的容量。
        if self.segment.as_ref().is_some_and(|segment| segment.pixels >= pixels && segment.pixels / 2 <= pixels) {
            return true;
        }
        self.release(display);
        let Some(api) = &self.api else { return false };
        let Some(bytes) = pixels.checked_mul(4) else { return false };
        let Ok(memory) = crate::resources::current().lease(Kind::Canvas, bytes) else { return false };
        let id = unsafe { libc::shmget(libc::IPC_PRIVATE, bytes, libc::IPC_CREAT | 0o600) };
        if id < 0 {
            self.disable(display);
            return false;
        }
        let address = unsafe { libc::shmat(id, std::ptr::null(), 0) };
        if address as isize == -1 {
            unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) };
            self.disable(display);
            return false;
        }
        let mut info = Box::new(SegmentInfo { shmseg: 0, shmid: id, shmaddr: address.cast(), read_only: 0 });
        // 附加失败（例如服务器不能访问本进程的段）只记录，不让默认错误处理退出进程。
        ATTACH_FAILED.store(false, Ordering::Relaxed);
        let previous = unsafe { XSetErrorHandler(Some(record_error)) };
        let attached = unsafe { (api.attach)(display, &mut *info) } != 0;
        unsafe { XSync(display, 0) };
        unsafe { XSetErrorHandler(previous) };
        // 双方都附加后标记删除：最后一方断开时系统回收，崩溃也不会残留。
        unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) };
        if !attached || ATTACH_FAILED.load(Ordering::Relaxed) {
            unsafe { libc::shmdt(address) };
            self.disable(display);
            return false;
        }
        self.segment = Some(Segment { info, pixels, _memory: memory });
        true
    }

    /// 窗口关闭或尺寸变大前调用：等服务器读完，再断开并释放共享段。
    pub(super) fn release(&mut self, display: *mut Display) {
        let Some(mut segment) = self.segment.take() else { return };
        unsafe {
            XSync(display, 0);
            if let Some(api) = &self.api {
                (api.detach)(display, &mut *segment.info);
                XSync(display, 0);
            }
            libc::shmdt(segment.info.shmaddr.cast());
        }
        self.pending = false;
    }

    /// 按 damage 的各段提交；返回 false 表示没有处理，由调用方用 XPutImage。
    pub(super) fn present(
        &mut self,
        display: *mut Display,
        window: XWindow,
        gc: Gc,
        visual: *mut Visual,
        fb: &Framebuffer,
        damage: &Damage,
    ) -> bool {
        if self.api.is_none() || fb.stride != fb.width || !self.ensure(display, fb.width * fb.height) {
            return false;
        }
        if self.pending {
            // 上一帧的请求处理完之前不能改写共享段，否则会画出撕裂的画面。
            unsafe { XSync(display, 0) };
            self.pending = false;
        }
        let (api, segment) = (self.api.as_ref().unwrap(), self.segment.as_mut().unwrap());
        let image = unsafe {
            (api.create)(
                display,
                visual,
                24,
                Z_PIXMAP,
                segment.info.shmaddr,
                &mut *segment.info,
                fb.width as c_uint,
                fb.height as c_uint,
            )
        };
        if image.is_null() {
            self.disable(display);
            return false;
        }
        let head = unsafe { &*image.cast::<ImageHead>() };
        if head.bytes_per_line as usize != fb.width * 4 || head.bits_per_pixel != 32 {
            unsafe { XFree(image) };
            self.disable(display);
            return false;
        }
        let shared = unsafe { std::slice::from_raw_parts_mut(segment.info.shmaddr.cast::<u32>(), segment.pixels) };
        for band in damage.bands() {
            let y = band.y.min(fb.height);
            let end = band.y.saturating_add(band.height).min(fb.height);
            if y == end {
                continue;
            }
            let range = y * fb.width..end * fb.width;
            shared[range.clone()].copy_from_slice(&fb.pixels[range]);
            unsafe {
                (api.put)(
                    display,
                    window,
                    gc,
                    image,
                    0,
                    y as c_int,
                    0,
                    y as c_int,
                    fb.width as c_uint,
                    (end - y) as c_uint,
                    0,
                );
            }
        }
        // 图像结构只是描述共享段，释放它不影响段本身。
        unsafe { XFree(image) };
        self.pending = true;
        true
    }
}
