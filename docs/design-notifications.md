# 通知中心：来源接入、去重限速与定位

本轮把 Git、监控、SFTP 写操作与 SSH 连接状态接进已有通知中心，并补上“从通知跳回相关页面”的入口。命令完成/失败、会话结束与传输结局在 M3/M6 已接入，本文只记录本轮新增部分与它们的共同管线。

## 事件来源

| 来源 | 触发 | 文案（应用内） | 系统横幅（ASCII 模板） |
| --- | --- | --- | --- |
| `Source::Command(id)` | 命令完成/失败、长命令、后台结束（单窗口与**每个工作区窗格**都结算） | 原有文案 | `... failed`/`completed` |
| `Source::Session` | 会话结束（单窗口与窗格退出） | 原有文案 | 原有模板 |
| `Source::Transfers` | 传输结局（`sftp-preview` 构建） | 原有汇总文案 | 原有模板 |
| `Source::Git` | 暂存、取消暂存、提交成功或失败 | `Git · 暂存完成` / `Git · 提交失败` | `Git: write completed` / `failed` |
| `Source::Monitor` | 采样从正常转为失败（平台不支持、额度不足、任务失败） | `监控 · 采样失败` | `Monitor: sampling failed` |
| `Source::Sftp` | 远程新建目录/重命名/删除成功或失败（`sftp-preview` 构建） | `SFTP · 删除失败` | `SFTP: remote write completed` / `failed` |
| `Source::Ssh` | SSH 会话断开、连接失败（认证/主机校验/解析/拒绝/超时）与重连准备失败 | `SSH · 连接已断开` / `SSH · 认证失败` / `SSH · 重连失败` | `SSH: connection closed` / `SSH: connection failed` |

细节约束：

- 应用内文案只用固定动作词（暂存、提交、新建目录、重命名、删除），**不含路径、分支名或提交正文**；系统横幅只用 ASCII 模板，避免 Windows 上的编码问题。
- 远程写操作的“用户主动取消”不算失败，不进通知中心（面板状态行仍有提示）。
- 监控只在错误从“无”变“有”时报一次；同一次失败持续期间不重复上报，恢复后再次失败会重新上报。
- SSH 失败原因**只认 ssh 自己打印的固定特征**：`Permission denied` → 认证失败，`Host key verification failed` → 主机校验失败，`Could not resolve hostname` → 主机名解析失败，`Connection refused` → 连接被拒绝，`Connection timed out`/`Operation timed out` → 连接超时；都没有就写“连接失败”。不从屏幕内容推断认证细节，也不把 ssh 原文甩进通知。
- 正常退出（含远端主动关闭、`exit`）只写“连接已断开”，不猜原因；准备阶段失败按目标写成“连接失败”或“重连失败”。

## 管线

0. **按主机静音**：`mute-hosts` 命中来源主机时整条通知不产生（不排队、不计数、不进合并窗口）。主机取自该窗格 shell 报告的 OSC 7 目录（状态栏显示的正是它）或 SSH 配置档的 `host`；本地命令、本地 Git 与监控没有主机维度，永不被主机规则拦掉。比较为大小写不敏感的精确匹配，最多 32 条、每条不超过 253 字节。
1. **来源开关**：`Settings` 里的 `git`、`monitor`、`ssh` 各自控制，`transfers` 管传输与远程写操作，`muted` 全局静音。被关掉的来源直接丢弃，不排队、不补发。
2. **固定窗口限速**：10 秒内最多 `burst`（默认 12）条进入本地记录；超出只累加 `throttled` 计数，状态栏显示“限速丢弃N”。
3. **同类合并**：`Git`、`SFTP`、`Monitor`、`Ssh` 这几类无载荷事件，同来源 + 同类别 + 同动作词且间隔小于 `dedupe-seconds`（默认 5）时合并成一条并累加 `repeat`，文案追加 `×N`。命令与传输带载荷，不参与合并。
4. **系统横幅**：真正入列的通知才走 `SystemGate` 的 10 秒闸门与既有后台 helper（5 秒超时、取消即 kill）。系统通知默认关闭（`system = false`）。

记录仍是固定的 128 槽环形窗口，首次准入、满后复用、`clear()` 释放；额度走既有的 notifications 资源项。

## 敏感上下文与清理

- 通知本身只含固定动作词，从不写命令正文、路径、分支名或提交正文。
- `hide-context = true` 时，状态栏（命令视图）与右栏状态页的 Shell 摘要不再显示 `主机:工作目录`，改为 `| 目录已隐藏`；命令状态、退出码与耗时仍保留。远程会话行本来就只有状态与退出码，无需处理。
- 清理：通知浮层里 `Delete` 清空；右栏状态页在有通知时多出一行 `通知 N 条 · 点击清空`，点它清空记录、限速计数与行号表。
- 动作名 `clear-notifications` 可用 `[keybindings]` 绑定（默认不占按键），工作区模式下也能用。

## 定位入口

- **单窗口**：`Primary+F10` 打开通知浮层，↑/↓ 浏览，**回车**跳到该条通知对应的命令输出（复用命令选择与复制路径，不新开解析），Esc 关闭。工作区面板类通知在单窗口下没有对应页面，回车只给出提示。
- **工作区**：右栏“状态”页列出最近 8 条通知，并记下每条的行号与来源；**点击某条通知行**即切到对应页面——Git 通知切到 Git 页并重新申请额度，监控通知切到监控页并恢复采样，SFTP/传输通知切到左栏文件页，SSH 通知切到左栏连接页（可直接重连）。点击行本身不重复触发事件。

## 配置

```toml
[notifications]
git = true            # Git 写操作结局
monitor = true        # 监控采样失败
ssh = true            # SSH 断开、连接失败与重连失败
transfers = true      # 传输与 SFTP 远程写操作
muted = false
system = false
dedupe-seconds = 5    # 0 视为 1，同类事件在该窗口内合并
burst = 12            # 10 秒窗口内的本地通知上限，至少 1
hide-context = false  # 隐藏状态栏与状态页里的主机名、工作目录
mute-hosts = []       # 按主机静音，大小写不敏感，最多 32 条
```

工作区模式下每个窗格各自持有通知中心：后台窗格的命令完成/失败与窗格退出只进**它自己**的列表（切到该窗格可见）并立刻走系统通知闸门，不会把当前窗格的列表塞满。

## 测试

- `notifications::tests`：同类合并与 `×N`、限速计数与不补发、按来源静音、按主机静音（大小写不敏感、无主机事件不受影响）、SFTP 文案与 ASCII 系统模板、`current()`/`recent_notices()` 顺序（19 项含传输侧）。
- `config::tests::mute_hosts_are_normalized_deduped_and_bounded`：去空白、统一小写、去重、空串给警告、超 32 条只保留上限并提示。
- `integration_tests::notices_from_a_muted_host_are_dropped`：命中 `mute-hosts` 的 OSC 7 主机不产生通知，换一台主机后同一条事件照常上报。
- `workspace::tests::workspace_reports_command_completions_from_every_pane`：工作区里后台窗格与前台的命令完成各进自己的通知中心（此前工作区模式下完全不上报）。
- `workspace::tests::git_panel_stages_and_commits_through_the_sidebar`：真实仓库暂存与提交后，通知中心出现 `Git · 暂存完成` 与 `Git · 提交完成`，且文案不含文件名与提交正文。
- `workspace::tests::status_page_notice_rows_jump_to_the_related_page_and_monitor_failure_is_reported_once`：状态页点击 Git 通知行切到 Git 页并重新打开面板；监控失败只上报一次。
- `workspace::tests::monitor_failure_notice_fires_once_per_failure_and_rearms_after_recovery`：失败持续不重复、恢复后重新置位。
- `workspace::remote::tests::failure_details_come_from_ssh_own_markers_and_default_to_a_generic_reason`：五类 ssh 特征各归各的原因，认不出与空屏幕都退回“连接失败”。
- `notifications::tests::ssh_connection_events_use_their_own_switch_and_host_muting`：断开/失败文案与 ASCII 系统模板、`ssh = false` 与 `mute-hosts` 都能拦下、同类在去重窗口内合并成 `×2`。
- `workspace::tests::ssh_connection_events_reach_the_notification_center_and_jump_to_the_connection_page`：认证被拒的假 ssh 报 `SSH · 认证失败`、正常退出报 `SSH · 连接已断开`，状态页点 SSH 通知行切到连接页。
- `workspace::tests::failed_reconnect_keeps_the_running_session_and_reports_why`：程序消失后的重连失败同时写状态页与通知中心（`SSH · 重连失败`），旧会话继续可用。
- `config::tests::ssh_notifications_switch_is_read_and_defaults_to_on`：`[notifications] ssh` 可关，缺省为开。
- `integration_tests::command_navigation_copy_notifications_and_focus_use_the_same_session`：浮层回车定位到命令输出并关闭浮层。
- `shell::tests::hidden_context_keeps_the_summary_but_drops_host_and_path`：隐藏上下文后摘要仍在、主机与路径消失。
- `config::tests::notification_settings_parse_hidden_context_and_clamp_rate_limits`：`hide-context` 生效，`dedupe-seconds`/`burst` 越界时保留默认并给警告。
- `integration_tests::hidden_context_and_clear_notifications_follow_the_configuration`：配置打开时状态栏不含主机与路径；`F5 = 'clear-notifications'` 按名字绑定后能清空记录。

## 预览

![状态页通知行](previews/m8-notice-status.png)

1200×700 窗口、右栏 280 px 时“状态”页的样子：第三行是任务数，第四行是 `通知 N 条 · 点击清空`，之后是最近通知；点击 `Git · 提交完成` 那一行会切到 Git 页，点击清空行会清掉记录。

## 未验收

- 认证过程本身不可观察：只有 ssh 打印出固定失败特征时才能归类，静默超时或自绘错误信息一律写“连接失败”。
- 主机维度依赖配置档的 `host`：连接后临时换主机（例如在 ssh 会话里再 ssh）不会被单独识别。
- 主机名依赖 OSC 7 或 SSH 配置档：远端 shell 不报告目录时无法按主机静音，也不会猜测主机。
- 命令正文（状态栏之外的输入行本身）无法隐藏；`hide-context` 只覆盖摘要行。
- 真实系统通知权限、送达与峰值内存（M3 遗留，需要真机）。
- 工作区模式下没有通知浮层，只有状态页的行；键盘浏览浮层仍只在单窗口可用。
