# 配置与快捷键参考

一页看完所有配置项、默认值与默认快捷键。带注释的完整示例在
[`assets/congminal.example.toml`](../assets/congminal.example.toml)，`congminal --print-config` 会打印同一份内容。

## 文件位置与生效时机

| 平台 | 路径 |
| --- | --- |
| Linux / macOS | `~/.config/congminal/congminal.toml`（设置了 `XDG_CONFIG_HOME` 时在它下面） |
| Windows | `%APPDATA%\congminal\congminal.toml` |

- `--config <路径>` 或环境变量 `CONGMINAL_CONFIG` 可以指定别的文件。
- 保存后自动重新读取：字体、主题、颜色、快捷键、内边距、光标、Option 键、通知开关、Git/监控/SFTP 的开关与阈值立即生效。
- 需要重启才生效：`shell`、`window.columns`/`rows`、`workspace.restore-layout`、`memory`。
- 解析失败或越界的项会在状态页与启动日志里给出警告，并按默认值继续运行；不认识的键不会让整份配置失效。

## 分组与配置项

### `font`

| 键 | 默认值 | 说明 |
| --- | --- | --- |
| `font.family` | `["Maple Mono NF CN", "Maple Mono NF", "Maple Mono CN", "Maple Mono"]` | 依次尝试，第一个找到的作为主字体，其余在缺字时兜底；也可写字体文件路径 |
| `font.size` | `15` | 逻辑像素字号，高分屏自动放大 |
| `font.line-height` | `1.0` | 行高倍数，1.0 用字体自身行高 |
| `font.ligatures` | `true` | 编程连字（`->`、`=>` 等） |

### `window`

| 键 | 默认值 | 说明 |
| --- | --- | --- |
| `window.columns` | `80` | 启动列数 |
| `window.rows` | `24` | 启动行数 |
| `window.padding` | `4` | 文字与窗口边缘的空白（逻辑像素） |

### `workspace`

| 键 | 默认值 | 说明 |
| --- | --- | --- |
| `workspace.restore-layout` | `false` | 异步保存/恢复标签、分屏、焦点与侧栏；只存 UI 元数据 |

### `appearance`

| 键 | 默认值 | 说明 |
| --- | --- | --- |
| `appearance.theme` | `"dark"` | `dark` / `light` / `midnight`；`Primary+F6` 循环切换（不写回文件） |
| `appearance.cursor-blink` | `false` | 光标闪烁；关闭时静止界面不重绘 |
| `appearance.command-marks` | `true` | 提示行左侧的结果色条（需要 shell 集成上报 OSC 133） |
| `appearance.opacity` | `1.0` | 整窗不透明度 0.3–1.0；1.0 不碰窗口属性 |

### `colors`

| 键 | 默认值 | 说明 |
| --- | --- | --- |
| `colors.foreground` | 主题值 | 前景色 |
| `colors.background` | 主题值 | 背景色 |
| `colors.cursor` | 主题值 | 光标色 |
| `colors.cursor-text` | 主题值 | 光标下的文字色 |
| `colors.selection-background` | 主题值 | 选中背景 |
| `colors.preedit-background` | 主题值 | 输入法组合文字的背景 |
| `colors.bold-is-bright` | `true` | 加粗时用亮色 |
| `colors.palette` | 主题值 | 最多 256 项，依次覆盖调色板；显式设置的颜色在切换主题时保留 |

### `keybindings`

自由表：键是快捷键，值是动作名或 `{ action = "send-text", text = "…" }`。见下面的[默认快捷键](#默认快捷键)与[动作名](#动作名)。

### `cursor` / `scrollback`

| 键 | 默认值 | 说明 |
| --- | --- | --- |
| `cursor.shape` | `"block"` | `block` / `underline` / `bar`，程序可临时改变 |
| `scrollback.lines` | `10000` | 回滚历史行数，0 表示不保留 |

### `shell` / `env`

| 键 | 默认值 | 说明 |
| --- | --- | --- |
| `shell.program` | `$SHELL` / `%COMSPEC%` | 本地会话启动的程序 |
| `shell.args` | 空 | 传给程序的参数 |
| `shell.working-directory` | 启动目录 | `~` 表示用户主目录 |
| `env` | 空 | 额外环境变量，`TERM` 默认 `xterm-256color` |

### `macos`

| 键 | 默认值 | 说明 |
| --- | --- | --- |
| `macos.option-as-alt` | `true` | `true` / `false` / `"left"` / `"right"`：Option 是否当 Alt 用 |

### `ssh`

| 键 | 默认值 | 说明 |
| --- | --- | --- |
| `ssh.program` | `"ssh"` | 连接别名时用的 ssh；配置档里写了 `program` 的以配置档为准 |
| `ssh.config-file` | `"~/.ssh/config"` | 读别名的文件；不存在时不报错，列表为空 |

### `connections`

自由表，每个 `[connections.<id>]` 是一个配置档：`kind`（`local` / `ssh`）、`name`、`group`、`theme`，以及 `host`、`user`、`port`、`identity-file`、`jump`、`forward`、`program` 等 SSH 字段。详见 [配置档说明](execution-m4.md#连接配置档)。

### `sftp`

| 键 | 默认值 | 说明 |
| --- | --- | --- |
| `sftp.enabled` | `false` | 启用文件页（需要 macOS/Linux 的 `sftp-preview` 构建） |
| `sftp.helper` | 主程序旁的 `congminal-ssh-helper` | 独立 SFTP 的辅助程序 |
| `sftp.ssh-config` | 系统默认 | 独立 SFTP 用的 ssh 配置，不影响终端 |
| `sftp.opener` | 平台默认 | 打开远端文件用的程序（`xdg-open` / `open` / `cmd`） |
| `sftp.open-max-mib` | `4` | 允许打开（临时下载）的最大 MiB，1–32 |

### `git`

| 键 | 默认值 | 说明 |
| --- | --- | --- |
| `git.enabled` | `true` | 关闭后不运行 git，Git 页显示停用提示 |
| `git.program` | `PATH` 中的 `git` | 可写绝对路径 |
| `git.refresh-seconds` | `2` | 面板可见且窗口聚焦时的最小查询间隔 |
| `git.timeout-seconds` | `20` | 单次查询超时，超时杀子进程并保留上次结果 |
| `git.max-files` | `2000` | 一次快照最多列出的变更文件数 |
| `git.diff-mib` | `8` | 单次 diff 读取上限 |

### `monitor`

| 键 | 默认值 | 说明 |
| --- | --- | --- |
| `monitor.enabled` | `true` | 关闭后不启动采样，监控页显示停用提示 |
| `monitor.interval-seconds` | `1` | CPU/内存采样间隔，1–5 |
| `monitor.history-seconds` | `300` | 历史保留时长，60–900 |
| `monitor.disk-seconds` | `10` | 磁盘刷新间隔，5–120 |
| `monitor.alert-cpu` | `0` | CPU 告警阈值（百分比），0 关闭 |
| `monitor.alert-memory` | `0` | 内存告警阈值 |
| `monitor.alert-disk` | `0` | 磁盘告警阈值 |
| `monitor.alert-seconds` | `300` | 同一告警的最短间隔，30–3600 |
| `monitor.processes` | `true` | 是否列出 CPU 占用最高的 5 个进程 |

### `notifications`

| 键 | 默认值 | 说明 |
| --- | --- | --- |
| `notifications.muted` | `false` | 全部静音 |
| `notifications.system` | `false` | 调用系统通知（有超时与频率限制） |
| `notifications.failures` | `true` | 命令失败 |
| `notifications.long-commands` | `true` | 长命令完成 |
| `notifications.background` | `true` | 后台窗格的命令完成 |
| `notifications.transfers` | `true` | SFTP 传输结局 |
| `notifications.git` | `true` | Git 写操作 |
| `notifications.monitor` | `true` | 监控采样失败与阈值告警 |
| `notifications.ssh` | `true` | SSH 断开、连接失败、重连失败 |
| `notifications.long-seconds` | `10` | 超过多少秒算“长命令” |
| `notifications.dedupe-seconds` | `5` | 同类事件合并窗口，0 表示不合并 |
| `notifications.burst` | `12` | 10 秒窗口内最多产生多少条本地通知 |
| `notifications.hide-context` | `false` | 隐藏状态栏与状态页里的主机名、目录等上下文 |
| `notifications.mute-hosts` | 空 | 按主机名静音，最多 32 个，大小写不敏感 |

### `memory`

| 键 | 默认值 | 说明 |
| --- | --- | --- |
| `memory.managed-mib` | `512` | 共享准入额度，128–4096 MiB；不是完整堆/RSS 上限，下次启动生效 |
| `memory.mapped-font-mib` | `1024` | 字体只读映射的地址空间上限，64–4096 MiB |

## 默认快捷键

`Primary` 在 macOS 是 `Cmd`，其他平台是 `Ctrl+Shift`。键名不区分大小写，可以在 `[keybindings]` 里覆盖或新增。

| 快捷键 | 动作名 | 说明 |
| --- | --- | --- |
| `Shift+PageUp` | `page-up` | 向上翻页（历史） |
| `Shift+PageDown` | `page-down` | 向下翻页 |
| `Shift+Home` | `history-top` | 跳到历史开头 |
| `Shift+End` | `history-bottom` | 跳到历史末尾 |
| `Ctrl+Shift+C` | `copy` | 复制 |
| `Ctrl+Shift+V` | `paste` | 粘贴 |
| `Shift+Insert` | `paste` | 粘贴 |
| `Primary+C` | `copy` | 复制 |
| `Primary+V` | `paste` | 粘贴 |
| `Primary+Equal` | `font-increase` | 放大字号 |
| `Primary+Plus` | `font-increase` | 放大字号 |
| `Primary+Minus` | `font-decrease` | 缩小字号 |
| `Primary+0` | `font-reset` | 复位字号 |
| `Primary+F5` | `preview-themes` | 整窗实时预览主题（←/→ 切换、Enter 应用、Esc 取消） |
| `Primary+F6` | `next-theme` | 下一个主题 |
| `Primary+F7` | `select-command-output` | 选中上一条命令的输出 |
| `Primary+F8` | `copy-command-output` | 复制上一条命令的输出 |
| `Primary+F9` | `show-shell-status` | 显示 shell 集成状态 |
| `Primary+F10` | `show-notifications` | 打开通知中心 |
| `Primary+F11` | `toggle-notifications` | 显示/隐藏通知浮层 |
| `Primary+F12` | `show-resources` | 资源统计 |
| `Primary+T` | `new-tab` | 新建标签 |
| `Primary+W` | `close-pane` | 关闭窗格 |
| `Ctrl+Tab` | `next-tab` | 下一个标签 |
| `Ctrl+Shift+Tab` | `previous-tab` | 上一个标签 |
| `Ctrl+Shift+PageUp` | `move-tab-left` | 标签左移 |
| `Ctrl+Shift+PageDown` | `move-tab-right` | 标签右移 |
| `Primary+B` | `toggle-left-sidebar` | 左栏开关 |
| `Primary+I` | `toggle-right-sidebar` | 右栏开关 |
| `Primary+G` | `show-git-panel` | 右栏 Git 页 |
| `Primary+M` | `show-monitor-panel` | 右栏监控页 |
| `Primary+D` | `split-right` | 向右分屏 |
| `Primary+Enter` | `split-down` | 向下分屏 |
| `Ctrl+PageDown` | `next-pane` | 下一个窗格 |
| `Ctrl+PageUp` | `previous-pane` | 上一个窗格 |
| `Primary+F` | `search` | 搜索 |
| `F3` | `search-next` | 下一个匹配 |
| `Shift+F3` | `search-previous` | 上一个匹配 |
| `Primary+Up` | `command-previous` | 上一条命令 |
| `Primary+Down` | `command-next` | 下一条命令 |

## 动作名

`[keybindings]` 的值可以是下面任一动作名，也可以是 `{ action = "send-text", text = "…" }`（最多 8 KiB）。

`copy`、`paste`、`page-up`、`page-down`、`history-top`、`history-bottom`、`font-increase`、`font-decrease`、`font-reset`、`next-theme`、`preview-themes`、`reset-theme`、`show-resources`、`search`、`search-next`、`search-previous`、`open-link`、`copy-link`、`command-previous`、`command-next`、`select-command-output`、`copy-command-output`、`show-shell-status`、`show-notifications`、`toggle-notifications`、`clear-notifications`、`new-tab`、`close-pane`、`next-tab`、`previous-tab`、`move-tab-left`、`move-tab-right`、`split-right`、`split-down`、`next-pane`、`previous-pane`、`toggle-left-sidebar`、`toggle-right-sidebar`、`show-git-panel`、`show-monitor-panel`、`none`、`pass-through`。

- `none`：吃掉这个按键，不传给终端。
- `pass-through`：不拦截，直接交给终端程序。
- 同一个快捷键写两次会报“与…是同一快捷键”，后写的不会生效。

## 旧配置兼容

- 只写想改的项即可，缺的键一律用默认值；示例文件里写的都是默认值。
- 不认识的**分组或键**会给出警告并跳过，其余配置照常生效——旧版本留下的键不会让新版本启动失败。
- 越界数值（例如 `monitor.interval-seconds = 0`）按默认值处理并警告，不会静默截断。
- `[keybindings]` 里写错的动作名或按键名同样只警告、跳过该条。
- 配置里没有 `[ssh]` 分组时不会去读 `~/.ssh/config`，连接页也就不会列出别名。
