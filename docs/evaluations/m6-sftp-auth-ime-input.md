# 共同受保护输入协议与原生到编辑器交接

状态：共同 InputContext/ProtectedInput、App 的默认安全接收接口和 Editor 的身份/账本检查已实现；Cocoa 客户端已改为使用这些共同类型。在独立主线程程序完成真实 Objective-C 回调到实际 Backend/Front/Editor 的串联验证。认证编辑器、Cocoa 候选和 SFTP 仍在测试路径；普通窗口尚未选择该 context，没有打开认证面板的用户操作入口。本文不代表工作区认证 UI、真实输入法或 M6 已交付。

## 协议与消费

InputContext 只保存完整起始身份和原有 Budget 的 Arc，不保留秘密，也不新建独立池。Editor.input_context 先同步挑战并检查 token/Secret 类型，再创建该快照。Cocoa Context 构建接收这一快照，不能改用“当前视图”或另一个预算；same_source 同时比较完整身份和账本引用。

ProtectedInput 区分 Commit、Preedit 和 Rejected，包含清理文字及其 Lease，没有 Clone/Debug/Display。共同 UTF-8 适配入口在复制前预留 1151 B，限制1023字节；超长、非法UTF-8、NUL/CR/LF不生成输出String。from_native还拒绝缺失文字、超容量、无相应额度及错误Rejected负载，清理后整次拒绝；二次校验不能为已绕过准入创建的大缓冲补做内存证明。

Editor.native_input 每次先同步 Front、检查当前 token、原始完整身份和账本归属，再借用消费。Commit 替换选区并结束组合，Preedit 更新固定组合 Secret，Rejected 清理组合并报告拒绝、保留已提交答案。旧视图或错误账本的结果不会清理当前有效组合；取消/过期则先清理本地秘密。原生输入不重新包装成普通 Text/Preedit，不复制第二份 String。

App.protected_context 默认 None，App.protected_input 默认丢弃清理并保持运行。普通 Congminal/Workspace 继承该行为；本机真实 cat 会话及搜索栏验证未收到受保护文字，另有无窗口的共同默认接收器测试在全部目标编译。没有向普通 Event 增加新的大载荷变体，也没有给默认路径新增线程、进程、定时器或包。

Cocoa 高频 client 查询直接读取 context 自己强持有的专用 ivar，构建时仍校验 superclass 的 client；避免每次交接反复通过弱属性 getter 产生临时 autorelease。对象的持有/析构规则沿用 [原生候选](m6-sftp-auth-ime.md)。

## 真实回调串联证据

[工具 --editor](../../tools/check_ime_native.py) 在临时副本启用实际 auth 模块，仅屏蔽未参与测量的 view/viewport（其 renderer 辅助函数仍是测试配置）；不改 Backend、Front、Editor 的逻辑。主线程真实构建/激活 Cocoa context，通过 Objective-C 消息调用 client，再取出共同 ProtectedInput 交给实际 Editor；不创建窗口，不发 NSEvent，不改变系统输入源，也不运行 SSH/helper。

覆盖组合、最大1023字节提交、旧 Editor 关闭/同挑战重建、新 client 激活后的旧回调、迟到结果拒绝和关闭挑战后清理。两个工具链的采样一致：

| 状态 | Rust请求存活 | 请求峰值 | 账本存活 | 账本峰值 |
| --- | ---: | ---: | ---: | ---: |
| Backend/Editor/首个context创建 | 2886 B | 2886 B | 10105 B | 10105 B |
| 最大提交已消费 | 2886 B | 3909 B | 8954 B | 10105 B |
| 新client接收、旧视图结果被拒绝 | 3158 B | 3909 B | 14574 B | 15725 B |
| 关闭挑战后清理本地答案/组合 | 2135 B | 3909 B | 12272 B | 15725 B |
| Cocoa/autorelease及全部结果释放 | 0 | 3909 B | 0 | 15725 B |

六个采样点的请求存活/峰值有额度覆盖。组合和最大提交的 Editor 消费段均零分配/零扩容；五个输出事件分别在 allocator **释放前**检查清零，5次通过、0次失败，不读释放后的内存。[当前工具链](m6-sftp-auth-ime-editor-native.json)、[Rust1.88](m6-sftp-auth-ime-editor-native-msrv.json)。

另做200次预热“直接回调→取出→同步/校验→提交”微诊断：每条只分配原生解码的一个 String，共200次分配、0扩容。当前/Rust1.88 的 P50 为292/291 ns，P95 为333/292 ns，P99为334/375 ns，最大1250/1292 ns，原始200点排序样本保留。选区准备在计时外，事件文字是已准备的短 NSString；不包含真实输入法进程、窗口调度、按键排队、软件绘制或呈现，不作为GUI输入延迟。早期微秒计时大量截为0，已改纳秒记录，未把0微秒当作零耗时。

共同类型迁移后，原生对象/四槽最大排队探针亦在两个工具链复验通过：请求峰值5387 B、账本峰值12391 B、四次释放前清零和最终归零。[当前](m6-sftp-auth-ime-input-native.json)、[Rust1.88](m6-sftp-auth-ime-input-native-msrv.json)。两类夹具负载不同，不以3909/5387之差宣称内存优化。

全部请求字节不含allocator、C/ObjC/OS内部副本、RSS、栈及预初始化共享控制上下文。源NSString、通知/取消对象、Budget/线程/框架初始化在计量外；没有把已知对象余量解释成完整Cocoa堆硬上限。无新增Cargo依赖，完整全局资源和真实GUI延迟仍待验收。

## 回归与剩余接入

新增7项常规测试：3项共同协议/默认接收、3项Editor消费、1项真实终端/搜索的默认丢弃。覆盖完整身份/池区别、失效挑战、错误账本、组合/提交/空输入、最大长度/非法/拒绝、额度不足和确认型提示。最终默认debug/release各419通过、27忽略，原生可选后端各428通过、29忽略；本机默认/原生与Linux/Windows默认all-target Clippy、Rust1.88原生release编译通过。[最终回归](m6-sftp-auth-ime-input-build.json)。

初次Windows检查发现旧测试别名导入和两个默认方法仅在Unix用例使用；已移除别名并增加真正跨平台的默认接收器行为测试，不用屏蔽Windows测试警告处理。[失败记录](m6-sftp-auth-ime-input-build-before-windows-coverage.json)。交叉检查仍不代表Windows/Linux原生IME运行。

下一步接工作区认证控制器、NSView.inputContext选择、焦点失效/重建view、异步唤醒和按键命令调度，再接真实窗口、selectedRange/replacementRange及各输入法。Windows/Linux分别实现原生来源，不能以共同类型替代平台适配。之后继续完整身份展示、SFTP全局分项控制器/共享队列、退出确认、Windows安全IPC以及其余M4–M8和三平台/长测，完整目标保持。
