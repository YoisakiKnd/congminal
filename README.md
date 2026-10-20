# congminal

一个从零手写的跨平台终端模拟器（Rust 学习项目）。

**当前无 Rust 第三方依赖**：`Cargo.toml` 的 `[dependencies]` 是空的，现有底层通过系统 API（libc、Xlib、Win32、Cocoa）实现。
后续允许引入第三方包或外部程序，但先测量性能与资源影响；不达标时优化、更换或自写适配，见 [依赖与适配策略](docs/dependency-policy.md)。

## 运行

```sh
cargo run --release
cargo test
```

## 快捷键

| 操作 | macOS | Windows / Linux |
|---|---|---|
| 复制选中的文字 | Cmd+C | Ctrl+Shift+C |
| 粘贴 | Cmd+V | Ctrl+Shift+V、Shift+Insert |
| 往回看历史 | 滚轮 / 触控板，Shift+PageUp / PageDown | 同左 |
| 回到最顶 / 最底 | Shift+Home / Shift+End | 同左 |
| 字体放大 / 缩小 / 复位 | Cmd+Equal（或 Plus）/ Minus / 0 | Ctrl+Shift+Equal（或 Plus）/ Minus / 0 |
| 循环切换主题 | Cmd+F6 | Ctrl+Shift+F6 |
| 查看资源快照 | Cmd+F12 | Ctrl+Shift+F12 |
| 搜索历史 | Cmd+F | Ctrl+Shift+F |
| 后一项 / 前一项 | Enter / Shift+Enter（搜索栏内），F3 / Shift+F3 | 同左 |
| 打开链接 / 复制链接目标 | Cmd+单击 / Cmd+Shift+单击 | Ctrl+单击 / Ctrl+Shift+单击 |

快捷键可在 `[keybindings]` 中覆盖；字号调整会合并连续请求并在后台加载，范围为 4–200 逻辑像素。

搜索支持中文输入、粘贴、匹配高亮和 Esc 返回终端；精确区分大小写。命中缓存最多 1024 条，`…` 表示扫描中，`+` 表示还有内容，继续导航会发现更多。
链接支持 OSC 8 与可见 HTTP/HTTPS URL，悬停显示真实目标；Linux 打开链接需要 `gio`，缺失时仍可复制地址。

鼠标：拖动选择，双击选词，三击选整行（包括自动换行的部分），中键粘贴。
程序开启了鼠标模式时（vim 的 `mouse=a`、tmux、htop），点击、拖动、滚轮会发给程序；这时按住 Shift 仍然可以选文字。
没开鼠标模式的全屏程序（比如 less）里，滚轮会变成上下方向键。

## 配置

配置文件是 TOML 格式，位置：

- macOS / Linux：`~/.config/congminal/congminal.toml`（设置了 `XDG_CONFIG_HOME` 时在它下面）
- Windows：`%APPDATA%\congminal\congminal.toml`

先生成一份带注释的默认配置，再按需修改：

```sh
mkdir -p ~/.config/congminal
congminal --print-config > ~/.config/congminal/congminal.toml
```

可以配置字体（多个字体、字号、行高）、窗口初始大小和内边距、主题（dark / light / midnight）、颜色和调色板、快捷键、光标形状、
shell 程序和参数、工作目录、环境变量，以及 macOS 上 Option 键是否当 Alt 用。
完整说明见 [`assets/congminal.example.toml`](assets/congminal.example.toml)。

保存后自动生效（字体、主题、颜色、快捷键、内边距、光标、Option 键）；shell 和窗口大小在下次启动时生效。
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

支持编程连字：`->` `=>` `!=` `===` `<=` 这类符号会显示成连起来的样子（字体的 `calt` 功能，Maple Mono、Fira Code、JetBrains Mono 等都有）。
不喜欢的话在配置文件里写 `ligatures = false`。光标所在的字不参与连字，方便看清楚正在编辑的是哪个符号。

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
| `src/term/` | 终端状态机：光标、滚动区域、备用屏幕、SGR 属性、宽字符、回滚历史…… | 否 |
| `src/selection.rs` | 鼠标选区：拖动、双击选词、三击选行，取出选中的文字 | 否 |
| `src/mouse.rs` | 鼠标上报：把点击、拖动、滚轮编码成 xterm 鼠标协议的转义序列 | 否 |
| `src/input.rs` | 按键编码成字节序列 | 否 |
| `src/action.rs` | 可配置动作、按键规范化与焦点路由 | 否 |
| `src/search.rs` / `src/text.rs` | 分片搜索、软换行文本与坐标映射 | 否 |
| `src/links.rs` / `src/launcher.rs` | 有界链接目标、可见 URL 命中与桌面打开适配 | 打开接口相关 |
| `src/ui.rs` | 搜索栏与高亮模型，复用软件渲染 | 否 |
| `src/session.rs` | 单会话终端状态、PTY、输出消费及关闭生命周期 | 通过 PTY/鼠标事件接口 |
| `src/tasks.rs` / `src/resources.rs` | 有限后台任务、取消/版本校验和按需资源快照 | 否 |
| `src/config/` | 配置文件：手写的 TOML 子集解析器、选项校验 | 否 |
| `src/font/` | 字体：TrueType 解析和光栅化、编程连字（GSUB）、彩色 emoji 表、系统字体查找、mmap、PSF 点阵字体 | 否 |
| `src/image/` | zlib 解压、PNG 解码、图像缩放（emoji 用） | 否 |
| `src/render.rs` | 软件渲染到像素缓冲区 | 否 |
| `src/platform/` | 窗口和事件循环：X11 / Win32 / Cocoa | 是 |

## 开发计划

历史搜索、缩放、链接、快捷键、Shell 集成、多标签/分屏、左右侧栏、SSH/SFTP、
监控、Git、通知与终端美化的分阶段范围和验收标准见 [功能开发计划](docs/roadmap.md)。

## 资源边界与回归验证

PTY 收发队列、回滚历史、字体缓存、图片解压和帧缓冲均有明确预算。
大量输出会向子进程施加背压；输入排队超过预算时整笔拒绝并提示。
配置的历史行数是上限，实际保留内容同时受 64 MiB 行存储预算限制。
详细预算、测试方法和后续范围见 [底层加固说明](docs/foundations.md)。

## 已知限制

- 彩色 emoji：macOS 的 sbix、Linux 的 CBDT、Windows 的 COLR 都是自己解析的。
  👨‍👩‍👧、🇨🇳、👍🏽、1️⃣ 这类组合序列会合成一个字形（Linux、Windows 用字体的 GSUB 规则，macOS 用 Apple 的 morx 状态机，也是自己解析的）。
  为了和 shell 算的宽度一致，组合序列仍然按单个字符占格子（👨‍👩‍👧 占 6 格），合成的字形画在开头两格。
  程序可以用模式 2027（`ESC [ ? 2027 h`）声明自己按字素簇算宽度，这时整个组合只占 2 格
- 只支持 TrueType 轮廓的字体；OTF/CFF 字体（比如思源黑体的 .otf/.ttc）会被跳过
- 连字只支持"一个字换一个字"的方式（编程字体都是这样），把多个字合成一个字形的不支持；没有斜体字体（斜体属性按正常字体显示）
- 组合字符：能合成的（e + ◌́ → é、韩文字母 → 音节）画合成后的字；合不进去的符号直接叠在字上，没有按字形精确定位，多个符号会重叠
- 程序正在使用备用屏幕（vim、less 等）时缩放窗口，主屏幕的内容不重排，只截断
- 输入法：macOS 实现了 `NSTextInputClient`，预编辑文字显示在光标处、候选词窗口跟随光标（已在真机验证）；Windows 用系统自己的输入框（尚未在真机验证）；Linux 只有基本的 XIM 支持

## 授权

`assets/fonts/` 里是 Terminus Font，使用 SIL Open Font License 1.1 授权，见 `assets/fonts/LICENSE-Terminus.txt`。
Maple Mono 不随程序分发，需要另外安装。

本轮评估、实施目标、已实现内容与剩余验证见 [M1 执行记录](docs/execution-m1.md)，测量数据见 [M1 性能评估](docs/evaluations/m1.md)。

M2 已实现搜索与链接，使用方法、资源边界及剩余验证见 [M2 执行记录](docs/execution-m2.md)。

Shell 集成与命令输出导航已加入：可选 source zsh/Bash 脚本，Primary+↑/↓ 跳命令，Primary+F7/F8 选择/复制输出，Primary+F9 查看状态，Primary+F10 查看通知，Primary+F11 静音。Primary 在 macOS 为 Cmd，其他平台为 Ctrl+Shift。系统通知适配默认关闭，资源边界、启用方式与未验证项目见 [M3 使用说明](docs/execution-m3.md)。
