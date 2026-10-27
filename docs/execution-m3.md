# M3：Shell 集成、命令导航与通知

更新日期：2026-10-20。前置基线为当前未提交工作区的 M2 快照；不覆盖已有修改。实现完成项与平台验收分别记录。

## 使用

脚本是可选的，不自动修改个人 Shell 配置。最省事的方式是让程序自己打印脚本，不必找仓库路径：

```sh
source <(congminal --print-shell-integration bash)   # 或 zsh
```

也可以先落盘再 source（`congminal --print-shell-integration zsh > ~/.config/congminal.zsh`）。zsh 用户在 `.zshrc` 最后加入：

```sh
source ~/.config/congminal.zsh
```

### 提示符美化（可选）

默认**不动**用户自己的提示符，只加标记与目录上报。设置 `CONGMINAL_PROMPT=pretty` 才换成两行的紧凑提示符：第一行 `user@host 路径 [分支] (!退出码)`（退出码只在非 0 时出现），第二行 `$ `。分支名通过 `git symbolic-ref`/`git rev-parse` 取，**每 2 秒最多查一次**，同一个提示符反复重画不会重复起子进程；`CONGMINAL_PROMPT_GIT=0` 彻底关掉这次 git 调用。只使用 ASCII 字符，不依赖终端字体里的图标字形。

Bash 4.4+ 用户在 `.bashrc` 最后 source `assets/shell/congminal.bash`。Bash 使用 PS0，不安装 DEBUG trap；保留字符串/数组形式的 PROMPT_COMMAND。macOS 自带 Bash 3.2 会明确提示版本不足并保持原来的 Shell 行为；可以使用 zsh 或自行安装较新 Bash。**本机验证了 zsh 5.9 的标记；Bash 5.2 在 Linux pty 里跑通标记、目录上报与 `CONGMINAL_PROMPT=pretty`（`shell_integration` 的集成测试会真起一个交互 bash）；zsh 的提示符美化未重跑（本机没有 zsh），只用 `zsh -n` 做语法检查。**

两个脚本都只在交互 Shell 启用，重复 source 不重复安装钩子。zsh 使用 preexec/precmd hook，返回零以免拦截后续 hook；zsh 给各 hook 保留相同的原始退出码。已有 hook 自己返回非零会按 zsh 约定阻断后续 hook，此时不能保证集成事件完整。主题或插件后续替换整个 PS1/PROMPT_COMMAND 时，须按其加载顺序重新配置集成；不承诺兼容所有异步主题。不要同时启用多份终端集成脚本。

脚本按字节百分号编码工作目录，不启动 `pwd`、`hostname`、Python 或其他辅助进程。没有集成时终端照常工作，目录和 Shell 状态显示“未知”。远程 OSC 7 的主机名单独保留，当前阶段不会把远程路径当本地路径读取。退出重开 Shell 即可卸载本次集成；移除 source 行可停用后续会话。

| 快捷键 | 操作 |
|---|---|
| Primary+↑ / ↓ | 上一条 / 下一条已完成命令 |
| Primary+F7 | 选择当前命令输出，未选命令时用最近一条 |
| Primary+F8 | 按片复制命令输出 |
| Primary+F9 | 显示/隐藏命令状态、耗时、退出码和工作目录 |
| Primary+F10 | 打开/关闭通知中心 |
| Primary+F11 | 切换通知静音 |
| ↑ / ↓、Del、Esc（通知中心内） | 浏览、清空、关闭；键盘输入不传给 Shell |
| Esc（命令状态条内） | 关闭状态条，取消尚未完成的复制 |

Primary 在 macOS 是 Cmd，其他平台是 Ctrl+Shift。上述动作均可在 `[keybindings]` 中重新绑定：`command-previous`、`command-next`、`select-command-output`、`copy-command-output`、`show-shell-status`、`show-notifications`、`toggle-notifications`。这些键仍受原有 IME/focus 路由保护。

## 状态与坐标

- OSC 7 接收 file URI，限制 3072 字节；校验 UTF-8、主机字符和百分号编码，拒绝控制字符、NUL、查询/片段及畸形输入。无效上报清除目录，不沿用一个伪装成当前目录的旧值。
- OSC 133 A/B/C/D 对应提示符、输入起点、执行起点、完成。只有在 B 后收到 C 才开始计时；只有执行中的 D 才能完成。D 的合法退出码是 0–255；缺省或无效码始终为未知。
- 命令行正文不另行捕获；耗时是解析器接收到 C/D 时的单调时钟差，包含 PTY 排队影响，不能当作进程 CPU 时间。
- 输入阶段取消、重复 D、异步提示符重画、Shell 退出和缺失事件不推断成功。130/143 显示中断，保留原始退出码。
- 标记使用稳定行坐标，随历史淘汰整条删除。重排/尺寸变化和清历史使范围失效；计时中的命令仍可接收显式退出码。进入备用屏幕使当前命令输出范围失效，备用屏幕内的 OSC 7/133 不修改主屏语义。
- 清屏、局部编辑和光标回写保守失效相交及之后的已完成输出范围，避免复制被覆盖后的内容；因此某些光标控制程序可能只能查看耗时/退出码，不能再选择原输出。没有为每条命令复制一份正文快照。
- 复制从现有网格/历史逐字读取，中文、宽字符、组合字符、软换行共用现有文本坐标规则。数据变化/历史丢失取消复制；超限时明确提示并保持剪贴板。普通选区复制也使用同一个有界分片机制。

## 通知

应用内中心以单行状态条浏览最近 128 个事件，为后续 M4 右侧栏提供数据模型。前台短命令成功安静；非零退出、长命令和后台完成按配置入中心，每个命令只处理一次。通知正文只有命令编号、耗时和结果；不包含命令文本、输出、目录、主机或认证内容。130/143 不使用成功模板；未知码明确显示未知。

```toml
[notifications]
muted = false
system = false
failures = true
long-commands = true
background = true
long-seconds = 10
```

失败类别关闭后不把失败改装为长命令/后台通知。静音期间不积压待补发事件；静音快捷键只改本次运行状态，配置热重载按文件恢复。中心清空不重置去重水位。后台 Shell 结束有独立事件，不推断最后一个命令结果；当前单会话自然退出仍关闭窗口，退出时会取消尚未发送的系统通知，不能保证结束通知送达，后续多会话阶段可保留已结束会话。

系统通知是**默认关闭的实验适配**，不进入默认性能保证：macOS 调用系统 osascript，Linux 调用可选 notify-send，Windows 调用 PowerShell/.NET NotifyIcon。统一固定模板，不传外部脚本文本；AppleScript 数据通过 argv，Windows 脚本仅插入本程序生成的 ASCII 模板及数值。没有新增 Cargo 依赖，也不自动安装辅助程序。

系统任务按需启动；一个工作槽、最多一个辅助进程、无延迟补发队列，每 10 秒最多一次。5 秒超时，取消/失败 kill + wait 回收辅助进程，任务结束后释放工作线程。通知不会激活窗口或设置键盘焦点。系统权限、桌面服务、勿扰模式或工具缺失可阻止显示；中心标记工具错误，进程退出零也不宣称用户已看到通知。未测量这些外部工具的真实送达及峰值 RSS，保留默认关闭；平台原生常驻接口可在后续对照后替换。

## 内存与调度边界

| 项目 | 限制/所有者 |
|---|---|
| 命令标记 | 每会话 1024 条，只存坐标/时间/退出状态，当前基准分配约 90 KiB |
| 尚未消费的完成事件 | 64 条；过载丢最旧事件并计数，不阻塞 PTY 或无限排队 |
| 工作目录 | 单条 URI 3072 字节，取代旧值 |
| 通知中心 | 128 条固定结构，无外部正文字符串 |
| 复制输出/普通选区 | 一个任务、2 MiB 文本；每片至多 32768 token / 约 2 ms，每 128 token 检查时钟 |
| 系统通知 | 默认零线程/子进程；启用且有事件才申请单个任务，结果/取消后释放；外部工具内存另列未验证 |

上限是分项预算，全局 512 MiB 资源准入属于 M4，当前不把它标为完成。命令解析在 OSC 边界做元数据维护，没有在每个普通字符上插入标记扫描。失效使用保守的结束坐标水位，正常追加输出不会逐字符遍历 1024 条记录。空闲、状态条关闭或命令运行中均没有新增轮询定时器。

## 验证与后续验收

本机单元/真实 PTY/主线程集成覆盖退出码、管道、取消、Shell 退出、未知/重复边界、备用屏幕、重排/编辑/历史淘汰、URI、复制超限/取消、通知去重/静音/类别/速率、工具失败/取消回收及焦点隔离。实际渲染帧见 [通知中心预览](previews/m3-notifications.png)，它来自本程序 Renderer，不代替操作系统截图或三平台 GUI 验收。

性能原始数据与验证限制见 [M3 性能评估](evaluations/m3.md)。Windows/Linux 仅交叉检查；Bash 4.4+、真实桌面通知送达/权限与外部工具峰值、三平台输入法和 30 分钟长测仍待补，不能以短测替代。

协议依据：[iTerm2 OSC 133/7 文档](https://iterm2.com/documentation-escape-codes.html)、[zsh hook 约定](https://zsh.sourceforge.io/Doc/Release/Functions.html)、[Bash PS0/PROMPT_COMMAND](https://www.gnu.org/software/bash/manual/html_node/Bash-Variables.html)、[桌面通知规范](https://specifications.freedesktop.org/notification/latest-single/)。
