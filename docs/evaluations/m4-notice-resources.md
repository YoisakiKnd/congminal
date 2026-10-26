# M4 通知记录的资源准入

2026-10-25。本页记录通知阶段冻结版本；后续改动以[最新执行记录](../execution-plan.md)为准。`Center` 的通知记录接入 `notifications-reserved`，首次合格事件先准入固定 128 槽，再分配；满后轮换复用，不逐事件更新预算。清空释放真实容量后归还额度。无新增包、线程或运行时子进程。本页只验收通知记录存储；系统工具、后台任务、侧栏临时文字及完整 M4/M8 继续待办。

## 行为与所有权

空 Center 只持有创建池的零字节票据，静音、关闭类别和普通前台短命令不分配记录缓冲。首次合格事件从该池取额度，即使调用线程切换 Scope 也不转记到另一池。额度不足不缩小缓冲绕过上限，不返回系统派发候选，累计丢弃计数；通知栏显示“额度不足丢弃”。应用 tick 比较 revision，拒绝事件也触发可见状态更新。

命令 ID 和会话结束标记在拒绝前消费，恢复额度后不会重放。清空归还记录容量、清除浏览偏移和本地丢弃计数，保留 ID、结束标记及系统通知的十秒速率闸门。失败恢复后的新事件可以重新准入。

记录是固定数值/类别，不持有命令、主机或输出正文。[本机 release 布局探针](m4-notice-resources-layout.json)直接运行临时发布副本的早返回分支：默认 Notice **32 B**，128 槽 **4096 B**；预览 Notice **48 B**，128 槽 **6144 B**。Center 本身均为 **120 B**，不属于其记录票据。本机测试也启用传输类别，Notice 为 48 B，不能把测试的 6144 B 混称为默认 release 记录大小。探针没有打开窗口、PTY 或系统通知工具，也不证明其他平台运行行为。

## 分配与回归

[当前工具链](m4-notice-resources-allocation.json)和 [Rust 1.88](m4-notice-resources-allocation-msrv.json)采用全线程 epoch Rust 请求计量器，从第一次插入持续到最终释放。预算、上下文与计量前锁初始化已在区间外；不计分配器/C/OS、实际栈或完整应用初始元数据。

| 阶段 | 存活请求与记录额度 | 累计分配/扩容 |
|---|---:|---:|
| 第一次合格事件 | 6144 B | 1 / 0 |
| 持续补满并轮换、静音、额度拒绝 | 6144 B | 1 / 0 |
| 第二个 Center 恢复并接收新事件 | 12288 B | 2 / 0 |
| 两者清空 | 0 B | 2 / 0 |
| 清空后重新接收 | 6144 B | 3 / 0 |
| 全部销毁 | 0 B | 3 / 0 |

连续事件与拒绝没有新增分配。探针请求峰值 12288 B；预算峰值 65536 B 包含人为填满预算的压力票据，不能当作真实堆峰值。清空释放的是 Rust 缓冲和额度，分配器可能继续保留物理页。

[完整回归](m4-notice-resources-build.json)普通 **556** 项、预览主程序 **559** 项及 helper **18** 项通过，专用忽略分别 38/39 项；Rust 1.88 通知相关 **14** 项通过。格式、三目标静态 Clippy 和默认 release 通过。新增常规回归覆盖拒绝提示/不派发、不重放、清空真实容量和速率门保留、静音及创建池归属；交叉检查不是 Linux/Windows 真机验收。

新增分类后，本机 Budget/Stats 为 **304/256 B**。[文件服务](m4-notice-resources-file-quota.json)仍用原 16 MiB 子池及 9 MiB I/O 余量，最大字段场景接收 2 个运行和 11 个等待后由额度拒绝，最后请求及父/子额度归零。[Workspace 复验](m4-notice-resources-workspace.json)请求峰值 **1958029 B**，最后请求/子池/父预留归零，模态绘制零新增分配；这不是原生远程用户流程验收。

## 同负载成本与取舍

[冻结源码对照](m4-notice-resources-cost.json)在前后临时副本注入同一 System 分配器夹具，两次预热后五组交替。每轮测量 5000 次首次插入 API、200 批各 1000 次连续合格事件、200 批各 1000 次有界格式化、5000 次显式清空并重填，以及 200 批各 1000 次静音事件。首次计时不含 Default/drop；清空重填明确包含释放和重新准入。记录、格式化和去重的累积语义指纹一致。

| 场景 | 五组配对耗时中位变化 | 范围 |
|---|---:|---:|
| 首次插入 | +44.98% | +38.55% ～ +45.86% |
| 已有容量的连续事件 | -0.35% | -3.33% ～ +1.78% |
| 有界描述输出 | -1.50% | -2.93% ～ +0.19% |
| 清空并重填 | +180.48% | +175.65% ～ +186.06% |
| 静音事件 | -0.40% | -0.91% ～ +1.16% |

首次插入批次折算约 **21→30 ns**，清空重填约 **15→43 ns**；计时含 Instant/夹具开销，不能当作用户响应延迟。首次测试记录容量由 **192→6144 B**，提前取到固定窗口以避免之后的扩容/逐事件预算操作。旧 clear 保留容量，新 clear 真正释放，因此重填成本增加是明确取舍。连续事件与静音整体接近，不宣称稳定提速。

整进程 RSS 高水位配对中位 **−96 KiB**（−128～−80 KiB），CPU 中位 0s（−0.01～0s，分辨率 0.01s），包含构造、验证及析构，不能逐字节归因到 Center。首次实现连续事件约回退 50%，分离冷路径后仅用普通 inline 提示的候选约回退 130%；原始报告保留为 `*-before-cold-path.json` 和 `*-cost-cold-path-inline-hint.json`。最终显式内联合格入口和固定热路径，保留冷准入，不以取消计费恢复性能。

[实际 UI 成本](m4-notice-resources-ui-cost.json)使用同一前后冻结源码，真实 PTY/app 的五种提示模式，每轮 5000 次更新及 500 次完整软件绘制。更新耗时配对中位 **−19.74%**（−20.79～−16.14%），更新加完整绘制 **−2.62%**（−2.99～−2.54%）；整进程 RSS 中位 **+112 KiB**（0～+144 KiB）。这是该构建与本机负载的结果，不能全部归因到通知，也不代替原生呈现延迟。[暖区间计量](m4-notice-resources-ui-allocation.json)仍零新增分配，像素累积指纹 **6821514814623131185** 与之前相同。

九份当前报告的源码、清单、工具、夹具、语义及基线由[核对记录](m4-notice-resources-audit.json)关联。基线归档 [m4-notice-resources-before.tar.gz](baselines/m4-notice-resources-before.tar.gz)为 664286 B，SHA-256 `78c9e2971bcdff4a3211270ba6de0dac9011e25c9efe8387eafe9b499f7efa0f`，与上一搜索阶段最终源码一致。

## 尚未完成

`Notice::system_text`、系统 Command/参数、SystemGate 和 LatestTask 的完整元数据及平台送达仍未计入这张票据；侧栏 `recent()` 创建的文字仍待后续准入。SFTP 使用另有额度的结果 Ring 和派发路径，本次没有把它改接命令 ID 队列，也不据此宣称统一通知管线全部完成。系统工具默认关闭，依赖成本、权限与平台真机送达继续验收。

Shell/链接存储、配置/侧栏/平台临时文本、根预算/同步对象和通用任务、真实交互延迟仍待。完整 M4–M8、终端兼容及三平台/长测保持执行中；既有 Alpha.6 安装包未更新，未提交或推送。

```sh
python3 tools/check_notice_resources.py --allocation --output /tmp/notice-allocation.json
python3 tools/check_notice_resources.py --allocation --toolchain 1.88.0 --output /tmp/notice-msrv.json
python3 tools/check_notice_resources.py --cost --baseline /absolute/baseline --output /tmp/notice-cost.json
python3 tools/check_notice_layout.py --output /tmp/notice-layout.json
python3 tools/check_notice_evidence.py --output /tmp/notice-evidence.json
```
