# v0.1.0 发布验收记录（进行中）

本目录记录新正式版目标的实际检查；不会改写此前已冻结的资源审计。版本尚未发布。

| 检查点 | 原生结果 | 范围 |
| --- | --- | --- |
| `e016754` / [40979440483](https://github.com/YoisakiKnd/congminal/actions/runs/40979440483) | 未启动测试 | 工作流命令中的 `::` 需要 YAML 块；已修正，不能计作运行验收 |
| `025b237` / [40981013654](https://github.com/YoisakiKnd/congminal/actions/runs/40981013654) | 三平台通过 | 认证 IPC、秘密生命周期、Windows 文件拥有者、Clippy、helper 构建 |
| `e8c56be` / [40983256800](https://github.com/YoisakiKnd/congminal/actions/runs/40983256800) | Windows、Linux 完整单元套件和 release 构建通过；macOS 完整套件失败 | macOS 为 872 通过、3 失败、50 忽略；3 项都来自原生监控采样，失败日志原样保留 |
| `16e40e6` / [40984152876](https://github.com/YoisakiKnd/congminal/actions/runs/40984152876) | 三平台通过 | 完整原生单元套件、Clippy、主程序和 helper release 构建；验证旧 macOS 内核回复的兼容修复 |

macOS 原因与修复：新版 `libc` 的 `vm_statistics64` 包括旧内核未提供的尾部字段；原实现要求返回长度与新版结构完全相等。修复只要求本程序访问的字段前缀完整、且返回长度不超过接收缓冲，继续拒绝截断和越界。新增完整旧版本/缺字段/超长回复检查；本机原生监控子集为 6 通过、1 忽略，随后 `16e40e6` 的三平台 CI 全部通过。

本机新上传准备/目录识别/续传回归为 7 通过。曾因测试残留文件名称不满足 `.congminal-*.part` 约束而失败，修正夹具后通过；实现继续保留该约束，并拒绝把最终目标当作续传暂存。

目录和临时副本修复检查点：macOS 工作区回归 19 项通过，包括空目录落地、目录准备失败时停止文件提交、脱离工作区状态的完成回调仍能打开副本且保留编辑后上传目标。macOS Clippy 和 Windows 全目标交叉 Clippy 通过。目录扫描的 6 项边界回归、本地句柄相对建树、目录页额度保留回归均在本机通过；真实本机 `sftp-server` 的 11 项互操作检查通过，覆盖流式二进制、目录、续传与原子提交。这些结果不替代该检查点后续的 Windows 原生运行。

新适配未增加运行时依赖或目录线程：沿用 2 个工作槽、64 等待槽；远端目录队列最多 128，目录结果最多 512，文件最多 2000，深度最多 16。目录结果表、页、批次和票据在其存活期保留额度，每帧最多提交 4 个文件。空目录创建与临时副本的私有目录创建都由后台执行。Windows 文件打开采用 [ShellExecuteExW](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shellexecuteexw) 与 [NOASYNC 交接语义](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/ns-shellapi-shellexecuteinfow)，不把文件名交给 `cmd`。

以上是单元与模块运行证据，不等于真实 SSH 服务、三平台 SFTP 窗口、安装包或性能验收。完整检查清单见 [正式版执行目标](../../execution-v0.1.0.md)。
