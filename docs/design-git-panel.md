# 本地 Git 面板（M7 第一步：只读状态与受限 diff）

右栏新增 Git 页，显示活动窗格工作目录对应仓库的分支、暂存/未暂存/未跟踪条目和受大小限制的 diff。只读：不执行任何写仓库的 git 子命令（无 `add`/`commit`/`push`/`stash`），也不弹凭据。

## 入口、配置与快捷键

| 项目 | 值 |
| --- | --- |
| 动作 | `Action::ShowGitPanel`（配置名 `show-git-panel`） |
| 默认绑定 | `Primary+G`（macOS 为 Cmd+G，其他平台为 Ctrl+Shift+G） |
| 右栏页签 | `状态` / `Git`，点击行 0 左半/右半切换 |
| 按钮行（行 1） | 列表页：`刷新`、`打开差异`；diff 页：`刷新`、`暂存侧`↔`工作区`（同一按钮切换）、`返回` |
| 关闭 | 关闭右栏（`Primary+B` 或点击 `状态` 页）即取消任务并释放快照 |

`[git]` 段（见 `assets/congminal.example.toml`）：

```toml
[git]
enabled = true            # 关闭后不创建任务、不运行 git，面板显示“已停用”
# program = "/usr/bin/git"  # 不写则用 PATH 中的 git
refresh-seconds = 2       # 0.5–60，实际下限 1s
timeout-seconds = 20      # 1–120，超时杀子进程并保留上一次结果
max-files = 2000          # 上限 5000
diff-mib = 8              # 上限 32
```

越界值只产生配置警告并保留默认，不会静默放大预算（`src/config/git.rs` 测试覆盖）。

## 正常流程

1. 打开 Git 页后，面板取活动窗格的目录：远程会话直接显示“未知”（远程通道未实现）；否则用 shell 的 OSC 7 目录，其次配置档/全局 `working_directory`，最后进程当前目录。
2. 首次查询延迟 300 ms 去抖后提交，随后按 `refresh-seconds`（至少 1 s）周期复查；`刷新` 按钮或重新打开面板会立即重排。
3. 行 2 显示 `分支 master · repo`（detached 显示 `分离 HEAD abc1234`、未出生显示 `尚无提交`、裸仓库显示 `裸仓库 · 目录名`），行 3 显示 `暂存 0 · 未暂存 1 · 未跟踪 1`，超过 1 s 的查询追加 ` · 慢 1.2s`。
4. 行 4 起按路径排序列出条目，两列状态字母（`M`/`A`/`D`/`R`/`?`/`U` 等）与转义后的文件名。
5. 点击条目选中并打开 diff（`打开差异` 按钮同样可用）。默认看有改动的一侧，diff 页可在工作区/暂存侧之间切换，`返回` 回到列表并释放 diff 文本。

## 取消、超时、失败、超限与关闭

| 情况 | 行为 |
| --- | --- |
| 切走页签 / 窗口失焦 / 关右栏 | 取消在途查询（`LatestTask::invalidate`），结果丢弃；失焦保留旧快照，关栏释放快照与目录票据 |
| 切换窗格或目录 | 同一仓库（新目录在当前快照根内）复用旧结果；否则清空并重新去抖查询 |
| 超时 | 到 `timeout-seconds` 杀掉子进程并 `wait`，错误行提示超时，保留上一次结果 |
| 非仓库目录 | 显示“不在 Git 仓库中”，不启动重试风暴（按周期复查） |
| 缺少 git 程序 | 显示“找不到 git 可执行程序”，不创建任务 |
| 额度不足 | 查询前按 `Kind::Git` 申请内存票据，失败即报“额度不足”，不启动子进程 |
| 大仓库 | 条目数到 `max-files`、status 输出 4 MiB、diff 8 MiB 时截断并标 `已截断`；diff 最多 8000 行 |
| 关闭应用 | 任务在 `Drop` 中取消，子进程被杀，票据归还 |

代次隔离：每次查询递增 `generation`，回包必须同时匹配 `owner`（窗格 id）与 `generation`，切窗格/切目录后的迟到结果直接丢弃。

## 后台任务、线程、子进程、缓存与数据所有权

- 每个在途查询一个 `LatestTask` 线程（`git-status` / `git-diff`，命名线程，受全局线程预算准入）；一次只有一个在途查询，新请求覆盖旧请求。
- 每次查询最多一个 `git` 子进程（`run()`），stdin 关闭，stdout/stderr 各一个托管读取线程（`runtime::prepare("git-io")`），超限后继续读完丢弃。
- 缓存：仅保留最近一次 `Snapshot`（最多 5000 条、status 输出 4 MiB）与一份 `Diff`（最多 8 MiB），都按 `Kind::Git` 计费；关闭面板即释放。没有跨进程/磁盘缓存。
- 数据所有权：`Panel` 持有目录票据、快照、diff 与任务句柄；`Workspace` 持有 `Panel`；子进程与读取线程由 `run()` 内部回收。
- 环境：`GIT_OPTIONAL_LOCKS=0`（不抢 index.lock）、`GIT_TERMINAL_PROMPT=0`（不弹凭据）、`GIT_PAGER=cat`、`LC_ALL=C`。

## Git CLI 与库的比较结论

先交付 CLI 只读路径，理由：`git status --porcelain=v1 -z` 是稳定机器接口，NUL 分隔天然处理空格/换行/非法 UTF-8 路径；worktree、子模块、detached HEAD、未出生 HEAD、裸仓库与全局配置（`core.excludesFile`、`.gitignore`）语义与用户命令行完全一致，无需自建索引缓存。`git2` 需 libgit2 C 依赖与自有配置解析，`gix` 需要自建索引并与其 ignore 实现保持一致；两者都会引入第二套仓库语义。库路径的收益（无子进程、可增量读索引）留到需要高频刷新或写操作时再评估，届时转换为同一 `Entry`/`Snapshot` 模型即可（`src/git/mod.rs` 已按此分层：`status()`/`diff()` 是唯一接触子进程的地方）。

## 测试

```
cargo test git                # src/git/git_tests.rs（11 项，真实临时仓库）
cargo test git_panel          # 面板生命周期与配置（3 项）
cargo test config::           # [git] 解析与越界（24 项）
```

覆盖：分支/分离/未出生/裸仓库识别、子目录解析到仓库根、改名记录、控制字符与非法 UTF-8 路径转义、条目上限截断、非仓库/缺程序/额度不足、diff 行索引与截断、两侧 diff、取消与超时后子进程回收（unix 伪造 git 脚本）、面板打开→查询→打开 diff→返回→切页→关栏的完整路径（含 `bytes()` 归零）、缺程序与停用配置的失败路径。

手工验证：`CONGMINAL_GIT_PREVIEW_PATH=/tmp/list.ppm CONGMINAL_GIT_DIFF_PREVIEW_PATH=/tmp/diff.ppm cargo test git_panel` 会导出列表页与 diff 页各一帧 PPM（1200x700），用于检查右栏排版；两帧已在 Linux 本机核对。

## 已知限制与未验收项

- 远程会话（SSH）的 Git 状态未实现，面板显示“未知”；远程写操作与远程监控同批处理。
- 暂存/取消暂存、提交、拉取/推送、冲突查看（M7 第二步）未实现。
- 子模块内部改动只按父仓库的 ` M` 记录显示，未递归进入子模块。
- shell 未发 OSC 7 时回退到配置的 `working_directory` 或进程当前目录，可能与终端里的实际目录不同（普通交互 shell 都会发 OSC 7）。
- 只在 Linux 本机验证；macOS/Windows 未运行，交叉编译不算真机验证。
- diff 长行（如 `diff --git a/… b/…`）超出右栏宽度时按列裁切，未折行，也没有横向滚动；折行会改变行号与滚动映射，留到下一步统一处理。
- 大仓库（10 万级条目）与网络文件系统的刷新成本未实测，`max-files`/4 MiB 截断路径已测但真实大仓库延迟未采样。
