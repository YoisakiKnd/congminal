//! GPU 呈现：Direct3D 11 + DXGI 翻转模型交换链，按顺序回放绘制记录（`render::list`）。
//!
//! - `d3d11.dll`、`d3dcompiler_47.dll` 在运行时加载，没有设备、着色器编译失败、额度不足、设备丢失都返回错误，
//!   调用方退回 CPU。DXGI 工厂从设备取，不另外加载 `dxgi.dll`
//! - COM 方法按虚函数表下标调用，下标见各 `mod` 里的常量（与 d3d11.h、dxgi.h 的声明顺序一致）
//! - 持久渲染目标与窗口同尺寸（BGRA8，与 CPU 像素同一字节序），第 0 行在最上面，不用翻转；
//!   每帧回放后整张拷进交换链的后台缓冲再 Present
//! - 立即上下文按提交顺序执行，`UpdateSubresource` 上传夹在绘制之间也保持顺序（与 GLES 相同）
//! - 所有采样都是整数纹素读取；混合只作用于颜色，目标的 alpha 保持 1
use crate::render::list::{ATLAS_SIZE, Atlas, DrawList, Op, Rect};
use crate::resources::{Kind, Lease};
use std::ffi::{CStr, c_char, c_void};
use std::ptr::{null, null_mut};

type Hresult = i32;
type Raw = *mut c_void;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(name: *const u16, file: isize, flags: u32) -> isize;
    fn GetProcAddress(module: isize, name: *const u8) -> *mut c_void;
}

/// IUnknown
mod unknown {
    pub const QUERY_INTERFACE: usize = 0;
    pub const RELEASE: usize = 2;
}
/// ID3D11Device
mod device {
    pub const CREATE_BUFFER: usize = 3;
    pub const CREATE_TEXTURE_2D: usize = 5;
    pub const CREATE_SHADER_RESOURCE_VIEW: usize = 7;
    pub const CREATE_RENDER_TARGET_VIEW: usize = 9;
    pub const CREATE_INPUT_LAYOUT: usize = 11;
    pub const CREATE_VERTEX_SHADER: usize = 12;
    pub const CREATE_PIXEL_SHADER: usize = 15;
    pub const CREATE_BLEND_STATE: usize = 20;
    pub const CREATE_RASTERIZER_STATE: usize = 22;
    pub const GET_DEVICE_REMOVED_REASON: usize = 39;
}
/// ID3D11DeviceContext
mod context {
    pub const PS_SET_SHADER_RESOURCES: usize = 8;
    pub const PS_SET_SHADER: usize = 9;
    pub const VS_SET_SHADER: usize = 11;
    pub const MAP: usize = 14;
    pub const UNMAP: usize = 15;
    pub const VS_SET_CONSTANT_BUFFERS: usize = 7;
    pub const IA_SET_INPUT_LAYOUT: usize = 17;
    pub const IA_SET_VERTEX_BUFFERS: usize = 18;
    pub const DRAW_INSTANCED: usize = 21;
    pub const IA_SET_PRIMITIVE_TOPOLOGY: usize = 24;
    pub const OM_SET_RENDER_TARGETS: usize = 33;
    pub const OM_SET_BLEND_STATE: usize = 35;
    pub const RS_SET_STATE: usize = 43;
    pub const RS_SET_VIEWPORTS: usize = 44;
    pub const COPY_SUBRESOURCE_REGION: usize = 46;
    #[cfg(test)]
    pub const COPY_RESOURCE: usize = 47;
    pub const UPDATE_SUBRESOURCE: usize = 48;
    pub const CLEAR_RENDER_TARGET_VIEW: usize = 50;
    pub const FLUSH: usize = 111;
}
/// IDXGIObject / IDXGIDevice / IDXGIAdapter / IDXGIFactory2 / IDXGISwapChain
mod dxgi {
    pub const GET_PARENT: usize = 6;
    pub const DEVICE_GET_ADAPTER: usize = 7;
    pub const ADAPTER_GET_DESC: usize = 8;
    pub const FACTORY_MAKE_WINDOW_ASSOCIATION: usize = 8;
    pub const FACTORY2_CREATE_SWAP_CHAIN_FOR_HWND: usize = 15;
    pub const SWAP_CHAIN_PRESENT: usize = 8;
    pub const SWAP_CHAIN_GET_BUFFER: usize = 9;
    pub const SWAP_CHAIN_RESIZE_BUFFERS: usize = 13;
}
/// ID3DBlob
mod blob {
    pub const GET_BUFFER_POINTER: usize = 3;
    pub const GET_BUFFER_SIZE: usize = 4;
}

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);
const IID_IDXGI_DEVICE: Guid = Guid(0x54ec77fa, 0x1377, 0x44e6, [0x8c, 0x32, 0x88, 0xfd, 0x5f, 0x44, 0xc8, 0x4c]);
const IID_IDXGI_FACTORY2: Guid = Guid(0x50c83a1c, 0xe072, 0x4c48, [0x87, 0xb0, 0x36, 0x30, 0xfa, 0x36, 0xa6, 0xd0]);
const IID_ID3D11_TEXTURE2D: Guid = Guid(0x6f15aaf2, 0xd208, 0x4e89, [0x9a, 0xb4, 0x48, 0x95, 0x35, 0xd3, 0x4f, 0x9c]);

const FORMAT_UNKNOWN: u32 = 0;
const FORMAT_RGBA8: u32 = 28;
const FORMAT_R8: u32 = 61;
const FORMAT_BGRA8: u32 = 87;
const FORMAT_R32G32B32A32_SINT: u32 = 4;
const BIND_VERTEX_BUFFER: u32 = 0x1;
const BIND_CONSTANT_BUFFER: u32 = 0x4;
const BIND_SHADER_RESOURCE: u32 = 0x8;
const BIND_RENDER_TARGET: u32 = 0x20;
const USAGE_DEFAULT: u32 = 0;
const USAGE_DYNAMIC: u32 = 2;
#[cfg(test)]
const USAGE_STAGING: u32 = 3;
const CPU_ACCESS_WRITE: u32 = 0x10000;
#[cfg(test)]
const CPU_ACCESS_READ: u32 = 0x20000;
const MAP_WRITE_DISCARD: u32 = 4;
#[cfg(test)]
const MAP_READ: u32 = 1;
const TOPOLOGY_TRIANGLE_STRIP: u32 = 5;
const BLEND_ZERO: u32 = 1;
const BLEND_ONE: u32 = 2;
const BLEND_SRC_ALPHA: u32 = 5;
const BLEND_INV_SRC_ALPHA: u32 = 6;
const BLEND_OP_ADD: u32 = 1;
const DRIVER_TYPE_HARDWARE: u32 = 1;
#[cfg(test)]
const DRIVER_TYPE_WARP: u32 = 5;
const CREATE_DEVICE_BGRA_SUPPORT: u32 = 0x20;
const SDK_VERSION: u32 = 7;
const FEATURE_LEVELS: [u32; 3] = [0xb000, 0xa100, 0xa000];
const USAGE_RENDER_TARGET_OUTPUT: u32 = 0x20;
const SWAP_EFFECT_FLIP_SEQUENTIAL: u32 = 3;
const SWAP_BUFFERS: u32 = 2;
/// DXGI 不替我们处理 Alt+Enter 全屏
const MWA_NO_ALT_ENTER: u32 = 2;
/// WARP（Microsoft Basic Render Driver）
const WARP_VENDOR: u32 = 0x1414;
/// 每个实例 16 个 i32：矩形、三组参数
const INSTANCE: usize = 64;

#[repr(C)]
struct SampleDesc {
    count: u32,
    quality: u32,
}
#[repr(C)]
struct Texture2dDesc {
    width: u32,
    height: u32,
    mip_levels: u32,
    array_size: u32,
    format: u32,
    sample: SampleDesc,
    usage: u32,
    bind_flags: u32,
    cpu_access: u32,
    misc: u32,
}
#[repr(C)]
struct BufferDesc {
    bytes: u32,
    usage: u32,
    bind_flags: u32,
    cpu_access: u32,
    misc: u32,
    stride: u32,
}
#[repr(C)]
struct Mapped {
    data: *mut c_void,
    row_pitch: u32,
    depth_pitch: u32,
}
#[repr(C)]
struct Box3 {
    left: u32,
    top: u32,
    front: u32,
    right: u32,
    bottom: u32,
    back: u32,
}
#[repr(C)]
struct Viewport {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    min_depth: f32,
    max_depth: f32,
}
#[repr(C)]
struct InputElement {
    semantic: *const c_char,
    index: u32,
    format: u32,
    slot: u32,
    offset: u32,
    class: u32,
    step: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct TargetBlend {
    enable: i32,
    source: u32,
    destination: u32,
    operation: u32,
    source_alpha: u32,
    destination_alpha: u32,
    operation_alpha: u32,
    write_mask: u8,
}
#[repr(C)]
struct BlendDesc {
    alpha_to_coverage: i32,
    independent: i32,
    targets: [TargetBlend; 8],
}
#[repr(C)]
struct RasterizerDesc {
    fill: u32,
    cull: u32,
    front_counter_clockwise: i32,
    depth_bias: i32,
    depth_bias_clamp: f32,
    slope_scaled_depth_bias: f32,
    depth_clip: i32,
    scissor: i32,
    multisample: i32,
    antialiased_line: i32,
}
#[repr(C)]
struct SwapChainDesc1 {
    width: u32,
    height: u32,
    format: u32,
    stereo: i32,
    sample: SampleDesc,
    usage: u32,
    buffers: u32,
    scaling: u32,
    effect: u32,
    alpha_mode: u32,
    flags: u32,
}
#[repr(C)]
struct AdapterDesc {
    description: [u16; 128],
    vendor: u32,
    device: u32,
    subsystem: u32,
    revision: u32,
    dedicated_video: usize,
    dedicated_system: usize,
    shared_system: usize,
    luid: (u32, i32),
}

/// 取对象虚函数表里第 `index` 个方法。调用方给出与头文件一致的函数类型
unsafe fn method<F: Copy>(object: Raw, index: usize) -> F {
    unsafe {
        let table = *(object as *const *const *const c_void);
        std::mem::transmute_copy(&*table.add(index))
    }
}

/// 调用 COM 方法：`com!(对象, 下标, fn(参数类型...) -> 返回类型, 参数...)`，第一个参数 this 自动加上
macro_rules! com {
    ($object:expr, $index:expr, fn($($ty:ty),*) -> $ret:ty $(, $arg:expr)*) => {{
        let object: Raw = $object;
        let f: unsafe extern "system" fn(Raw $(, $ty)*) -> $ret = method(object, $index);
        f(object $(, $arg)*)
    }};
}

/// 持有一个引用的 COM 对象
struct Com(Raw);
impl Com {
    fn new(raw: Raw, hr: Hresult, what: &str) -> Result<Self, String> {
        if hr < 0 || raw.is_null() { Err(format!("{what}失败：{:#010x}", hr as u32)) } else { Ok(Self(raw)) }
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        unsafe { com!(self.0, unknown::RELEASE, fn() -> u32) };
    }
}

fn check(hr: Hresult, what: &str) -> Result<(), String> {
    if hr < 0 { Err(format!("{what}失败：{:#010x}", hr as u32)) } else { Ok(()) }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 只从系统目录加载，不受当前目录里同名 DLL 影响
const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x800;

fn load(library: &str, symbol: &CStr) -> Result<*mut c_void, String> {
    // 句柄不关闭：函数指针在整个进程期间有效
    let module = unsafe { LoadLibraryExW(wide(library).as_ptr(), 0, LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if module == 0 {
        return Err(format!("无法加载 {library}"));
    }
    let address = unsafe { GetProcAddress(module, symbol.as_ptr().cast()) };
    if address.is_null() { Err(format!("{library} 缺少 {}", symbol.to_string_lossy())) } else { Ok(address) }
}

type CreateDevice =
    unsafe extern "system" fn(Raw, u32, isize, u32, *const u32, u32, u32, *mut Raw, *mut u32, *mut Raw) -> Hresult;
type Compile = unsafe extern "system" fn(
    *const c_void,
    usize,
    *const c_char,
    *const c_void,
    *const c_void,
    *const c_char,
    *const c_char,
    u32,
    u32,
    *mut Raw,
    *mut Raw,
) -> Hresult;

const SHADERS: &str = "
cbuffer View : register(b0) { float4 size; };
struct Instance { int4 rect : A; int4 b : B; int4 c : C; int4 d : D; };
struct Varying {
    float4 position : SV_Position;
    nointerpolation int4 b : B;
    nointerpolation int4 c : C;
    nointerpolation int4 d : D;
};
Varying vertex_main(Instance n, uint v : SV_VertexID) {
    float2 p = float2((v & 1) ? n.rect.z : n.rect.x, (v & 2) ? n.rect.w : n.rect.y);
    Varying o;
    o.position = float4(p.x / size.x * 2.0 - 1.0, 1.0 - p.y / size.y * 2.0, 0.0, 1.0);
    o.b = n.b;
    o.c = n.c;
    o.d = n.d;
    return o;
}
Texture2D<float4> t : register(t0);
float3 rgb(int c) {
    return float3(float((c >> 16) & 255), float((c >> 8) & 255), float(c & 255)) / 255.0;
}
// 矩形：b = (颜色, alpha)
float4 rect_main(Varying o) : SV_Target {
    return float4(rgb(o.b.x), float(o.b.y) / 255.0);
}
// 灰度字形：b = (字形原点, 图集位置)，c.x = 颜色
float4 alpha_main(Varying o) : SV_Target {
    int2 p = int2(o.position.xy) - o.b.xy + o.b.zw;
    return float4(rgb(o.c.x), t.Load(int3(p, 0)).r);
}
// 彩色字形：BGRA 图集，预乘
float4 color_main(Varying o) : SV_Target {
    int2 p = int2(o.position.xy) - o.b.xy + o.b.zw;
    return t.Load(int3(p, 0));
}
// 图片：b = (目标原点, 图块内起点)，c.xy = 图块尺寸，d = 裁剪区域；按 Tile::pixel 的整数除法取源像素
float4 image_main(Varying o) : SV_Target {
    int2 q = int2(o.position.xy) - o.b.xy + o.b.zw;
    if (any(q < 0) || any(q >= o.c.xy)) discard;
    return t.Load(int3(o.d.xy + q * o.d.zw / o.c.xy, 0));
}
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Batch {
    Rect,
    Alpha,
    Color,
    Image,
}

/// 纹理和它的着色器资源视图
struct Texture {
    texture: Com,
    view: Com,
}

/// 渲染目标：纹理 + 渲染目标视图
struct Target {
    texture: Com,
    view: Com,
}

/// GPU 资源的额度；驱动内部开销不计入
const ATLAS_BYTES: [usize; 2] = [(ATLAS_SIZE * ATLAS_SIZE) as usize, (ATLAS_SIZE * ATLAS_SIZE * 4) as usize];

pub(super) struct D3d11 {
    device: Com,
    context: Com,
    swap_chain: Option<Com>,
    vertex: Com,
    layout: Com,
    pixels: [Com; 4],
    blend: [Com; 2],
    rasterizer: Com,
    constants: Com,
    instances: Option<(Com, usize)>,
    size: (usize, usize),
    target: Option<Target>,
    scratch: Option<Com>,
    atlases: [Option<Texture>; 2],
    textures: Vec<(u64, Texture, usize)>,
    queued: Vec<[i32; 16]>,
    batch: (Batch, Raw),
    memory: Lease,
    name: String,
    software: bool,
}

impl D3d11 {
    /// 不带窗口：只能回放和读回（测试用）。`driver` 是 D3D_DRIVER_TYPE
    fn with_driver(driver: u32) -> Result<Self, String> {
        let create: CreateDevice = unsafe { std::mem::transmute(load("d3d11.dll", c"D3D11CreateDevice")?) };
        let compile: Compile = unsafe { std::mem::transmute(load("d3dcompiler_47.dll", c"D3DCompile")?) };
        let memory = crate::resources::current().lease(Kind::Canvas, 0).map_err(|_| "Canvas 额度不足")?;
        unsafe {
            let (mut device, mut context, mut level) = (null_mut(), null_mut(), 0);
            let hr = create(
                null_mut(),
                driver,
                0,
                CREATE_DEVICE_BGRA_SUPPORT,
                FEATURE_LEVELS.as_ptr(),
                FEATURE_LEVELS.len() as u32,
                SDK_VERSION,
                &mut device,
                &mut level,
                &mut context,
            );
            let device = Com::new(device, hr, "D3D11CreateDevice")?;
            let context = Com::new(context, hr, "取立即上下文")?;
            let (name, software) = adapter(&device).unwrap_or_else(|_| ("Direct3D 11".into(), false));
            let name = format!("{name}（特性级别 {}.{}）", level >> 12, (level >> 8) & 15);

            let shader = |entry: &CStr, target: &CStr| -> Result<Com, String> {
                let (mut code, mut errors) = (null_mut(), null_mut());
                let hr = compile(
                    SHADERS.as_ptr().cast(),
                    SHADERS.len(),
                    c"congminal".as_ptr(),
                    null(),
                    null(),
                    entry.as_ptr(),
                    target.as_ptr(),
                    0,
                    0,
                    &mut code,
                    &mut errors,
                );
                let errors = (!errors.is_null()).then(|| Com(errors));
                if hr < 0 || code.is_null() {
                    let log = errors.map(|blob| String::from_utf8_lossy(bytes(&blob)).into_owned()).unwrap_or_default();
                    return Err(format!("着色器编译失败：{}", log.trim()));
                }
                Ok(Com(code))
            };
            let vertex_code = shader(c"vertex_main", c"vs_4_0")?;
            let mut vertex = null_mut();
            let code = bytes(&vertex_code);
            let hr = com!(
                device.0,
                device::CREATE_VERTEX_SHADER,
                fn(*const c_void, usize, Raw, *mut Raw) -> Hresult,
                code.as_ptr().cast(),
                code.len(),
                null_mut(),
                &mut vertex
            );
            let vertex = Com::new(vertex, hr, "创建顶点着色器")?;
            let elements: Vec<InputElement> = [c"A", c"B", c"C", c"D"]
                .iter()
                .enumerate()
                .map(|(i, name)| InputElement {
                    semantic: name.as_ptr(),
                    index: 0,
                    format: FORMAT_R32G32B32A32_SINT,
                    slot: 0,
                    offset: i as u32 * 16,
                    class: 1,
                    step: 1,
                })
                .collect();
            let mut layout = null_mut();
            let hr = com!(
                device.0,
                device::CREATE_INPUT_LAYOUT,
                fn(*const InputElement, u32, *const c_void, usize, *mut Raw) -> Hresult,
                elements.as_ptr(),
                elements.len() as u32,
                code.as_ptr().cast(),
                code.len(),
                &mut layout
            );
            let layout = Com::new(layout, hr, "创建输入布局")?;
            let mut pixels = Vec::new();
            for entry in [c"rect_main", c"alpha_main", c"color_main", c"image_main"] {
                let code = shader(entry, c"ps_4_0")?;
                let code = bytes(&code);
                let mut pixel = null_mut();
                let hr = com!(
                    device.0,
                    device::CREATE_PIXEL_SHADER,
                    fn(*const c_void, usize, Raw, *mut Raw) -> Hresult,
                    code.as_ptr().cast(),
                    code.len(),
                    null_mut(),
                    &mut pixel
                );
                pixels.push(Com::new(pixel, hr, "创建像素着色器")?);
            }
            let pixels: [Com; 4] = pixels.try_into().map_err(|_| "像素着色器数量不对")?;
            let blend = |source: u32| -> Result<Com, String> {
                let target = TargetBlend {
                    enable: 1,
                    source,
                    destination: BLEND_INV_SRC_ALPHA,
                    operation: BLEND_OP_ADD,
                    source_alpha: BLEND_ZERO,
                    destination_alpha: BLEND_ONE,
                    operation_alpha: BLEND_OP_ADD,
                    write_mask: 0xf,
                };
                let desc = BlendDesc { alpha_to_coverage: 0, independent: 0, targets: [target; 8] };
                let mut state = null_mut();
                let hr = com!(
                    device.0,
                    device::CREATE_BLEND_STATE,
                    fn(*const BlendDesc, *mut Raw) -> Hresult,
                    &desc,
                    &mut state
                );
                Com::new(state, hr, "创建混合状态")
            };
            let blend = [blend(BLEND_SRC_ALPHA)?, blend(BLEND_ONE)?];
            let desc = RasterizerDesc {
                fill: 3,
                cull: 1,
                front_counter_clockwise: 0,
                depth_bias: 0,
                depth_bias_clamp: 0.0,
                slope_scaled_depth_bias: 0.0,
                depth_clip: 1,
                scissor: 0,
                multisample: 0,
                antialiased_line: 0,
            };
            let mut rasterizer = null_mut();
            let hr = com!(
                device.0,
                device::CREATE_RASTERIZER_STATE,
                fn(*const RasterizerDesc, *mut Raw) -> Hresult,
                &desc,
                &mut rasterizer
            );
            let rasterizer = Com::new(rasterizer, hr, "创建光栅化状态")?;
            let constants = buffer(&device, 16, USAGE_DEFAULT, BIND_CONSTANT_BUFFER, 0)?;
            Ok(Self {
                device,
                context,
                swap_chain: None,
                vertex,
                layout,
                pixels,
                blend,
                rasterizer,
                constants,
                instances: None,
                size: (0, 0),
                target: None,
                scratch: None,
                atlases: [None, None],
                textures: Vec::new(),
                queued: Vec::new(),
                batch: (Batch::Rect, null_mut()),
                memory,
                name,
                software,
            })
        }
    }

    /// 硬件设备加窗口交换链
    pub(super) fn new(hwnd: isize, width: usize, height: usize) -> Result<Self, String> {
        let mut d3d = Self::with_driver(DRIVER_TYPE_HARDWARE)?;
        d3d.size = (width.max(1), height.max(1));
        unsafe {
            let mut dxgi_device = null_mut();
            let hr = com!(
                d3d.device.0,
                unknown::QUERY_INTERFACE,
                fn(*const Guid, *mut Raw) -> Hresult,
                &IID_IDXGI_DEVICE,
                &mut dxgi_device
            );
            let dxgi_device = Com::new(dxgi_device, hr, "取 IDXGIDevice")?;
            let mut adapter = null_mut();
            let hr = com!(dxgi_device.0, dxgi::DEVICE_GET_ADAPTER, fn(*mut Raw) -> Hresult, &mut adapter);
            let adapter = Com::new(adapter, hr, "取显卡")?;
            let mut factory = null_mut();
            let hr = com!(
                adapter.0,
                dxgi::GET_PARENT,
                fn(*const Guid, *mut Raw) -> Hresult,
                &IID_IDXGI_FACTORY2,
                &mut factory
            );
            let factory = Com::new(factory, hr, "取 IDXGIFactory2（需要 Windows 8 及以上）")?;
            let desc = SwapChainDesc1 {
                width: d3d.size.0 as u32,
                height: d3d.size.1 as u32,
                format: FORMAT_BGRA8,
                stereo: 0,
                sample: SampleDesc { count: 1, quality: 0 },
                usage: USAGE_RENDER_TARGET_OUTPUT,
                buffers: SWAP_BUFFERS,
                scaling: 0,
                effect: SWAP_EFFECT_FLIP_SEQUENTIAL,
                alpha_mode: 0,
                flags: 0,
            };
            let mut swap_chain = null_mut();
            let hr = com!(
                factory.0,
                dxgi::FACTORY2_CREATE_SWAP_CHAIN_FOR_HWND,
                fn(Raw, isize, *const SwapChainDesc1, *const c_void, Raw, *mut Raw) -> Hresult,
                d3d.device.0,
                hwnd,
                &desc,
                null(),
                null_mut(),
                &mut swap_chain
            );
            d3d.swap_chain = Some(Com::new(swap_chain, hr, "创建交换链")?);
            com!(factory.0, dxgi::FACTORY_MAKE_WINDOW_ASSOCIATION, fn(isize, u32) -> Hresult, hwnd, MWA_NO_ALT_ENTER);
        }
        // 交换链的尺寸由第一帧决定
        d3d.size = (0, 0);
        Ok(d3d)
    }

    /// 渲染目标与暂存目标各一份画布，交换链缓冲按个数，加上已创建的图集和纹理
    fn account(&mut self) -> Result<(), String> {
        let (w, h) = self.size;
        let chain = if self.swap_chain.is_some() { SWAP_BUFFERS as usize } else { 0 };
        let atlases: usize = (0..2).filter(|&i| self.atlases[i].is_some()).map(|i| ATLAS_BYTES[i]).sum();
        let textures: usize = self.textures.iter().map(|t| t.2).sum();
        let instances = self.instances.as_ref().map_or(0, |i| i.1 * INSTANCE);
        let bytes = w * h * 4 * (2 + chain) + atlases + textures + instances;
        self.memory.resize(bytes).map_err(|_| "Canvas 额度不足，无法创建 GPU 资源".to_string())
    }

    fn texture(&self, format: u32, width: usize, height: usize, bind: u32) -> Result<Com, String> {
        let desc = Texture2dDesc {
            width: width as u32,
            height: height as u32,
            mip_levels: 1,
            array_size: 1,
            format,
            sample: SampleDesc { count: 1, quality: 0 },
            usage: USAGE_DEFAULT,
            bind_flags: bind,
            cpu_access: 0,
            misc: 0,
        };
        texture(&self.device, &desc)
    }

    fn view(&self, texture: Com) -> Result<Texture, String> {
        let mut view = null_mut();
        let hr = unsafe {
            com!(
                self.device.0,
                device::CREATE_SHADER_RESOURCE_VIEW,
                fn(Raw, *const c_void, *mut Raw) -> Hresult,
                texture.0,
                null(),
                &mut view
            )
        };
        Ok(Texture { view: Com::new(view, hr, "创建着色器资源视图")?, texture })
    }

    /// 尺寸变化时重建渲染目标和交换链缓冲（内容作废，由调用方要求整屏重画）
    fn resize(&mut self, width: usize, height: usize) -> Result<bool, String> {
        if self.size == (width, height) && self.target.is_some() {
            return Ok(false);
        }
        self.target = None;
        self.scratch = None;
        self.size = (width, height);
        self.account()?;
        let texture = self.texture(FORMAT_BGRA8, width, height, BIND_RENDER_TARGET | BIND_SHADER_RESOURCE)?;
        let mut view = null_mut();
        unsafe {
            let hr = com!(
                self.device.0,
                device::CREATE_RENDER_TARGET_VIEW,
                fn(Raw, *const c_void, *mut Raw) -> Hresult,
                texture.0,
                null(),
                &mut view
            );
            let view = Com::new(view, hr, "创建渲染目标视图")?;
            let black = [0.0f32, 0.0, 0.0, 1.0];
            com!(self.context.0, context::CLEAR_RENDER_TARGET_VIEW, fn(Raw, *const [f32; 4]) -> (), view.0, &black);
            self.target = Some(Target { texture, view });
            self.scratch = Some(self.texture(FORMAT_BGRA8, width, height, BIND_SHADER_RESOURCE)?);
            if let Some(chain) = &self.swap_chain {
                let hr = com!(
                    chain.0,
                    dxgi::SWAP_CHAIN_RESIZE_BUFFERS,
                    fn(u32, u32, u32, u32, u32) -> Hresult,
                    0,
                    width as u32,
                    height as u32,
                    FORMAT_UNKNOWN,
                    0
                );
                check(hr, "调整交换链尺寸")?;
            }
            let size = [width as f32, height as f32, 0.0, 0.0];
            com!(
                self.context.0,
                context::UPDATE_SUBRESOURCE,
                fn(Raw, u32, *const Box3, *const c_void, u32, u32) -> (),
                self.constants.0,
                0,
                null(),
                size.as_ptr().cast(),
                0,
                0
            );
        }
        Ok(true)
    }

    fn atlas(&mut self, atlas: Atlas) -> Result<Raw, String> {
        let index = atlas as usize;
        if let Some(texture) = &self.atlases[index] {
            return Ok(texture.texture.0);
        }
        let format = if atlas == Atlas::Alpha { FORMAT_R8 } else { FORMAT_BGRA8 };
        let side = ATLAS_SIZE as usize;
        let texture = self.view(self.texture(format, side, side, BIND_SHADER_RESOURCE)?)?;
        let raw = texture.texture.0;
        self.atlases[index] = Some(texture);
        if let Err(error) = self.account() {
            self.atlases[index] = None;
            return Err(error);
        }
        Ok(raw)
    }

    /// 绑定本帧不变的状态：管线固定部分、渲染目标、视口
    unsafe fn bind(&self) {
        let context = self.context.0;
        let target = self.target.as_ref().unwrap().view.0;
        let viewport = Viewport {
            x: 0.0,
            y: 0.0,
            width: self.size.0 as f32,
            height: self.size.1 as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        };
        unsafe {
            com!(context, context::OM_SET_RENDER_TARGETS, fn(u32, *const Raw, Raw) -> (), 1, &target, null_mut());
            com!(context, context::RS_SET_VIEWPORTS, fn(u32, *const Viewport) -> (), 1, &viewport);
            com!(context, context::RS_SET_STATE, fn(Raw) -> (), self.rasterizer.0);
            com!(context, context::IA_SET_INPUT_LAYOUT, fn(Raw) -> (), self.layout.0);
            com!(context, context::IA_SET_PRIMITIVE_TOPOLOGY, fn(u32) -> (), TOPOLOGY_TRIANGLE_STRIP);
            com!(context, context::VS_SET_SHADER, fn(Raw, *const Raw, u32) -> (), self.vertex.0, null(), 0);
            com!(context, context::VS_SET_CONSTANT_BUFFERS, fn(u32, u32, *const Raw) -> (), 0, 1, &self.constants.0);
        }
    }

    fn queue(&mut self, batch: Batch, view: Raw, instance: [[i32; 4]; 4]) -> Result<(), String> {
        if (batch, view) != self.batch {
            self.flush()?;
            self.batch = (batch, view);
        }
        self.queued.push(instance.as_flattened().try_into().unwrap());
        Ok(())
    }

    fn flush(&mut self) -> Result<(), String> {
        if self.queued.is_empty() {
            return Ok(());
        }
        let count = self.queued.len();
        if self.instances.as_ref().is_none_or(|i| i.1 < count) {
            let capacity = count.next_power_of_two().max(1024);
            self.instances = None;
            let bytes = capacity * INSTANCE;
            self.instances =
                Some((buffer(&self.device, bytes, USAGE_DYNAMIC, BIND_VERTEX_BUFFER, CPU_ACCESS_WRITE)?, capacity));
            self.account()?;
        }
        let context = self.context.0;
        let instances = self.instances.as_ref().unwrap().0.0;
        let (batch, view) = self.batch;
        unsafe {
            let mut mapped = Mapped { data: null_mut(), row_pitch: 0, depth_pitch: 0 };
            let hr = com!(
                context,
                context::MAP,
                fn(Raw, u32, u32, u32, *mut Mapped) -> Hresult,
                instances,
                0,
                MAP_WRITE_DISCARD,
                0,
                &mut mapped
            );
            check(hr, "映射实例缓冲")?;
            std::ptr::copy_nonoverlapping(self.queued.as_ptr(), mapped.data.cast::<[i32; 16]>(), count);
            com!(context, context::UNMAP, fn(Raw, u32) -> (), instances, 0);
            let (stride, offset) = (INSTANCE as u32, 0u32);
            com!(
                context,
                context::IA_SET_VERTEX_BUFFERS,
                fn(u32, u32, *const Raw, *const u32, *const u32) -> (),
                0,
                1,
                &instances,
                &stride,
                &offset
            );
            com!(
                context,
                context::PS_SET_SHADER,
                fn(Raw, *const Raw, u32) -> (),
                self.pixels[batch as usize].0,
                null(),
                0
            );
            if !view.is_null() {
                com!(context, context::PS_SET_SHADER_RESOURCES, fn(u32, u32, *const Raw) -> (), 0, 1, &view);
            }
            let blend = if batch == Batch::Color { self.blend[1].0 } else { self.blend[0].0 };
            com!(context, context::OM_SET_BLEND_STATE, fn(Raw, *const [f32; 4], u32) -> (), blend, null(), u32::MAX);
            com!(context, context::DRAW_INSTANCED, fn(u32, u32, u32, u32) -> (), 4, count as u32, 0, 0);
        }
        self.queued.clear();
        Ok(())
    }

    /// 回放一帧并呈现。返回 Ok(true) 表示渲染目标是新建的，调用方要让应用整屏重画。
    fn present(&mut self, list: &DrawList, width: usize, height: usize) -> Result<bool, String> {
        let recreated = self.resize(width, height)?;
        if !list.is_replayed() {
            unsafe { self.bind() };
            let result = self.replay(list);
            self.queued.clear();
            self.batch = (Batch::Rect, null_mut());
            result?;
            list.mark_replayed();
        }
        let Some(chain) = &self.swap_chain else { return Ok(recreated) };
        unsafe {
            let mut back = null_mut();
            let hr = com!(
                chain.0,
                dxgi::SWAP_CHAIN_GET_BUFFER,
                fn(u32, *const Guid, *mut Raw) -> Hresult,
                0,
                &IID_ID3D11_TEXTURE2D,
                &mut back
            );
            let back = Com::new(back, hr, "取后台缓冲")?;
            let region = Box3 { left: 0, top: 0, front: 0, right: width as u32, bottom: height as u32, back: 1 };
            com!(
                self.context.0,
                context::COPY_SUBRESOURCE_REGION,
                fn(Raw, u32, u32, u32, u32, Raw, u32, *const Box3) -> (),
                back.0,
                0,
                0,
                0,
                0,
                self.target.as_ref().unwrap().texture.0,
                0,
                &region
            );
            drop(back);
            // 不等垂直同步：Present 不阻塞 UI 线程，帧率仍由应用的重画节奏决定；DWM 合成，不会撕裂
            let hr = com!(chain.0, dxgi::SWAP_CHAIN_PRESENT, fn(u32, u32) -> Hresult, 0, 0);
            if hr < 0 {
                let reason = com!(self.device.0, device::GET_DEVICE_REMOVED_REASON, fn() -> Hresult);
                return Err(format!("Present 失败：{:#010x}（设备状态 {:#010x}）", hr as u32, reason as u32));
            }
        }
        Ok(recreated)
    }

    fn replay(&mut self, list: &DrawList) -> Result<(), String> {
        for op in list.ops() {
            match *op {
                Op::Fill { rect, color } => {
                    self.queue(Batch::Rect, null_mut(), [rect4(rect), [color as i32, 255, 0, 0], [0; 4], [0; 4]])?
                }
                Op::Blend { rect, color, alpha } => self.queue(
                    Batch::Rect,
                    null_mut(),
                    [rect4(rect), [color as i32, i32::from(alpha), 0, 0], [0; 4], [0; 4]],
                )?,
                Op::Glyph { x, y, slot, color, clip } => {
                    let (batch, atlas) = match slot.atlas {
                        Atlas::Alpha => (Batch::Alpha, &self.atlases[0]),
                        Atlas::Color => (Batch::Color, &self.atlases[1]),
                    };
                    let Some(view) = atlas.as_ref().map(|a| a.view.0) else { continue };
                    let b = [x, y, slot.x as i32, slot.y as i32];
                    self.queue(batch, view, [rect4(clip), b, [color as i32, 0, 0, 0], [0; 4]])?;
                }
                Op::Image { serial, crop, tile, source, x, y, clip } => {
                    let Some(view) = self.textures.iter().find(|t| t.0 == serial).map(|t| t.1.view.0) else { continue };
                    let b = [x, y, source.0, source.1];
                    let c = [tile.0 as i32, tile.1 as i32, 0, 0];
                    self.queue(Batch::Image, view, [rect4(clip), b, c, crop.map(|v| v as i32)])?;
                }
                Op::Shift { rect, dy } => {
                    self.flush()?;
                    unsafe { self.shift(rect, dy) };
                }
                Op::Upload { slot, start } => {
                    self.flush()?;
                    let texture = self.atlas(slot.atlas)?;
                    let (data, pitch): (*const c_void, u32) = match slot.atlas {
                        Atlas::Alpha => (list.alpha()[start..].as_ptr().cast(), slot.w),
                        Atlas::Color => (list.color()[start..].as_ptr().cast(), slot.w * 4),
                    };
                    let region = Box3 {
                        left: slot.x,
                        top: slot.y,
                        front: 0,
                        right: slot.x + slot.w,
                        bottom: slot.y + slot.h,
                        back: 1,
                    };
                    unsafe {
                        com!(
                            self.context.0,
                            context::UPDATE_SUBRESOURCE,
                            fn(Raw, u32, *const Box3, *const c_void, u32, u32) -> (),
                            texture,
                            0,
                            &region,
                            data,
                            pitch,
                            0
                        );
                    }
                }
                Op::ResetAtlas(_) => self.flush()?,
                Op::Texture { serial, width, height, start } => {
                    self.flush()?;
                    self.textures.retain(|t| t.0 != serial);
                    let (w, h) = (width as usize, height as usize);
                    let texture = self.view(self.texture(FORMAT_RGBA8, w, h, BIND_SHADER_RESOURCE)?)?;
                    let raw = texture.texture.0;
                    self.textures.push((serial, texture, w * h * 4));
                    if let Err(error) = self.account() {
                        self.textures.pop();
                        return Err(error);
                    }
                    let data = list.rgba()[start..start + w * h * 4].as_ptr();
                    unsafe {
                        com!(
                            self.context.0,
                            context::UPDATE_SUBRESOURCE,
                            fn(Raw, u32, *const Box3, *const c_void, u32, u32) -> (),
                            raw,
                            0,
                            null(),
                            data.cast(),
                            width * 4,
                            0
                        );
                    }
                }
                Op::ResetTextures => {
                    self.flush()?;
                    self.textures.clear();
                    // 缩小额度不会失败
                    self.account()?;
                }
            }
        }
        self.flush()
    }

    /// 区域内垂直平移：先拷到暂存目标，再拷回偏移后的位置（同一资源内的重叠拷贝没有定义）
    unsafe fn shift(&self, rect: Rect, dy: i32) {
        let rows = rect.y1 - rect.y0;
        let k = dy.unsigned_abs();
        if k >= rows {
            return;
        }
        let (from, to) = if dy > 0 { (rect.y0, rect.y0 + k) } else { (rect.y0 + k, rect.y0) };
        let (target, scratch) = (self.target.as_ref().unwrap().texture.0, self.scratch.as_ref().unwrap().0);
        let region = Box3 { left: rect.x0, top: from, front: 0, right: rect.x1, bottom: from + rows - k, back: 1 };
        unsafe {
            let copy = |destination: Raw, y: u32, source: Raw| {
                com!(
                    self.context.0,
                    context::COPY_SUBRESOURCE_REGION,
                    fn(Raw, u32, u32, u32, u32, Raw, u32, *const Box3) -> (),
                    destination,
                    0,
                    rect.x0,
                    y,
                    0,
                    source,
                    0,
                    &region
                );
            };
            copy(scratch, from, target);
            copy(target, to, scratch);
        }
    }

    /// 读回渲染目标，格式与 CPU 画布相同（0x00RRGGBB，第 0 行在最上面）
    #[cfg(test)]
    fn read(&self) -> Vec<u32> {
        let (w, h) = self.size;
        let desc = Texture2dDesc {
            width: w as u32,
            height: h as u32,
            mip_levels: 1,
            array_size: 1,
            format: FORMAT_BGRA8,
            sample: SampleDesc { count: 1, quality: 0 },
            usage: USAGE_STAGING,
            bind_flags: 0,
            cpu_access: CPU_ACCESS_READ,
            misc: 0,
        };
        let staging = texture(&self.device, &desc).unwrap();
        let mut pixels = Vec::with_capacity(w * h);
        unsafe {
            let context = self.context.0;
            com!(
                context,
                context::COPY_RESOURCE,
                fn(Raw, Raw) -> (),
                staging.0,
                self.target.as_ref().unwrap().texture.0
            );
            let mut mapped = Mapped { data: null_mut(), row_pitch: 0, depth_pitch: 0 };
            let hr = com!(
                context,
                context::MAP,
                fn(Raw, u32, u32, u32, *mut Mapped) -> Hresult,
                staging.0,
                0,
                MAP_READ,
                0,
                &mut mapped
            );
            check(hr, "映射读回纹理").unwrap();
            for y in 0..h {
                let row = mapped.data.cast::<u8>().add(y * mapped.row_pitch as usize).cast::<u32>();
                pixels.extend(std::slice::from_raw_parts(row, w).iter().map(|p| p & 0xff_ffff));
            }
            com!(context, context::UNMAP, fn(Raw, u32) -> (), staging.0, 0);
        }
        pixels
    }
}

fn rect4(rect: Rect) -> [i32; 4] {
    [rect.x0 as i32, rect.y0 as i32, rect.x1 as i32, rect.y1 as i32]
}

fn bytes(blob: &Com) -> &[u8] {
    unsafe {
        let pointer = com!(blob.0, blob::GET_BUFFER_POINTER, fn() -> *const u8);
        let size = com!(blob.0, blob::GET_BUFFER_SIZE, fn() -> usize);
        if pointer.is_null() { &[] } else { std::slice::from_raw_parts(pointer, size) }
    }
}

fn texture(device: &Com, desc: &Texture2dDesc) -> Result<Com, String> {
    let mut texture = null_mut();
    let hr = unsafe {
        com!(
            device.0,
            device::CREATE_TEXTURE_2D,
            fn(*const Texture2dDesc, *const c_void, *mut Raw) -> Hresult,
            desc,
            null(),
            &mut texture
        )
    };
    Com::new(texture, hr, "创建纹理")
}

fn buffer(device: &Com, bytes: usize, usage: u32, bind: u32, cpu_access: u32) -> Result<Com, String> {
    let desc = BufferDesc { bytes: bytes as u32, usage, bind_flags: bind, cpu_access, misc: 0, stride: 0 };
    let mut buffer = null_mut();
    let hr = unsafe {
        com!(
            device.0,
            device::CREATE_BUFFER,
            fn(*const BufferDesc, *const c_void, *mut Raw) -> Hresult,
            &desc,
            null(),
            &mut buffer
        )
    };
    Com::new(buffer, hr, "创建缓冲区")
}

/// 显卡名与是否为 WARP 软件光栅器
fn adapter(device: &Com) -> Result<(String, bool), String> {
    unsafe {
        let mut dxgi_device = null_mut();
        let hr = com!(
            device.0,
            unknown::QUERY_INTERFACE,
            fn(*const Guid, *mut Raw) -> Hresult,
            &IID_IDXGI_DEVICE,
            &mut dxgi_device
        );
        let dxgi_device = Com::new(dxgi_device, hr, "取 IDXGIDevice")?;
        let mut adapter = null_mut();
        let hr = com!(dxgi_device.0, dxgi::DEVICE_GET_ADAPTER, fn(*mut Raw) -> Hresult, &mut adapter);
        let adapter = Com::new(adapter, hr, "取显卡")?;
        let mut desc: AdapterDesc = std::mem::zeroed();
        check(com!(adapter.0, dxgi::ADAPTER_GET_DESC, fn(*mut AdapterDesc) -> Hresult, &mut desc), "读显卡信息")?;
        let length = desc.description.iter().position(|&c| c == 0).unwrap_or(desc.description.len());
        Ok((String::from_utf16_lossy(&desc.description[..length]), desc.vendor == WARP_VENDOR))
    }
}

impl super::super::gpu::Backend for D3d11 {
    fn name(&self) -> &str {
        &self.name
    }
    fn software(&self) -> bool {
        self.software
    }
    fn present(&mut self, list: &DrawList, width: usize, height: usize) -> Result<bool, String> {
        D3d11::present(self, list, width, height)
    }
}

impl Drop for D3d11 {
    fn drop(&mut self) {
        // 交换链先于窗口释放；把已提交的命令交给驱动
        unsafe { com!(self.context.0, context::FLUSH, fn() -> ()) };
    }
}

/// 不需要窗口：在 `cargo test` 的线程上直接创建设备回放
#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::list::tests::{GpuTarget, gpu_scenarios};

    impl GpuTarget for D3d11 {
        fn replay(&mut self, list: &DrawList, width: usize, height: usize) {
            D3d11::present(self, list, width, height).unwrap();
        }
        fn read(&mut self, width: usize, height: usize) -> Option<Vec<u32>> {
            (self.target.is_some() && self.size == (width, height)).then(|| D3d11::read(self))
        }
    }

    #[test]
    #[ignore = "需要 Direct3D 11"]
    fn d3d11_replay_matches_cpu_within_one_per_channel() {
        // 没有显卡的机器（CI）用 WARP，结果语义相同
        let mut d3d = D3d11::with_driver(DRIVER_TYPE_HARDWARE)
            .or_else(|_| D3d11::with_driver(DRIVER_TYPE_WARP))
            .expect("Direct3D 11 不可用");
        let device = format!("Direct3D 11：{}", d3d.name);
        gpu_scenarios(&device, &mut d3d);
    }
}
