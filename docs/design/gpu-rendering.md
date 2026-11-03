# GPU 渲染设计

基于 `718b1d3`。本稿先单独提交，再分阶段实现；不新增 crate，图形 API 全部通过运行时加载的系统库调用。按[依赖策略](../dependency-policy.md)，GPU 后端在各平台完成同负载对照前保持默认关闭，CPU 渲染是永久保留的回退路径，不是过渡代码。

## 1. 现状与瓶颈

所有绘制都在 CPU 上写入 `Framebuffer`（`src/render.rs`），平台层只负责把像素交给系统：

| 平台 | 呈现 | 每帧 CPU 工作 |
| --- | --- | --- |
| Linux X11 | MIT-SHM，失败退回 `XPutImage`，按 damage 分段 | 填充、逐像素混合字形、提交 |
| macOS | damage 段做成 `CGImage`，设为局部 `CALayer` 内容 | 同上，另有快照复制 |
| Windows | `SetDIBitsToDevice` 按段提交 | 同上 |

增量机制已经完整：行指纹（`RowKey`）决定哪些行重画，整屏滚动用 `scroll_up/scroll_down` 平移已有像素，damage 最多八段。剩余成本集中在两处：

1. **填充和混合**：重画一行要对整行像素 `fill_rect`，再对每个字形逐像素 `mix`/`over`。4K 下整屏重画是 3840×2160 次写入加上字形混合，与分辨率成正比。
2. **提交**：像素要复制到共享段/CGImage/DIB，再由系统合成器上传到 GPU。

GPU 渲染要去掉的是这两项；行指纹、damage、字形光栅化和缓存继续留在 CPU，它们与分辨率无关或已经被缓存。

## 2. 架构：同一套绘制操作，两种执行方式

生产代码里绘制几乎都经过 `Framebuffer` 的少数操作（直接读写 `pixels` 的地方除测试外只有图片混合、认证覆盖层和工作区清屏几处，迁移时逐一改成操作）。因此不改上层逻辑，只替换这些操作的执行者：

| 操作 | CPU 执行（现状） | GPU 执行 |
| --- | --- | --- |
| `fill_rect` | 写像素 | 实例化矩形，不混合 |
| `blend_rect` | `mix` | 实例化矩形，`SRC_ALPHA, ONE_MINUS_SRC_ALPHA` |
| 灰度字形 | 逐像素 `mix` | 图集采样 alpha，同上混合 |
| 彩色字形（预乘） | 逐像素 `over` | 图集采样 RGBA，`ONE, ONE_MINUS_SRC_ALPHA` |
| Kitty 图片 | 逐像素采样混合 | 每张图一张纹理，同样的采样规则 |
| `scroll_up/down` | `copy_within` | 渲染目标内部区域复制 |
| `view`（工作区窗格） | 带 stride 的借用切片 | 原点偏移 + 裁剪矩形 |

`Framebuffer` 的存储改为二选一：现有的像素（`PixelStorage`），或一个**绘制记录器**。记录器把操作追加成定长实例（位置、尺寸、颜色、图集坐标、裁剪），不分配每帧对象；`view` 产生共享同一记录器的子视图。上层的行指纹、平移、damage 计算完全不变，所以增量语义不变。

GPU 后端持有一张与窗口同尺寸的**持久渲染目标**，每帧按顺序回放记录：区域复制、矩形、字形、图片。回放后只把 damage 段（或整个目标）画到交换链/drawable 上。持久目标保证“没重画的行保留上一帧内容”这一前提在 GPU 上同样成立。尺寸变化、设备丢失时目标内容作废，调用 `Renderer::invalidate()` 触发整屏重画。

混合在非线性 sRGB 值上进行（与 CPU 的 `mix` 一致），渲染目标用 8 位 UNORM、非 sRGB 格式。GPU 与 CPU 的结果每通道最多差 1（舍入方式不同），验收以此为容差。

### 2.1 字形图集

- 键：`(字符, 粗体, 斜体, emoji, 跨格数, 字体代次)`，与 `Fonts::glyph` 的缓存键一致；换字体、改字号、DPI 变化时代次递增，图集整体作废。
- 两张图集：灰度（R8）和彩色（RGBA8，预乘）。按行装箱（shelf packing），满了先整体清空重建本帧用到的字形，仍放不下再扩容到上限；超过上限的字形退回 CPU 光栅化后作为临时纹理绘制，不丢字。
- 上传只发生在首次使用：CPU 端 `Glyph` 位图仍由现有缓存持有，上传后不复制第二份。

### 2.2 资源账本

GPU 显存不在进程堆里，但必须计入预算：渲染目标、图集、实例缓冲、图片纹理按“宽 × 高 × 每像素字节”申请 `Canvas` 类票据（与现有画布共享额度），申请失败时不创建 GPU 资源、整窗退回 CPU。驱动内部的额外开销无法测量，在验收报告中注明为“未计入”，不把票据当作显存上限。

### 2.3 选择与回退

新配置项 `renderer = "cpu" | "gpu" | "auto"`，需要重启生效。阶段完成前默认 `cpu`；某平台验收通过后，该平台的 `auto` 才优先 GPU。

以下任一情况整窗退回 CPU，并在 `CONGMINAL_TRACE` 记录原因：库加载失败、上下文/设备创建失败、远程 `DISPLAY`、软件光栅器（llvmpipe、WARP、Apple 软件渲染，`auto` 下视为不可用，`gpu` 下照用）、着色器编译失败、票据被拒、运行中设备丢失且重建失败。退回只发生在帧之间：先 `invalidate()`，下一帧由 CPU 整屏重画，不出现半帧。

## 3. 各平台后端

| 平台 | API | 加载 | 着色器 |
| --- | --- | --- | --- |
| Linux | EGL + OpenGL ES 3.0 | `dlopen` `libEGL.so.1`、`libGLESv2.so.2` | GLSL ES 源码，运行时编译 |
| macOS | Metal + `CAMetalLayer` | 系统框架，经现有 objc 消息封装 | MSL 源码，`newLibraryWithSource` |
| Windows | Direct3D 11 + DXGI 翻转模型交换链 | `LoadLibrary` `d3d11.dll`、`dxgi.dll`、`d3dcompiler_47.dll` | HLSL 源码，`D3DCompile` |

选型理由：

- **Linux 用 EGL 而不是 GLX**：EGL 同时覆盖 X11 和以后的 Wayland，Mesa、NVIDIA 都提供。GLES 3.0 有实例化绘制和 R8 纹理，够用，不需要桌面 GL 的兼容层。
- **macOS 用 Metal**：OpenGL 在 macOS 上已弃用。`CAMetalLayer` 替换现有的 CGImage 分层呈现。
- **Windows 用 D3D11 而不是 OpenGL**：D3D11 在所有 Win10 机器上都有，OpenGL 驱动质量参差，远程桌面下还会退化成 GDI 软件实现。`d3dcompiler_47.dll` 是 Win10 系统组件，不需要构建期工具。

垂直同步：交换链/drawable 按显示器刷新节奏提交，但空闲时不提交、不唤醒，沿用现有的 `deadline()`/damage 驱动，不引入渲染循环。

## 4. 验收测量

每个平台、每个阶段都要有同负载对照，原始样本写入 `docs/evaluations/gpu-gN/`：

| 项目 | 场景 | 要求 |
| --- | --- | --- |
| 正确性 | 现有渲染测试的场景逐帧截图，CPU 与 GPU 对比 | 每通道差 ≤ 1；字形、图片、选区、光标、滚动条、工作区边框位置完全一致 |
| 吞吐 | 1920×1080 与 3840×2160，200 列，`seq 1 4000000`；整屏刷新的 TUI（`cmatrix` 类） | 终端进程 CPU 时间、总耗时，五次中位数；GPU 不得慢于 CPU |
| 帧时间 | `bench_full_frame` 同场景整屏重画；单行更新 | 记录 P50/P95/P99，4K 整屏 P95 应明显下降 |
| 输入延迟 | 按键到呈现 | P95 增量不超过 1 ms |
| 启动 | 冷/热启动到首帧 | 上下文创建若超过 20 ms，延后到首帧之后或放进启动并行段 |
| 空闲 | 静默 60 秒 | 无新增唤醒、线程；CPU 增量 ≤ 0.2 个百分点 |
| 内存 | RSS、Canvas 票据、图集占用 | 列出 GPU 票据明细；RSS 增量注明含驱动 |
| 回退 | 强制各失败点 | 都能退回 CPU，画面完整，无资源泄漏 |

Linux 在本机 Xvfb（Mesa llvmpipe）验证正确性与回退；llvmpipe 是软件光栅器，**它的性能数字不能代表 GPU**，只用来确认没有退化到不可用。真实 GPU 的性能数字需要在有显卡的机器上测，没有测到就标记“未验证”。macOS、Windows 由真机测试，提供 `CONGMINAL_TRACE` 日志和截图对比工具。

## 5. 阶段

| 阶段 | 内容 | 验证 |
| --- | --- | --- |
| G0 | 本文；CPU 基线数据 | 基线写入 `docs/evaluations/gpu-g0/` |
| G1 | `Framebuffer` 记录器 + CPU 回放器；迁移直接读写像素的生产代码 | 现有全部渲染测试在“记录 → CPU 回放”下与直接绘制逐字节相同 |
| G2 | Linux EGL/GLES 后端：持久目标、图集、实例化回放、呈现、回退、配置项 | Xvfb 截图对比；回退注入测试；llvmpipe 下可用 |
| G3 | macOS Metal 后端 | 用户 Mac 真机：截图对比、trace、性能对照 |
| G4 | Windows D3D11 后端 | 用户 Windows 真机：同上 |
| G5 | 各平台按验收结果决定 `auto` 的默认值，更新文档 | 发布说明列出各平台状态与未验证项 |

G1 不改变任何用户可见行为，是后面所有后端的共同基础；它也让 CPU 路径多了一个可测的“只记录”模式，用来单独测量上层逻辑的开销。
