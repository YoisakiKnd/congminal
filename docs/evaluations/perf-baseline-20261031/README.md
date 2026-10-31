# T0 返工审计（2026-10-31）

基于 147553ee88b8b26274bbc5da547cfcca0c485959 和本地 db3c31983757528d24d74b29159ef4ac6308e1d1。原始日志移到 `target/evaluations/perf-baseline-20261031/`；仓库没有要求本组原始数据入库的约定，现有 foundations 中的原生评估也使用被忽略的 target/evaluations。

## 原提交 26 文件逐一分类

(a) 基准和工具；(b) 文档及原始输出；(c) 产品代码和测试钩子。

| 文件 | 分类与处理 |
| --- | --- |
| `Cargo.toml` | (a) 增加非默认 alloc-count；撤回 LTO / codegen-units。 |
| `ci/perf-compare.py` | (a) 手动比较或 RSS 工具，修正 5% 门槛、skipped 与三时间点采样。 |
| `ci/rss-sample.ps1` | (a) 手动比较或 RSS 工具，修正 5% 门槛、skipped 与三时间点采样。 |
| `ci/rss-sample.sh` | (a) 手动比较或 RSS 工具，修正 5% 门槛、skipped 与三时间点采样。 |
| `docs/evaluations/perf-baseline-20261031/README.md` | (b) 用本审计结论和运行命令替换旧报告，移除原始数据链接。 |
| `docs/evaluations/perf-baseline-20261031/after-1.log` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/evaluations/perf-baseline-20261031/after-2.log` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/evaluations/perf-baseline-20261031/after-3.log` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/evaluations/perf-baseline-20261031/after-summary.txt` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/evaluations/perf-baseline-20261031/before-1.log` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/evaluations/perf-baseline-20261031/before-2.log` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/evaluations/perf-baseline-20261031/before-3.log` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/evaluations/perf-baseline-20261031/before-summary.txt` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/evaluations/perf-baseline-20261031/comparison.txt` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/evaluations/perf-baseline-20261031/environment.json` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/evaluations/perf-baseline-20261031/initial-trials.txt` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/evaluations/perf-baseline-20261031/rss-after-1.txt` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/evaluations/perf-baseline-20261031/rss-after-2.txt` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/evaluations/perf-baseline-20261031/rss-after-3.txt` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/evaluations/perf-baseline-20261031/rss-before-1.txt` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/evaluations/perf-baseline-20261031/rss-before-2.txt` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/evaluations/perf-baseline-20261031/rss-before-3.txt` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/evaluations/perf-baseline-20261031/validation.txt` | (b) 移到 target/evaluations/perf-baseline-20261031/，从版本控制删除。 |
| `docs/foundations.md` | (b) 同步 feature、独立额度、指标口径和脚本用法。 |
| `src/main.rs` | (c) 必要测试挂载；唯一两行均受 cfg(test) 保护，release 不编译基准或分配器。 |
| `src/perf_bench.rs` | (a) 整个模块由 main 的 cfg(test) 挂载；新增 feature 隔离计数器和逐指标 Budget。 |

## 范围与资源

无产品热路径改动、无新增 crate、无新增 Kind。测试输入仍为 Queue 的 Tracked<Vec<u8>>，先准入再分配、先销毁再还额度。每个指标独立根预算 1 GiB，字体映射 256 MiB；开始与结束检查 denied=0、没有 history_reclaim_pending，销毁后 used 恢复初值。预算拒绝会累计在根，不能因后续清理掩盖；历史压力路径请求回收且首先发生拒绝，故这两个断言会捕获压力降级，无需添加产品测试钩子。

计数器只在 cfg(all(test, feature = "alloc-count")) 注册；release（包括带 feature 的 release）不挂载 perf_bench。默认测试及旧注入工具没有第二个全局分配器。未启用统计的 allocs/alloc_bytes 输出明确 skipped。

原 LTO 三轮中位数显示 parse_sgr 150.806920 → 143.441207 MiB/s（-4.88%），parse_cjk 192.808494 → 182.693262（-5.25%）；超过 3%，故撤回 fat LTO 和单 codegen unit，保留 debug=1 和默认 unwind。不据此声称新协议的性能提升。

为降低帧基准噪声，保留 64 帧预热，将 full/one_row/unchanged/scroll 次数调整为 1800/25000/60000/3000；各场景新建独立 Scene。解析负载保持原大小和预热协议。基准独立总耗时应低于 10 秒，非整套基准十秒。更改测试次数及 Scope 是 T0 规定的隔离与稳定性修正，之后任务固定该协议。

## 运行命令

```sh
for run in 1 2 3; do
  cargo test --release --bin congminal perf_bench -- --ignored --nocapture --test-threads=1 > target/evaluations/t0-default-$run.log 2>&1
done
for run in 1 2 3; do
  cargo test --release --features alloc-count --bin congminal perf_bench -- --ignored --nocapture --test-threads=1 > target/evaluations/t0-count-$run.log 2>&1
done
python3 ci/perf-compare.py target/evaluations/t0-default-{1,2,3}.log
python3 ci/perf-compare.py target/evaluations/t0-count-{1,2,3}.log
```

RSS 的 start 使用生产者开始门控；end 为生产者输出完毕时的存活样本；idle 为 end 后三秒、五个间隔 200 ms 样本的中位数。并非解析队列排空证明。Unix 用 EXIT trap 递归终止后代并清临时目录，PowerShell finally 用 taskkill /T /F 清后代，正常退出也有超时。Windows 脚本未在真机验证。

## 新增/修订测试

- benchmarks：每个指标独立预算；捕获拒绝、回收请求、释放遗漏、不滚动误入历史、满历史未填满、静止帧产生 damage 等错误。
- denied_input_and_unwinding_restore_state：输入准入失败不改变 used；alloc-count 开启时 unwind 关闭计数作用域。
- allocator_counts_system_calls_without_changing_leases（仅 alloc-count）：alloc/zeroed/realloc 三次、64 字节；先放行 Queue 128 字节再分配，释放后还 Lease。

## 平台和环境

MacBook Air Mac16,12，Apple M4（4P+6E），16 GiB，macOS 27.0.1 (26A434)，rustc 1.99.0 (b940084d7 2026-09-28)，AC Power / 电池 100%。Linux/X11、Windows、macOS Intel **未在真机验证**，交叉编译不代表运行验证。



## 三轮中位数与稳定性

全部为 release。旧列来自 db3c319 保存的同机三轮（LTO、计数器默认注册）；新列为 T0 无 LTO 的两种 feature 配置。T0 按要求调整了隔离/迭代协议，不能将差值解释为产品热路径的纯性能变化。后续任务必须分别使用相同 feature 和命令与 T0 对比。

| 指标 | 单位 | db3c319 | T0 默认 | T0 alloc-count |
| --- | --- | ---: | ---: | ---: |
| `size_of::<Cell>()` | bytes | 20.000000 | 20.000000 | 20.000000 |
| `size_of::<Row>()` | bytes | 32.000000 | 32.000000 | 32.000000 |
| `history_bytes_10k` | bytes | 40082112.000000 | 40082112.000000 | 40082112.000000 |
| `allocs_per_mib_parsed` | allocs/MiB | 156.437500 | skipped | 156.437500 |
| `alloc_bytes_per_mib_parsed` | bytes/MiB | 638188.000000 | skipped | 638188.000000 |
| `allocs_per_frame` | allocs/frame | 0.000000 | skipped | 0.000000 |
| `alloc_bytes_per_frame` | bytes/frame | 0.000000 | skipped | 0.000000 |
| `parse_ascii_scroll` | MiB/s | 134.264537 | 137.220325 | 135.469411 |
| `parse_ascii_noscroll` | MiB/s | 176.461789 | 171.373984 | 171.110947 |
| `parse_ascii_2027` | MiB/s | 30.228620 | 29.620451 | 29.904947 |
| `parse_sgr` | MiB/s | 143.441207 | 148.073526 | 148.481085 |
| `parse_cjk` | MiB/s | 182.693262 | 195.328967 | 194.130608 |
| `frame_full` | us/frame | 3527.523757 | 4149.073757 | 3576.467802 |
| `frame_one_row` | us/frame | 243.436358 | 271.439801 | 246.532466 |
| `frame_scroll_1line_growing` | us/frame | 2027.331033 | 2345.404453 | 2043.786204 |
| `frame_scroll_1line_full` | us/frame | 2023.480816 | 2360.756121 | 2042.541453 |
| `frame_unchanged` | us/frame | 63.359071 | 63.472915 | 63.930633 |

默认配置全部指标波动低于 5%，最大为 parse_ascii_scroll 的 3.95%；alloc-count 最大同样是 parse_ascii_scroll，4.70%。没有删除或选择替换正式轮次。各绘制测量区间中最长约 7.53 秒（默认 full），解析预热和正式输入合计约 1.1 秒；场景初始化只创建固定小网格/帧及字体，不将整套约 34–37 秒误认为单个基准超时。原始六份日志与 summary 均在 target/evaluations。

## 验证环境修正

初次 `cargo +1.88 check` 因本机 1.88 短名工具链缺 manifest 失败；通过已有临时 `/tmp/congminal-perf-rustup/toolchains/1.88-aarch64-apple-darwin` 到已安装 1.88.0 的链接，以 `RUSTUP_HOME=/tmp/congminal-perf-rustup cargo +1.88 check` 原样重跑通过。未安装新编译器、未改全局工具链。

沙箱内完整回归有七项既有 Unix IPC 测试因 Operation not permitted 失败（906 passed）；重跑需允许创建本地 socket 的沙箱外环境，不修改测试或跳过用例。失败原始记录保存在 target/evaluations/t0-check-6.log。

## 额外兼容性和脚本验证

两套旧探针文件均未修改。它们直接运行时暴露既有临时夹具遗漏：只复制 src/assets，而配置测试 include_str 需要 docs/configuration.md，尚未编译到分配器就失败。为只验证本任务的全局分配器兼容性，用运行时 copytree 包装在复制 assets 后补齐同级 docs，再原样执行工具；输入探针和认证探针均通过，无重复全局分配器错误。认证 10000 次编辑计数为零分配。此临时环境补齐并非工具文件修改，但直接运行旧工具的 docs 缺失仍是已知问题，没有任务外修理。结果保留在 target/evaluations/t0-{input,auth}-probe-with-docs.json。

macOS 原生 RSS 脚本成功退出：rss_start 123764736 B，rss_end 188760064 B，rss_idle 188596224 B，rss_peak 223330304 B。仅单次功能验收，不声称 RSS 性能改善或三轮稳定性。沙箱内禁止 ps/pgrep，故原生采样在允许读取进程信息的环境执行。额外用自启动的模拟终端及 sleep 子进程注入 TERM，确认退出码 130、没有存活父/子进程、临时目录消失。正常路径也完成 wait 后退出。PowerShell / Linux 脚本未在真机验证。

比较工具手动故障注入：错单位、NaN、重复字段、缺字段、缺成功结果均拒绝；shell 脚本 bash -n 通过。上述临时验证脚本和原始输出不纳入产品源码。

## 必需检查结果

| 命令 | 结果 |
| --- | --- |
| cargo fmt --check | 通过 |
| cargo clippy --all-targets --target x86_64-unknown-linux-gnu -- -D warnings | 通过；未在真机验证 |
| cargo clippy --all-targets --target x86_64-pc-windows-gnu -- -D warnings | 通过；未在真机验证 |
| cargo clippy --all-targets --target aarch64-apple-darwin -- -D warnings | 通过 |
| cargo clippy --all-targets --target x86_64-apple-darwin -- -D warnings | 通过；未在真机验证 |
| RUSTUP_HOME=/tmp/congminal-perf-rustup cargo +1.88 check | 通过，实际 Rust 1.88.0 |
| cargo test -- --test-threads=1 | 主程序 913 passed / 57 ignored；helper 21 passed |
| cargo test --features alloc-count -- --test-threads=1 | 主程序 913 passed / 58 ignored；helper 21 passed |
| cargo clippy --all-targets --features alloc-count -- -D warnings | 额外检查通过 |

完整检查与基准原始日志均在 target/evaluations/t0-*，不提交。两个完整回归在允许本地 socket 的环境运行；四目标 Clippy 只是编译检查。T0 将 db3c319 和本次返工整理为一个 [T0] 提交，其直接父提交为 147553e；main 保持原状。推送 HEAD 完整 SHA 记录在聊天进度日志，避免文档自引用提交哈希。
