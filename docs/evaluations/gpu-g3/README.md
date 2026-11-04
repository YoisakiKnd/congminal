# G3：macOS Metal 后端

基于 G2 `6780aff`（[设计](../../design/gpu-rendering.md)）。默认仍是 `window.renderer = "cpu"`。

**状态：已实现，只在 Linux 上做过交叉编译检查（aarch64/x86_64-apple-darwin 的 clippy、1.88），还没有在 Mac 上运行过。** 下面“真机验证”一节的结果填进来之前，本阶段不算完成。

## 改动

| 文件 | 内容 |
| --- | --- |
| src/platform/macos/metal.rs | 运行时加载 Metal.framework；MSL 源码运行时编译成四条管线（矩形、灰度字形、彩色字形、图片）；持久渲染目标 + 暂存目标（BGRA8，private），灰度/彩色图集，每张图片一张纹理；每帧一个命令缓冲：上传按段提到段首经暂存缓冲 blit，绘制按批次实例化，平移两次 blit，最后拷进 drawable；上一帧命令缓冲出错时报告并退回 CPU。 |
| src/platform/macos.rs、presentation.rs | `set_renderer`；首帧之后创建 Metal 并叠 `CAMetalLayer`；第一帧 GPU 画面出现时清掉 CPU 图层内容、补丁图层和快照池；trace 输出窗口编号。 |
| src/platform/gpu.rs | 从 X11 抽出的共用逻辑：首帧后创建、`auto` 判断软件光栅器、换记录或溢出作废、渲染目标重建整屏重画、出错退回、`CONGMINAL_GPU_FAULT` 注入。X11 改为使用它。 |
| src/platform/x11/gles.rs | 混合只作用于颜色（`glBlendFuncSeparate`，alpha 用 0/1），目标 alpha 保持 1，与 Metal 一致。 |
| src/render_list.rs | GPU 对照测试的公共部分（`GpuTarget`、`gpu_scenarios`、按混合次数给容差）从 GLES 测试移过来，两个后端共用。 |
| .github/workflows/ci.yml | Linux 装 Mesa EGL/GLES，在 Xvfb 下跑 GLES 对照测试；macOS 跑 Metal 对照测试（托管运行机没有 Metal 设备时只留 notice）。 |
| ci/check-macos-metal.sh | 真机验证脚本，见下。 |

与 GLES 的差别：

- **上传不打断绘制**：Apple GPU 分块渲染，每个渲染 pass 都加载、存回整张目标。两次清空图集/纹理之间字形槽位不复用，所以这段的上传全部提到段首做成一次 blit；首帧几百个新字形也只有几个 pass。
- **不用 `replaceRegion` 上传**：它由 CPU 立即写纹理，会越过同一帧里还没执行的绘制（图集清空后复用槽位时画错字）。改为写进共享暂存缓冲，在命令缓冲里按序 blit。
- **`presentsWithTransaction`**：drawable 与 Core Animation 事务一起生效，切到 GPU 时 CPU 图层内容清掉与 Metal 画面出现在同一帧，不闪。图层在第一帧呈现之后才设为不透明。
- 第 0 行在纹理最上面，Metal 的 NDC 与帧缓冲坐标都不用翻转；渲染目标 BGRA8 与 CPU 像素同一字节序，`CAMetalLayer.colorspace` 与 CPU 路径的 CGImage 同为 sRGB。

## 本机（Linux）已做

- `cargo clippy --all-targets -D warnings`：Linux、Windows GNU、aarch64-apple-darwin、x86_64-apple-darwin；`cargo +1.88 check`；rustfmt。
- GLES 后端改用共用模块后：GLES 对照单元测试、`ci/check-x11-gles.sh`（单窗格、分屏、关回、改尺寸、三种回退）、`ci/check-x11-close.sh`（cpu、gpu）全部通过。
- 全部测试除 3 个已有的 SFTP 本地传输测试外通过（与 G0 起一致，本机临时目录权限问题）。

## 真机验证（待做）

在 Mac 上，仓库根目录：

```sh
cargo build --release
ci/check-macos-metal.sh
```

终端需要“屏幕录制”权限；运行时新窗口要保持在最前面。脚本会：

1. 跑 `platform::macos::metal` 回放对照单元测试：G1 全部场景（1、2 倍缩放的文字、CJK、装饰线、彩色 emoji、选区、光标、命令标记、整屏滚动、回看、两种图片、预编辑；工作区视图与裁剪；图集写满清空），每步与软件回放比较，只经过填充的像素必须相同，每多一次混合容差加 1。
2. 用 cpu、gpu、auto 各跑一次同一段输出，按窗口编号截图：gpu、auto 与 CPU 每通道差 ≤ 3。
3. 创建时、运行中第 3 帧注入故障：trace 写明原因，截图与 CPU 逐字节相同。

需要带回来的：脚本完整输出；失败时结果目录里的 `*.log`（trace）和 `*.bmp`。另外请手动用 `renderer = "gpu"` 试一下分屏（Cmd+D）、关窗格（Cmd+W）、拖动改窗口大小、在两块不同缩放的屏幕之间拖动窗口，看有没有错位、闪烁或残影。

| 项目 | 结果 |
| --- | --- |
| 单元测试 | 未验证 |
| 截图对照 | 未验证 |
| 回退 | 未验证 |
| 分屏、改尺寸、换屏 | 未验证 |
| 上下文创建时间（trace `创建用时`） | 未验证 |
| 性能（设计第 4 节） | 未验证 |
