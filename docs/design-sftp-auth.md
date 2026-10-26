# SFTP 可见认证交接方案

状态：挑战状态、秘密输入及单次 IPC 帧已实现并仅编译进测试，见 [核心验证](evaluations/m6-sftp-auth-core.md)。系统随机能力、Unix 私有票据/套接字和按需独立 helper 已完成本机隔离互操作，见 [helper 验证](evaluations/m6-sftp-auth-helper.md)。父端双槽推进器及独立流回调已通过真实 Rust 父端/helper/OpenSSH 组合验收，VERSION 后关闭认证阶段，见 [组合验证](evaluations/m6-sftp-auth-parent.md)。可见界面已有Unix工作区候选；有界执行器及后台join确认也已完成组件验证，见[执行器证据](evaluations/m6-sftp-executor.md)。目录任务已接后台认证发布及显式清理/join，见[任务层证据](evaluations/m6-sftp-service.md)。工作区唯一拥有者、远端目录和关闭门禁已有[Unix测试候选](evaluations/m6-sftp-workspace.md)；发布入口、传输任务/失败退出处置及Windows权限/端点仍待完成；本文件不表示完整认证已经交付。

采用系统 OpenSSH 的 askpass 入口，继续由它处理加密、身份、主机记录与代理。依据 [ssh 环境约定](https://man.openbsd.org/ssh.1#ENVIRONMENT)，单独为目标子进程配置 SSH_ASKPASS/SSH_ASKPASS_REQUIRE=force；不修改 GUI 进程全局环境，不将 SFTP 二进制管道当成认证输入。无需让用户已有终端执行命令，也不依赖其配置恰好开启连接复用。

## 交互与凭据边界

- 每次请求携带窗格会话 ID、代次和单调 challenge ID；只接受同一请求的回复。切换、重连、关闭和取消使旧请求失效，不能把旧答案给下一条连接。
- 同一连接最多 16 次交互，认证阶段从状态创建起最多 120 秒；完成认证后关闭该阶段，独立数据通道不能受这个计时器误杀。单提示最多 16 KiB；显示时保留换行及可读信息，转义终端控制字符和双向控制字符，不截断后继续确认。
- [OpenSSH 当前 askpass 读取实现](https://github.com/openssh/openssh-portable/blob/master/readpass.c) 使用有界答案缓冲并截断换行。应用答案限制为 1023 字节，不接受 NUL/CR/LF，超限拒绝，不能静默截成另一个密码。允许用户显式提交空答案；取消有独立状态，不能转换成空答案。
- 默认掩码输入，不解析英文提示来猜密码类型。按明确的 SSH_ASKPASS_PROMPT 提示区分 confirm/none：权限确认默认拒绝，必须显式同意；none 只作通知，不索取凭据。普通主机确认仍完整展示提示，由用户按提示输入。
- [OpenSSH 主机确认代码](https://github.com/openssh/openssh-portable/blob/master/sshconnect.c) 将主机及指纹放在传给确认函数的完整提示内。界面须展示完整身份信息；不默认同意，不关闭严格检查，不将未知、变化或撤销主机当作密码问题自动重试。
- 答案不实现 Debug/Clone/Display，不进入配置、粘贴历史、诊断或持久化。输入、IPC 暂存和 helper 输出副本分别拥有明确额度，交接后立即清零旧块，释放实际内存后归还额度；IME 临时组合和取消路径也要覆盖。

未分类提示更新：真实OpenSSH主机确认不保证提供confirm hint，缺省/空hint现为Unspecified（IPC类型3），不再当作已知Secret或根据英文猜测。界面保留完整审阅和掩码编辑，Enter取消，仅明确点击Send提交；明确Confirm仍默认拒绝。验证及兼容边界见[工作区证据](evaluations/m6-sftp-workspace.md)。

## 独立 helper 与本机 IPC

采用单独的 congminal-ssh-helper，复用协议、秘密缓冲、票据和额度源码，不加载字体、配置监视器或 GUI；由默认关闭的 ssh-auth-helper 功能构建。单个参数总是 askpass 提示，三个及以上参数才识别 --launch，提示恰好叫 --launch 也不误入启动器。启动器只设置目标 SSH 的环境并传递分项 argv；Unix 用 exec 替换自身，Windows 等待受控子进程并转交结果。两者都不得启动 shell 拼接命令。Windows 启动器必须留在原有 Job 中，不启用 breakaway；其额外常驻 RSS 和进程数须重新测量，当前 Windows 票据读取明确返回不支持。

askpass 入口按 OpenSSH 调用形式接收提示，通过私有本机端点向父工作器请求一次答案。Unix 使用 0700 目录内的 Unix socket；不采用仅验证客户端 nonce 的裸回环 TCP，以免端口回收后被其他本机用户冒充服务端。Windows 后续需要私有命名管道、创建时 DACL 和服务端身份核对。请求有协议版本、固定头、长度和由系统随机源生成的 32 字节能力值；先验证身份和长度，再分配提示。父工作器须限制为最多一个等待输入及一个通知连接，其他请求明确拒绝；部分帧、错误 nonce、断开和取消不能悬挂工作槽。答案帧区分成功/取消，最大 1023 字节。helper 只向 stdout 的非缓冲 File 写原答案字节后结束，不追加换行或诊断，避免在答案恰好达到工具读取边界时发生多余写入；结束前清零。

能力值不放命令行、SSH 环境或实际凭据文件。启动器只向目标 SSH 环境传私有票据的路径，askpass helper 自行读取 IPC 地址/能力值；票据只含临时连接能力，不含答案。[SendEnv 支持通配](https://man.openbsd.org/ssh_config.5#SendEnv)，不能让用户配置把内部能力值送往远端。票据名称使用独立标识，不从能力值截取。Unix 私有目录/文件须在创建时具备 0700/0600 权限；Windows 必须在创建时应用当前用户的私有 DACL，不能先公开创建再收紧，也不能以 Unix mode 或 readonly 代替 ACL。权限适配没有通过时明确拒绝该桥接，继续保留已经可用的 key/agent 路径。票据由后台拥有者在任务结束时清理，不能在 UI 析构中做磁盘读取或遍历。

父端复用 SFTP 工作槽的非阻塞等待来处理 IPC，不为每个提示增加线程。接收和完成写入均有限量与期限。等待用户期间单独计入认证时限，不冒充网络超时；取消必须唤醒/结束该等待并清理 SSH/helper。UI 只处理有界内存中的提示和一次答案。Unix 非原子继承标志设置仍须与 PTY fork 协调，不能让普通 shell 继承凭据套接字。窗口退出必须有工作器清理确认；仅丢弃目前不 join 的 LatestTask 句柄不算完成回收，也不能留到原生库 atexit 清理之后。

## 接入顺序及验收

1. 实现共享挑战/答案状态与短生命周期秘密输入，验证空答案、取消、超限、重复回复、跨窗格/代次回复和清零。随后实现有界帧及 helper，保持默认测试隔离。
2. 接通正常后台流的服务回调及认证等候期限；以自建加密私钥、首次主机接受/拒绝、错误口令、helper 断开/伪造帧和取消验证真实 OpenSSH，不读取用户 SSH 文件。
3. 接认证界面，验证键盘焦点、IME、遮罩、Esc、关闭窗格及迟到结果；借用原有终端输入不是完成该流程。
4. 接活动窗格的远端目录和共享传输队列。全局 16 MiB 分项、2 活动/64 等待、原始路径字节、进度与原子本地提交继续按 roadmap 完成。
5. 新增 helper 后重做至少五轮同负载开销及认证等待 CPU/RSS/进程/线程测量，特别计入 Windows 启动器；不能沿用无认证桥接时的零额外线程/进程结论。

真实密码/多因素服务需专用测试环境，尚未提供运行方式；不修改本机用户密码、系统 PAM 或全局 sshd 来制造夹具。可独立完成帧/界面/加密私钥交接测试，但不能据此勾选真实密码、多因素和 Windows/Linux 运行验收。用户已确认可稍后提供环境，继续推进本机实现即可。
