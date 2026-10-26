# 独立认证 helper 与私有 Unix 端点

本轮交付按需 `congminal-ssh-helper`、系统随机能力、40 字节私有票据及 Unix socket 拥有者。默认 `cargo build` 不生成 helper，应用也尚未调用认证桥接；SFTP 后端仍仅用于测试。无新增 Cargo 依赖，生产路径未增加常驻线程、文件扫描或进程。功能开关是 `ssh-auth-helper`，通过专用入口复用状态类型、秘密缓冲、帧和额度源码。

## 边界与所有权

能力由 `/dev/urandom` 或 Windows `BCryptGenRandom` 产生，失败即拒绝，没有自制随机或弱回退。Unix 使用独立随机名字的 0700 临时目录、0600 票据和目录内 socket；能力不出现在 argv、SSH 环境或目录名中。环境只传票据路径，避免用户 `SendEnv *` 把能力转交远端。父目录不能是非 sticky 的组/其他用户可写目录。票据读取拒绝最终目录/文件符号链接、错误拥有者/权限、特殊文件、长度/版本/保留位异常；文件 nonblock 打开，FIFO 不会阻塞读取。权限检查不是对同一用户恶意进程的隔离承诺。

`OwnedTicket` 创建前预留 32 KiB 工作额度，完成后按三个路径容量加 1024 B 收费；明确 `finish` 报告清理错误，Drop 兜底只删除自身文件/socket/目录，不递归清扫。它必须由后台拥有者使用，尚未接窗口退出的工作器确认。路径目前保守受 Darwin socket 长度约束，过长返回错误。

helper 的池为 128 KiB，其中 64 KiB 预留给参数及工作；最多 64 参数、总计 16 KiB，提示最多 16 KiB，答案最多 1023 字节。进程启动时标准库内部参数分配、系统随机及文件/内核资源没有成为完整堆或 RSS 证明。Unix 启动器用 exec 替换自身，只给目标 SSH 设置 askpass 环境；单参数即使为 `--launch` 也仍是提示。IPC 发送采用绝对 5 秒期限，答案采用绝对 120 秒期限，不随每个分片续期；本机 connect 和文件系统调用本身尚没有硬中断保证。

答案通过非缓冲 File 输出，不追加换行，不经过 stdout 的 LineWriter；256 字节接收暂存每段立即 volatile 清零，错误/退出的 Drop 也清理。Secret 释放前清零，取消独立于空答案。Windows 私有 DACL/命名管道服务端身份尚未交付，当前票据读取返回 Unsupported，不能把静态编译通过当成 Windows 认证可用。

## 功能验证及缺陷修复

使用自动生成的私钥、自建回环 sshd 和私有 IPC 夹具；不读取用户 SSH 文件、agent 或凭据。9 个直接用例通过：1023 字节整帧立即关闭、逐字节回复、空答案、取消、错误版本、断开、无效 UTF-8、confirm、none。6 个真实 OpenSSH 场景通过：正常及最大加密私钥口令、错误口令、取消、首次主机接受及拒绝；先收到 SFTP v3 VERSION 再关闭输入。未知主机例检查提示含指纹，只有显式接受才写入隔离 known_hosts。

早期发现 macOS 在对端关闭后，即使还有排队数据，重复 `set_read_timeout` 也可能失败。1023 字节答案整帧失败而逐字节成功；独立 IPC 用例和 `ssh-keygen -y` 的密钥核对排除了口令生成问题。先验证只设置一次超时的修复，再改成非阻塞 poll 加绝对截止时间；最终整帧/分片及真实 SSH 均通过。保留 [首次失败](m6-sftp-auth-helper-initial.json)、[整帧定位](m6-sftp-auth-helper-whole-frame.json)、[单次超时实验](m6-sftp-auth-helper-timeout-check.json)，不将早期失败删除。

[最终互操作记录](m6-sftp-auth-helper.json) 包含源码散列、退出码和进程树采样，不保存完整提示/能力/答案。其后仅在票据测试内加入不安全父目录的拒绝/退额度断言；实际 helper 实现未改。三项票据测试覆盖权限、符号链接、FIFO、长度/版本、取消、额度拒绝和显式/Drop 清理。完整回归另见 [构建记录](m6-sftp-auth-helper-build.json)。

## 性能结论及剩余工作

本机 release helper 文件 549032 B（含当前 release 调试信息）。功能夹具故意延迟回复 30 ms，以观察短命进程；采样看到 SSH 与临时 askpass helper 同时存在，Unix launcher 没有额外常驻一层。这里的 CPU/RSS 不是公平成本对照，不能沿用之前 BatchMode 的零额外进程结论，也不能承诺默认接入后没有性能影响。

下一步接父工作器的有界 IPC 服务、现有认证状态和独立流回调，再接可见界面与取消/退出确认。默认采用前须补五轮同负载认证成本、等待 CPU/RSS/进程/线程及 Windows 启动器成本；仍需真实密码/多因素、Windows/Linux 原生运行、GUI 输入/IME、远端目录和共享任务队列。M6 完整交付项继续不勾选。
