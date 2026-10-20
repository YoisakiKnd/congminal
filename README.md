# congminal

一个从零手写的跨平台终端模拟器（Rust 学习项目）。

**零第三方依赖**：`Cargo.toml` 的 `[dependencies]` 是空的，只调用操作系统自带的 API（libc、Xlib、Win32、Cocoa）。

## 运行

```sh
cargo run --release
cargo test
```

## 配置

配置文件是 TOML 格式，位置：

- macOS / Linux：`~/.config/congminal/congminal.toml`（设置了 `XDG_CONFIG_HOME` 时在它下面）
- Windows：`%APPDATA%\congminal\congminal.toml`

先生成一份带注释的默认配置，再按需修改：

```sh
mkdir -p ~/.config/congminal
congminal --print-config > ~/.config/congminal/congminal.toml
```

可以配置字体（多个字体、字号、行高）、窗口初始大小和内边距、颜色和调色板、光标形状、
shell 程序和参数、工作目录、环境变量，以及 macOS 上 Option 键是否当 Alt 用。
完整说明见 [`assets/congminal.example.toml`](assets/congminal.example.toml)。

保存后自动生效（字体、颜色、内边距、光标、Option 键）；shell 和窗口大小在下次启动时生效。
写错的地方会用黄色文字显示在终端里，那一项保持默认值，不影响其他设置。

| 环境变量 | 作用 |
|---|---|
| `CONGMINAL_CONFIG` | 配置文件路径（也可以用 `--config <路径>`） |
| `CONGMINAL_SHELL` | 临时指定 shell 程序，优先于配置文件 |
| `CONGMINAL_FONT` | 临时指定字体（逗号分隔多个），优先于配置文件 |
| `CONGMINAL_FONT_SIZE` | 临时指定字号，优先于配置文件 |
| `CONGMINAL_SCALE` | 缩放比例（Linux 上没有自动检测，高分屏可以设成 2） |
| `CONGMINAL_TRACE` | 设为任意值后，把实际使用的字体、输入法回调和还没实现的转义序列打印到 stderr |

## 字体

默认依次寻找 **Maple Mono NF CN** → Maple Mono NF → Maple Mono CN → Maple Mono → 系统等宽字体（Menlo / Consolas / DejaVu Sans Mono）→ 内置的 Terminus 点阵字体。
可以在配置文件的 `[font]` 里改。
推荐安装 Maple Mono NF CN，它同时包含中文和 Nerd Font 图标（starship 等提示符要用）：

```sh
brew install --cask font-maple-mono-nf-cn     # macOS
```

Windows 和 Linux 可以从 [Maple Mono 的发布页](https://github.com/subframe7536/maple-font/releases) 下载 `MapleMono-NF-CN.zip`，
安装到系统字体目录即可。

主字体缺字时会去系统的中文字体里找（macOS：苹方、冬青黑体、华文黑体；Windows：微软雅黑、宋体；Linux：思源黑体、文泉驿）。

字体引擎是自己写的（`src/font/`）：TrueType 解析、二次贝塞尔轮廓、抗锯齿光栅化、按名字查找系统字体。

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
| `src/term/` | 终端状态机：光标、滚动区域、备用屏幕、SGR 属性、宽字符…… | 否 |
| `src/input.rs` | 按键编码成字节序列 | 否 |
| `src/config/` | 配置文件：手写的 TOML 子集解析器、选项校验 | 否 |
| `src/font/` | 字体：TrueType 解析和光栅化、系统字体查找、PSF 点阵字体 | 否 |
| `src/render.rs` | 软件渲染到像素缓冲区 | 否 |
| `src/platform/` | 窗口和事件循环：X11 / Win32 / Cocoa | 是 |

## 已知限制

- 彩色 emoji 不支持（macOS、Linux 上显示为方框；Windows 上会用 Segoe UI Emoji 显示单色版本，尚未在真机验证）
- 只支持 TrueType 轮廓的字体；OTF/CFF 字体（比如思源黑体的 .otf/.ttc）会被跳过
- 没有连字（ligature）、没有斜体字体（斜体属性按正常字体显示）
- 组合字符（单独的重音符号等）会被丢弃
- 没有回滚缓冲区、文本选择、复制粘贴、鼠标上报
- 窗口缩放时不重排文本
- 输入法：macOS 实现了 `NSTextInputClient`，预编辑文字显示在光标处、候选词窗口跟随光标（已在真机验证）；Windows 用系统自己的输入框（尚未在真机验证）；Linux 只有基本的 XIM 支持

## 授权

`assets/fonts/` 里是 Terminus Font，使用 SIL Open Font License 1.1 授权，见 `assets/fonts/LICENSE-Terminus.txt`。
Maple Mono 不随程序分发，需要另外安装。
