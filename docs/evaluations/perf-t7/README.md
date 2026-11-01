# T7：网格行修改计数与防漏画校验

基于 T6 HEAD `e0e411bc6fef10028bc2924376799bd46eba1455`，不新增 crate。

## 实现与资源

Grid 为每行维护独立 RowState，release 为 u64 修改计数。Grid 的单调 clock 在变更时给被修改行赋新值；旋转/移动时 RowState 与 Row 同步搬移，仍可与 T3 的像素及旧行键平移配合。存储身份来自同一个进程内原子序列，屏幕 Grid 与 History 各有独立身份；历史行不可变，以身份、text_epoch、稳定 line_id 和零修改计数标识。不同会话的历史不会因为坐标相同而共用行键。备用屏交换保留各网格身份，原有 text_epoch 继续区分屏幕切换、RIS 和重排。

Renderer 的 RowKey 保存四元组 `(text_epoch, line_id, storage_identity, revision)` 和非内容装饰哈希。光标、焦点、preedit、选区覆盖范围、shell outcome、UI decorations、底栏、回看时历史长度等仍逐项参与。release 不再遍历 Cell 计算行内容哈希；Row 保持 32 B，Cell 保持 20 B。

在 debug_assertions 构建中，RowState 额外保存上次观测内容哈希和有效标志；每次可变入口先使该行观测失效。再次读行时，如内容改变却没有修改计数入口就 panic。Renderer 的 RowKey 另保存旧内容哈希，在身份/计数完全相同而内容不同（包括可见历史）时 panic。RowKey 随 ViewState 交换、随 last_rows 平移，跨窗格和滚动校验不依赖另一份未交换数组。此校验由普通 debug 回归、语料和随机差分运行；release 测量使用真实产品计数路径，不运行旧逐格校验。

新增堆数组仍先由 Grid 总 Lease 预留，新增 Kind/独立 Lease 均无。令 C=retained_cols(cols)、R=rows、I=max(2R,32)、S=size_of(RowState)，两屏 Grid 预留改为 `2*((R+1)*C*sizeof(Cell)+I*(sizeof(Row)+2*S))`。2*S 保守覆盖状态数组新旧重分配重叠；allocated_bytes 包含 states.capacity()*S，缩小和取走行后仍保留的容量持续计费。release S=8，debug S=24；80×24 新增预留分别 1536 / 4608 B，200×60 为 3840 / 11520 B。已有创建/缩放准入失败测试沿用动态公式，仍须申请失败时不修改原状态。

RowKey 工作/旧帧数组继续使用 Tracked<Vec> 的 Kind::Render，按真实容量×size_of(RowKey) 准入。元素从 8 B 变为 release 40 B / debug 48 B；两份 60 行索引在容量恰好为 60 时分别增加 3840 / 4800 B。每个 ViewState 的旧帧索引也按同样元素大小计费，失败退回完整重画。身份、clock 和 History 身份为内联字段，不额外计堆容量。

## 所有可变入口核对

| 入口 | 修改/搬移方式 |
| --- | --- |
| Grid::IndexMut | 返回 &mut Row 前 touch；覆盖 Row 的 IndexMut、cells 修改、reset/erase/resize/repair_wide 及 wrapped 赋值。 |
| clear | 所有行 touch 后重置。 |
| scroll_up | Row/RowState 同步 rotate_left，尾部回收/压缩/重置前 touch；搬移行保留计数。 |
| scroll_down | 同步 rotate_right，顶部空行 touch。 |
| take_rows | 清除状态长度，保留状态容量继续计费；外部取走的 Row 不再属于可见 Grid。 |
| set_rows | 按新行数重建状态长度，所有行 touch；原行新内容不会沿用旧计数。 |
| retire_row | 同步 pop 状态和行；退休网格不再可见。 |
| take_top_rows | 同步 drain 头部状态和行，其余状态保留。 |
| push_top | 修复输入 Row 后，同步移除尾部并插入新头部状态，touch 新头行。 |
| resize | 调整状态长度/容量，所有保留/新增行 touch 后 resize 和宽字修复。 |
| new | 新存储身份、全新状态与网格同生命周期。 |

Grid.rows 私有；生产代码绕开 IndexMut 的修改只存在以上 Grid 自身方法。外部直接修改 `grid[r].cells`、`grid[r].wrapped`、`&mut grid[r]` 都先调用 IndexMut。同步 reflow、流式 reflow 与 stepped 输出直接操作的 Row 属于尚未装入网格的工作集，最终分别经 set_rows/resize/push_top 安装；旧源 take_rows/retire_row 入口已覆盖。History 的压缩物理行不可变，头部淘汰通过稳定行号/身份反映；RowView 解码生成独立 Row，不提供可变借用。

## 测试与文件清单

- every_mutable_entry_preserves_or_advances_the_matching_row_stamp：所有入口、局部上/下滚、进入历史、移动保留行计数、set/take/retire/resize/clear、状态容量和存储身份。
- reservation_covers_both_state_arrays_and_their_growth_overlap：最小、常规、宽网格公式，覆盖新旧状态数组重叠。
- content_guard_catches_a_bypassed_mutable_entry：故意直接改私有 cells，必须 panic。
- touching_another_row_does_not_hide_a_missed_revision：其他行合法修改不能掩盖漏标，marks 变化也被捕获。
- previous_visible_history_hash_is_checked_even_when_stamp_is_unchanged：故意使历史旧哈希不匹配，证明 Renderer 的历史防漏画检查执行。
- equal_history_coordinates_from_different_sessions_do_not_share_row_keys：两个会话的相同行号、epoch 和回看位置仍有独立历史身份；切换后的像素与 invalidate 全量绘制一致。
- ascii_tests::same 每块比较前校验两屏；fuzz_tests 初始化后及逐块差分执行校验；corpus_tests 每个 512 B 块前后校验两屏。修改计数是内部机制，不要求 ASCII 批量与逐字符路径的内部计数相同。
- 原 partial/full、滚动平移、溢出字形、选区/光标/搜索/命令标记、stride 和多窗格像素一致性测试全部保留。

| 文件 | 用途 |
| --- | --- |
| src/term/grid.rs | RowState、全入口计数/搬移、观测校验、额度公式与实际容量。 |
| src/term/grid/history.rs | 不可变历史存储的唯一内联身份。 |
| src/term/grid/compact.rs | 旧内容哈希仅在 debug_assertions 构建保留。 |
| src/term/grid/revision_tests.rs | 全入口、容量公式及两种漏标失败测试。 |
| src/term/mod.rs | 可见行身份/计数和两屏校验入口。 |
| src/term/ascii_tests.rs | 快慢逐块差分同时执行防漏画校验。 |
| src/term/fuzz_tests.rs | 随机输入第一块前建立观测基线。 |
| src/term/corpus_tests.rs | 逐块语料前后检查网格变化。 |
| src/render.rs | 行键改用身份/计数和装饰，调试历史哈希与失败测试，平移适配。 |
| src/render_work.rs | Tracked 行键数组元素改为 RowKey。 |
| docs/foundations.md | 同步 Grid/Render 堆容量与校验口径。 |
| docs/evaluations/perf-t7/README.md | 审计材料、命令、测量、风险。 |

## 环境与运行

MacBook Air Mac16,12 / Apple M4 / 16 GiB / macOS 27.0.1 (26A434) / rustc 1.99.0，AC Power、电池 100%。macOS arm64 实际运行；Linux/X11、Windows、macOS Intel **未在真机验证**。

```sh
cargo test --release --bin congminal perf_bench -- --ignored --nocapture --test-threads=1
cargo test --release --bin congminal --features alloc-count perf_bench -- --ignored --nocapture --test-threads=1
```

两配置各连续三次，同机同命令中位数，前为 T6 t6-measured-*，后为 t7-*。没有并行编译/回归或原生 UI 操作污染正式计时。原始输出保存在 target/evaluations。初次容量测试漏写整数 usize 类型导致编译失败，修复后重新执行全部测量，失败日志保留 t7-compile-failed.log，不纳入样本。

## 固定基准（三次中位数）

| 指标 | 单位 | T6 默认 | T7 默认 | 变化 | T6 alloc-count | T7 alloc-count | 变化 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `size_of::<Cell>()` | bytes | 20.000000 | 20.000000 | +0.00% | 20.000000 | 20.000000 | +0.00% |
| `size_of::<Row>()` | bytes | 32.000000 | 32.000000 | +0.00% | 32.000000 | 32.000000 | +0.00% |
| `history_bytes_10k` | bytes | 1533274.000000 | 1533274.000000 | +0.00% | 1533274.000000 | 1533274.000000 | +0.00% |
| `allocs_per_mib_parsed` | allocs/MiB | skipped | skipped | — | 156.437500 | 156.437500 | +0.00% |
| `alloc_bytes_per_mib_parsed` | bytes/MiB | skipped | skipped | — | 34297.500000 | 34297.500000 | +0.00% |
| `allocs_per_frame` | allocs/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `alloc_bytes_per_frame` | bytes/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `parse_ascii_scroll` | MiB/s | 224.355056 | 220.458047 | -1.74% | 221.800538 | 205.533811 | -7.33% |
| `parse_ascii_noscroll` | MiB/s | 1187.093152 | 1186.235025 | -0.07% | 1219.952894 | 1172.827063 | -3.86% |
| `parse_ascii_2027` | MiB/s | 215.849176 | 211.699602 | -1.92% | 216.122513 | 196.119290 | -9.26% |
| `parse_sgr` | MiB/s | 186.612180 | 180.548189 | -3.25% | 185.075159 | 170.434819 | -7.91% |
| `parse_cjk` | MiB/s | 123.694105 | 111.708173 | -9.69% | 123.207377 | 110.642314 | -10.20% |
| `frame_full` | us/frame | 3603.647843 | 3628.568747 | +0.69% | 3592.611901 | 3657.485839 | +1.81% |
| `frame_one_row` | us/frame | 126.454609 | 59.725362 | -52.77% | 127.325788 | 59.868663 | -52.98% |
| `frame_scroll_1line_growing` | us/frame | 774.400522 | 768.793084 | -0.72% | 768.507138 | 776.862017 | +1.09% |
| `frame_scroll_1line_full` | us/frame | 772.141385 | 776.597126 | +0.58% | 767.797726 | 792.361793 | +3.20% |
| `frame_unchanged` | us/frame | 65.746599 | 0.290014 | -99.56% | 68.443092 | 0.309034 | -99.55% |
| `frame_scrollback_view` | us/frame | 2253.548732 | 2222.358383 | -1.38% | 2233.155283 | 2249.356120 | +0.73% |

T6 default 三轮波动 >=5%：无。
T6 count 三轮波动 >=5%：无。
T7 default 三轮波动 >=5%：无。
T7 count 三轮波动 >=5%：parse_ascii_scroll 10.38%；parse_ascii_2027 11.57%；parse_sgr 7.24%；frame_scroll_1line_full 8.81%；frame_unchanged 8.45%。

## 验收结论与风险

状态：**部分完成**。行修改计数、所有可变入口覆盖、屏幕/历史身份、调试防漏画校验及渲染一致性已实现；unchanged 默认/计数组降低 99.56% / 99.55%，超过 80% 目标；单行修改降低 52.77% / 52.98%，allocs_per_frame 仍为零。

“滚动帧不退化”未完全满足：满历史默认 +0.58%、计数组 +3.20%，增长中计数组 +1.09%。计数组 CJK 吞吐 -10.20%，默认 -9.69%，是当前最明显的非目标路径代价；新增每次可变借用的计数工作与此路径相关，但没有独立采样证明全部差距均由它引起，不能归为已证实的布局噪声。计数组比较器因 CJK 超过 10% 门槛及五项波动返回失败；默认组比较器通过，亦不等于严格不退化达标。

计数组上述五项波动没有达到低于 5% 的稳定性要求。unchanged 约 0.3 微秒的绝对跨度很小；其他项目存在运行/调度波动，但尚未独立定位原因。保留按原命令顺序的三次结果，不挑选最快样本或更改基准来达标。额外 Grid 和 Render 保留容量已计费，80×24 网格两屏预留增加 1536 B（release），共享工作/旧帧行键索引也扩大；这是性能收益的内存代价。

风险顺序：解析吞吐代价；滚动帧小幅退化；计数组波动和未定位原因；调试构建故意保留逐格哈希及线性查找旧行键，不能用其帧耗时代表 release；Windows/X11、macOS Intel 未在真机验证。T4 的历史编码吞吐缺口、T5 的滚动帧差距及 T6 的 RSS 增长仍保留在各任务报告，本任务未宣称修复。

## 最终检查

全部通过：cargo fmt --check；x86_64-unknown-linux-gnu、x86_64-pc-windows-gnu、aarch64-apple-darwin、x86_64-apple-darwin 的 cargo clippy --all-targets -- -D warnings；额外本机 alloc-count Clippy；Rust 1.88.0 check（沿用 `RUSTUP_HOME=/tmp/congminal-perf-rustup cargo +1.88 check`）。默认完整回归 950 passed / 56 ignored，alloc-count 952 passed / 56 ignored，两组辅助程序均 21 passed。语料、随机差分、防漏画 panic、全部渲染一致性及资源/额度回归均在完整回归中执行。

按本次阶段收尾要求停在 T7；T8 的 X11/Win32 提交与 MIT-SHM 尚未实施。
