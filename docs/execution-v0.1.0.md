# v0.1.0 正式版执行目标

目标已完成：补全 SFTP 三平台交付、维持资源边界、完成实际验证，并推送 main、v0.1.0 标签和首个正式 GitHub Release。

**[下载 v0.1.0](https://github.com/YoisakiKnd/congminal/releases/tag/v0.1.0)**。发布时间 2026-10-30 07:10 UTC；固定实现提交 `fe861a467caeb332ff79b1711feaec814253730e`。版本标签保持不变，发布后仅补充文档与验收证据。

## 发布检查

- [x] 核对现状及默认包缺口，明确系统 OpenSSH 与内置 SFTP 的职责。
- [x] Windows 私有票据、认证命名管道、服务端身份验证、最大提示/答案、ACK、超时、取消与额度归还。
- [x] Windows 本地源身份、句柄绑定暂存和原子发布、默认不覆盖/显式覆盖，以及准确的清理与残留结果。
- [x] 三平台工作区、文件选择、认证、传输、退出确认及 helper 自动发现；默认构建包含 SFTP/helper。
- [x] 真实 OpenSSH 主机确认/变更拒绝、密钥/密码认证、文件与目录双向传输、覆盖、取消、异常与关闭。
- [x] macOS Cocoa 原生认证、中文名粘贴上传、下载、审阅及正常退出；Windows/Linux 原生单元、真实 SFTP 和应用启动/退出 CI。
- [x] 目录与批量操作保留存活额度；2 工作槽/64 等待槽、路径/消息/目录/文件总数有界，每帧最多提交 4 文件。
- [x] 目录创建、本地上传检查及临时副本准备移到后台；拒绝树内链接和越界路径，保留失败暂存。
- [x] 实际 Rust 请求计量、暖绘制零新增分配、真实认证成本、默认构建启动/空闲对照和同一父进程 30 分钟启停长测。
- [x] 正式 README、配置示例、v0.1.0 发布说明、版本与默认功能一致。
- [x] 三平台包包含主程序/helper、文档、字体许可、配置和 shell 脚本；macOS 主程序/helper 均为通用二进制。
- [x] 最终候选 CI/原生门槛、main CI 和稳定标签 CI 全部通过；源码、main、正式标签及非预发布 Release 已推送。
- [x] 下载正式 Release 的 6 个附件，验证 SHA256、来源提交、版本、架构及必要内容；macOS 包签名/实际版本通过。

## 实现与成本

SFTP 界面、v3 协议、队列和任务管理在程序内实现；系统 OpenSSH 提供连接、加密及认证，helper 按需启动。Windows 安全和文件适配使用系统 API，未增加常驻认证线程或异步运行时依赖。默认 Unix Rust 运行时依赖仅 libc；Windows 无额外 Rust 运行依赖。

Windows 完整最大消息需要 16448 B 请求缓冲，最多 3 个管道实例，端点预留 128 KiB。默认 SFTP 子池仍为 16 MiB；资源账本不等于整个进程 RSS，系统库、内核、实际线程栈和外部 SSH/编辑器成本另计。

30 分钟完成 2539 轮认证、目录和关闭，每轮额度归零。五组默认/精简构建对照中，就绪中位数增量约 0.25 ms，空闲 RSS 增量约 624 KiB；保留原始范围、源码与仪器限制，不外推为全部平台结论。

## 验证范围和记录

最终 [候选 CI](https://github.com/YoisakiKnd/congminal/actions/runs/41017524703)、[原生门槛](https://github.com/YoisakiKnd/congminal/actions/runs/41017524711)、[main CI](https://github.com/YoisakiKnd/congminal/actions/runs/41018010460) 和 [标签发布](https://github.com/YoisakiKnd/congminal/actions/runs/41018009760) 全部通过。原始数据、失败日志和下载附件核验见 [发布验收记录](evaluations/v0.1.0/README.md)。历史冻结审计不改写哈希。

测试使用生成的临时密钥、随机密码和隔离回环服务；未接触生产主机。Windows/Linux 人工桌面、全部输入法、原生输入 P95、OS FD 计数和其他平台长测仍不在本轮证明范围；不能用编译、单元测试或 Rust 额度归零替代这些结论。macOS 正式包尚无开发者公证，Linux 为 X11/XWayland；安装要求见 README。
