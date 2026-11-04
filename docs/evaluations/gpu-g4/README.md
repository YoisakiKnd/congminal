# G4：Windows Direct3D 11 后端

基于 G3 `ae3c8ae`（[设计](../../design/gpu-rendering.md)）。默认仍是 `window.renderer = "cpu"`。

**状态：已实现。在本机用 wine 9.0（wined3d → OpenGL → Mesa llvmpipe）跑通了回放对照和真实窗口；Windows 真机还没有验证。**

## 改动

| 文件 | 内容 |
| --- | --- |
| src/platform/win32/d3d11.rs | 从 System32 运行时加载 `d3d11.dll`、`d3dcompiler_47.dll`；HLSL（vs_4_0/ps_4_0，特性级别 10.0 起可用）运行时编译成一个顶点着色器和四个像素着色器；实例数据经动态顶点缓冲、按实例步进；持久渲染目标 + 暂存目标（BGRA8）、灰度/彩色图集、每张图片一张纹理；`UpdateSubresource` 上传；平移两次 `CopySubresourceRegion`；回放后拷进翻转模型交换链（`FLIP_SEQUENTIAL`，两个缓冲）的后台缓冲再 `Present(0, 0)`；`Present` 失败时带上 `GetDeviceRemovedReason` 退回 CPU。显卡是 WARP（厂商 0x1414）时算软件光栅器。 |
| src/platform/win32.rs | `set_renderer`；首帧之后创建；记录模式的帧交给 D3D11；设置了整窗不透明度时不启用 GPU（翻转模型不能用于分层窗口），运行中改成半透明时关掉 GPU、退回 CPU。 |
| src/platform/gpu.rs | `disable`：平台条件不再允许 GPU 时主动关掉，并且以后不再创建。`platform::gpu` 现在三个平台都编译。 |
| src/render.rs、src/render_list.rs、src/platform/mod.rs | 去掉“Windows 上没有回放”的 dead_code 放行。场景测试里“彩色字形”改为有彩色字体才要求（wine 没有）；GPU 对照测试在 CI 上把设备名写成 notice。 |
| .github/workflows/ci.yml | Windows 任务跑 D3D11 回放对照（托管运行机没有显卡，测试退到 WARP）。 |
| ci/check-windows-d3d11.ps1 | 真机验证脚本，见下。 |

## 本机验证（wine）

环境：Ubuntu 24.04，wine 9.0，mingw-w64 交叉编译（`x86_64-pc-windows-gnu`），Xvfb + Mesa 25.2.8 llvmpipe。wine 把设备报成 “NVIDIA GeForce GTX 470（特性级别 11.0）”，实际是 wined3d 转 OpenGL 再由 llvmpipe 光栅化。

### 回放对照单元测试

```sh
WINEPREFIX=~/.wine-congminal WINEDEBUG=-all DISPLAY=:97 \
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=/usr/lib/wine/wine64 \
cargo test --target x86_64-pc-windows-gnu --bin congminal d3d11 -- --ignored --nocapture
```

第一次运行：设备、着色器、管线都创建成功，但画面全黑。逐步排查（全屏常量色能画出 → 换成实例里的矩形只画出两个像素 → 把实例数据当颜色输出，后三个分量是 (0, 0, 1)）定位到输入布局的格式常量写错：`DXGI_FORMAT_R32G32B32A32_SINT` 是 4，写成了 43（`R32_SINT`），每个实例只读进了第一个分量。修正后全部场景通过：1、2 倍缩放的终端场景、工作区视图、图集写满清空，只经过填充的像素与软件回放相同，每次混合每通道最多差 1。wine 里没有彩色 emoji 字体，终端场景不含彩色字形；彩色字形由图集场景里直接构造的彩色位图覆盖。

### 真实窗口

Windows 版 release 在 wine 下运行（wine 的 ConPTY 不回传 cmd 输出，画面里只有光标；分屏后标签栏、状态栏、分隔线由本程序绘制），CPU 与 GPU 各一次，用 `xwd` 截图：

| 步骤 | GPU 与 CPU |
| --- | --- |
| 启动 | 逐字节相同 |
| `Ctrl+Shift+D` 分屏 | 逐字节相同 |
| 改为 820×500（交换链 `ResizeBuffers`、渲染目标重建） | 逐字节相同 |
| `Ctrl+Shift+W` 关回单窗格（换回另一份记录） | 逐字节相同 |
| `CONGMINAL_GPU_FAULT=present` 后分屏 | trace：`GPU 出错，退回 CPU：…`；截图与 CPU 逐字节相同（交换链释放后 GDI 重新接管窗口） |
| `appearance.opacity = 0.9` | 没有创建 GPU，trace：`不使用 GPU：半透明窗口不能使用翻转模型交换链` |

上下文创建（含 HLSL 编译）在 wine 下约 205–220 ms，已放在首帧之后。这个数字包含 wined3d 的开销，不代表真机。

wine 的 D3D11 实现与真 Windows 不同，以上只说明调用序列和语义正确；真机上的交换链行为、DWM 合成、色彩、性能仍需验证。

## 真机验证（待做）

在 Windows 上，仓库根目录（PowerShell）：

```powershell
cargo build --release
pwsh ci/check-windows-d3d11.ps1
```

运行时新窗口要保持在最前面。脚本会跑 D3D11 回放对照单元测试；用 cpu、gpu、auto 各跑一次同一段输出，截取窗口客户区比较（gpu 与 CPU 每通道差 ≤ 3；auto 在有显卡时用 GPU，只有 WARP 时退回 CPU）；再测创建时和运行中的故障退回。需要带回来的：脚本完整输出；失败时结果目录里的 `*.log` 和 `*.bmp`。另外请手动用 `renderer = "gpu"` 试分屏、关窗格、拖动改大小、最小化再恢复、在两块不同 DPI 的屏幕之间拖动。

| 项目 | 结果 |
| --- | --- |
| 单元测试 | wine 通过；CI（WARP）待下一次运行；真机未验证 |
| 截图对照 | wine 通过；真机未验证 |
| 回退 | wine 通过；真机未验证 |
| 分屏、改尺寸、最小化、换屏 | wine 下分屏、改尺寸通过；真机未验证 |
| 性能（设计第 4 节） | 未验证 |
