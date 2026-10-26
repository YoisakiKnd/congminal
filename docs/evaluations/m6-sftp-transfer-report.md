# 固定传输报告与不可逆提交语义候选

状态：已实现并验证受资助的固定进度/结果槽、上传RENAME实际write边界及ACK观察，修正通用执行器的取消覆盖。后续已接[Service统一上传/下载Action](m6-sftp-file-service.md)；仍是测试候选，Workspace任务列表、目标确认或默认发布入口尚未接入。完整M6、M4–M8及真机/长测不勾选。没有新增Cargo包、每任务线程或可扩容事件队列。

## 报告、预算和退出边界

[transfer_state](../../src/sftp/transfer_state.rs) 的Status是只读共享拥有者，Progress是唯一写入凭据，可直接放进未来任务DTO。固定槽保存Source会话/代次、方向、长度/已传字节、阶段、发布事实、停止原因及清理标记。原始源/目标路径与最多一个临时位置复用有额度的RemotePath，保留原名字节和转义显示；跨池输入拒绝，共享槽先取得额度再创建Arc。它不打开文件、不持有连接/FD，也不替代LocalPath、Profiles绑定或目标覆盖授权检查。

advance只允许流式阶段的单调字节数，不能超过声明长度；verify要求长度完整。状态转移、进度更新和Snapshot读取只在锁内复制固定字段，不格式化名字、不逐块分配消息。没有新增通知/定时器，速度计算与界面读取节流留待接线。多个回调可借用同一Progress，写入凭据不提供Clone；底层锁保护状态。

begin必须在创建执行资源前调用。尚未开始的任务凭据被丢弃记为NotStarted/Confirmed；开始后没有显式finish的析构记为Interrupted/Unknown。这是适配器使用契约，不是类型系统对任意调用者的资源证明。同步状态poison也保持Unknown，已确认或可能发生的发布事实继续保留。读者/部分路径可以跨执行器关闭存活，最后拥有者释放才归还父预留。

End::Completed表示已记录发布事实，**不代表清理、目录同步或跨设备持久化全部成功**。清理失败、后续取消或异常分别保留在cleanup/failure。读取方必须优先展示publication，再说明清理问题；不能只看一个Completed/Cancelled标签。临时路径是定位候选，不保证文件仍存在：重命名后可能已经消失，失败也不能触发自动递归删除或自动覆盖重试。

## 上传提交观察

[client::issue_observed](../../src/sftp/client.rs) 先进行关闭、取消、ID、编码及额度检查，直到第一次底层write调用前才通知一次。碎片write、Interrupted重试或flush不会再次通知；即使write返回零/错误，也保守记录可能产生副作用。通知仅表示开始尝试，不能当成完整请求已发送。

[transfer::upload_observed](../../src/sftp/transfer.rs) 仍按写块ACK、FSTAT/可选fsync、CLOSE ACK、源核对和最后取消检查的顺序执行。只有RENAME首个write前记录Attempted；随后匹配ID的完整STATUS=0记录Acknowledged，非零STATUS保留Rejected(code)。EOF、截断、错误ID、取消、写入/flush错误都不能伪造确认，因此保持Indeterminate。Rejected只表示对端明确返回失败码，不证明任意服务器没有部分副作用；不能据此自动删除/重试或假定原目标未动。

固定报告在Attempted后变为Indeterminate，完整成功ACK后变为RemoteAcknowledged。停止发生在Attempted之前可标记Cancelled；处于Indeterminate时终态为Uncertain；成功ACK后收到取消仍为Completed，并可另记取消原因。本地发布另存parent_synced/staging_removed，收到本地Commit后应立即记录，再执行任何后续清理。它没有增加覆盖许可，也不承诺磁盘持久化。

[executor](../../src/sftp/executor.rs) 不再在work返回后因取消标记覆盖结果。开始前的取消仍阻止工作；开始后的停止含义由适配器解释。只读目录适配器在确认清理后自行归一化取消，清理失败保持失败。关闭或清理异常仍可能丢弃普通Completion，独立Status负责保存不可逆事实；不能仅靠给Outcome增加一个字段来覆盖该生命周期。

## 功能与真实子系统证据

新增6项常规测试：固定状态/额度/原名字节/非法转移与保留，取消及清理失败，poison/放弃凭据，实际执行器取消/关闭/清理失败/两个panic位置，12类上传边界，以及首个IO通知的准入规则。实际执行器证明成功后取消仍交付成功；关闭、清理失败或panic丢弃普通结果后，Status仍保存已发布或不确定事实。

32MiB本地sftp-server夹具接入实际上传观察、本地私有下载发布和源变化报告。成功上传为Acknowledged、下载为Published；源在传输中同大小变化阻止RENAME，原目标保留。三次操作共用子系统，等明确stop/wait/join后才记录清理确认，随后释放报告；不是借Drop证明进程已退出。保留报告时额度2754 B，最后归零；协议/文件工作峰值177032 B，包池上限8388608 B。两工具链均通过。该夹具无SSH/网络、Service传输DTO、Workspace操作或真实窗口。

默认debug/release各472通过、33忽略；可选原生各481通过、35忽略。本机默认/原生及Linux/Windows默认all-target Clippy、Rust1.88可选原生release编译、默认/helper生产构建均通过。Rust1.88实际运行6项传输/报告、1项首IO观察、10项执行器常规测试；两工具链各5项本地子系统互操作通过。共16项检查，原始命令、日志和当时的完整源码SHA保存在[构建报告](m6-sftp-report-build.json)。交叉静态检查不等于其他平台运行。

8类真实目录Service/helper/OpenSSH认证、主机接受/拒绝、取消、任务取消及等待回归均通过，确认适配器自行解释取消后仍可显式清理并join；只使用工具生成的临时密钥/回环sshd，不访问用户SSH文件。它仍只有目录请求，不构成上传/下载任务层或GUI验收。[Service回归报告](m6-sftp-report-service.json)。

## 全线程分配与最坏路径准入

[计量工具](../../tools/check_transfer_state.py) `--allocation` 临时复制源码并注入epoch分配器，区分跨线程释放/既有对象，保守计入realloc旧/新重叠。计量头、allocator/C/OS内部、实际栈/RSS不在此范围。根Budget、通知、16KiB原始输入和既有应用级线程全局初始化在计量前准备；本轮子池、报告/路径、新执行器/三线程/TLS均在计量中。

单报告使用三条16384字节无效UTF-8路径，显示各四倍转义。创建后Rust请求live/peak为246544 B，13次分配、0次扩容；1000001次更新、合并读取和发布/结束阶段不增加分配或扩容。丢弃写者/读者/子池句柄，只保留一条路径仍有82328 B请求，父预留继续存活；最后路径释放后请求与父额度均为零。新建Budget自身元数据计入父预留，不能把child_used单独当作这一整段请求总额。

真实2工作器/64等待/2结果池，在相同三最大路径的输入下，两工具链均准入48项（2活动、46等待），第49份因子池额度拒绝。最大child_used16775168 B，小于16777216 B上限，未抬高预算。**64等待是槽容量，不能保证最大字段的66份任务全部适合该预算。** 后续Service身份/配置字段还会消耗额度，不能承诺其同样准入48份。

当前/1.88全线程请求峰值分别11829009/11829418 B，均由父预留16777584 B覆盖。Pool结束并释放后，保留报告仍有11819072 B请求及父预留；最后报告释放后归零。新线程等运行时合计出现3次realloc，不属于进度更新。没有文件/SSH/界面负载，不能外推整体RSS；保留过多完成记录仍会占用同一预算，后续界面必须明确额度拒绝/释放记录，不能无限保留历史。[当前计量](m6-sftp-report-allocation.json)、[Rust1.88计量](m6-sftp-report-allocation-msrv.json)。

## 增量成本和剩余接线

实际System分配器的release微诊断，两次预热/五组交替配对，每组100万次更新及976次读取。弱基线只更新调用者计数，无共享报告，不能当作等价功能替代。单线程无争用时，报告相对控制的增量中位4.425 ns/次，范围4.395–4.574 ns/次；报告循环总耗时约4.78–4.99 ms。它没有真实文件/协议IO、CPU/RSS采样、窗口输入/呈现、多线程争用或长时间观察，不计算采用门槛百分比，不宣称完整SFTP在5%以内。[14次原始运行/配对](m6-sftp-report-cost.json)。

后续[Service统一Action/Output、Progress请求和实际上传/下载](m6-sftp-file-service.md)已补。下一步落实IO余量/成本，再将Status放入工作区固定任务记录。先划分连接归属与目录请求代次，防止目录刷新/隐藏取消或废弃后台传输；再接原始路径/主机身份确认、默认不覆盖、显式覆盖授权、唯一远端临时名、远端源版本核对、后台进度/重试/通知和失败退出交互。Windows文件拥有者/安全IPC、真实系统焦点/IME及SSH终端组合、完整性能、M4全局元数据/其他M4–M8和平台/长测继续按原目标推进。额外测试环境已确认可用，接入方法待用户后续提供，不重复询问，不提交或推送。
