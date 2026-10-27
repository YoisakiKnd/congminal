# Git 面板在大仓库上的实测成本（M7 F09）

roadmap 的刷新条目一直标着「大仓库实测延迟未采样」。本文补上这次采样：用真实 `git` 在 2 万文件的仓库上跑面板用的两条命令（`status --porcelain=v1 -z -uall` 与受限 `diff`），记录耗时、条目数与截断行为。

## 方法与复现

```sh
# 2 万文件、每十个改一个（常规情况）
cargo test --locked large_repository_status_and_diff_cost -- --ignored --nocapture
# 2 万文件全部改动（走 max-files 截断路径）
CONGMINAL_LARGE_REPO_STRIDE=1 cargo test --locked large_repository_status_and_diff_cost -- --ignored --nocapture
# 改规模
CONGMINAL_LARGE_REPO_FILES=100000 cargo test --locked large_repository_status_and_diff_cost -- --ignored --nocapture
```

测试自建临时仓库（`git init` → 写 N 个文件 → `git add` → `commit` → 改文件），然后连续跑三次 `status()` 取中位与最大值，再对其中一条未暂存条目跑一次受限 `diff()`，最后断言「条目数不超过 `max-files`」「2 万文件 20 秒内完成」「采样结束后额度归零」。测试默认 `#[ignore]`，不进常规 CI（2 万文件的仓库创建本身就是几秒磁盘写入）。

## 结果（本机：AMD EPYC 7B13 4 vCPU、Linux 6.8、git 2.43.0、debug 构建）

| 场景 | 未暂存条目 | 列出的条目 | `limited` | `status` 中位 | `status` 最大 | 单文件 `diff` | 快照占用额度 |
|---|---|---|---|---|---|---|---|
| 2 万文件，改 2 000（常规） | 2 000 | 2 000 | false | **124 ms** | 128 ms | 5.7 ms | 已归还 |
| 2 万文件，全改（截断路径） | 20 000 | 5 000 | true | **234 ms** | 259 ms | 5.1 ms | 已归还 |

- 计数（`Counts`）在截断后仍然是全量（20 001 条未暂存），只有列表被截到 `max-files`（5 000）并置 `limited`，界面据此显示「不完整」提示——不会把 5 000 当成真实变更总数。
- 耗时随「有变更的文件数」增长，不随仓库总文件数线性增长：`git status` 只 stat 索引里的路径，未改动的文件不产生输出。
- 这是 debug 构建（`cargo test` 默认 profile）；release 构建下 Rust 侧的解析与额度记账更快，`git` 子进程本身的耗时不变，所以上表是偏保守的上界。

## 对预算与并发的影响

- 一次 `status` 的额度按 `max-files` × 每条目的路径/标签长度预留，2 万文件的截断场景仍受 `max-files` 上限约束，不会因为仓库大而无限增长。
- `status` 与 `diff` 各占一个全局执行槽与两个读取线程（`git::run` 里 stdout/stderr 各一个托管线程），跑完即回收；面板同一时间只允许一个在途 `status` 和一个在途 `diff`，重排会取消旧任务并杀子进程。
- UI 线程不参与：上面 124/234 ms 都在后台任务线程里，期间输入与重绘照常；超过 `timeout-seconds`（默认 5 s）就杀子进程并保留旧内容。

## 仍然未验收

- 网络文件系统（NFS/SMB）与慢盘上的表现没有实测：那里的 `stat` 延迟取决于网络往返，`git status` 可能比本机慢一到两个数量级，`timeout-seconds` 与 `max-files` 需要按现场调整。
- 10 万级文件、超大 diff（`max-files` 与 4 MiB 上限都被打满）与子模块密集的仓库没有采样。
- 面板的「目录元数据指纹」只 stat 三个路径，与大仓库规模无关；这部分没有额外成本。
