# v0.1.0 发布验收记录（进行中）

本目录记录新正式版目标的实际检查；不会改写此前已冻结的资源审计。版本尚未发布。

| 检查点 | 原生结果 | 范围 |
| --- | --- | --- |
| `e016754` / [40979440483](https://github.com/YoisakiKnd/congminal/actions/runs/40979440483) | 未启动测试 | 工作流命令中的 `::` 需要 YAML 块；已修正，不能计作运行验收 |
| `025b237` / [40981013654](https://github.com/YoisakiKnd/congminal/actions/runs/40981013654) | 三平台通过 | 认证 IPC、秘密生命周期、Windows 文件拥有者、Clippy、helper 构建 |
| `e8c56be` / [40983256800](https://github.com/YoisakiKnd/congminal/actions/runs/40983256800) | Windows、Linux 完整单元套件和 release 构建通过；macOS 完整套件失败 | macOS 为 872 通过、3 失败、50 忽略；3 项都来自原生监控采样，失败日志原样保留 |
| `16e40e6` / [40984152876](https://github.com/YoisakiKnd/congminal/actions/runs/40984152876) | 三平台通过 | 完整原生单元套件、Clippy、主程序和 helper release 构建；验证旧 macOS 内核回复的兼容修复 |
| `7a4fae1` / [40992773288](https://github.com/YoisakiKnd/congminal/actions/runs/40992773288) | macOS 通过；Linux Clippy、Windows 新建目录内发布失败 | 本机完整套件 883 + helper 20 通过；原生 CI 发现 Linux 条件编译后冗余闭包，以及 Windows 树目录 DELETE 访问权与复核打开 share=读/写冲突，失败原始日志保留 |
| `f32d027` / [40993736893](https://github.com/YoisakiKnd/congminal/actions/runs/40993736893) | 三平台原生套件通过 | Windows 新建树内发布修复通过；默认 SFTP 与 helper 构建通过 |
| `4d96d7c` / [40994447302](https://github.com/YoisakiKnd/congminal/actions/runs/40994447302) | 三平台安装包与 MSRV 通过 | macOS 主程序/helper 双架构、`.app` 签名校验；三平台原生应用启动/退出 smoke、包布局、版本及 SHA256 文件。未发布标签 |

macOS 原因与修复：新版 `libc` 的 `vm_statistics64` 包括旧内核未提供的尾部字段；原实现要求返回长度与新版结构完全相等。修复只要求本程序访问的字段前缀完整、且返回长度不超过接收缓冲，继续拒绝截断和越界。新增完整旧版本/缺字段/超长回复检查；本机原生监控子集为 6 通过、1 忽略，随后 `16e40e6` 的三平台 CI 全部通过。

本机新上传准备/目录识别/续传回归为 7 通过。曾因测试残留文件名称不满足 `.congminal-*.part` 约束而失败，修正夹具后通过；实现继续保留该约束，并拒绝把最终目标当作续传暂存。

目录和临时副本修复检查点：macOS 工作区回归 19 项通过，包括空目录落地、目录准备失败时停止文件提交、脱离工作区状态的完成回调仍能打开副本且保留编辑后上传目标。macOS Clippy 和 Windows 全目标交叉 Clippy 通过。目录扫描的 6 项边界回归、本地句柄相对建树、目录页额度保留回归均在本机通过；真实本机 `sftp-server` 的 11 项互操作检查通过，覆盖流式二进制、目录、续传与原子提交。这些结果不替代该检查点后续的 Windows 原生运行。

新适配未增加运行时依赖或目录线程：沿用 2 个工作槽、64 等待槽；远端目录队列最多 128，目录结果最多 512，文件最多 2000，深度最多 16。目录结果表、页、批次和票据在其存活期保留额度，每帧最多提交 4 个文件。空目录创建与临时副本的私有目录创建都由后台执行。Windows 文件打开采用 [ShellExecuteExW](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shellexecuteexw) 与 [NOASYNC 交接语义](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/ns-shellapi-shellexecuteinfow)，不把文件名交给 `cmd`。

`7a4fae1` 后的修复让普通树目录不再申请 DELETE；私有 staging 目录仍保留删除权限，句柄绑定和不跟随 reparse point 的约束不变。Linux 将目录 ACL 初始化抽成独立函数，消除条件编译后无意义的闭包。当前 macOS、Linux 和 Windows 的全目标交叉 Clippy 均通过；Windows 新目录内发布仍以后一轮原生 CI 为准。

客户端兼容基线：强制 askpass 的 `SSH_ASKPASS_REQUIRE` 来自 [OpenSSH 8.4](https://www.openssh.org/txt/release-8.4)；连接参数还使用 [OpenSSH 8.7](https://www.openssh.org/txt/release-8.7) 引入的 `ForkAfterAuthentication`，因此正式文档列为 8.7+，Windows 另要求支持 askpass 的移植版。系统旧客户端不在已支持范围内。

以上是单元与模块运行证据，不等于真实 SSH 服务、三平台 SFTP 窗口、安装包或性能验收。完整检查清单见 [正式版执行目标](../../execution-v0.1.0.md)。

默认正式构建准备：版本设为 `0.1.0`，默认 `sftp` 包含 helper，旧特性作为兼容别名；配置默认开放入口但仍延迟建立服务。新版默认构建本机完整回归 **884 项 + helper 20 项通过，50 项环境测试未运行**；默认入口不创建服务/认证的回归通过。Rust 1.88 最低版本检查、三平台包布局及其原生运行结果继续在后续检查点记录。README 与配置示例已重写，CI 已区分稳定发布和预发布并打包 helper、应用、文档和 SHA256。此时尚未发布正式标签。

## 真实回环与窗口

固定源码 `f32d027` 的真实 OpenSSH 入口、独立认证、上传/下载及退出场景通过；12 个工作区场景通过，包括首次主机确认/拒绝、1023 字节口令、错误口令、等待、取消、导航、换配置和清理失败确认。`0519a4f` 的工作区文件流程 16 项与 Service 传输 15 项通过，覆盖每次 32 MiB 的双向数据、覆盖/拒绝、变更/过期版本、后台切换、排队压力、取消、重试、属性回查与退出。原始 JSON 保存 source/tool/manifest 哈希和日志。先前夹具仍使用旧按钮列/旧列表行，导致两次失败；改为当前按钮列和实际绘制的命中矩形后通过，失败记录保留。

macOS 本机 `.app` 已通过真实 Cocoa 窗口点击、受保护口令输入、真实 helper/OpenSSH 认证、目录显示、路径表单/审阅和下载。60 字节中英文本逐字节相同，SHA256 为 `744cc72f5c8b09f43131f93a0abec0642d7fde72fd953349395d9ee75aea7528`。此轮也发现成功任务误显示“残留”，已按“发布消耗暂存名且清理确认”修正，未知/失败清理仍保留路径。窗口夹具 20 分钟超时后自动回收，因此这次记录**不能计作正常关闭验收**，后续需重新验证；并非 Windows/Linux 桌面人工验收。

Windows 官方 OpenSSH 回环夹具首次失败来自测试文件残留的显式 `OWNER RIGHTS` ACE；严格服务端拒绝 authorized_keys。夹具改为建立仅测试用户/SYSTEM/Administrators 的全新 DACL，尚待后续原生重跑，不能把已有模块测试当作此回环已通过。包流水线的 macOS `lipo` 校验参数顺序错误也已修复，随后三平台包全部通过。

## 当前资源计量

原始数据保留初次失败与修复后结果；旧冻结审计不改写。以下为当前源码临时副本的 **Rust requested-byte** 测量，排除计量器头、分配器、C/系统、内核、实际栈与 RSS；不等于全堆或进程 RSS 硬上限。并行运行了部分功能/编译检查，仪器下绘制耗时不用于发布延迟结论。

| 范围 | 实际 Rust 请求与额度 | 证据 |
| --- | --- | --- |
| 2000 文件 / 5 目录的真实 Scan/Plan | 结果存活 520842 B ≤ 额度 601170 B；实际峰值 608234 B ≤ 额度峰值 3618282 B；最终都为零 | `directory-allocation-v2.json` |
| 递归扇出直到 512 目录 | 结果存活 275008 B ≤ 额度 295616 B；峰值 330798 B ≤ 额度峰值 3445728 B；最终都为零 | 同上；单页先前仅 512 条，修正为多目录页，失败夹具保留 |
| 文件 DTO 队列、IO 余量填满、取消、迟到状态 | 峰值 11735945 B ≤ 父额度 16778703 B；最终请求与额度均为零 | `0519a4f-file-service-allocation.json` |
| 工作区、最大路径、512 条本地选择、取消与迟到输入 | 实际峰值 1964317 B；迟到输入释放后 SFTP 额度归零，所有持有者释放后 Rust 请求归零 | `workspace-allocation-v3.json` |

第一次工作区计量发现通用 `Renderer::chrome` 为裁剪每帧创建 `String`，200 次路径表单重绘新增 5800 次分配。改为过滤至多 256 字符并按显示列有界迭代，保留 Unicode 省略显示；修复后路径表单、属性回查、本地选择、退出警告和重试审阅各 200 次直接暖绘制均为 **0 分配 / 0 realloc**。这不代表冷字形或整个窗口事件循环零分配。


后续原生检查点 `c88c8ad` 已通过 Windows 官方 OpenSSH 加密密钥、Linux 真实密码、目录、双向 256 KiB 数据、取消及确认 join 的回环门槛；当前正在把该门槛接到正式标签 CI，后续提交仍需重新运行。Windows 普通树目录允许已验证可信拥有者的 OWNER RIGHTS（well-known SID 71）ACE；认证票据和暂存文件仍使用原先严格单 ACE 私有检查。

macOS 复测补齐正常退出：`67d3395` 的 `.app` 在真实 Cocoa 窗口完成原生受保护输入、路径审阅、中文名粘贴上传（22 B）和下载（60 B），两向内容一致，没有暂存残留，Cmd+Q 后夹具得到 `NATIVE_APP_EXIT=0`。证据为 `native-macos-files.json` 和 `native-macos-transfers.png`。CUA 的直接非 ASCII 键入未保留中文，改用系统粘贴后原名保留；此次只证明原生 UTF-8 粘贴路径，不能当作中文输入法组词验收。退出后一次状态查询自动重新打开了空白测试实例，已另行关闭。

`67d3395-auth-cost.json` 为同一 OpenSSH/加密密钥/父端/数据负载的五组交替对照，固定假答案 helper 是成本夹具，不是安全可替代实现：无人工等待时，有界 IPC 的连接就绪中位数 192528 µs，相比固定答案 179175 µs 增加 13353 µs；上传 250.225 / 250.305 MiB/s，下载 252.880 / 253.381 MiB/s。人为延迟 1 秒时，就绪中位数增量 3542 µs，上传 249.264 / 252.346 MiB/s，下载 252.396 / 253.293 MiB/s。流式阶段额度峰值两者相同为 3430102 B；认证额度多 1205 B，客户端树 RSS 中位峰值多约 0.30–0.50 MiB。三进程共同等待区间单核 CPU 中位数 1.241% / 1.186%，增量约 0.055 个百分点；夹具轮询、OpenSSH 和 helper 均在其中，不是生产本地终端空闲 CPU。

此结果说明认证有可见的一次连接成本，传输中位数无超过 5% 的回退；它不证明冷启动、原生输入 P95 或全部平台吞吐。没有以去掉私有 IPC 换取固定假答案的更快结果。30 分钟重复生命周期仍在进行，本地功能关闭/启用的启动与空闲增量另行记录。
