# congminal

用 Rust 编写的跨平台终端模拟器，提供多标签、分屏、SSH/SFTP、Git、状态监控和通知。窗口与界面直接使用 Cocoa/AppKit、Win32、X11，终端和侧栏由自有软件渲染器绘制。

当前稳定版 **[v0.1.1](https://github.com/YoisakiKnd/congminal/releases/tag/v0.1.1)**，提供带完整 GPL 许可的 macOS、Windows、Linux 安装包和 SHA256。首个正式版 v0.1.0 的功能与性能验收见 [正式版记录](docs/evaluations/v0.1.0/README.md)。

## 安装和运行

从 Releases 下载对应平台的包，解压后保留主程序旁的 `congminal-ssh-helper`（Windows 为 `.exe`）。helper 用于 SFTP 认证；Windows 还用它调用系统文件打开接口。

| 平台 | 要求与启动方式 |
| --- | --- |
| macOS | macOS 11 或更新；通用包支持 Apple 芯片与 Intel。将 `.app` 放到应用程序目录后打开。发布包仅临时签名，尚无 Apple 开发者签名和公证，首次打开需按系统提示允许。 |
| Windows | Windows 10 1809 或更新，使用 ConPTY；运行 `congminal.exe`。 |
| Linux | X11，Wayland 桌面通过 XWayland；运行 `./congminal`。CI 包在 Ubuntu 上构建，旧发行版可能不满足其 glibc 要求。 |

通过个人 Homebrew tap 安装（macOS）：

```sh
brew tap YoisakiKnd/tap
brew install --cask YoisakiKnd/tap/congminal
```

通过个人 Scoop bucket 安装（Windows x86_64）：

```powershell
scoop bucket add yoisaki https://github.com/YoisakiKnd/scoop-bucket
scoop install yoisaki/congminal
```

两种方式都保留 SFTP helper；Homebrew 将许可放在应用包内，Scoop 随安装目录保留。SSH/OpenSSH 要求仍按下文配置，macOS 首次运行仍需系统允许。

源码构建需要 Rust **1.88+**。Linux 编译需 X11 开发库（Debian/Ubuntu 为 `libx11-dev`）；其他平台使用系统 SDK。

```sh
cargo run --release
cargo install --path . --bins
```

默认构建包含 SFTP 和 helper；仅需本地终端时可用 `cargo build --release --no-default-features --bin congminal`。旧 `sftp-preview` 特性保留为 `sftp` 的兼容别名。

本机 macOS 应用包可用 `bash ci/package-macos.sh` 构建，输出到 `target/distributions/`。CI 生成三平台安装包和 SHA256 校验文件。

## 功能

- 终端：UTF-8、宽字符、ANSI/VT 转义序列、真彩色、备用屏、回滚、选区、历史搜索、链接及鼠标上报。
- 工作区：多标签、横纵分屏、左右侧栏、连接配置档；可选保存和恢复布局，恢复时重新启动 shell。
- SSH：复用系统 OpenSSH 配置、密钥与 agent，支持别名、跳板和端口转发。终端认证在终端中进行。
- SFTP：独立文件连接、文件与目录上传/下载、上传断点续传、重命名、创建和删除；目录操作先预览范围。支持临时下载后用系统程序打开，并显式上传编辑结果。
- Git：状态、diff、历史和分支；本地支持暂存、提交、分支操作、冲突处理、快进拉取和推送，写操作由明确点击触发。远程会话的 Git 面板只读。
- 监控：原生 CPU、内存、磁盘和进程采样，面板隐藏或窗口失焦时停止；可配置阈值通知。
- 通知与外观：命令结果、SSH/SFTP、Git 和监控通知；主题、字体回退、编程连字、彩色 emoji、透明度与光标配置。

## 第一次使用

启动后即可输入命令。macOS 的工作区主修饰键是 **Cmd**，Windows/Linux 是 **Ctrl+Shift**。

| 操作 | 快捷键 |
| --- | --- |
| 新标签 / 关闭窗格 | 主修饰键 + T / W |
| 左右分屏 / 上下分屏 | 主修饰键 + D / Enter |
| 切换标签 | Ctrl+Tab / Ctrl+Shift+Tab |
| 切换窗格 | Ctrl+PageDown / Ctrl+PageUp |
| 左侧栏 / 右侧栏 | 主修饰键 + B / I |
| 复制 / 粘贴 | 主修饰键 + C / V |
| 历史搜索 | 主修饰键 + F；Enter / Shift+Enter 切换结果，Esc 返回 |
| 字号放大 / 缩小 / 复位 | 主修饰键 + Equal / Minus / 0 |
| 主题预览 / 切换 | 主修饰键 + F5 / F6 |
| 资源快照 | 主修饰键 + F12 |

拖动选择、双击选词、三击选行。应用开启鼠标上报时按住 Shift 可以选择文字。滚轮浏览历史，Shift+PageUp/PageDown 翻页。macOS 按 Cmd+单击打开链接，其他平台按 Ctrl+单击。

命令完成标记、通知和精确工作目录依赖 shell 集成。将仓库或安装包中的对应脚本加入 shell 配置：

```sh
# ~/.zshrc；替换为脚本的实际路径
source /absolute/path/congminal.zsh
# ~/.bashrc
source /absolute/path/congminal.bash
```

## 配置

配置位置：macOS/Linux 为 `~/.config/congminal/congminal.toml`（遵循 `XDG_CONFIG_HOME`），Windows 为 `%APPDATA%\congminal\congminal.toml`。可用 `--config <路径>` 或 `CONGMINAL_CONFIG` 指定另一份配置。

```sh
# macOS / Linux
mkdir -p ~/.config/congminal
congminal --print-config > ~/.config/congminal/congminal.toml
```

完整选项见 [带注释的配置示例](assets/congminal.example.toml)。字体、主题、颜色和快捷键等保存后生效；shell、启动尺寸、布局恢复和内存额度在下次启动时生效。文件解析支持项目使用的 TOML 子集。

```toml
[connections.server]
kind = "ssh"
name = "开发服务器"
host = "dev-server" # 可使用 ~/.ssh/config 中的明确 Host 别名

[sftp]
enabled = true
# helper = "/absolute/path/congminal-ssh-helper"
# ssh-config = "/absolute/path/ssh_config"
open-max-mib = 4
```

在左栏“连接”页打开 SSH 配置档，进入该会话的“文件”页，点击“连接 SFTP”。默认启用只开放入口，点击前不创建 SFTP 池、工作器或网络连接。终端与 SFTP 是独立连接。

## SSH 与 SFTP 的依赖和行为

SSH 使用系统 `ssh`。SFTP 的界面、v3 协议、文件操作、队列和资源管理在程序内实现，通过 `ssh -T -s … sftp` 复用 OpenSSH 的加密、主机验证和认证，**无需调用外部交互式 `sftp` 程序**。服务端需启用 SFTP 子系统。

SFTP 要求支持 `ForkAfterAuthentication` 和强制 askpass 的 OpenSSH 客户端，发布支持基线为 **OpenSSH 8.7+**；Windows 请用支持 askpass 的 Windows OpenSSH 版本。可用 `ssh -V` 检查，配置档 `program` 可指定程序绝对路径。旧系统自带客户端可能需要更新。首次主机确认、密码及密钥口令在受保护的独立认证面板处理；主机密钥变更由 OpenSSH 拒绝。私钥和密码正文不要写入本程序配置。

每次操作使用独立子系统连接，密码认证可能重复提示；多文件工作建议使用密钥与 agent。认证 helper 按需启动，不增加常驻认证线程；SSH 的进程和加密成本仍由系统客户端承担。

下载使用私有暂存和原子发布，默认不覆盖；覆盖需要明确确认。取消、连接中断或清理失败时会报告结果并保留可恢复暂存。目录扫描受文件数、目录数、深度和总大小上限约束；符号链接和特殊文件不递归跟随，截断结果会提示。递归删除不可恢复，不对截断或含跳过项的目录预览执行整树删除。

临时副本默认限 4 MiB，可调到 32 MiB；副本和失败暂存保留在提示的路径，不自动递归删除。编辑不会自动同步，需显式上传。Windows 本地传输拒绝 UNC、设备路径、ADS、保留名和不可无损表示的文件名；树内 junction/reparse point 不跟随。普通文件名按原始字节保留，界面转义显示。

## 内存、性能与实现

默认 Unix 运行时 Rust 依赖仅为 `libc` 的系统 FFI 定义；Windows 使用直接系统接口。`libssh2-sys` 是可选测试后端，不进入正式默认包。监控不使用 `sysinfo`。引入其他包前评估资源和性能影响，见 [依赖策略](docs/dependency-policy.md)。

PTY 队列、历史、字体缓存、图片解压、帧缓冲、任务和目录模型有明确预算与上限；大量输出使用背压，不能准入的输入整笔拒绝并提示。历史行数还受实际行存储额度限制。SFTP 沿用 2 个工作槽、64 等待槽，子池额度为 16 MiB；单目录列表最多 512 条，目录扫描最多 2000 个文件、512 个目录、16 层及 8 GiB 文件总量，每帧最多提交 4 个文件。

这些额度是受管资源账本，不是整个进程 RSS 的硬上限；系统库、内核资源、线程栈和外部 SSH/编辑器需分别测量。最新结果和已验证的平台范围见 [v0.1.0 验收记录](docs/evaluations/v0.1.0/README.md)，历史性能数据保留其原始源码版本，不能直接代表当前发布。

## 开发与已知限制

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -- --test-threads=1
```

真实 SSH、SFTP 和窗口检查使用隔离夹具，复现工具位于 `tools/`；部分测试带 `ignored` 标记，需要显式准备环境。CI 包含三个平台原生单元回归、应用启动/退出和最低 Rust 版本检查。编译或单元测试通过不等于所有桌面环境和输入法已验收。

- Linux 使用 X11/XIM，尚无原生 Wayland 后端；Windows 输入法与不同 Linux 桌面的覆盖范围仍有限。
- 字体支持 TrueType 轮廓，不支持 OTF/CFF；复杂文字定位和所有终端扩展并未完整实现。
- 大型历史重排和字体/emoji 工作均受预算约束；兼容性细节见 [底层说明](docs/foundations.md) 和 [终端查询设计](docs/design-term-queries.md)。
- macOS 包未公证；Linux 包的 glibc 下限取决于实际构建系统。

开发资料：[功能计划](docs/roadmap.md)、[发布执行清单](docs/execution-v0.1.0.md)、[交接文档](docs/DEEPSEEK_HANDOFF.md)。

## 许可证

congminal 原创源码采用 **[GNU GPL v3，仅第 3 版（GPL-3.0-only）](LICENSE)**，Copyright © 2026 YoisakiKnd。版权及第三方说明见 [NOTICE](NOTICE)。此声明也覆盖原创 `v0.1.0` 代码。发布源码与构建脚本可从对应 GitHub Release 的源码归档和同名标签取得。

从 `v0.1.1` 起，三平台安装包包含 `LICENSE`、`NOTICE` 及第三方许可。`assets/fonts/` 中的 Terminus Font 继续使用 SIL Open Font License 1.1，见 [字体许可证](assets/fonts/LICENSE-Terminus.txt)；第三方组件保留各自许可，Maple Mono 不随程序分发。
