# T6：原生快照池、窗口可见性与分配器归还提示

基于 T5 HEAD `98d5550391e88a1bb6d927f54a3b9a82cea4f6d9`，不新增 crate。

## 实现与额度

macOS 完整 CGImage 的 provider 释放回调先释放 Box 外壳，再将 Snapshot 值（像素 Vec 和原 Canvas Lease）移入 Mutex 保护的最多三个槽位。只有当前宽高、当前代次的完整帧可归还，条带沿用直接释放。命中只复制到已有像素容量，provider 外壳仍会分配，未宣称整条提交路径零分配。尺寸变化在新画布准入前移除旧池；普通渲染缓存回收、关闭窗格、清历史及不可见同样清池。晚到回调不能重新填入旧代次。

池共享分配先申请 Canvas：`size_of::<Mutex<PoolData>>() + 2*size_of::<usize>() + align_of::<Mutex<PoolData>>()`，覆盖 Arc 头部与对齐上界。每块仍为 `pixels.capacity()*4 + size_of::<Snapshot>()`，入池不归还额度，元数据保守重复覆盖。固定注册表最多八个窗口，不另分配，满时退回原快照路径。最后一个自定义句柄以 Arc::into_inner 先释放共享分配，再销毁缓存数据及 Lease；池内 Snapshot 不保留池句柄，避免引用环。申请失败保持现有原生画面。

Event::Occluded 在 macOS 由 windowDidChangeOcclusionState 通知唤醒、查询 occlusionState；Win32 来自 WM_SIZE/SIZE_MINIMIZED，X11 来自 VisibilityNotify，另以 Map/Unmap 覆盖映射状态。隐藏时过滤绘制与闪烁计时，保留 PTY、重排、搜索及其他异步工作；恢复时 invalidate 并要求工作区完整重画。没有将遮挡变成 Focus(false)，没有复制 Git/监控的焦点门控。

释放事件由 Session 消费 Terminal 的内联标志，终端解析器不直接依赖平台 API。关闭窗格先销毁被移除的 Pane，再清池/提示系统；最后一个窗格沿用退出流程。系统调用全进程最小五秒间隔：glibc malloc_trim(0)，macOS malloc_zone_pressure_relief(NULL,0)，Windows HeapCompact(GetProcessHeap(),0)，musl 排除 malloc_trim。清池不受间隔限制；间隔内的归还提示不会另建计时线程或每帧重试。CONGMINAL_TRACE 输出池命中/未命中、可见性变化及系统提示耗时（微秒）。提示不保证 RSS 下降。

## 测试与原生验证

- pool_bounds_cross_thread_returns_and_keeps_leases：同时五块归还仅留三块，跨线程释放，地址复用且像素更新，命中不增加额度，清池及最终句柄归还全部额度。
- resizing_and_reclaim_reject_late_callbacks：新旧尺寸像素数相同也不能混用，尺寸/代次变化后的晚到回调不入池，窗口句柄先消失、三个 provider 回调经屏障并发释放最后引用仍无泄漏，元数据额度拒绝不分配。
- renderer_pressure_reclaims_registered_snapshots：从真实 Renderer::reclaim_caches 入口清理注册池，防止池继续占用资源压力回收的预算。
- return_interval_is_at_least_five_seconds：4999 ms 拒绝、5000 ms 允许，不依赖测试睡眠。
- occluded_pty_still_parses_and_has_no_blink_or_draw_deadline：真实 PTY 回显在隐藏时继续解析，清历史标志被消费，绘制/闪烁截止时间消失，焦点不变，恢复可重画。
- occlusion_suppresses_drawing_but_preserves_terminal_and_focus：工作区多标签下全局隐藏状态不会随 Frozen 交换丢失，恢复要求整屏。
- 原 provider 跨线程释放及来源 Framebuffer 提前销毁测试继续按原语义验证，不经池的 helper 最终额度为零。

## 文件清单

| 文件 | 用途 |
| --- | --- |
| src/platform/macos/presentation.rs | 有界、带代次、持有额度的跨线程快照池，注册回收入口及池测试。 |
| src/platform/macos.rs | 遮挡通知与查询、停绘、尺寸准入前销毁池、窗口销毁注销。 |
| src/platform/mod.rs | Occluded 事件及平台快照/分配器回收入口。 |
| src/platform/memory.rs | 三平台系统分配器提示、musl 排除及五秒节流测试。 |
| src/platform/win32.rs | 最小化/恢复时发送 Occluded。 |
| src/platform/x11.rs | VisibilityChangeMask、VisibilityNotify 和 Map/Unmap 事件。 |
| src/render.rs | 所有渲染缓存压力回收同时清理平台快照池。 |
| src/main.rs | 窗口级可见性、停止绘制/闪烁计时，恢复 invalidate 与真实 PTY 回归。 |
| src/workspace.rs | 全局事件先于覆盖层处理、多窗格停绘、关闭后回收与回归。 |
| src/session.rs | 解析及历史记账完成后消费释放事件。 |
| src/term/mod.rs | 清历史/RIS 的内联释放标志，不增加堆记账。 |
| src/ssh/auth_controller.rs | 新平台事件在独立认证控制器中的穷举兼容。 |
| ci/rss-sample.sh | 增加 tabs 场景入口，原单窗口命令不变。 |
| ci/rss-sample-tabs.py | 四标签 RSS 三时点采样、控制文件、正常/异常进程树及临时目录清理。 |
| docs/foundations.md | 同步池保留容量、元数据上界、可见性和系统提示的额度口径。 |
| docs/evaluations/perf-t6/README.md | 本任务审计说明、测量和限制。 |

## 运行环境与命令

MacBook Air Mac16,12 / Apple M4 / 16 GiB / macOS 27.0.1 (26A434) / rustc 1.99.0，AC Power，电池 100%。**Linux/X11、Windows、macOS Intel 未在真机验证**；四目标仅交叉检查，未将其描述为运行通过。macOS arm64 实际运行本机窗口、PTY、Core Graphics provider 和活动监视器。

```sh
cargo build --release --bin congminal
CONGMINAL_RSS_HOLD=1 bash ci/rss-sample.sh "$release_binary" "$output" tabs
# 窗口出现后 Primary+T 三次；控制器四个 ready 后启动固定负载。
# 四个 done 后 Primary+W 三次；关闭完成后等三秒，采五次取中位数。
# HOLD 阶段用最小化按钮及活动监视器核对 CPU，之后在 control.json 的 work 目录创建 finish。
cargo test --release --bin congminal perf_bench -- --ignored --nocapture --test-threads=1
cargo test --release --bin congminal --features alloc-count perf_bench -- --ignored --nocapture --test-threads=1
```

RSS 使用隔离的本地测试 bundle `target/evaluations/t6-native/Congminal Perf.app`，只替换其可执行文件，前后命令、配置与负载一致；避免与用户已安装的同名应用混淆。控制目录与原始曲线均在 target/evaluations，不入库。每个标签输出 seq 1..2000000 和固定 16 MiB ASCII 文件，四标签各保留 10000 行、额度 512 MiB；活动监视器过滤为 congminal，完成后恢复原内存分类及空搜索。

初始 UI 绑定冷启动曾长时间阻塞，旧同名安装包也被误选；相关失败/超时进程已清理，仅在完整四 ready、四 done、关闭三标签、三秒及五采样全部完成时才纳入统计。T6 初轮三次原生测量及六轮核心基准后，又补齐 Renderer 压力入口、尺寸准入前清池，最终数据重新在稳定实现上采集；中间日志保留，不挑选最快运行。


锁屏恢复期间的中间六轮核心测量另保留在 t6-final-*：其中默认 parse_ascii_2027 为 151.944534 / 216.848691 / 225.487618 MiB/s。最终正式计时使用 t6-measured-*，所有原生 UI 操作结束后顺序运行，不并行编译/测试，也不继续操作活动监视器。补强跨线程测试后同一轮再次执行全部检查。

初轮原生实现 after-rss-{1,2,3} 的空闲中位数为 147,357,696 B（相对基线 +2.87%），仅为中间记录，未替代最终实现的三轮结果。

## 最终原生 RSS 与 CPU

纳入统计：T5 基线 before-rss-{4,6,7}；稳定产品实现 after-rss-{5,6,7}，按首次三次完整有效样本取中位数。after-rss-4 因锁屏中断；after-rss-{1,2,3} 为补齐压力/尺寸入口前的中间实现，另保留原始值。

| 指标 | T5 中位数 B | T6 中位数 B | 变化 | T5 极差/中位数 | T6 极差/中位数 |
| --- | ---: | ---: | ---: | ---: | ---: |
| rss_start | 130990080 | 130416640 | -0.44% | 14.56% | 0.38% |
| rss_end | 162037760 | 161906688 | -0.08% | 11.81% | 11.97% |
| rss_idle | 143245312 | 162086912 | +13.15% | 24.60% | 12.00% |
| rss_peak | 190676992 | 191315968 | +0.34% | 10.04% | 9.72% |

RSS 空闲结束值并未下降，中位数增加约 13%；波动很大，不能将差异单独归因于池或系统提示，也不能声称已经改善。池保留像素继续计费，系统合成与分配器页也影响 RSS。曲线每 200 ms 原始采样在各目录 rss.txt；汇总 CSV 位于 target/evaluations/t6-rss-summary.csv。四标签场景增加关闭阶段，三秒空闲等待从关闭完成算起，以免把关闭前值当作释放效果；默认单窗口口径不变。

- after-rss-5 整帧 checkout：命中 886、未命中 6，99.33%。
- after-rss-6 整帧 checkout：命中 894、未命中 6，99.33%。
- after-rss-7 整帧 checkout：命中 885、未命中 7，99.22%。

合计整帧命中率 99.29%，不含局部条带，包含关闭、恢复与退出前更新；不是整个提交路径的零分配比例。

最终二进制 after-rss-7：最小化后 trace 为 occluded=true，连续 5.014 秒 ps CPU 用时增量 0.00 秒；活动监视器 PID 81984 显示 CPU 0.0%、CPU 时间 34.55 秒、8 线程，原生截图在本次对话的工具结果中。恢复后 trace 为 occluded=false，紧随完整快照重建。仅报告工具精度下的空闲 0%；持续输出仍会解析并消耗 CPU。另两次中间原生实现的最小化复测同样为 0.0%。T5 可见/最小化请求后的空闲也为 0.0%，没有宣称这是新增的性能下降比例。

真机验证了最小化产生的整窗不可见状态；没有单独安排其他窗口完全覆盖的场景。Windows/X11 的事件与分配器提示未在真机验证。

## 固定基准（三次中位数）

| 指标 | 单位 | T5 默认 | T6 默认 | 变化 | T5 alloc-count | T6 alloc-count | 变化 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `size_of::<Cell>()` | bytes | 20.000000 | 20.000000 | +0.00% | 20.000000 | 20.000000 | +0.00% |
| `size_of::<Row>()` | bytes | 32.000000 | 32.000000 | +0.00% | 32.000000 | 32.000000 | +0.00% |
| `history_bytes_10k` | bytes | 1533274.000000 | 1533274.000000 | +0.00% | 1533274.000000 | 1533274.000000 | +0.00% |
| `allocs_per_mib_parsed` | allocs/MiB | skipped | skipped | — | 156.437500 | 156.437500 | +0.00% |
| `alloc_bytes_per_mib_parsed` | bytes/MiB | skipped | skipped | — | 34297.500000 | 34297.500000 | +0.00% |
| `allocs_per_frame` | allocs/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `alloc_bytes_per_frame` | bytes/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `parse_ascii_scroll` | MiB/s | 230.940771 | 224.355056 | -2.85% | 231.248230 | 221.800538 | -4.09% |
| `parse_ascii_noscroll` | MiB/s | 1196.342743 | 1187.093152 | -0.77% | 1180.620295 | 1219.952894 | +3.33% |
| `parse_ascii_2027` | MiB/s | 217.826736 | 215.849176 | -0.91% | 220.323969 | 216.122513 | -1.91% |
| `parse_sgr` | MiB/s | 185.325390 | 186.612180 | +0.69% | 183.917581 | 185.075159 | +0.63% |
| `parse_cjk` | MiB/s | 123.201398 | 123.694105 | +0.40% | 122.687851 | 123.207377 | +0.42% |
| `frame_full` | us/frame | 3634.346188 | 3603.647843 | -0.84% | 3567.564280 | 3592.611901 | +0.70% |
| `frame_one_row` | us/frame | 125.498419 | 126.454609 | +0.76% | 124.159399 | 127.325788 | +2.55% |
| `frame_scroll_1line_growing` | us/frame | 777.300661 | 774.400522 | -0.37% | 786.296319 | 768.507138 | -2.26% |
| `frame_scroll_1line_full` | us/frame | 791.456833 | 772.141385 | -2.44% | 784.297043 | 767.797726 | -2.10% |
| `frame_unchanged` | us/frame | 64.863276 | 65.746599 | +1.36% | 64.467228 | 68.443092 | +6.17% |
| `frame_scrollback_view` | us/frame | 2216.561528 | 2253.548732 | +1.67% | 2233.739652 | 2233.155283 | -0.03% |

T5 default 三轮波动 >=5%：parse_ascii_2027 5.25%。
T5 count 三轮波动 >=5%：无。
T6 default 三轮波动 >=5%：无。
T6 count 三轮波动 >=5%：无。

## 检查、结论与限制

最终 t6-measured-*：fmt、四目标 Clippy、额外本机 alloc-count Clippy、Rust 1.88.0 check 全部通过；MSRV 沿用 `RUSTUP_HOME=/tmp/congminal-perf-rustup cargo +1.88 check` 的现有映射。默认完整回归 944 passed / 56 ignored，alloc-count 946 passed / 56 ignored，两组辅助程序均 21 passed。RSS 控制器 Python 语法与 shell 语法通过；所有记录过的临时控制目录均已清理，包含超时、锁屏中断与正常退出。

比较器默认组因 T5 基线原有的 2027 波动 5.25% 返回失败；T6 两种配置本身各项均低于 5%，计数组比较器通过。比较器的 10% 吞吐/帧耗时门槛不能掩盖表中更小的差距：计数组 ASCII 滚动解析下降 4.09%、unchanged 增加 6.17%，默认组回看历史增加 1.67%，原因未独立定位，没有把它们断言为布局噪声。没有修改基准负载或调整比较器门槛。

功能与规定检查完成；原生整帧复用、可见性停绘/恢复及系统提示已实测。RSS 收益未获证实，空闲结束中位数增加 13.15%，这是本任务首要风险；正确但未改善的结果按通用约束保留并报告。其次是上述核心指标退化与 RSS 波动。三块 4K 池像素可达 99,532,800 B，持续计入 Canvas，另有当前/在途 native 图像；额度拒绝仍保持旧画面。密集关闭的后续系统提示可能落在五秒节流内，没有额外调度延后提示；清池照常执行。其他窗口完全覆盖场景未单独实测，Windows/X11 和 macOS Intel 未在真机验证。
