# M5 SSH 后端评估

当前优先推进 OpenSSH 进程适配，未新增 Cargo 依赖。系统候选是 OpenSSH 10.3p1 / LibreSSL 3.3.6；库候选为已安装的 libssh2 1.11.1，动态链接 Homebrew OpenSSL 3.6.4、系统 zlib。库原型直接使用 C API，不是 Rust ssh2 包集成或完整应用后端；约 36 KiB 的测试程序不代表动态库分发成本。库尺寸/摘要在原始数据中，OpenSSH 由系统提供。

## 相同算法的隔离原型

回环服务仅使用临时 Ed25519 主机/客户端密钥和独立已知主机记录。双方均验证主机密钥，固定 PTY、aes128-ctr、hmac-sha2-256、curve25519-sha256、ssh-ed25519，无压缩，显式测试密钥且禁用外部 agent。五组交替，READY 后空闲 0.5 秒，再下载 16 MiB 零数据，长度与 SHA256 一致。热 OS 缓存；就绪时间包含启动、认证及远端 shell/PTY，不是纯 TCP 或冷 OS 启动。

| 中位 | OpenSSH | libssh2 C 原型 |
|---|---:|---:|
| 启动到 READY | 50.480 ms | 49.139 ms |
| PTY 下载 | 113.990 MiB/s | 115.711 MiB/s |
| 空闲客户端树 RSS | 3.891 MiB | 5.344 MiB |
| 采样峰值 RSS | 3.969 MiB | 5.438 MiB |
| 最大客户端进程/线程 | 1 / 1 | 1 / 1 |

libproc 每 5 ms 采样，Mach CPU 计数按时间基准换算，客户端/服务端分别记录。短空闲窗口内 CPU 计数没有可观测增量，不构成长期 0.2 个百分点验收。RSS 含共享映射，不是 PSS，尚未包含完整 congminal。

统一条件后库就绪约早 1.3 ms、吞吐约高 1.5%，RSS 约多 1.5 MiB；不把这些差异推广到其他负载。首轮未统一 KEX/agent 条件，保留为 [初轮记录](m5-ssh-prototype-initial.json)，最终以 [统一条件记录](m5-ssh-prototype.json) 为准。应用不继承原型的固定算法，保留 OpenSSH 正常协商。

选择先适配 OpenSSH，主要因为能保留用户系统配置与终端认证交互。库原型尚未实现配置展开、交互认证 UI、取消及分配回调约束，不能从该原型推出完整库方案的性能或兼容结论。Include/Match/ProxyJump 由工具按 [官方配置手册](https://man.openbsd.org/ssh_config) 解释。库提供 [已知主机校验](https://libssh2.org/libssh2_knownhost_checkp.html) 和 [分配回调](https://libssh2.org/libssh2_session_init_ex.html)，继续接入时仍须量化其边界。

## 应用接入

实现与操作见 [执行记录](../execution-m5.md)。后台仅用固定的无网络参数检查能力；实际会话通过独立 PTY 启动，强制前台生命周期，保留主机校验。状态不解析提示符，CLI 下认证/活动状态明确显示未知；不把非零远端退出猜成认证或网络失败，依据 [官方退出语义](https://man.openbsd.org/ssh#EXIT_STATUS)。

已通过本机真实应用隔离用例：直接密钥、独立 agent、加密测试密钥口令、首次主机确认/拒绝、严格模式未知主机、密钥变化、错误密钥、拒绝连接和 ProxyJump。各用例检查原本地会话仍可输入、额度和字体映射归零，采样未观察到残留客户端子进程。不是三平台 GUI 或长期泄漏验收。

直接连接首轮约 524 ms，后四轮就绪中位约 108 ms；加密密钥约 228 ms，含测试输入及解密；跳板约 151 ms。首轮后台检查/创建阶段约 329 ms，不将其当 UI 阻塞或纯网络延迟。单 tick 包含同步 PTY 创建/替换，本批最大约 4.6 ms；完整输入到呈现仍待验证。回显约 6 ms 包含测试轮转和系统计时合并，不等于实际键盘延迟。

应用与客户端树采样 RSS：直接约 25.1 MiB，跳板约 29.2 MiB，含测试框架及软件 Renderer，未含真实窗口/合成器。求和可能重复计算共享映射；agent 服务另有采样。跳板增加一个客户端子进程。两会话（本地+SSH）混合额度约 20.7 MiB，重连旧新重叠峰值约 30.1 MiB。五轮微小增长包含新数字字形缓存，不能据此推断长期资源稳定。

关闭功能的五组对照：VT 131.8→129.9 MiB/s，整帧 2.998→2.967 ms，四窗格 608→601 µs，工作区输入 P95 250→208 ns、P99 2875→1875 ns。十组独立帧 2.986→2.992 ms，单行 0.304→0.313 ms；未见超过 5% 的稳定回退，不承诺全场景零成本。Debug/release 各 317 通过、16 忽略；三平台 all-target Clippy `-D warnings`。

## 未完成验收

密码/二次认证、三平台真机、30 分钟、完整句柄及 GUI 延迟继续验收。

隔离 TCP 转发器强制切断已有连接，应用观察退出后显式重连；新会话编号替换旧会话，原本地会话保持可输入。已通过 [断链与手动恢复](m5-ssh-interrupt.json)：额度、映射归零，采样未观察到残留客户端子进程。本用例客户端树 RSS 峰值约 21.5 MiB，不代表长期峰值。早期脚本未能正确识别带前缀的控制消息，保留 [早期失败](m5-ssh-interrupt-initial.json)；审批额度恢复后按正常流程完成复测。

证据：[直接](m5-ssh-native.json)、[agent](m5-ssh-agent.json)、[密钥口令](m5-ssh-passphrase.json)、[首次确认](m5-ssh-ask.json)、[拒绝确认](m5-ssh-decline.json)、[未知主机](m5-ssh-unknown.json)、[密钥变化](m5-ssh-changed.json)、[错误密钥](m5-ssh-auth-failure.json)、[拒绝连接](m5-ssh-refused.json)、[跳板](m5-ssh-jump.json)、[通用基准](m5-ssh-bench.json)、[独立帧](m5-ssh-render-isolated.json)、[构建摘要](m5-ssh-build.json)。
