# 可见认证界面接入

状态：编辑模型、分片提示、掩码绘制、单面板控制、完整身份和Unix工作区候选路由已在测试路径实现，见[工作区证据](evaluations/m6-sftp-auth-workspace.md)。macOS平台窗口已接context选择/退役/唤醒，真实视图回调桥接通过，但焦点由夹具显式提供，完整系统焦点探针仍未通过，见 [窗口证据](evaluations/m6-sftp-auth-ime-window.md)。有界执行器和后台结束确认已完成组件验证，见[执行器证据](evaluations/m6-sftp-executor.md)；实际目录任务已接执行器和后台认证发布，见[任务层证据](evaluations/m6-sftp-service.md)；工作区唯一拥有者、远端目录和关闭门禁已有Unix测试候选，见[最新接线证据](evaluations/m6-sftp-workspace.md)；发布入口、传输任务/后台列表、失败退出处置、真实输入法和其他平台来源仍待完成。后端与成本证据见 [父端](evaluations/m6-sftp-auth-parent.md) 和 [成本](evaluations/m6-sftp-auth-cost.md)；本文件不代表完整UI/IME已验收。

## 交互与归属

每个正在认证的连接仍由 Backend/Front 的会话、代次、唯一连接和 challenge 隔离；界面一次只编辑一个明确 Ticket。活动窗格的认证请求可以打开面板；后台窗格只显示待处理状态，不抢当前输入。切换/关闭/重连使编辑和异步剪贴板请求失效，不能将旧文字送进新认证或普通终端。

Secret 默认掩码，允许 UTF-8 光标移动、删除和显式空答案；不启用复制、剪切或粘贴历史，不将答案放进通知/调试/持久化。confirm 采用明确的 yes/no 操作，默认拒绝；普通提示不按英文内容猜密码或主机类型。none 只显示通知。提示使用已有转义后的不可变文本，保留完整指纹/身份；超出可见区域时可滚动，在查看到提示末尾前不启用提交/同意。

Esc 取消并清理编辑/组合内容。明确Secret在组合输入完成后可用Enter提交；明确Confirm默认拒绝。缺少hint/空hint进入Unspecified，掩码编辑但Enter取消，只有已审阅的可见Send控件能提交；不通过提示文字猜测类型，详见[真实主机确认发现与修正](evaluations/m6-sftp-workspace.md)。连接过期、取消、任务结束和旧 Ticket 回复全部显示为失效，不自动重试旧答案。成功提交后立即移动 Secret 给 Front，界面不留答案副本。

## 内存与原生输入

编辑和 IME 暂存各使用现有固定 1023 字节 Secret，删除/替换/取消清零。输入超限或包含 NUL/CR/LF 整次拒绝，不能截断成另一个密码。平台 Text/Preedit 仍携带普通 String，认证消费后须清理其已初始化字节；受保护粘贴已采用带原始身份/独立额度的清理拥有者。不承诺擦除操作系统输入法/剪贴板、CPU 寄存器或其他进程内部副本。

原生剪贴板已扩展按请求限量/严格 Unicode 策略，见 [入口验证](evaluations/m6-sftp-auth-input-admission.md)；现有普通终端仍使用默认策略。认证接入必须实际选择 1023 字节限量，再创建应用副本；已接Unix工作区候选，默认发布尚未启用。异步剪贴板归属包含完整认证 Ticket 和焦点代次，失配结果清理后丢弃。IME 组合也需要焦点/组成归属，防止切换上下文后迟到的提交；仅在 UI 上清空 preedit 不构成这项保证。

显示掩码和可见布局单独准入，不把实际答案借给普通搜索栏或日志。长提示换行索引必须有明确上限、额度及分片构建，事件处理不扫描全部长提示；只绘制可见行。改变窗口大小、滚动和编辑不重新复制全部提示或 Secret。

原生入口进度：macOS 已移除 marked 明文副本和回调文本日志，普通提交/组合在应用副本前限量，暂存替换与拒绝先清理旧副本。后续Cocoa候选已补1023字节策略、不可变起始身份和固定四槽，Unix工作区候选已实际选择该源。普通路径总队列额度、其他平台来源与完整输入链路仍待验收，单条解码探针不证明全局准入。

受保护粘贴进度：三平台请求/结果已移动同一拥有者，Editor 捕获完整挑战及 view，选择 1023 字节严格策略；单个在途请求、状态同步和旧结果隔离已验证。额度独立于原 Editor 存活，原视图关闭后迟到副本仍受管；普通终端/搜索只丢弃并清理。见 [归属和分配证据](evaluations/m6-sftp-auth-clipboard.md)。后续已接Cocoa起始归属和Unix工作区候选；发布入口、其他平台来源和全局队列额度仍待完成。

共同输入进度：InputContext/ProtectedInput 已接到 Editor，先同步 Front、校验完整原身份和账本，再借用消费 Commit/Preedit；Rejected 清组合并保留已提交答案。App 默认无接收器，普通终端/搜索安全丢弃。真实主线程 Cocoa 回调到实际 Editor 已在两工具链验证，见 [串联证据](evaluations/m6-sftp-auth-ime-input.md)。后续Cocoa窗口选择/退役/唤醒及Unix工作区候选已补实现，见最新证据；默认入口、真实输入法和其他平台来源仍待完成。

## 后台与收尾

工作区候选更新：Identity以同池额度捕获Profiles/索引，并把完整请求目标前缀加入同一提示视口/审阅；固定命中区域只接受同view/几何的按下释放，Waiting保留Esc/可见取消。Unix测试路径的Workspace已处理App通道、单独连接代次、普通在途粘贴屏障和覆盖层画布，见[实现与边界](evaluations/m6-sftp-auth-workspace.md)。这些要求已有候选实现，默认构建仍无SFTP入口；执行器基础已实现，目录任务已有后台候选；共享拥有者、远端目录和窗口关闭门禁已有Unix测试候选；传输任务、失败退出处置、发布入口和各平台验收仍未完成。

单面板协调进度：Controller已在测试模块实现上面的焦点清理/新view、挑战切换、单个粘贴交接、审阅提交及明确决定，见[证据](evaluations/m6-sftp-auth-controller.md)。InputContext可保持输入占用而明确阻塞原生client，提交后等待/关闭时不恢复普通终端。真实视图回调到Controller/Panel/Backend已串联，焦点仍显式提供；后续Unix工作区候选已补活动连接路由、完整身份和鼠标控件，后续已接工作区唯一Service/目录/关闭门禁候选，默认发布仍待传输任务及完整验收。

窗口桥接约束更新：失焦先退役client，再通知App；已退役的同一源不得重新激活。工作区须清理本地秘密/组合并重建Editor/view，在认证仍占用输入而未聚焦时保留旧的阻塞描述符，恢复焦点后发布新源。不能在失焦时直接返回None使按键落入终端。命令只返回动作；Submit/Cancel/Paste仍经Panel和控制器处理，滚动/重新排版不能跳过提示审阅。当前默认App仍为None；协调器和Unix工作区候选已执行这些规则，发布路径尚未启用。

SFTP 控制器继续采用全局 16 MiB、2 活动/64 等待；分项额度须与应用全局账本关联，不能另建无限额池。目录结果与连接/窗格代次绑定。后台创建端点/进程，UI 只操作有界状态和 Secret；隐藏时不保留无用途的刷新循环。

关闭应用/窗格先取消并失效界面，再由工作器报告 SSH/helper/端点清理结果。现有 LatestTask 的不 join 兜底不能当作原生库或认证进程已退出的证明。之后才启用默认发布路径，并补原生窗口焦点/输入法/粘贴、输入到呈现 P95/P99、关闭与后台等待、Windows 安全 IPC、三平台及长测；没有用“能画出面板”替代这些验收。

## 原生 IME 接入的下一步约束

进度更新：独立 Cocoa client/context、固定四槽、共同输入及macOS窗口选择/焦点/唤醒已经实现，见 [对象](evaluations/m6-sftp-auth-ime.md)、[共同输入](evaluations/m6-sftp-auth-ime-input.md) 和 [窗口](evaluations/m6-sftp-auth-ime-window.md)。默认App无认证接收器，普通运行不会选择该context；下面的约束仍适用于工作区和实际系统输入法接入，不能把对象/回调探针当作完整IME验收。

普通macOS insertText/setMarkedText仍使用线程局部ImeState；受保护模式会在复制前阻止这一入口，并使用独立client的不可变起始身份。回调先验证client仍活动，再限量生成应用文字，旧client不能借新焦点重新取token。工作区必须实际发布对应源，不能只给普通ImeState增加“当前token”。

本机 SDK 的 NSTextInputContext.h 提供 initWithClient:，并明确 context 不保留 client，由 owner 保留 context。实现须显式处理两者所有权，不能只释放旧 client 后让仍存活的 context 留下悬空引用。discardMarkedText 的声明只要求丢弃转换会话并清理 marked range，没有提供所有旧回调已排空的确认，不能以它代替客户端身份校验或生命周期证明。接入时优先验证独立 client/context 的析构与迟到回调，再接现有窗口；不能把存储“当前 token”的模拟测试当作系统输入法验收。

受保护文字队列须有固定槽和独立额度，丢弃/替换先清理；组合更新可合并，提交溢出须明确拒绝而非截断密码。普通键盘事件、关闭事件和候选窗坐标不能被队列限额静默吞掉。Windows/Linux 分别按其原生输入来源实现和验收，不假设 macOS 的客户端对象模型适用所有平台。
