# congminal

一个从零手写的跨平台终端模拟器（Rust 学习项目）。

**零第三方依赖**：`Cargo.toml` 的 `[dependencies]` 是空的，只调用操作系统自带的 API（libc、Xlib、Win32、Cocoa）。

## 运行

```sh
cargo run --release
cargo test
```

| 环境变量 | 作用 |
|---|---|
| `CONGMINAL_SHELL` | 指定要启动的 shell（默认 Unix 用 `$SHELL`，Windows 用 `%COMSPEC%`） |
| `CONGMINAL_SCALE` | 缩放比例，≥1.5 时使用 16×32 的大字体（Linux 上没有自动检测） |
| `CONGMINAL_TRACE` | 设为任意值后，把还没实现的转义序列打印到 stderr |

## CI 与测试版

`.github/workflows/ci.yml` 在 Linux、Windows、macOS 上各自执行：格式检查、clippy、单元测试、release 编译，
以及**冒烟测试**：真正启动 congminal 运行 `ci/demo.sh`（Windows 上是 `ci/demo.ps1`），
确认它启动后不崩溃、截图（在 Actions 页面的 `smoke-*` artifact 里），并且 shell 退出后它会自己退出。

每次构建的安装包都在 `congminal-*` artifact 里。推送 `v` 开头的标签会自动发布为 GitHub 预发布版本：

```sh
git tag v0.1.0-alpha.1
git push origin v0.1.0-alpha.1
```

下载后：

- **Windows**：需要 Windows 10 1809 或更新版本（ConPTY）。
- **macOS**：通用二进制，Intel 和 Apple 芯片都能用。没有签名，第一次运行前先执行
  `xattr -d com.apple.quarantine ./congminal`。
- **Linux**：需要 X11（Wayland 桌面上通过 XWayland）。在 ubuntu-latest 上编译，太旧的发行版可能因为 glibc 版本跑不起来。

## 结构

```text
 键盘 ──> platform ──Event::Key──> input::encode ──字节──> pty ──> shell
                                                                    │
 屏幕 <── platform <──像素── render <──网格── term <──动作── vt <──字节┘
```

| 模块 | 做什么 | 平台相关 |
|---|---|---|
| `src/pty/` | 启动 shell 并与之通信：Unix 用 `posix_openpt` + `fork`，Windows 用 ConPTY | 是 |
| `src/vt.rs` | 转义序列解析器（Paul Williams 状态机）+ UTF-8 解码 | 否 |
| `src/term/` | 终端状态机：光标、滚动区域、备用屏幕、SGR 属性…… | 否 |
| `src/input.rs` | 按键编码成字节序列 | 否 |
| `src/font.rs` | PSF 点阵字体解析 | 否 |
| `src/render.rs` | 软件渲染到像素缓冲区 | 否 |
| `src/platform/` | 窗口和事件循环：X11 / Win32 / Cocoa | 是 |

## 已知限制

- 中日韩等宽字符按 1 格处理，字体里也没有这些字形（显示为 `◆`）
- 没有回滚缓冲区、文本选择、复制粘贴、鼠标上报
- 窗口缩放时不重排文本
- macOS 上没有接输入法（按键直接取自 `NSEvent`）
- Windows 和 macOS 版目前只在 CI 上做过冒烟测试，还没有人在真机上用过

## 字体

`assets/fonts/` 里是 Terminus Font，使用 SIL Open Font License 1.1 授权，见 `assets/fonts/LICENSE-Terminus.txt`。
