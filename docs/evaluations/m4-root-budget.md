# 根预算对象自身的准入

补上主程序全局池、配置引导池、SSH 认证 helper 和本地目录 helper 的 Budget/Arc/首次 Mutex 元数据。生产独立根只能使用 `Budget::root`；原零基线 `Budget::new` 仅在 `cfg(test)` 下存在。已经取得父票据的子池通过私有构造创建，父票据覆盖其对象和锁，避免双重收费。

根在 Arc 分配前检查额度，并在初始 Stats 中直接记入 `budget-reserved`，不建立持有自己的 Lease/Arc 环。当前 64 位本机 Budget/Stats 为 **352/304 B**；根预留 **480 B**，包含 Arc/同步余量。默认 512 MiB 等配置上限不增加，自身元数据从其中扣除。低于自身费用的创建先拒绝，不产生堆请求；初始额度不足传回固定 Denied，主入口明确处理。

这只覆盖账本自身，其他全局/进程创建锁、线程运行时/TLS/栈/退出确认、配置解析和普通取消令牌等仍待。测试的原始小池不假装是生产独立根；本阶段没有把这类预初始化对象计入受测区间。生产根未使用 Weak 拥有者，本轮不证明任意 Weak 在强拥有者退出后继续保留 Arc 块的完整计费。

## 冷请求和最后拥有者

[当前工具链](m4-root-budget-allocation.json)与 [Rust 1.88](m4-root-budget-allocation-msrv.json)从零开始计量根对象及第一次锁使用。快照先于计量采样，确保把首次快照引发的 64 B 锁分配计入同一检查点；不能用尚未取得锁之前的读数代表冷路径。源码副本使用 epoch 全线程请求计量器，生产分配器未替换。

| 阶段 | 实际存活请求 | 根总额度已占用 |
|---|---:|---:|
| Arc/根对象及首次 Mutex | 432 B（2 次请求） | 480 B |
| 增加实际 64 B 数据 | 496 B | 544 B |
| 数据释放后建立 4 KiB 子池及首次锁 | 864 B | 5056 B |
| 子池实际填满 4 KiB | 4960 B | 5056 B |
| 原根变量销毁，子池/输出继续拥有 | 4960 B | 5056 B |
| 最后数据与票据释放 | 0 B | 根和子池均销毁 |

全程 6 次请求、0 次扩容；拒绝路径零请求，各检查点实际峰值不超过当时预留峰值。子池自身 `budget-reserved=0`，没有把父票据已包含的元数据在子池再扣一次。另两项常规回归验证根扣费后的精确剩余额度、独立映射额度、父变量先销毁及无自引用环。

## 同负载成本与回归

[成本报告](m4-root-budget-cost.json)使用同一 System 分配器夹具和明确的前后构造适配器：100×100 次冷根创建/快照/32 B 票据/快照/释放、100×10000 次票据取得释放、100×10000 次快照。前后以扣除根自身费用后的 payload 差值比较语义，指纹固定 320000；两次预热、五组交替。不用零基线根的 used 值要求新根漏计以保持相同数字。

五组配对根创建中位 **+0.21%**（-5.39%～+1.41%），票据取得/释放 **+1.84%**（+1.63%～+2.30%，每次约 11.77→11.99ns），快照 **-0.28%**（-30.60%～+0.42%，约 9.00→8.97ns）。快照有明显单组离散，不能以范围极值宣称稳定提速。整进程 RSS 配对中位 **-80 KiB**（-96～-64 KiB），CPU 按 0.01s 分辨率无差异；不把 RSS 差值归因为根对象省下的内存。

[UI 成本](m4-root-budget-ui-cost.json)更新中位 **+0.13%**（-2.32%～+1.33%），更新加完整软件绘制 **+0.25%**（-0.24%～+0.62%），整进程 RSS **-112 KiB**（-208～+32 KiB），CPU 中位按 0.01s 分辨率约零（范围约±0.01s）。测量含 API/拥有者释放，进程 CPU/RSS 含测试框架与核验；不能外推系统进程、真实窗口输入到呈现、其他平台或长测。

[完整回归](m4-root-budget-build.json)普通 **585** 项、预览主程序 **588** 项及 helper **20** 项通过；MSRV 账本 **11** 项、helper **20** 项通过；格式、默认 release 及三目标静态 Clippy 通过。交叉检查不是真机验收。

[SFTP 文件额度](m4-root-budget-file-quota.json)仍为 **16 MiB 子池、9 MiB I/O 余量**，2 个运行/11 个等待后因额度拒绝，最终实际请求/父/子费用归零。此夹具父池是显式隔离测试池，独立根冷费用由上面的从零计量直接验证，不把它们混成同一个测量区间。

[暖 UI](m4-root-budget-ui-allocation.json)实际更新 5000 次、完整软件绘制 500 次无新增请求，像素指纹 **6821514814623131185**。另有 [真实隔离 SSH/SFTP 预览入口](m4-root-budget-preview-entry.json)核验使用新根构造的 helper、认证、目录、本地选择/上传/下载和服务回收；仅为本机回环及软件画布，不是原生窗口/输入法或用户环境验收。

另以 [生产最小目录 helper](m4-root-budget-directory-helper.json)运行真实 512 项目录/原名字节、完整性及故障回收回归；七轮直接读与子进程模型相同，普通主程序的目录 helper 构建也通过。它只验证根构造改变后的可用性，没有完成整个目录隔离候选的成本/默认启用门槛。

九份当前报告由 [证据核对](m4-root-budget-audit.json)关联当前全部 Rust 源码、清单、工具、夹具及[基线](baselines/m4-root-budget-before.tar.gz)。基线 **678487 B**，SHA-256 `0c66dd0ab6f1eddb2bd39fc2b9f1002866425084558deb5b9c97d8f693dcb58f`，对应前一任务核心阶段最终源码。无新增包/线程/子进程依赖；上述检查本身会创建临时测试进程。

## 未复现的绘制分配与下一步

前一任务阶段工作区全线程绘制区间捕获的两次请求仍未定位来源。本轮加入固定 64×32 原生调用栈诊断，独立线程 ID、37 B 已知分配校准通过，并用每轮独立临时目录运行 **10 轮、每轮 20000 次**路径表单绘制；[诊断](m4-task-draw-allocation-trace.json)未复现任何分配。它绑定改根之前的冻结源码，扩大区间且有诊断开销，既不是当前根版本性能证据，也不是原两次分配已修复的证明。首次诊断等待超时及后来重复目录错误均保留在对应 `*-before-*.json` 记录，后者在诊断驱动中通过每轮独立 cwd 修正，没有放宽生产检查。

LatestTask 当前仍丢弃 JoinHandle。下一步必须把运行时/显式栈费用留给外部回收拥有者，在工作线程真正 join 后归还；仅让工作器函数返回时丢票据不够，也不能把可能等待 TLS 的 join 放进 UI 回调。继续补完整线程/全局同步、配置/平台/launcher/通知元数据、交互/关闭时延，之后完成全部 M5–M8、终端兼容、三平台及长测。外部测试方式待用户稍后提供。本阶段不勾选完整 M4，不更新安装包，不提交或推送。

```sh
python3 tools/check_root_budget.py --allocation --output /tmp/root-budget-allocation.json
python3 tools/check_root_budget.py --allocation --toolchain 1.88.0 --output /tmp/root-budget-msrv.json
python3 tools/check_root_budget.py --cost --baseline /absolute/baseline --output /tmp/root-budget-cost.json
python3 tools/check_root_budget_build.py --output /tmp/root-budget-build.json
python3 tools/check_root_budget_evidence.py --output /tmp/root-budget-evidence.json
```
