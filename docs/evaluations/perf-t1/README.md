# T1：ASCII 批量写入与 2027 增量分段

基于已审计 T0：`5e84e86a5de95e0980cabf41ff691cecb8e93f1e`。本次只实现 T1，T2–T8 待后续任务；不新增 crate、不修改平台提交路径或 T0 负载。

## 实现与副作用核对

| 调用链/状态 | 核对结果 |
| --- | --- |
| Parser::advance → begin_batch | 每个非空输入块只调用一次，保留 graphics.expire 和 text_revision 的次数；空输入不调用。 |
| Ground / UTF-8 | 只有字节为 0x20..=0x7E、Ground、解码器空闲时才扫描连续 ASCII；多字节中途和转义序列继续原 byte 状态机。 |
| Perform::print_ascii | 默认逐字符调用 print；Terminal 覆盖为批量写入，不改变其他 performer。 |
| 字符集与 IRM | 当前激活的 G0/G1 非 ASCII，或 insert=true 时整段回退 print_char；不跳过字符集转换、insert_blanks 及命令输出失效。 |
| 字形边界 | 2027 前一簇能与首个 ASCII 合并时，先逐字符处理首字。ASCII 的 GCB 为 Other，之后相邻 ASCII 均为断点，可继续批量写入。 |
| 行尾与 autowrap | 行内按剩余列宽分块，恰好写到末列只设置 pending_wrap；下次字符通过 wrap_line → index → scroll_region_up 执行原滚动语义。DECAWM 关闭时继续覆盖末列。 |
| 宽字符碎片 | 按原逐字符顺序检查 WIDE/WIDE_SPACER；另一半通过 Cell::blank 保留其原背景。包含批量写入范围外的相邻格。 |
| last_char / last_glyph | 每段写入结束更新为实际最后写入的字符/格子；仅追加字素簇时保持旧 last_char，REP 仍使用原入口。 |
| cursor.template | 整个 Cell 模板照抄，保留 fg/bg/flags、超链接 ID 和 marks 中的 Kitty 占位颜色位。 |
| epoch / revision | 普通 print_char 不单独增加 revision，快速路径也不增加；begin_batch、滚动、重置和重排沿用原入口。差分逐块核对 text_stamp、text_bounds、graphics.revision。 |
| shell 标记/命令输出 | 普通覆盖不额外调用失效函数；IRM、字符集回退及局部滚动仍走原链；主屏滚动保持历史/line_id，回看 display_offset 随历史增长调整。测试比较命令 ID、prompt、output、outcome 及目录/phase。 |
| 链接水位 | finish_link_run 位于 OSC/重置等原有控制入口，不在普通字符上调用；批量路径保留同一 flags.link_id，差分比较链接目标。 |

2027 的每次边界查询复制缓存的 Segmenter 后 push(c)，不分配字符串。Cell 一致时使用增量状态；不一致时从实际 cell.c 和 marks 的迭代器重建。emoji 宽度判定也用 base+marks 迭代器，不构造 String。附加成功后推进缓存；附加被上限拒绝时不推进。样式变体选择符若仅存于 EMOJI flag，仍不进入分段文本，保持旧实现的细节。

旧实现中 last_glyph 只在初始化/RIS 时置 None；RIS 通过 with_storage 同时清空缓存。光标移动、擦除、插入/删除、备用屏切换、宽化、重排可能使坐标内容变化：cursor_follows 校验位置和范围，缓存再校验实际 Cell（包含 marks 编号、flags），不匹配即重建。已有 marks 表只追加且编号不复用；无需每个编辑入口维护另一份失效清单，也不改变原本零宽字符仍可附加到 last_glyph 的行为。

## 资源记账

没有新堆缓冲或 Kind。Terminal 新增内联 `Option<(Cell, Segmenter)>`；令其大小为 S，Grid 预留由原公式变成：

```text
2 × ((rows + 1) × max(2 × cols, 32) × sizeof(Cell)
     + max(2 × rows, 32) × sizeof(Row)) + S
```

依旧在 Terminal::try_new / prepare_resize_inner 先申请 Kind::Grid。拒绝时保留旧网格、光标和 revision；缩放新旧预留重叠时保守覆盖两份 S，提交后只保留一份。缓存字段在 grid_memory 前销毁；没有额外 Lease。既有 initial_grid_admission_and_drop_cover_both_screens 测试的容量断言同步纳入 S，额度不足/恰好够/释放的行为继续验证。docs/foundations.md 同步公式。

## 差分与零分配测试

- Parser::advance_slow 仅在 cfg(test) 下逐字节调用旧状态机；Terminal 的 slow_print 开关让 2027 查询保留旧 String + grapheme::extends 算法，RIS 后保留测试开关。release 不含这些测试钩子。
- ascii_boundaries_and_graphemes_match_the_old_path：1/2/7/80 列、1/7/1024 字节输入块；行尾、pending_wrap、DECAWM、IRM、G0/G1 DEC 字符集、宽字符两半覆盖、ASCII 与组合符/ZWJ/RI/Indic/Prepend 混合、RIS、超链接、REP、shell 标记，以及 UTF-8/ESC 恰好跨 1 KiB 块边界。
- corpus_ascii_fast_and_slow_paths_match_after_every_chunk：四份 term/corpus/*.bin，80×24、17 和 1024 字节分块，逐块比较主/备用网格、历史及全部终端/图片 revision。
- 现有 fuzz_tests 随机输入用相同八个固定种子同步驱动快慢终端，每块之后及每次重排之后比较，不删改原来的健壮性断言。
- grapheme_boundary_checks_do_not_allocate（alloc-count）：在预热且不滚动的 2027 终端中分别使用批量和单字符入口，断言边界查询及普通 ASCII 写入为零分配。组合字符的持久存储/驻留表仍沿用原有路径，没有宣称持久文本存储零分配。

差分还比较 wrapped、完整 Cell、历史 dropped、光标/template/pending_wrap、saved cursor、modes、字符集、tabs、marks 表、应答、标题、键盘栈、last_char/last_glyph 和 shell 输出位置。命令 elapsed 为实际墙钟时长，不要求两次顺序执行恰好相等；其他命令字段逐项一致。

## 复现和环境

```sh
for run in 1 2 3; do
  cargo test --release --bin congminal perf_bench -- --ignored --nocapture --test-threads=1 > target/evaluations/t1-default-$run.log 2>&1
done
for run in 1 2 3; do
  cargo test --release --bin congminal --features alloc-count perf_bench -- --ignored --nocapture --test-threads=1 > target/evaluations/t1-count-$run.log 2>&1
done
python3 ci/perf-compare.py target/evaluations/t1-default-{1,2,3}.log --baseline target/evaluations/t0-default-{1,2,3}.log
python3 ci/perf-compare.py target/evaluations/t1-count-{1,2,3}.log --baseline target/evaluations/t0-count-{1,2,3}.log
```

MacBook Air Mac16,12 / Apple M4（4P+6E）/ 16 GiB / macOS 27.0.1 (26A434)，rustc 1.99.0 (b940084d7 2026-09-28)。测量前 pmset 确认 AC Power、电池 100%；未并行编译或跑其他回归。相同 T0 workload、字体、缩放、预热及迭代数，各运行三轮取中位数，不调整基准补齐目标。原始输出全部保留 target/evaluations。

首版试测发现 CJK 退化和吞吐目标缺口；保留 trial-a 日志，暂停后将非 ASCII 字节从 ASCII 扫描候选检查中提前排除，并合并宽字符属性的快速排除条件。trial-b 为修正后的单次试测。正式六轮在最终源码上完整顺序运行，不以试测挑选替换正式样本。

## 改动文件

| 文件 | 用途 |
| --- | --- |
| src/vt.rs | ASCII run 回调、Ground 扫描及仅测试的逐字节入口。 |
| src/term/mod.rs | 行内批量写入、增量 Segmenter 缓存、内容校验和测试慢路径。 |
| src/term/grapheme.rs | ASCII 分段初态，旧 extends 仅保留为测试参照。 |
| src/term/emoji.rs | 共享字符迭代器算法，允许无 String 的 base+marks 输入。 |
| src/term/grid.rs | Grid 预留增加固定分段缓存 S。 |
| src/term/memory.rs | 既有网格准入测试将缓存纳入容量断言。 |
| src/term/ascii_tests.rs | 统一状态差分与专门边界输入。 |
| src/term/corpus_tests.rs | 四份语料逐块快慢差分。 |
| src/term/fuzz_tests.rs | 既有随机流和重排同步驱动慢路径参照。 |
| src/perf_bench.rs | 新增不改变基准负载的 2027 零分配回归测试。 |
| docs/foundations.md | 同步固定状态记账和 ASCII/2027 行为。 |
| docs/evaluations/perf-t1/README.md | 本审计材料、运行方法和最终指标。 |

Linux/X11、Windows、macOS Intel **未在真机验证**；仅 macOS arm64 执行软件解析/绘制/完整测试，交叉编译不视作运行验证。本任务不涉及原生窗口改动。

## 三轮中位数（完整指标）

Δ 为数值变化；MiB/s 越高越好，us/frame 越低越好。默认和 alloc-count 是独立构建，分别与相同 feature 的 T0 对比，不能交叉比较。

| 指标 | 单位 | T0 默认 | T1 默认 | Δ | T0 alloc-count | T1 alloc-count | Δ |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `size_of::<Cell>()` | bytes | 20.000000 | 20.000000 | 0.00% | 20.000000 | 20.000000 | 0.00% |
| `size_of::<Row>()` | bytes | 32.000000 | 32.000000 | 0.00% | 32.000000 | 32.000000 | 0.00% |
| `history_bytes_10k` | bytes | 40082112.000000 | 40082112.000000 | 0.00% | 40082112.000000 | 40082112.000000 | 0.00% |
| `allocs_per_mib_parsed` | allocs/MiB | skipped | skipped | — | 156.437500 | 156.437500 | 0.00% |
| `alloc_bytes_per_mib_parsed` | bytes/MiB | skipped | skipped | — | 638188.000000 | 638188.000000 | 0.00% |
| `allocs_per_frame` | allocs/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `alloc_bytes_per_frame` | bytes/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `parse_ascii_scroll` | MiB/s | 137.220325 | 403.752162 | +194.24% | 135.469411 | 338.900629 | +150.17% |
| `parse_ascii_noscroll` | MiB/s | 171.373984 | 974.430102 | +468.60% | 171.110947 | 976.789184 | +470.85% |
| `parse_ascii_2027` | MiB/s | 29.620451 | 395.629119 | +1235.66% | 29.904947 | 328.535065 | +998.60% |
| `parse_sgr` | MiB/s | 148.073526 | 297.880569 | +101.17% | 148.481085 | 265.475484 | +78.79% |
| `parse_cjk` | MiB/s | 195.328967 | 172.710562 | -11.58% | 194.130608 | 166.589500 | -14.19% |
| `frame_full` | us/frame | 4149.073757 | 4188.316136 | +0.95% | 3576.467802 | 3749.851989 | +4.85% |
| `frame_one_row` | us/frame | 271.439801 | 273.639386 | +0.81% | 246.532466 | 254.930795 | +3.41% |
| `frame_scroll_1line_growing` | us/frame | 2345.404453 | 2366.779297 | +0.91% | 2043.786204 | 2174.516282 | +6.40% |
| `frame_scroll_1line_full` | us/frame | 2360.756121 | 2362.668539 | +0.08% | 2042.541453 | 2160.457437 | +5.77% |
| `frame_unchanged` | us/frame | 63.472915 | 63.551741 | +0.12% | 63.930633 | 64.701083 | +1.21% |

## 性能验收结论：未通过

实现和差分验证完成，状态按性能目标记为**部分完成**，依据用户允许“正确但没有达到验收指标的优化，可以提交”的规则提交。不调整基准条件，也不把下列缺口标为通过：

- parse_ascii_scroll：默认约 2.94 倍，alloc-count 约 2.50 倍，低于 3 倍。
- parse_ascii_noscroll：默认约 5.69 倍，alloc-count 约 5.71 倍；parse_ascii_2027 分别约 13.36 倍、10.99 倍，二者自身的三倍提升目标通过。
- 2027 / noscroll 吞吐比例：默认约 40.60%，alloc-count 约 33.63%，低于 70%。T0 固定的 2027 场景包含滚动/历史维护，而 noscroll 没有；保持原负载，未改成无滚动来补齐比例。
- parse_cjk：默认退化 11.58%，alloc-count 退化 14.19%。新增 ASCII 候选检查已提前排除高位字节，但退化尚未解决，具体归因未独立定位，不将代码布局或机器噪声假定为已证实原因。
- 帧耗时也未达到“其他指标不退化”：alloc-count 下 full / one_row / scroll_growing / scroll_full / unchanged 分别约增加 4.85% / 3.41% / 6.40% / 5.77% / 1.21%。默认配置的相应变化见完整表。本任务未改 render.rs，但不据此排除实测退化。
- 分配次数、分配字节、Cell/Row 大小、history_bytes_10k 均未增加；2027 普通 ASCII 边界查询的零分配由专门测试验证。

默认配置 parse_ascii_scroll 三轮波动为 6.75%，超过 5%；其他默认指标低于 5%。alloc-count 全部指标低于 5%，最大 parse_ascii_scroll 为 3.34%。比较工具默认组因波动及 CJK 回归失败，alloc-count 组因 CJK 回归失败；失败结果均保留 target/evaluations/t1-*-comparison.txt，没有丢弃样本或重跑挑选稳定轮组。

## 必需检查结果

| 检查 | 结果 |
| --- | --- |
| cargo fmt --check | 通过 |
| cargo clippy --all-targets --target x86_64-unknown-linux-gnu -- -D warnings | 通过；未在真机验证 |
| cargo clippy --all-targets --target x86_64-pc-windows-gnu -- -D warnings | 通过；未在真机验证 |
| cargo clippy --all-targets --target aarch64-apple-darwin -- -D warnings | 通过 |
| cargo clippy --all-targets --target x86_64-apple-darwin -- -D warnings | 通过；未在真机验证 |
| RUSTUP_HOME=/tmp/congminal-perf-rustup cargo +1.88 check | 通过，实际 Rust 1.88.0；沿用 T0 的临时工具链映射 |
| cargo test -- --test-threads=1 | 主程序 915 passed / 57 ignored，helper 21 passed |
| cargo test --features alloc-count -- --test-threads=1 | 主程序 916 passed / 58 ignored，helper 21 passed；含新增 2027 零分配测试 |
| cargo clippy --all-targets --features alloc-count -- -D warnings | 额外检查通过 |

完整回归在允许本地 socket 的环境执行，避免 T0 已确认的沙箱 IPC 拒绝，不跳过任何既有用例。六轮 release 基准本身全部通过其正确性/额度断言；性能比较工具失败已在上一节明确记录，不混同为性能验收通过。

## 已知问题（按严重程度）

1. CJK 与部分帧指标实测退化，违反“其他指标不退化”；未定位到足以证明和修复该退化的原因。
2. ASCII 滚动的三倍目标及 2027/noscroll 的 70% 目标未达到；实现保留，提交状态为部分完成。
3. 默认组 ASCII 滚动波动超过 5%，对应数据可信区间有限；没有更换负载、选取单轮或删去样本。
4. Linux/X11、Windows、macOS Intel 仅交叉检查，未在真机验证。

所有任务内代码和必需回归均完成；后续性能工作需在此提交上另行验证，不能把本次缺口自动算入 T2 的收益。提交以 [T1] 开头，推送的完整 HEAD SHA 记录在聊天进度日志，避免文档自引用提交哈希。
