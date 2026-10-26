# M6：SFTP 后端、协议与内存评估

当前结论：后端实现采用 libssh2 加受限分配、固定目录缓冲与非阻塞取消的薄适配方向；自写 v3 协议原型保留作对照和可能的 OpenSSH 兼容后端。受限回调及原生故障清理已通过隔离验证，默认发布仍待完整应用、配置兼容和三平台验收。项目已加入默认关闭的可选依赖，运行时界面尚未接入 SFTP，不能将原型通过视为 M6 完成。

## 同负载后端比较

macOS 隔离回环 SSH，临时 Ed25519 密钥、严格固定主机记录、无 agent，固定相同 KEX/主机算法/AES-CTR/HMAC、关闭压缩。五组交替运行：认证并建立 SFTP 后空闲 500 ms，列出 512 个中文文件名；生成 16 MiB 二进制数据，排他临时上传、fstat/fsync/关闭确认/v3 不覆盖重命名；再逐字节验证下载、大小与 EOF，删除本次明确创建的文件。每 5 ms 采样客户端树与服务端树，分别记录。方法、原型源码、哈希、事件与时间序列均在原始 JSON 中。

| 中位数 | OpenSSH 子系统＋本项目适配 | ssh2 0.9.6 Rust 原型 |
|---|---:|---:|
| 认证及 SFTP 就绪 | 43.80 ms | 37.70 ms |
| 512 项目录 | 131.04 ms | 137.32 ms |
| 上传（含提交） | 185.94 MiB/s | 224.26 MiB/s |
| 下载（含内容验证） | 192.28 MiB/s | 363.92 MiB/s |
| 客户端树峰值 RSS | 6.39 MiB | 5.95 MiB |
| 就绪后空闲 RSS | 5.81 MiB | 5.41 MiB |
| 客户端进程/线程最大数 | 2 / 2 | 1 / 1 |

两者都是独立原型，RSS 含原型主进程，OpenSSH 列另含客户端子进程；求和可能重复计算共享映射，未含真实 GUI。短暂空闲期均未采到 CPU 增长，不能证明长测空闲成本为零。目录适配层工作不同：本项目还生成转义标签、保留目录模型；Rust 包原型保留路径及属性，不以这组目录耗时隔离比较库自身成本。所有已观察到的客户端子进程均已退出。

Rust 包实际链接 vendored libssh2 `1.11.1_DEV`、Homebrew OpenSSL 4.0.3 和系统 zlib；设置过 pkg-config 开关，但构建机缺少该工具，已核对实际构建日志和 `otool`，不能误记为系统 libssh2。另一个 C 原型使用系统 libssh2 1.11.1 / OpenSSL 3.6.4，五组中位上传/下载约 225.22 / 359.16 MiB/s。两组均显示成熟实现值得继续评估，但不能将 C/Rust 差异全部归因于包装开销。

证据：[Rust 包五组](m6-sftp-ssh2.json)、[C 库五组](m6-sftp-backends.json)、[包构建与依赖图](m6-sftp-ssh2-build.json)。

## 依赖与准入约束

`ssh2 0.9.6` 默认功能关闭的独立临时项目锁定了 19 个依赖包（包含其他目标及构建工具）；本机依赖图、完整锁文件与构建输出已保留。Rust 1.88 的独立目标目录 release 构建通过，用时 5.50 秒，注册表缓存已热；这不是完整应用的冷构建时间。动态加密库、静态打包以及功能关闭时的启动/体积影响仍需在应用中测量，不能以独立原型文件大小推断发布体积。

官方 [ssh2 文档](https://docs.rs/ssh2/0.9.6/ssh2/) 和 [libssh2 SFTP API](https://libssh2.org/libssh2_sftp_init.html) 提供成熟的协议与认证实现。当前还存在以下工程边界，不能只因吞吐较高便接入：

- `Session::new` 使用默认 libssh2 分配回调，0.9.6 没有暴露自定义分配器构造接口；内部 C 分配与加密库分配尚未接入共享额度。
- `Sftp::readdir` 返回整个 Vec；`File::readdir` 每次从 4 KiB 缓冲开始，并在 BUFFER_TOO_SMALL 时扩容。必须使用可限制的分页适配并验证分配前准入。
- 已下载的 vendored `sftp.c` 对一般包设 256 KiB 限制，但匹配的 READDIR NAME 回复有例外。结果返回后再截断 512 项不能限制接收峰值；需要分配回调/受限后端或其他适配措施。
- 非阻塞及超时 API 可用，仍需验证取消、断链、清理、配置别名/跳板和主机记录语义；不重写 SSH 加密算法。

## libssh2 共享额度适配与故障验证

项目新增 `src/sftp/allocation.rs`：稳定地址的单线程池，为 C API 提供 alloc/free/realloc 回调；单块最多 1 MiB，每会话测试上限 8 MiB，实际分配前取得共享 Budget 额度。16 字节对齐的头部持有 Lease，先释放实际块再归还额度；扩容先准入旧、新重叠，失败保留旧块及内容。池元数据在 Box 销毁后归还，回调禁止 Rust unwind 跨越 C ABI。调用方必须在池销毁前释放所有 C 对象，不能跨线程并发使用。

通过官方 [自定义分配接口](https://libssh2.org/libssh2_session_init_ex.html) 接入真实系统 libssh2 1.11.1 / OpenSSL 3.6.4，五组交替复核结果如下。加密库内部独立分配仍不经过该回调，账目不等于全部进程内存。

| 中位数 | OpenSSH＋本项目协议 | libssh2＋真实 Rust 共享额度回调 |
|---|---:|---:|
| 上传 / 下载 | 184.91 / 192.79 MiB/s | 225.92 / 359.85 MiB/s |
| 客户端树峰值 RSS | 6.62 MiB | 5.84 MiB |
| 就绪后空闲 RSS | 5.81 MiB | 5.52 MiB |
| 就绪时间 | 41.95 ms | 35.31 ms |

五轮回调分配峰值 163297–185499 字节，各约 6900 次分配/释放，均完全平衡，结束后共享账目归零。RSS 包含原型而非完整 GUI，不能从不同构建和库版本的原型隔离推断每项开销。早期多链接了未使用的系统框架，已移除并重测，保留早期记录供核对。[最终五组](m6-sftp-native-pool.json)、[早期额外框架记录](m6-sftp-native-pool-extra-frameworks.json)。

隔离 SSH 子系统提供四类故障，各 5 次：声明 1 GiB NAME 包在分配前拒绝；包内伪造 1 GiB 名称长度在固定 1024 字节目录缓冲处终止；不响应时以约 200 ms 测试期限触发非阻塞取消，shutdown 后清理；服务端关闭时检查通道 EOF，避免把 EAGAIN 一直误当作继续等待。句柄必须显式关闭，之后关闭 SFTP 和会话，不能假设 SFTP shutdown 会代收全部句柄。20 次测试 C 分配与释放平衡、共享账目归零。此处是原生 API 可行性验证，尚非工作区取消按钮的完整验收。

证据：[故障与清理](m6-sftp-native-faults.json)、[补 EOF 前的记录](m6-sftp-native-faults-before-eof.json)、[构建与回归](m6-sftp-native-pool-build.json)。最新标准 debug/release 各 337 通过、20 忽略，三目标 Clippy 无告警。库会话所有权、主机记录验证和密钥认证现已通过 Rust 接入验收；下一步完成配置兼容、可取消连接和业务接口，再接远端界面与任务队列；不直接采用会自动扩容的高层目录接口。

## 自写协议原型的边界与成本

限定 SFTP v3；以独立大端字节向量和真实 OpenSSH 子系统核对，而非自编解码互相证明。包体最多 1 MiB，单次数据 64 KiB，路径 16 KiB，句柄 256 字节，最多 32 个在途请求；NAME 项数、扩展项数、字段、属性和尾部均严格检查。未知/重复 ID、类型错误、超限数据和错版本关闭协议状态。回复借用持有的包，不复制整文件。

分配器探针记录的实际 Rust 请求峰值：64 KiB DATA 帧 65545 字节、1 MiB 包 1048576 字节、64 KiB WRITE 加最大句柄 65817 字节；账目覆盖这些峰值，释放后实际请求和账目均归零。输入数据、线程栈、系统库与进程不计入此探针。

五轮独立 release 中位：64 KiB/16 KiB 分片解码约 976 ns，逐字节分片约 271252 ns；1 MiB/16 KiB 分片约 14578 ns；64 KiB WRITE 编码约 675 ns。逐字节碎片存在明显 CPU 成本；这些数字不能当作网络吞吐。[协议分配与计时原始记录](m6-sftp-wire-workload.json)。

目录模型单工作集先预留 3 MiB，512 项展示、8192 项扫描上限，原名字节与显示转义分离，带会话编号和结果代次。通道自身缓冲、部分包、结果包和流式上传缓冲均拥有额度。32 MiB 流式操作测试在 8 MiB 额度下峰值 140514 字节，结束归零；另一个仅协议包测试峰值 65565 字节。它们不包含外部 SSH/服务端或全部进程堆。

标准 debug/release 各 334 通过、20 忽略；四项本地子系统手动验收通过；三目标 Clippy 无告警。见 [构建与子系统日志](m6-sftp-build.json)。本阶段只编译进测试，目录界面、真实通道生命周期、完整传输任务与三平台/弱网/磁盘满/长测继续待办；操作和复验方式见 [执行记录](../execution-m6.md)。


## 项目原生会话与依赖状态

已通过 `libssh2-sys` 薄绑定把上述回调、非阻塞等待和会话/句柄所有权接入 Rust 模块；可选 `sftp-libssh2` 仍仅用于测试模块。直接使用 sys 绑定锁定 9 个包，避免高层目录 API 的自动扩容；默认功能不编译这些依赖，首次解析/下载及启用后的 C 构建成本仍存在。构建/锁文件、默认及原生依赖图见 [接入记录](m6-sftp-native-session-build.json)。

macOS 真实 Rust 会话的 13 场景通过，补有严格主机校验顺序、哈希记录、内存加密密钥、错误口令分类、旧/跨连接句柄与文件/目录类型约束。主机记录行以有界 NUL 缓冲传入 C API；早期未加终止字节使有效记录被截断误判，已复测修复，保留 [早期记录](m6-sftp-native-session-positive-initial.json)。密钥副本按大小预留且认证后清零，C 加密库独立内存仍不计入共享账目。

32 MiB 文件在 8 MiB 测试额度下传输、逐字节核对和不覆盖提交；共享峰值包括 4 MiB 的隔离测试输入预留，不代表应用开销。测试打印的单轮传输时间仅为验收诊断，不替代前面的五组同负载比较。正向配额检查通过继续 READDIR 验证原连接，没有假设服务端支持目录句柄 FSTAT。记录：[全部场景](m6-sftp-native-session.json)、[最终正向](m6-sftp-native-session-positive-final.json)。

最新默认 debug/release 各 337 通过、20 忽略，原生功能各 341 通过、21 忽略；主机默认/原生 Clippy、两 GNU 目标默认 Clippy 及 1.88 编译检查通过。尚未验证原生功能的 Windows/Linux 构建和运行、库内部在途数、共享 16 MiB 调度、DNS/系统配置兼容、GUI 及长测；以上是默认发布前的待办。详见 [执行步骤](../execution-m6.md)。


## 地址解析与连接成本

网络建立增加同程序 DNS 辅助模式、16 项/60 秒地址缓存及非阻塞双栈 TCP 候选；未增加新的锁文件包。`libc` 作为已有锁定包的可选直接依赖，只随原生开关启用。数值 IP 不启动辅助进程，空闲缓存没有定时线程或轮询。

[五轮解析与缓存记录](m6-sftp-connect-cache.json) 中，直接系统 localhost 解析为 0.20–1.29 ms；辅助进程首次约 371 ms，其后约 7.76–15.66 ms；新构建程序的首次运行因素尚未分离。1000 次已热缓存平均 103 ns，数值地址约 0.79–2.29 µs。这些是单机微测量，包含父任务约 5 ms 间隔的进程完成检查；不代表外部 DNS 延迟或 GUI 输入延迟，也不把首次样本删掉。缓存有效期内不重复解析，用户发起新连接时仍必须重新验证主机密钥。

该轮 localhost 场景客户端进程树观测峰值 14188544 字节；5 ms 采样仅捕获到一个解析辅助进程样本，其 RSS 为 5505024 字节，不能当成准确峰值。各次解析工作预留峰值 256 KiB，结束为零；缓存另预留 32 KiB，销毁后归还。辅助进程、系统解析和加密库的内部内存仍不是完整共享堆上限。

真实 14 场景、双栈数值连接、拒绝回退及描述符标志通过；系统配置结果模型也已用实际 OpenSSH Include/Match/ProxyJump 输出核对。后者目前仅处理有界结果，尚未交付生产配置查询进程监督、认证桥接和选项应用。详见 [执行记录](../execution-m6.md) 与 [配置核对](m6-sftp-settings.json)。Windows/Linux 原生构建与真机、完整地址族调度、队列/界面和长测仍待办。


## 配置查询监督的开销与覆盖

已用真正的 Unix OpenSSH -G 查询核对前述结果模型；输出限制 128 KiB、命令参数和工作额度先准入，超时/取消结束受控进程组，保留主进程身份直至组清理完成。受控 Match exec 等待用例约 57 ms 后取消，采样未见子进程残留；自行脱离组或改变身份的程序仍不在完整树管理保证内。Windows 捕获明确不支持，尚未接入默认应用。

五轮交替、同一受控 Include/Match/ProxyJump 配置并构建同一模型：直接调用中位 5.43 ms，监督调用 5.87 ms。初始固定等待使查询约增加 5 ms，已改为管道就绪唤醒与有限的退出尾声等待；结果有波动，不把 0.44 ms 差值解释成所有负载的恒定成本。没有新增包或常驻线程。测试受管峰值 456532 字节、结束零；其进程树观测峰值约 14.23 MiB 包含测试程序和派生程序，5 ms 采样可能漏掉短进程，不是 GUI 或全堆预算证明。

[最终五轮与取消记录](m6-sftp-query-supervisor.json)、[初始等待](m6-sftp-query-supervisor-initial.json)、[中间记录](m6-sftp-query-supervisor-poll-initial.json)。最终测量后补了提前关闭 stdout 的超时断言，并让已清理的进程/命令守卫按声明逆序在返回时析构以通过 Windows 类型检查；已在 [回归构建](m6-sftp-query-build.json) 覆盖；最终记录的范围描述已修正，原始测量和散列保持原样。


## Windows Job 适配与当前成本证据

Windows 捕获已由占位改为原生 Job/管道实现，仍仅编译进测试。创建时采用 [Job 与句柄白名单属性](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute)，并使用 [Job 关闭时结束成员](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)；实际任务清理还检查 ActiveProcesses。没有添加包或外部运行工具，相关调用属于系统 Kernel32。PE 测试产物导入表只有 Windows 系统库；不能据此推断调用时延或 RSS 为零。

Windows x64 SDK 静态 ABI 核对、Rust 编译和完整 PE 链接通过。[验收包清单](m6-sftp-windows-capture-bundle.json) 仍将 native_runtime_verified 设为 false。debug 测试程序约 31.67 MiB、C 测试程序约 149 KiB，属于验收工具，不能推断发布程序体积。没有目标机运行结果前不启用普通应用 SFTP，也不勾选跨平台验收。运行方法和覆盖见 [执行记录](../execution-m6.md)。

统一参数规格后真实 POSIX 查询与取消复测通过，受管预留峰值 456532 字节、结束归零，进程树观测峰值约 13.95 MiB。五轮交替中位直接 12.86 ms、监督 9.31 ms，当前机器波动明显，不将差值解释成监督带来加速。Windows 新增 Job 创建、属性设置、管道检查及清理查询的开销/RSS 仍待真机测量；固定工作预留不代表已核对全部 std 或系统堆。[复测](m6-sftp-windows-spec-query.json)、[当前回归](m6-sftp-windows-capture-build.json)。


## 有界认证输入与配置核心字段

新增文件/路径薄适配未增加包。路径最多 16 KiB，64 KiB 工作预留只覆盖展开与扩容峰值，返回按实际容量收费；文件工作预留 32 KiB，数据按 stat 大小＋1 哨兵及元数据收费，单文件最多 1 MiB。私钥拥有者释放前清零；Unix 私钥权限、非普通文件、读取期间短读/增长/取消均有测试。32 MiB 传输远大于 8 MiB 测试预算，字段链路两例共享峰值约 7.12 MiB，结束零；包括夹具保守预留，不等于实际堆/RSS。同步磁盘/网络文件系统 I/O 和完整进程内存还没有硬实时/全堆保证。

16 场景验证配置查询字段、路径 token、字面 HostKeyAlias none、文件输入与原生 SFTP；另用 OpenSSH 对同一生成密钥和路径完成独立 VERSION 握手。这个测试验证核心字段，不证明代理、全部算法策略、多身份/认证或全部系统配置已经兼容。-G 主机记录路径列表的引号丢失已实测，返回 AmbiguousPaths 交兼容路线，不能猜测忽略文件。Windows ACL 和原生平台验证继续待办，模块仍不在普通应用路径。

[完整互操作](m6-sftp-auth-inputs.json)、[明确路径对照](m6-sftp-auth-path-oracle.json)、[当前构建](m6-sftp-auth-input-build.json)。互操作后仅改了 SSH 能力检查结果优先级与测试诊断；SFTP 文件/路径/主机验证逻辑未再改变。单轮打印的上传/下载时间只作功能诊断，本轮没有完整应用输入延迟/RSS或公平吞吐新结论。

## OpenSSH 有界流适配

新增薄适配复用系统 OpenSSH 和自有 SFTP v3 协议，没有新增 Cargo 包/常驻写线程。每条流预留 256 KiB；Unix 直接非阻塞读写，Windows 一个异步写槽最多 64 KiB，按结构和元数据另行收费。Windows 增加系统 bcrypt 随机数调用及命名管道/事件；[异步管道](https://learn.microsoft.com/en-us/windows/win32/ipc/synchronous-and-overlapped-input-and-output)和取消后的完成观察均有所有权约束，未完成时不释放对象或退额度。Windows 真机成本尚待测，不能用系统 DLL 或交叉构建推断开销为零。

默认依赖测试程序完成 15 个实际 OpenSSH/SFTP 场景，包含独立生成的 agent、带空格多记录文件、Include/Match、token、哈希/别名及故障清理。正向 32 MiB 双向内容核对的受管峰值 3418157 字节，8 MiB 测试预算，结束零。进程树 5 ms 采样观测峰值最高 15237120 字节，包含测试程序与 OpenSSH，可能遗漏短时峰值；不包含外置测试 agent/服务端，也不是完整 GUI/RSS 上限。功能运行未安排五轮平衡对照，不据此更新库/CLI 吞吐结论；新的管道等待及 Windows 轮询开销仍须进入 UI 前单独测量。

[15 类互操作](m6-sftp-openssh-stream.json)、[初期退出回收失败](m6-sftp-openssh-stream-initial.json)、[九项构建/回归](m6-sftp-stream-build.json)、[Windows 新验收包](m6-sftp-stream-windows-bundle.json)。模块仍只在测试中使用，BatchMode 不等于完整交互认证；目录界面和共享传输调度未交付。

原生后端的 [16 类回归](m6-sftp-stream-native-regression.json) 同样通过。互操作之后仅调整 Windows 写槽的受管 Box 析构顺序，实际块释放后再归还额度；macOS 不编译此文件，测量散列与最终构建散列的区别明确保留。Windows 仍仅完成编译/链接，不能将这些 POSIX 数据解释成其运行结果。
