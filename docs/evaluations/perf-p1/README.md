# P1：压缩缓冲复用上限与 macOS 快照池

基于 P0 `b65e634e1fddadeb16de93a4bbcc2e28e2622c50`。

## 改动

| 文件 | 内容 |
| --- | --- |
| src/term/grid/history.rs | 满历史复用旧缓冲时要求容量在需要的 1 倍到 2 倍加 32 B 之间；快路径和慢路径的 spare 都按此判断，过大的旧缓冲直接释放。 |
| src/term/grid/compact_tests.rs | 回归测试：长彩色行之后的短行历史不得超过直接写入短行的 1.5 倍（未修复时为 11 倍）。 |
| src/perf_bench.rs、ci/perf-compare.py | 新增 `history_bytes_10k_after_long`。 |
| src/platform/macos/presentation.rs | 快照池上限 3 → 1。 |
| docs | 设计稿与资源边界说明。 |

## 结果

Linux x86_64 release：

| 指标 | P0 | P1 |
| --- | ---: | ---: |
| history_bytes_10k_after_long B | 13,660,000 | 1,611,508 |
| history_bytes_10k B | 1,533,274 | 1,533,274 |
| history_bytes_10k_cjk B | 2,398,141 | 2,398,141 |

解析吞吐在噪声范围内不变（统一行长时复用条件始终满足）。

macOS 快照池只做了交叉编译检查，**未在真机验证**。T6 在 M4 上测得三块池使空闲 RSS 上升约 13%；降为一块后需用 `CONGMINAL_TRACE` 的 `snapshot_pool hit=` 记录确认命中率，并用 `ci/rss-sample.sh` 对比空闲 RSS。
