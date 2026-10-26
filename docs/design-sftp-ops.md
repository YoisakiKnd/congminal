# SFTP 远端写操作（M6：新建目录、重命名、删除）

左栏“文件”页的 SFTP 目录视图新增三个远端写操作。所有写操作都由用户显式点击或按键触发，走既有 Service 队列、预算、取消与认证流程，不新增线程、子进程或后台设施。

平台范围：SFTP 服务层本就是 Unix 专用（`src/sftp/service.rs` 以 `cfg(unix)` 编译），因此写操作函数（`operations::mkdir/rename/remove`）与本地 `sftp-server` 互操作测试同样只在 Unix 构建中编译；Windows/macOS 交叉编译仍通过 `cargo check` 与 `clippy -D warnings`，只是不含这条写路径。

## 入口、配置与快捷键

| 项目 | 值 |
| --- | --- |
| 入口 | 左栏“文件”页（SFTP 目录视图），操作行（行 3）六个槽位 |
| 按钮 | `[新建]` `[改名]` `[删除]` `[上传]` `[下载]` `[任务]` |
| 前置条件 | 已有远端目录视图；`改名`/`删除` 需要先选中条目 |
| 名称输入 | `Enter` 提交、`Esc` 取消、`Backspace`/`Delete` 删除、`←`/`→` 移动、`Home`/`End` 跳到首尾；输入期间键盘不进终端 |
| 删除确认 | `Enter` 执行删除、`Esc` 取消；行 2 显示 `删除目录 主机:绝对路径` |
| 关闭 | 关闭右栏/切换左栏页/窗口失焦都会取消在途写任务并清空输入与确认 |

按钮标签按每槽可用显示列自动降级：完整 `[新建]` → 双字 `新建` → 单字 `新`，避免窄侧栏把标签裁成半个词。SFTP 页沿用既有 `[sftp]` 配置与 `sftp-preview` 入口，本批不新增配置项。

## 正常流程

1. 目录视图加载完成后，操作行显示六个按钮。
2. `新建`：行 2 变成 `新目录名 › `，输入名称后 `Enter` 在当前目录下创建。
3. `改名`：先选中条目，点 `改名`，行 2 变成 `新名称 › 原名`，`Enter` 把所选条目改成输入的名称。
4. `删除`：先选中条目，点 `删除`，行 2 变成 `删除目录/删除文件 主机:绝对路径`，`Enter` 才真正下发（两次确认，且目标主机与路径同时可见）。
5. 提交后行 3 显示 `正在等待远端确认：新建目录/重命名/删除…`，其余按钮不接受点击。
6. 成功后自动重读当前目录并给出通知（`已创建目录`/`已重命名`/`已删除`）；失败保留原列表并在错误行显示服务端错误。

## 取消、超时、失败、超限与关闭

| 情况 | 行为 |
| --- | --- |
| 输入阶段按 `Esc` | 关闭输入，不产生任何远端请求 |
| 确认阶段按 `Esc` | 关闭确认，不删除 |
| 提交前已有在途写任务 | 返回 `Failure::Cancelled`，不排队第二个 |
| 名称非法（空、含 `/`、`.`、`..`、含 NUL、超 1024 字节） | 本地拒绝为 `Failure::Invalid`，不触网；超长输入在录入时即丢弃 |
| 服务层校验（相对路径、`/`、`/a/..`、超长名、`rename` 源目标相同） | `Failure::Invalid`，不排队（`src/sftp/service.rs::mutation_path`） |
| 额度不足 | 名称输入按 `size_of::<NameInput>() + 2×1024 + 256` 申请 `Kind::Sftp` 票据，写任务按 `size_of::<Mutation>() + 128` 申请；失败报“额度不足”，不排队 |
| 需要认证（口令/主机校验） | 复用同一前台认证覆盖层，认证完成后写任务才继续；取消认证即取消写任务 |
| 超时 | 每个写任务 `Duration::from_secs(10)`，超时由 Service 取消票据 |
| 连接关闭/回收 | 面板切到 `Failure::Connection`，写任务随 Service 关闭一起取消 |
| 删除非空目录 | 服务端返回错误（`Rmdir` 不递归），目录内容保留，面板显示错误 |
| 重命名目标已存在 | 不替换（`replace = false`），返回服务端错误，原目标内容保留 |
| 关闭应用 | 在途写任务取消，票据归还，`Drop` 不重试 |

## 后台任务、线程、子进程、缓存与数据所有权

- 不新增线程：写请求进入既有 SFTP Service（2 活动 / 64 等待 / 16 MiB 子池），完成结果由既有 `service.take()` 在 `sftp_tick` 中取出。
- 不新增子进程：仍由 helper/OpenSSH 会话承载；helper 缺失时立刻失败为 `Failure::Authentication`。
- 缓存：只有一个 `Mutation` 槽位（票据、绑定、认证句柄、操作类型、额度票据）；名称输入上限 1024 字节、预编辑 256 字节，均按 `Kind::Sftp` 计费。没有跨请求缓存。
- 数据所有权：`Workspace` 持有 `remote_files::State`；`State` 持有 `form`/`confirm`/`mutation`；`Mutation` 持有 `Ticket` 与 `Authentication`；Service 持有实际请求与结果。成功后的目录重读走既有 `submit_remote_directory`，票据仍是原目录票据。
- 认证归属：`Mutation.binding` 与目录视图同源（窗格 id、profile、generation），换窗格/换配置档后的迟到结果直接丢弃。

## 协议层与薄适配

`src/sftp/operations.rs` 新增三个函数，全部复用既有 `Request`/`Expected` 通道：

- `mkdir(path)` → `Request::Mkdir`，`Expected::Status`，只接受 `code == 0`。
- `rename(from, to)` → `Request::Rename { replace: false }`，不覆盖既有目标。
- `remove(path, directory)` → `PathOp::Rmdir` 或 `PathOp::Remove`，不递归。

出错时通道 `abort()`（与既有目录操作一致），由上层映射为 `Failure::Remote(code)`。没有引入新的 SFTP 扩展或库依赖。

## 测试

```
cargo test --locked remote_files          # 面板逻辑、输入上限、键盘归属、窄栏裁切
cargo test --locked service_prepare       # 服务层参数校验（不触网）
CONGMINAL_TEST_SFTP_SERVER=/usr/lib/openssh/sftp-server \
  cargo test --locked local_subsystem_creates_renames_and_removes_entries -- --ignored
```

覆盖：新建目录成功与重名失败（失败后通道失效）、重命名成功与目标已存在不覆盖、删除文件/空目录成功与非空目录被拒、提交后额度归零；面板侧覆盖无选中时改名/删除被拒、删除确认行含主机与路径且 `Esc` 不留任务、名称 1024 字节封顶与非法名称本地拒绝、合法名称进入在途任务并在 helper 缺失时留下 `Failure::Authentication`、输入期间键盘不写入终端、窄侧栏保留名称末尾。

手工验证（Linux 本机已核对）：

```
CONGMINAL_SFTP_OPS_PREVIEW_PATH=/tmp/ops.ppm \
CONGMINAL_SFTP_FORM_PREVIEW_PATH=/tmp/form.ppm \
CONGMINAL_SFTP_DELETE_PREVIEW_PATH=/tmp/delete.ppm \
cargo test --locked name_input_is_bounded
```

三帧分别为操作行、名称输入、删除确认；窄侧栏下按钮降级为单字，确认行保留路径末尾（`删除目录 fix…/tmp/folder`）。

## 已知限制与未验收项

- 仍只在 `sftp-preview`/测试构建中可用，默认发布入口未打开（与既有 SFTP 面板一致）。
- 没有递归删除、批量删除、拖拽移动、上传覆盖同一批的远端覆盖确认；删除只做单条目。
- 重命名只改名称，不支持跨目录移动（目标恒为当前目录）。
- 写操作没有进度条：远端确认期间只显示等待提示，长延迟目录只能等超时。
- 真实 SSH 主机回环（自建 sshd + helper）未验证：本机没有 `sshd`，只有 `sftp-server`；协议层与面板层分别用本地子系统和合成目录验证，不能替代真机验收。
- Windows 未运行；交叉编译不算真机验证。
