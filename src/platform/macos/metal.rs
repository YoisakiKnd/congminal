//! GPU 呈现：Metal + `CAMetalLayer`，按顺序回放绘制记录（`render::list`）。
//!
//! - Metal.framework 在运行时加载，没有设备、着色器编译失败、额度不足、命令缓冲出错都返回错误，调用方退回 CPU
//! - 持久渲染目标与窗口同尺寸（BGRA8，与 CPU 像素同一字节序），第 0 行在最上面，不用翻转；
//!   每帧回放后整张拷到 drawable。图层放在视图图层之上，`presentsWithTransaction` 让它与 CPU 图层同一事务更新
//! - 分块渲染器每开一个渲染 pass 都要加载、存回整张目标，所以上传不打断绘制：两次清空图集/纹理之间
//!   字形槽位不复用、绘制只读已上传的字形，这一段的上传全部提到段首，做成一次 blit
//! - 所有采样都是整数纹素读取；混合只作用于颜色，目标的 alpha 保持 1
use super::*;
use crate::render::list::{ATLAS_SIZE, Atlas, DrawList, Op, Rect};
use crate::resources::{Kind, Lease};

const PIXEL_R8: usize = 10;
const PIXEL_RGBA8: usize = 70;
const PIXEL_BGRA8: usize = 80;
const USAGE_SHADER_READ: usize = 1;
const USAGE_RENDER_TARGET: usize = 4;
const STORAGE_SHARED: usize = 0;
const STORAGE_PRIVATE: usize = 2;
const LOAD: usize = 1;
const CLEAR: usize = 2;
const STORE: usize = 1;
const TRIANGLE_STRIP: usize = 4;
const STATUS_ERROR: usize = 5;
const BLEND_ZERO: usize = 0;
const BLEND_ONE: usize = 1;
const BLEND_SOURCE_ALPHA: usize = 4;
const BLEND_ONE_MINUS_SOURCE_ALPHA: usize = 5;
/// `CAMetalLayer.maximumDrawableCount` 的默认值；drawable 归系统所有，按这个数计入额度
const DRAWABLES: usize = 3;
/// 暂存区里每块数据的起点与行距对齐
const OFFSET_ALIGN: usize = 256;
const ROW_ALIGN: usize = 4;
/// 每个实例 16 个 i32：矩形、三组参数
const INSTANCE: usize = 64;

#[repr(C)]
#[derive(Clone, Copy)]
struct Origin {
    x: usize,
    y: usize,
    z: usize,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Size {
    width: usize,
    height: usize,
    depth: usize,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct ClearColor {
    red: f64,
    green: f64,
    blue: f64,
    alpha: f64,
}

fn origin(x: usize, y: usize) -> Origin {
    Origin { x, y, z: 0 }
}
fn size(width: usize, height: usize) -> Size {
    Size { width, height, depth: 1 }
}
fn align(value: usize, to: usize) -> usize {
    value.div_ceil(to) * to
}

const SHADERS: &str = "#include <metal_stdlib>
using namespace metal;
struct Instance { int4 rect; int4 b; int4 c; int4 d; };
struct Varying {
    float4 position [[position]];
    int4 b [[flat]];
    int4 c [[flat]];
    int4 d [[flat]];
};
vertex Varying vertex_main(uint v [[vertex_id]], uint i [[instance_id]],
                           const device Instance *instances [[buffer(0)]],
                           constant float2 &size [[buffer(1)]]) {
    Instance n = instances[i];
    float2 p = float2((v & 1) ? n.rect.z : n.rect.x, (v & 2) ? n.rect.w : n.rect.y);
    Varying o;
    o.position = float4(p.x / size.x * 2.0 - 1.0, 1.0 - p.y / size.y * 2.0, 0.0, 1.0);
    o.b = n.b;
    o.c = n.c;
    o.d = n.d;
    return o;
}
float3 rgb(int c) {
    return float3(float((c >> 16) & 255), float((c >> 8) & 255), float(c & 255)) / 255.0;
}
// 矩形：b = (颜色, alpha)
fragment float4 rect_main(Varying o [[stage_in]]) {
    return float4(rgb(o.b.x), float(o.b.y) / 255.0);
}
// 灰度字形：b = (字形原点, 图集位置)，c.x = 颜色
fragment float4 alpha_main(Varying o [[stage_in]], texture2d<float, access::read> t [[texture(0)]]) {
    int2 p = int2(o.position.xy) - o.b.xy + o.b.zw;
    return float4(rgb(o.c.x), t.read(uint2(p)).r);
}
// 彩色字形：BGRA 图集，预乘
fragment float4 color_main(Varying o [[stage_in]], texture2d<float, access::read> t [[texture(0)]]) {
    int2 p = int2(o.position.xy) - o.b.xy + o.b.zw;
    return t.read(uint2(p));
}
// 图片：b = (目标原点, 图块内起点)，c.xy = 图块尺寸，d = 裁剪区域；按 Tile::pixel 的整数除法取源像素
fragment float4 image_main(Varying o [[stage_in]], texture2d<float, access::read> t [[texture(0)]]) {
    int2 q = int2(o.position.xy) - o.b.xy + o.b.zw;
    if (any(q < 0) || any(q >= o.c.xy)) discard_fragment();
    return t.read(uint2(o.d.xy + q * o.d.zw / o.c.xy));
}
";

/// 持有一个 +1 引用的 Objective-C 对象
struct Object(Id);
impl Object {
    fn new(id: Id, what: &str) -> Result<Self, String> {
        if id.is_null() { Err(format!("{what}失败")) } else { Ok(Self(id)) }
    }
    /// 自动释放的对象：保留一份
    unsafe fn retain(id: Id) -> Self {
        Self(unsafe { msg![id, "retain"; Id] })
    }
}
impl Drop for Object {
    fn drop(&mut self) {
        unsafe { msg![self.0, "release"; ()] };
    }
}

unsafe fn text(string: Id) -> String {
    unsafe {
        if string.is_null() {
            return String::new();
        }
        let pointer: *const c_char = msg![string, "UTF8String"; *const c_char];
        if pointer.is_null() { String::new() } else { CStr::from_ptr(pointer).to_string_lossy().into_owned() }
    }
}

unsafe fn error_text(error: Id) -> String {
    unsafe { if error.is_null() { "未知错误".into() } else { text(msg![error, "localizedDescription"; Id]) } }
}

type CreateDevice = unsafe extern "C" fn() -> Id;

fn create_device() -> Result<Id, String> {
    // 句柄不关闭：设备与类在整个进程期间有效
    let library = unsafe {
        libc::dlopen(c"/System/Library/Frameworks/Metal.framework/Metal".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL)
    };
    if library.is_null() {
        return Err("无法加载 Metal.framework".into());
    }
    let address = unsafe { libc::dlsym(library, c"MTLCreateSystemDefaultDevice".as_ptr()) };
    if address.is_null() {
        return Err("缺少 MTLCreateSystemDefaultDevice".into());
    }
    // 函数指针与数据指针大小相同（POSIX dlsym 的约定）。
    let create: CreateDevice = unsafe { std::mem::transmute_copy(&address) };
    let device = unsafe { create() };
    if device.is_null() { Err("没有可用的 Metal 设备".into()) } else { Ok(device) }
}

/// GPU 资源的额度；驱动内部开销不计入
const ATLAS_BYTES: [usize; 2] = [(ATLAS_SIZE * ATLAS_SIZE) as usize, (ATLAS_SIZE * ATLAS_SIZE * 4) as usize];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pipeline {
    Rect,
    Alpha,
    Color,
    Image,
}

/// 当前命令缓冲上正在录制的编码器：同一时间只有一个
struct Pass {
    buffer: Id,
    render: Id,
    blit: Id,
}
impl Pass {
    unsafe fn end(&mut self) {
        unsafe {
            for encoder in [&mut self.render, &mut self.blit] {
                if !encoder.is_null() {
                    msg![*encoder, "endEncoding"; ()];
                    *encoder = NIL;
                }
            }
        }
    }
    unsafe fn blit(&mut self) -> Id {
        unsafe {
            if !self.render.is_null() {
                self.end();
            }
            if self.blit.is_null() {
                self.blit = msg![self.buffer, "blitCommandEncoder"; Id];
            }
            self.blit
        }
    }
    unsafe fn render(&mut self, target: Id, load: usize, size: (usize, usize)) -> Id {
        unsafe {
            if !self.blit.is_null() {
                self.end();
            }
            if self.render.is_null() {
                let descriptor: Id = msg![class(c"MTLRenderPassDescriptor"), "renderPassDescriptor"; Id];
                let attachments: Id = msg![descriptor, "colorAttachments"; Id];
                let color: Id = msg![attachments, "objectAtIndexedSubscript:", 0usize => usize; Id];
                msg![color, "setTexture:", target => Id; ()];
                msg![color, "setLoadAction:", load => usize; ()];
                msg![color, "setStoreAction:", STORE => usize; ()];
                let black = ClearColor { red: 0.0, green: 0.0, blue: 0.0, alpha: 1.0 };
                msg![color, "setClearColor:", black => ClearColor; ()];
                self.render = msg![self.buffer, "renderCommandEncoderWithDescriptor:", descriptor => Id; Id];
                let viewport = [size.0 as f32, size.1 as f32];
                msg![self.render, "setVertexBytes:length:atIndex:",
                    viewport.as_ptr().cast::<c_void>() => *const c_void, 8usize => usize, 1usize => usize; ()];
            }
            self.render
        }
    }
}

pub(super) struct Metal {
    device: Object,
    queue: Object,
    pipelines: [Object; 4],
    /// 视图图层之上的 `CAMetalLayer`；测试里没有
    layer: Option<Object>,
    view: Id,
    size: (usize, usize),
    target: Option<Object>,
    scratch: Option<Object>,
    atlases: [Option<Object>; 2],
    textures: Vec<(u64, Object, usize)>,
    /// 上一帧的命令缓冲：下一帧先检查它有没有出错
    last: Option<Object>,
    /// 图层已经呈现过一帧
    shown: bool,
    memory: Lease,
    name: String,
}

impl Metal {
    /// 不带窗口：只能回放和读回（测试用）
    pub(super) fn new() -> Result<Self, String> {
        let memory = crate::resources::current().lease(Kind::Canvas, 0).map_err(|_| "Canvas 额度不足")?;
        unsafe {
            let device = Object(create_device()?);
            let name = text(msg![device.0, "name"; Id]);
            let queue = Object::new(msg![device.0, "newCommandQueue"; Id], "创建命令队列")?;
            let mut error: Id = NIL;
            let library: Id = msg![device.0, "newLibraryWithSource:options:error:",
                nsstring(SHADERS) => Id, NIL => Id, &mut error => *mut Id; Id];
            if library.is_null() {
                return Err(format!("着色器编译失败：{}", error_text(error)));
            }
            let library = Object(library);
            let function = |name: &str| -> Result<Object, String> {
                let function: Id = msg![library.0, "newFunctionWithName:", nsstring(name) => Id; Id];
                Object::new(function, &format!("加载着色器函数 {name} "))
            };
            let vertex = function("vertex_main")?;
            let pipeline = |fragment: &str, source: usize| -> Result<Object, String> {
                let fragment = function(fragment)?;
                let descriptor = Object(msg![msg![class(c"MTLRenderPipelineDescriptor"), "alloc"; Id], "init"; Id]);
                msg![descriptor.0, "setVertexFunction:", vertex.0 => Id; ()];
                msg![descriptor.0, "setFragmentFunction:", fragment.0 => Id; ()];
                let attachments: Id = msg![descriptor.0, "colorAttachments"; Id];
                let color: Id = msg![attachments, "objectAtIndexedSubscript:", 0usize => usize; Id];
                msg![color, "setPixelFormat:", PIXEL_BGRA8 => usize; ()];
                msg![color, "setBlendingEnabled:", 1i8 => i8; ()];
                msg![color, "setSourceRGBBlendFactor:", source => usize; ()];
                msg![color, "setDestinationRGBBlendFactor:", BLEND_ONE_MINUS_SOURCE_ALPHA => usize; ()];
                msg![color, "setSourceAlphaBlendFactor:", BLEND_ZERO => usize; ()];
                msg![color, "setDestinationAlphaBlendFactor:", BLEND_ONE => usize; ()];
                let mut error: Id = NIL;
                let state: Id = msg![device.0, "newRenderPipelineStateWithDescriptor:error:",
                    descriptor.0 => Id, &mut error => *mut Id; Id];
                if state.is_null() {
                    return Err(format!("渲染管线创建失败：{}", error_text(error)));
                }
                Ok(Object(state))
            };
            let pipelines = [
                pipeline("rect_main", BLEND_SOURCE_ALPHA)?,
                pipeline("alpha_main", BLEND_SOURCE_ALPHA)?,
                pipeline("color_main", BLEND_ONE)?,
                pipeline("image_main", BLEND_SOURCE_ALPHA)?,
            ];
            Ok(Self {
                device,
                queue,
                pipelines,
                layer: None,
                view: NIL,
                size: (0, 0),
                target: None,
                scratch: None,
                atlases: [None, None],
                textures: Vec::new(),
                last: None,
                shown: false,
                memory,
                name,
            })
        }
    }

    /// 在视图图层上叠一个 `CAMetalLayer`
    pub(super) fn attach(&mut self, view: Id, color_space: *mut c_void) -> Result<(), String> {
        unsafe {
            let root: Id = msg![view, "layer"; Id];
            if root.is_null() {
                return Err("视图没有图层".into());
            }
            let layer: Id = msg![class(c"CAMetalLayer"), "layer"; Id];
            if layer.is_null() {
                return Err("创建 CAMetalLayer 失败".into());
            }
            let layer = Object::retain(layer);
            msg![layer.0, "setDevice:", self.device.0 => Id; ()];
            msg![layer.0, "setPixelFormat:", PIXEL_BGRA8 => usize; ()];
            // 要把渲染目标 blit 进 drawable
            msg![layer.0, "setFramebufferOnly:", 0i8 => i8; ()];
            // 与 CPU 路径的 CGImage 同一色彩空间，由合成器做同样的色彩管理
            msg![layer.0, "setColorspace:", color_space => *mut c_void; ()];
            msg![layer.0, "setPresentsWithTransaction:", 1i8 => i8; ()];
            msg![root, "addSublayer:", layer.0 => Id; ()];
            self.layer = Some(layer);
            self.view = view;
        }
        Ok(())
    }

    /// 渲染目标、暂存目标各一份画布，drawable 按系统默认个数，加上已创建的图集和纹理
    fn account(&mut self) -> Result<(), String> {
        let (w, h) = self.size;
        let drawables = if self.layer.is_some() { DRAWABLES } else { 0 };
        let atlases: usize = (0..2).filter(|&i| self.atlases[i].is_some()).map(|i| ATLAS_BYTES[i]).sum();
        let textures: usize = self.textures.iter().map(|t| t.2).sum();
        let bytes = w * h * 4 * (2 + drawables) + atlases + textures;
        self.memory.resize(bytes).map_err(|_| "Canvas 额度不足，无法创建 GPU 资源".to_string())
    }

    fn texture(&self, format: usize, width: usize, height: usize, usage: usize) -> Result<Object, String> {
        unsafe {
            let descriptor: Id = msg![class(c"MTLTextureDescriptor"),
                "texture2DDescriptorWithPixelFormat:width:height:mipmapped:",
                format => usize, width => usize, height => usize, 0i8 => i8; Id];
            msg![descriptor, "setUsage:", usage => usize; ()];
            msg![descriptor, "setStorageMode:", STORAGE_PRIVATE => usize; ()];
            Object::new(msg![self.device.0, "newTextureWithDescriptor:", descriptor => Id; Id], "创建纹理")
        }
    }

    fn buffer(&self, bytes: usize) -> Result<Object, String> {
        unsafe {
            let buffer: Id = msg![self.device.0, "newBufferWithLength:options:",
                bytes.max(1) => usize, STORAGE_SHARED => usize; Id];
            Object::new(buffer, "创建缓冲区")
        }
    }

    /// 尺寸变化时重建渲染目标（内容作废，由调用方要求整屏重画）
    fn resize(&mut self, width: usize, height: usize, pass: &mut Pass) -> Result<bool, String> {
        if self.size == (width, height) && self.target.is_some() {
            return Ok(false);
        }
        self.target = None;
        self.scratch = None;
        self.size = (width, height);
        self.account()?;
        self.target = Some(self.texture(PIXEL_BGRA8, width, height, USAGE_RENDER_TARGET | USAGE_SHADER_READ)?);
        self.scratch = Some(self.texture(PIXEL_BGRA8, width, height, USAGE_SHADER_READ)?);
        unsafe {
            pass.render(self.target.as_ref().unwrap().0, CLEAR, self.size);
            pass.end();
            if let Some(layer) = &self.layer {
                let transaction = class(c"CATransaction");
                let actions: i8 = msg![transaction, "disableActions"; i8];
                msg![transaction, "setDisableActions:", 1i8 => i8; ()];
                let bounds = rect_property(self.view, c"bounds");
                msg![layer.0, "setFrame:", bounds => NSRect; ()];
                if bounds.size.width > 0.0 {
                    msg![layer.0, "setContentsScale:", width as f64 / bounds.size.width => f64; ()];
                }
                let drawable = NSSize { width: width as f64, height: height as f64 };
                msg![layer.0, "setDrawableSize:", drawable => NSSize; ()];
                msg![transaction, "setDisableActions:", actions => i8; ()];
            }
        }
        Ok(true)
    }

    fn atlas(&mut self, atlas: Atlas) -> Result<Id, String> {
        let index = atlas as usize;
        if let Some(texture) = &self.atlases[index] {
            return Ok(texture.0);
        }
        let format = if atlas == Atlas::Alpha { PIXEL_R8 } else { PIXEL_BGRA8 };
        let side = ATLAS_SIZE as usize;
        let texture = self.texture(format, side, side, USAGE_SHADER_READ)?;
        let id = texture.0;
        self.atlases[index] = Some(texture);
        if let Err(error) = self.account() {
            self.atlases[index] = None;
            return Err(error);
        }
        Ok(id)
    }

    /// 上一帧的命令缓冲出错（设备丢失、超时）时报告
    fn check(&mut self) -> Result<(), String> {
        let Some(last) = self.last.take() else { return Ok(()) };
        unsafe {
            let status: usize = msg![last.0, "status"; usize];
            if status == STATUS_ERROR {
                return Err(format!("命令缓冲出错：{}", error_text(msg![last.0, "error"; Id])));
            }
        }
        Ok(())
    }

    /// 回放一帧并呈现。返回 Ok(true) 表示渲染目标是新建的，调用方要让应用整屏重画。
    fn present(&mut self, list: &DrawList, width: usize, height: usize) -> Result<bool, String> {
        self.check()?;
        unsafe {
            let buffer: Id = msg![self.queue.0, "commandBuffer"; Id];
            if buffer.is_null() {
                return Err("创建命令缓冲失败".into());
            }
            let mut pass = Pass { buffer, render: NIL, blit: NIL };
            let result = self.encode(list, width, height, &mut pass);
            pass.end();
            // 出错时不提交半帧；没提交的命令缓冲随自动释放池释放
            let recreated = result?;
            let drawable = match &self.layer {
                Some(layer) => msg![layer.0, "nextDrawable"; Id],
                None => NIL,
            };
            if !drawable.is_null() {
                let texture: Id = msg![drawable, "texture"; Id];
                let w = width.min(msg![texture, "width"; usize]);
                let h = height.min(msg![texture, "height"; usize]);
                let blit = pass.blit();
                msg![blit, "copyFromTexture:sourceSlice:sourceLevel:sourceOrigin:sourceSize:toTexture:destinationSlice:destinationLevel:destinationOrigin:",
                    self.target.as_ref().unwrap().0 => Id, 0usize => usize, 0usize => usize, origin(0, 0) => Origin,
                    size(w, h) => Size, texture => Id, 0usize => usize, 0usize => usize, origin(0, 0) => Origin; ()];
                pass.end();
            } else if self.layer.is_some() {
                crate::term::trace(format_args!("renderer: nextDrawable 超时，这一帧没有呈现"));
            }
            msg![buffer, "commit"; ()];
            if !drawable.is_null() {
                // presentsWithTransaction：排进 GPU 之后在当前事务里呈现，与其他图层的变化同时生效
                msg![buffer, "waitUntilScheduled"; ()];
                msg![drawable, "present"; ()];
                if !self.shown {
                    // 有了第一帧再设不透明，之前透出下面的 CPU 画面
                    self.shown = true;
                    msg![self.layer.as_ref().unwrap().0, "setOpaque:", 1i8 => i8; ()];
                }
            }
            self.last = Some(Object::retain(buffer));
            Ok(recreated)
        }
    }

    fn encode(&mut self, list: &DrawList, width: usize, height: usize, pass: &mut Pass) -> Result<bool, String> {
        let recreated = self.resize(width, height, pass)?;
        if list.is_replayed() {
            return Ok(recreated);
        }
        let ops = list.ops();
        // 暂存区：上传按段提到段首，每块起点和行距对齐
        let mut staging_bytes = 0;
        let mut instances = 0;
        for op in ops {
            match *op {
                Op::Upload { slot, .. } => {
                    let pixel = if slot.atlas == Atlas::Alpha { 1 } else { 4 };
                    staging_bytes = align(staging_bytes, OFFSET_ALIGN)
                        + align(slot.w as usize * pixel, ROW_ALIGN) * slot.h as usize;
                }
                Op::Texture { width, height, .. } => {
                    staging_bytes = align(staging_bytes, OFFSET_ALIGN) + width as usize * height as usize * 4;
                }
                Op::Fill { .. } | Op::Blend { .. } | Op::Glyph { .. } | Op::Image { .. } => instances += 1,
                _ => {}
            }
        }
        let staging = self.buffer(staging_bytes)?;
        let instance_buffer = self.buffer(instances * INSTANCE)?;
        let mut replay = Replay {
            staging: unsafe { msg![staging.0, "contents"; *mut u8] },
            staging_id: staging.0,
            staging_at: 0,
            instances: unsafe { msg![instance_buffer.0, "contents"; *mut [i32; 16]] },
            instance_id: instance_buffer.0,
            count: 0,
            batch: None,
        };
        let mut start = 0;
        while start < ops.len() {
            // 一段：到下一次清空图集/纹理为止（清空本身属于下一段的开头）
            let end = ops[start + 1..]
                .iter()
                .position(|op| matches!(op, Op::ResetAtlas(_) | Op::ResetTextures))
                .map_or(ops.len(), |at| start + 1 + at);
            self.upload(&ops[start..end], list, pass, &mut replay)?;
            self.draw(&ops[start..end], pass, &mut replay);
            start = end;
        }
        unsafe { replay.flush(self, pass) };
        list.mark_replayed();
        Ok(recreated)
    }

    /// 一段里的全部上传：一次 blit（`pass.blit()` 先结束上一段的渲染编码器）
    fn upload(&mut self, ops: &[Op], list: &DrawList, pass: &mut Pass, replay: &mut Replay) -> Result<(), String> {
        if ops.iter().any(|op| matches!(op, Op::Upload { .. } | Op::Texture { .. } | Op::ResetTextures)) {
            unsafe { replay.flush(self, pass) };
        }
        for op in ops {
            match *op {
                Op::ResetTextures => {
                    self.textures.clear();
                    // 缩小额度不会失败
                    self.account()?;
                }
                Op::Upload { slot, start } => {
                    let texture = self.atlas(slot.atlas)?;
                    let (w, h) = (slot.w as usize, slot.h as usize);
                    let (pitch, offset) = match slot.atlas {
                        Atlas::Alpha => {
                            let pitch = align(w, ROW_ALIGN);
                            let offset = replay.reserve(pitch * h);
                            for row in 0..h {
                                let source = &list.alpha()[start + row * w..start + (row + 1) * w];
                                unsafe { replay.write(offset + row * pitch, source) };
                            }
                            (pitch, offset)
                        }
                        Atlas::Color => {
                            let offset = replay.reserve(w * 4 * h);
                            let source = &list.color()[start..start + w * h];
                            let bytes = unsafe { std::slice::from_raw_parts(source.as_ptr().cast::<u8>(), w * h * 4) };
                            unsafe { replay.write(offset, bytes) };
                            (w * 4, offset)
                        }
                    };
                    unsafe { replay.copy(pass, offset, pitch, (w, h), texture, (slot.x as usize, slot.y as usize)) };
                }
                Op::Texture { serial, width, height, start } => {
                    self.textures.retain(|t| t.0 != serial);
                    let (w, h) = (width as usize, height as usize);
                    let texture = self.texture(PIXEL_RGBA8, w, h, USAGE_SHADER_READ)?;
                    let id = texture.0;
                    self.textures.push((serial, texture, w * h * 4));
                    if let Err(error) = self.account() {
                        self.textures.pop();
                        return Err(error);
                    }
                    let offset = replay.reserve(w * h * 4);
                    unsafe {
                        replay.write(offset, &list.rgba()[start..start + w * h * 4]);
                        replay.copy(pass, offset, w * 4, (w, h), id, (0, 0));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// 一段里的绘制与平移，按顺序
    fn draw(&mut self, ops: &[Op], pass: &mut Pass, replay: &mut Replay) {
        for op in ops {
            match *op {
                Op::Fill { rect, color } => replay.queue(
                    self,
                    pass,
                    Pipeline::Rect,
                    NIL,
                    [rect4(rect), [color as i32, 255, 0, 0], [0; 4], [0; 4]],
                ),
                Op::Blend { rect, color, alpha } => replay.queue(
                    self,
                    pass,
                    Pipeline::Rect,
                    NIL,
                    [rect4(rect), [color as i32, i32::from(alpha), 0, 0], [0; 4], [0; 4]],
                ),
                Op::Glyph { x, y, slot, color, clip } => {
                    let (pipeline, atlas) = match slot.atlas {
                        Atlas::Alpha => (Pipeline::Alpha, &self.atlases[0]),
                        Atlas::Color => (Pipeline::Color, &self.atlases[1]),
                    };
                    let Some(atlas) = atlas.as_ref().map(|a| a.0) else { continue };
                    let b = [x, y, slot.x as i32, slot.y as i32];
                    replay.queue(self, pass, pipeline, atlas, [rect4(clip), b, [color as i32, 0, 0, 0], [0; 4]]);
                }
                Op::Image { serial, crop, tile, source, x, y, clip } => {
                    let Some(texture) = self.textures.iter().find(|t| t.0 == serial).map(|t| t.1.0) else { continue };
                    let b = [x, y, source.0, source.1];
                    let c = [tile.0 as i32, tile.1 as i32, 0, 0];
                    replay.queue(self, pass, Pipeline::Image, texture, [rect4(clip), b, c, crop.map(|v| v as i32)]);
                }
                Op::Shift { rect, dy } => {
                    replay.flush_now(self, pass);
                    unsafe { self.shift(rect, dy, pass) };
                }
                Op::Upload { .. } | Op::Texture { .. } | Op::ResetAtlas(_) | Op::ResetTextures => {}
            }
        }
    }

    /// 区域内垂直平移：先拷到暂存目标，再拷回偏移后的位置（同一纹理内的重叠拷贝没有定义）。
    /// 两次拷贝分属两个 blit 编码器，Metal 保证前一个写完后一个才读。
    unsafe fn shift(&self, rect: Rect, dy: i32, pass: &mut Pass) {
        let rows = (rect.y1 - rect.y0) as usize;
        let k = dy.unsigned_abs() as usize;
        if k >= rows {
            return;
        }
        let (x, w) = (rect.x0 as usize, (rect.x1 - rect.x0) as usize);
        let y0 = rect.y0 as usize;
        let (from, to) = if dy > 0 { (y0, y0 + k) } else { (y0 + k, y0) };
        let (target, scratch) = (self.target.as_ref().unwrap().0, self.scratch.as_ref().unwrap().0);
        unsafe {
            let copy = |pass: &mut Pass, source: Id, at: usize, destination: Id, to: usize| {
                let blit = pass.blit();
                msg![blit, "copyFromTexture:sourceSlice:sourceLevel:sourceOrigin:sourceSize:toTexture:destinationSlice:destinationLevel:destinationOrigin:",
                    source => Id, 0usize => usize, 0usize => usize, origin(x, at) => Origin, size(w, rows - k) => Size,
                    destination => Id, 0usize => usize, 0usize => usize, origin(x, to) => Origin; ()];
                pass.end();
            };
            copy(pass, target, from, scratch, from);
            copy(pass, scratch, from, target, to);
        }
    }

    /// 读回渲染目标，格式与 CPU 画布相同（0x00RRGGBB，第 0 行在最上面）
    #[cfg(test)]
    pub(super) fn read(&self) -> Vec<u32> {
        let (w, h) = self.size;
        let buffer = self.buffer(w * h * 4).unwrap();
        unsafe {
            let command: Id = msg![self.queue.0, "commandBuffer"; Id];
            let blit: Id = msg![command, "blitCommandEncoder"; Id];
            msg![blit, "copyFromTexture:sourceSlice:sourceLevel:sourceOrigin:sourceSize:toBuffer:destinationOffset:destinationBytesPerRow:destinationBytesPerImage:",
                self.target.as_ref().unwrap().0 => Id, 0usize => usize, 0usize => usize, origin(0, 0) => Origin,
                size(w, h) => Size, buffer.0 => Id, 0usize => usize, w * 4 => usize, w * h * 4 => usize; ()];
            msg![blit, "endEncoding"; ()];
            msg![command, "commit"; ()];
            msg![command, "waitUntilCompleted"; ()];
            let status: usize = msg![command, "status"; usize];
            assert_ne!(status, STATUS_ERROR, "读回失败：{}", error_text(msg![command, "error"; Id]));
            let pixels = std::slice::from_raw_parts(msg![buffer.0, "contents"; *const u32], w * h);
            pixels.iter().map(|p| p & 0xff_ffff).collect()
        }
    }
}

fn rect4(rect: Rect) -> [i32; 4] {
    [rect.x0 as i32, rect.y0 as i32, rect.x1 as i32, rect.y1 as i32]
}

/// 一帧回放的状态：暂存区写到哪、实例写到哪、正在攒的批次
struct Replay {
    staging: *mut u8,
    staging_id: Id,
    staging_at: usize,
    instances: *mut [i32; 16],
    instance_id: Id,
    count: usize,
    /// (管线, 纹理, 第一个实例, 实例数)
    batch: Option<(Pipeline, Id, usize, usize)>,
}

impl Replay {
    fn reserve(&mut self, bytes: usize) -> usize {
        let offset = align(self.staging_at, OFFSET_ALIGN);
        self.staging_at = offset + bytes;
        offset
    }
    /// 预先按同样的规则算过总大小，写入不会越界
    unsafe fn write(&mut self, offset: usize, bytes: &[u8]) {
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.staging.add(offset), bytes.len()) };
    }
    unsafe fn copy(
        &mut self,
        pass: &mut Pass,
        offset: usize,
        pitch: usize,
        (w, h): (usize, usize),
        texture: Id,
        (x, y): (usize, usize),
    ) {
        unsafe {
            let blit = pass.blit();
            msg![blit, "copyFromBuffer:sourceOffset:sourceBytesPerRow:sourceBytesPerImage:sourceSize:toTexture:destinationSlice:destinationLevel:destinationOrigin:",
                self.staging_id => Id, offset => usize, pitch => usize, pitch * h => usize, size(w, h) => Size,
                texture => Id, 0usize => usize, 0usize => usize, origin(x, y) => Origin; ()];
        }
    }
    fn queue(&mut self, metal: &Metal, pass: &mut Pass, pipeline: Pipeline, texture: Id, instance: [[i32; 4]; 4]) {
        if let Some((current, bound, _, _)) = self.batch
            && (current != pipeline || bound != texture)
        {
            unsafe { self.flush(metal, pass) };
        }
        let flat: [i32; 16] = instance.as_flattened().try_into().unwrap();
        unsafe { self.instances.add(self.count).write(flat) };
        match &mut self.batch {
            Some((_, _, _, count)) => *count += 1,
            None => self.batch = Some((pipeline, texture, self.count, 1)),
        }
        self.count += 1;
    }
    /// 平移、上传之前：先把攒着的绘制交给渲染编码器，再结束它
    fn flush_now(&mut self, metal: &Metal, pass: &mut Pass) {
        unsafe {
            self.flush(metal, pass);
            pass.end();
        }
    }
    unsafe fn flush(&mut self, metal: &Metal, pass: &mut Pass) {
        let Some((pipeline, texture, first, count)) = self.batch.take() else { return };
        unsafe {
            let encoder = pass.render(metal.target.as_ref().unwrap().0, LOAD, metal.size);
            msg![encoder, "setRenderPipelineState:", metal.pipelines[pipeline as usize].0 => Id; ()];
            if !texture.is_null() {
                msg![encoder, "setFragmentTexture:atIndex:", texture => Id, 0usize => usize; ()];
            }
            msg![encoder, "setVertexBuffer:offset:atIndex:", self.instance_id => Id, first * INSTANCE => usize, 0usize => usize; ()];
            msg![encoder, "drawPrimitives:vertexStart:vertexCount:instanceCount:",
                TRIANGLE_STRIP => usize, 0usize => usize, 4usize => usize, count => usize; ()];
        }
    }
}

impl super::super::gpu::Backend for Metal {
    fn name(&self) -> &str {
        &self.name
    }
    fn software(&self) -> bool {
        // macOS 没有常见的软件 Metal 实现；名字里写明的才算
        self.name.to_ascii_lowercase().contains("software")
    }
    fn present(&mut self, list: &DrawList, width: usize, height: usize) -> Result<bool, String> {
        Metal::present(self, list, width, height)
    }
}

impl Drop for Metal {
    fn drop(&mut self) {
        // 等 GPU 用完再释放资源（命令缓冲自己也持有引用，这里只是让退回 CPU 时不留半帧）
        if let Some(last) = self.last.take() {
            unsafe { msg![last.0, "waitUntilCompleted"; ()] };
        }
        if let Some(layer) = self.layer.take() {
            unsafe {
                let transaction = class(c"CATransaction");
                let actions: i8 = msg![transaction, "disableActions"; i8];
                msg![transaction, "setDisableActions:", 1i8 => i8; ()];
                msg![layer.0, "removeFromSuperlayer"; ()];
                msg![transaction, "setDisableActions:", actions => i8; ()];
            }
        }
    }
}

/// 不需要窗口：在 `cargo test` 的线程上直接创建设备回放
#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::list::tests::{GpuTarget, gpu_scenarios};

    impl GpuTarget for Metal {
        fn replay(&mut self, list: &DrawList, width: usize, height: usize) {
            Metal::present(self, list, width, height).unwrap();
        }
        fn read(&mut self, width: usize, height: usize) -> Option<Vec<u32>> {
            (self.target.is_some() && self.size == (width, height)).then(|| Metal::read(self))
        }
    }

    #[test]
    #[ignore = "需要 Metal 设备"]
    fn metal_replay_matches_cpu_within_one_per_channel() {
        unsafe {
            let pool = objc_autoreleasePoolPush();
            let mut metal = match Metal::new() {
                Ok(metal) => metal,
                // CI 的虚拟机可能没有 Metal 设备；真机上必须有
                Err(error) if std::env::var_os("CONGMINAL_GPU_TEST_OPTIONAL").is_some() => {
                    println!("::notice::Metal 不可用，跳过 GPU 对照：{error}");
                    objc_autoreleasePoolPop(pool);
                    return;
                }
                Err(error) => panic!("Metal 不可用：{error}"),
            };
            let device = format!("Metal：{}", metal.name);
            gpu_scenarios(&device, &mut metal);
            drop(metal);
            objc_autoreleasePoolPop(pool);
        }
    }
}
