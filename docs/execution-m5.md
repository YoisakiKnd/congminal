# M5：OpenSSH 会话与显式重连

已接入可选系统 OpenSSH 后端，未新增 Cargo 依赖。配置 `[connections.<id>] kind = "ssh"` 后，从左栏“连接”点击启动独立 PTY；主机别名由 OpenSSH 读取系统配置，Include/Match/ProxyJump 不由应用自行猜测展开。名称、分组、主机、用户、端口、密钥路径、程序路径及主题关联沿用有界配置档，主题按会话应用仍属 M8。

```toml
[connections.build]
kind = "ssh"
name = "构建主机"
host = "build-alias"
# user = "builder"
# port = 22
# identity-file = "/absolute/path/to/key"
# program = "/absolute/path/to/ssh"
```

工具缺失、不能执行、不支持参数、额度不足、取消和超时分别反馈。后台先执行固定的 `-G -F none` 无网络检查，不读取用户 Include/Match；实际会话再使用正常配置。检查最多一个运行、一个最新待办和一个结果，2 秒软超时，取消会终止并回收检查进程；该超时不限制用户的认证等待。UI 不等待后台检查，检查期间仍可使用原终端。首次使用后复用一个条件变量等待的线程，不周期轮询。

实际启动参数结构化传递，保留密钥、agent、口令和首次主机确认的终端交互；应用不关闭主机密钥校验，不保存密码或私钥正文。`ForkAfterAuthentication=no` 保持进程归应用生命周期管理。其余认证、代理、加密和主机验证遵循用户配置，应用不强制原型测试使用的固定算法。相对密钥路径按 OpenSSH 参数语义解释，建议使用绝对路径。

启动中显示工具检查状态；运行后明确显示“认证/活动状态未知”，不解析提示符或普通终端文字判断登录成功。结束保留终端文本，显示原始退出码或原因未知；非零退出不自动解释为网络、认证或远端命令失败。工具检查耗时、进程存活时长均有各自含义，CLI 下认证完成时间未知。

连接页“取消”取消尚未启动的检查，保留已有会话；已启动连接可关闭对应窗格，OpenSSH 自身的退出交互继续可用。“重连”使用当前会话启动时的配置快照；失败保留旧会话，成功后以新会话编号替换原位置，沿用画布。新连接不继承输入、命令、历史、选区或搜索；旧编号的异步粘贴回复被丢弃。切换上下文或配置后，过期的新建结果不覆盖当前状态，不自动重连。

检查/参数工作集先预留 128 KiB、结果按容量保留，检查槽另预留 2 KiB 元数据；重连另覆盖新参数副本，旧、新队列/网格重叠由共享准入保护。外部 SSH、agent、跳板子进程及系统线程内存另测，不属于完整 512 MiB 堆/RSS 上限；其余任务/字体/界面元数据仍有未接入项目。

本机真实应用已验证直接密钥、独立 agent、加密测试密钥口令、首次主机确认/拒绝、严格模式未知主机、密钥变化、错误密钥、连接拒绝及跳板，成功/失败后额度、映射归零，采样未观察到残留客户端子进程。密码/二次认证、Windows/Linux 真机、30 分钟和完整句柄/交互延迟验收仍待补。隔离 TCP 转发器强制断开已有连接后，已验证旧连接退出、显式重连使用新会话编号、原本地会话仍可输入，并在结束后额度和映射归零。

评估与原始数据见 [M5 后端评估](evaluations/m5-ssh.md)，实际软件 Renderer [画面](previews/m5-ssh.png)。隔离工具不使用用户 SSH 文件：

```sh
python3 tools/check_ssh_transport.py --scenario positive --output /tmp/ssh-positive.json
python3 tools/check_ssh_transport.py --scenario jump --output /tmp/ssh-jump.json
python3 tools/check_ssh_transport.py --scenario interrupt --output /tmp/ssh-interrupt.json
```

该工具需要本机回环监听与进程采样权限；审批额度恢复后已按正常流程完成断链复测。测试监听器、生成密钥和临时主机记录仅用于隔离验收，不把交叉编译或无网络模拟当作真实 SSH 验收。


## 已完成能力检查的结果优先级

并行回归再次触发旧时序失败，保留 [失败记录](evaluations/m6-sftp-auth-input-build-parallel-failure.json) 与 [同一旧二进制回溯](evaluations/m6-sftp-ssh-probe-before.json)。普通桌面权限下也曾失败，不能归因于执行沙箱本身，见 [对照](evaluations/m6-sftp-probe-unsandboxed.json)。

修复一个确定的结果误判：先保留取消优先级，再读取进程退出结果，仅对仍在运行的检查应用原有两秒等待期限。父线程晚观察不能把已经完成的成功/不支持结果改成 Timeout；没有延长产品等待期限。新增实际 Child 完成后的边界回归，并给工作区等待断言加入调用位置；测试模式可输出纯计时和退出状态诊断，不含认证数据。

一次修复后的并行重复仍有旧快速夹具 Timeout，因此没有宣布整个时序问题根治；日志见 [重复失败](evaluations/m6-sftp-probe-repeat-failure.json)。带诊断的相同 release 程序随后三次完整并行运行均 358 通过、22 忽略，见 [连续复测](evaluations/m6-sftp-probe-diagnostic.json)。最终状态与真实 PTY 重连以 [当前构建](evaluations/m6-sftp-auth-input-build.json)、[真实连接](evaluations/m5-ssh-probe-result.json) 为准；长期与 UI 时延验收继续进行。
