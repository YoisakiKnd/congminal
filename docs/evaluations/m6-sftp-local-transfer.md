# 本地传输拥有者、原子发布与上传源核对候选

状态：已实现macOS/Linux测试候选本地文件拥有者和上传重命名前检查，完成本机/两工具链、本地SFTP子系统及组件成本验证。后续[固定进度/提交报告候选](m6-sftp-transfer-report.md)已补；后续[Service传输DTO/共享结果](m6-sftp-file-service.md)已补，目标确认、进度界面/后台列表及工作区操作仍未完成；默认发布入口仍关闭。M4–M8和平台/长测范围不变，完整M6不勾选。

## 文件、路径和提交状态

[local_transfer](../../src/sftp/local_transfer.rs) 的LocalPath复用有额度的原名字节/转义显示，要求绝对路径、禁止NUL、空文件名、结尾斜线及`.`/`..`尾项。Path::file_name会规范化尾项，因此先检查原始尾字节，避免把目录样式目标误当成另一个文件。路径与实际工作使用同一预算，跨池拒绝；取消/额度检查先于文件IO。每个Source/Download固定32KiB Rust工作预留，路径另收费；文件内容只经调用者的固定块，不按文件总长度分配。

Source以NOFOLLOW/NONBLOCK和标准库CLOEXEC只读打开，拒绝目录、FIFO和末级符号链接。持有FD，记录设备/inode/长度及mtime/ctime纳秒；检查不比较atime，避免自己的读取产生误报。结束显式close，错误不能因后续成功关闭覆盖；close失败不按可能已复用的数字重试。元数据检查用于观察普通变化，不是不可变文件系统快照，也不能保证检查之后源文件不再变化。

Download在目标父目录内创建独立随机0700目录及0600的payload.part，持有父目录和暂存目录FD，用openat执行后续操作。macOS在尚无数据的暂存目录清除继承扩展ACL；权限适配失败就返回错误，不先写用户内容。文件/目录均设置CLOEXEC，创建失败也返回已知部分位置和清理确认。Drop只关闭仍持有的File，不遍历/删除磁盘路径；取消或失败保留可识别部分文件，Native对象不得放进UI输入/结果DTO。

写入使已校验状态失效。sync核对精确大小并刷新/同步文件；提交前再次比较文件版本、暂存名称对应的inode和可观察到的父目录替换。没有sync、之后再写、大小不符、路径被换或取消时不发布。固定临时名称和完整路径准确预留后构建，消除了早期路径构造的3次realloc。

默认linkat不覆盖发布，已有普通文件或符号链接都会拒绝；该模式需要目标文件系统支持硬链接，不支持时保留暂存并报错，不回退到有覆盖竞争的检查再重命名。ExplicitlyApproved模式才使用renameat原子替换目标项，不跟随目标符号链接去改它指向的文件；这个枚举只是底层契约，尚没有用户确认UI，不能当成已经取得许可。[linkat语义](https://man7.org/linux/man-pages/man2/link.2.html)、[renameat语义](https://man7.org/linux/man-pages/man2/rename.2.html)、[Apple ACL接口](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/acl_set_fd_np.3.html)。

发布后立即记录published；目录同步失败、暂存清理不完整或随后取消，都不能重新解释成“目标没有提交”。Commit分别返回published、parent_synced、staging_removed，结束仍须显式关闭。非sticky且可被其他用户改名的父目录不按可替换名称删除暂存目录，可能留下已清空的目录并明确返回staging_removed=false。不递归删除额外文件。父目录核对和私有目录减少替换风险，不承诺阻止同UID进程或并发命名空间在检查后继续变化；sync成功也不宣称跨所有设备/文件系统的断电持久化。

## 上传源核对

[upload_checked](../../src/sftp/transfer.rs) 在远端临时文件收到CLOSE ACK后、发送RENAME前调用源核对，再检查取消。失败或取消使通道失效，保留远端临时文件，不提交最终名称。既有upload仍供确定数据生成器调用，文件任务接入时必须选择checked版本、核对本地Source并处理其关闭状态，不能以旧包装器默认空检查替代验证。

确定性协议测试核对请求序列：INIT、OPEN、WRITE、FSTAT、CLOSE之后才运行核对，失败/取消都不出现RENAME。实际sftp-server互操作用32MiB文件（大于8MiB预算）上传并经私有本地暂存下载，逐字节核对后原子发布；另外在上传中修改同大小源并改变mtime，最终核对失败，原远端目标完整保留、临时文件可识别。它没有经过SSH、Service队列或Workspace，不代表端到端传输任务已经交付。

## 增量请求计量与全局锁余项

[工具](../../tools/check_local_transfer.py) 的allocation模式在临时副本注入全线程epoch请求计量；每次先跑原快捷键回归。计量前准备输入文件、Caller路径/预算/缓冲和应用已使用的进程创建全局锁，再计量Source、Download及其路径、复制、同步、发布、关闭和独立保留的部分路径。Caller使用64KiB栈缓冲，Root配额256KiB；计量头、allocator/C ACL/OS内部、实际栈/RSS/文件页缓存不在Rust请求字节中，不据此承诺整个进程/系统内存固定。

最初未初始化全局锁时，关闭文件后还有64B静态Mutex懒初始化存活，路径拥有者不能承担它：[原始失败](m6-sftp-local-allocation-initial.json)。独立标准库Mutex进程在当前及Rust1.88均复现一次64B存活，记录在最终报告cold_lock字段。这属于应用全局同步元数据，仍是M4完整准入的余项；只将文件组件测量条件改为应用已初始化，不把暖机当作修复或全局验收通过。初步暖机但尚有路径扩容的[中间记录](m6-sftp-local-allocation-after-launch-warmup.json)也保留。失败计量夹具增加RAII清理；先前已确认的自有失败夹具文件已核对内容后移除。

当前/Rust1.88在4MiB、32MiB两负载分别都是5次新增Rust分配、0次realloc。4MiB请求峰值472 B、存活348 B，32MiB峰值475 B、存活350 B；3 B差来自夹具路径长度，不是按文件长度扩张。两文件拥有者及Caller路径账本峰值约66.85KiB；复制一半、读足、同步和提交不增加分配。Source/Download关闭后仍保留路径数据，最后路径释放时请求和额度均归零。[当前报告](m6-sftp-local-allocation.json)、[Rust1.88报告](m6-sftp-local-allocation-msrv.json)。这不是整个传输栈只有475B：两个32KiB工作模型、Caller块、预先创建的路径、网络协议/线程及C/OS数据各在相应边界。

## 普通成功发布成本

生产System分配器另测相同32MiB内容、64KiB顺序写、私有目录/0600文件、文件同步、link不覆盖发布、暂存清理、父目录同步和关闭；每模式2次预热、5组交替。标准库普通对照缺少FD锚定、替换核对和明确关闭契约，只是正常成功路径成本对照，不是故障语义等价或可默认采用的替代后端。[所有原始样本](m6-sftp-local-cost.json)。

五组配对总耗时差的中位为+2.61%，范围-8.09%至+33.32%；不能声称所有轮次均在5%内。分别取中位时，普通对照102.16ms，包装100.58ms，后者单次最高139.52ms；磁盘同步波动占主要部分，不能解释成包装更快。创建中位约178/308µs，明确关闭约21/40µs；这些额外核对留在后台，未测GUI输入到呈现。采样RSS中位普通约8.125MiB、包装约8.188MiB，均2个Rust测试框架线程/1进程；不是完整RSS峰值、长期空闲或SFTP吞吐验证。没有新增Cargo包或线程，未提高预算。

## 回归和下一步

新增8项常规测试（本地拥有者7、协议顺序1），另有组件成本及本地子系统2项显式运行测试。默认debug/release各466通过/32忽略，可选原生各475通过/34忽略；本机默认/原生和Linux/Windows默认all-target Clippy、Rust1.88原生release编译、默认/helper生产构建通过。Rust1.88实际本地7项及协议1项通过；两工具链32MiB子系统均通过，受管峰值174278 B、结束零。共15项最终检查。[构建/互操作报告](m6-sftp-local-build.json)。Linux仅静态检查，ARM64标志按缓存libc源核对但未实际构建/运行；Windows尚无此本地拥有者适配。原生故障注入、磁盘满/其他文件系统与各平台仍待补。

后续已补[固定报告、不可逆提交/取消和ACK不确定状态候选](m6-sftp-transfer-report.md)，并移除执行器统一取消覆盖。后续[Service统一Action/Output](m6-sftp-file-service.md)已补，下一步落实IO余量/成本和Workspace共享任务记录。目录刷新也不能随意废弃后台传输的连接归属，隐藏只取消目录、后台文件任务继续按其原配置/会话绑定管理。之后完成目标/覆盖确认、后台列表/进度速度/重试通知、失败退出交互、完整性能及平台验收，完整M6仍未完成。
