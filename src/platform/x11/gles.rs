//! GPU 呈现：EGL + OpenGL ES 3.0，按顺序回放绘制记录（`render::list`）。
//!
//! - `libEGL.so.1`、`libGLESv2.so.2` 在运行时加载，缺库、远程 DISPLAY、没有合适配置、着色器编译失败、
//!   额度不足都返回错误，调用方继续用 CPU
//! - 持久渲染目标与窗口同尺寸；坐标与 CPU 画布一致（第 0 行在纹理第 0 行），呈现时上下翻转拷到窗口
//! - 混合只作用于颜色，目标的 alpha 保持 1
//! - 所有采样都是整数纹素读取，混合在 8 位非 sRGB 目标上进行；每次混合与 CPU 结果每通道最多差 1，
//!   多层叠加时逐层累积（硬件对 src·a 与 dst·(1−a) 分别舍入）
use super::{Display, XWindow};
use crate::render::list::{ATLAS_SIZE, Atlas, DrawList, Op, Rect};
use crate::resources::{Kind, Lease};
use std::ffi::{CStr, c_char, c_int, c_uint, c_ulong, c_void};
use std::ptr::null;

type Egl = *mut c_void;

const EGL_NONE: c_int = 0x3038;
const EGL_RED_SIZE: c_int = 0x3024;
const EGL_GREEN_SIZE: c_int = 0x3023;
const EGL_BLUE_SIZE: c_int = 0x3022;
const EGL_RENDERABLE_TYPE: c_int = 0x3040;
const EGL_OPENGL_ES3_BIT: c_int = 0x40;
const EGL_SURFACE_TYPE: c_int = 0x3033;
const EGL_WINDOW_BIT: c_int = 4;
const EGL_NATIVE_VISUAL_ID: c_int = 0x302E;
const EGL_OPENGL_ES_API: c_uint = 0x30A0;
const EGL_CONTEXT_CLIENT_VERSION: c_int = 0x3098;

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_NEAREST: i32 = 0x2600;
const GL_CLAMP_TO_EDGE: i32 = 0x812F;
const GL_RGBA: u32 = 0x1908;
const GL_RGBA8: i32 = 0x8058;
const GL_R8: i32 = 0x8229;
const GL_RED: u32 = 0x1903;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_UNPACK_ALIGNMENT: u32 = 0x0CF5;
const GL_PACK_ALIGNMENT: u32 = 0x0D05;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
const GL_DRAW_FRAMEBUFFER: u32 = 0x8CA9;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_ARRAY_BUFFER: u32 = 0x8892;
const GL_STREAM_DRAW: u32 = 0x88E0;
const GL_INT: u32 = 0x1404;
const GL_TRIANGLE_STRIP: u32 = 0x0005;
const GL_BLEND: u32 = 0x0BE2;
const GL_DITHER: u32 = 0x0BD0;
const GL_ZERO: u32 = 0;
const GL_ONE: u32 = 1;
const GL_SRC_ALPHA: u32 = 0x0302;
const GL_ONE_MINUS_SRC_ALPHA: u32 = 0x0303;
const GL_VERTEX_SHADER: u32 = 0x8B31;
const GL_FRAGMENT_SHADER: u32 = 0x8B30;
const GL_COMPILE_STATUS: u32 = 0x8B81;
const GL_LINK_STATUS: u32 = 0x8B82;
const GL_TEXTURE0: u32 = 0x84C0;
const GL_RENDERER: u32 = 0x1F01;
const GL_VERSION: u32 = 0x1F02;
const GL_NO_ERROR: u32 = 0;

macro_rules! api {
    ($name:ident { $($(#[$meta:meta])* $field:ident: $symbol:literal fn($($arg:ty),*) $(-> $ret:ty)?;)* }) => {
        struct $name {
            $($(#[$meta])* $field: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }
        impl $name {
            fn load(library: *mut c_void) -> Result<Self, String> {
                Ok(Self { $($field: symbol(library, $symbol)?,)* })
            }
        }
    };
}

api!(EglApi {
    get_display: c"eglGetDisplay" fn(*mut Display) -> Egl;
    initialize: c"eglInitialize" fn(Egl, *mut c_int, *mut c_int) -> c_uint;
    terminate: c"eglTerminate" fn(Egl) -> c_uint;
    choose_config: c"eglChooseConfig" fn(Egl, *const c_int, *mut Egl, c_int, *mut c_int) -> c_uint;
    get_config_attrib: c"eglGetConfigAttrib" fn(Egl, Egl, c_int, *mut c_int) -> c_uint;
    bind_api: c"eglBindAPI" fn(c_uint) -> c_uint;
    create_context: c"eglCreateContext" fn(Egl, Egl, Egl, *const c_int) -> Egl;
    destroy_context: c"eglDestroyContext" fn(Egl, Egl) -> c_uint;
    create_window_surface: c"eglCreateWindowSurface" fn(Egl, Egl, XWindow, *const c_int) -> Egl;
    destroy_surface: c"eglDestroySurface" fn(Egl, Egl) -> c_uint;
    make_current: c"eglMakeCurrent" fn(Egl, Egl, Egl, Egl) -> c_uint;
    swap_interval: c"eglSwapInterval" fn(Egl, c_int) -> c_uint;
    swap_buffers: c"eglSwapBuffers" fn(Egl, Egl) -> c_uint;
    get_error: c"eglGetError" fn() -> c_int;
});

api!(GlApi {
    get_string: c"glGetString" fn(u32) -> *const c_char;
    get_error: c"glGetError" fn() -> u32;
    enable: c"glEnable" fn(u32);
    disable: c"glDisable" fn(u32);
    blend_func: c"glBlendFuncSeparate" fn(u32, u32, u32, u32);
    viewport: c"glViewport" fn(i32, i32, i32, i32);
    clear_color: c"glClearColor" fn(f32, f32, f32, f32);
    clear: c"glClear" fn(u32);
    pixel_store: c"glPixelStorei" fn(u32, i32);
    gen_textures: c"glGenTextures" fn(i32, *mut u32);
    delete_textures: c"glDeleteTextures" fn(i32, *const u32);
    bind_texture: c"glBindTexture" fn(u32, u32);
    active_texture: c"glActiveTexture" fn(u32);
    tex_parameter: c"glTexParameteri" fn(u32, u32, i32);
    tex_image: c"glTexImage2D" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void);
    tex_sub_image: c"glTexSubImage2D" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void);
    gen_framebuffers: c"glGenFramebuffers" fn(i32, *mut u32);
    delete_framebuffers: c"glDeleteFramebuffers" fn(i32, *const u32);
    bind_framebuffer: c"glBindFramebuffer" fn(u32, u32);
    framebuffer_texture: c"glFramebufferTexture2D" fn(u32, u32, u32, u32, i32);
    check_framebuffer: c"glCheckFramebufferStatus" fn(u32) -> u32;
    blit_framebuffer: c"glBlitFramebuffer" fn(i32, i32, i32, i32, i32, i32, i32, i32, u32, u32);
    #[cfg_attr(not(test), allow(dead_code))]
    read_pixels: c"glReadPixels" fn(i32, i32, i32, i32, u32, u32, *mut c_void);
    gen_buffers: c"glGenBuffers" fn(i32, *mut u32);
    delete_buffers: c"glDeleteBuffers" fn(i32, *const u32);
    bind_buffer: c"glBindBuffer" fn(u32, u32);
    buffer_data: c"glBufferData" fn(u32, isize, *const c_void, u32);
    gen_vertex_arrays: c"glGenVertexArrays" fn(i32, *mut u32);
    delete_vertex_arrays: c"glDeleteVertexArrays" fn(i32, *const u32);
    bind_vertex_array: c"glBindVertexArray" fn(u32);
    enable_attrib: c"glEnableVertexAttribArray" fn(u32);
    attrib_ipointer: c"glVertexAttribIPointer" fn(u32, i32, u32, i32, *const c_void);
    attrib_divisor: c"glVertexAttribDivisor" fn(u32, u32);
    draw_instanced: c"glDrawArraysInstanced" fn(u32, i32, i32, i32);
    create_shader: c"glCreateShader" fn(u32) -> u32;
    shader_source: c"glShaderSource" fn(u32, i32, *const *const c_char, *const i32);
    compile_shader: c"glCompileShader" fn(u32);
    get_shader: c"glGetShaderiv" fn(u32, u32, *mut i32);
    shader_log: c"glGetShaderInfoLog" fn(u32, i32, *mut i32, *mut c_char);
    delete_shader: c"glDeleteShader" fn(u32);
    create_program: c"glCreateProgram" fn() -> u32;
    attach_shader: c"glAttachShader" fn(u32, u32);
    link_program: c"glLinkProgram" fn(u32);
    get_program: c"glGetProgramiv" fn(u32, u32, *mut i32);
    program_log: c"glGetProgramInfoLog" fn(u32, i32, *mut i32, *mut c_char);
    delete_program: c"glDeleteProgram" fn(u32);
    use_program: c"glUseProgram" fn(u32);
    uniform_location: c"glGetUniformLocation" fn(u32, *const c_char) -> i32;
    uniform2f: c"glUniform2f" fn(i32, f32, f32);
    uniform1i: c"glUniform1i" fn(i32, i32);
    uniform4i: c"glUniform4i" fn(i32, i32, i32, i32, i32);
});

fn symbol<T>(library: *mut c_void, name: &CStr) -> Result<T, String> {
    let address = unsafe { libc::dlsym(library, name.as_ptr()) };
    if address.is_null() {
        return Err(format!("缺少 {}", name.to_string_lossy()));
    }
    // 函数指针与数据指针大小相同（POSIX dlsym 的约定）。
    Ok(unsafe { std::mem::transmute_copy(&address) })
}

fn open(name: &CStr) -> Result<*mut c_void, String> {
    // 句柄不关闭：函数指针在整个进程期间有效。
    let library = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if library.is_null() { Err(format!("无法加载 {}", name.to_string_lossy())) } else { Ok(library) }
}

const VERTEX: &str = "#version 300 es
precision highp float;
precision highp int;
layout(location = 0) in ivec4 a_rect;
layout(location = 1) in ivec4 a_b;
layout(location = 2) in ivec4 a_c;
uniform vec2 u_size;
flat out ivec4 v_b;
flat out ivec4 v_c;
void main() {
    int i = gl_VertexID;
    vec2 p = vec2(float((i & 1) == 0 ? a_rect.x : a_rect.z), float((i & 2) == 0 ? a_rect.y : a_rect.w));
    gl_Position = vec4(p / u_size * 2.0 - 1.0, 0.0, 1.0);
    v_b = a_b;
    v_c = a_c;
}
";

const FRAGMENT_HEAD: &str = "#version 300 es
precision highp float;
precision highp int;
flat in ivec4 v_b;
flat in ivec4 v_c;
uniform highp sampler2D u_texture;
uniform ivec4 u_crop;
out vec4 o;
vec3 rgb(int c) {
    return vec3(float((c >> 16) & 255), float((c >> 8) & 255), float(c & 255)) / 255.0;
}
";

/// 矩形：v_b = (颜色, alpha)
const RECT: &str = "void main() { o = vec4(rgb(v_b.x), float(v_b.y) / 255.0); }";
/// 灰度字形：v_b = (字形原点, 图集位置)，v_c.x = 颜色
const ALPHA: &str = "void main() {
    ivec2 p = ivec2(gl_FragCoord.xy);
    o = vec4(rgb(v_c.x), texelFetch(u_texture, p - v_b.xy + v_b.zw, 0).r);
}";
/// 彩色字形：图集里按字节存 B、G、R、A（预乘）
const COLOR: &str = "void main() {
    ivec2 p = ivec2(gl_FragCoord.xy);
    vec4 t = texelFetch(u_texture, p - v_b.xy + v_b.zw, 0);
    o = vec4(t.b, t.g, t.r, t.a);
}";
/// 图片：v_b = (目标原点, 图块内起点)，v_c.xy = 图块尺寸，按 Tile::pixel 的整数除法取源像素
const IMAGE: &str = "void main() {
    ivec2 t = ivec2(gl_FragCoord.xy) - v_b.xy + v_b.zw;
    if (any(lessThan(t, ivec2(0))) || any(greaterThanEqual(t, v_c.xy))) discard;
    o = texelFetch(u_texture, u_crop.xy + t * u_crop.zw / v_c.xy, 0);
}";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Batch {
    Rect,
    Alpha,
    Color,
}

struct Program {
    id: u32,
    size: i32,
    texture: i32,
    crop: i32,
}

/// 渲染目标：纹理 + 帧缓冲
struct Target {
    texture: u32,
    framebuffer: u32,
}

/// GPU 资源的额度；驱动内部开销不计入
const ATLAS_BYTES: [usize; 2] = [(ATLAS_SIZE * ATLAS_SIZE) as usize, (ATLAS_SIZE * ATLAS_SIZE * 4) as usize];

pub(super) struct Gles {
    egl: EglApi,
    gl: GlApi,
    display: Egl,
    context: Egl,
    surface: Egl,
    programs: [Program; 4],
    vertex_array: u32,
    buffer: u32,
    size: (usize, usize),
    target: Option<Target>,
    scratch: Option<Target>,
    atlases: [Option<u32>; 2],
    textures: Vec<(u64, u32, usize)>,
    instances: Vec<[i32; 12]>,
    batch: Batch,
    memory: Lease,
    renderer: String,
    software: bool,
}

impl Gles {
    pub(super) fn new(display: *mut Display, window: XWindow, visual: c_ulong) -> Result<Self, String> {
        let egl = EglApi::load(open(c"libEGL.so.1")?)?;
        let gl = GlApi::load(open(c"libGLESv2.so.2")?)?;
        let memory = crate::resources::current().lease(Kind::Canvas, 0).map_err(|_| "Canvas 额度不足")?;
        unsafe {
            let egl_display = (egl.get_display)(display);
            if egl_display.is_null() || (egl.initialize)(egl_display, std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
                return Err("eglInitialize 失败".into());
            }
            let mut gles = Gles {
                egl,
                gl,
                display: egl_display,
                context: std::ptr::null_mut(),
                surface: std::ptr::null_mut(),
                programs: std::array::from_fn(|_| Program { id: 0, size: -1, texture: -1, crop: -1 }),
                vertex_array: 0,
                buffer: 0,
                size: (0, 0),
                target: None,
                scratch: None,
                atlases: [None; 2],
                textures: Vec::new(),
                instances: Vec::new(),
                batch: Batch::Rect,
                memory,
                renderer: String::new(),
                software: false,
            };
            gles.create_context(window, visual)?;
            gles.setup()?;
            Ok(gles)
        }
    }

    unsafe fn create_context(&mut self, window: XWindow, visual: c_ulong) -> Result<(), String> {
        let egl = &self.egl;
        let attributes = [
            EGL_RED_SIZE,
            8,
            EGL_GREEN_SIZE,
            8,
            EGL_BLUE_SIZE,
            8,
            EGL_RENDERABLE_TYPE,
            EGL_OPENGL_ES3_BIT,
            EGL_SURFACE_TYPE,
            EGL_WINDOW_BIT,
            EGL_NONE,
        ];
        let mut configs = [std::ptr::null_mut(); 64];
        let mut count = 0;
        unsafe {
            if (egl.choose_config)(self.display, attributes.as_ptr(), configs.as_mut_ptr(), 64, &mut count) == 0 {
                return Err("eglChooseConfig 失败".into());
            }
            // 表面必须与窗口的视觉类型一致
            let config = configs[..count.clamp(0, 64) as usize]
                .iter()
                .copied()
                .find(|&config| {
                    let mut id = 0;
                    (egl.get_config_attrib)(self.display, config, EGL_NATIVE_VISUAL_ID, &mut id) != 0
                        && id as c_ulong == visual
                })
                .ok_or("没有与窗口视觉类型匹配的 EGL 配置")?;
            if (egl.bind_api)(EGL_OPENGL_ES_API) == 0 {
                return Err("eglBindAPI 失败".into());
            }
            let context_attributes = [EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE];
            self.context =
                (egl.create_context)(self.display, config, std::ptr::null_mut(), context_attributes.as_ptr());
            if self.context.is_null() {
                return Err(format!("eglCreateContext 失败：{:#x}", (egl.get_error)()));
            }
            self.surface = (egl.create_window_surface)(self.display, config, window, null());
            if self.surface.is_null() {
                return Err(format!("eglCreateWindowSurface 失败：{:#x}", (egl.get_error)()));
            }
            if (egl.make_current)(self.display, self.surface, self.surface, self.context) == 0 {
                return Err(format!("eglMakeCurrent 失败：{:#x}", (egl.get_error)()));
            }
            // 不等垂直同步：交换不阻塞 UI 线程，帧率仍由应用的重画节奏决定
            (egl.swap_interval)(self.display, 0);
        }
        Ok(())
    }

    unsafe fn setup(&mut self) -> Result<(), String> {
        let gl = &self.gl;
        unsafe {
            let text = |name| {
                let s = (gl.get_string)(name);
                if s.is_null() { String::new() } else { CStr::from_ptr(s).to_string_lossy().into_owned() }
            };
            self.renderer = format!("{}（{}）", text(GL_RENDERER), text(GL_VERSION));
            let lower = self.renderer.to_ascii_lowercase();
            self.software = ["llvmpipe", "softpipe", "swiftshader", "software"].iter().any(|n| lower.contains(n));
            for (i, body) in [RECT, ALPHA, COLOR, IMAGE].iter().enumerate() {
                self.programs[i] = self.program(body)?;
            }
            let gl = &self.gl;
            (gl.gen_vertex_arrays)(1, &mut self.vertex_array);
            (gl.gen_buffers)(1, &mut self.buffer);
            (gl.bind_vertex_array)(self.vertex_array);
            (gl.bind_buffer)(GL_ARRAY_BUFFER, self.buffer);
            for i in 0..3 {
                (gl.enable_attrib)(i);
                (gl.attrib_ipointer)(i, 4, GL_INT, 48, (i as usize * 16) as *const c_void);
                (gl.attrib_divisor)(i, 1);
            }
            (gl.disable)(GL_DITHER);
            (gl.enable)(GL_BLEND);
            (gl.pixel_store)(GL_UNPACK_ALIGNMENT, 1);
            (gl.pixel_store)(GL_PACK_ALIGNMENT, 4);
            self.check("初始化")
        }
    }

    unsafe fn program(&self, body: &str) -> Result<Program, String> {
        let gl = &self.gl;
        unsafe {
            let compile = |kind, source: &str| -> Result<u32, String> {
                let shader = (gl.create_shader)(kind);
                let pointer = source.as_ptr().cast::<c_char>();
                let length = source.len() as i32;
                (gl.shader_source)(shader, 1, &pointer, &length);
                (gl.compile_shader)(shader);
                let mut ok = 0;
                (gl.get_shader)(shader, GL_COMPILE_STATUS, &mut ok);
                if ok == 0 {
                    let mut log = [0 as c_char; 1024];
                    (gl.shader_log)(shader, log.len() as i32, std::ptr::null_mut(), log.as_mut_ptr());
                    (gl.delete_shader)(shader);
                    return Err(format!("着色器编译失败：{}", CStr::from_ptr(log.as_ptr()).to_string_lossy()));
                }
                Ok(shader)
            };
            let vertex = compile(GL_VERTEX_SHADER, VERTEX)?;
            let fragment = compile(GL_FRAGMENT_SHADER, &format!("{FRAGMENT_HEAD}{body}"))?;
            let id = (gl.create_program)();
            (gl.attach_shader)(id, vertex);
            (gl.attach_shader)(id, fragment);
            (gl.link_program)(id);
            (gl.delete_shader)(vertex);
            (gl.delete_shader)(fragment);
            let mut ok = 0;
            (gl.get_program)(id, GL_LINK_STATUS, &mut ok);
            if ok == 0 {
                let mut log = [0 as c_char; 1024];
                (gl.program_log)(id, log.len() as i32, std::ptr::null_mut(), log.as_mut_ptr());
                (gl.delete_program)(id);
                return Err(format!("着色器链接失败：{}", CStr::from_ptr(log.as_ptr()).to_string_lossy()));
            }
            Ok(Program {
                id,
                size: (gl.uniform_location)(id, c"u_size".as_ptr()),
                texture: (gl.uniform_location)(id, c"u_texture".as_ptr()),
                crop: (gl.uniform_location)(id, c"u_crop".as_ptr()),
            })
        }
    }

    fn check(&self, step: &str) -> Result<(), String> {
        let error = unsafe { (self.gl.get_error)() };
        if error == GL_NO_ERROR { Ok(()) } else { Err(format!("{step}：GL 错误 {error:#x}")) }
    }

    /// 额度：渲染目标与暂存目标各一份画布，加上已创建的图集和纹理
    fn account(&mut self) -> Result<(), String> {
        let (w, h) = self.size;
        let atlases: usize = (0..2).filter(|&i| self.atlases[i].is_some()).map(|i| ATLAS_BYTES[i]).sum();
        let textures: usize = self.textures.iter().map(|t| t.2).sum();
        let bytes = w * h * 4 * 2 + atlases + textures;
        self.memory.resize(bytes).map_err(|_| "Canvas 额度不足，无法创建 GPU 资源".to_string())
    }

    unsafe fn texture(&self, format: i32, width: usize, height: usize, data: *const c_void) -> u32 {
        let gl = &self.gl;
        let mut texture = 0;
        unsafe {
            (gl.gen_textures)(1, &mut texture);
            (gl.bind_texture)(GL_TEXTURE_2D, texture);
            for (name, value) in [
                (GL_TEXTURE_MIN_FILTER, GL_NEAREST),
                (GL_TEXTURE_MAG_FILTER, GL_NEAREST),
                (GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE),
                (GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE),
            ] {
                (gl.tex_parameter)(GL_TEXTURE_2D, name, value);
            }
            let (pixel, kind) = if format == GL_R8 { (GL_RED, GL_UNSIGNED_BYTE) } else { (GL_RGBA, GL_UNSIGNED_BYTE) };
            (gl.tex_image)(GL_TEXTURE_2D, 0, format, width as i32, height as i32, 0, pixel, kind, data);
        }
        texture
    }

    unsafe fn target(&self) -> Result<Target, String> {
        let gl = &self.gl;
        unsafe {
            let texture = self.texture(GL_RGBA8, self.size.0, self.size.1, null());
            let mut framebuffer = 0;
            (gl.gen_framebuffers)(1, &mut framebuffer);
            (gl.bind_framebuffer)(GL_FRAMEBUFFER, framebuffer);
            (gl.framebuffer_texture)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
            if (gl.check_framebuffer)(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE {
                (gl.delete_framebuffers)(1, &framebuffer);
                (gl.delete_textures)(1, &texture);
                return Err("渲染目标不完整".into());
            }
            Ok(Target { texture, framebuffer })
        }
    }

    fn drop_target(&mut self) {
        let gl = &self.gl;
        for target in [self.target.take(), self.scratch.take()].into_iter().flatten() {
            unsafe {
                (gl.delete_framebuffers)(1, &target.framebuffer);
                (gl.delete_textures)(1, &target.texture);
            }
        }
    }

    /// 尺寸变化时重建渲染目标（内容作废，由调用方要求整屏重画）
    fn resize(&mut self, width: usize, height: usize) -> Result<bool, String> {
        if self.size == (width, height) && self.target.is_some() {
            return Ok(false);
        }
        self.drop_target();
        self.size = (width, height);
        self.account()?;
        unsafe {
            self.target = Some(self.target()?);
            self.scratch = Some(self.target()?);
            let gl = &self.gl;
            (gl.bind_framebuffer)(GL_FRAMEBUFFER, self.target.as_ref().unwrap().framebuffer);
            (gl.clear_color)(0.0, 0.0, 0.0, 1.0);
            (gl.clear)(GL_COLOR_BUFFER_BIT);
        }
        self.check("创建渲染目标")?;
        Ok(true)
    }

    fn atlas(&mut self, atlas: Atlas) -> Result<u32, String> {
        let index = atlas as usize;
        if let Some(texture) = self.atlases[index] {
            return Ok(texture);
        }
        self.atlases[index] = Some(0);
        if let Err(error) = self.account() {
            self.atlases[index] = None;
            return Err(error);
        }
        let format = if atlas == Atlas::Alpha { GL_R8 } else { GL_RGBA8 };
        let side = ATLAS_SIZE as usize;
        let texture = unsafe { self.texture(format, side, side, null()) };
        self.atlases[index] = Some(texture);
        Ok(texture)
    }

    fn flush(&mut self) {
        if self.instances.is_empty() {
            return;
        }
        let gl = &self.gl;
        let program = &self.programs[self.batch as usize];
        unsafe {
            (gl.use_program)(program.id);
            (gl.uniform2f)(program.size, self.size.0 as f32, self.size.1 as f32);
            match self.batch {
                Batch::Rect => (gl.blend_func)(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA, GL_ZERO, GL_ONE),
                Batch::Alpha | Batch::Color => {
                    let atlas = if self.batch == Batch::Alpha { Atlas::Alpha } else { Atlas::Color };
                    (gl.active_texture)(GL_TEXTURE0);
                    (gl.bind_texture)(GL_TEXTURE_2D, self.atlases[atlas as usize].unwrap_or(0));
                    (gl.uniform1i)(program.texture, 0);
                    if self.batch == Batch::Alpha {
                        (gl.blend_func)(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA, GL_ZERO, GL_ONE);
                    } else {
                        (gl.blend_func)(GL_ONE, GL_ONE_MINUS_SRC_ALPHA, GL_ZERO, GL_ONE);
                    }
                }
            }
            self.draw_instances();
        }
        self.instances.clear();
    }

    unsafe fn draw_instances(&self) {
        let gl = &self.gl;
        unsafe {
            (gl.bind_vertex_array)(self.vertex_array);
            (gl.bind_buffer)(GL_ARRAY_BUFFER, self.buffer);
            let bytes = std::mem::size_of_val(self.instances.as_slice());
            (gl.buffer_data)(GL_ARRAY_BUFFER, bytes as isize, self.instances.as_ptr().cast(), GL_STREAM_DRAW);
            (gl.draw_instanced)(GL_TRIANGLE_STRIP, 0, 4, self.instances.len() as i32);
        }
    }

    fn queue(&mut self, batch: Batch, instance: [i32; 12]) {
        if batch != self.batch {
            self.flush();
            self.batch = batch;
        }
        self.instances.push(instance);
    }

    fn rect(rect: Rect) -> [i32; 4] {
        [rect.x0 as i32, rect.y0 as i32, rect.x1 as i32, rect.y1 as i32]
    }

    /// 回放一帧并拷到窗口。返回 Ok(true) 表示渲染目标是新建的，调用方要让应用整屏重画。
    fn present(&mut self, list: &DrawList, width: usize, height: usize) -> Result<bool, String> {
        let recreated = self.resize(width, height)?;
        if !list.is_replayed() {
            self.replay(list)?;
            list.mark_replayed();
        }
        let gl = &self.gl;
        unsafe {
            (gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, self.target.as_ref().unwrap().framebuffer);
            (gl.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, 0);
            let (w, h) = (width as i32, height as i32);
            (gl.blit_framebuffer)(0, 0, w, h, 0, h, w, 0, GL_COLOR_BUFFER_BIT, GL_NEAREST as u32);
            self.check("呈现")?;
            if (self.egl.swap_buffers)(self.display, self.surface) == 0 {
                return Err(format!("eglSwapBuffers 失败：{:#x}", (self.egl.get_error)()));
            }
        }
        Ok(recreated)
    }

    fn replay(&mut self, list: &DrawList) -> Result<(), String> {
        let target = self.target.as_ref().unwrap().framebuffer;
        unsafe {
            (self.gl.bind_framebuffer)(GL_FRAMEBUFFER, target);
            (self.gl.viewport)(0, 0, self.size.0 as i32, self.size.1 as i32);
        }
        for op in list.ops() {
            match *op {
                Op::Fill { rect, color } => {
                    let [x0, y0, x1, y1] = Self::rect(rect);
                    self.queue(Batch::Rect, [x0, y0, x1, y1, color as i32, 255, 0, 0, 0, 0, 0, 0]);
                }
                Op::Blend { rect, color, alpha } => {
                    let [x0, y0, x1, y1] = Self::rect(rect);
                    self.queue(Batch::Rect, [x0, y0, x1, y1, color as i32, i32::from(alpha), 0, 0, 0, 0, 0, 0]);
                }
                Op::Glyph { x, y, slot, color, clip } => {
                    let [x0, y0, x1, y1] = Self::rect(clip);
                    let batch = if slot.atlas == Atlas::Alpha { Batch::Alpha } else { Batch::Color };
                    let (sx, sy) = (slot.x as i32, slot.y as i32);
                    self.queue(batch, [x0, y0, x1, y1, x, y, sx, sy, color as i32, 0, 0, 0]);
                }
                Op::Shift { rect, dy } => {
                    self.flush();
                    self.shift(rect, dy);
                }
                Op::Upload { slot, start } => {
                    self.flush();
                    let texture = self.atlas(slot.atlas)?;
                    let gl = &self.gl;
                    let (format, data): (u32, *const c_void) = match slot.atlas {
                        Atlas::Alpha => (GL_RED, list.alpha()[start..].as_ptr().cast()),
                        Atlas::Color => (GL_RGBA, list.color()[start..].as_ptr().cast()),
                    };
                    unsafe {
                        (gl.bind_texture)(GL_TEXTURE_2D, texture);
                        let (x, y, w, h) = (slot.x as i32, slot.y as i32, slot.w as i32, slot.h as i32);
                        (gl.tex_sub_image)(GL_TEXTURE_2D, 0, x, y, w, h, format, GL_UNSIGNED_BYTE, data);
                    }
                }
                Op::ResetAtlas(_) => self.flush(),
                Op::Texture { serial, width, height, start } => {
                    self.flush();
                    self.drop_texture(serial);
                    let bytes = width as usize * height as usize * 4;
                    self.textures.push((serial, 0, bytes));
                    if let Err(error) = self.account() {
                        self.textures.pop();
                        return Err(error);
                    }
                    let data = list.rgba()[start..start + bytes].as_ptr().cast();
                    let texture = unsafe { self.texture(GL_RGBA8, width as usize, height as usize, data) };
                    self.textures.last_mut().unwrap().1 = texture;
                }
                Op::ResetTextures => {
                    self.flush();
                    for (_, texture, _) in std::mem::take(&mut self.textures) {
                        unsafe { (self.gl.delete_textures)(1, &texture) };
                    }
                    // 缩小额度不会失败
                    self.account()?;
                }
                Op::Image { serial, crop, tile, source, x, y, clip } => {
                    self.flush();
                    let Some(&(_, texture, _)) = self.textures.iter().find(|t| t.0 == serial) else { continue };
                    let [x0, y0, x1, y1] = Self::rect(clip);
                    let (tw, th) = (tile.0 as i32, tile.1 as i32);
                    self.instances.push([x0, y0, x1, y1, x, y, source.0, source.1, tw, th, 0, 0]);
                    let gl = &self.gl;
                    let program = &self.programs[3];
                    unsafe {
                        (gl.use_program)(program.id);
                        (gl.uniform2f)(program.size, self.size.0 as f32, self.size.1 as f32);
                        let c = crop.map(|v| v as i32);
                        (gl.uniform4i)(program.crop, c[0], c[1], c[2], c[3]);
                        (gl.active_texture)(GL_TEXTURE0);
                        (gl.bind_texture)(GL_TEXTURE_2D, texture);
                        (gl.uniform1i)(program.texture, 0);
                        (gl.blend_func)(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA, GL_ZERO, GL_ONE);
                        self.draw_instances();
                    }
                    self.instances.clear();
                }
            }
        }
        self.flush();
        self.check("回放")
    }

    /// 区域内垂直平移：先整块拷到暂存目标，再拷回偏移后的位置（同一帧缓冲内的重叠拷贝没有定义）
    fn shift(&mut self, rect: Rect, dy: i32) {
        let rows = (rect.y1 - rect.y0) as i32;
        let k = dy.abs();
        if k >= rows {
            return;
        }
        let (x0, x1) = (rect.x0 as i32, rect.x1 as i32);
        let (y0, y1) = (rect.y0 as i32, rect.y1 as i32);
        let (from, to) = if dy > 0 { ((y0, y1 - k), (y0 + k, y1)) } else { ((y0 + k, y1), (y0, y1 - k)) };
        let gl = &self.gl;
        let (target, scratch) = (self.target.as_ref().unwrap().framebuffer, self.scratch.as_ref().unwrap().framebuffer);
        unsafe {
            (gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, target);
            (gl.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, scratch);
            (gl.blit_framebuffer)(
                x0,
                from.0,
                x1,
                from.1,
                x0,
                from.0,
                x1,
                from.1,
                GL_COLOR_BUFFER_BIT,
                GL_NEAREST as u32,
            );
            (gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, scratch);
            (gl.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, target);
            (gl.blit_framebuffer)(x0, from.0, x1, from.1, x0, to.0, x1, to.1, GL_COLOR_BUFFER_BIT, GL_NEAREST as u32);
            (gl.bind_framebuffer)(GL_FRAMEBUFFER, target);
        }
    }

    fn drop_texture(&mut self, serial: u64) {
        if let Some(index) = self.textures.iter().position(|t| t.0 == serial) {
            let (_, texture, _) = self.textures.swap_remove(index);
            unsafe { (self.gl.delete_textures)(1, &texture) };
        }
    }

    /// 读回渲染目标，格式与 CPU 画布相同（0x00RRGGBB，第 0 行在最上面）
    #[cfg(test)]
    pub(super) fn read(&self) -> Vec<u32> {
        let (w, h) = self.size;
        let mut bytes = vec![0u8; w * h * 4];
        unsafe {
            (self.gl.bind_framebuffer)(GL_FRAMEBUFFER, self.target.as_ref().unwrap().framebuffer);
            (self.gl.read_pixels)(0, 0, w as i32, h as i32, GL_RGBA, GL_UNSIGNED_BYTE, bytes.as_mut_ptr().cast());
        }
        let (pixels, _) = bytes.as_chunks::<4>();
        pixels.iter().map(|p| (u32::from(p[0]) << 16) | (u32::from(p[1]) << 8) | u32::from(p[2])).collect()
    }
}

impl super::super::gpu::Backend for Gles {
    fn name(&self) -> &str {
        &self.renderer
    }
    fn software(&self) -> bool {
        self.software
    }
    fn present(&mut self, list: &DrawList, width: usize, height: usize) -> Result<bool, String> {
        Gles::present(self, list, width, height)
    }
}

impl Drop for Gles {
    fn drop(&mut self) {
        self.drop_target();
        let gl = &self.gl;
        unsafe {
            for texture in self.atlases.iter().flatten().chain(self.textures.iter().map(|t| &t.1)) {
                (gl.delete_textures)(1, texture);
            }
            for program in &self.programs {
                (gl.delete_program)(program.id);
            }
            (gl.delete_buffers)(1, &self.buffer);
            (gl.delete_vertex_arrays)(1, &self.vertex_array);
            let egl = &self.egl;
            (egl.make_current)(self.display, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            if !self.surface.is_null() {
                (egl.destroy_surface)(self.display, self.surface);
            }
            if !self.context.is_null() {
                (egl.destroy_context)(self.display, self.context);
            }
            (egl.terminate)(self.display);
        }
    }
}

/// 需要本机 X 服务器和 EGL：`ci/check-x11-gles.sh` 在 Xvfb（Mesa llvmpipe）下运行
#[cfg(test)]
mod tests {
    use super::super::{Window, XDefaultVisual, XVisualIDFromVisual};
    use super::*;
    use crate::render::list::tests::{GpuTarget, gpu_scenarios};

    /// 字段顺序就是析构顺序：EGL 表面先于 X 连接释放
    struct Backend {
        gles: Gles,
        _window: Window,
    }

    impl GpuTarget for Backend {
        fn replay(&mut self, list: &DrawList, width: usize, height: usize) {
            self.gles.present(list, width, height).unwrap();
        }
        fn read(&mut self, width: usize, height: usize) -> Option<Vec<u32>> {
            (self.gles.target.is_some() && self.gles.size == (width, height)).then(|| self.gles.read())
        }
    }

    #[test]
    #[ignore = "需要 X 服务器和 EGL"]
    fn gpu_replay_matches_cpu_within_one_per_channel() {
        let window = Window::new("congminal gles test", |_| (64, 64)).expect("需要 DISPLAY");
        let visual = unsafe { XVisualIDFromVisual(XDefaultVisual(window.display, window.screen)) };
        let gles = Gles::new(window.display, window.window, visual).expect("EGL/GLES 不可用");
        eprintln!("GLES：{}", gles.renderer);
        gpu_scenarios(&mut Backend { gles, _window: window });
    }
}
