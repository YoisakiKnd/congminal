# congminal 0.1.0-alpha.6 使用说明

本版是 Apple 芯片 Mac 的本地 Alpha 体验包。解压后双击 `congminal.app`，也可将它拖到“应用程序”。无需安装 Rust、Homebrew 或额外字体。内置备用字体，优先使用已安装的等宽字体。

这是本机临时签名的包，尚未经过 Apple 公证；系统若拦截，从“系统设置 → 隐私与安全性”允许打开。

## 这版可以用什么

本地终端、中文输入、历史搜索、复制粘贴、主题与字号切换、标签页、分屏、左右侧栏、本地文件目录和连接配置档、系统 OpenSSH 终端连接。命令状态通知依赖 shell 集成；系统通知默认关闭。

SFTP 尚未进入发布构建，相关界面和传输测试仍在开发；Git 面板、监控面板及完整美化计划也尚未交付。Windows/Linux 真机验收和长时间稳定性测试仍待完成。本包只包含 macOS arm64，不是 Intel 通用版。

## 常用快捷键

| 操作 | 快捷键 |
|---|---|
| 新标签 / 关闭当前窗格 | Cmd+T / Cmd+W |
| 向右 / 向下分屏 | Cmd+D / Cmd+Enter |
| 切换标签 | Ctrl+Tab / Ctrl+Shift+Tab |
| 切换窗格 | Ctrl+PageDown / Ctrl+PageUp |
| 左栏 / 右栏 | Cmd+B / Cmd+I |
| 搜索历史 | Cmd+F，Esc 退出 |
| 复制 / 粘贴 | Cmd+C / Cmd+V |
| 字号放大 / 缩小 / 复位 | Cmd+Plus / Cmd+Minus / Cmd+0 |
| 切换主题 / 查看资源快照 | Cmd+F6 / Cmd+F12（可能需要 Fn） |

## 配置与 SSH

配置路径：`~/.config/congminal/congminal.toml`（设置了 `XDG_CONFIG_HOME` 时使用它）。包中的 `congminal.example.toml` 有完整注释。没有配置也可运行；若已有配置，请保留并按需合并。字体、主题等保存后自动生效，shell 和启动目录下次启动生效。

希望像平时的 Mac 终端一样加载登录环境，可在配置中加入：

```toml
[shell]
program = "/bin/zsh"
args = ["-l"]
working-directory = "~"

[workspace]
restore-layout = true

[connections.myserver]
kind = "ssh"
name = "我的服务器"
host = "your-ssh-alias"
```

把 `your-ssh-alias` 改为 `~/.ssh/config` 中的别名或实际主机，然后在左栏“连接”页点击。认证和主机指纹确认出现在终端内，沿用系统 OpenSSH；也可以直接在终端输入 `ssh 用户@主机`。配置不要写密码或私钥正文。

可选 shell 集成：在 `~/.zshrc` 中 source 本包的 `congminal.zsh`（先将脚本放到固定位置），启用命令完成/失败状态。未配置时基本终端和 SSH 照常工作。

## 资源边界

使用 release 构建，默认不引入 Rust 第三方包；启动脚本 exec 后不驻留后台进程。SSH 使用系统 ssh 进程，其资源不算入程序的 Rust 内存额度。最多 8 个窗格，历史共享池 64 MiB；共享资源准入默认 512 MiB，但不是整进程堆或 RSS 上限。SFTP 开发用工作器和认证 helper 不包含在本包中。
