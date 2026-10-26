# Cocoa 受保护输入客户端及固定队列

状态：macOS 独立 NSTextInputClient、显式持有 client 的 NSTextInputContext 子类、固定四槽和输出额度交接已在测试候选实现，并在真实主线程进程验证对象生命周期。普通窗口仍使用原客户端；本阶段没有接工作区认证入口、真实按键或输入法进程。默认发布路径未启用候选，不勾选完整 UI/IME/M6。

## 身份及生命周期

每个 client 在构建时持有不可变 InputOrigin，沿用完整 session/generation/connection/challenge/view，不从当前 App 取 token。回调先检查该 client 是否活动，再读取 NSString 并限量创建应用副本。停用先标记无效并清理队列，然后才向系统发送 discardMarkedText/deactivate；即使系统或其他对象仍持有旧 context，旧 client 的提交也不能获得新身份。

本机 SDK 明确 NSTextInputContext 不保留 client。本实现用 context 子类的独立强引用补足：context 的 superclass 完成 dealloc 后才释放 client，防止外部 retain context 导致 client 悬空。client 的 dealloc 把 Owner 移出 Box，先释放 Box 存储和队列，再完成 NSObject dealloc，最后释放剩余额度。构建失败在取得额度前不创建用户数据/实例；Context 拒绝非主线程构建，Rust 拥有者不可 Send/Sync。

独立主线程探针实际创建、激活、停用和销毁这两个 Cocoa 子类，额外 retain 旧 context、释放原 UI 拥有者，再经 Objective-C 消息向旧 client 直接提交。新 client 激活后再发一次旧 client 消息，均无事件/新 String 分配；当前 client 的结果保留新身份。最后清理 autorelease 暂存并验证对象额度归还，不能只以“Rust 拥有者已 drop”推断 Cocoa 对象已销毁。

## 输入与队列

- 每个 client 固定四槽，不使用可增长 Vec/VecDeque；单条最多 1023 字节，严格 UTF-8，NUL/CR/LF 整次拒绝。NSString/NSAttributedString 先看单元/字节上限再借用有效 UTF-8，仅为接收的文字复制一次 String。
- 只有连续末尾 Preedit 可以合并：先清理旧内容再读新内容。Commit 保留顺序，空 unmark 也有明确事件，不借合并丢失提交。
- 超限、非法字符、额度不足或第五条未交付提交会清理当前未交付批次并返回 Rejected，不截断密码。拒绝未交付时，即使新一轮 keyDown 重置普通标志，consumed 仍保持拒绝锁存，阻止普通按键回落。
- UTF-16 marked range 只记录长度；命令请求、候选窗几何和文字事件分开。编辑命令不作为文字静默吞掉；停用后的几何查询返回空范围/矩形。
- 不跨 Foundation 消息调用持有 RefCell 借用。已取出的 Input 独立拥有清理文字和 Lease；NativeContext 全部释放后，尚未交付的输出仍保留额度。

初始固定预留为本机每 client 5620 B：Owner、两个已知 ObjC 实例大小和固定余量，加四份 1151 B 输出预留。每取出一条结果原子转移 1151 B；下一输入先补准入，补不到则拒绝，不因旧输出在途而超支。Rejected 无文字/堆分配，使用零字节 Lease。队列和结果不支持 Clone/Debug/Display；不输出输入文字日志。

该额度不是 NSTextInputContext 的完整内部堆上限；class/runtime、系统服务、ObjC/C 编码缓存及线程栈等仍在测量范围外，不能把已知实例大小和余量解释成完整全局内存验收。

## 原生探针与成本

[工具](../../tools/check_ime_native.py) 复制源码到临时目录，仅在那里启用候选、改为主线程探针入口、注入同线程 GlobalAlloc 统计。没有改实际 main 或产品分配器；不创建窗口，不改变系统输入源。框架、两个共享 Budget Mutex、线程和时钟预热在计量外。

当前与 Rust 1.88 的七点采样一致：

| 状态 | Rust 请求存活 | 请求峰值 | 账本存活 | 账本峰值 |
| --- | ---: | ---: | ---: | ---: |
| 一个最大输出已取出 | 1295 B | 1295 B | 5620 B | 5620 B |
| 再排满四条最大文字 | 5387 B | 5387 B | 6771 B | 6771 B |
| 原拥有者关闭、旧 context 被额外持有 | 1295 B | 5387 B | 6771 B | 6771 B |
| 新 client 活动且旧输出仍在途 | 1567 B | 5387 B | 11240 B | 12391 B |
| Cocoa 对象/autorelease 全部释放、仅旧输出在途 | 1023 B | 5387 B | 1151 B | 12391 B |
| 全部释放 | 0 | 5387 B | 0 | 12391 B |

第五条提交不分配新 String；满队列清理、停用清理、新 client 正常输出、对象退出后的旧输出，共四次在 allocator **释放前**检查已初始化字节，4 次通过、0 次失败，不读释放后的内存。client 的 Rust Owner 为一次 272 B Box；每次接收非空文字只新增那一个 String，无额外队列节点/结果 Box，结果移动不复制文字。

首次 Context::new（框架已初始化）当前/Rust1.88 为 2825/2903 µs。20 次预热“构建、激活、停用、释放、autorelease 清理”P50 为 10/9 µs、P95 均 17 µs、最大均 32 µs，原始排序样本保留。两者测量范围不同，不作冷/热优化比例，也不代表应用冷启动、真实 IME 或输入到呈现延迟。无新增 Cargo 包、工作线程或进程；探针进程只用于验收。

[当前证据](m6-sftp-auth-ime-native.json)、[Rust 1.88](m6-sftp-auth-ime-native-msrv.json)。早期末尾多出 64 B：原生分配回溯明确来自第二个“额度不足”夹具 Budget 的 pthread Mutex/OnceBox 初始化，该测试池未像主测试池一样预热。已修正探针范围，未提高产品预算；[诊断](m6-sftp-auth-ime-native-diagnostic.json) 保留。沙箱内系统文本服务连接报错的初次运行也保留；最终在允许系统文本服务访问的本机环境运行，不能把该错误当作输入法功能验证。

数据只涵盖本线程 Rust GlobalAlloc 请求和已知对象预留，不含 allocator/C/ObjC/OS/RSS/栈及预初始化共享控制上下文，不能证明完整原生堆上限。临时主线程程序直接发送公开夹具回调，没有发真实 NSEvent/组合按键。

## 回归及剩余接入

四项新增常规测试覆盖不可变归属、停用和结果独立额度、组合合并、非法/超限/满队列拒绝锁存、UTF-16 范围、命令/几何及资源失败。最终默认 debug/release 各 412 通过、27 忽略，原生可选后端各 421 通过、29 忽略；本机默认/原生及 Linux/Windows 默认 all-target Clippy、Rust 1.88 原生 release 编译通过。[稳定源码回归](m6-sftp-auth-ime-build.json)。四项新测试只在 macOS 编译；Linux/Windows 静态检查不等于该系统 IME 已实现。

继续接共同的受保护 Input 接口、Editor/工作区路由、NSView.inputContext 选择、焦点失效、异步唤醒/排队调度和按键命令回落。selectedRange/replacementRange、真实输入法候选/组合及 Windows/Linux 各原生来源还需实现、逐项验收。本阶段已验证 proxy/context 的身份和生命周期，不以手工回调替代系统输入法或 GUI 输入 P95/P99。

之后仍按完整计划接认证面板/完整身份、SFTP 共享控制器/队列、退出确认、Windows 安全 IPC、M4 全资源、M7/M8 和三平台/长测；用户已确认额外环境稍后提供方式，目前不阻断本机工作。
