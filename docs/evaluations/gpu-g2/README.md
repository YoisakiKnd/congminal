# G2：Linux EGL + GLES 3 后端

基于 G1 `da19f57`（[设计](../../design/gpu-rendering.md)）。默认仍是 `window.renderer = "cpu"`，不设置时用户可见行为不变。

## 改动

| 文件 | 内容 |
| --- | --- |
| src/platform/x11/gles.rs | 运行时加载 `libEGL.so.1`、`libGLESv2.so.2`；按窗口视觉类型选 EGL 配置；持久渲染目标 + 暂存目标（区域平移）、灰度/彩色图集、每张图片一张纹理；矩形、灰度字形、彩色字形三类实例化批次，遇到上传、清空、平移、图片时先提交；回放后整张翻转拷到窗口。资源按字节计入 Canvas 额度。 |
| src/platform/x11.rs | `set_renderer`；首帧之后创建 GPU（`auto` 遇到软件光栅器不用）；记录模式的帧交给 GPU，换了记录、记录溢出时作废并发 `Event::Redraw`，渲染目标重建时只整屏重画；出错时关闭 GPU、应用切回像素模式并整屏重画。 |
| src/platform/mod.rs、src/main.rs、src/workspace.rs | `App::set_recording`：单窗格画布与工作区画布在记录/像素模式之间切换。 |
| src/render.rs、src/render_list.rs | `Framebuffer::set_recording`；去掉每帧的 `begin_frame`：回放后第一条新操作自动清空旧记录；`DrawList::id`/`overflowed`。G1 的测试场景抽成可复用的 `terminal_scenario`/`views_scenario`/`atlas_scenario`。 |
| src/config/mod.rs、docs/configuration.md | `window.renderer = "cpu" | "gpu" | "auto"`，重启生效。 |
| src/platform/macos.rs、win32.rs | `set_renderer` 只记录 trace，仍是 CPU（G3/G4）。 |
| ci/check-x11-gles.sh、ci/check-x11-close.sh | 见下。 |

## 正确性

### 回放对照单元测试

`platform::x11::gles::tests::gpu_replay_matches_cpu_within_one_per_channel`（`#[ignore]`，需要 X 服务器和 EGL）。跑 G1 的全部三个场景：1、2 倍缩放下的文字、CJK、装饰线、彩色 emoji、选区、四种光标、命令标记、整屏上滚、回看滚动条、两种图片放置、预编辑；工作区视图原点与越界裁剪、面板；图集写满后清空。每一步：

1. 读回 GPU 渲染目标作为本帧起点，GPU 回放本帧记录后再读回；
2. 软件回放器（G1 已证明与直接绘制逐字节相同）从同一起点回放；
3. 只经过填充的像素必须完全相同，本帧每经过一次混合，每通道容差加 1。

为什么不是“每通道最多差 1”：第一次按 G0 设计的固定容差比较时，`┌──────┐` 相邻两个 `─` 字形重叠的像素差 2。llvmpipe 对 `src·a` 和 `dst·(1−a)` 分别舍入，每次混合最多差 1，两层叠加差 2；这是固定功能混合的性质，不是回放错误。设计文档已按此改写验收口径。

测试能发现真实错误：把灰度字形采样偏移一列，第 0 步就失败（`CPU 292929，GPU afafaf`）；把平移方向反过来，第 6 步（整屏滚动）失败（`CPU 1e1e1e，GPU 264f78`）。

### 真实窗口截图（`ci/check-x11-gles.sh`）

Xvfb 1024×768×24，Mesa 25.2.8 llvmpipe（OpenGL ES 3.2）。同一段输出（框线、中文、emoji、16 色、粗体、下划线、60 行滚动），每种渲染方式依次截图：初始单窗格 → `Ctrl+Shift+D` 分屏 → `Ctrl+Shift+W` 关回单窗格 → 窗口改为 900×560。原始输出见 [check-x11-gles.log](check-x11-gles.log)。

| 步骤 | GPU 与 CPU：最大差 / 不同像素 | auto、创建时故障、运行中故障 与 CPU |
| --- | --- | --- |
| 单窗格 | 1 / 8143（共 302848） | 逐字节相同（运行中故障在第 3 帧，这一步可能还没触发，按 GPU 容差比较） |
| 分屏（工作区画布） | 1 / 15349 | 逐字节相同 |
| 关回单窗格（换回另一份记录） | 1 / 7980 | 逐字节相同 |
| 改尺寸（渲染目标重建） | 1 / 10739（共 504000） | 逐字节相同 |

另外手动在 700×420、900×600、500×300 之间放大缩小，GPU 与 CPU 最大差都是 1。

### 回退

| 失败点 | 结果 |
| --- | --- |
| `auto` + 软件光栅器 | trace：`GPU 不可用，使用 CPU：llvmpipe … 是软件光栅器`，画面与 CPU 逐字节相同 |
| 创建时（`CONGMINAL_GPU_FAULT=init`） | trace：`GPU 不可用，使用 CPU：CONGMINAL_GPU_FAULT 注入的故障`，同上 |
| 运行中第 3 帧（`CONGMINAL_GPU_FAULT=present`） | trace：`GPU 出错，退回 CPU：…`，退回后的三张截图与 CPU 逐字节相同 |
| 窗口被直接销毁（`ci/check-x11-close.sh`，cpu 与 gpu 各一次） | 进程和 shell 子进程都退出，退出码 0，没有 X 错误 |

缺库、远程 DISPLAY、没有匹配视觉类型的配置、着色器编译失败、额度不足走同一条 `Err` → CPU 路径，没有逐个注入。

## 启动、空闲、内存

本机：AMD EPYC 7B13，4 核，Xvfb。

- **上下文创建**：llvmpipe 上 45–48 ms（trace `创建用时`），超过设计的 20 ms 门槛，所以放到首帧之后：首帧照常由 CPU 画出并提交，随后创建 GPU、整屏重画一次。首帧时间与 CPU 路径相同。
- **空闲 20 秒**（输出结束、光标不闪烁）：CPU 与 GPU 模式进程 CPU 时间增量都是 0 tick。
- **线程**：CPU 6 个，GPU 15 个；多出的 9 个是 llvmpipe 的光栅化线程，空闲时不唤醒。
- **RSS**：CPU 10.8 MB，GPU 119.2 MB。增量主要是 Mesa/LLVM 的库映射和 JIT，**含驱动**，不能当作本程序的显存占用。
- **Canvas 额度**（本程序申请的部分）：渲染目标 + 暂存目标 `宽×高×8` 字节，灰度图集 4 MiB，彩色图集 16 MiB（首次用到彩色字形才建），图片纹理按 RGBA 字节。728×416 的窗口：2,422,784 B + 4 MiB（+ 16 MiB）；1920×1080：16,588,800 B + 4 MiB（+ 16 MiB）。驱动内部开销未计入。

## 性能（llvmpipe，不代表 GPU）

`seq 1 400000`，200×50，`/usr/bin/time`，五次中位数（原始数据 [seq-400000.txt](seq-400000.txt)）：

| | 总耗时 s | 用户态 s | 内核态 s |
| --- | ---: | ---: | ---: |
| CPU | 0.51 | 0.18 | 0.76 |
| GPU（llvmpipe） | 0.88 | 1.31 | 0.76 |

llvmpipe 用 CPU 线程做光栅化，比直接写像素慢是预期的；这里只说明软件光栅器下仍然可用，`auto` 也因此不选它。CPU 默认路径的绘制代码没有改动（平台层只多了一次 `fb.list()` 判断），没有重跑基准。

## 未验证

- 真实 GPU（Mesa 硬件驱动、NVIDIA）上的正确性与性能：本机没有显卡。设计要求的 1080p/4K 吞吐、整屏 TUI、帧时间 P50/P95/P99、按键到呈现延迟都还没有测，G5 决定 `auto` 默认值之前需要在有显卡的 Linux 机器上补测。
- 硬件驱动的上下文创建时间。
- Wayland：EGL 已经覆盖，但窗口层仍是 X11。
- `ci/check-x11-gles.sh` 还没加进 CI 流水线（与 `check-x11-shm.sh` 一样手动运行），runner 需要 `libegl1 libgles2 libegl-mesa0`。
