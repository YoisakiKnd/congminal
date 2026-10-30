# v0.1.0 发布验收记录（进行中）

本目录记录新正式版目标的实际检查；不会改写此前已冻结的资源审计。版本尚未发布。

| 检查点 | 原生结果 | 范围 |
| --- | --- | --- |
| `e016754` / [40979440483](https://github.com/YoisakiKnd/congminal/actions/runs/40979440483) | 未启动测试 | 工作流命令中的 `::` 需要 YAML 块；已修正，不能计作运行验收 |
| `025b237` / [40981013654](https://github.com/YoisakiKnd/congminal/actions/runs/40981013654) | 三平台通过 | 认证 IPC、秘密生命周期、Windows 文件拥有者、Clippy、helper 构建 |
| `e8c56be` / [40983256800](https://github.com/YoisakiKnd/congminal/actions/runs/40983256800) | Windows、Linux 完整单元套件和 release 构建通过；macOS 完整套件失败 | macOS 为 872 通过、3 失败、50 忽略；3 项都来自原生监控采样，失败日志原样保留 |
| `16e40e6` / [40984152876](https://github.com/YoisakiKnd/congminal/actions/runs/40984152876) | 三平台通过 | 完整原生单元套件、Clippy、主程序和 helper release 构建；验证旧 macOS 内核回复的兼容修复 |
| `7a4fae1` / [40992773288](https://github.com/YoisakiKnd/congminal/actions/runs/40992773288) | macOS 通过；Linux Clippy、Windows 新建目录内发布失败 | 本机完整套件 883 + helper 20 通过；原生 CI 发现 Linux 条件编译后冗余闭包，以及 Windows 树目录 DELETE 访问权与复核打开 share=读/写冲突，失败原始日志保留 |

macOS 原因与修复：新版 `libc` 的 `vm_statistics64` 包括旧内核未提供的尾部字段；原实现要求返回长度与新版结构完全相等。修复只要求本程序访问的字段前缀完整、且返回长度不超过接收缓冲，继续拒绝截断和越界。新增完整旧版本/缺字段/超长回复检查；本机原生监控子集为 6 通过、1 忽略，随后 `16e40e6` 的三平台 CI 全部通过。

本机新上传准备/目录识别/续传回归为 7 通过。曾因测试残留文件名称不满足 `.congminal-*.part` 约束而失败，修正夹具后通过；实现继续保留该约束，并拒绝把最终目标当作续传暂存。

目录和临时副本修复检查点：macOS 工作区回归 19 项通过，包括空目录落地、目录准备失败时停止文件提交、脱离工作区状态的完成回调仍能打开副本且保留编辑后上传目标。macOS Clippy 和 Windows 全目标交叉 Clippy 通过。目录扫描的 6 项边界回归、本地句柄相对建树、目录页额度保留回归均在本机通过；真实本机 `sftp-server` 的 11 项互操作检查通过，覆盖流式二进制、目录、续传与原子提交。这些结果不替代该检查点后续的 Windows 原生运行。

新适配未增加运行时依赖或目录线程：沿用 2 个工作槽、64 等待槽；远端目录队列最多 128，目录结果最多 512，文件最多 2000，深度最多 16。目录结果表、页、批次和票据在其存活期保留额度，每帧最多提交 4 个文件。空目录创建与临时副本的私有目录创建都由后台执行。Windows 文件打开采用 [ShellExecuteExW](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shellexecuteexw) 与 [NOASYNC 交接语义](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/ns-shellapi-shellexecuteinfow)，不把文件名交给 `cmd`。

`7a4fae1` 后的修复让普通树目录不再申请 DELETE；私有 staging 目录仍保留删除权限，句柄绑定和不跟随 reparse point 的约束不变。Linux 将目录 ACL 初始化抽成独立函数，消除条件编译后无意义的闭包。当前 macOS、Linux 和 Windows 的全目标交叉 Clippy 均通过；Windows 新目录内发布仍以后一轮原生 CI 为准。

客户端兼容基线：强制 askpass 的 `SSH_ASKPASS_REQUIRE` 来自 [OpenSSH 8.4](https://www.openssh.org/txt/release-8.4)；连接参数还使用 [OpenSSH 8.7](https://www.openssh.org/txt/release-8.7) 引入的 `ForkAfterAuthentication`，因此正式文档列为 8.7+，Windows 另要求支持 askpass 的移植版。系统旧客户端不在已支持范围内。

以上是单元与模块运行证据，不等于真实 SSH 服务、三平台 SFTP 窗口、安装包或性能验收。完整检查清单见 [正式版执行目标](../../execution-v0.1.0.md)。

默认正式构建准备：版本设为 `0.1.0`，默认 `sftp` 包含 helper，旧特性作为兼容别名；配置默认开放入口但仍延迟建立服务。新版默认构建本机完整回归 **884 项 + helper 20 项通过，50 项环境测试未运行**；默认入口不创建服务/认证的回归通过。Rust 1.88 最低版本检查、三平台包布局及其原生运行结果继续在后续检查点记录。README 与配置示例已重写，CI 已区分稳定发布和预发布并打包 helper、应用、文档和 SHA256。此时尚未发布正式标签。
