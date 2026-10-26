# macOS 受保护输入的窗口桥接

状态：平台窗口已接入 context 选择、失焦前退役、异步唤醒与有归属的命令。默认 App 仍返回 `None`，认证编辑器/SFTP 仍仅测试编译，没有打开认证面板的用户入口。真实窗口系统焦点和物理输入法未验收，完整 M6 保持未完成。

## 路由与生命周期

`NSView.inputContext` 在普通模式调用 NSView 实现；受保护模式选择独立 context，未聚焦、构建失败或旧源被重复使用时返回 nil 并阻塞普通输入。窗口失去 key 状态先退役 context，再给 App 发送 Focus(false)。退役先关闭 client，再清理组合/队列、discard/deactivate；额外持有的旧 context 仍拥有其 client，但回调不能产生新文字或借用新源。

单窗口槽保存源快照、context 和一个不持有 Arc 的退役身份；身份包含完整挑战/view 和账本指针等值标记，指针不解引用。失焦后须使用新 Editor/view；重复提供刚退役的源只报告一次 Rejected，不能重新启用。界面在认证仍占用输入且失焦时应继续保留阻塞描述符，清理秘密并重建视图，恢复焦点后发布新源；过早返回 None 会恢复普通输入。控制器还未接入，不能把这一调用约束当作工作区已实现。

同步按键经独立 context 处理，未被输入法消费时按限量 NSString 路径产生提交；不经过普通键盘的 Vec/String 队列。移动、选择、删除、Submit、Cancel、Paste、滚动等使用仅含枚举/数字的 Command，携带原始身份和账本，不包含字符答案。Editor 校验后只返回提交/取消/粘贴等动作，最终提交仍需 Panel 的提示审阅与明确确认，不能由平台回调自行同意主机或读剪贴板。复制/剪切不产生秘密副本。

Command 不申请动态载荷；Cancel 清理已有四槽/拒绝状态后排队，满额时也能产生。异步组合/提交通过现有 Cocoa application wake 唤醒，连续更新合并一次通知；同步按键不多发唤醒。回调不调用 App、不跨系统消息持有 RefCell 借用；临时 Handle 只保持对象存活，只有根 Context 退役 client。每批至多消费四项，App 消费后立即同步源；余项重新唤醒。受保护模式每轮 Cocoa 事件最多64个或合作式2ms，随后 tick/绘制；单次原生调用仍可能超时，不是硬实时保证。普通模式保留原排空策略，M4 全局队列/延迟仍待收尾。

源切换会清理普通文字暂存及排队的 Text/Preedit/Paste；受保护期间普通文字、按键和菜单请求不会回落到终端。源为 None 时不构建代理对象，无新增后台线程、进程、轮询定时器或 Cargo 包。每轮有固定成本的源/焦点检查，其完整普通窗口性能尚未量化，不声称零开销。

## 原生桥接证据与系统焦点限制

[工具 --bridge](../../tools/check_ime_native.py) 创建真实 NSWindow/NSView，并通过 Objective-C 调用 inputContext、keyDown、组合/提交及菜单粘贴，串联实际 Backend/Front/Editor。**聚焦状态由夹具显式传入**，不运行完整 Window.run；JSON 标记 `system_focus_verified:false`。两工具链均通过：普通按键1、受保护命令2、文字输入4、合并唤醒1、旧 client 迟到交付0、退役源拒绝1、最终额度0。旧源重复同步不会重复报错，新视图不能消费旧菜单粘贴结果。[当前](m6-sftp-auth-ime-window-bridge.json)、[Rust1.88](m6-sftp-auth-ime-window-bridge-msrv.json)。

严格 `--window` 另走完整事件循环、自己投递 NSEvent 和自己的两个窗口；仍要求真实 isKeyWindow。当前自动运行环境下窗口 visible=1、canBecomeKeyWindow=1，但应用 active=0、isKeyWindow=0；普通启动和 LaunchServices app bundle 启动均不能取得系统 key 状态。诊断已证明 Cmd+A 到达 keyDown，只因焦点门禁被阻塞，不按菜单派发故障修改产品。[路由诊断](m6-sftp-auth-ime-window-route-diagnostic.json)、[焦点状态](m6-sftp-auth-ime-window-state-diagnostic.json)、[LaunchServices](m6-sftp-auth-ime-window-launchservices-diagnostic.json)。显式调用框架 becomeKeyWindow 的尝试也未通过，[记录](m6-sftp-auth-ime-window-framework-diagnostic.json)。没有弱化生产焦点检查或把模型焦点记为系统验收通过；不存在自动审批拒绝。

实际输入法、selectedRange/replacementRange、系统激活切换、候选窗位置及输入到呈现仍须原生窗口验证。Apple [激活接口说明](https://developer.apple.com/documentation/appkit/nsapplication/activate%28ignoringotherapps%3A%29?changes=_5&language=objc) 说明激活可能延后，不能由一次激活请求推断已获得 key 状态。

## 内存与耗时复验

增加命令和唤醒状态后，arm64 Owner Box 从272增至344 B；代理基础预留1088 B，加四槽每槽1151 B，首次共5692 B。Command 无载荷/无新堆分配，文字仍最多1023字节，保留先申请额度再复制以及输出移动额度的规则。退役身份不持有预算 Arc，避免仅记住旧源而继续钉住子预算/上级额度。

真实主线程对象/四槽探针和回调到实际 Editor 探针分别在当前工具链、Rust1.88复验：

| 负载 | Rust请求峰值 | 账本峰值 | 最终请求/账本 | 释放前清零 |
| --- | ---: | ---: | ---: | ---: |
| 四槽最大文字＋在途输出、外部保留旧context、新client | 5459 B | 12535 B | 0/0 | 4通过/0失败 |
| Backend/Editor、组合/最大提交、旧视图和取消 | 3981 B | 15869 B | 0/0 | 5通过/0失败 |

逐点存活/峰值均由额度覆盖；两种负载不可直接比较为优化幅度。[对象当前](m6-sftp-auth-ime-window-lifetime.json)、[对象Rust1.88](m6-sftp-auth-ime-window-lifetime-msrv.json)、[编辑当前](m6-sftp-auth-ime-window-editor.json)、[编辑Rust1.88](m6-sftp-auth-ime-window-editor-msrv.json)。组合和最大提交的消费段零分配/扩容。200次预热直接回调/取出/消费，每条一个原生解码 String，共200分配、0扩容；P50/P95/P99/最大为291/292/334/1208 ns，Rust1.88为292/333/334/1250 ns。短 NSString 已准备，选区操作在计时外；不含 NSEvent、输入法、窗口调度、绘制或呈现。

框架预热后的首次 context 构建2922/2960 µs；20次暖构建/激活/停用/释放 P50为10/9 µs、P95均14 µs、最大均34 µs。这些只是组件诊断。Rust请求计量不含栈、allocator、C/ObjC/OS内部副本和RSS，已预热共享预算 Mutex、线程、时钟/框架，源 NSString 也在计量外；对象余量不能证明完整 Cocoa 堆硬上限。计量器只注入临时副本，不替换生产 allocator。

## 回归和后续

新增4项常规测试：Editor命令身份/显式动作及满额取消、异步合并唤醒、命令无普通字符、失焦时阻塞普通输入/关闭源后恢复。默认 debug/release 各423通过/27忽略，可选原生后端各432通过/29忽略；本机默认/原生和 Linux/Windows 默认 all-target Clippy、Rust1.88原生 release 测试编译、默认生产 release 二进制构建，共10项通过。[最终记录](m6-sftp-auth-ime-window-build.json)。交叉检查不代表 Windows/Linux 原生运行。

初次跨目标检查发现 identity 仅本机调用；共同 same_source 已改为直接使用同一完整身份比较，没有用屏蔽警告处理。[初次记录](m6-sftp-auth-ime-window-build-before-identity-use.json)。工具与夹具源码、清单及实际源 SHA256保存在各 JSON；更新退役源断言前的桥接结果另存，没有覆盖失败诊断为通过。

继续接工作区认证控制器、完整配置档/主机身份、Panel审阅和事件路由；补各平台来源/真实IME、退出确认、全局16MiB/2活动/64等待SFTP控制器及远端目录/传输。Windows安全IPC、完整资源准入和GUI尾延迟、M7/M8、三平台与30分钟长测均保持原计划。
