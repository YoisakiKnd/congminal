# congminal

开发接手入口：[DeepSeek 功能接手 / Codex 内存与性能审查](docs/DEEPSEEK_HANDOFF.md)。交接分支包含当前源码、必要工具和评估证据；请先按文档核对状态，不要把历史记录或旧安装包当作最新实现。

一个从零手写的跨平台终端模拟器（Rust 学习项目）。

**默认构建不启用 Rust 第三方包**，终端底层通过系统 API（libc、Xlib、Win32、Cocoa）实现。macOS/Linux 可显式构建 `sftp-preview` 并启用配置，接入独立 OpenSSH SFTP 界面，见 [实验预览说明](docs/releases/sftp-preview.md)；原生窗口认证/传输仍待验收，普通 Alpha.6 不包含此功能。`sftp-libssh2` 仅保留原生会话测试依赖，见 [M6 记录](docs/execution-m6.md)。

最新资源进度：会话启动/恢复/重连改为同步借用配置，字体任务只携带字体设置，移除无关配置副本。给定32KiB环境/自定义快捷键负载，启动命令构造从34764B降为零，字体请求从253142B降为148B；后者仍需完整计费。默认601/预览604及helper20项、隔离SSH连接/中断重连与SFTP回归通过，见[分配、成本和边界](docs/evaluations/m4-config-borrow.md)。此前[固定布局快照](docs/evaluations/m4-layout-resources.md)及[线程准入](docs/evaluations/m4-system-threads.md)保持；配置解析/其他元数据、真实窗口及三平台长测继续，既有安装包未更新。
后续允许引入第三方包或外部程序，但先测量性能与资源影响；不达标时优化、更换或自写适配，见 [依赖与适配策略](docs/dependency-policy.md)。

## 运行

当前本地体验版为 **0.1.0-alpha.6（macOS Apple 芯片）**，安装、快捷键、SSH 配置和功能范围见 [使用说明](docs/releases/alpha.6.md)。在 Mac 上执行 `bash ci/package-macos.sh`，会离线构建 `.app` 和 ZIP 到 `target/distributions/`；只做本机临时签名，未公证。

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
| 预览主题（方向键换、Enter 应用、Esc 取消） | Cmd+F5 | Ctrl+Shift+F5 |
| 回到配置里写的主题 | 需自行绑定 `reset-theme` | 同左 |
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

可以配置字体（多个字体、字号、行高）、窗口初始大小和内边距、主题（dark / light / midnight）、颜色和调色板、快捷键、光标形状与闪烁、
shell 程序和参数、工作目录、环境变量，以及 macOS 上 Option 键是否当 Alt 用。
完整说明见 [`assets/congminal.example.toml`](assets/congminal.example.toml)。

保存后自动生效（字体、主题、颜色、快捷键、内边距、光标、光标闪烁、Option 键）；shell 和窗口大小在下次启动时生效。
光标闪烁默认关闭：静止界面不会为了光标持续重绘，打开后每 530 ms 醒一次，且只重画光标所在的那一格。
命令行标记默认开启（`[appearance] command-marks`）：配置了 shell 集成（OSC 133）后，每条命令的提示行左侧会有一条结果色条——成功绿、失败红、中断黄，退出状态未知时不画。

整窗不透明度用 `[appearance] opacity`（0.3–1.0，默认 1.0）。默认值下完全不碰窗口属性；设成 0.8 这类值后由系统合成器混合（X11 走 `_NET_WM_WINDOW_OPACITY`，需要合成器才会真的半透明；Windows 用分层窗口，macOS 用窗口 alpha），混合不在应用里做，也不因此多画一帧。`ci/check-opacity-x11.sh` 可以在无头环境里用 `xprop` 验证写进去的值。
轮换主题按多了可以用 `reset-theme` 动作回到配置文件里写的主题（例如 `F3 = "reset-theme"`）。三个主题的配色有自动化截图回归（`every_theme_paints_its_own_palette` 按位核对背景/前景/调色板，`CONGMINAL_THEME_PREVIEW_DIR` 可导出三张帧人工比对），见 [外观设计](docs/design-appearance.md)。
不确定要哪个主题时按主题预览（`preview-themes`，默认 Cmd/Ctrl+Shift+F5）：窗口当场换成候选主题，方向键在 dark / light / midnight 之间循环，Enter 留下，Esc 退回原来那个；预览期间其它按键不会送进 shell。
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
- 终端兼容：应用小键盘（`ESC =` / `ESC >`）、窗口/字符尺寸查询（`CSI 14/16/18 t`）、
  颜色查询（`OSC 4/10/11/12`，按配置的调色板回答，程序改颜色的请求被忽略）与
  `DECRQSS`（`m`/`r`/` q`）已接通；Kitty 键盘协议接通了可协商子集（flags 栈、
  查询应答、消除歧义与全键转义编码），左右边距（`DECSLRM`）未实现
- 输入法：macOS 实现了 `NSTextInputClient`，预编辑文字显示在光标处、候选词窗口跟随光标（已在真机验证）；Windows 用系统自己的输入框（尚未在真机验证）；Linux 只有基本的 XIM 支持

## 授权

`assets/fonts/` 里是 Terminus Font，使用 SIL Open Font License 1.1 授权，见 `assets/fonts/LICENSE-Terminus.txt`。
Maple Mono 不随程序分发，需要另外安装。

本轮评估、实施目标、已实现内容与剩余验证见 [M1 执行记录](docs/execution-m1.md)，测量数据见 [M1 性能评估](docs/evaluations/m1.md)。

M2 已实现搜索与链接，使用方法、资源边界及剩余验证见 [M2 执行记录](docs/execution-m2.md)。

Shell 集成与命令输出导航已加入：`congminal --print-shell-integration bash|zsh` 直接打印可选脚本（`source <(congminal --print-shell-integration bash)`，不自动改 rc 文件），默认只加 OSC 133 标记与目录上报，`CONGMINAL_PROMPT=pretty` 才换成两行紧凑提示符（分支名 2 秒缓存）；Primary+↑/↓ 跳命令，Primary+F7/F8 选择/复制输出，Primary+F9 查看状态，Primary+F10 查看通知，Primary+F11 静音。Primary 在 macOS 为 Cmd，其他平台为 Ctrl+Shift。系统通知适配默认关闭，资源边界、启用方式与未验证项目见 [M3 使用说明](docs/execution-m3.md)。

工作区首批功能已接入：Primary+T 新标签，Primary+D / Primary+Enter 左右/上下分屏，Ctrl+Tab 切换标签，Primary+W 关闭窗格，Primary+B/I 切换侧栏。字体和缓存共享，会话最多 8 个，历史共用 64 MiB 配额。首次使用这些功能时启用工作区，普通单终端沿用直接绘制路径。详见 [M4 操作说明](docs/execution-m4.md)。

左栏顶部“本地”提供只读目录浏览、上级、刷新及隐藏文件开关；从配置的工作目录开始，普通文件只选中，不读取正文或执行。单次最多 512 项，达到扫描上限会提示结果可能不完整；详见 [目录侧栏](docs/execution-m4.md#本地目录侧栏)。

左栏“连接”读取命名配置档 `[connections.<id>]`（最多 16 个）；本地配置点击后新建独立标签，SSH 配置通过 OpenSSH 新建独立 PTY，保留终端认证/主机确认，连接页支持取消检查和显式重连。连接状态只报能判断的阶段：`启动中`（进程已起、还没有任何输出，不猜认证进行到哪一步）、`活动`（收到过输出）、`断开`（退出码 0 或未知）、`失败`（退出码非 0，状态页写“SSH 失败 · 退出码 255 · N s · 连接页可重连”）；阶段名同时显示在右栏状态页第一行与连接页对应配置档后面（截图 [`docs/previews/m7-ssh-phase.png`](docs/previews/m7-ssh-phase.png)）。重连会新建连接代次（新会话编号，不重放未发送输入与历史命令）；准备阶段就失败（程序缺失等）时旧会话照常运行，状态页另起一行写“重连失败 · 原因”。详见 [配置档说明](docs/execution-m4.md#连接配置档)。

跨运行保存标签、分屏、焦点和侧栏，可设置 `[workspace] restore-layout = true`（默认关闭，下次启动生效）。只保存 UI 元数据，恢复时启动当前配置的 Shell，不保存或重放旧命令与输入。文件为配置旁的 `<配置文件名>.workspace`；资源成本和恢复边界见 [持久化评估](docs/evaluations/m4-layout.md)。

SFTP 文件页（`sftp-preview` 构建）在目录视图的操作行提供 `[新建]` `[改名]` `[删除]`：新建与改名先输入名称（`Enter` 提交、`Esc` 取消，输入期间键盘不进终端），删除需要看到主机与绝对路径后再次确认；三者都走既有 SFTP 队列、额度与前台认证，成功后才重读目录，非空目录与同名目标不会被覆盖。窄侧栏按钮自动降级为单字，确认行保留路径末尾。限额、测试与未验收项见 [SFTP 写操作说明](docs/design-sftp-ops.md)。

右栏“Git”页显示活动窗格目录对应仓库的分支、暂存/未暂存/未跟踪条目与受限 diff（Primary+G 打开），并支持用户显式触发的暂存、取消暂存、提交、拉取、推送与切分支（按钮行 `刷新`/`差异`/`暂存`/`提交`/`拉取`/`推送`/`分支`；提交输入 Enter 提交、Esc 取消）。拉取与推送要**点两次**：第一次只进入确认态（按钮变成 `确认拉取`/`确认推送`，10 秒内有效），再点一次才执行；拉取只做 `git pull --ff-only`，分叉时失败而不生成合并提交，推送推到当前分支的上游、不带 `--force`，也不弹凭据。状态除了按 `refresh-seconds` 周期复查，还每 500 ms 比对 `.git/index`、`HEAD` 与 `refs` 的指纹：在终端里暂存、提交或切分支会立刻刷新，工作树里改文件仍等周期复查。写操作同一时间只有一个在途，关栏时等它跑完再释放，避免留下 `index.lock`；有未解决冲突时“提交”不打开输入，改为在列表里选中冲突条目点 `解决`：进入解决页后点 `保留我方` 或 `保留对方`，面板执行 `git checkout --ours|--theirs -- <路径>` 再 `git add -- <路径>` 标记已解决，成功即回到列表。点 `分支` 进分支页（`刷新`/`切到`/`返回`）：列出本地分支，当前分支带 `●` 且默认选中，点一行只选中，`切到` 同样要点两次（第一次进入确认态，10 秒内有效）才执行 `git switch`；未提交改动跟着走或由 git 自己拒绝，面板不自动 stash，也不做强制切换。分支页与状态、diff 一样按 `Kind::Git` 计费，退出分支页即释放列表。

SSH 窗格里的 Git 页走**独立只读通道**：每次查询另起一个 `ssh`（配置档里的 `program`、用户、端口、密钥，`BatchMode=yes` 不弹密码），在远端执行 `git -C <远端目录> …`，不碰终端的 stdin/stdout；远端目录来自远端 shell 报告的 OSC 7。面板状态行标 `远程 · N 变更`，按钮只有 `刷新`/`差异`——暂存、提交、拉取、推送在远端一律拒绝并提示“远程仓库暂只支持只读查看”。主机密钥未确认或认证失败时不做任何绕过：分别提示“先在终端里连接一次并核对指纹”和“先用终端连接一次，或加载密钥/agent”；远端目录还没报告时提示“远程目录未知：先在远程终端进入仓库目录”。限额与未验收项见 [Git 面板说明](docs/design-git-panel.md)；大仓库实测（2 万文件：`status` 中位 124 ms、截断到 5 000 条 234 ms）见[大仓库实测](docs/evaluations/m7-git-large-repo.md)。

界面图标（`·` `…` `→` `✓` `●` 等）在字体缺字时自动回退成 ASCII（`●`→`*`、`✓`→`v`、`…`→`.`），只作用于侧栏、页签与状态栏；终端里程序输出的缺字仍按原样显示缺字方框。见 [外观设计](docs/design-appearance.md)。

右栏“监控”页显示本机 CPU、内存、磁盘占用、网络速率与最近 5 分钟趋势（Primary+M 打开；后台 1 Hz 采样，磁盘单独降频到 10 s）。网络速率读 `/proc/net/dev`，汇总非回环接口的收发字节并按两次采样差算，首轮采样与非 Linux 本机显示“网络 未知”而不是 0。窗口失焦或切走页签即暂停采样，关闭右栏释放历史与额度。`[monitor]` 的 `alert-cpu`/`alert-memory`/`alert-disk` 可以按百分比设阈值（默认 0 = 关闭），超过就发一条 `监控 · CPU 超过阈值` 之类的通知，`alert-seconds`（默认 300 秒）内不重复；SSH 窗格里的监控页改读**远端主机**：每次采样另起一个 `ssh`（`BatchMode=yes`，不弹密码、不关主机密钥校验）执行 `cat /proc/stat /proc/meminfo /proc/net/dev`（网络速率用同一趟输出，不多开 ssh），磁盘到期那轮再加一次 `df -P -k /`；远端间隔至少 2 秒，标题标 `远程 <主机> · 往返 N ms`（主读数那一趟的耗时，不含降频的磁盘趟），读不到就显示“远程主机未返回读数”并进通知中心，切换主机即清空旧曲线。监控页底部还会列出 CPU 占用最高的 5 个进程（每 5 秒刷新一次，名字与条数都有上限）。分核 CPU 与本机 ICMP 延迟尚未接入，成本与未验收项见 [监控评估](docs/evaluations/m7-monitor.md) 与 [监控面板说明](docs/design-monitor-panel.md)。

Git 写操作、监控采样失败、SFTP 远程写操作与 SSH 连接状态都会进通知中心：应用内只写固定动作词（不含路径、分支名或提交正文），同类事件在 `dedupe-seconds` 内合并成一条并累加次数，10 秒窗口内最多 `burst` 条、超出只计数不排队；`[notifications]` 的 `git`、`monitor`、`transfers`、`ssh` 可分别关掉。SSH 断开写 `SSH · 连接已断开`，连接失败按 ssh 自己打印的特征写成 `SSH · 认证失败`/`主机校验失败`/`主机名解析失败`/`连接被拒绝`/`连接超时`，认不出就写 `SSH · 连接失败`，重连准备失败写 `SSH · 重连失败`；状态页点这条通知行会切到连接页，方便直接重连。单窗口下 Primary+F10 打开通知浮层、回车跳到对应命令输出；工作区里点击右栏“状态”页第一屏还有一行外部程序检测：启动后按配置检查 `git`（`[git] program`，默认 `git`）和各 SSH 配置档用的 `ssh`（默认 `ssh`），全部找到时显示 `外部程序 git ssh 就绪`，缺哪个就写 `外部程序：Git 页缺 git` / `外部程序：SSH/SFTP 缺 ssh`。检测只查 PATH 与执行位，不启动子进程、不起线程；程序换路径后配置重载会重新检测。缺程序时其余功能照常工作：Git 页显示“找不到 git 可执行程序”、SFTP 连接直接报错，不会假装成功。截图见 [`docs/previews/m8-tools-status.png`](docs/previews/m8-tools-status.png)。

右栏“状态”页的通知行会切到对应页面。`[notifications] hide-context = true` 会隐藏状态栏与状态页摘要里的主机名和工作目录；`mute-hosts = ["web-1"]` 可按主机静音（大小写不敏感，主机取自 shell 报告的 OSC 7 目录或 SSH 配置档，本地事件不受影响）。工作区里每个窗格各自记账：后台窗格的命令完成与窗格退出只进它自己的列表并立刻走系统通知闸门，不会刷满当前窗格。通知记录可用 `clear-notifications` 动作（按名字绑定按键）或在状态页点击“通知 N 条 · 点击清空”清掉。管线、配置与未验收项见 [通知设计](docs/design-notifications.md)。

共享资源准入已接入画布扩容峰值、PTY 队列预留、缓存计费、字体文件、主/备用网格、持久历史 cell/行索引、缩放临时工作集和 PNG 解码/返回像素；压力下先清缓存、分片回收非活动历史，仍不足则拒绝缩放并保留原终端。Primary+F12 可查看额度和拒绝次数。默认混合额度 512 MiB、字体映射另限 1024 MiB/128 个；缩放、轮廓/光栅化、GSUB/PSF、缓存桶、目录及配置档已接入；行指纹、连字/Unicode绘制工作集已准入并复用，见[评估](docs/evaluations/m4-render-work.md)；字体发现/读取头部及共享记录已准入，查询复用与成本见[评估](docs/evaluations/m4-font-discovery.md)；轮廓Box、后备列表及说明/警告已接入，见[评估](docs/evaluations/m4-font-metadata.md)；映射Storage/Arc、固定弱缓存及固定错误值已完成本机计费验证，见[评估](docs/evaluations/m4-font-map.md)；搜索/链接高亮与顶部提示栏已准入并复用，见[评估](docs/evaluations/m4-ui-state.md)；搜索查询/预编辑、KMP/检查点和匹配队列也已准入复用，见[评估](docs/evaluations/m4-search-resources.md)；通知Center的固定记录窗口已准入、清空释放，首次及重填成本增加而连续事件持平，见[评估](docs/evaluations/m4-notice-resources.md)；Shell命令/事件和OSC7目录已准入，搜索高亮改为可信切片批量复制，见[评估](docs/evaluations/m4-shell-resources.md)；根预算/同步对象、配置、系统剪贴板内部/原生协议、launcher内部、通知工具/任务及其他界面元数据仍待完整接入，不能把该额度当作完整堆/RSS 上限。[配置与边界](docs/execution-m4-memory.md)。

软换行重排逐段处理，不拼接整份逻辑长行。超过 256 KiB 的主屏宽度重排已接入 UI 分片：旧文本保持可读，准备完成后交换，旧行分批回收；连续缩放只保留最新目标。期间暂停解析 PTY 输出并沿用有界背压，键盘仍可写入 PTY。小网格、高度变化及备用屏幕仍有同步路径；完整回显延迟尚未验收。[流式重排说明](docs/execution-m4-reflow.md)。

输出复制已加入额度交接和窗口代次取消，避免原窗格关闭后提前还票或晚到结果覆盖新复制；额外成本及尚未验收的输入/平台边界见 [复制评估](docs/evaluations/m4-copy-resources.md)。

普通粘贴读取及在途结果已持有额度，发送前转换改为单个缓冲，未改变原窗格归属和认证屏障。性能对照和原生边界见 [粘贴评估](docs/evaluations/m4-paste-resources.md)。

链接目标池和URL文本已先准入，可见检测不再逐格建立临时String，转交复制/打开任务保持票据。性能取舍和launcher剩余边界见 [链接评估](docs/evaluations/m4-link-resources.md)。
